use super::*;
use crate::prompt_processor::{CompileResult, LayerValidation, OutputContract};
use crate::types::SplitDirection;
use rsi_common::types::{ContextUsageConfidence, Session};
use std::path::PathBuf;

fn test_client() -> DaemonClient {
    DaemonClient::new(PathBuf::from("/tmp/test.sock"))
}

/// Create a clean App for tests — resets tabs to a single default tab so
/// tests aren't affected by persisted state loaded from disk.
fn test_app() -> App {
    let mut app = App::new(test_client());
    let initial_pane_id = PaneId(0);
    app.tabs = vec![Tab {
        name: "[1]".to_string(),
        session_list_state: Tab::default_session_list_state(),
        layout: SplitNode::Leaf {
            pane: Pane::SessionList {
                selected_index: 0,
                selected_session: None,
                scroll_offset: 0,
                active_zone: Default::default(),
                taskrabbit_selected_index: 0,
                archive_selected_index: 0,
                jobs_selected_index: 0,
            },
            id: initial_pane_id,
        },
        focused_pane: initial_pane_id,
        project_id: None,
        detail_split_offset: 0,
        layout_x_offset_adj: 0,
        session_list_width_pct: 0,
        descent_path: Vec::new(),
        bottom_focus_target: crate::types::BottomZone::Off,
        mini_dag_focus: None,
    }];
    app.active_tab = 0;
    app.next_pane_id = 1;
    app
}

#[tokio::test]
async fn quick_input_transport_drop_preserves_exact_draft_until_retry_accepts() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let temp_dir = tempfile::tempdir().expect("temporary quick-launch socket directory");
    let socket_path = temp_dir.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).expect("quick-launch listener");
    let returned_id = uuid::Uuid::new_v4();
    let server = tokio::spawn(async move {
        for attempt in 0..2 {
            let (stream, _) = listener.accept().await.expect("quick-launch client");
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let line = lines
                .next_line()
                .await
                .expect("quick-launch request read")
                .expect("quick-launch request line");
            let request: serde_json::Value =
                serde_json::from_str(&line).expect("quick-launch request JSON");
            assert_eq!(request["method"], "LaunchSession");
            assert_eq!(request["params"]["query"], "quick durable draft");
            if attempt == 1 {
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"].clone(),
                    "result": {"session_id": returned_id},
                });
                writer
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .expect("quick-launch response write");
            }
        }
    });

    let mut app = App::new(DaemonClient::new(socket_path));
    app.poll.connected = true;
    app.poll.authoritative_config_ready = true;
    app.input_mode = InputMode::Input;
    app.input_purpose = InputPurpose::NewSession;
    app.input_buffer = "quick durable draft".to_string();
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    crate::event::test_handle_input_mode(&mut app, enter).await;
    assert_eq!(app.input_buffer, "quick durable draft");
    assert_eq!(app.input_mode, InputMode::Input);

    let dropped = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        app.interactive_launch_rx.recv(),
    )
    .await
    .expect("quick transport-drop deadline")
    .expect("quick transport-drop result");
    assert!(app.apply_interactive_launch_result(dropped));
    assert_eq!(app.input_buffer, "quick durable draft");
    assert_eq!(app.input_mode, InputMode::Input);

    crate::event::test_handle_input_mode(&mut app, enter).await;
    let accepted = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        app.interactive_launch_rx.recv(),
    )
    .await
    .expect("quick acceptance deadline")
    .expect("quick acceptance result");
    assert!(app.apply_interactive_launch_result(accepted));
    assert_eq!(app.input_buffer, "");
    assert_eq!(app.input_mode, InputMode::Normal);
    assert!(
        app.notifications
            .iter()
            .any(|notification| { notification.session_id == Some(returned_id) })
    );
    server.await.expect("quick-launch server");
}

#[test]
fn test_app_initial_state() {
    let app = test_app();
    assert!(!app.poll.connected);
    assert!(!app.quit);
    assert_eq!(app.input_mode, InputMode::Normal);
    assert!(app.sessions.is_empty());
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.active_tab, 0);
}

#[test]
fn session_list_empty_state_tracks_authoritative_bootstrap_progress() {
    let mut app = test_app();
    assert_eq!(app.session_list_empty_message(), "Connecting to daemon…");

    app.poll.connected = true;
    app.bootstrap.config = bootstrap::BootstrapComponentState::Loading;
    assert_eq!(
        app.session_list_empty_message(),
        "Loading daemon configuration…"
    );

    app.bootstrap.config = bootstrap::BootstrapComponentState::Ready;
    app.bootstrap.sessions = bootstrap::BootstrapComponentState::Loading;
    assert_eq!(app.session_list_empty_message(), "Loading sessions…");

    app.poll.sessions_authoritative = true;
    app.bootstrap.sessions = bootstrap::BootstrapComponentState::Ready;
    assert_eq!(
        app.session_list_empty_message(),
        "Ready · No sessions. Press 'n' to create one."
    );
}

#[test]
fn config_failure_is_explicit_and_not_launch_ready() {
    let mut app = test_app();
    app.poll.connected = true;
    app.mark_daemon_config_error("authoritative config rejected".to_string());

    assert!(!app.authoritative_config_ready());
    assert_eq!(
        app.daemon_config_unavailable_reason(),
        "Daemon configuration unavailable: authoritative config rejected; draft preserved"
    );
    assert_eq!(
        app.session_list_empty_message(),
        "Daemon configuration error: authoritative config rejected"
    );
}

#[test]
fn pioneer_has_supported_fallback_label_and_independent_availability() {
    let mut app = test_app();

    assert_eq!(App::provider_label(SessionProvider::Pioneer), "Pioneer");
    assert_eq!(
        models_for_provider(SessionProvider::Pioneer),
        vec![("claude-sonnet-5".to_string(), "Claude Sonnet 5".to_string())]
    );

    app.provider_availability
        .insert(SessionProvider::Codex, true);
    app.provider_availability
        .insert(SessionProvider::Pioneer, false);
    assert!(app.is_provider_available(SessionProvider::Codex));
    assert!(!app.is_provider_available(SessionProvider::Pioneer));
}

#[test]
fn test_tab_creation() {
    let mut app = test_app();
    app.create_tab();
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(app.active_tab, 1);
    assert_eq!(app.tabs[0].name, "[1]");
    assert_eq!(app.tabs[1].name, "[2]");
}

#[test]
fn test_tab_navigation() {
    let mut app = test_app();
    app.create_tab();
    app.create_tab();
    assert_eq!(app.active_tab, 2);

    app.next_tab();
    assert_eq!(app.active_tab, 0); // wraps

    app.prev_tab();
    assert_eq!(app.active_tab, 2); // wraps back
}

#[test]
fn test_tab_close() {
    let mut app = test_app();
    app.create_tab();
    assert_eq!(app.tabs.len(), 2);
    app.active_tab = 0;
    app.close_tab();
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.active_tab, 0);
}

#[test]
fn test_close_last_tab_quits() {
    let mut app = test_app();
    app.close_tab();
    assert!(app.quit);
}

#[test]
fn test_split_focused() {
    let mut app = App::new(test_client());
    let original_id = app.active_tab().focused_pane;
    app.split_focused(SplitDirection::Vertical);

    let tab = app.active_tab();
    let leaf_ids = tab.layout.leaf_ids();
    assert_eq!(leaf_ids.len(), 2);
    assert_eq!(tab.focused_pane, original_id); // focus stays on original
}

