mod support;

use rsi::app::App;
use rsi::client::DaemonClient;
use rsi::settings_registry::SettingsSection;
use rsi::state::{DevState, PersistedState, emergency_drafts, enable_test_state_isolation};
use rsi::types::{
    FileViewerState, InputMode, ModelDropdownState, OverlayState, Pane, PaneId, SessionState,
    SettingsFocus, SplitDirection, SplitNode, Tab,
};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Create a test app with a non-existent socket (won't connect).
fn test_app() -> App {
    enable_test_state_isolation();
    let client = DaemonClient::new(PathBuf::from("/tmp/nonexistent.sock"));
    DevState::clear();
    PersistedState::default().save();
    emergency_drafts::save(None, &HashMap::new());
    let mut app = App::new(client);
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
        bottom_focus_target: rsi::types::BottomZone::Off,
        mini_dag_focus: None,
    }];
    app.active_tab = 0;
    app.next_pane_id = 1;
    app.current_project_id = None;
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

fn add_test_sessions(app: &mut App, count: usize) {
    let sessions: Vec<rsi_common::types::Session> = (0..count)
        .map(|i| rsi_common::types::Session {
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
            continued_from: None,
            context_usage_confidence: rsi_common::types::ContextUsageConfidence::Missing,
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

    for s in sessions {
        app.session_order.push(s.id);
        app.sessions.insert(s.id, SessionState::new(s));
    }

    // Recalculate filtered order (nav methods use this)
    app.set_project_filter(None);
}

fn focus_session_detail(app: &mut App, session_id: uuid::Uuid) {
    let pane_id = PaneId(0);
    app.tabs[0].layout = SplitNode::Leaf {
        pane: Pane::SessionDetail { session_id },
        id: pane_id,
    };
    app.tabs[0].focused_pane = pane_id;
}

fn open_blank_prompt_with_draft(app: &mut App, lines: Vec<String>) {
    add_test_sessions(app, 1);
    let session_id = app.session_order[0];
    app.sessions
        .get_mut(&session_id)
        .expect("test session should exist")
        .session
        .working_dir = PathBuf::from("/tmp/rsi-s-input");
    if let Some(Pane::SessionList {
        selected_session, ..
    }) = app.focused_pane_mut()
    {
        *selected_session = Some(session_id);
    }
    app.blank_draft = lines;
    rsi::overlay::open_blank_popup(app);
}

fn numbered_lines(count: usize) -> Vec<String> {
    (1..=count).map(|i| format!("line {i}")).collect()
}

fn ideas_intake_like_content() -> String {
    let mut lines: Vec<String> = (1..=42).map(|i| format!("short capture {i}")).collect();
    lines.push(format!(
        "  2026-06-18 | {}",
        "custom commands with space leader and settings layout ".repeat(9)
    ));
    lines.push(format!(
        "2026-06-18 | {}",
        "Run brainstorm then master orchestrate with a detailed yazi-like navigation idea "
            .repeat(13)
    ));
    lines.push(format!(
        "2026-06-18 | {}",
        "I want an AI assistant component that understands recent sessions and app functionality "
            .repeat(13)
    ));
    lines.push("2026-06-18 | where is my cron job feature".to_string());
    lines.push(format!(
        "2026-06-18 | {}",
        "master orchestrate could split an enormous task into well-managed swarm chunks "
            .repeat(12)
    ));
    lines.extend((48..=57).map(|i| format!("new capture {i}")));
    lines.join("\n")
}

fn fixed_uuid(byte: u8) -> uuid::Uuid {
    uuid::Uuid::from_bytes([byte; 16])
}

fn fixed_ts(minutes_ago: i64) -> chrono::DateTime<chrono::Utc> {
    let base = chrono::DateTime::parse_from_rfc3339("2100-01-01T12:00:00Z")
        .expect("fixed timestamp should parse")
        .with_timezone(&chrono::Utc);
    base - chrono::Duration::minutes(minutes_ago)
}

fn codex_context_budget(
    source: rsi_common::provider_capabilities::CapabilitySource,
    confidence: rsi_common::provider_capabilities::CapabilityConfidence,
) -> rsi_common::provider_capabilities::ResolvedContextBudget {
    use rsi_common::provider_capabilities::{
        CapabilityEvidence, CapabilitySource, ContextCapacity, ResolvedContextBudget,
    };

    ResolvedContextBudget {
        active_tokens: 258_400,
        capacity: ContextCapacity {
            advertised_max_tokens: Some(1_050_000),
            provider_default_tokens: Some(272_000),
            provider_max_tokens: Some(872_000),
            effective_percent: Some(95),
            configured_tokens: None,
            runtime_effective_tokens: (source == CapabilitySource::RuntimeTelemetry)
                .then_some(258_400),
            compaction_limit_tokens: Some(230_000),
            max_output_tokens: Some(128_000),
        },
        evidence: CapabilityEvidence {
            source,
            source_version: Some("codex-cli 0.155.1".to_string()),
            source_digest: Some(format!("sha256:{}", "a".repeat(64))),
            observed_at: Some(
                "2026-09-02T12:34:56Z"
                    .parse()
                    .expect("fixed capability timestamp"),
            ),
            confidence,
        },
    }
}

/// Mirrors production math end-to-end: `compute_detail_layout_areas`'s
/// sidebar split (local `34`/`48` clamp literals, matching this file's
/// existing convention of not importing private production items) followed
/// by T5's `compute_centered_detail_area` centering step (local `110`/`100`
/// literals mirroring `DETAIL_CENTERING_ENABLE_WIDTH`/`DETAIL_MAX_CONTENT_WIDTH`
/// in `ui/mod.rs`). A no-op at today's covered widths/sidebar-pcts (detail
/// width stays under the 110 enable threshold), but keeps this helper a
/// complete, accurate mirror rather than a stale pre-centering fragment.
fn expected_detail_content_x(width: u16, sidebar_pct: u16) -> u16 {
    let sidebar_w = ((width as u32 * sidebar_pct as u32) / 100) as u16;
    let sidebar_w = sidebar_w.clamp(34, width.saturating_sub(48));
    let detail_x = sidebar_w + 1;
    let detail_w = width.saturating_sub(detail_x);

    const ENABLE_WIDTH: u16 = 110;
    const MAX_CONTENT_WIDTH: u16 = 100;
    if detail_w <= ENABLE_WIDTH {
        return detail_x;
    }
    let target_width = detail_w.min(MAX_CONTENT_WIDTH);
    let gutter = (detail_w - target_width) / 2;
    detail_x + gutter
}

fn temp_test_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).expect("temp test directory should be created");
    dir
}

