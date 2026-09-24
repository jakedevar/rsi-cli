//! Tests for overlay input handling.

use super::*;
use crate::client::DaemonClient;
use crate::state::{DevState, PersistedState};
use crate::types::{
    GraphBrowseMode, GraphCamera, GraphDraftPersistenceState, GraphMode, GraphPickerKind,
    GraphViewport, OverlayState, Pane, PopupMode,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rsi_common::types::{
    Recurrence, ScheduleSpec, ScheduledJob, WakeMode, WorkflowExecutionSnapshot,
    WorkflowExecutionStatus,
};
use std::path::PathBuf;
use uuid::Uuid;

fn test_app() -> App {
    test_app_at(PathBuf::from("/tmp/test.sock"))
}

fn test_app_at(socket_path: PathBuf) -> App {
    DevState::clear();
    PersistedState::default().save();
    let mut app = App::new(DaemonClient::new(socket_path));
    if let Some(Pane::SessionList {
        selected_index,
        selected_session,
        ..
    }) = app.focused_pane_mut()
    {
        *selected_index = 0;
        *selected_session = None;
    }
    app
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl_key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

fn shift_key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::SHIFT)
}

fn scheduled_job(name: &str) -> ScheduledJob {
    let now = chrono::Utc::now();
    ScheduledJob {
        id: Uuid::new_v4(),
        name: name.to_string(),
        message: "test job".to_string(),
        schedule: ScheduleSpec {
            recurrence: Recurrence::Once,
            anchor: now,
        },
        last_fired_at: None,
        next_fire_at: now,
        enabled: true,
        working_dir: None,
        provider: None,
        model: None,
        project_id: None,
        created_at: now,
        updated_at: now,
        wake_mode: WakeMode::Fresh,
        wake_session_id: None,
    }
}

fn graph_review_draft_id(app: &App) -> Uuid {
    match app.overlay {
        OverlayState::GraphReview { draft_id, .. } => draft_id,
        _ => panic!("Expected GraphReview overlay"),
    }
}

fn graph_review_mode(app: &App) -> GraphMode {
    match app.overlay {
        OverlayState::GraphReview { mode, .. } => mode,
        _ => panic!("Expected GraphReview overlay"),
    }
}

fn graph_review_selected_node(app: &App) -> usize {
    match app.overlay {
        OverlayState::GraphReview { selected_node, .. } => selected_node,
        _ => panic!("Expected GraphReview overlay"),
    }
}

fn graph_review_viewport(app: &App) -> GraphViewport {
    match app.overlay {
        OverlayState::GraphReview { viewport, .. } => viewport,
        _ => panic!("Expected GraphReview overlay"),
    }
}

#[tokio::test]
async fn schedule_browser_requires_dd_before_deleting() {
    let mut app = test_app();
    // This exercises the local delete chord; connected availability is covered
    // by the action-registry matrix rather than suppressing the RPC attempt.
    app.poll.connected = true;
    app.overlay = OverlayState::ScheduleBrowser {
        jobs: vec![scheduled_job("do not delete yet")],
        selected_index: 0,
        loading: false,
        pending_delete: false,
    };

    handle_overlay_key(&mut app, key(KeyCode::Char('d'))).await;

    match &app.overlay {
        OverlayState::ScheduleBrowser {
            jobs,
            pending_delete,
            ..
        } => {
            assert_eq!(jobs.len(), 1, "a single d must not delete a job");
            assert!(*pending_delete, "the first d should arm the dd chord");
        }
        _ => panic!("expected schedule browser to remain open"),
    }

    handle_overlay_key(&mut app, key(KeyCode::Char('d'))).await;

    match &app.overlay {
        OverlayState::ScheduleBrowser {
            jobs,
            pending_delete,
            ..
        } => {
            assert_eq!(jobs.len(), 1, "a failed delete must retain the job");
            assert!(!pending_delete, "the second d must consume the dd chord");
        }
        _ => panic!("expected schedule browser to remain open"),
    }
    assert!(
        app.notifications
            .back()
            .is_some_and(|notification| notification.message.starts_with("Delete failed:")),
        "the second d must attempt the delete RPC"
    );
}

#[tokio::test]
#[allow(clippy::expect_used)]
async fn schedule_browser_single_space_toggles_selected_job() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let temp_dir = tempfile::tempdir().expect("temporary schedule socket directory");
    let socket_path = temp_dir.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).expect("schedule listener");
    let job = scheduled_job("toggle me");
    let job_id = job.id;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("schedule client");
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        let line = lines
            .next_line()
            .await
            .expect("schedule request read")
            .expect("schedule request line");
        let request: serde_json::Value =
            serde_json::from_str(&line).expect("schedule request JSON");
        assert_eq!(request["method"], "ToggleScheduledJob");
        assert_eq!(request["params"]["id"], job_id.to_string());
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"].clone(),
            "result": {"enabled": false},
        });
        writer
            .write_all(format!("{response}\n").as_bytes())
            .await
            .expect("schedule response write");
    });

    let mut app = test_app_at(socket_path);
    app.client.connect().await.expect("connect schedule client");
    app.poll.connected = true;
    app.overlay = OverlayState::ScheduleBrowser {
        jobs: vec![job],
        selected_index: 0,
        loading: false,
        pending_delete: false,
    };

    assert!(handle_overlay_key(&mut app, key(KeyCode::Char(' '))).await);
    assert!(
        matches!(
            &app.overlay,
            OverlayState::ScheduleBrowser { jobs, .. } if !jobs[0].enabled
        ),
        "notification: {:?}",
        app.notifications.back().map(|n| n.message.as_str())
    );
    assert!(!app.overlay_leader_pending);
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .expect("toggle request reached daemon")
        .expect("toggle request observed");
}

fn file_explorer_app(finder_active: bool) -> App {
    let mut app = test_app();
    app.overlay = OverlayState::FileExplorer {
        root: PathBuf::from("/tmp"),
        entries: Vec::new(),
        selected_index: 0,
        scroll_offset: 0,
        show_hidden: false,
        trash: Vec::new(),
        pending_yank: false,
        pending_delete: false,
        finder_active,
        finder_query: String::new(),
        finder_cache: vec![PathBuf::from("query file")],
        finder_results: Vec::new(),
        finder_selected: 0,
        explorer_focused: true,
    };
    app
}

#[tokio::test]
async fn file_explorer_tree_space_and_close_keys_close() {
    for code in [KeyCode::Char(' '), KeyCode::Esc, KeyCode::Char('q')] {
        let mut app = file_explorer_app(false);
        assert!(handle_overlay_key(&mut app, key(code)).await);
        assert!(matches!(app.overlay, OverlayState::None));
        assert!(!app.overlay_leader_pending);
    }
}

#[tokio::test]
async fn file_explorer_finder_keeps_printable_keys_and_escape_returns_to_tree() {
    let mut app = file_explorer_app(true);
    for code in [KeyCode::Char('q'), KeyCode::Char(' ')] {
        assert!(handle_overlay_key(&mut app, key(code)).await);
    }
    assert!(matches!(
        &app.overlay,
        OverlayState::FileExplorer { finder_query, .. } if finder_query == "q "
    ));

    assert!(handle_overlay_key(&mut app, key(KeyCode::Esc)).await);
    assert!(matches!(
        &app.overlay,
        OverlayState::FileExplorer {
            finder_active: false,
            ..
        }
    ));
    assert!(handle_overlay_key(&mut app, key(KeyCode::Esc)).await);
    assert!(matches!(app.overlay, OverlayState::None));
}

#[tokio::test]
async fn other_non_text_overlay_keeps_space_palette_leader() {
    let mut app = test_app();
    app.overlay = OverlayState::ThemePicker {
        selected_index: 0,
        original_index: 0,
    };

    assert!(handle_overlay_key(&mut app, key(KeyCode::Char(' '))).await);
    assert!(app.overlay_leader_pending);
    assert!(handle_overlay_key(&mut app, key(KeyCode::Char(';'))).await);
    assert!(matches!(app.overlay, OverlayState::CommandPalette { .. }));
}

#[tokio::test]
async fn test_model_dropdown_opens_and_closes() {
    let mut app = test_app();
    // Open the model dropdown via the widget
    app.model_dropdown = crate::types::ModelDropdownState::new(
        app.selected_provider,
        app.available_models.clone(),
        app.selected_model.as_deref(),
    );
    assert!(app.model_dropdown.open);

    // Esc closes via key handler
    handle_overlay_key(&mut app, key(KeyCode::Esc)).await;
    assert!(!app.model_dropdown.open);
}

#[tokio::test]
async fn test_model_dropdown_navigate_and_select() {
    let mut app = test_app();
    app.model_dropdown = crate::types::ModelDropdownState::new(
        app.selected_provider,
        app.available_models.clone(),
        app.selected_model.as_deref(),
    );

    // Navigate down
    handle_overlay_key(&mut app, key(KeyCode::Char('j'))).await;
    assert_eq!(app.model_dropdown.selected_index, 1);

    // Select with Enter
    let expected_model = app.available_models[1].0.clone();
    handle_overlay_key(&mut app, key(KeyCode::Enter)).await;
    assert!(!app.model_dropdown.open);
    assert_eq!(app.selected_model.as_deref(), Some(expected_model.as_str()));
}