#[test]
fn test_close_focused_pane_with_split() {
    let mut app = App::new(test_client());
    app.split_focused(SplitDirection::Vertical);
    assert_eq!(app.active_tab().layout.leaf_ids().len(), 2);

    app.close_focused_pane();
    assert_eq!(app.active_tab().layout.leaf_ids().len(), 1);
}

#[test]
fn test_focus_neighbor() {
    let mut app = App::new(test_client());
    app.split_focused(SplitDirection::Vertical);
    let leaf_ids = app.active_tab().layout.leaf_ids();
    let first = leaf_ids[0];
    let second = leaf_ids[1];

    assert_eq!(app.active_tab().focused_pane, first);
    app.focus_neighbor(NavDirection::Right);
    assert_eq!(app.active_tab().focused_pane, second);
    app.focus_neighbor(NavDirection::Left);
    assert_eq!(app.active_tab().focused_pane, first);
}

#[test]
fn test_update_sessions() {
    let mut app = App::new(test_client());
    let session = rsi_common::types::Session {
        context_fill_pct: None,
        id: uuid::Uuid::new_v4(),
        provider: rsi_common::types::SessionProvider::Claude,
        claude_session_id: None,
        query: "test".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        pending_question: None,
        pending_archive: false,
        working_dir: PathBuf::from("/tmp"),
        git_branch: None,
        status: rsi_common::types::SessionStatus::Running,
        project_id: None,
        session_kind: rsi_common::types::SessionKind::Standard,
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
        total_output_tokens: None,
        total_cache_creation_tokens: None,
        total_cache_read_tokens: None,
        stop_reason: None,
        context_usage_confidence: ContextUsageConfidence::default(),
        continued_from: None,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        scheduled_job_id: None,
        pipeline_artifact: None,
        workflow_id: None,
        workflow_id_override: None,
        rotation_depth: 0,
        retry_attempt: None,
        max_retries: None,
        daemon_input_tokens: None,
        daemon_output_tokens: None,
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
    };
    let id = session.id;
    app.update_sessions(vec![session]);

    assert_eq!(app.session_order.len(), 1);
    assert!(app.sessions.contains_key(&id));
}

#[test]
fn test_update_sessions_sorted_by_staleness() {
    let mut app = App::new(test_client());
    app.settings.sort_order = SortOrder::StalestFirst; // Ensure deterministic sort for test
    let now = chrono::Utc::now();
    let s1 = rsi_common::types::Session {
        context_fill_pct: None,
        id: uuid::Uuid::new_v4(),
        provider: rsi_common::types::SessionProvider::Claude,
        claude_session_id: None,
        query: "recent".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        working_dir: PathBuf::from("/tmp"),
        git_branch: None,
        status: rsi_common::types::SessionStatus::Completed,
        project_id: None,
        session_kind: rsi_common::types::SessionKind::Standard,
        created_at: now,
        updated_at: now, // more recent
        cost_usd: None,
        duration_ms: None,
        num_turns: None,
        model: None,
        input_tokens: None,
        output_tokens: None,
        context_window: None,
        resolved_context_budget: None,
        total_input_tokens: None,
        total_output_tokens: None,
        total_cache_creation_tokens: None,
        total_cache_read_tokens: None,
        stop_reason: None,
        context_usage_confidence: ContextUsageConfidence::default(),
        continued_from: None,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        scheduled_job_id: None,
        pipeline_artifact: None,
        workflow_id: None,
        workflow_id_override: None,
        pending_question: None,
        pending_archive: false,
        rotation_depth: 0,
        retry_attempt: None,
        max_retries: None,
        daemon_input_tokens: None,
        daemon_output_tokens: None,
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
    };
    let s2 = rsi_common::types::Session {
        context_fill_pct: None,
        id: uuid::Uuid::new_v4(),
        provider: rsi_common::types::SessionProvider::Claude,
        claude_session_id: None,
        query: "stale".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        working_dir: PathBuf::from("/tmp"),
        git_branch: None,
        status: rsi_common::types::SessionStatus::Completed,
        project_id: None,
        session_kind: rsi_common::types::SessionKind::Standard,
        created_at: now - chrono::Duration::hours(2),
        updated_at: now - chrono::Duration::hours(2), // older = more stale
        cost_usd: None,
        duration_ms: None,
        num_turns: None,
        model: None,
        input_tokens: None,
        output_tokens: None,
        context_window: None,
        resolved_context_budget: None,
        total_input_tokens: None,
        total_output_tokens: None,
        total_cache_creation_tokens: None,
        total_cache_read_tokens: None,
        stop_reason: None,
        context_usage_confidence: ContextUsageConfidence::default(),
        continued_from: None,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        scheduled_job_id: None,
        pipeline_artifact: None,
        workflow_id: None,
        workflow_id_override: None,
        pending_question: None,
        pending_archive: false,
        rotation_depth: 0,
        retry_attempt: None,
        max_retries: None,
        daemon_input_tokens: None,
        daemon_output_tokens: None,
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
    };
    let id1 = s1.id;
    let id2 = s2.id;

    app.update_sessions(vec![s1, s2]);

    // Stalest (oldest updated_at) should be at index 0
    assert_eq!(app.session_order[0], id2);
    assert_eq!(app.session_order[1], id1);
}

#[test]
fn test_prompt_compile_result_routes_to_stacked_prompt_overlay() {
    let mut app = test_app();
    crate::overlay::open_blank_popup(&mut app);

    let overlay_id = match app.focused_input_overlay_mut() {
        Some(crate::types::OverlayState::Prompt {
            overlay_id,
            surface,
            ..
        }) => {
            surface.correction_in_flight = true;
            *overlay_id
        }
        _ => panic!("Expected Prompt overlay"),
    };

    // Simulate another top-level overlay being active while the stacked popup remains open.
    app.overlay = crate::types::OverlayState::ThemePicker {
        selected_index: 0,
        original_index: 0,
    };

    app.apply_prompt_compile_result(
        overlay_id,
        Ok(CompileResult {
            compiled: "compiled prompt".to_string(),
            contract: OutputContract::Complete,
            layer_validation: LayerValidation {
                semantic: true,
                syntactic: true,
                deictic: true,
                discourse: true,
                pragmatic: true,
            },
        }),
        Some("original input".to_string()),
    );

    assert!(matches!(
        app.overlay,
        crate::types::OverlayState::ThemePicker { .. }
    ));

    match app.focused_input_overlay() {
        Some(crate::types::OverlayState::Prompt { surface, .. }) => {
            assert!(!surface.correction_in_flight);
            assert_eq!(
                surface.corrected_preview.as_deref(),
                Some("compiled prompt")
            );
        }
        _ => panic!("Expected Prompt overlay"),
    }
}

