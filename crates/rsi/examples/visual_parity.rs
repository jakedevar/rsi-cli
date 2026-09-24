use std::fs;
use std::path::{Path, PathBuf};

use chrono::{Duration, Utc};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use rsi::app::{App, SortOrder};
use rsi::client::DaemonClient;
use rsi::settings::ActivityIndicatorStyle;
use rsi::settings_registry::SettingsSection;
use rsi::types::{
    BottomZone, Pane, PaneId, PopupMode, SessionFocusGroup, SessionState, SettingsFocus, SplitNode,
    Tab, compute_session_focus_index,
};
use rsi::ui;
use rsi_common::types::{
    ContextUsageConfidence, ConversationEvent, EventType, PendingQuestion, Project, QuestionItem,
    Role, Session, SessionKind, SessionProvider, SessionStatus,
};
use uuid::Uuid;

const WIDE_COLS: u16 = 200;
const WIDE_ROWS: u16 = 58;
const NARROW_COLS: u16 = 120;
const NARROW_ROWS: u16 = 40;
const ULTRA_COLS: u16 = 240;
const ULTRA_ROWS: u16 = 70;
const CELL_WIDTH_PX: f32 = 8.0;
const CELL_HEIGHT_PX: f32 = 18.0;

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let fixture_now = Utc::now();
    let fixture_theme =
        std::env::var("RSI_VISUAL_PARITY_THEME").unwrap_or_else(|_| "transparent".to_string());

    let out_dir = PathBuf::from("target/visual-parity");
    fs::create_dir_all(&out_dir)?;

    let mut list_app = fixture_app(false, fixture_now, &fixture_theme);
    render_svg(
        &mut list_app,
        &out_dir.join("session-browser-bounded-relay-200x58.svg"),
        WIDE_COLS,
        WIDE_ROWS,
    )?;

    let mut narrow_list_app = fixture_app(false, fixture_now, &fixture_theme);
    render_svg(
        &mut narrow_list_app,
        &out_dir.join("session-browser-bounded-relay-120x40.svg"),
        NARROW_COLS,
        NARROW_ROWS,
    )?;

    let mut ultra_list_app = fixture_app(false, fixture_now, &fixture_theme);
    render_svg(
        &mut ultra_list_app,
        &out_dir.join("session-browser-bounded-relay-240x70.svg"),
        ULTRA_COLS,
        ULTRA_ROWS,
    )?;

    for (style, name) in [
        (
            ActivityIndicatorStyle::Semantic,
            "session-detail-semantic-200x58.svg",
        ),
        (
            ActivityIndicatorStyle::RainbowClassic,
            "session-detail-rainbow-classic-200x58.svg",
        ),
        (
            ActivityIndicatorStyle::RainbowCompact,
            "session-detail-rainbow-compact-200x58.svg",
        ),
    ] {
        let mut detail_app = fixture_app(true, fixture_now, &fixture_theme);
        detail_app.settings.activity_indicator_style = style;
        render_svg(&mut detail_app, &out_dir.join(name), WIDE_COLS, WIDE_ROWS)?;
    }

    for (cols, rows, name) in [
        (
            NARROW_COLS,
            NARROW_ROWS,
            "settings-context-workbench-120x40.svg",
        ),
        (
            WIDE_COLS,
            WIDE_ROWS,
            "settings-context-workbench-200x58.svg",
        ),
        (
            ULTRA_COLS,
            ULTRA_ROWS,
            "settings-context-workbench-240x70.svg",
        ),
    ] {
        let mut settings_app = settings_fixture_app(fixture_now, &fixture_theme);
        render_svg(&mut settings_app, &out_dir.join(name), cols, rows)?;
    }

    println!("wrote {}", out_dir.display());
    Ok(())
}

fn render_svg(app: &mut App, path: &Path, cols: u16, rows: u16) -> color_eyre::Result<()> {
    let backend = TestBackend::new(cols, rows);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| ui::render(frame, app))?;
    let svg = buffer_to_svg(terminal.backend().buffer());
    fs::write(path, svg)?;
    Ok(())
}