#[tokio::test]
async fn pioneer_model_selection_preserves_provider_for_overlapping_model_prefixes() {
    let mut app = test_app();
    app.selected_provider = rsi_common::types::SessionProvider::Pioneer;
    app.available_models = vec![("gpt-5.5".to_string(), "GPT-5.5".to_string())];
    app.model_dropdown = crate::types::ModelDropdownState::new(
        app.selected_provider,
        app.available_models.clone(),
        None,
    );

    handle_overlay_key(&mut app, key(KeyCode::Enter)).await;

    assert_eq!(
        app.selected_provider,
        rsi_common::types::SessionProvider::Pioneer
    );
    assert_eq!(app.selected_model.as_deref(), Some("gpt-5.5"));
}

#[tokio::test]
async fn test_model_dropdown_direct_number() {
    let mut app = test_app();
    app.model_dropdown = crate::types::ModelDropdownState::new(
        app.selected_provider,
        app.available_models.clone(),
        app.selected_model.as_deref(),
    );

    // Press '2' to select second model directly
    let expected_model = app.available_models[1].0.clone();
    handle_overlay_key(&mut app, key(KeyCode::Char('2'))).await;
    assert!(!app.model_dropdown.open);
    assert_eq!(app.selected_model.as_deref(), Some(expected_model.as_str()));
}

#[tokio::test]
async fn test_model_dropdown_preselects_current() {
    let mut app = test_app();
    app.selected_model = Some(app.available_models[2].0.clone());
    app.model_dropdown = crate::types::ModelDropdownState::new(
        app.selected_provider,
        app.available_models.clone(),
        app.selected_model.as_deref(),
    );
    assert_eq!(app.model_dropdown.selected_index, 2);
}

#[test]
fn claude_fallback_models_put_opus_5_5_before_historical_opus() {
    let opus_models: Vec<_> = crate::app::CLAUDE_MODELS
        .iter()
        .filter(|(id, _)| id.contains("opus"))
        .copied()
        .collect();
    assert_eq!(
        opus_models.first(),
        Some(&("claude-opus-5-5", "Opus 5.5 (1M)"))
    );
    assert_eq!(
        opus_models
            .iter()
            .filter(|(id, _)| *id == "claude-opus-5-5")
            .count(),
        1
    );
}

#[test]
fn codex_fallback_models_prioritize_gpt_6() {
    assert_eq!(
        crate::app::CODEX_MODELS,
        [
            ("gpt-6-sol", "GPT-6-Sol"),
            ("gpt-6-luna", "GPT-6-Luna"),
            ("gpt-6-astra", "GPT-6-Astra"),
            ("gpt-5.5", "GPT-5.5"),
            ("gpt-5.2", "GPT-5.2"),
        ]
    );
}

#[test]
fn prompt_modal_compiler_uses_its_selected_remote_target() {
    let mut app = test_app();
    app.settings.prompt_processor.provider = rsi_common::types::SessionProvider::Local;
    app.settings.prompt_processor.model = "gemma4:e4b".to_string();

    let config = selected_prompt_processor_config(
        &app.settings.prompt_processor,
        app.selected_model.clone(),
        app.selected_provider,
        None,
        Some("claude-sonnet-5".to_string()),
        Some(rsi_common::types::SessionProvider::Claude),
        None,
    );

    assert_eq!(config.model, "claude-sonnet-5");
    assert_eq!(config.provider, rsi_common::types::SessionProvider::Claude);
}

#[test]
fn main_overlay_effort_cycle_follows_model_ladder_and_default() {
    let mut app = test_app();
    app.selected_provider = rsi_common::types::SessionProvider::Claude;
    app.selected_model = Some("claude-opus-5".to_string());
    app.selected_effort = None;

    for expected in ["xhigh", "max", "low", "medium", "high", "xhigh"] {
        cycle_effort(&mut app, None, None);
        assert_eq!(app.selected_effort.as_deref(), Some(expected));
    }

    app.selected_model = Some("claude-sonnet-4-6".to_string());
    app.selected_effort = Some("high".to_string());
    cycle_effort(&mut app, None, None);
    assert_eq!(app.selected_effort.as_deref(), Some("max"));
    cycle_effort(&mut app, None, None);
    assert_eq!(app.selected_effort.as_deref(), Some("low"));

    app.selected_provider = rsi_common::types::SessionProvider::Codex;
    app.selected_model = Some("gpt-6-astra".to_string());
    app.selected_effort = Some("high".to_string());
    for expected in ["xhigh", "max", "ultra", "low"] {
        cycle_effort(&mut app, None, None);
        assert_eq!(app.selected_effort.as_deref(), Some(expected));
    }

    app.selected_model = Some("gpt-6-astra".to_string());
    app.selected_effort = Some("max".to_string());
    cycle_effort(&mut app, None, None);
    assert_eq!(app.selected_effort.as_deref(), Some("ultra"));

    app.selected_model = Some("gpt-6-astra".to_string());
    app.selected_effort = Some("xhigh".to_string());
    cycle_effort(&mut app, None, None);
    assert_eq!(app.selected_effort.as_deref(), Some("max"));
}

#[tokio::test]
async fn main_model_selection_clears_unsupported_effort() {
    let mut app = test_app();
    app.selected_provider = rsi_common::types::SessionProvider::Codex;
    app.selected_model = Some("gpt-6-astra".to_string());
    app.selected_effort = Some("ultra".to_string());

    app.model_dropdown = crate::types::ModelDropdownState::new(
        rsi_common::types::SessionProvider::Codex,
        vec![("gpt-5.5".to_string(), "GPT-5.5".to_string())],
        None,
    );
    handle_overlay_key(&mut app, key(KeyCode::Enter)).await;

    assert_eq!(app.selected_model.as_deref(), Some("gpt-5.5"));
    assert_eq!(app.selected_effort, None);
}

#[tokio::test]
async fn main_provider_cycle_reaches_codex_app_server_before_empty_harness_catalog() {
    let mut app = test_app();
    app.selected_provider = rsi_common::types::SessionProvider::Antigravity;
    app.selected_model = Some("gpt-6-astra".to_string());
    app.selected_effort = Some("ultra".to_string());
    app.model_dropdown = crate::types::ModelDropdownState::new(
        rsi_common::types::SessionProvider::Antigravity,
        crate::app::models_for_provider(rsi_common::types::SessionProvider::Antigravity),
        None,
    );

    handle_overlay_key(&mut app, key(KeyCode::Tab)).await;

    assert_eq!(
        app.selected_provider,
        rsi_common::types::SessionProvider::CodexAppServer
    );
    assert_eq!(app.selected_model.as_deref(), Some("gpt-6-sol"));
    assert_eq!(app.selected_effort.as_deref(), Some("ultra"));

    handle_overlay_key(&mut app, key(KeyCode::Tab)).await;

    assert_eq!(
        app.selected_provider,
        rsi_common::types::SessionProvider::Harness
    );
    assert_eq!(app.selected_model, None);
    assert_eq!(app.selected_effort, None);
}

#[tokio::test]
async fn overlay_prompt_picker_reconciles_typed_prompt_submission_effort() {
    let mut app = test_app();
    app.selected_provider = rsi_common::types::SessionProvider::Codex;
    app.selected_model = Some("gpt-6-astra".to_string());
    app.selected_effort = Some("ultra".to_string());
    prompt::open_typed_prompt(&mut app, rsi_common::types::SessionKind::Task, None);
    app.overlay = app.input_overlays.pop().expect("typed prompt should open");

    if let OverlayState::Prompt { model_dropdown, .. } = &mut app.overlay {
        *model_dropdown = crate::types::ModelDropdownState::new(
            rsi_common::types::SessionProvider::Codex,
            vec![("gpt-5.5".to_string(), "GPT-5.5".to_string())],
            None,
        );
    }
    handle_overlay_key(&mut app, key(KeyCode::Enter)).await;

    match &app.overlay {
        OverlayState::Prompt {
            purpose: crate::types::PromptPurpose::CreateTyped { .. },
            model_override,
            provider_override,
            ..
        } => {
            assert_eq!(model_override.as_deref(), Some("gpt-5.5"));
            assert_eq!(
                *provider_override,
                Some(rsi_common::types::SessionProvider::Codex)
            );
        }
        _ => panic!("expected typed prompt overlay"),
    }
    assert_eq!(app.selected_effort, None);
}