fn add_snapshot_sessions(app: &mut App) {
    let rows = [
        (
            1,
            rsi_common::types::SessionProvider::Claude,
            rsi_common::types::SessionStatus::Running,
            "Implement event seam",
            "Extract key dispatch into production-backed step_once",
            "claude-sonnet-5",
            Some(0.042),
            Some(8),
        ),
        (
            2,
            rsi_common::types::SessionProvider::Codex,
            rsi_common::types::SessionStatus::WaitingApproval,
            "Review harness snapshots",
            "Pin session list rendering with TestBackend",
            "codex-2",
            Some(0.015),
            Some(3),
        ),
        (
            3,
            rsi_common::types::SessionProvider::Local,
            rsi_common::types::SessionStatus::Completed,
            "Clean fixture state",
            "Keep daemon-free test data deterministic",
            "gpt-oss:20b",
            None,
            Some(5),
        ),
        (
            4,
            rsi_common::types::SessionProvider::Claude,
            rsi_common::types::SessionStatus::Failed,
            "Investigate render edge",
            "Capture narrow list behavior for later slices",
            "claude-opus-4-1",
            Some(0.108),
            Some(13),
        ),
    ];

    app.poll.connected = false;
    app.sessions.clear();
    app.session_order.clear();
    app.filtered_session_order.clear();
    app.filtered_taskrabbit_order.clear();
    app.filtered_archived_order.clear();
    app.filtered_jobs_order.clear();

    for (idx, provider, status, title, summary, model, cost_usd, num_turns) in rows {
        let id = fixed_uuid(idx);
        let session = rsi_common::types::Session {
            context_fill_pct: None,
            id,
            provider,
            claude_session_id: None,
            query: format!("snapshot query {idx}"),
            title: Some(title.to_string()),
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: Some(summary.to_string()),
            pending_question: None,
            pending_archive: false,
            working_dir: PathBuf::from("/tmp"),
            git_branch: Some("openai/v1".to_string()),
            status,
            project_id: None,
            session_kind: rsi_common::types::SessionKind::Standard,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            created_at: fixed_ts(120 + i64::from(idx)),
            updated_at: fixed_ts(i64::from(idx)),
            cost_usd,
            duration_ms: Some(42_000 + u64::from(idx) * 1_000),
            num_turns,
            model: Some(model.to_string()),
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: Some(10_000 + u64::from(idx) * 1_000),
            total_output_tokens: Some(2_000 + u64::from(idx) * 100),
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            continued_from: None,
            context_usage_confidence: rsi_common::types::ContextUsageConfidence::Partial,
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
        app.session_order.push(id);
        app.sessions.insert(id, SessionState::new(session));
    }

    app.set_project_filter(None);
    let selected = app.filtered_session_order.first().copied();
    if let Some(Pane::SessionList {
        selected_index,
        selected_session,
        scroll_offset,
        active_zone,
        taskrabbit_selected_index,
        archive_selected_index,
        jobs_selected_index,
    }) = app.focused_pane_mut()
    {
        *selected_index = 0;
        *selected_session = selected;
        *scroll_offset = 0;
        *active_zone = Default::default();
        *taskrabbit_selected_index = 0;
        *archive_selected_index = 0;
        *jobs_selected_index = 0;
    }
}

fn add_browser_contract_sessions(app: &mut App) {
    add_snapshot_sessions(app);
    let template = app.sessions[&fixed_uuid(1)].session.clone();
    for idx in 5u8..=33 {
        let id = fixed_uuid(idx);
        let mut session = template.clone();
        session.id = id;
        session.query = format!("browser contract query {idx:02}");
        session.title = Some(format!("Browser session {idx:02}"));
        session.short_summary = Some(format!("Low-priority preview {idx:02}"));
        session.updated_at = chrono::Utc::now() - chrono::Duration::days(i64::from(idx));
        session.created_at = session.updated_at - chrono::Duration::minutes(17);
        session.status = match idx {
            5..=10 => rsi_common::types::SessionStatus::Running,
            11 => rsi_common::types::SessionStatus::Starting,
            12 => rsi_common::types::SessionStatus::Interrupted,
            13..=22 => rsi_common::types::SessionStatus::Completed,
            _ => rsi_common::types::SessionStatus::Archived,
        };
        session.stop_reason = (session.status == rsi_common::types::SessionStatus::Interrupted)
            .then(|| "operator stopped the fixture".to_string());
        app.sessions.insert(id, SessionState::new(session));
        app.session_order.push(id);
    }
    if let Some(state) = app.sessions.get_mut(&fixed_uuid(2)) {
        state.session.provider = rsi_common::types::SessionProvider::OpenRouter;
        state.session.model = Some("openrouter/anthropic/claude-opus-4-6".to_string());
        state.session.effort = Some("high".to_string());
        state.session.pending_question = Some(rsi_common::types::PendingQuestion {
            questions: vec![rsi_common::types::QuestionItem {
                question: "Choose the bounded relay policy?".to_string(),
                header: "Policy".to_string(),
                options: Vec::new(),
                multi_select: false,
            }],
        });
    }
    if let Some(state) = app.sessions.get_mut(&fixed_uuid(4)) {
        state.session.stop_reason = Some("snapshot mismatch".to_string());
        state.session.retry_attempt = Some(2);
        state.session.max_retries = Some(3);
    }
    if let Some(state) = app.sessions.get_mut(&fixed_uuid(3)) {
        state.session.test_passed = Some(true);
        state.session.clippy_passed = Some(true);
        state.session.pipeline_artifact = Some("thoughts/shared/plans/relay.md".to_string());
    }
    app.set_project_filter(None);
    let focus = rsi::types::compute_session_focus_index(&app.sessions, chrono::Utc::now());
    app.filtered_session_order.sort_by_key(|id| {
        focus
            .get(id)
            .map_or(rsi::types::SessionFocusGroup::Quiet, |entry| entry.group)
    });
    app.session_order = app.filtered_session_order.clone();
    let selected = app.filtered_session_order.first().copied();
    if let Some(Pane::SessionList {
        selected_index,
        selected_session,
        ..
    }) = app.focused_pane_mut()
    {
        *selected_index = 0;
        *selected_session = selected;
    }
}

fn focus_settings(app: &mut App, section: SettingsSection, selected_index: usize) {
    app.tabs[0].layout = SplitNode::Leaf {
        pane: Pane::Settings,
        id: PaneId(0),
    };
    app.settings_state.section = section;
    app.settings_state.selected_index = selected_index;
    app.settings_state.focus = SettingsFocus::Items;
}

fn add_status_snapshot_sessions(app: &mut App) -> uuid::Uuid {
    add_snapshot_sessions(app);

    let project_id = fixed_uuid(90);
    let now = fixed_ts(0);
    app.projects = vec![rsi_common::types::Project {
        id: project_id,
        name: "core".to_string(),
        path: Some(PathBuf::from("/tmp/rsi")),
        description: None,
        color: rsi_common::types::Project::DEFAULT_COLOR.to_string(),
        context_files: None,
        created_at: now,
        updated_at: now,
    }];
    app.selected_model = Some("claude-sonnet-5".to_string());
    app.available_models = vec![("claude-sonnet-5".to_string(), "claude-sonnet-5".to_string())];

    for state in app.sessions.values_mut() {
        state.session.project_id = Some(project_id);
    }

    let focused_id = fixed_uuid(1);
    if let Some(state) = app.sessions.get_mut(&focused_id) {
        state.session.cost_usd = Some(0.043);
        state.session.test_passed = Some(true);
        state.session.clippy_passed = Some(false);
        state.session.testing_needed_at = Some(now);
        state.session.context_usage_confidence = rsi_common::types::ContextUsageConfidence::Partial;
        state.live_context_pct = Some(42.0);
    }

    if let Some(state) = app.sessions.get_mut(&fixed_uuid(2)) {
        state.session.pending_question = Some(rsi_common::types::PendingQuestion {
            questions: vec![rsi_common::types::QuestionItem {
                question: "Proceed?".to_string(),
                header: "Confirm".to_string(),
                options: Vec::new(),
                multi_select: false,
            }],
        });
    }

    app.set_project_filter(Some(project_id));
    focused_id
}

/// Feed a key through the production event seam.
async fn feed_key(app: &mut App, key: crossterm::event::KeyEvent) {
    rsi::event::step_once(app, key).await;
}

/// Helper to create a KeyEvent from a char.
fn key(c: char) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char(c),
        crossterm::event::KeyModifiers::NONE,
    )
}

/// Helper to create a KeyEvent from a KeyCode.
fn key_code(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
}

fn modified_key_code(
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, modifiers)
}

/// Helper to create a Ctrl+key KeyEvent.
fn ctrl_key(c: char) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char(c),
        crossterm::event::KeyModifiers::CONTROL,
    )
}

// === Non-key tests (no changes needed) ===

#[test]
fn test_app_starts_with_one_tab() {
    let app = test_app();
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.active_tab, 0);
    assert_eq!(app.input_mode, InputMode::Normal);
    assert!(!app.poll.connected);
}

#[test]
fn test_initial_pane_is_session_list() {
    let app = test_app();
    match app.focused_pane() {
        Some(Pane::SessionList { .. }) => {}
        other => panic!("Expected SessionList, got {:?}", other),
    }
}