fn fixture_app(detail: bool, fixture_now: chrono::DateTime<Utc>, fixture_theme: &str) -> App {
    let client = DaemonClient::new(PathBuf::from("/tmp/rsi-visual-parity.sock"));
    let mut app = App::new(client);
    assert!(
        rsi::ui::theme::set_theme_by_name(fixture_theme),
        "unknown RSI_VISUAL_PARITY_THEME={fixture_theme}"
    );
    app.settings.sort_order = SortOrder::FreshestFirst;
    app.selected_provider = SessionProvider::Codex;
    app.selected_model = Some("gpt-5.5".to_string());
    app.available_models = vec![("gpt-5.5".to_string(), "gpt-5.5".to_string())];
    app.poll.connected = true;

    let project_id = stable_id("project-rsi");
    app.projects = vec![Project {
        id: project_id,
        name: "rsi".to_string(),
        path: Some(PathBuf::from("/home/jakedevar/rsi")),
        description: None,
        color: "#89d185".to_string(),
        context_files: None,
        created_at: fixture_now,
        updated_at: fixture_now,
    }];
    app.current_project_id = Some(project_id);

    let rows = [
        (
            "Context-window patch for PCT denominator",
            "Implement the context-window patch for the PCT denominator...",
            SessionStatus::Running,
            "gpt-5.5",
            3.50,
            42,
            3.0,
            4,
        ),
        (
            "SQLite migration for prompt storage",
            "Migrate prompt storage to SQLite with FTS and metadata...",
            SessionStatus::Starting,
            "gpt-5.5",
            2.93,
            31,
            2.0,
            7,
        ),
        (
            "Career / negotiation strategy brainstorming",
            "Initial thoughts on career direction and negotiation...",
            SessionStatus::WaitingApproval,
            "opus",
            2.90,
            18,
            1.0,
            2 * 24 * 60,
        ),
        (
            "Agent fallback memory investigation",
            "Investigate and resolve memory fallback in agent actors...",
            SessionStatus::Running,
            "gpt-5.5",
            1.82,
            27,
            4.0,
            2 * 24 * 60,
        ),
        (
            "Image generation via Codex CLI",
            "Implement image generation capabilities in the Codex CLI...",
            SessionStatus::Completed,
            "gpt-5.5",
            1.20,
            14,
            2.0,
            3 * 24 * 60,
        ),
        (
            "Active Work Verification cockpit",
            "Design core architecture and UI components for AWV...",
            SessionStatus::Completed,
            "gpt-5.5",
            0.91,
            9,
            1.0,
            3 * 24 * 60,
        ),
        (
            "Discrepancy in memory fallback dropdown",
            "Investigating discrepancy between memory model fallback...",
            SessionStatus::Failed,
            "opus",
            0.76,
            11,
            1.0,
            4 * 24 * 60,
        ),
        (
            "Model selector UX polish",
            "Improve model selector navigation and search...",
            SessionStatus::Running,
            "gpt-5.5",
            0.62,
            8,
            2.0,
            5 * 24 * 60,
        ),
        (
            "Project picker overlay refactor",
            "Refactor project picker overlay for performance...",
            SessionStatus::Running,
            "gpt-5.5",
            0.38,
            7,
            1.0,
            5 * 24 * 60,
        ),
        (
            "TaskRabbit temp dir cleanup",
            "Fix TaskRabbit temp dir cleanup on interrupts...",
            SessionStatus::Completed,
            "opus",
            0.27,
            6,
            1.0,
            6 * 24 * 60,
        ),
        (
            "Bug report template improvements",
            "Improve bug_report_template with system info...",
            SessionStatus::Completed,
            "gpt-5.5",
            0.19,
            5,
            1.0,
            7 * 24 * 60,
        ),
        (
            "Docs: keybindings reference",
            "Update keybindings.md with new actions...",
            SessionStatus::Completed,
            "gpt-5.5",
            0.14,
            4,
            1.0,
            8 * 24 * 60,
        ),
        (
            "Overlay stacking bug investigation",
            "",
            SessionStatus::Completed,
            "opus",
            0.11,
            3,
            1.0,
            10 * 24 * 60,
        ),
        (
            "Status bar info density pass",
            "",
            SessionStatus::Completed,
            "gpt-4o",
            0.09,
            2,
            1.0,
            12 * 24 * 60,
        ),
        (
            "Daemon reconnect robustness",
            "",
            SessionStatus::Completed,
            "opus",
            0.07,
            3,
            1.0,
            13 * 24 * 60,
        ),
        (
            "Prompt preview rendering fixes",
            "",
            SessionStatus::Completed,
            "gpt-5.5",
            0.05,
            2,
            1.0,
            14 * 24 * 60,
        ),
        (
            "Audio viz pipewire integration",
            "",
            SessionStatus::Completed,
            "gpt-5.5",
            0.04,
            2,
            1.0,
            16 * 24 * 60,
        ),
        (
            "Approval UI micro-interactions",
            "",
            SessionStatus::Completed,
            "opus",
            0.03,
            1,
            1.0,
            18 * 24 * 60,
        ),
        (
            "Theme polish: Catppuccin tweaks",
            "",
            SessionStatus::Completed,
            "gpt-4o",
            0.02,
            1,
            1.0,
            19 * 24 * 60,
        ),
        (
            "Jumplist persistence fix",
            "",
            SessionStatus::Completed,
            "gpt-5.5",
            0.02,
            1,
            1.0,
            21 * 24 * 60,
        ),
        (
            "Early prototype notes",
            "",
            SessionStatus::Archived,
            "gpt-4o",
            0.01,
            1,
            1.0,
            36 * 24 * 60,
        ),
        (
            "Old session for parser experiments",
            "",
            SessionStatus::Archived,
            "opus",
            0.01,
            1,
            1.0,
            41 * 24 * 60,
        ),
        (
            "Initial brainstorming dump",
            "",
            SessionStatus::Interrupted,
            "gpt-3.5",
            0.00,
            1,
            1.0,
            52 * 24 * 60,
        ),
        (
            "Release train coordination",
            "Coordinate the bounded relay release train.",
            SessionStatus::Completed,
            "gpt-5.5",
            0.18,
            6,
            2.0,
            2 * 24 * 60,
        ),
        (
            "Session browser redesign epic",
            "Track the session browser visual-parity slices.",
            SessionStatus::Completed,
            "gpt-5.5",
            0.00,
            0,
            0.0,
            2 * 24 * 60,
        ),
        (
            "Validate live transparency",
            "Capture transparent and opaque terminal evidence.",
            SessionStatus::Completed,
            "gpt-5.5",
            0.08,
            3,
            1.0,
            2 * 24 * 60,
        ),
        (
            "Tether scroll regression",
            "Verify tether alignment near the bottom viewport.",
            SessionStatus::Completed,
            "gpt-5.5",
            0.12,
            5,
            2.0,
            3 * 24 * 60,
        ),
        (
            "Theme contrast audit",
            "Audit selectable title contrast across themes.",
            SessionStatus::Completed,
            "opus",
            0.06,
            2,
            1.0,
            4 * 24 * 60,
        ),
        (
            "Mouse gutter regression",
            "Protect centered gutter and inspector hit testing.",
            SessionStatus::Completed,
            "gpt-5.5",
            0.04,
            2,
            1.0,
            5 * 24 * 60,
        ),
        (
            "Operator queue bounds",
            "Keep cross-session activity selection independent.",
            SessionStatus::Completed,
            "gpt-5.5",
            0.05,
            2,
            1.0,
            6 * 24 * 60,
        ),
        (
            "Inspector state templates",
            "Verify exhaustive state-adaptive inspector bodies.",
            SessionStatus::Completed,
            "opus",
            0.07,
            3,
            1.0,
            7 * 24 * 60,
        ),
        (
            "Exact canvas snapshots",
            "Pin 120, 200, and 240 column render contracts.",
            SessionStatus::Completed,
            "gpt-5.5",
            0.03,
            2,
            1.0,
            8 * 24 * 60,
        ),
        (
            "Legacy compact preservation",
            "Protect embedded and non-main list behavior.",
            SessionStatus::Completed,
            "gpt-5.5",
            0.03,
            2,
            1.0,
            9 * 24 * 60,
        ),
    ];

    let mut order = Vec::new();
    for (idx, row) in rows.iter().enumerate() {
        let id = stable_id(&format!("session-{idx}"));
        let mut session = baseline_session(id, project_id, fixture_now);
        session.title = Some(row.0.to_string());
        session.short_summary = (!row.1.is_empty()).then(|| row.1.to_string());
        session.status = row.2;
        session.model = Some(row.3.to_string());
        session.cost_usd = Some(row.4);
        session.num_turns = Some(row.5);
        session.updated_at = if idx >= 23 {
            fixture_now - Duration::days(60 + idx as i64)
        } else {
            fixture_now - Duration::minutes(row.7)
        };
        session.created_at = session.updated_at - Duration::minutes(17);
        session.context_window = Some(200_000);
        if idx == 2 {
            session.pending_question = Some(PendingQuestion {
                questions: vec![QuestionItem {
                    question:
                        "Should the negotiation plan optimize for title, cash, or optionality?"
                            .to_string(),
                    header: "Priority".to_string(),
                    options: Vec::new(),
                    multi_select: false,
                }],
            });
        }
        if idx == 4 {
            session.test_passed = Some(true);
            session.clippy_passed = Some(true);
            session.pipeline_artifact = Some("thoughts/shared/plans/browser-relay.md".to_string());
        }
        if idx == 6 {
            session.stop_reason =
                Some("retry budget exhausted after snapshot mismatch".to_string());
            session.retry_attempt = Some(3);
            session.max_retries = Some(3);
        }
        if idx == 23 {
            session.session_kind = SessionKind::Group;
            session.provider = SessionProvider::Local;
            session.model = None;
        }
        if idx == 24 {
            session.session_kind = SessionKind::Epic;
            session.parent_id = Some(stable_id("session-23"));
            session.provider = SessionProvider::Local;
            session.model = None;
        }
        if idx >= 25 {
            session.session_kind = SessionKind::Task;
            session.parent_id = Some(stable_id("session-24"));
        }
        let mut state = SessionState::new(session);
        state.live_context_pct = Some(row.6);
        if idx == 0 {
            state.session.work_time_ms = Some(193_000);
            install_detail_events(&mut state, fixture_now);
            state.input_bar.surface.mode = PopupMode::Insert;
        }
        app.sessions.insert(id, state);
        app.session_order.push(id);
        order.push(id);
    }
    app.children_by_parent
        .insert(Some(stable_id("session-23")), vec![stable_id("session-24")]);
    app.children_by_parent.insert(
        Some(stable_id("session-24")),
        (25..rows.len())
            .map(|idx| stable_id(&format!("session-{idx}")))
            .collect(),
    );

    let detail_session = order.first().copied();
    let focus_index = compute_session_focus_index(&app.sessions, fixture_now);
    order.sort_by_key(|id| {
        focus_index
            .get(id)
            .map_or(SessionFocusGroup::Quiet, |entry| entry.group)
    });
    app.session_order = order.clone();
    app.filtered_session_order = order.clone();
    let selected_session = if detail {
        detail_session
    } else {
        order.first().copied()
    };
    let selected_index = selected_session
        .and_then(|id| order.iter().position(|candidate| *candidate == id))
        .unwrap_or(0);
    let list_pane = Pane::SessionList {
        selected_index,
        selected_session,
        scroll_offset: 0,
        active_zone: Default::default(),
        taskrabbit_selected_index: 0,
        archive_selected_index: 0,
        jobs_selected_index: 0,
    };

    let pane_id = PaneId(0);
    let layout = if detail {
        SplitNode::Leaf {
            pane: Pane::SessionDetail {
                session_id: selected_session.expect("fixture selected session"),
            },
            id: pane_id,
        }
    } else {
        SplitNode::Leaf {
            pane: list_pane.clone(),
            id: pane_id,
        }
    };

    app.tabs = vec![Tab {
        name: "[1]".to_string(),
        session_list_state: list_pane,
        layout,
        focused_pane: pane_id,
        project_id: Some(project_id),
        detail_split_offset: 0,
        layout_x_offset_adj: 0,
        session_list_width_pct: 0,
        descent_path: Vec::new(),
        bottom_focus_target: BottomZone::Off,
        mini_dag_focus: None,
    }];
    app.active_tab = 0;
    app.next_pane_id = 1;
    app
}