#[tokio::test]
async fn overlay_prompt_provider_cycle_targets_discovery_without_changing_global_provider() {
    let mut app = test_app();
    app.selected_provider = rsi_common::types::SessionProvider::Codex;
    prompt::open_typed_prompt(&mut app, rsi_common::types::SessionKind::Task, None);
    app.overlay = app.input_overlays.pop().expect("typed prompt should open");

    if let OverlayState::Prompt { model_dropdown, .. } = &mut app.overlay {
        *model_dropdown = crate::types::ModelDropdownState::new(
            rsi_common::types::SessionProvider::Codex,
            crate::app::models_for_provider(rsi_common::types::SessionProvider::Codex),
            None,
        );
    }
    handle_overlay_key(&mut app, key(KeyCode::Tab)).await;

    assert_eq!(
        app.selected_provider,
        rsi_common::types::SessionProvider::Codex
    );
    assert_eq!(
        app.model_refresh_provider,
        Some(rsi_common::types::SessionProvider::Pioneer)
    );
    assert!(app.needs_model_refresh);
}

#[tokio::test]
async fn input_prompt_picker_reconciles_blank_and_taskrabbit_submission_effort() {
    let mut app = test_app();
    for open_prompt in [
        prompt::open_blank_popup as fn(&mut App),
        prompt::open_taskrabbit_popup,
    ] {
        app.selected_provider = rsi_common::types::SessionProvider::Codex;
        app.selected_model = Some("gpt-6-astra".to_string());
        app.selected_effort = Some("ultra".to_string());
        open_prompt(&mut app);

        if let Some(OverlayState::Prompt { model_dropdown, .. }) = app.input_overlays.last_mut() {
            *model_dropdown = crate::types::ModelDropdownState::new(
                rsi_common::types::SessionProvider::Codex,
                vec![("gpt-5.5".to_string(), "GPT-5.5".to_string())],
                None,
            );
        }
        handle_overlay_key(&mut app, key(KeyCode::Enter)).await;

        match app.focused_input_overlay() {
            Some(OverlayState::Prompt {
                purpose:
                    crate::types::PromptPurpose::Blank | crate::types::PromptPurpose::TaskRabbit,
                model_override,
                provider_override,
                ..
            }) => {
                assert_eq!(model_override.as_deref(), Some("gpt-5.5"));
                assert_eq!(
                    *provider_override,
                    Some(rsi_common::types::SessionProvider::Codex)
                );
            }
            _ => panic!("expected new-session prompt overlay"),
        }
        assert_eq!(app.selected_effort, None);
        app.input_overlays.clear();
    }
}

#[tokio::test]
async fn input_prompt_provider_cycle_targets_discovery_without_changing_global_provider() {
    let mut app = test_app();
    app.selected_provider = rsi_common::types::SessionProvider::Codex;
    prompt::open_blank_popup(&mut app);

    if let Some(OverlayState::Prompt { model_dropdown, .. }) = app.input_overlays.last_mut() {
        *model_dropdown = crate::types::ModelDropdownState::new(
            rsi_common::types::SessionProvider::Codex,
            crate::app::models_for_provider(rsi_common::types::SessionProvider::Codex),
            None,
        );
    }
    handle_overlay_key(&mut app, key(KeyCode::Tab)).await;

    assert_eq!(
        app.selected_provider,
        rsi_common::types::SessionProvider::Codex
    );
    assert_eq!(
        app.model_refresh_provider,
        Some(rsi_common::types::SessionProvider::Pioneer)
    );
    assert!(app.needs_model_refresh);
}

#[tokio::test]
async fn test_overlay_returns_false_when_none() {
    let mut app = test_app();
    let consumed = handle_overlay_key(&mut app, key(KeyCode::Char('j'))).await;
    assert!(!consumed);
}

#[tokio::test]
async fn test_overlay_returns_true_when_active() {
    let mut app = test_app();
    prompt::open_blank_popup(&mut app);
    let consumed = handle_overlay_key(&mut app, key(KeyCode::Char('a'))).await;
    assert!(consumed);
}

#[tokio::test]
async fn test_graph_review_reopens_active_draft() {
    let mut app = test_app();

    graph::open_graph_review(&mut app).await;
    let draft_id = match app.overlay {
        OverlayState::GraphReview { draft_id, .. } => draft_id,
        _ => panic!("Expected GraphReview overlay"),
    };

    {
        let draft = app
            .graph_draft_mut(&draft_id)
            .expect("graph draft should exist");
        draft
            .workflow
            .nodes
            .push(rsi_graph::format::NodeDef::action(
                "draft-node",
                "Draft Node",
            ));
    }
    app.mark_graph_draft_dirty(draft_id);

    app.overlay = OverlayState::None;
    graph::open_graph_review(&mut app).await;

    match app.overlay {
        OverlayState::GraphReview {
            draft_id: reopened_id,
            ..
        } => assert_eq!(reopened_id, draft_id),
        _ => panic!("Expected GraphReview overlay"),
    }

    let draft = app
        .graph_draft(&draft_id)
        .expect("graph draft should persist");
    assert_eq!(draft.workflow.nodes.len(), 1);
}

#[tokio::test]
async fn test_graph_review_run_blocks_invalid_saved_workflow() {
    let mut app = test_app();

    graph::open_graph_review(&mut app).await;
    let draft_id = match app.overlay {
        OverlayState::GraphReview { draft_id, .. } => draft_id,
        _ => panic!("Expected GraphReview overlay"),
    };

    {
        let draft = app
            .graph_draft_mut(&draft_id)
            .expect("graph draft should exist");
        draft.workflow_id = Some(uuid::Uuid::new_v4());
        draft.persistence_state = GraphDraftPersistenceState::Clean;
        draft
            .workflow
            .nodes
            .push(rsi_graph::format::NodeDef::topology("hub", "Hub", "hub"));
    }

    handle_overlay_key(&mut app, key(KeyCode::Char('r'))).await;

    let draft = app
        .graph_draft(&draft_id)
        .expect("graph draft should still exist");
    assert!(draft.last_execution_id.is_none());
    assert!(
        app.notifications
            .back()
            .expect("expected validation notification")
            .message
            .contains("Execution blocked")
    );
}

#[tokio::test]
async fn test_graph_review_empty_state_topology_picker_populates_draft() {
    let mut app = test_app();

    graph::open_graph_review(&mut app).await;
    assert!(matches!(
        graph_review_mode(&app),
        GraphMode::Navigate {
            camera: GraphCamera::FollowSelection
        }
    ));

    handle_overlay_key(&mut app, key(KeyCode::Char('t'))).await;
    assert!(matches!(
        graph_review_mode(&app),
        GraphMode::Picker {
            kind: GraphPickerKind::Topology,
            ..
        }
    ));

    handle_overlay_key(&mut app, key(KeyCode::Enter)).await;

    let draft_id = graph_review_draft_id(&app);
    let draft = app
        .graph_draft(&draft_id)
        .expect("graph draft should exist after selecting a template");
    assert!(!draft.workflow.nodes.is_empty());
    assert_eq!(draft.persistence_state, GraphDraftPersistenceState::Dirty);
    assert!(matches!(
        graph_review_mode(&app),
        GraphMode::Navigate {
            camera: GraphCamera::FollowSelection
        }
    ));
}

#[tokio::test]
async fn test_graph_review_spatial_navigation_uses_canvas_layout() {
    let mut app = test_app();

    graph::open_graph_review(&mut app).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('t'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Enter)).await;

    let draft_id = graph_review_draft_id(&app);
    let initial_node_id = app
        .graph_draft(&draft_id)
        .expect("graph draft should exist after selecting hub template")
        .workflow
        .nodes[graph_review_selected_node(&app)]
    .id
    .clone();
    assert_eq!(initial_node_id, "router");

    handle_overlay_key(&mut app, key(KeyCode::Char('l'))).await;

    let selected_node_id = app
        .graph_draft(&draft_id)
        .expect("graph draft should still exist")
        .workflow
        .nodes[graph_review_selected_node(&app)]
    .id
    .clone();
    assert_eq!(selected_node_id, "tests");
}

#[tokio::test]
async fn test_graph_review_manual_pan_recenters_and_selection_resets_follow_mode() {
    let mut app = test_app();

    graph::open_graph_review(&mut app).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('t'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Enter)).await;

    handle_overlay_key(&mut app, key(KeyCode::Char('L'))).await;
    assert!(matches!(
        graph_review_mode(&app),
        GraphMode::Navigate {
            camera: GraphCamera::Manual
        }
    ));
    assert!(!graph_review_viewport(&app).is_centered());

    handle_overlay_key(&mut app, key(KeyCode::Char('c'))).await;
    assert!(matches!(
        graph_review_mode(&app),
        GraphMode::Navigate {
            camera: GraphCamera::FollowSelection
        }
    ));
    assert!(graph_review_viewport(&app).is_centered());

    handle_overlay_key(&mut app, key(KeyCode::Char('L'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('l'))).await;
    assert!(matches!(
        graph_review_mode(&app),
        GraphMode::Navigate {
            camera: GraphCamera::FollowSelection
        }
    ));
    assert!(graph_review_viewport(&app).is_centered());
}

#[tokio::test]
async fn test_graph_review_picker_esc_restores_previous_mode() {
    let mut app = test_app();

    graph::open_graph_review(&mut app).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('t'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Esc)).await;

    assert!(matches!(
        graph_review_mode(&app),
        GraphMode::Navigate {
            camera: GraphCamera::FollowSelection
        }
    ));
}