#[test]
fn test_ai_command_result_routes_to_stacked_prompt_overlay() {
    let mut app = test_app();
    crate::overlay::open_blank_popup(&mut app);

    let overlay_id = match app.focused_input_overlay_mut() {
        Some(crate::types::OverlayState::Prompt {
            overlay_id,
            surface,
            ..
        }) => {
            surface.corrected_preview = None;
            *overlay_id
        }
        _ => panic!("Expected Prompt overlay"),
    };

    // Simulate some unrelated top-level overlay state; delivery should still target
    // the stacked prompt popup by stable overlay id.
    app.overlay = crate::types::OverlayState::ThemePicker {
        selected_index: 0,
        original_index: 0,
    };

    app.apply_ai_command_result(
        crate::types::AiAssistantSource::OverlaySurface(overlay_id),
        Ok("rewritten prompt".to_string()),
    );

    assert!(matches!(
        app.overlay,
        crate::types::OverlayState::ThemePicker { .. }
    ));

    match app.focused_input_overlay() {
        Some(crate::types::OverlayState::Prompt { surface, .. }) => {
            assert_eq!(
                surface.corrected_preview.as_deref(),
                Some("rewritten prompt")
            );
        }
        _ => panic!("Expected Prompt overlay"),
    }
}

#[test]
fn test_update_sessions_fixup_selected_index() {
    let mut app = App::new(test_client());
    app.settings.sort_order = SortOrder::StalestFirst; // Ensure deterministic sort for test
    app.current_project_id = None; // No project filter
    let now = chrono::Utc::now();

    // Insert first session (recent) — pane has no selection yet
    let s1 = rsi_common::types::Session {
        context_fill_pct: None,
        id: uuid::Uuid::new_v4(),
        provider: rsi_common::types::SessionProvider::Claude,
        claude_session_id: None,
        query: "recent".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        working_dir: PathBuf::from("/tmp"),
        git_branch: None,
        status: rsi_common::types::SessionStatus::Completed,
        project_id: None,
        session_kind: rsi_common::types::SessionKind::Standard,
        created_at: now,
        updated_at: now,
        cost_usd: None,
        duration_ms: None,
        num_turns: None,
        model: None,
        input_tokens: None,
        output_tokens: None,
        context_window: None,
        resolved_context_budget: None,
        total_input_tokens: None,
        total_output_tokens: None,
        total_cache_creation_tokens: None,
        total_cache_read_tokens: None,
        stop_reason: None,
        context_usage_confidence: ContextUsageConfidence::default(),
        continued_from: None,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        scheduled_job_id: None,
        pipeline_artifact: None,
        workflow_id: None,
        workflow_id_override: None,
        pending_question: None,
        pending_archive: false,
        rotation_depth: 0,
        retry_attempt: None,
        max_retries: None,
        daemon_input_tokens: None,
        daemon_output_tokens: None,
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
    };
    let id1 = s1.id;
    app.update_sessions(vec![s1]);

    assert_eq!(app.session_order[0], id1);

    // Now select the session (simulating user interaction)
    let tab = &mut app.tabs[app.active_tab];
    let focused = tab.focused_pane;
    if let Some(Pane::SessionList {
        selected_index,
        selected_session,
        ..
    }) = tab.layout.find_pane_mut(focused)
    {
        *selected_index = 0;
        *selected_session = Some(id1);
    }

    // Insert a staler session — sort should place it before id1
    let s2 = rsi_common::types::Session {
        context_fill_pct: None,
        id: uuid::Uuid::new_v4(),
        provider: rsi_common::types::SessionProvider::Claude,
        claude_session_id: None,
        query: "stale".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        working_dir: PathBuf::from("/tmp"),
        git_branch: None,
        status: rsi_common::types::SessionStatus::Completed,
        project_id: None,
        session_kind: rsi_common::types::SessionKind::Standard,
        created_at: now - chrono::Duration::hours(1),
        updated_at: now - chrono::Duration::hours(1),
        cost_usd: None,
        duration_ms: None,
        num_turns: None,
        model: None,
        input_tokens: None,
        output_tokens: None,
        context_window: None,
        resolved_context_budget: None,
        total_input_tokens: None,
        total_output_tokens: None,
        total_cache_creation_tokens: None,
        total_cache_read_tokens: None,
        stop_reason: None,
        context_usage_confidence: ContextUsageConfidence::default(),
        continued_from: None,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        scheduled_job_id: None,
        pipeline_artifact: None,
        workflow_id: None,
        workflow_id_override: None,
        pending_question: None,
        pending_archive: false,
        rotation_depth: 0,
        retry_attempt: None,
        max_retries: None,
        daemon_input_tokens: None,
        daemon_output_tokens: None,
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
    };
    let id2 = s2.id;
    // Daemon's ListSessions returns the full set, so include both
    let s1_clone = app.sessions.get(&id1).unwrap().session.clone();
    app.update_sessions(vec![s1_clone, s2]);

    // Staleness sort: s2 (older) at 0, s1 (recent) at 1
    assert_eq!(app.session_order[0], id2);
    assert_eq!(app.session_order[1], id1);

    // Background poll preserves visual position: cursor stays at index 0,
    // UUID updates to whatever session now occupies that row (id2)
    match app.focused_pane() {
        Some(Pane::SessionList {
            selected_index,
            selected_session,
            ..
        }) => {
            assert_eq!(*selected_index, 0);
            assert_eq!(*selected_session, Some(id2));
        }
        _ => panic!("Expected SessionList pane"),
    }
}

#[test]
fn test_nav_down_session_list() {
    let mut app = App::new(test_client());
    app.current_project_id = None; // No project filter
    // Add sessions
    let sessions: Vec<Session> = (0..3)
        .map(|i| Session {
            context_fill_pct: None,
            id: uuid::Uuid::new_v4(),
            provider: rsi_common::types::SessionProvider::Claude,
            claude_session_id: None,
            query: format!("query {}", i),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            pending_question: None,
            pending_archive: false,
            working_dir: PathBuf::from("/tmp"),
            git_branch: None,
            status: rsi_common::types::SessionStatus::Completed,
            project_id: None,
            session_kind: rsi_common::types::SessionKind::Standard,
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
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            context_usage_confidence: ContextUsageConfidence::default(),
            continued_from: None,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            scheduled_job_id: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
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
        })
        .collect();
    app.update_sessions(sessions);

    // Navigate down
    app.nav_down();
    match app.focused_pane() {
        Some(Pane::SessionList { selected_index, .. }) => assert_eq!(*selected_index, 1),
        _ => panic!("Expected SessionList pane"),
    }
}

#[tokio::test]
async fn test_auto_launch_resume_handoff_completed_session() {
    let mut app = App::new(test_client());
    let sid = uuid::Uuid::new_v4();
    let session = Session {
        context_fill_pct: None,
        id: sid,
        provider: rsi_common::types::SessionProvider::Claude,
        claude_session_id: None,
        query: "test handoff".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        working_dir: PathBuf::from("/tmp"),
        git_branch: None,
        status: rsi_common::types::SessionStatus::Completed,
        project_id: None,
        session_kind: rsi_common::types::SessionKind::Standard,
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
        total_output_tokens: None,
        total_cache_creation_tokens: None,
        total_cache_read_tokens: None,
        stop_reason: None,
        context_usage_confidence: ContextUsageConfidence::default(),
        continued_from: None,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        scheduled_job_id: None,
        pipeline_artifact: None,
        workflow_id: None,
        workflow_id_override: None,
        pending_question: None,
        pending_archive: false,
        rotation_depth: 0,
        retry_attempt: None,
        max_retries: None,
        daemon_input_tokens: None,
        daemon_output_tokens: None,
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
    };
    let mut state = SessionState::new(session);
    state.docregblock_contents =
        vec!["/resume_handoff @thoughts/shared/handoffs/test.md".to_string()];
    app.sessions.insert(sid, state);

    let target_id = uuid::Uuid::new_v4();
    app.auto_resume_handoff_active_generation = Some(1);
    app.auto_resume_handoff_inflight.insert(sid);
    let accepted = app.apply_auto_resume_handoff_result(AutoResumeHandoffResult {
        generation: 1,
        source_session_id: sid,
        accepted: Ok(target_id),
        model_warning: None,
    });
    assert!(accepted);
    assert_eq!(app.auto_launched_handoff_sessions.len(), 1);
    assert!(app.notifications.iter().any(|notification| matches!(
        notification.kind,
        crate::types::NotificationKind::SessionLaunching
    )));
}