#[test]
fn test_session_state_creation() {
    let session = rsi_common::types::Session {
        context_fill_pct: None,
        id: uuid::Uuid::new_v4(),
        provider: rsi_common::types::SessionProvider::Claude,
        claude_session_id: None,
        query: "test query".to_string(),
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
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
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
        continued_from: None,
        context_usage_confidence: rsi_common::types::ContextUsageConfidence::Missing,
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

    let state = SessionState::new(session.clone());
    assert_eq!(state.session.query, "test query");
    assert!(state.events.is_empty());
    assert_eq!(state.scroll_offset, 0);
}

#[test]
fn test_enter_session_detail() {
    let mut app = test_app();
    add_test_sessions(&mut app, 1);

    app.nav_down();

    let first_id = app.session_order.first().copied();
    if let Some(Pane::SessionList {
        selected_session, ..
    }) = app.focused_pane_mut()
    {
        *selected_session = first_id;
    }

    app.enter_session();

    match app.focused_pane() {
        Some(Pane::SessionDetail { session_id }) => {
            assert_eq!(*session_id, app.session_order[0]);
        }
        other => panic!("Expected SessionDetail, got {:?}", other),
    }

    app.back_to_list();
    match app.focused_pane() {
        Some(Pane::SessionList { .. }) => {}
        other => panic!("Expected SessionList, got {:?}", other),
    }
}

#[test]
fn test_split_node_operations() {
    use rsi::types::SplitNode;

    let id1 = PaneId(1);
    let id2 = PaneId(2);
    let id3 = PaneId(3);

    let tree = SplitNode::Split {
        direction: SplitDirection::Vertical,
        first: Box::new(SplitNode::Leaf {
            pane: Pane::SessionList {
                selected_index: 0,
                selected_session: None,
                scroll_offset: 0,
                active_zone: Default::default(),
                taskrabbit_selected_index: 0,
                archive_selected_index: 0,
                jobs_selected_index: 0,
            },
            id: id1,
        }),
        second: Box::new(SplitNode::Leaf {
            pane: Pane::SessionList {
                selected_index: 0,
                selected_session: None,
                scroll_offset: 0,
                active_zone: Default::default(),
                taskrabbit_selected_index: 0,
                archive_selected_index: 0,
                jobs_selected_index: 0,
            },
            id: id2,
        }),
        id: id3,
    };

    let ids = tree.leaf_ids();
    assert_eq!(ids, vec![id1, id2]);

    assert!(tree.find_pane(id1).is_some());
    assert!(tree.find_pane(id2).is_some());
    assert!(tree.find_pane(id3).is_none());

    let after_remove = tree.remove_pane(id1).unwrap();
    assert_eq!(after_remove.leaf_ids(), vec![id2]);
}

#[test]
fn snap_session_list_initial() {
    let mut app = test_app();
    add_snapshot_sessions(&mut app);
    let buffer = support::render_app(&mut app, 100, 28);
    insta::assert_snapshot!(support::buffer_to_snapshot(&buffer));
}

fn visible_browser_rows(app: &App) -> usize {
    let Some(cards) = app.session_list_render.cards_area else {
        return 0;
    };
    let zone = &app.session_list_render.main;
    let start = zone.scroll_offset;
    let end = start + cards.height as usize;
    zone.card_offsets
        .iter()
        .zip(&zone.card_heights)
        .filter(|(offset, height)| **offset < end && **offset + **height > start)
        .count()
}

#[test]
fn snap_session_browser_120x40() {
    let mut app = test_app();
    add_browser_contract_sessions(&mut app);
    let buffer = support::render_app(&mut app, 120, 40);
    let text = support::buffer_to_snapshot(&buffer);

    assert!(text.contains("01  ? waiting · reply required"), "{text}");
    assert!(text.contains("#   ∞  S  !  FUNCTION"));
    assert!(text.contains("◔") && text.contains("⇄"));
    assert!(
        visible_browser_rows(&app) >= 22,
        "120x40 must retain at least 22 visible operational rows"
    );
    insta::assert_snapshot!("snap_session_browser_120x40", text);
}

#[test]
fn snap_session_browser_200x58() {
    let mut app = test_app();
    add_browser_contract_sessions(&mut app);
    let buffer = support::render_app(&mut app, 200, 58);
    let text = support::buffer_to_snapshot(&buffer);

    assert!(text.contains("01  ? waiting"), "{text}");
    assert!(
        text.contains("→ "),
        "next action keeps its arrow gutter:\n{text}"
    );
    assert!(text.contains("? reply required"), "{text}");
    assert!(text.contains("#   ∞  S  !  FUNCTION"));
    for heading in ["MODEL", "▮"] {
        assert!(text.contains(heading), "missing {heading} heading:\n{text}");
    }
    for identity in ["⋈ openrouter/anthropic/claude-opus-4-6", "▮▮▮▯ high"] {
        assert!(
            text.contains(identity),
            "missing selected execution identity {identity:?}:\n{text}"
        );
    }
    let cards = app.session_list_render.cards_area.unwrap();
    assert_eq!(cards.width, 100);
    let selected_y = cards.y
        + app.session_list_render.main.card_offsets[0]
            .saturating_sub(app.session_list_render.main.scroll_offset) as u16;
    let selected_cell = &buffer[(cards.x, selected_y)];
    assert_ne!(selected_cell.fg, ratatui::style::Color::Reset);
    assert_ne!(selected_cell.bg, ratatui::style::Color::Reset);
    insta::assert_snapshot!("snap_session_browser_200x58", text);
}

#[test]
fn navigator_detail_path_preserves_full_openrouter_identity_when_columns_are_hidden() {
    let mut app = test_app();
    add_browser_contract_sessions(&mut app);
    let selected_id = fixed_uuid(2);
    focus_session_detail(&mut app, selected_id);

    for (width, height, case) in [(120, 68, "narrow"), (120, 24, "short")] {
        let buffer = support::render_app(&mut app, width, height);
        let text = support::buffer_to_snapshot(&buffer);
        // Provider is the `⋈` glyph before the complete canonical model ID
        // (which wraps below it when the pane is narrower than the ID); the
        // recorded effort keeps its ladder bars and its own word.
        for identity in ["⋈", "openrouter/anthropic/claude-opus-4-6", "▮▮▮▯ high"] {
            assert!(
                text.contains(identity),
                "{case} detail path lost {identity:?}:\n{text}"
            );
        }
    }
}

#[test]
fn snap_session_browser_240x70() {
    let mut app = test_app();
    add_browser_contract_sessions(&mut app);
    let buffer = support::render_app(&mut app, 240, 70);
    let text = support::buffer_to_snapshot(&buffer);

    for label in ["⚑ QUEUE", "Δ CHANGES", "FLOW"] {
        assert!(text.contains(label), "missing activity section {label}");
    }
    let cards = app.session_list_render.cards_area.unwrap();
    assert_eq!(cards.x, 4);
    assert_eq!(cards.width, 100);
    insta::assert_snapshot!("snap_session_browser_240x70", text);
}

#[test]
fn snap_settings_context_workbench_120x40() {
    let mut app = test_app();
    focus_settings(&mut app, SettingsSection::InputPrompts, 0);
    let buffer = support::render_app(&mut app, 120, 40);
    let text = support::buffer_to_snapshot(&buffer);

    for label in [
        "APPEARANCE",
        "WORKSPACE",
        "MODELS",
        "SAFETY & SPEND",
        "AGENT AUTOMATION",
        "PROVIDERS & SANDBOXES",
        "INTEGRATIONS",
        "ITEMS FOCUS",
        "Submit on Enter",
        "[ON]",
        "Enter toggle → OFF",
    ] {
        assert!(text.contains(label), "missing Settings label {label}");
    }
    assert!(!text.contains("CONTEXT\n"));
    insta::assert_snapshot!("snap_settings_context_workbench_120x40", text);
}

#[test]
fn snap_settings_context_workbench_200x58() {
    let mut app = test_app();
    focus_settings(&mut app, SettingsSection::InputPrompts, 0);
    let buffer = support::render_app(&mut app, 200, 58);
    let text = support::buffer_to_snapshot(&buffer);

    for label in [
        "CONTEXT WORKBENCH",
        "SECTIONS",
        "ITEMS FOCUS",
        "CONTEXT",
        "ENTER RESULT",
        "Persistence",
        "state.json",
    ] {
        assert!(text.contains(label), "missing Settings label {label}");
    }
    assert!(!text.contains("LIVE IMPACT"));
    insta::assert_snapshot!("snap_settings_context_workbench_200x58", text);
}

#[test]
fn snap_settings_context_workbench_240x70() {
    let mut app = test_app();
    focus_settings(&mut app, SettingsSection::InputPrompts, 0);
    let buffer = support::render_app(&mut app, 240, 70);
    let text = support::buffer_to_snapshot(&buffer);

    for label in [
        "CONTEXT",
        "LIVE IMPACT",
        "INPUT CONTRACT",
        "AFFECTED SURFACES",
        "DEPENDENCY",
        "PROVENANCE",
    ] {
        assert!(text.contains(label), "missing Settings label {label}");
    }
    insta::assert_snapshot!("snap_settings_context_workbench_240x70", text);
}

#[test]
fn settings_dense_daemon_category_keeps_selected_row_visible() {
    let mut app = test_app();
    let rows =
        rsi::settings_keys::daemon_feature_rows_for_section(&app, SettingsSection::StallDetection);
    let selected_index = rows
        .iter()
        .position(|(spec, _)| {
            spec.id == rsi::settings_registry::SettingId::ClassifierConfidenceFloor
        })
        .expect("stall classifier confidence floor setting");
    let total = rows.len();
    let selected_position = selected_index + 1;
    let selected_label = app.daemon_features[rows[selected_index].1].label.clone();
    let first_label = app.daemon_features[rows[0].1].label.clone();
    focus_settings(&mut app, SettingsSection::StallDetection, selected_index);
    // Epic M design D.1 shrank each section to a handful of rows, so a
    // shorter viewport (rather than the 24-row one this test used against
    // the old 33-row DaemonFeatures bucket) is needed to force scrolling.
    let buffer = support::render_app(&mut app, 120, 12);
    let text = support::buffer_to_snapshot(&buffer);

    assert!(text.contains(&format!("{selected_position}/{total}")));
    assert!(text.contains(&selected_label));
    assert!(!text.contains(&first_label));
}

#[test]
fn settings_settlement_row_renders_as_an_action_without_column_clipping() {
    let mut app = test_app();
    let rows =
        rsi::settings_keys::daemon_feature_rows_for_section(&app, SettingsSection::SandboxStorage);
    let selected_index = rows
        .iter()
        .position(|(spec, _)| {
            spec.id == rsi::settings_registry::SettingId::SourceWorktreeSettlement
        })
        .expect("source worktree settlement setting");
    focus_settings(&mut app, SettingsSection::SandboxStorage, selected_index);
    let buffer = support::render_app(&mut app, 240, 70);
    let text = support::buffer_to_snapshot(&buffer);

    assert!(text.contains("Press Enter"));
    assert!(text.contains("Enter open audit"));
    assert!(!text.contains("Open destructive a"));
    assert!(!text.contains("Read-only value"));
}

#[test]
fn settings_codex_sandbox_value_fits_without_clipping() {
    let mut app = test_app();
    let rows = rsi::settings_keys::daemon_feature_rows_for_section(
        &app,
        SettingsSection::ProviderIsolation,
    );
    let selected_index = rows
        .iter()
        .position(|(spec, _)| spec.id == rsi::settings_registry::SettingId::CodexSandbox)
        .expect("Codex sandbox setting");
    focus_settings(&mut app, SettingsSection::ProviderIsolation, selected_index);
    let buffer = support::render_app(&mut app, 240, 70);
    let text = support::buffer_to_snapshot(&buffer);

    assert!(text.contains("‹ danger-full-access ›"));
}

#[test]
fn settings_model_dropdown_keeps_fixed_row_anchor() {
    let mut app = test_app();
    focus_settings(&mut app, SettingsSection::ModelRoles, 3);
    app.settings_state.active_dropdown_item = Some(3);
    app.settings_state.model_dropdown = ModelDropdownState::new(
        rsi_common::types::SessionProvider::Claude,
        vec![
            ("claude-opus-4".to_string(), "Opus 4".to_string()),
            ("claude-sonnet-4".to_string(), "Sonnet 4".to_string()),
        ],
        Some("claude-sonnet-4"),
    );

    let buffer = support::render_app(&mut app, 200, 58);
    let text = support::buffer_to_snapshot(&buffer);

    assert!(text.contains("Model [Claude"));
    assert!(text.contains("Sonnet 4"));
    assert_eq!(buffer[(38, 12)].symbol(), "╭");
}

#[test]
fn snap_top_status_bar_global_only() {
    let mut app = test_app();
    let session_id = add_status_snapshot_sessions(&mut app);
    focus_session_detail(&mut app, session_id);

    let buffer = support::render_app(&mut app, 132, 28);
    let snapshot = support::buffer_to_snapshot(&buffer)
        .lines()
        .next()
        .expect("top status line should render")
        .to_string();

    // The compact header keeps the active count on the left. Project tab dots
    // may render on the right.
    assert!(
        snapshot.contains("● 1"),
        "missing active count:\n{snapshot}"
    );
    // Chrome/leak guards (not entity identity): removed chips and per-session
    // metadata must stay out of the global header.
    assert!(
        !snapshot.contains('⚡'),
        "legacy active lightning icon must not render:\n{snapshot}"
    );
    assert!(
        !snapshot.contains("⇄"),
        "merge-queue chip must not render after ST-MERGEQ-CUT:\n{snapshot}"
    );
    assert!(
        !snapshot.contains("▸ core"),
        "project chip must not render after T2-HEADER-CUT:\n{snapshot}"
    );
    assert!(
        !snapshot.contains("⚙ claude-sonnet-5"),
        "model chip must not render after T2-HEADER-CUT:\n{snapshot}"
    );
    assert!(
        !snapshot.contains("☰4/4"),
        "session count chip must not render after T2-HEADER-CUT:\n{snapshot}"
    );
    assert!(
        !snapshot.contains("✗1"),
        "failed chip must not render after T2-HEADER-CUT:\n{snapshot}"
    );
    assert!(
        !snapshot.contains("$0.17"),
        "budget chip must not render after T2-HEADER-CUT:\n{snapshot}"
    );
    assert!(
        !snapshot.contains(" mode "),
        "detail mode leaked into top status:\n{snapshot}"
    );
    assert!(
        !snapshot.contains("NORMAL"),
        "detail input mode leaked into top status:\n{snapshot}"
    );
    assert!(
        !snapshot.contains("turn"),
        "session metadata leaked into top status:\n{snapshot}"
    );
    assert!(
        !snapshot.contains("ctx"),
        "context metadata leaked into top status:\n{snapshot}"
    );
    insta::assert_snapshot!("snap_top_status_bar_global_only", snapshot);
}

#[test]
fn session_detail_places_status_in_metadata_and_mode_in_bottom_input() {
    let mut app = test_app();
    let session_id = add_status_snapshot_sessions(&mut app);
    focus_session_detail(&mut app, session_id);

    // Legacy footer resizing must not leave unused rows beneath the composer.
    app.tabs[0].detail_split_offset = 8;
    // LEADER covers the pending-leader state: focused normal input with the vim
    // machine awaiting a continuation (App.vim_machine_pending), rendered through
    // the real full-app draw path.
    for width in [60, 132] {
        for (mode, label, pending) in [
            (rsi::types::PopupMode::Normal, "NORMAL", false),
            (rsi::types::PopupMode::Insert, "INSERT", false),
            (rsi::types::PopupMode::Normal, "LEADER", true),
        ] {
            app.sessions
                .get_mut(&session_id)
                .unwrap()
                .input_bar
                .surface
                .mode = mode;
            app.vim_machine_pending = pending;
            let buffer = support::render_app(&mut app, width, 28);
            let metadata = support::line_text(&buffer, 2);
            assert!(metadata.contains("●  Claude  ·  Sonnet 5"), "{metadata}");
            let composer = support::line_text(&buffer, 26);
            assert!(composer.contains(&format!("{label} ›")), "{composer}");
            let bottom = support::line_text(&buffer, 27);
            assert!(
                bottom.ends_with('┘'),
                "input border must end the pane: {bottom}"
            );
            app.vim_machine_pending = false;
        }
    }
}

#[test]
fn session_detail_input_grows_for_text_wrapped_after_mode_label() {
    let mut app = test_app();
    let session_id = add_status_snapshot_sessions(&mut app);
    focus_session_detail(&mut app, session_id);
    let surface = &mut app.sessions.get_mut(&session_id).unwrap().input_bar.surface;
    surface.mode = rsi::types::PopupMode::Insert;
    surface.textarea.insert_str(format!("{}Z", "a".repeat(47)));

    let buffer = support::render_app(&mut app, 60, 28);
    assert_eq!(buffer[(0, 24)].symbol(), "┌");
    assert!(support::line_text(&buffer, 25).contains(&"a".repeat(47)));
    assert_eq!(buffer[(11, 26)].symbol(), "Z");
    assert_eq!(buffer[(59, 27)].symbol(), "┘");
}

#[test]
fn h2_metadata_push_updates_visible_provider_model_and_context() {
    use rsi_common::provider_capabilities::{CapabilityConfidence, CapabilitySource};

    let mut app = test_app();
    add_test_sessions(&mut app, 1);
    let session_id = app.session_order[0];
    focus_session_detail(&mut app, session_id);
    let state = app
        .sessions
        .get_mut(&session_id)
        .expect("test session exists");
    state.session.provider = rsi_common::types::SessionProvider::Codex;
    state.session.model = Some("older-model".to_string());
    state.session.status = rsi_common::types::SessionStatus::Running;

    let catalog_budget = codex_context_budget(
        CapabilitySource::ProviderCatalog,
        CapabilityConfidence::Verified,
    );
    assert!(app.apply_push_event(rsi_common::rpc::BusEvent {
        event_type: "session_metadata_changed".to_string(),
        timestamp: chrono::Utc::now(),
        data: serde_json::json!({
            "session_id": session_id,
            "model": "gpt-6-astra",
            "resolved_context_budget": catalog_budget,
        }),
    }));
    assert_eq!(
        app.sessions[&session_id]
            .session
            .resolved_context_budget
            .as_ref()
            .map(|budget| budget.evidence.source),
        Some(CapabilitySource::ProviderCatalog)
    );

    let runtime_budget = codex_context_budget(
        CapabilitySource::RuntimeTelemetry,
        CapabilityConfidence::Authoritative,
    );
    assert!(app.apply_push_event(rsi_common::rpc::BusEvent {
        event_type: "context_usage_updated".to_string(),
        timestamp: chrono::Utc::now(),
        data: serde_json::json!({
            "session_id": session_id,
            "input_tokens": 34_000,
            "output_tokens": 0,
            "daemon_total": 34_000,
            "confidence": rsi_common::types::ContextUsageConfidence::Partial,
            "context_window": 258_400,
            "pct": 9.0,
            "resolved_context_budget": runtime_budget,
        }),
    }));

    let state = app
        .sessions
        .get(&session_id)
        .expect("pushed session exists");
    assert_eq!(
        state.session.provider,
        rsi_common::types::SessionProvider::Codex
    );
    assert_eq!(state.session.model.as_deref(), Some("gpt-6-astra"));
    assert_eq!(state.live_context_pct, Some(9.0));
    assert_eq!(state.session.input_tokens, Some(34_000));
    assert_eq!(state.session.context_window, Some(258_400));
    assert_eq!(
        state
            .session
            .resolved_context_budget
            .as_ref()
            .map(|budget| budget.evidence.source),
        Some(CapabilitySource::RuntimeTelemetry)
    );
    assert_eq!(
        state.session.context_usage_confidence,
        rsi_common::types::ContextUsageConfidence::Partial
    );

    let buffer = support::render_app(&mut app, 132, 28);
    let detail_header = (1..=3)
        .map(|line| support::line_text(&buffer, line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        detail_header.contains("Codex"),
        "missing Codex detail metadata: {detail_header}"
    );
    assert!(
        detail_header.contains("GPT-6 Astra"),
        "missing model detail metadata: {detail_header}"
    );
    assert!(
        detail_header.contains("9%≈·T ctx"),
        "missing context detail metadata: {detail_header}"
    );

    rsi::overlay::open_session_info_panel(&mut app);
    let buffer = support::render_app(&mut app, 132, 28);
    let panel = (0..28)
        .map(|line| support::line_text(&buffer, line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        panel.contains("provider: Codex"),
        "missing provider panel: {panel}"
    );
    assert!(
        panel.contains("model: gpt-6-astra"),
        "missing model panel: {panel}"
    );
    for detail in [
        "context: 34k / 258k runtime",
        "provider: default 272k · max 872k · effective factor 95%",
        "API: context max 1.05m · output max 128k",
        "compaction: limit 230k",
        "source: runtime telemetry",
        "version: codex-cli 0.155.1",
        "digest: sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "freshness: fresh · observed 2026-09-02T12:34:56Z · usage partial",
    ] {
        assert!(
            panel.contains(detail),
            "missing {detail:?} panel row: {panel}"
        );
    }
}

#[test]
fn wide_inspector_keeps_active_and_capacity_labels_distinct() {
    use rsi_common::provider_capabilities::{CapabilityConfidence, CapabilitySource};

    let mut app = test_app();
    add_browser_contract_sessions(&mut app);
    let selected_id = app.filtered_session_order[0];
    let state = app
        .sessions
        .get_mut(&selected_id)
        .expect("selected browser session exists");
    state.session.provider = rsi_common::types::SessionProvider::Codex;
    state.session.model = Some("gpt-6-astra".to_string());
    state.session.input_tokens = Some(34_000);
    state.session.context_usage_confidence = rsi_common::types::ContextUsageConfidence::Full;
    state.session.resolved_context_budget = Some(codex_context_budget(
        CapabilitySource::RuntimeTelemetry,
        CapabilityConfidence::Authoritative,
    ));
    state.live_context_pct = Some(9.0);

    let text = support::buffer_to_snapshot(&support::render_app(&mut app, 200, 58));
    for detail in [
        "◔ ▰▱▱▱▱▱▱▱▱▱ 9%  34k / 258k runtime",
        "provider: default 272k · max 872k · effective factor 95%",
        "API: context max 1.05m · output max 128k",
        "compaction: limit 230k",
        "source: runtime telemetry",
        "version: codex-cli 0.155.1",
        "digest:",
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "freshness: fresh · observed 2026-09-02T12:34:56Z · usage full",
    ] {
        assert!(
            text.contains(detail),
            "wide inspector missing {detail:?}:\n{text}"
        );
    }
}

#[test]
fn compact_session_row_distinguishes_context_source_states() {
    use rsi_common::provider_capabilities::{CapabilityConfidence, CapabilitySource};
    use rsi_common::types::ContextUsageConfidence;

    let cases = [
        (
            CapabilitySource::RuntimeTelemetry,
            CapabilityConfidence::Authoritative,
            ContextUsageConfidence::Full,
            Some(42.0),
            "42%",
        ),
        (
            CapabilitySource::RuntimeTelemetry,
            CapabilityConfidence::Authoritative,
            ContextUsageConfidence::Stale,
            Some(42.0),
            "42%",
        ),
        (
            CapabilitySource::ProviderCatalog,
            CapabilityConfidence::Verified,
            ContextUsageConfidence::Missing,
            None,
            "◌",
        ),
        (
            CapabilitySource::RepositoryFallback,
            CapabilityConfidence::Degraded,
            ContextUsageConfidence::Partial,
            Some(42.0),
            "42%",
        ),
        (
            CapabilitySource::LegacyUnverified,
            CapabilityConfidence::Degraded,
            ContextUsageConfidence::Missing,
            None,
            "◌",
        ),
    ];

    for (source, evidence_confidence, usage_confidence, pct, expected) in cases {
        let mut app = test_app();
        add_test_sessions(&mut app, 1);
        let session_id = app.session_order[0];
        let state = app
            .sessions
            .get_mut(&session_id)
            .expect("compact-state session exists");
        state.session.status = rsi_common::types::SessionStatus::Running;
        state.session.provider = rsi_common::types::SessionProvider::Codex;
        state.session.model = Some("older-model".to_string());

        let resolved = codex_context_budget(source, evidence_confidence);
        assert!(app.apply_push_event(rsi_common::rpc::BusEvent {
            event_type: "session_metadata_changed".to_string(),
            timestamp: chrono::Utc::now(),
            data: serde_json::json!({
                "session_id": session_id,
                "model": "gpt-6-astra",
                "resolved_context_budget": resolved.clone(),
            }),
        }));
        assert!(app.apply_push_event(rsi_common::rpc::BusEvent {
            event_type: "context_usage_updated".to_string(),
            timestamp: chrono::Utc::now(),
            data: serde_json::json!({
                "session_id": session_id,
                "input_tokens": if pct.is_some() { 34_000 } else { 0 },
                "output_tokens": 0,
                "daemon_total": if pct.is_some() { 34_000 } else { 0 },
                "confidence": usage_confidence,
                "context_window": 258_400,
                "pct": pct.unwrap_or(0.0),
                "resolved_context_budget": resolved,
            }),
        }));

        let row_buffer = support::render_app(&mut app, 118, 28);
        let row_text = support::buffer_to_snapshot(&row_buffer);
        assert!(
            row_text.contains(expected),
            "session row must positively render {expected:?}:\n{row_text}"
        );
    }
}

#[test]
fn snap_list_footer_no_clutter() {
    let mut app = test_app();
    add_snapshot_sessions(&mut app);

    let buffer = support::render_app(&mut app, 100, 28);
    let snapshot = support::line_text(&buffer, 27).trim_end().to_string();
    let text = support::buffer_to_snapshot(&buffer);

    assert!(snapshot.is_empty());
    for fragment in [
        "session 1/4",
        "cached",
        "sort stale",
        "j/k nav",
        "Enter open",
        "yy copy id",
        "ga archive",
        "; jump",
    ] {
        assert!(!text.contains(fragment));
    }

    insta::assert_snapshot!("snap_list_footer_no_clutter", format!("{snapshot:?}"));
}

#[tokio::test]
async fn test_sidebar_resize_uses_last_rendered_terminal_width() {
    let mut shrink_app = test_app();
    let shrink_session_id = add_status_snapshot_sessions(&mut shrink_app);
    focus_session_detail(&mut shrink_app, shrink_session_id);
    shrink_app.last_terminal_size = (220, 40);

    feed_key(
        &mut shrink_app,
        modified_key_code(
            crossterm::event::KeyCode::Left,
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::SHIFT,
        ),
    )
    .await;

    assert_eq!(
        shrink_app.tabs[0].session_list_width_pct, 43,
        "sidebar shrink should initialize from the last rendered backend width"
    );

    support::render_app(&mut shrink_app, 220, 30);
    let content_area = shrink_app.sessions[&shrink_session_id].last_content_area;
    assert_eq!(content_area.x, expected_detail_content_x(220, 43));

    let mut grow_app = test_app();
    let grow_session_id = add_status_snapshot_sessions(&mut grow_app);
    focus_session_detail(&mut grow_app, grow_session_id);
    grow_app.last_terminal_size = (220, 40);

    feed_key(
        &mut grow_app,
        modified_key_code(
            crossterm::event::KeyCode::Right,
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::SHIFT,
        ),
    )
    .await;

    assert_eq!(
        grow_app.tabs[0].session_list_width_pct, 49,
        "sidebar grow should initialize from the last rendered backend width"
    );

    support::render_app(&mut grow_app, 220, 30);
    let content_area = grow_app.sessions[&grow_session_id].last_content_area;
    assert_eq!(content_area.x, expected_detail_content_x(220, 49));
}

/// Regression for #43 / ST-RESIZE — a reversing sidebar drag must move the
/// divider on the FIRST keypress, with no dead steps. The bug: the grow handler
/// capped the stored width at 60 while the renderer clamps to `SIDEBAR_MAX_PCT`
/// (55), so after a saturated grow the stored width sat off-screen above the
/// ceiling and the first 1–2 shrink presses only re-synced it without moving the
/// visible divider. A wide backend (220) makes the percent ceiling — the value
/// this slice fixed — the binding constraint rather than the column clamp.
#[tokio::test]
async fn test_sidebar_reverse_drag_moves_divider_on_first_keypress() {
    fn grow() -> crossterm::event::KeyEvent {
        modified_key_code(
            crossterm::event::KeyCode::Right,
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::SHIFT,
        )
    }
    fn shrink() -> crossterm::event::KeyEvent {
        modified_key_code(
            crossterm::event::KeyCode::Left,
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::SHIFT,
        )
    }

    let mut app = test_app();
    let session_id = add_status_snapshot_sessions(&mut app);
    focus_session_detail(&mut app, session_id);
    app.last_terminal_size = (220, 40);

    // Saturate the grow direction well past the ceiling.
    for _ in 0..25 {
        feed_key(&mut app, grow()).await;
    }
    // Stored width is pinned at the rendered ceiling, never the old off-screen 60.
    assert_eq!(
        app.tabs[0].session_list_width_pct,
        rsi::ui::SIDEBAR_MAX_PCT,
        "grow must saturate at the render ceiling, not above it"
    );

    support::render_app(&mut app, 220, 30);
    let saturated_x = app.sessions[&session_id].last_content_area.x;

    // The very first reverse (shrink) keypress must move the divider left.
    feed_key(&mut app, shrink()).await;
    support::render_app(&mut app, 220, 30);
    let after_first_shrink_x = app.sessions[&session_id].last_content_area.x;
    assert!(
        after_first_shrink_x < saturated_x,
        "divider must move on the FIRST reverse keypress (no dead steps): \
         saturated_x={saturated_x}, after_first_shrink_x={after_first_shrink_x}"
    );

    // Symmetry: saturate the shrink direction, then the first grow must move it.
    for _ in 0..25 {
        feed_key(&mut app, shrink()).await;
    }
    assert_eq!(
        app.tabs[0].session_list_width_pct,
        rsi::ui::SIDEBAR_MIN_PCT,
        "shrink must saturate at the render floor"
    );
    support::render_app(&mut app, 220, 30);
    let floored_x = app.sessions[&session_id].last_content_area.x;

    feed_key(&mut app, grow()).await;
    support::render_app(&mut app, 220, 30);
    let after_first_grow_x = app.sessions[&session_id].last_content_area.x;
    assert!(
        after_first_grow_x > floored_x,
        "divider must move on the FIRST reverse keypress from the floor too: \
         floored_x={floored_x}, after_first_grow_x={after_first_grow_x}"
    );
}

#[test]
fn snap_window_chassis_terminal_resize_detail_layout() {
    let mut app = test_app();
    let session_id = add_status_snapshot_sessions(&mut app);
    app.sessions
        .get_mut(&session_id)
        .expect("focused snapshot session should exist")
        .session
        .status = rsi_common::types::SessionStatus::Completed;
    focus_session_detail(&mut app, session_id);
    app.tabs[0].session_list_width_pct = 45;

    support::render_app(&mut app, 120, 26);
    let before = app.sessions[&session_id].last_content_area;
    assert_eq!(before.x, expected_detail_content_x(120, 45));

    let resized = support::render_app(&mut app, 110, 26);
    let after = app.sessions[&session_id].last_content_area;
    assert_eq!(after.x, expected_detail_content_x(110, 45));
    assert_ne!(before.x, after.x);

    insta::assert_snapshot!(
        "snap_window_chassis_terminal_resize_detail_layout",
        support::buffer_to_snapshot(&resized)
    );
}

#[tokio::test]
async fn snap_file_viewer_trailing_space_same_frame() {
    let mut app = test_app();
    add_test_sessions(&mut app, 1);
    let session_id = app.session_order[0];
    focus_session_detail(&mut app, session_id);
    let viewer = FileViewerState::new(PathBuf::from("/tmp/rsi-s-input.txt"), "alpha".into());
    app.sessions
        .get_mut(&session_id)
        .expect("test session should exist")
        .file_viewer = Some(viewer);

    feed_key(&mut app, key('A')).await;
    let before_version = app.sessions[&session_id]
        .file_viewer
        .as_ref()
        .expect("file viewer should be active")
        .content_version;
    feed_key(&mut app, key(' ')).await;

    let state = app.sessions[&session_id]
        .file_viewer
        .as_ref()
        .expect("file viewer should be active");
    assert_eq!(state.surface.content(), "alpha ");
    assert!(state.content_version > before_version);

    let buffer = support::render_app(&mut app, 90, 18);
    let snapshot = support::buffer_to_snapshot(&buffer);
    assert!(
        snapshot.contains("alpha·"),
        "trailing space should render visibly in the same frame:\n{snapshot}"
    );
    insta::assert_snapshot!("snap_file_viewer_trailing_space_same_frame", snapshot);
}

/// A file viewer opened from session detail replaces the entire pane, matching
/// the main session-list path rather than preserving the detail sidebar.
#[tokio::test]
async fn test_file_viewer_from_detail_matches_main_list_bounds_at_wide_terminal() {
    let mut app = test_app();
    add_test_sessions(&mut app, 1);
    let session_id = app.session_order[0];
    focus_session_detail(&mut app, session_id);
    let viewer = FileViewerState::new(PathBuf::from("/tmp/rsi-s-input.txt"), "alpha".into());
    app.sessions
        .get_mut(&session_id)
        .expect("test session should exist")
        .file_viewer = Some(viewer);

    // The default 37% detail sidebar is present at this width. The viewer must
    // still begin at the pane's left edge and span its full width.
    let buffer = support::render_app(&mut app, 220, 30);

    let total_width: u16 = 220;

    // y=1: y=0 is the top status/chrome bar.
    assert_eq!(buffer[(0, 1)].symbol(), "┌");
    assert_eq!(buffer[(total_width - 1, 1)].symbol(), "┐");
}

#[tokio::test]
async fn test_file_explorer_enter_opens_viewer_and_focuses_it() {
    let root = temp_test_dir("rsi-viewer-enter");
    let file_path = root.join("notes.md");
    fs::write(&file_path, "# Notes\n\nOpened from explorer.\n")
        .expect("test file should be written");

    let mut app = test_app();
    add_test_sessions(&mut app, 1);
    let session_id = app.session_order[0];
    let project_id = uuid::Uuid::new_v4();
    app.projects = vec![rsi_common::types::Project {
        id: project_id,
        name: "viewer".to_string(),
        path: Some(root.clone()),
        description: None,
        color: rsi_common::types::Project::DEFAULT_COLOR.to_string(),
        context_files: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }];
    app.current_project_id = Some(project_id);
    focus_session_detail(&mut app, session_id);

    rsi::overlay::file_explorer::open_file_explorer(&mut app);
    feed_key(&mut app, key_code(crossterm::event::KeyCode::Enter)).await;

    let viewer = app.sessions[&session_id]
        .file_viewer
        .as_ref()
        .expect("Enter on a file should open the file viewer");
    assert_eq!(viewer.file_path, file_path);
    assert_eq!(viewer.surface.content(), "# Notes\n\nOpened from explorer.");

    let OverlayState::FileExplorer {
        explorer_focused, ..
    } = app.overlay
    else {
        panic!("file explorer should stay open as a drawer");
    };
    assert!(!explorer_focused, "viewer should receive focus after open");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn file_explorer_drawer_renders_full_width_on_compact_wide_terminal() {
    let root = temp_test_dir("rsi-full-explorer-width");
    fs::write(root.join("notes.md"), "# Notes\n").expect("test file should be written");

    let mut app = test_app();
    let project_id = uuid::Uuid::new_v4();
    app.projects = vec![rsi_common::types::Project {
        id: project_id,
        name: "explorer".to_string(),
        path: Some(root.clone()),
        description: None,
        color: rsi_common::types::Project::DEFAULT_COLOR.to_string(),
        context_files: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }];
    app.current_project_id = Some(project_id);

    rsi::overlay::file_explorer::open_file_explorer(&mut app);
    let buffer = support::render_app(&mut app, 160, 30);

    // At 160 columns, the old transcript-gutter rule produced a 14-column
    // drawer. Verify actual rendered border geometry uses full 50-column cap.
    assert_eq!(buffer[(0, 0)].symbol(), "┌");
    assert_eq!(buffer[(49, 0)].symbol(), "┐");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn test_file_explorer_viewer_wraps_to_visible_drawer_width() {
    let root = temp_test_dir("rsi-viewer-drawer-width");
    let thoughts_dir = root.join("thoughts");
    fs::create_dir_all(&thoughts_dir).expect("thoughts dir should be created");
    let file_path = thoughts_dir.join("ideas-intake.md");
    fs::write(&file_path, ideas_intake_like_content()).expect("test file should be written");

    let mut app = test_app();
    add_test_sessions(&mut app, 1);
    let session_id = app.session_order[0];
    if let Some(Pane::SessionList {
        selected_session, ..
    }) = app.focused_pane_mut()
    {
        *selected_session = Some(session_id);
    }
    let project_id = uuid::Uuid::new_v4();
    app.projects = vec![rsi_common::types::Project {
        id: project_id,
        name: "viewer".to_string(),
        path: Some(root.clone()),
        description: None,
        color: rsi_common::types::Project::DEFAULT_COLOR.to_string(),
        context_files: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }];
    app.current_project_id = Some(project_id);

    rsi::overlay::file_explorer::open_file_explorer(&mut app);
    rsi::overlay::file_explorer::open_file_by_path(&mut app, &file_path);

    let buffer = support::render_app(&mut app, 100, 18);
    let snapshot = support::buffer_to_snapshot(&buffer);
    assert!(snapshot.contains("ideas-intake.md"));

    let viewer = app.sessions[&session_id]
        .file_viewer
        .as_ref()
        .expect("file viewer should be active");
    assert!(
        viewer.surface.wrap_width.get() <= 55,
        "viewer should wrap to the visible area beside the drawer, got wrap width {}",
        viewer.surface.wrap_width.get()
    );

    feed_key(&mut app, key(':')).await;
    feed_key(&mut app, key('4')).await;
    feed_key(&mut app, key('5')).await;
    feed_key(&mut app, key_code(crossterm::event::KeyCode::Enter)).await;
    support::render_app(&mut app, 100, 18);
    let before = app.sessions[&session_id]
        .file_viewer
        .as_ref()
        .expect("file viewer should still be active")
        .viewport_top;
    let viewport_height = app.sessions[&session_id]
        .file_viewer
        .as_ref()
        .expect("file viewer should still be active")
        .viewport_height;

    for _ in 0..viewport_height + 2 {
        feed_key(&mut app, key('j')).await;
    }
    support::render_app(&mut app, 100, 18);
    let after = app.sessions[&session_id]
        .file_viewer
        .as_ref()
        .expect("file viewer should still be active")
        .viewport_top;
    assert!(
        after > before,
        "viewer should scroll through wrapped visual rows beside the drawer"
    );

    let OverlayState::FileExplorer {
        explorer_focused, ..
    } = app.overlay
    else {
        panic!("file explorer drawer should remain open");
    };
    assert!(!explorer_focused, "viewer should keep focus");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn test_file_viewer_page_scroll_moves_viewport() {
    let mut app = test_app();
    add_test_sessions(&mut app, 1);
    let session_id = app.session_order[0];
    focus_session_detail(&mut app, session_id);
    let viewer = FileViewerState::new(
        PathBuf::from("/tmp/rsi-scroll.txt"),
        numbered_lines(80).join("\n"),
    );
    app.sessions
        .get_mut(&session_id)
        .expect("test session should exist")
        .file_viewer = Some(viewer);

    support::render_app(&mut app, 90, 20);
    let first_step = app.sessions[&session_id]
        .file_viewer
        .as_ref()
        .expect("file viewer should be active")
        .viewport_height;

    feed_key(&mut app, key_code(crossterm::event::KeyCode::PageDown)).await;
    support::render_app(&mut app, 90, 20);
    let viewer = app.sessions[&session_id]
        .file_viewer
        .as_ref()
        .expect("file viewer should still be active");
    assert_eq!(viewer.surface.textarea.cursor().0, first_step.min(79));
    assert!(viewer.viewport_top > 0);

    feed_key(&mut app, key_code(crossterm::event::KeyCode::PageUp)).await;
    support::render_app(&mut app, 90, 20);
    let viewer = app.sessions[&session_id]
        .file_viewer
        .as_ref()
        .expect("file viewer should still be active");
    assert_eq!(viewer.surface.textarea.cursor().0, 0);
    assert_eq!(viewer.viewport_top, 0);
}

#[tokio::test]
async fn test_file_viewer_wrapped_cursor_scrolls_viewport() {
    let mut app = test_app();
    add_test_sessions(&mut app, 1);
    let session_id = app.session_order[0];
    focus_session_detail(&mut app, session_id);
    let viewer = FileViewerState::new(
        PathBuf::from("/tmp/rsi-wrapped-scroll.txt"),
        "x".repeat(2000),
    );
    app.sessions
        .get_mut(&session_id)
        .expect("test session should exist")
        .file_viewer = Some(viewer);

    support::render_app(&mut app, 50, 12);
    let viewport_height = app.sessions[&session_id]
        .file_viewer
        .as_ref()
        .expect("file viewer should be active")
        .viewport_height;

    for _ in 0..viewport_height + 2 {
        feed_key(&mut app, key('j')).await;
    }

    support::render_app(&mut app, 50, 12);
    let viewer = app.sessions[&session_id]
        .file_viewer
        .as_ref()
        .expect("file viewer should still be active");
    let (cursor_row, cursor_col) = viewer.surface.textarea.cursor();
    assert_eq!(cursor_row, 0, "cursor should still be on the wrapped line");
    assert!(cursor_col > 0, "j should move through wrapped visual rows");
    assert!(
        viewer.viewport_top > 0,
        "viewport should scroll when the cursor moves below the visible wrapped rows"
    );
}

#[test]
fn test_file_viewer_cursor_line_highlight_is_single_render_row() {
    let mut app = test_app();
    add_test_sessions(&mut app, 1);
    let session_id = app.session_order[0];
    focus_session_detail(&mut app, session_id);
    let viewer = FileViewerState::new(
        PathBuf::from("/tmp/rsi-highlight.txt"),
        "alpha\nbeta\ngamma".to_string(),
    );
    app.sessions
        .get_mut(&session_id)
        .expect("test session should exist")
        .file_viewer = Some(viewer);

    let buffer = support::render_app(&mut app, 90, 16);
    let highlighted =
        support::find_cells_with_bg(&buffer, rsi::ui::theme::file_viewer_cursor_line_bg());
    let rows: std::collections::BTreeSet<u16> = highlighted.into_iter().map(|(_, y)| y).collect();
    assert_eq!(
        rows.len(),
        1,
        "cursor-line background should only occupy one rendered row"
    );
    let row = *rows.iter().next().expect("highlighted row should exist");
    let row_text = support::line_text(&buffer, row);
    assert!(row_text.contains("alpha"));
    assert!(!row_text.contains("beta"));
}

#[test]
fn test_file_viewer_rendered_rows_support_mouse_cursor_placement() {
    let mut app = test_app();
    add_test_sessions(&mut app, 1);
    let session_id = app.session_order[0];
    focus_session_detail(&mut app, session_id);
    app.sessions
        .get_mut(&session_id)
        .expect("test session should exist")
        .file_viewer = Some(FileViewerState::new(
        PathBuf::from("/tmp/rsi-mouse.txt"),
        "alpha\nbeta\ngamma".to_string(),
    ));

    support::render_app(&mut app, 90, 20);
    let (content_area, editor_area) = {
        let viewer = app.sessions[&session_id]
            .file_viewer
            .as_ref()
            .expect("file viewer should be active");
        (
            viewer
                .mouse_layout
                .content_area
                .expect("content area should render"),
            viewer
                .mouse_layout
                .editor_area
                .expect("editor area should render"),
        )
    };

    let viewer = app
        .sessions
        .get_mut(&session_id)
        .expect("test session should exist")
        .file_viewer
        .as_mut()
        .expect("file viewer should be active");
    assert!(rsi::file_viewer::set_cursor_from_mouse(
        viewer,
        content_area.x + 2,
        content_area.y + 1,
    ));
    assert_eq!(viewer.surface.textarea.cursor(), (1, 2));

    assert!(rsi::file_viewer::set_cursor_from_mouse(
        viewer,
        editor_area.x,
        content_area.y + 2,
    ));
    assert_eq!(viewer.surface.textarea.cursor(), (2, 0));
}

#[tokio::test]
async fn snap_file_viewer_markdown_preview_page_scroll() {
    let mut app = test_app();
    add_test_sessions(&mut app, 1);
    let session_id = app.session_order[0];
    focus_session_detail(&mut app, session_id);
    let mut markdown = vec![
        "# Viewer Notes".to_string(),
        String::new(),
        "Intro with **bold text** and `inline code`.".to_string(),
        String::new(),
        "- first item".to_string(),
        "- second item".to_string(),
        String::new(),
        "```rust".to_string(),
        "fn main() {".to_string(),
        "    println!(\"hi\");".to_string(),
        "}".to_string(),
        "```".to_string(),
        String::new(),
    ];
    for n in 1..=18 {
        markdown.push(format!("## Section {n:02}"));
        markdown.push(format!("Paragraph {n:02} with stable preview text."));
        markdown.push(String::new());
    }
    let viewer = FileViewerState::new(PathBuf::from("/tmp/rsi-preview.md"), markdown.join("\n"));
    app.sessions
        .get_mut(&session_id)
        .expect("test session should exist")
        .file_viewer = Some(viewer);

    feed_key(&mut app, key(' ')).await;
    feed_key(&mut app, key('m')).await;
    let initial = support::render_app(&mut app, 96, 22);
    let initial_snapshot = support::buffer_to_snapshot(&initial);
    assert!(initial_snapshot.contains("Viewer Notes"));
    assert!(initial_snapshot.contains("bold text"));
    assert!(!initial_snapshot.contains("**bold text**"));

    feed_key(&mut app, key_code(crossterm::event::KeyCode::PageDown)).await;
    let scrolled = support::render_app(&mut app, 96, 22);
    let scrolled_snapshot = support::buffer_to_snapshot(&scrolled);
    let viewer = app.sessions[&session_id]
        .file_viewer
        .as_ref()
        .expect("file viewer should still be active");
    assert!(viewer.markdown_preview);
    assert!(viewer.viewport_top > 0);
    assert!(scrolled_snapshot.contains("Section"));
    assert!(!scrolled_snapshot.contains("Viewer Notes"));
    insta::assert_snapshot!(
        "snap_file_viewer_markdown_preview_page_scroll",
        scrolled_snapshot
    );
}

#[tokio::test]
async fn snap_prompt_scrolloff_keeps_cursor_context() {
    let mut app = test_app();
    let lines: Vec<String> = (0..12).map(|i| format!("line {i:02}")).collect();
    open_blank_prompt_with_draft(&mut app, lines);
    support::render_app(&mut app, 100, 24);

    for _ in 0..5 {
        feed_key(&mut app, key_code(crossterm::event::KeyCode::Down)).await;
    }

    let OverlayState::Prompt { surface, .. } = &app.input_overlays[0] else {
        panic!("blank prompt should be active");
    };
    assert_eq!(surface.textarea.cursor().0, 5);
    let line_count = surface.textarea.lines().len();

    let buffer = support::render_app(&mut app, 100, 24);
    let rect = support::find_prompt_rect_by_marker(&buffer, "cwd: ");
    let textarea_top = rect.y + 3;
    let textarea_bottom = rect.y + rect.height - 4;
    let cursor_cells = support::find_cells_with_bg(&buffer, rsi::ui::theme::cursor_insert_bg());
    let cursor = cursor_cells
        .into_iter()
        .find(|(x, y)| {
            *x >= rect.x && *x < rect.x + rect.width && *y >= textarea_top && *y <= textarea_bottom
        })
        .expect("insert cursor cell should be rendered inside the prompt textarea");
    assert_eq!(cursor.1, textarea_bottom - 3);
    assert!(line_count > (cursor.1 - textarea_top + 1) as usize);

    insta::assert_snapshot!(
        "snap_prompt_scrolloff_keeps_cursor_context",
        support::buffer_to_snapshot(&buffer)
    );
}

#[tokio::test]
async fn snap_prompt_manual_resize_newline_stable() {
    let mut app = test_app();
    open_blank_prompt_with_draft(
        &mut app,
        vec![
            "line 1".to_string(),
            "line 2".to_string(),
            "line 3".to_string(),
        ],
    );
    support::render_app(&mut app, 100, 28);

    feed_key(
        &mut app,
        modified_key_code(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::SHIFT,
        ),
    )
    .await;
    let resized = support::render_app(&mut app, 100, 28);
    let resized_rect = support::find_prompt_rect_by_marker(&resized, "cwd: ");

    feed_key(&mut app, key_code(crossterm::event::KeyCode::Enter)).await;
    let after_newline = support::render_app(&mut app, 100, 28);
    let after_rect = support::find_prompt_rect_by_marker(&after_newline, "cwd: ");
    assert_eq!(after_rect.height, resized_rect.height);

    let OverlayState::Prompt { surface, .. } = &app.input_overlays[0] else {
        panic!("blank prompt should be active");
    };
    assert_eq!(surface.textarea.lines().len(), 4);
    insta::assert_snapshot!(
        "snap_prompt_manual_resize_newline_stable",
        support::buffer_to_snapshot(&after_newline)
    );
}

#[tokio::test]
async fn test_prompt_manual_resize_tall_content_grows_from_dynamic_height() {
    let mut app = test_app();
    open_blank_prompt_with_draft(&mut app, numbered_lines(8));

    let initial = support::render_app(&mut app, 100, 28);
    let initial_rect = support::find_prompt_rect_by_marker(&initial, "cwd: ");

    feed_key(
        &mut app,
        modified_key_code(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::SHIFT,
        ),
    )
    .await;
    let resized = support::render_app(&mut app, 100, 28);
    let resized_rect = support::find_prompt_rect_by_marker(&resized, "cwd: ");
    assert_eq!(
        resized_rect.height,
        initial_rect.height + 2,
        "first manual resize should grow from the content-driven height"
    );

    feed_key(&mut app, key_code(crossterm::event::KeyCode::Enter)).await;
    let after_newline = support::render_app(&mut app, 100, 28);
    let after_rect = support::find_prompt_rect_by_marker(&after_newline, "cwd: ");
    assert_eq!(
        after_rect.height, resized_rect.height,
        "content growth after manual resize should not grow the popup"
    );
}

#[tokio::test]
async fn test_legacy_taskrabbit_override_uses_manual_height_semantics() {
    let mut app = test_app();
    open_blank_prompt_with_draft(&mut app, numbered_lines(3));
    let mut stacked = app
        .input_overlays
        .pop()
        .expect("blank prompt should have opened");
    if let OverlayState::Prompt { working_dir, .. } = &mut stacked {
        *working_dir = PathBuf::from("/tmp/rsi-legacy-blank");
    }
    app.overlay_stack.push(stacked);
    app.focused_input_idx = 0;

    app.taskrabbit_draft = numbered_lines(8);
    rsi::overlay::open_taskrabbit_popup(&mut app);
    let mut taskrabbit = app
        .input_overlays
        .pop()
        .expect("taskrabbit prompt should have opened");
    if let OverlayState::Prompt { working_dir, .. } = &mut taskrabbit {
        *working_dir = PathBuf::from("/tmp/rsi-legacy-taskrabbit");
    }
    app.overlay = taskrabbit;
    app.focused_input_idx = 0;

    let initial = support::render_app(&mut app, 100, 32);
    let initial_rect = support::find_prompt_rect_by_marker(&initial, "/tmp/rsi-legacy-taskrabbit");

    feed_key(
        &mut app,
        modified_key_code(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::SHIFT,
        ),
    )
    .await;
    let resized = support::render_app(&mut app, 100, 32);
    let resized_rect = support::find_prompt_rect_by_marker(&resized, "/tmp/rsi-legacy-taskrabbit");
    assert_eq!(
        resized_rect.height,
        initial_rect.height + 2,
        "legacy TaskRabbit override should grow from the content-driven height"
    );

    feed_key(&mut app, key_code(crossterm::event::KeyCode::Enter)).await;
    let after_newline = support::render_app(&mut app, 100, 32);
    let after_rect =
        support::find_prompt_rect_by_marker(&after_newline, "/tmp/rsi-legacy-taskrabbit");
    assert_eq!(
        after_rect.height, resized_rect.height,
        "legacy TaskRabbit override should pin manual height after content growth"
    );
}

// === Key-driven integration tests (rewritten for KeyManager path) ===

#[tokio::test]
async fn test_navigation_keys() {
    let mut app = test_app();
    add_test_sessions(&mut app, 3);

    // j moves down
    feed_key(&mut app, key('j')).await;
    match app.focused_pane() {
        Some(Pane::SessionList { selected_index, .. }) => assert_eq!(*selected_index, 1),
        _ => panic!("Expected SessionList"),
    }

    // k moves up
    feed_key(&mut app, key('k')).await;
    match app.focused_pane() {
        Some(Pane::SessionList { selected_index, .. }) => assert_eq!(*selected_index, 0),
        _ => panic!("Expected SessionList"),
    }

    // G jumps to end
    feed_key(&mut app, key('G')).await;
    match app.focused_pane() {
        Some(Pane::SessionList { selected_index, .. }) => assert_eq!(*selected_index, 2),
        _ => panic!("Expected SessionList"),
    }
}

#[tokio::test]
async fn test_command_quit() {
    let mut app = test_app();

    // ':' opens the command palette, where exact command aliases still run.
    feed_key(&mut app, key(':')).await;
    assert!(matches!(app.overlay, OverlayState::CommandPalette { .. }));

    // Type "quit" and execute the exact command.
    for c in "quit".chars() {
        feed_key(&mut app, key(c)).await;
    }

    feed_key(&mut app, key_code(crossterm::event::KeyCode::Enter)).await;

    assert!(app.quit);
}

#[tokio::test]
async fn test_tab_workflow() {
    let mut app = test_app();
    assert_eq!(app.tabs.len(), 1);

    // :tabnew via command mode
    app.input_mode = InputMode::Command;
    app.command_buffer = "tabnew".to_string();
    feed_key(&mut app, key_code(crossterm::event::KeyCode::Enter)).await;
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(app.active_tab, 1);

    // > (NextTab, relocated off L for T4 yazi container nav) wraps to tab 0
    feed_key(&mut app, key('>')).await;
    assert_eq!(app.active_tab, 0);
}

#[tokio::test]
async fn test_split_and_focus_workflow() {
    let mut app = test_app();

    // :vsplit via command mode
    app.input_mode = InputMode::Command;
    app.command_buffer = "vsplit".to_string();
    feed_key(&mut app, key_code(crossterm::event::KeyCode::Enter)).await;

    let leaf_ids = app.active_tab().layout.leaf_ids();
    assert_eq!(leaf_ids.len(), 2);
    assert_eq!(app.active_tab().focused_pane, leaf_ids[0]);

    // Ctrl-W l moves focus right
    feed_key(&mut app, ctrl_key('w')).await;
    feed_key(&mut app, key('l')).await;
    assert_eq!(app.active_tab().focused_pane, leaf_ids[1]);

    // :close removes the focused pane
    app.input_mode = InputMode::Command;
    app.command_buffer = "close".to_string();
    feed_key(&mut app, key_code(crossterm::event::KeyCode::Enter)).await;
    assert_eq!(app.active_tab().layout.leaf_ids().len(), 1);
}

// === New integration tests for features added in Phase 4C ===

#[tokio::test]
async fn test_zq_quits() {
    let mut app = test_app();
    feed_key(&mut app, key('Z')).await;
    feed_key(&mut app, key('Q')).await;
    assert!(app.quit);
}

#[tokio::test]
async fn test_zz_quits() {
    let mut app = test_app();
    feed_key(&mut app, key('Z')).await;
    feed_key(&mut app, key('Z')).await;
    assert!(app.quit);
}

#[tokio::test]
async fn test_q_does_not_quit() {
    let mut app = test_app();
    feed_key(&mut app, key('q')).await;
    assert!(!app.quit);
}

#[tokio::test]
async fn test_ctrl_c_quits() {
    let mut app = test_app();
    feed_key(&mut app, ctrl_key('c')).await;
    assert!(app.quit);
}

#[test]
fn test_markdown_rendering_produces_styled_spans() {
    use rsi::ui::content;

    // Bold
    let spans = content::parse_inline_markdown("hello **world**");
    assert!(spans.len() >= 2);

    // Block-level header
    let (block, text) = content::detect_block_element("## Title");
    assert_eq!(block, content::BlockElement::Header(2));
    assert_eq!(text, "Title");

    // System event toggle default
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
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
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
        continued_from: None,
        context_usage_confidence: rsi_common::types::ContextUsageConfidence::Missing,
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
    let state = SessionState::new(session);
    assert!(!state.show_system_events);
}

#[tokio::test]
async fn test_gg_jumps_to_top() {
    let mut app = test_app();
    add_test_sessions(&mut app, 5);

    // Navigate down
    feed_key(&mut app, key('j')).await;
    feed_key(&mut app, key('j')).await;

    // gg jumps to top
    feed_key(&mut app, key('g')).await;
    feed_key(&mut app, key('g')).await;

    match app.focused_pane() {
        Some(Pane::SessionList { selected_index, .. }) => assert_eq!(*selected_index, 0),
        _ => panic!("Expected SessionList"),
    }
}

#[tokio::test]
async fn test_gs_goes_back_to_session_list() {
    let mut app = test_app();
    add_test_sessions(&mut app, 1);

    // Start in session list, select first session
    let first_id = app.session_order.first().copied();
    if let Some(Pane::SessionList {
        selected_session, ..
    }) = app.focused_pane_mut()
    {
        *selected_session = first_id;
    }

    // Enter session detail
    app.enter_session();

    // Verify we're in detail view
    match app.focused_pane() {
        Some(Pane::SessionDetail { .. }) => {}
        other => panic!("Expected SessionDetail, got {:?}", other),
    }

    // Use gs keybinding to go back to session list
    feed_key(&mut app, key('g')).await;
    feed_key(&mut app, key('s')).await;

    // Verify we're back in session list
    match app.focused_pane() {
        Some(Pane::SessionList { .. }) => {}
        other => panic!("Expected SessionList after gs, got {:?}", other),
    }
}

#[tokio::test]
async fn issue_workspace_space_i_opens_focuses_preserves_uuid_and_restores_pane() {
    let mut app = test_app();
    let project_id = uuid::Uuid::new_v4();
    let issue_id = uuid::Uuid::new_v4();
    app.tabs[app.active_tab].project_id = Some(project_id);

    feed_key(&mut app, key(' ')).await;
    feed_key(&mut app, key('i')).await;
    let issues_pane = app.active_tab().focused_pane;
    match app.focused_pane_mut() {
        Some(Pane::Issues(state)) => {
            assert_eq!(state.project_id, Some(project_id));
            state.local.selected_issue_id = Some(issue_id);
            state.local.scroll_offset = 7;
            state.local.filters.archive = rsi_common::types::IssueArchiveFilterV1::All;
        }
        other => panic!("expected first-class Issues pane, got {other:?}"),
    }

    feed_key(&mut app, key(' ')).await;
    feed_key(&mut app, key('i')).await;
    assert_eq!(app.active_tab().focused_pane, issues_pane);
    assert!(matches!(
        app.focused_pane(),
        Some(Pane::Issues(state))
            if state.local.selected_issue_id == Some(issue_id)
                && state.local.scroll_offset == 7
                && state.local.filters.archive == rsi_common::types::IssueArchiveFilterV1::All
    ));

    feed_key(
        &mut app,
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ),
    )
    .await;
    assert!(matches!(app.focused_pane(), Some(Pane::SessionList { .. })));
}

#[tokio::test]
async fn issue_workspace_without_project_starts_no_issue_request() {
    let mut app = test_app();
    feed_key(&mut app, key(' ')).await;
    feed_key(&mut app, key('i')).await;
    assert!(matches!(
        app.focused_pane(),
        Some(Pane::Issues(state))
            if state.project_id.is_none() && state.transient.in_flight.is_empty()
    ));
}