#[tokio::test]
async fn test_graph_review_detail_esc_steps_back_and_q_closes() {
    let mut app = test_app();

    graph::open_graph_review(&mut app).await;
    let draft_id = graph_review_draft_id(&app);
    {
        let draft = app
            .graph_draft_mut(&draft_id)
            .expect("graph draft should exist");
        draft
            .workflow
            .nodes
            .push(rsi_graph::format::NodeDef::action(
                "draft-node",
                "Draft Node",
            ));
    }
    app.mark_graph_draft_dirty(draft_id);

    handle_overlay_key(&mut app, key(KeyCode::Enter)).await;
    assert!(matches!(
        graph_review_mode(&app),
        GraphMode::Detail {
            camera: GraphCamera::FollowSelection
        }
    ));

    handle_overlay_key(&mut app, key(KeyCode::Esc)).await;
    assert!(matches!(
        graph_review_mode(&app),
        GraphMode::Navigate {
            camera: GraphCamera::FollowSelection
        }
    ));

    handle_overlay_key(&mut app, key(KeyCode::Enter)).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('q'))).await;
    assert!(matches!(app.overlay, OverlayState::None));
}

#[tokio::test]
async fn test_graph_review_reopens_active_execution_in_executing_mode() {
    let mut app = test_app();

    graph::open_graph_review(&mut app).await;
    let draft_id = graph_review_draft_id(&app);
    let workflow_id = Uuid::new_v4();
    let execution_id = Uuid::new_v4();

    {
        let draft = app
            .graph_draft_mut(&draft_id)
            .expect("graph draft should exist");
        draft.workflow_id = Some(workflow_id);
        draft.last_execution_id = Some(execution_id);
        draft
            .workflow
            .nodes
            .push(rsi_graph::format::NodeDef::action(
                "draft-node",
                "Draft Node",
            ));
    }

    app.graph_executions.insert(
        execution_id,
        WorkflowExecutionSnapshot {
            execution_id,
            workflow_id,
            workflow_name: "Test".to_string(),
            status: WorkflowExecutionStatus::Running,
            accepted_at: chrono::Utc::now(),
            started_at: None,
            finished_at: None,
            dry_run: false,
            input: None,
            output: None,
            error: None,
            last_sequence: 0,
            row_version: None,
            blocked_attempt_id: None,
            blocked_reason: None,
            updates: Vec::new(),
        },
    );

    app.overlay = OverlayState::None;
    graph::open_graph_review(&mut app).await;

    assert!(matches!(
        graph_review_mode(&app),
        GraphMode::Executing {
            previous_mode: GraphBrowseMode::Navigate,
            camera: GraphCamera::FollowSelection
        }
    ));

    if let Some(snapshot) = app.graph_executions.get_mut(&execution_id) {
        snapshot.status = WorkflowExecutionStatus::Succeeded;
    }
    app.reconcile_graph_review_execution_mode();

    assert!(matches!(
        graph_review_mode(&app),
        GraphMode::Navigate {
            camera: GraphCamera::FollowSelection
        }
    ));
}

/// #634: a durable execution blocked on preserved work keeps its draft
/// linkage, stays active (interruptible), and resumes with the next update.
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn test_graph_review_keeps_blocked_topology_execution_linked() {
    let mut app = test_app();
    graph::open_graph_review(&mut app).await;
    let draft_id = graph_review_draft_id(&app);
    let workflow_id = Uuid::new_v4();
    let execution_id = Uuid::new_v4();
    app.graph_draft_mut(&draft_id)
        .expect("graph draft should exist")
        .workflow_id = Some(workflow_id);

    let update =
        |sequence: u64, status: WorkflowExecutionStatus| rsi_common::types::GraphExecutionUpdate {
            execution_id,
            workflow_id,
            node_id: Some("A".to_string()),
            sequence,
            status,
            node_state: None,
            finished: false,
            error: None,
            output_preview: None,
            updated_at: chrono::Utc::now(),
        };
    assert!(app.apply_graph_execution_update(update(1, WorkflowExecutionStatus::Running)));
    assert!(app.apply_graph_execution_update(update(2, WorkflowExecutionStatus::Blocked)));
    assert_eq!(
        app.graph_draft(&draft_id).unwrap().last_execution_id,
        Some(execution_id)
    );
    assert_eq!(
        app.graph_execution_for_draft(&draft_id).unwrap().status,
        WorkflowExecutionStatus::Blocked
    );
    assert!(app.graph_execution_is_active_for_draft(&draft_id));

    assert!(app.apply_graph_execution_update(update(3, WorkflowExecutionStatus::Running)));
    assert_eq!(
        app.graph_execution_for_draft(&draft_id).unwrap().status,
        WorkflowExecutionStatus::Running
    );
}

#[test]
#[allow(clippy::unwrap_used)]
fn test_topology_resolve_command_parses_every_action() {
    use rsi_common::rpc::TopologyAttemptAction as Action;
    let id = Uuid::new_v4();
    let commit = "a".repeat(40);
    assert_eq!(
        graph::parse_topology_resolve("inspect").unwrap(),
        graph::TopologyResolveCommand {
            execution_id: None,
            action: Action::Inspect,
            confirm: None,
        }
    );
    assert_eq!(
        graph::parse_topology_resolve(&format!("{id} discard {commit}")).unwrap(),
        graph::TopologyResolveCommand {
            execution_id: Some(id),
            action: Action::Discard,
            confirm: Some(commit),
        }
    );
    for (text, action) in [("accept", Action::Accept), ("retry", Action::Retry)] {
        assert_eq!(graph::parse_topology_resolve(text).unwrap().action, action);
    }
    assert!(
        graph::parse_topology_resolve("merge")
            .unwrap_err()
            .contains("usage: :topology-resolve")
    );
}

#[tokio::test]
async fn test_insert_mode_types_text() {
    let mut app = test_app();
    prompt::open_blank_popup(&mut app);

    handle_overlay_key(&mut app, key(KeyCode::Char('h'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('i'))).await;

    if let Some(OverlayState::Prompt { surface, .. }) = app.focused_input_overlay() {
        assert_eq!(surface.textarea.lines(), &["hi"]);
    } else {
        panic!("Expected Prompt overlay");
    }
}

#[tokio::test]
async fn test_esc_switches_to_normal_mode() {
    let mut app = test_app();
    prompt::open_blank_popup(&mut app);

    handle_overlay_key(&mut app, key(KeyCode::Esc)).await;

    if let Some(OverlayState::Prompt { surface, .. }) = app.focused_input_overlay() {
        assert_eq!(surface.mode, PopupMode::Normal);
    } else {
        panic!("Expected Prompt overlay");
    }
}

#[tokio::test]
async fn test_normal_mode_ctrl_q_closes() {
    let mut app = test_app();
    prompt::open_blank_popup(&mut app);

    // Switch to normal mode
    handle_overlay_key(&mut app, key(KeyCode::Esc)).await;
    // Press Ctrl+Q to close.
    handle_overlay_key(&mut app, ctrl_key(KeyCode::Char('q'))).await;

    assert!(app.input_overlays.is_empty());
}

#[tokio::test]
async fn test_insert_mode_ctrl_q_closes_new_session_modal() {
    let mut app = test_app();
    prompt::open_blank_popup(&mut app);

    // Blank popup opens in insert mode; Ctrl+Q must still close it.
    handle_overlay_key(&mut app, ctrl_key(KeyCode::Char('q'))).await;

    assert!(app.input_overlays.is_empty());
}

#[tokio::test]
async fn test_ctrl_e_uses_prompt_override_when_default_model_lacks_effort() {
    let mut app = test_app();
    // Global default model supports no effort ladder.
    app.selected_provider = rsi_common::types::SessionProvider::Claude;
    app.selected_model = Some("claude-haiku-5".to_string());
    app.selected_effort = None;
    prompt::open_blank_popup(&mut app);

    // Switch the modal's model to one that supports effort.
    if let Some(OverlayState::Prompt {
        model_override,
        provider_override,
        ..
    }) = app.input_overlays.last_mut()
    {
        *model_override = Some("claude-opus-5".to_string());
        *provider_override = Some(rsi_common::types::SessionProvider::Claude);
    }

    // Ctrl+E requires normal mode.
    handle_overlay_key(&mut app, key(KeyCode::Esc)).await;
    handle_overlay_key(&mut app, ctrl_key(KeyCode::Char('e'))).await;

    // Cycling must follow the override model's ladder (Opus 5 → xhigh).
    assert_eq!(app.selected_effort.as_deref(), Some("xhigh"));
}

#[tokio::test]
async fn test_normal_mode_esc_does_not_close() {
    let mut app = test_app();
    prompt::open_blank_popup(&mut app);

    // Switch to normal mode
    handle_overlay_key(&mut app, key(KeyCode::Esc)).await;
    // Press Esc again — should NOT close (Esc only switches modes, not close)
    handle_overlay_key(&mut app, key(KeyCode::Esc)).await;

    // Overlay should still be open (Esc is not a close key in normal mode)
    if let Some(OverlayState::Prompt { surface, .. }) = app.focused_input_overlay() {
        assert_eq!(surface.mode, PopupMode::Normal);
    } else {
        panic!("Expected Prompt overlay in normal mode");
    }
}

#[tokio::test]
async fn test_normal_mode_i_reenters_insert() {
    let mut app = test_app();
    prompt::open_blank_popup(&mut app);

    // Switch to normal, then back to insert
    handle_overlay_key(&mut app, key(KeyCode::Esc)).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('i'))).await;

    if let Some(OverlayState::Prompt { surface, .. }) = app.focused_input_overlay() {
        assert_eq!(surface.mode, PopupMode::Insert);
    } else {
        panic!("Expected Prompt overlay");
    }
}