#[tokio::test]
async fn test_auto_launch_resume_handoff_running_session_not_launched() {
    let mut app = App::new(test_client());
    let sid = uuid::Uuid::new_v4();
    let session = Session {
        context_fill_pct: None,
        id: sid,
        provider: rsi_common::types::SessionProvider::Claude,
        claude_session_id: None,
        query: "still running".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        working_dir: PathBuf::from("/tmp"),
        git_branch: None,
        status: rsi_common::types::SessionStatus::Running,
        project_id: None,
        session_kind: rsi_common::types::SessionKind::Standard,
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
        total_output_tokens: None,
        total_cache_creation_tokens: None,
        total_cache_read_tokens: None,
        stop_reason: None,
        context_usage_confidence: ContextUsageConfidence::default(),
        continued_from: None,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        scheduled_job_id: None,
        pipeline_artifact: None,
        workflow_id: None,
        workflow_id_override: None,
        pending_question: None,
        pending_archive: false,
        rotation_depth: 0,
        retry_attempt: None,
        max_retries: None,
        daemon_input_tokens: None,
        daemon_output_tokens: None,
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
    };
    let mut state = SessionState::new(session);
    state.docregblock_contents =
        vec!["/resume_handoff @thoughts/shared/handoffs/test.md".to_string()];
    app.sessions.insert(sid, state);

    app.request_auto_launch_resume_handoff();
    assert!(app.auto_launched_handoff_sessions.is_empty());
}

#[tokio::test]
async fn app_drop_cancels_owned_conversation_poll() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    struct CancellationProbe(Arc<AtomicBool>);
    impl Drop for CancellationProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let cancelled = Arc::new(AtomicBool::new(false));
    let mut app = App::new(test_client());
    let probe = Arc::clone(&cancelled);
    app.conversation_poll_handle = Some(tokio::spawn(async move {
        let _probe = CancellationProbe(probe);
        std::future::pending::<()>().await;
    }));
    tokio::task::yield_now().await;
    drop(app);
    tokio::task::yield_now().await;

    assert!(cancelled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn app_drop_cancels_all_response_bearing_launch_and_config_tasks() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct CancellationProbe(Arc<AtomicUsize>);
    impl Drop for CancellationProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn pending_task(counter: &Arc<AtomicUsize>) -> tokio::task::JoinHandle<()> {
        let counter = Arc::clone(counter);
        tokio::spawn(async move {
            let _probe = CancellationProbe(counter);
            std::future::pending::<()>().await;
        })
    }

    let cancelled = Arc::new(AtomicUsize::new(0));
    let mut app = App::new(test_client());
    app.interactive_launch_handle = Some(pending_task(&cancelled));
    app.docreg_operation_handle = Some(pending_task(&cancelled));
    app.auto_resume_handoff_handle = Some(pending_task(&cancelled));
    app.classifier_config_handle = Some(pending_task(&cancelled));
    tokio::task::yield_now().await;
    drop(app);
    tokio::task::yield_now().await;

    assert_eq!(cancelled.load(Ordering::SeqCst), 4);
}

#[test]
fn stale_response_generations_preserve_newer_launch_docreg_auto_resume_and_classifier_owners() {
    let mut app = test_app();
    let source_id = uuid::Uuid::new_v4();
    let mut source = make_session_with_status(rsi_common::types::SessionStatus::Completed);
    source.id = source_id;
    source.title = Some("Newer generation source".to_string());
    app.sessions.insert(source_id, SessionState::new(source));

    app.interactive_launch_pending = Some(PendingInteractiveLaunch {
        generation: 2,
        origin: InteractiveLaunchOrigin::QuickInput {
            query: "newer draft".to_string(),
        },
        placement: LaunchPlacement::NewTab,
    });
    app.input_buffer = "newer draft".to_string();
    assert!(
        !app.apply_interactive_launch_result(InteractiveLaunchResult {
            generation: 1,
            accepted: Ok(uuid::Uuid::new_v4()),
            model_warning: None,
        })
    );
    assert_eq!(
        app.interactive_launch_pending
            .as_ref()
            .map(|pending| pending.generation),
        Some(2)
    );
    assert_eq!(app.input_buffer, "newer draft");

    app.docreg_operation_pending = Some(PendingDocregOperation {
        generation: 2,
        session_id: source_id,
        launch_commands: vec!["/plan @newer.md".to_string()],
        was_detail: true,
        should_archive: true,
    });
    assert!(!app.apply_docreg_operation_result(DocregOperationResult {
        generation: 1,
        session_id: source_id,
        launch_commands: vec!["/plan @older.md".to_string()],
        outcome: handoff::DocregOperationOutcome::Archive(Ok(())),
    }));
    assert_eq!(
        app.sessions[&source_id].session.title.as_deref(),
        Some("Newer generation source")
    );
    assert_eq!(
        app.docreg_operation_pending
            .as_ref()
            .map(|pending| pending.generation),
        Some(2)
    );

    app.auto_resume_handoff_active_generation = Some(2);
    app.auto_resume_handoff_inflight.insert(source_id);
    assert!(
        !app.apply_auto_resume_handoff_result(AutoResumeHandoffResult {
            generation: 1,
            source_session_id: source_id,
            accepted: Ok(uuid::Uuid::new_v4()),
            model_warning: None,
        })
    );
    assert_eq!(app.auto_resume_handoff_active_generation, Some(2));
    assert!(app.auto_resume_handoff_inflight.contains(&source_id));

    app.classifier_config_pending = Some(PendingClassifierConfig {
        generation: 2,
        model_id: "newer-classifier".to_string(),
        restore_dropdown: crate::types::ModelDropdownState::default(),
    });
    assert!(!app.apply_classifier_config_result(ClassifierConfigResult {
        generation: 1,
        model_id: "older-classifier".to_string(),
        accepted: Ok(()),
    }));
    assert_eq!(
        app.classifier_config_pending
            .as_ref()
            .map(|pending| (pending.generation, pending.model_id.as_str())),
        Some((2, "newer-classifier"))
    );
}