fn settings_fixture_app(fixture_now: chrono::DateTime<Utc>, fixture_theme: &str) -> App {
    let mut app = fixture_app(false, fixture_now, fixture_theme);
    // Epic M slice (c): "Activity indicator" moved from the old Display
    // category's last row (idx 7) to the Screen section's last row (idx 3);
    // same row, same rendered content, new address.
    app.settings_state.section = SettingsSection::Screen;
    app.settings_state.selected_index = 3;
    app.settings_state.focus = SettingsFocus::Items;
    app.tabs[0].layout = SplitNode::Leaf {
        pane: Pane::Settings,
        id: PaneId(0),
    };
    app
}

fn baseline_session(id: Uuid, project_id: Uuid, fixture_now: chrono::DateTime<Utc>) -> Session {
    Session {
        context_fill_pct: None,
        id,
        provider: SessionProvider::Codex,
        claude_session_id: None,
        query: "visual parity fixture".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        pending_question: None,
        pending_archive: false,
        working_dir: PathBuf::from("/home/jakedevar/rsi"),
        git_branch: Some("main".to_string()),
        status: SessionStatus::Running,
        project_id: Some(project_id),
        session_kind: SessionKind::Standard,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        created_at: fixture_now,
        updated_at: fixture_now,
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
        context_usage_confidence: ContextUsageConfidence::Full,
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

fn install_detail_events(state: &mut SessionState, now: chrono::DateTime<Utc>) {
    let session_id = state.session.id;
    let events = vec![
        message(
            session_id,
            1,
            Role::User,
            "Implement the context-window patch for the PCT denominator.\nThis session focuses on staging specific code changes and verifying the fix.",
            now - Duration::minutes(4),
        ),
        message(
            session_id,
            2,
            Role::Assistant,
            "I'll implement the context-window patch for the PCT denominator.\nLet me first inspect the current implementation.",
            now - Duration::minutes(4),
        ),
        tool(
            session_id,
            3,
            "Read",
            "src/metrics/pct.rs (120-220)",
            now - Duration::minutes(4),
        ),
        message(
            session_id,
            4,
            Role::Assistant,
            "I found the issue. The denominator should use max(context_window, tokens_used)\ninstead of just context_window. I'll make the change.",
            now - Duration::minutes(3),
        ),
        tool(
            session_id,
            5,
            "Write",
            "src/metrics/pct.rs (1 file changed)",
            now - Duration::minutes(3),
        ),
        message(
            session_id,
            6,
            Role::Assistant,
            "Now I'll run the tests to verify the fix.",
            now - Duration::minutes(2),
        ),
        tool(
            session_id,
            7,
            "Shell",
            "cargo test -p rsi-common pct::tests::test_denominator -- --nocapture",
            now - Duration::minutes(2),
        ),
        message(
            session_id,
            8,
            Role::Assistant,
            "The test is failing because we need to update the expected value in the test.\nLet me fix that.",
            now - Duration::minutes(1),
        ),
        tool(
            session_id,
            9,
            "Write",
            "src/metrics/pct.rs (1 file changed)",
            now - Duration::minutes(1),
        ),
    ];
    state.events = events;
    state.follow_tail = false;
    state.current_event_index = Some(7);
    state.scroll_offset = 0;
    state.events_generation = 1;
}

fn message(
    session_id: Uuid,
    sequence: i32,
    role: Role,
    content: &str,
    created_at: chrono::DateTime<Utc>,
) -> ConversationEvent {
    ConversationEvent {
        id: sequence as i64,
        session_id,
        sequence,
        event_type: EventType::Message,
        role: Some(role),
        content: content.to_string(),
        tool_name: None,
        tool_input: None,
        created_at,
        offload_id: None,
        tool_use_id: None,
        metadata: None,
    }
}

fn tool(
    session_id: Uuid,
    sequence: i32,
    tool_name: &str,
    content: &str,
    created_at: chrono::DateTime<Utc>,
) -> ConversationEvent {
    ConversationEvent {
        id: sequence as i64,
        session_id,
        sequence,
        event_type: EventType::ToolUse,
        role: None,
        content: content.to_string(),
        tool_name: Some(tool_name.to_string()),
        tool_input: None,
        created_at,
        offload_id: None,
        tool_use_id: None,
        metadata: None,
    }
}

fn stable_id(seed: &str) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_OID, seed.as_bytes())
}