#[tokio::test]
async fn test_ctrl_enter_config_pending_preserves_prompt_draft() {
    let mut app = test_app();
    prompt::open_blank_popup(&mut app);

    // Type some text
    handle_overlay_key(&mut app, key(KeyCode::Char('t'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('e'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('s'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('t'))).await;

    // Submit with Ctrl+Enter
    handle_overlay_key(&mut app, ctrl_key(KeyCode::Enter)).await;

    let Some(OverlayState::Prompt { surface, .. }) = app.focused_input_overlay() else {
        panic!("config-pending rejection preserves the prompt overlay");
    };
    assert_eq!(surface.textarea.lines(), &["test"]);
    assert!(app.notifications.iter().any(|notification| {
        notification
            .message
            .contains("Daemon configuration is still loading; draft preserved")
    }));
}

async fn assert_rejected_prompt_navigation_intent_is_not_reused(stacked: bool, key_code: KeyCode) {
    let mut app = test_app();
    prompt::open_blank_popup(&mut app);
    if !stacked {
        app.overlay = app.input_overlays.pop().expect("legacy prompt overlay");
    }
    handle_overlay_key(&mut app, key(KeyCode::Char('d'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('r'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('a'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('f'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('t'))).await;
    handle_overlay_key(&mut app, ctrl_key(key_code)).await;

    assert!(app.accepted_launch_placements.is_empty());
    let prompt = if stacked {
        app.focused_input_overlay()
    } else {
        Some(&app.overlay)
    };
    assert!(matches!(prompt, Some(OverlayState::Prompt { .. })));

    let unrelated_id = Uuid::new_v4();
    let mut unrelated = crate::app::app_test_helpers::baseline_session(
        unrelated_id,
        rsi_common::types::SessionKind::Standard,
    );
    unrelated.title = Some("Unrelated preserved insertion".to_string());
    app.update_sessions(vec![unrelated]);

    assert_eq!(app.tabs.len(), 1);
    assert!(matches!(
        app.tabs[app.active_tab].layout,
        crate::types::SplitNode::Leaf { .. }
    ));
    assert_eq!(
        app.sessions[&unrelated_id].session.title.as_deref(),
        Some("Unrelated preserved insertion")
    );
}

#[tokio::test]
async fn rejected_ctrl_tab_and_split_do_not_leak_from_legacy_prompt() {
    assert_rejected_prompt_navigation_intent_is_not_reused(false, KeyCode::Char('t')).await;
    assert_rejected_prompt_navigation_intent_is_not_reused(false, KeyCode::Char('s')).await;
}

#[tokio::test]
async fn rejected_ctrl_tab_and_split_do_not_leak_from_stacked_prompt() {
    assert_rejected_prompt_navigation_intent_is_not_reused(true, KeyCode::Char('t')).await;
    assert_rejected_prompt_navigation_intent_is_not_reused(true, KeyCode::Char('s')).await;
}

async fn assert_semantic_launch_response_owns_surface_and_placement(
    stacked: bool,
    key_code: KeyCode,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let temp_dir = tempfile::tempdir().expect("temporary prompt socket directory");
    let socket_path = temp_dir.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).expect("prompt listener");
    let returned_id = Uuid::new_v4();
    let server = tokio::spawn(async move {
        for attempt in 0..2 {
            let (stream, _) = listener.accept().await.expect("prompt client");
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let line = lines
                .next_line()
                .await
                .expect("prompt request read")
                .expect("prompt request line");
            let request: serde_json::Value =
                serde_json::from_str(&line).expect("prompt request JSON");
            assert_eq!(request["method"], "LaunchSession");
            assert_eq!(request["params"]["query"], "durable draft");
            let response = if attempt == 0 {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"].clone(),
                    "error": {"code": -32071, "message": "launch rejected exactly"},
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
                .expect("prompt response write");
        }
    });

    let mut app = test_app_at(socket_path);
    app.poll.connected = true;
    app.poll.authoritative_config_ready = true;
    prompt::open_blank_popup(&mut app);
    if !stacked {
        app.overlay = app.input_overlays.pop().expect("legacy prompt");
    }
    for c in "durable draft".chars() {
        handle_overlay_key(&mut app, key(KeyCode::Char(c))).await;
    }
    handle_overlay_key(&mut app, ctrl_key(key_code)).await;

    let draft = if stacked {
        app.focused_input_overlay()
    } else {
        Some(&app.overlay)
    };
    assert!(matches!(
        draft,
        Some(OverlayState::Prompt { surface, .. })
            if surface.content_for_send() == "durable draft"
    ));
    assert!(app.interactive_launch_pending.is_some());

    let unrelated_id = Uuid::new_v4();
    let mut unrelated = crate::app::app_test_helpers::baseline_session(
        unrelated_id,
        rsi_common::types::SessionKind::Standard,
    );
    unrelated.title = Some("Unrelated arrives before acceptance".to_string());
    app.upsert_session(unrelated);
    assert_eq!(app.tabs.len(), 1);
    assert!(matches!(
        app.tabs[app.active_tab].layout,
        crate::types::SplitNode::Leaf { .. }
    ));

    let rejected = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        app.interactive_launch_rx.recv(),
    )
    .await
    .expect("semantic rejection deadline")
    .expect("semantic rejection result");
    assert!(app.apply_interactive_launch_result(rejected));
    let draft = if stacked {
        app.focused_input_overlay()
    } else {
        Some(&app.overlay)
    };
    assert!(matches!(
        draft,
        Some(OverlayState::Prompt { surface, .. })
            if surface.content_for_send() == "durable draft"
    ));
    assert!(
        app.notifications
            .iter()
            .any(|notification| { notification.message.contains("launch rejected exactly") })
    );
    assert!(app.accepted_launch_placements.is_empty());

    handle_overlay_key(&mut app, ctrl_key(key_code)).await;
    let accepted = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        app.interactive_launch_rx.recv(),
    )
    .await
    .expect("semantic acceptance deadline")
    .expect("semantic acceptance result");
    assert!(app.apply_interactive_launch_result(accepted));
    assert!(app.accepted_launch_placements.contains_key(&returned_id));

    let another_id = Uuid::new_v4();
    let mut another = crate::app::app_test_helpers::baseline_session(
        another_id,
        rsi_common::types::SessionKind::Standard,
    );
    another.title = Some("Another unrelated preserved insertion".to_string());
    app.upsert_session(another);
    assert!(app.accepted_launch_placements.contains_key(&returned_id));
    assert_eq!(app.tabs.len(), 1);
    assert!(matches!(
        app.tabs[app.active_tab].layout,
        crate::types::SplitNode::Leaf { .. }
    ));

    let mut returned = crate::app::app_test_helpers::baseline_session(
        returned_id,
        rsi_common::types::SessionKind::Standard,
    );
    returned.title = Some("Exact accepted launch".to_string());
    app.upsert_session(returned);
    assert_eq!(
        app.sessions[&returned_id].session.title.as_deref(),
        Some("Exact accepted launch")
    );
    if key_code == KeyCode::Char('t') {
        assert_eq!(app.tabs.len(), 2);
    } else {
        assert!(matches!(
            app.tabs[app.active_tab].layout,
            crate::types::SplitNode::Split { .. }
        ));
    }
    server.await.expect("prompt server");
}

#[tokio::test]
async fn semantic_rejection_and_retry_bind_ctrl_placement_to_exact_legacy_launch() {
    assert_semantic_launch_response_owns_surface_and_placement(false, KeyCode::Char('t')).await;
    assert_semantic_launch_response_owns_surface_and_placement(false, KeyCode::Char('s')).await;
}

#[tokio::test]
async fn semantic_rejection_and_retry_bind_ctrl_placement_to_exact_stacked_launch() {
    assert_semantic_launch_response_owns_surface_and_placement(true, KeyCode::Char('t')).await;
    assert_semantic_launch_response_owns_surface_and_placement(true, KeyCode::Char('s')).await;
}

#[tokio::test]
async fn taskrabbit_and_typed_prompts_survive_semantic_launch_rejection() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    for typed in [false, true] {
        let temp_dir = tempfile::tempdir().expect("temporary typed prompt socket directory");
        let socket_path = temp_dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("typed prompt listener");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("typed prompt client");
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let line = lines
                .next_line()
                .await
                .expect("typed prompt request read")
                .expect("typed prompt request line");
            let request: serde_json::Value =
                serde_json::from_str(&line).expect("typed prompt request JSON");
            assert_eq!(request["method"], "LaunchSession");
            assert_eq!(
                request["params"]["session_kind"],
                if typed { "Task" } else { "TaskRabbit" }
            );
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"].clone(),
                "error": {"code": -32072, "message": "typed launch rejected exactly"},
            });
            writer
                .write_all(format!("{response}\n").as_bytes())
                .await
                .expect("typed prompt response write");
        });

        let mut app = test_app_at(socket_path);
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        if typed {
            prompt::open_typed_prompt(&mut app, rsi_common::types::SessionKind::Task, None);
        } else {
            prompt::open_taskrabbit_popup(&mut app);
        }
        for c in "keep exact draft".chars() {
            handle_overlay_key(&mut app, key(KeyCode::Char(c))).await;
        }
        handle_overlay_key(&mut app, ctrl_key(KeyCode::Enter)).await;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.interactive_launch_rx.recv(),
        )
        .await
        .expect("typed rejection deadline")
        .expect("typed rejection result");
        assert!(app.apply_interactive_launch_result(result));
        assert!(matches!(
            app.focused_input_overlay(),
            Some(OverlayState::Prompt { surface, .. })
                if surface.content_for_send() == "keep exact draft"
        ));
        assert!(app.notifications.iter().any(|notification| {
            notification
                .message
                .contains("typed launch rejected exactly")
        }));
        server.await.expect("typed prompt server");
    }
}