#[tokio::test]
async fn rejected_auto_resume_source_does_not_starve_later_eligible_handoff() {
    let mut app = App::new(test_client());
    app.poll.connected = true;
    app.poll.authoritative_config_ready = true;
    let rejected_id = uuid::Uuid::new_v4();
    let eligible_id = uuid::Uuid::new_v4();
    for (session_id, handoff) in [
        (rejected_id, "thoughts/shared/handoffs/rejected.md"),
        (eligible_id, "thoughts/shared/handoffs/eligible.md"),
    ] {
        let mut session = make_session_with_status(rsi_common::types::SessionStatus::Completed);
        session.id = session_id;
        let mut state = SessionState::new(session);
        state.docregblock_contents = vec![format!("/resume_handoff @{handoff}")];
        app.sessions.insert(session_id, state);
    }
    app.auto_resume_handoff_failed
        .insert(rejected_id, app.auto_resume_progress_epoch);

    app.request_auto_launch_resume_handoff();

    assert!(app.auto_resume_handoff_inflight.contains(&eligible_id));
    app.auto_resume_handoff_handle
        .take()
        .expect("eligible task")
        .abort();
}

#[tokio::test]
async fn repeated_empty_conversation_polls_do_not_retry_a_rejected_auto_resume() {
    let mut app = App::new(test_client());
    app.poll.connected = true;
    app.poll.authoritative_config_ready = true;
    let session_id = uuid::Uuid::new_v4();
    let mut session = make_session_with_status(rsi_common::types::SessionStatus::Completed);
    session.id = session_id;
    let mut state = SessionState::new(session);
    state.docregblock_contents = vec!["/resume_handoff @thoughts/shared/handoffs/retry.md".into()];
    app.sessions.insert(session_id, state);
    app.auto_resume_handoff_failed
        .insert(session_id, app.auto_resume_progress_epoch);

    for generation in 1..=3 {
        app.conversation_poll_active_generation = Some(generation);
        let progressed = app.apply_conversation_poll_result(ConversationPollResult {
            attempt: app.bootstrap.attempt(),
            generation,
            batches: Ok(Vec::new()),
            batch_unsupported: false,
            connection_lost: false,
        });

        assert!(!progressed);
        assert_eq!(
            app.auto_resume_handoff_failed.get(&session_id),
            Some(&app.auto_resume_progress_epoch)
        );
        assert!(app.auto_resume_handoff_inflight.is_empty());
        assert!(app.auto_resume_handoff_handle.is_none());
    }
}

#[tokio::test]
async fn conversation_event_progress_reopens_rejected_auto_resume_eligibility() {
    let mut app = App::new(test_client());
    app.poll.connected = true;
    app.poll.authoritative_config_ready = true;
    let session_id = uuid::Uuid::new_v4();
    let mut session = make_session_with_status(rsi_common::types::SessionStatus::Completed);
    session.id = session_id;
    let mut state = SessionState::new(session);
    state.docregblock_contents = vec!["/resume_handoff @thoughts/shared/handoffs/retry.md".into()];
    app.sessions.insert(session_id, state);
    app.auto_resume_handoff_failed
        .insert(session_id, app.auto_resume_progress_epoch);
    app.conversation_poll_active_generation = Some(1);

    let progressed = app.apply_conversation_poll_result(ConversationPollResult {
        attempt: app.bootstrap.attempt(),
        generation: 1,
        batches: Ok(vec![(
            session_id,
            None,
            vec![make_conversation_event(
                rsi_common::types::EventType::Message,
                1,
            )],
        )]),
        batch_unsupported: false,
        connection_lost: false,
    });

    assert!(progressed);
    assert_ne!(
        app.auto_resume_handoff_failed.get(&session_id),
        Some(&app.auto_resume_progress_epoch)
    );
}

#[tokio::test]
async fn auto_resume_semantic_rejection_retries_only_on_relevant_progress_epochs() {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let temp_dir = tempfile::tempdir().expect("temporary auto-resume socket directory");
    let socket_path = temp_dir.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).expect("auto-resume listener");
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let server_requests = Arc::clone(&requests);
    let returned_id = uuid::Uuid::new_v4();
    let server = tokio::spawn(async move {
        for attempt in 0..4 {
            let (stream, _) = listener.accept().await.expect("auto-resume client");
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let line = lines
                .next_line()
                .await
                .expect("auto-resume request read")
                .expect("auto-resume request line");
            let request: serde_json::Value =
                serde_json::from_str(&line).expect("auto-resume request JSON");
            assert_eq!(request["method"], "LaunchSession");
            server_requests
                .lock()
                .expect("auto-resume request log")
                .push(request["params"]["query"].as_str().unwrap().to_string());
            let response = if attempt < 3 {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"].clone(),
                    "error": {
                        "code": -32074,
                        "message": format!("auto resume rejected epoch {attempt}"),
                    },
                })
            } else {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"].clone(),
                    "result": {"session_id": returned_id},
                })
            };
            writer
                .write_all(format!("{response}\n").as_bytes())
                .await
                .expect("auto-resume response write");
        }
    });

    let mut app = App::new(DaemonClient::new(socket_path));
    app.poll.connected = true;
    app.poll.authoritative_config_ready = true;
    let source_id = uuid::Uuid::new_v4();
    let mut source = make_session_with_status(rsi_common::types::SessionStatus::Completed);
    source.id = source_id;
    let mut source_state = SessionState::new(source);
    source_state.docregblock_contents =
        vec!["/resume_handoff @thoughts/shared/handoffs/progress.md".to_string()];
    app.sessions.insert(source_id, source_state);

    let activity_id = uuid::Uuid::new_v4();
    let mut activity = make_session_with_status(rsi_common::types::SessionStatus::Running);
    activity.id = activity_id;
    app.sessions
        .insert(activity_id, SessionState::new(activity));

    app.request_auto_launch_resume_handoff();
    let first = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        app.auto_resume_handoff_rx.recv(),
    )
    .await
    .expect("first auto-resume deadline")
    .expect("first auto-resume result");
    assert!(app.apply_auto_resume_handoff_result(first));
    assert_eq!(requests.lock().expect("request log").len(), 1);

    for generation in 10..13 {
        app.conversation_poll_active_generation = Some(generation);
        assert!(!app.apply_conversation_poll_result(ConversationPollResult {
            attempt: app.bootstrap.attempt(),
            generation,
            batches: Ok(Vec::new()),
            batch_unsupported: false,
            connection_lost: false,
        }));
    }
    tokio::task::yield_now().await;
    assert_eq!(
        requests.lock().expect("request log").len(),
        1,
        "empty observations are not retry epochs"
    );

    let conversation_push =
        crate::notification_stream::NotificationStreamEvent::Bus(rsi_common::rpc::BusEvent {
            event_type: "conversation_event".to_string(),
            data: serde_json::json!({
                "session_id": activity_id,
                "event": make_conversation_event(rsi_common::types::EventType::Message, 1),
            }),
            timestamp: chrono::Utc::now(),
        });
    assert!(app.apply_notification_stream_event(conversation_push));
    let second = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        app.auto_resume_handoff_rx.recv(),
    )
    .await
    .expect("conversation-progress retry deadline")
    .expect("conversation-progress retry result");
    assert!(app.apply_auto_resume_handoff_result(second));

    let metadata_push =
        crate::notification_stream::NotificationStreamEvent::Bus(rsi_common::rpc::BusEvent {
            event_type: "session_metadata_changed".to_string(),
            data: serde_json::json!({
                "session_id": activity_id,
                "model": "metadata-progress-model",
            }),
            timestamp: chrono::Utc::now(),
        });
    assert!(app.apply_notification_stream_event(metadata_push));
    let third = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        app.auto_resume_handoff_rx.recv(),
    )
    .await
    .expect("metadata-progress retry deadline")
    .expect("metadata-progress retry result");
    assert!(app.apply_auto_resume_handoff_result(third));

    app.poll.authoritative_config_ready = false;
    app.bootstrap.config = bootstrap::BootstrapComponentState::Error("recovering".to_string());
    app.apply_authoritative_daemon_config(&serde_json::json!({}));
    let accepted = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        app.auto_resume_handoff_rx.recv(),
    )
    .await
    .expect("config-recovery retry deadline")
    .expect("config-recovery retry result");
    assert!(app.apply_auto_resume_handoff_result(accepted));
    assert!(app.auto_launched_handoff_sessions.contains(&source_id));
    assert!(
        app.notifications
            .iter()
            .any(|notification| { notification.session_id == Some(returned_id) })
    );
    server.await.expect("auto-resume server");
    assert_eq!(
        requests.lock().expect("request log").as_slice(),
        [
            "resume handoff @thoughts/shared/handoffs/progress.md",
            "resume handoff @thoughts/shared/handoffs/progress.md",
            "resume handoff @thoughts/shared/handoffs/progress.md",
            "resume handoff @thoughts/shared/handoffs/progress.md",
        ]
    );
}