fn buffer_to_svg(buffer: &Buffer) -> String {
    let cols = buffer.area.width;
    let rows = buffer.area.height;
    let width_px = CELL_WIDTH_PX * cols as f32;
    let height_px = CELL_HEIGHT_PX * rows as f32;
    let cell_w = CELL_WIDTH_PX;
    let cell_h = CELL_HEIGHT_PX;
    let font_size = cell_h * 0.78;
    let baseline = cell_h * 0.78;
    let mut svg = String::new();
    svg.push_str(&format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width_px}" height="{height_px}" viewBox="0 0 {width_px} {height_px}">
<rect width="100%" height="100%" fill="#08090e"/>
<g font-family="JetBrainsMono Nerd Font Mono, JetBrains Mono, monospace" font-size="{font_size:.2}" text-rendering="geometricPrecision">
"##
    ));

    for y in 0..rows {
        for x in 0..cols {
            let cell = &buffer[(x, y)];
            let px = x as f32 * cell_w;
            let py = y as f32 * cell_h;
            let bg = css_color(cell.bg, "#08090e");
            svg.push_str(&format!(
                r#"<rect x="{px:.2}" y="{py:.2}" width="{cell_w:.2}" height="{cell_h:.2}" fill="{bg}"/>"#
            ));
            let symbol = cell.symbol();
            if symbol.trim().is_empty() {
                continue;
            }
            let fg = css_color(cell.fg, "#e8e6dc");
            let weight = if cell.modifier.contains(Modifier::BOLD) {
                "700"
            } else {
                "400"
            };
            svg.push_str(&format!(
                r#"<text x="{:.2}" y="{:.2}" fill="{}" font-weight="{}">{}</text>"#,
                px,
                py + baseline,
                fg,
                weight,
                escape_xml(symbol)
            ));
        }
    }
    svg.push_str("</g>\n</svg>\n");
    svg
}

fn css_color(color: Color, reset: &str) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Black => "#000000".to_string(),
        Color::Red => "#ff0000".to_string(),
        Color::Green => "#00aa00".to_string(),
        Color::Yellow => "#ffff00".to_string(),
        Color::Blue => "#0000ff".to_string(),
        Color::Magenta => "#ff00ff".to_string(),
        Color::Cyan => "#00ffff".to_string(),
        Color::Gray => "#808080".to_string(),
        Color::DarkGray => "#404040".to_string(),
        Color::LightRed => "#ff5555".to_string(),
        Color::LightGreen => "#55ff55".to_string(),
        Color::LightYellow => "#ffff55".to_string(),
        Color::LightBlue => "#5555ff".to_string(),
        Color::LightMagenta => "#ff55ff".to_string(),
        Color::LightCyan => "#55ffff".to_string(),
        Color::White => "#ffffff".to_string(),
        Color::Indexed(_) | Color::Reset => reset.to_string(),
    }
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