#[tokio::test]
async fn ctrl_tab_and_split_submit_continue_without_launch_navigation_intent() {
    for key_code in [KeyCode::Char('t'), KeyCode::Char('s')] {
        let mut app = test_app();
        let session_id = Uuid::new_v4();
        let session = crate::app::app_test_helpers::baseline_session(
            session_id,
            rsi_common::types::SessionKind::Standard,
        );
        app.sessions
            .insert(session_id, crate::types::SessionState::new(session));
        prompt::open_continue_popup(&mut app, session_id);
        handle_overlay_key(&mut app, key(KeyCode::Char('x'))).await;

        handle_overlay_key(&mut app, ctrl_key(key_code)).await;

        assert!(app.accepted_launch_placements.is_empty());
        assert!(matches!(app.overlay, OverlayState::None));
        assert!(
            app.notifications
                .iter()
                .any(|notification| { notification.message.contains("Not connected to daemon") })
        );
    }
}

#[tokio::test]
async fn test_ctrl_enter_empty_does_not_launch() {
    let mut app = test_app();
    prompt::open_blank_popup(&mut app);

    // Submit empty content — overlay closes but no session launched
    handle_overlay_key(&mut app, ctrl_key(KeyCode::Enter)).await;

    assert!(app.input_overlays.is_empty());
    // No notifications because nothing was launched
    assert!(app.notifications.is_empty());
}

#[tokio::test]
async fn test_multiline_input() {
    // Under the new default (submit_on_enter: true), Shift+Enter is the
    // newline-insert key; plain Enter would submit the overlay instead.
    let mut app = test_app();
    prompt::open_blank_popup(&mut app);

    handle_overlay_key(&mut app, key(KeyCode::Char('a'))).await;
    handle_overlay_key(&mut app, shift_key(KeyCode::Enter)).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('b'))).await;

    if let Some(OverlayState::Prompt { surface, .. }) = app.focused_input_overlay() {
        assert_eq!(surface.textarea.lines(), &["a", "b"]);
    } else {
        panic!("Expected Prompt overlay");
    }
}

#[tokio::test]
async fn test_plain_enter_inserts_newline_in_session_prompt() {
    let mut app = test_app();
    assert!(app.settings.submit_on_enter);
    prompt::open_blank_popup(&mut app);

    // Type some text
    handle_overlay_key(&mut app, key(KeyCode::Char('t'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('e'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('s'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('t'))).await;

    // Session prompt overlays intentionally require Ctrl+Enter; plain Enter
    // remains a newline even when the global submit_on_enter setting is true.
    handle_overlay_key(&mut app, key(KeyCode::Enter)).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('x'))).await;

    let Some(OverlayState::Prompt { surface, .. }) = app.focused_input_overlay() else {
        panic!("Expected Prompt overlay");
    };
    assert_eq!(surface.textarea.lines(), &["test", "x"]);
    assert!(app.notifications.is_empty());
}

#[tokio::test]
async fn test_plain_enter_keeps_session_prompt_open_when_suggestions_visible() {
    let mut app = test_app();
    assert!(app.settings.submit_on_enter);
    app.available_commands = vec![crate::suggestions::CommandSuggestion {
        name: "debug".to_string(),
        description: "Debug command".to_string(),
        source: crate::suggestions::CommandSource::CustomCommand,
    }];
    prompt::open_blank_popup(&mut app);

    handle_overlay_key(&mut app, key(KeyCode::Char('/'))).await;

    let Some(OverlayState::Prompt { surface, .. }) = app.focused_input_overlay() else {
        panic!("Expected Prompt overlay");
    };
    assert!(surface.suggestions_visible);

    handle_overlay_key(&mut app, key(KeyCode::Enter)).await;

    let Some(OverlayState::Prompt { surface, .. }) = app.focused_input_overlay() else {
        panic!("Expected Prompt overlay");
    };
    assert_eq!(surface.textarea.lines(), &["/", ""]);
    assert!(app.notifications.is_empty());
}

#[tokio::test]
async fn test_plain_enter_legacy_inserts_newline_in_overlay() {
    let mut app = test_app();
    app.settings.submit_on_enter = false;
    prompt::open_blank_popup(&mut app);

    handle_overlay_key(&mut app, key(KeyCode::Char('a'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Enter)).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('b'))).await;

    if let Some(OverlayState::Prompt { surface, .. }) = app.focused_input_overlay() {
        assert_eq!(surface.textarea.lines(), &["a", "b"]);
    } else {
        panic!(
            "Expected Prompt overlay (legacy submit_on_enter=false should not submit on plain Enter)"
        );
    }
}

#[tokio::test]
async fn test_provider_form_paste_reaches_every_field() {
    let mut app = test_app();

    for field in 0..4 {
        crate::overlay::open_provider_form(&mut app, None);

        if let OverlayState::ProviderForm {
            focused_field,
            name,
            base_url,
            api_key,
            default_model,
            ..
        } = &mut app.overlay
        {
            *focused_field = field;
            match field {
                0 => name.mode = PopupMode::Normal,
                1 => base_url.mode = PopupMode::Normal,
                2 => api_key.mode = PopupMode::Normal,
                3 => default_model.mode = PopupMode::Normal,
                _ => unreachable!(),
            }
        } else {
            panic!("Expected ProviderForm overlay");
        }

        assert!(crate::overlay::try_paste_text_overlay(
            &mut app,
            "hello\nworld"
        ));

        match &app.overlay {
            OverlayState::ProviderForm {
                name,
                base_url,
                api_key,
                default_model,
                ..
            } => {
                let surface = match field {
                    0 => name,
                    1 => base_url,
                    2 => api_key,
                    3 => default_model,
                    _ => unreachable!(),
                };

                assert_eq!(surface.mode, PopupMode::Insert);
                assert_eq!(surface.content_for_send(), "hello world");
            }
            _ => panic!("Expected ProviderForm overlay"),
        }
    }
}

#[tokio::test]
async fn test_open_continue_popup_sets_purpose() {
    let mut app = test_app();
    let session_id = uuid::Uuid::new_v4();

    // Add a session
    let session = rsi_common::types::Session {
        context_fill_pct: None,
        id: session_id,
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
        working_dir: std::path::PathBuf::from("/custom/path"),
        git_branch: None,
        status: rsi_common::types::SessionStatus::Completed,
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
        context_usage_confidence: rsi_common::types::ContextUsageConfidence::default(),
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
    };
    app.sessions
        .insert(session_id, crate::types::SessionState::new(session));

    prompt::open_continue_popup(&mut app, session_id);

    match &app.overlay {
        OverlayState::Prompt {
            purpose,
            working_dir,
            ..
        } => {
            use crate::types::PromptPurpose;
            assert_eq!(*purpose, PromptPurpose::ContinueSession(session_id));
            assert_eq!(working_dir, &std::path::PathBuf::from("/custom/path"));
        }
        _ => panic!("Expected Prompt overlay"),
    }
}