#[tokio::test]
async fn auto_resume_conversation_push_discovers_handoff_and_awaits_semantic_response() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let temp_dir = tempfile::tempdir().expect("temporary push-discovery socket directory");
    let socket_path = temp_dir.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).expect("push-discovery listener");
    let returned_id = uuid::Uuid::new_v4();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("push-discovery client");
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        let line = lines
            .next_line()
            .await
            .expect("push-discovery request read")
            .expect("push-discovery request line");
        let request: serde_json::Value =
            serde_json::from_str(&line).expect("push-discovery request JSON");
        assert_eq!(request["method"], "LaunchSession");
        assert_eq!(
            request["params"]["query"],
            "resume handoff @thoughts/shared/handoffs/pushed.md"
        );
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"].clone(),
            "result": {"session_id": returned_id},
        });
        writer
            .write_all(format!("{response}\n").as_bytes())
            .await
            .expect("push-discovery response write");
    });

    let mut app = App::new(DaemonClient::new(socket_path));
    app.poll.connected = true;
    app.poll.authoritative_config_ready = true;
    let source_id = uuid::Uuid::new_v4();
    let mut source = make_session_with_status(rsi_common::types::SessionStatus::Completed);
    source.id = source_id;
    app.sessions.insert(source_id, SessionState::new(source));

    let mut event = make_conversation_event(rsi_common::types::EventType::Message, 1);
    event.session_id = source_id;
    event.content = "handoff ready <docregblock>/resume_handoff @thoughts/shared/handoffs/pushed.md</docregblock>".to_string();
    assert!(app.apply_notification_stream_event(
        crate::notification_stream::NotificationStreamEvent::Bus(rsi_common::rpc::BusEvent {
            event_type: "conversation_event".to_string(),
            data: serde_json::json!({"session_id": source_id, "event": event}),
            timestamp: chrono::Utc::now(),
        })
    ));
    assert_eq!(
        app.sessions[&source_id].docregblock_contents,
        vec!["/resume_handoff @thoughts/shared/handoffs/pushed.md".to_string()]
    );

    let accepted = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        app.auto_resume_handoff_rx.recv(),
    )
    .await
    .expect("push-discovery response deadline")
    .expect("push-discovery response result");
    assert!(app.apply_auto_resume_handoff_result(accepted));
    assert!(app.auto_launched_handoff_sessions.contains(&source_id));
    assert!(
        app.notifications
            .iter()
            .any(|notification| { notification.session_id == Some(returned_id) })
    );
    server.await.expect("push-discovery server");
}

#[tokio::test]
async fn notification_stream_disconnect_resets_readiness_and_starts_one_reconnect() {
    let mut app = App::new(test_client());
    app.poll.connected = true;
    app.poll.authoritative_config_ready = true;
    app.poll.sessions_authoritative = true;

    app.mark_notification_stream_lost();
    let reconnect_attempt = app.bootstrap.attempt();
    assert!(!app.poll.connected);
    assert!(!app.poll.authoritative_config_ready);
    assert!(!app.poll.sessions_authoritative);
    assert!(app.bootstrap.handshake_in_flight());

    app.mark_notification_stream_lost();
    assert_eq!(app.bootstrap.attempt(), reconnect_attempt);
}

#[tokio::test]
async fn established_notification_eof_reconnects_once_and_restores_subscription() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let temp_dir = tempfile::tempdir().expect("temporary socket directory");
    let socket_path = temp_dir.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).expect("socket listener");
    let subscriptions = Arc::new(AtomicUsize::new(0));
    let server_subscriptions = Arc::clone(&subscriptions);
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("client connection");
            let subscriptions = Arc::clone(&server_subscriptions);
            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let request: serde_json::Value =
                        serde_json::from_str(&line).expect("JSON-RPC request");
                    let method = request["method"].as_str().expect("request method");
                    let result = match method {
                        "Subscribe" => serde_json::json!({}),
                        "GetDaemonCapabilities" => {
                            serde_json::to_value(rsi_common::rpc::DaemonCapabilities {
                                push_notifications: true,
                                memory_search: false,
                                ..Default::default()
                            })
                            .expect("capabilities JSON")
                        }
                        "GetHealthStatus" => serde_json::json!({
                            "persistence_queue_depth": 0,
                            "persistence_queue_capacity": 1,
                            "last_command_duration_ms": 0,
                            "project_cache_size": 0
                        }),
                        "GetDaemonConfig" => serde_json::json!({}),
                        "UpdateDaemonConfig" => serde_json::Value::Null,
                        "ListSessions" | "ListProjects" | "ListLabels" => {
                            serde_json::json!([])
                        }
                        other => panic!("unexpected test RPC {other}"),
                    };
                    let response = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"].clone(),
                        "result": result
                    });
                    writer
                        .write_all(format!("{response}\n").as_bytes())
                        .await
                        .expect("write JSON-RPC response");
                    if method == "Subscribe" && subscriptions.fetch_add(1, Ordering::SeqCst) == 0 {
                        return;
                    }
                }
            });
        }
    });

    let mut app = App::new(crate::client::DaemonClient::new(socket_path.clone()));
    app.poll.connected = true;
    app.poll.authoritative_config_ready = true;
    app.poll.sessions_authoritative = true;
    app.notification_stream = Some(crate::notification_stream::NotificationStream::spawn(
        &socket_path,
    ));
    let loss = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        app.notification_stream
            .as_mut()
            .expect("notification stream")
            .recv(),
    )
    .await
    .expect("established EOF result")
    .expect("typed notification loss");
    assert!(matches!(
        &loss,
        crate::notification_stream::NotificationStreamEvent::Lost(
            crate::notification_stream::NotificationStreamLoss::EstablishedEof
        )
    ));
    assert!(app.apply_notification_stream_event(loss));
    let reconnect_attempt = app.bootstrap.attempt();
    assert!(!app.poll.connected);
    assert!(app.bootstrap.handshake_in_flight());

    let handshake = tokio::time::timeout(std::time::Duration::from_secs(1), app.bootstrap.recv())
        .await
        .expect("reconnect handshake result")
        .expect("bootstrap event");
    assert!(app.apply_bootstrap_event(handshake));
    assert_eq!(app.bootstrap.attempt(), reconnect_attempt);
    assert!(app.authoritative_config_ready());

    for _ in 0..50 {
        if subscriptions.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(subscriptions.load(Ordering::SeqCst), 2);
    assert!(app.notification_stream.is_some());
    server.abort();
}