#[tokio::test]
async fn test_submit_prompt_routes_to_continue_session() {
    let mut app = test_app();
    let session_id = uuid::Uuid::new_v4();

    // Add a completed session
    let session = rsi_common::types::Session {
        context_fill_pct: None,
        id: session_id,
        provider: rsi_common::types::SessionProvider::Claude,
        claude_session_id: Some("abc123".to_string()),
        query: "original".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        working_dir: std::path::PathBuf::from("/tmp"),
        git_branch: None,
        status: rsi_common::types::SessionStatus::Completed,
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
        context_usage_confidence: rsi_common::types::ContextUsageConfidence::default(),
        continued_from: None,
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
    app.sessions
        .insert(session_id, crate::types::SessionState::new(session));

    // Open continue popup and type a query
    prompt::open_continue_popup(&mut app, session_id);
    handle_overlay_key(&mut app, key(KeyCode::Char('h'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('i'))).await;

    // Submit
    handle_overlay_key(&mut app, ctrl_key(KeyCode::Enter)).await;

    // Overlay should close
    assert!(matches!(app.overlay, OverlayState::None));
    // Notification should indicate attempt (daemon not connected)
    assert!(!app.notifications.is_empty());
}

#[tokio::test]
async fn test_dd_deletes_line_in_overlay() {
    let mut app = test_app();
    prompt::open_blank_popup(&mut app);

    // Type "hello"
    for c in "hello".chars() {
        handle_overlay_key(&mut app, key(KeyCode::Char(c))).await;
    }

    // Switch to normal mode
    handle_overlay_key(&mut app, key(KeyCode::Esc)).await;

    // 'd' enters operator-pending, second 'd' deletes the line
    handle_overlay_key(&mut app, key(KeyCode::Char('d'))).await;

    // Verify pending operator is set
    if let Some(OverlayState::Prompt { surface, .. }) = app.focused_input_overlay() {
        assert_eq!(surface.vim_state.pending_operator, Some('d'));
    } else {
        panic!("Expected Prompt overlay");
    }

    // Second 'd' completes dd
    handle_overlay_key(&mut app, key(KeyCode::Char('d'))).await;

    // Line should be deleted (textarea empty)
    if let Some(OverlayState::Prompt { surface, .. }) = app.focused_input_overlay() {
        assert!(surface.textarea.lines().join("").is_empty());
        assert_eq!(surface.vim_state.pending_operator, None);
    } else {
        panic!("Expected Prompt overlay");
    }
}

#[tokio::test]
async fn test_dw_deletes_word_in_overlay() {
    let mut app = test_app();
    prompt::open_blank_popup(&mut app);

    // Type "hello world"
    for c in "hello world".chars() {
        handle_overlay_key(&mut app, key(KeyCode::Char(c))).await;
    }

    // Switch to normal mode, go to start of line
    handle_overlay_key(&mut app, key(KeyCode::Esc)).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('0'))).await;

    // 'dw' should delete first word
    handle_overlay_key(&mut app, key(KeyCode::Char('d'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('w'))).await;

    if let Some(OverlayState::Prompt { surface, .. }) = app.focused_input_overlay() {
        let text = surface.textarea.lines().join("");
        assert_eq!(text, "world");
    } else {
        panic!("Expected Prompt overlay");
    }
}

// === Sandbox toggle tests ===

#[tokio::test]
async fn prompt_overlay_sandbox_toggle() {
    let mut app = test_app();

    // Open a Blank popup — sandbox_enabled starts on for every new session.
    crate::overlay::open_blank_popup(&mut app);

    let sandbox_enabled_before = match app.focused_input_overlay() {
        Some(OverlayState::Prompt {
            sandbox_enabled, ..
        }) => *sandbox_enabled,
        _ => panic!("Expected Prompt overlay"),
    };
    assert!(sandbox_enabled_before, "sandbox_enabled should start true");

    // Plain `s` reaches shared text handling instead of changing sandbox state.
    handle_overlay_key(&mut app, key(KeyCode::Char('s'))).await;
    let plain_s_text = match app.focused_input_overlay() {
        Some(OverlayState::Prompt { surface, .. }) => surface.textarea.lines().join(""),
        _ => panic!("Expected Prompt overlay"),
    };
    assert_eq!(plain_s_text, "s");

    // With sandbox_supported=false (default), Ctrl+B is a no-op.
    handle_overlay_key(&mut app, key(KeyCode::Esc)).await;
    handle_overlay_key(&mut app, ctrl_key(KeyCode::Char('b'))).await;
    let sandbox_enabled_no_caps = match app.focused_input_overlay() {
        Some(OverlayState::Prompt {
            sandbox_enabled, ..
        }) => *sandbox_enabled,
        _ => panic!("Expected Prompt overlay"),
    };
    assert!(
        sandbox_enabled_no_caps,
        "Ctrl+B is a no-op when sandbox_supported=false"
    );

    // Enable capability — now Ctrl+B should toggle.
    app.poll.sandbox_supported = true;

    handle_overlay_key(&mut app, ctrl_key(KeyCode::Char('b'))).await;
    let sandbox_enabled_after = match app.focused_input_overlay() {
        Some(OverlayState::Prompt {
            sandbox_enabled, ..
        }) => *sandbox_enabled,
        _ => panic!("Expected Prompt overlay"),
    };
    assert!(
        !sandbox_enabled_after,
        "sandbox_enabled should be false after Ctrl+B toggle with caps"
    );

    // Toggle back on.
    handle_overlay_key(&mut app, ctrl_key(KeyCode::Char('b'))).await;
    let sandbox_enabled_off = match app.focused_input_overlay() {
        Some(OverlayState::Prompt {
            sandbox_enabled, ..
        }) => *sandbox_enabled,
        _ => panic!("Expected Prompt overlay"),
    };
    assert!(
        sandbox_enabled_off,
        "sandbox_enabled should toggle back to true after Ctrl+B"
    );
}

#[tokio::test]
async fn prompt_overlay_modifier_controls_preserve_vim_text_keys() {
    let mut app = test_app();
    app.selected_provider = rsi_common::types::SessionProvider::Claude;
    app.selected_model = Some("claude-opus-5".to_string());
    app.selected_effort = None;
    crate::overlay::open_blank_popup(&mut app);

    // Plain M remains editable text; Ctrl+M opens the prompt model selector.
    handle_overlay_key(&mut app, key(KeyCode::Char('M'))).await;
    let plain_m_text = match app.focused_input_overlay() {
        Some(OverlayState::Prompt { surface, .. }) => surface.textarea.lines().join(""),
        _ => panic!("Expected Prompt overlay"),
    };
    assert_eq!(plain_m_text, "M");

    handle_overlay_key(&mut app, key(KeyCode::Esc)).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('E'))).await;
    assert_eq!(
        app.selected_effort, None,
        "plain E must remain a Vim motion"
    );

    handle_overlay_key(&mut app, ctrl_key(KeyCode::Char('e'))).await;
    assert_eq!(app.selected_effort.as_deref(), Some("xhigh"));

    handle_overlay_key(&mut app, ctrl_key(KeyCode::Char('m'))).await;
    let dropdown_open = match app.focused_input_overlay() {
        Some(OverlayState::Prompt { model_dropdown, .. }) => model_dropdown.open,
        _ => panic!("Expected Prompt overlay"),
    };
    assert!(
        dropdown_open,
        "Ctrl+M should open the prompt model selector"
    );
}

#[tokio::test]
async fn legacy_prompt_uses_modifier_controls() {
    let mut app = test_app();
    app.selected_provider = rsi_common::types::SessionProvider::Claude;
    app.selected_model = Some("claude-opus-5".to_string());
    app.selected_effort = None;
    crate::overlay::open_blank_popup(&mut app);
    app.overlay = app.input_overlays.pop().expect("prompt overlay");
    app.poll.sandbox_supported = true;

    handle_overlay_key(&mut app, key(KeyCode::Char('s'))).await;
    let plain_s_text = match &app.overlay {
        OverlayState::Prompt { surface, .. } => surface.textarea.lines().join(""),
        _ => panic!("Expected Prompt overlay"),
    };
    assert_eq!(plain_s_text, "s");

    handle_overlay_key(&mut app, key(KeyCode::Esc)).await;
    handle_overlay_key(&mut app, ctrl_key(KeyCode::Char('b'))).await;
    let sandbox_disabled = match &app.overlay {
        OverlayState::Prompt {
            sandbox_enabled, ..
        } => !*sandbox_enabled,
        _ => panic!("Expected Prompt overlay"),
    };
    assert!(
        sandbox_disabled,
        "Ctrl+B should toggle sandbox in legacy prompts"
    );

    handle_overlay_key(&mut app, key(KeyCode::Char('E'))).await;
    assert_eq!(
        app.selected_effort, None,
        "plain E must remain a Vim motion"
    );

    handle_overlay_key(&mut app, ctrl_key(KeyCode::Char('e'))).await;
    assert_eq!(app.selected_effort.as_deref(), Some("xhigh"));

    handle_overlay_key(&mut app, ctrl_key(KeyCode::Char('m'))).await;
    let dropdown_open = match &app.overlay {
        OverlayState::Prompt { model_dropdown, .. } => model_dropdown.open,
        _ => panic!("Expected Prompt overlay"),
    };
    assert!(
        dropdown_open,
        "Ctrl+M should open the legacy prompt model selector"
    );
}

// -- RSI-026: QuestionModal submit_on_enter dual-mode coverage ----------------