#[tokio::test]
async fn test_auto_launch_resume_handoff_no_tag() {
    let mut app = App::new(test_client());
    let sid = uuid::Uuid::new_v4();
    let session = Session {
        context_fill_pct: None,
        id: sid,
        provider: rsi_common::types::SessionProvider::Claude,
        claude_session_id: None,
        query: "no handoff".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        working_dir: PathBuf::from("/tmp"),
        git_branch: None,
        status: rsi_common::types::SessionStatus::Completed,
        project_id: None,
        session_kind: rsi_common::types::SessionKind::Standard,
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
        total_output_tokens: None,
        total_cache_creation_tokens: None,
        total_cache_read_tokens: None,
        stop_reason: None,
        context_usage_confidence: ContextUsageConfidence::default(),
        continued_from: None,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        scheduled_job_id: None,
        pipeline_artifact: None,
        workflow_id: None,
        workflow_id_override: None,
        pending_question: None,
        pending_archive: false,
        rotation_depth: 0,
        retry_attempt: None,
        max_retries: None,
        daemon_input_tokens: None,
        daemon_output_tokens: None,
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
    };
    let mut state = SessionState::new(session);
    state.docregblock_contents = vec!["/merge_ready".to_string()];
    app.sessions.insert(sid, state);

    app.request_auto_launch_resume_handoff();
    assert!(app.auto_launched_handoff_sessions.is_empty());
}

// --- apply_session_events / formulation trigger ---