/// Build a single-question QuestionModal overlay directly for testing the
/// bespoke handler at `crates/rsi/src/overlay/question_modal.rs`. The modal
/// owns its own textarea (no `InputSurface`), so we exercise it without
/// having to wire up a real `WaitingApproval` session.
fn install_question_modal(app: &mut App, mode: PopupMode) -> Uuid {
    let session_id = Uuid::new_v4();
    let questions = vec![rsi_common::types::QuestionItem {
        question: "free-text question".to_string(),
        header: "header".to_string(),
        options: Vec::new(),
        multi_select: false,
    }];
    let mut textarea = tui_textarea::TextArea::default();
    textarea.set_cursor_line_style(ratatui::style::Style::default());

    app.overlay = OverlayState::QuestionModal {
        session_id,
        questions,
        current_question: 0,
        cursor: vec![0],
        selections: vec![crate::types::QuestionSelection::Single(None)],
        textarea: Box::new(textarea),
        mode,
        pending_operator: None,
    };
    session_id
}

#[tokio::test]
async fn test_question_modal_plain_enter_submits_in_insert_mode() {
    let mut app = test_app();
    assert!(app.settings.submit_on_enter);
    install_question_modal(&mut app, PopupMode::Insert);

    // Type "answer" in the textarea
    for ch in "answer".chars() {
        handle_overlay_key(&mut app, key(KeyCode::Char(ch))).await;
    }

    // Plain Enter must trigger submit. The real RPC call will fail without a
    // daemon, but the overlay is consumed/closed and a notification is pushed.
    handle_overlay_key(&mut app, key(KeyCode::Enter)).await;

    // QuestionModal overlay is dropped on submit (restore_previous_overlay).
    assert!(
        !matches!(app.overlay, OverlayState::QuestionModal { .. }),
        "QuestionModal should be dismissed after plain-Enter submit"
    );
}

#[tokio::test]
async fn test_question_modal_shift_enter_inserts_newline_in_insert_mode() {
    let mut app = test_app();
    install_question_modal(&mut app, PopupMode::Insert);

    handle_overlay_key(&mut app, key(KeyCode::Char('a'))).await;
    handle_overlay_key(&mut app, shift_key(KeyCode::Enter)).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('b'))).await;

    if let OverlayState::QuestionModal { textarea, mode, .. } = &app.overlay {
        assert_eq!(*mode, PopupMode::Insert);
        assert_eq!(textarea.lines(), &["a", "b"]);
    } else {
        panic!("Expected QuestionModal overlay to survive Shift+Enter");
    }
}

#[tokio::test]
async fn test_question_modal_ctrl_enter_still_submits() {
    let mut app = test_app();
    install_question_modal(&mut app, PopupMode::Insert);

    for ch in "hi".chars() {
        handle_overlay_key(&mut app, key(KeyCode::Char(ch))).await;
    }
    handle_overlay_key(&mut app, ctrl_key(KeyCode::Enter)).await;

    assert!(
        !matches!(app.overlay, OverlayState::QuestionModal { .. }),
        "Ctrl+Enter must always submit regardless of submit_on_enter"
    );
}

#[tokio::test]
async fn test_question_modal_legacy_plain_enter_inserts_newline() {
    let mut app = test_app();
    app.settings.submit_on_enter = false;
    install_question_modal(&mut app, PopupMode::Insert);

    handle_overlay_key(&mut app, key(KeyCode::Char('a'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Enter)).await;
    handle_overlay_key(&mut app, key(KeyCode::Char('b'))).await;

    if let OverlayState::QuestionModal { textarea, mode, .. } = &app.overlay {
        assert_eq!(*mode, PopupMode::Insert);
        assert_eq!(textarea.lines(), &["a", "b"]);
    } else {
        panic!("Legacy submit_on_enter=false: plain Enter must insert newline, not submit");
    }
}

#[tokio::test]
async fn test_question_modal_normal_mode_plain_enter_advances_question() {
    // Regression guard for the Normal-mode KeyCode::Enter arm in
    // question_modal.rs — it advances to the next question. The new
    // plain-Enter submit branch is gated on PopupMode::Insert, so Normal-mode
    // Enter must NOT be intercepted by the submit path.
    let mut app = test_app();
    let session_id = Uuid::new_v4();
    let questions = vec![
        rsi_common::types::QuestionItem {
            question: "q1".to_string(),
            header: "h1".to_string(),
            options: vec![rsi_common::types::QuestionOption {
                label: "yes".to_string(),
                description: String::new(),
            }],
            multi_select: false,
        },
        rsi_common::types::QuestionItem {
            question: "q2".to_string(),
            header: "h2".to_string(),
            options: vec![rsi_common::types::QuestionOption {
                label: "yes".to_string(),
                description: String::new(),
            }],
            multi_select: false,
        },
    ];
    let mut textarea = tui_textarea::TextArea::default();
    textarea.set_cursor_line_style(ratatui::style::Style::default());

    app.overlay = OverlayState::QuestionModal {
        session_id,
        questions,
        current_question: 0,
        cursor: vec![0, 0],
        selections: vec![
            crate::types::QuestionSelection::Single(None),
            crate::types::QuestionSelection::Single(None),
        ],
        textarea: Box::new(textarea),
        mode: PopupMode::Normal,
        pending_operator: None,
    };

    handle_overlay_key(&mut app, key(KeyCode::Enter)).await;

    if let OverlayState::QuestionModal {
        current_question, ..
    } = &app.overlay
    {
        assert_eq!(
            *current_question, 1,
            "Normal-mode plain Enter must advance current_question to next index"
        );
    } else {
        panic!("Normal-mode plain Enter must not submit the modal");
    }
}

#[tokio::test]
async fn test_question_modal_legacy_ctrl_enter_still_submits() {
    let mut app = test_app();
    app.settings.submit_on_enter = false;
    install_question_modal(&mut app, PopupMode::Insert);

    for ch in "hi".chars() {
        handle_overlay_key(&mut app, key(KeyCode::Char(ch))).await;
    }
    handle_overlay_key(&mut app, ctrl_key(KeyCode::Enter)).await;

    assert!(
        !matches!(app.overlay, OverlayState::QuestionModal { .. }),
        "Ctrl+Enter must still submit in legacy submit_on_enter=false mode"
    );
}

#[tokio::test]
async fn test_question_modal_normal_mode_d_declines_and_closes() {
    let mut app = test_app();
    install_question_modal(&mut app, PopupMode::Normal);

    handle_overlay_key(&mut app, key(KeyCode::Char('d'))).await;

    assert!(
        !matches!(app.overlay, OverlayState::QuestionModal { .. }),
        "Normal-mode d should decline and dismiss the QuestionModal"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// P1.13 — gv SavedWorkflow picker usability tests
// ─────────────────────────────────────────────────────────────────────────────

/// Build a Workflow with the given title and a fresh UUID.
fn make_test_workflow(title: impl Into<String>) -> rsi_common::types::Workflow {
    rsi_common::types::Workflow {
        id: Uuid::new_v4(),
        title: title.into(),
        stage: rsi_common::types::WorkflowStage::Research,
        artifact_path: None,
        project_id: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

#[test]
fn test_picker_truncates_long_titles() {
    use crate::ui::overlay::graph::truncate;
    assert_eq!(truncate("abc", 5), "abc");
    assert_eq!(truncate("abcdefg", 3), "ab\u{2026}");
    // Unicode: 6 chars (3 Hanzi + 3 Katakana); truncate to 3 keeps 2 chars + ellipsis.
    assert_eq!(
        truncate("\u{65e5}\u{672c}\u{8a9e}\u{30c6}\u{30b9}\u{30c8}", 3),
        "\u{65e5}\u{672c}\u{2026}"
    );
}

#[tokio::test]
async fn test_picker_filters_junk_workflow_titles() {
    let mut app = test_app();
    let clean = make_test_workflow("rpi-with-verify-and-docs");
    let junk = make_test_workflow("/research test prompt");
    app.workflows.insert(clean.id, clean.clone());
    app.workflows.insert(junk.id, junk);

    let entries = crate::overlay::graph::saved_workflow_picker_entries(&app);
    assert_eq!(
        entries.len(),
        1,
        "only the clean entry should survive the filter"
    );
    assert_eq!(entries[0].title, "rpi-with-verify-and-docs");
    assert_eq!(entries[0].id, clean.id);
}

#[test]
fn test_picker_renders_with_scroll_state() {
    use crate::types::GraphPickerKind;
    use crate::ui::overlay::graph::render_picker_panel;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::widgets::ListState;

    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");

    let workflows: Vec<rsi_common::types::Workflow> = (0..30)
        .map(|i| make_test_workflow(format!("workflow-{i}")))
        .collect();
    let mut state = ListState::default();
    state.select(Some(25));

    terminal
        .draw(|frame| {
            render_picker_panel(
                frame,
                frame.area(),
                GraphPickerKind::SavedWorkflow,
                25,
                &workflows,
                &[],
                &mut state,
            );
        })
        .expect("picker panel should render");

    let buf = terminal.backend().buffer();
    let text: String = buf.content().iter().map(|c| c.symbol()).collect();
    assert!(
        text.contains("workflow-25"),
        "selected entry should be visible in the rendered buffer; got: {text}"
    );
}