fn make_session_with_status(status: rsi_common::types::SessionStatus) -> Session {
    Session {
        context_fill_pct: None,
        id: uuid::Uuid::new_v4(),
        provider: rsi_common::types::SessionProvider::Claude,
        claude_session_id: None,
        query: "test".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        pending_question: None,
        pending_archive: false,
        working_dir: PathBuf::from("/tmp"),
        git_branch: None,
        status,
        project_id: None,
        session_kind: rsi_common::types::SessionKind::Standard,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
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
        total_output_tokens: None,
        total_cache_creation_tokens: None,
        total_cache_read_tokens: None,
        stop_reason: None,
        context_usage_confidence: ContextUsageConfidence::default(),
        continued_from: None,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        scheduled_job_id: None,
        pipeline_artifact: None,
        workflow_id: None,
        workflow_id_override: None,
        rotation_depth: 0,
        retry_attempt: None,
        max_retries: None,
        daemon_input_tokens: None,
        daemon_output_tokens: None,
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

#[test]
fn status_push_deleted_removes_session_from_list_cache() {
    let mut app = test_app();
    let mut session = make_session_with_status(rsi_common::types::SessionStatus::Completed);
    let session_id = session.id;
    session.project_id = Some(uuid::Uuid::new_v4());
    app.current_project_id = session.project_id;

    app.update_sessions(vec![session]);
    assert!(app.sessions.contains_key(&session_id));
    assert!(app.filtered_session_order.contains(&session_id));

    let redraw = app.apply_push_event(rsi_common::rpc::BusEvent {
        event_type: "session_status_changed".to_string(),
        timestamp: chrono::Utc::now(),
        data: serde_json::json!({
            "session_id": session_id,
            "new_status": "Deleted",
        }),
    });

    assert!(redraw);
    assert!(!app.sessions.contains_key(&session_id));
    assert!(!app.session_order.contains(&session_id));
    assert!(!app.filtered_session_order.contains(&session_id));
}

#[test]
fn stale_deleted_cache_is_excluded_from_filtered_session_list() {
    let mut app = test_app();
    app.current_project_id = None;
    let session = make_session_with_status(rsi_common::types::SessionStatus::Completed);
    let session_id = session.id;

    app.update_sessions(vec![session]);
    assert!(app.filtered_session_order.contains(&session_id));

    app.sessions.get_mut(&session_id).unwrap().session.status =
        rsi_common::types::SessionStatus::Deleted;
    app.recalculate_filtered_order();

    assert!(app.sessions.contains_key(&session_id));
    assert!(!app.filtered_session_order.contains(&session_id));
}

#[test]
fn update_sessions_ignores_hidden_terminal_statuses() {
    let mut app = test_app();
    let session = make_session_with_status(rsi_common::types::SessionStatus::Deleted);
    let session_id = session.id;

    app.update_sessions(vec![session]);

    assert!(!app.sessions.contains_key(&session_id));
    assert!(!app.session_order.contains(&session_id));
    assert!(!app.filtered_session_order.contains(&session_id));
}

fn make_conversation_event(
    event_type: rsi_common::types::EventType,
    sequence: i32,
) -> rsi_common::types::ConversationEvent {
    rsi_common::types::ConversationEvent {
        id: 0,
        session_id: uuid::Uuid::new_v4(),
        sequence,
        event_type,
        role: Some(rsi_common::types::Role::Assistant),
        created_at: chrono::Utc::now(),
        content: "hello".to_string(),
        tool_name: None,
        tool_input: None,
        offload_id: None,
        tool_use_id: None,
        metadata: None,
    }
}

#[test]
fn apply_session_events_sets_formulation_when_running() {
    use std::collections::HashMap;
    let session = make_session_with_status(rsi_common::types::SessionStatus::Running);
    let mut state = SessionState::new(session);
    let events = vec![make_conversation_event(
        rsi_common::types::EventType::Message,
        1,
    )];
    let workflows = HashMap::new();
    App::apply_session_events(&mut state, events, EventApplyMode::Append, &workflows, None);
    assert!(
        state.formulation.is_some(),
        "formulation should be set when Running and Message event appended"
    );
    assert_eq!(state.formulation.unwrap().event_index, 0);
}

#[test]
fn apply_session_events_no_formulation_when_completed() {
    use std::collections::HashMap;
    let session = make_session_with_status(rsi_common::types::SessionStatus::Completed);
    let mut state = SessionState::new(session);
    let events = vec![make_conversation_event(
        rsi_common::types::EventType::Message,
        1,
    )];
    let workflows = HashMap::new();
    App::apply_session_events(&mut state, events, EventApplyMode::Append, &workflows, None);
    assert!(
        state.formulation.is_none(),
        "formulation should not be set when status is Completed"
    );
}

#[test]
fn apply_session_events_no_formulation_for_system_events() {
    use std::collections::HashMap;
    let session = make_session_with_status(rsi_common::types::SessionStatus::Running);
    let mut state = SessionState::new(session);
    let events = vec![make_conversation_event(
        rsi_common::types::EventType::System,
        1,
    )];
    let workflows = HashMap::new();
    App::apply_session_events(&mut state, events, EventApplyMode::Append, &workflows, None);
    assert!(
        state.formulation.is_none(),
        "formulation should not be set for System events"
    );
}

#[test]
fn apply_session_events_replace_does_not_trigger_formulation() {
    use std::collections::HashMap;
    let session = make_session_with_status(rsi_common::types::SessionStatus::Running);
    let mut state = SessionState::new(session);
    let events = vec![make_conversation_event(
        rsi_common::types::EventType::Message,
        1,
    )];
    let workflows = HashMap::new();
    App::apply_session_events(
        &mut state,
        events,
        EventApplyMode::Replace,
        &workflows,
        None,
    );
    assert!(
        state.formulation.is_none(),
        "formulation should not be set in Replace mode (initial fetch path)"
    );
}

#[test]
fn apply_session_events_sets_formulation_for_tool_use() {
    use std::collections::HashMap;
    let session = make_session_with_status(rsi_common::types::SessionStatus::Starting);
    let mut state = SessionState::new(session);
    let events = vec![make_conversation_event(
        rsi_common::types::EventType::ToolUse,
        1,
    )];
    let workflows = HashMap::new();
    App::apply_session_events(&mut state, events, EventApplyMode::Append, &workflows, None);
    assert!(
        state.formulation.is_some(),
        "formulation should be set for ToolUse events when Starting"
    );
}

// --- App::effective_topology — P1.3 derive-on-read helper ---

/// Build a session for `effective_topology` tests with a specific id and
/// no parent / topology by default.
fn mk_blank_session(id: uuid::Uuid) -> Session {
    let mut s = make_session_with_status(rsi_common::types::SessionStatus::Running);
    s.id = id;
    s
}

#[test]
fn effective_topology_orphan_returns_none() {
    let mut app = test_app();
    let sid = uuid::Uuid::new_v4();
    let mut session = mk_blank_session(sid);
    session.parent_id = None;
    session.workflow_id = None;
    session.workflow_id_override = None;
    app.sessions.insert(sid, SessionState::new(session));
    assert_eq!(app.effective_topology(sid), None);
}

#[test]
fn effective_topology_unknown_session_returns_none() {
    let app = test_app();
    let sid = uuid::Uuid::new_v4();
    // No insert — the session map does not contain `sid`.
    assert_eq!(app.effective_topology(sid), None);
}

#[test]
fn effective_topology_direct_parent_with_workflow() {
    let mut app = test_app();
    let parent_id = uuid::Uuid::new_v4();
    let child_id = uuid::Uuid::new_v4();
    let workflow_id = uuid::Uuid::new_v4();

    let mut parent = mk_blank_session(parent_id);
    parent.workflow_id = Some(workflow_id);
    let mut child = mk_blank_session(child_id);
    child.parent_id = Some(parent_id);
    child.workflow_id = None;

    app.sessions.insert(parent_id, SessionState::new(parent));
    app.sessions.insert(child_id, SessionState::new(child));

    assert_eq!(app.effective_topology(child_id), Some(workflow_id));
}

#[test]
fn effective_topology_multi_hop_walk() {
    // grandparent → parent → child; only grandparent has a workflow_id.
    let mut app = test_app();
    let gp_id = uuid::Uuid::new_v4();
    let parent_id = uuid::Uuid::new_v4();
    let child_id = uuid::Uuid::new_v4();
    let workflow_id = uuid::Uuid::new_v4();

    let mut grandparent = mk_blank_session(gp_id);
    grandparent.parent_id = None;
    grandparent.workflow_id = Some(workflow_id);
    let mut parent = mk_blank_session(parent_id);
    parent.parent_id = Some(gp_id);
    parent.workflow_id = None;
    let mut child = mk_blank_session(child_id);
    child.parent_id = Some(parent_id);
    child.workflow_id = None;

    app.sessions.insert(gp_id, SessionState::new(grandparent));
    app.sessions.insert(parent_id, SessionState::new(parent));
    app.sessions.insert(child_id, SessionState::new(child));

    assert_eq!(app.effective_topology(child_id), Some(workflow_id));
}

#[test]
fn effective_topology_cycle_returns_none() {
    // a.parent_id = b, b.parent_id = a. Helper must terminate via depth cap.
    let mut app = test_app();
    let a_id = uuid::Uuid::new_v4();
    let b_id = uuid::Uuid::new_v4();

    let mut a = mk_blank_session(a_id);
    a.parent_id = Some(b_id);
    a.workflow_id = None;
    let mut b = mk_blank_session(b_id);
    b.parent_id = Some(a_id);
    b.workflow_id = None;

    app.sessions.insert(a_id, SessionState::new(a));
    app.sessions.insert(b_id, SessionState::new(b));

    assert_eq!(app.effective_topology(a_id), None);
}

#[test]
fn effective_topology_override_short_circuits() {
    // child.workflow_id_override = Some(X); parent.workflow_id = Some(Y).
    // The override must win without walking the chain.
    let mut app = test_app();
    let parent_id = uuid::Uuid::new_v4();
    let child_id = uuid::Uuid::new_v4();
    let parent_wf = uuid::Uuid::new_v4();
    let override_wf = uuid::Uuid::new_v4();

    let mut parent = mk_blank_session(parent_id);
    parent.workflow_id = Some(parent_wf);
    let mut child = mk_blank_session(child_id);
    child.parent_id = Some(parent_id);
    child.workflow_id = None;
    child.workflow_id_override = Some(override_wf);

    app.sessions.insert(parent_id, SessionState::new(parent));
    app.sessions.insert(child_id, SessionState::new(child));

    assert_eq!(app.effective_topology(child_id), Some(override_wf));
}

/// The picker's Claude list is exactly the canonical catalog.
///
/// Client half of the divergence guard for V-002/F-124/F-158; the daemon half
/// is `crates/rsid/src/claude.rs::discover_models_returns_the_canonical_catalog`.
/// Both project from `rsi_common::claude_catalog`, so a model added to one
/// surface and not the other fails here or there.
#[test]
fn tui_claude_models_are_the_canonical_catalog() {
    use rsi_common::claude_catalog::CLAUDE_MODEL_CATALOG;

    assert_eq!(
        crate::app::CLAUDE_MODELS,
        rsi_common::claude_catalog::CLAUDE_MODEL_MENU
    );
    assert_eq!(crate::app::CLAUDE_MODELS.len(), CLAUDE_MODEL_CATALOG.len());
    for (spec, (id, display_name)) in CLAUDE_MODEL_CATALOG
        .iter()
        .zip(crate::app::CLAUDE_MODELS.iter())
    {
        assert_eq!(spec.id, *id);
        assert_eq!(spec.display_name, *display_name);
    }

    // Every model the picker offers is one the daemon can size a context
    // window for — the defect behind "Fable 5 (1M)" being computed at 128k.
    for (id, _) in crate::app::CLAUDE_MODELS {
        assert!(
            rsi_common::claude_catalog::claude_catalog_context_window(id).is_some(),
            "{id} is offered in the picker but has no catalogued context window"
        );
    }
}

/// `models_for_provider` hands the picker the canonical Claude catalog.
#[test]
fn models_for_provider_claude_returns_the_canonical_catalog() {
    use rsi_common::types::SessionProvider;

    let models = crate::app::models_for_provider(SessionProvider::Claude);
    let expected: Vec<(String, String)> = rsi_common::claude_catalog::CLAUDE_MODEL_MENU
        .iter()
        .map(|(id, name)| (id.to_string(), name.to_string()))
        .collect();
    assert_eq!(models, expected);
}
