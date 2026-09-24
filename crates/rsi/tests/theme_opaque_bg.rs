//! T1.1 regression probe: opaque themes must paint full backgrounds.
//!
//! Renders real frames (TestBackend) under `gruvbox-warm` with DEFAULT
//! settings (text-area backfill toggle OFF) and asserts that no buffer cell
//! carries a `Color::Reset` background. Pre-T1.1, `text_area_bg_color()`
//! returned `Reset` whenever the backfill toggle was OFF, so the session
//! list surface, detail surface, message bubbles, input bar, and bottom
//! strip all punched translucent holes over the opaque root fill when the
//! terminal window itself is translucent.
//!
//! This lives in its own integration binary (own process): it mutates the
//! global theme atomic, which must not race `tui_integration.rs`'s
//! default-theme color assertions. Tests serialize on `PROBE_THEME_LOCK`, so
//! opaque and terminal-default probes cannot flip the global mid-render.
//! Legacy Transparent behavior is locked separately by
//! `theme::tests::transparent_theme_baseline_is_byte_identical_to_pre_tier_migration`
//! and the backfill-OFF gate assertions in
//! `theme::tests::all_themes_have_valid_tiers_and_readable_chrome`.
//!
//! Fixture note: unlike `tui_integration.rs::test_app`, this fixture does
//! NOT call `PersistedState::default().save()` (the ticketed `~/.rsi`
//! test-pollution gap — see
//! thoughts/shared/tickets/tui-redesign-2026-07-07/test-isolation-data-path.md);
//! it overrides `app.settings`, tabs, and theme in-memory for determinism.

// Shared test-support module; this binary uses only a subset of its helpers.
#[allow(dead_code)]
mod support;

use std::path::PathBuf;
use std::sync::Mutex;

use ratatui::style::Color;
use rsi::app::App;
use rsi::client::DaemonClient;
use rsi::types::{FileViewerState, OverlayState, Pane, PaneId, SessionState, SplitNode, Tab};
use rsi_common::types::{ConversationEvent, EventType, Role};

static PROBE_THEME_LOCK: Mutex<()> = Mutex::new(());

fn probe_theme_guard() -> std::sync::MutexGuard<'static, ()> {
    PROBE_THEME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn probe_session(i: usize) -> rsi_common::types::Session {
    rsi_common::types::Session {
        context_fill_pct: None,
        id: uuid::Uuid::new_v4(),
        provider: rsi_common::types::SessionProvider::Claude,
        claude_session_id: None,
        query: format!("query {}", i),
        title: Some(format!("probe session {}", i)),
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
    }
}

/// Deterministic app under `theme_key` with DEFAULT settings (backfill toggle
/// OFF, so theme policy is the sole source of passive frame backgrounds).
fn probe_app(session_count: usize, theme_key: &str) -> App {
    let client = DaemonClient::new(PathBuf::from("/tmp/nonexistent.sock"));
    let mut app = App::new(client);

    // In-memory determinism overrides (App::new may have loaded live user
    // state, including a persisted theme/settings — override everything the
    // probe depends on rather than writing sanitized state to disk).
    app.settings = rsi::settings::UserSettings::default();
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

    for i in 0..session_count {
        let s = probe_session(i);
        app.session_order.push(s.id);
        app.sessions.insert(s.id, SessionState::new(s));
    }
    app.set_project_filter(None);

    assert!(
        rsi::ui::theme::set_theme_by_name(theme_key),
        "{theme_key} must be a valid theme key"
    );
    app
}

fn push_event(
    state: &mut SessionState,
    sequence: i32,
    event_type: EventType,
    role: Option<Role>,
    content: &str,
    tool_name: Option<&str>,
) {
    let session_id = state.session.id;
    state.events.push(ConversationEvent {
        id: 0,
        session_id,
        sequence,
        event_type,
        role,
        created_at: chrono::Utc::now(),
        content: content.to_string(),
        tool_name: tool_name.map(str::to_string),
        tool_input: None,
        offload_id: None,
        tool_use_id: None,
        metadata: None,
    });
    state.events_generation += 1;
}

/// True when `(x, y)` is the shadowed continuation cell of a double-width
/// glyph: ratatui `reset()`s the cell after placing a wide symbol, but the
/// terminal never draws it — the wide glyph covers both columns using the
/// FIRST cell's style — so its Reset bg is a buffer-internal artifact, not
/// visible bleed (e.g. a double-width status glyph in the top chrome).
fn is_wide_glyph_shadow(buffer: &ratatui::buffer::Buffer, x: u16, y: u16) -> bool {
    use unicode_width::UnicodeWidthStr;
    x > buffer.area.x && UnicodeWidthStr::width(buffer[(x - 1, y)].symbol()) == 2
}

fn assert_no_reset_bg(buffer: &ratatui::buffer::Buffer, frame_name: &str) {
    let reset_cells: Vec<(u16, u16)> = support::find_cells_with_bg(buffer, Color::Reset)
        .into_iter()
        .filter(|&(x, y)| !is_wide_glyph_shadow(buffer, x, y))
        .collect();
    assert!(
        reset_cells.is_empty(),
        "{frame_name}: opaque gruvbox-warm frame must not paint any \
         Color::Reset background cell (terminal bleed-through under a \
         translucent terminal); found {} cells, first: {:?}",
        reset_cells.len(),
        &reset_cells[..reset_cells.len().min(12)]
    );
}

fn find_text(buffer: &ratatui::buffer::Buffer, needle: &str) -> Option<(u16, u16)> {
    let area = buffer.area;
    for y in area.y..area.y + area.height {
        let line = support::line_text(buffer, y);
        if let Some(byte_index) = line.find(needle) {
            let x = area.x + line[..byte_index].chars().count() as u16;
            return Some((x, y));
        }
    }
    None
}

fn assert_text_background(
    buffer: &ratatui::buffer::Buffer,
    needle: &str,
    expected: Color,
    region_name: &str,
) {
    let (x, y) = find_text(buffer, needle)
        .unwrap_or_else(|| panic!("{region_name}: expected text {needle:?} in rendered frame"));
    for offset in 0..needle.chars().count() as u16 {
        assert_eq!(
            buffer[(x + offset, y)].style().bg,
            Some(expected),
            "{region_name}: {needle:?} cell {offset} has wrong background"
        );
    }
}

fn assert_substantial_terminal_default_area(buffer: &ratatui::buffer::Buffer, frame_name: &str) {
    let reset_count = support::find_cells_with_bg(buffer, Color::Reset)
        .into_iter()
        .filter(|&(x, y)| !is_wide_glyph_shadow(buffer, x, y))
        .count();
    let cell_count = buffer.area.width as usize * buffer.area.height as usize;
    assert!(
        reset_count * 2 > cell_count,
        "{frame_name}: expected a majority terminal-default frame, got \
         {reset_count}/{cell_count} Reset cells"
    );
}

fn assert_visible_active_accents(buffer: &ratatui::buffer::Buffer, frame_name: &str) {
    let primary = rsi::ui::theme::focused_border();
    let has_primary = buffer
        .content
        .iter()
        .any(|cell| cell.style().fg == Some(primary) && !cell.symbol().trim().is_empty());
    assert!(
        has_primary,
        "{frame_name}: terminal-default frame must retain visible primary accents"
    );
}

#[test]
fn gruvbox_warm_session_list_frame_paints_no_reset_bg() {
    // Poison-tolerant: a sibling's assertion failure must not mask this
    // test's own result (every test sets the same theme, so the recovered
    // guard is still sound).
    let _guard = probe_theme_guard();
    let mut app = probe_app(3, "gruvbox-warm");

    let buffer = support::render_app(&mut app, 140, 45);

    assert_no_reset_bg(&buffer, "session list");
    // Representative surface sanity: the list surface fill (tier_panel via
    // list_surface_bg -> glass_panel_bg fallback) is actually present.
    let panel_cells = support::find_cells_with_bg(&buffer, rsi::ui::theme::tier_panel());
    assert!(
        !panel_cells.is_empty(),
        "session list: expected the list surface to be filled with the \
         theme's tier_panel background"
    );
}

#[test]
fn gruvbox_warm_session_detail_frame_paints_no_reset_bg() {
    // Poison-tolerant: a sibling's assertion failure must not mask this
    // test's own result (every test sets the same theme, so the recovered
    // guard is still sound).
    let _guard = probe_theme_guard();
    let mut app = probe_app(1, "gruvbox-warm");
    let session_id = app.session_order[0];

    {
        let state = app
            .sessions
            .get_mut(&session_id)
            .expect("probe session should exist");
        push_event(
            state,
            1,
            EventType::Message,
            Some(Role::User),
            "please probe the background fill",
            None,
        );
        push_event(
            state,
            2,
            EventType::ToolUse,
            Some(Role::Assistant),
            "{\"command\":\"rg Reset\"}",
            Some("Bash"),
        );
        push_event(
            state,
            3,
            EventType::Message,
            Some(Role::Assistant),
            "assistant reply long enough to wrap across the borderless \
             plain-assistant render path and exercise the detail bg fill",
            None,
        );
    }

    let pane_id = PaneId(0);
    app.tabs[0].layout = SplitNode::Leaf {
        pane: Pane::SessionDetail { session_id },
        id: pane_id,
    };
    app.tabs[0].focused_pane = pane_id;

    let buffer = support::render_app(&mut app, 140, 45);

    assert_no_reset_bg(&buffer, "session detail");
    // Representative surface sanity: bubble/input-bar/bottom-strip surfaces
    // (surface0 == tier_panel for gruvbox-warm) are actually present.
    let panel_cells = support::find_cells_with_bg(&buffer, rsi::ui::theme::tier_panel());
    assert!(
        !panel_cells.is_empty(),
        "session detail: expected bubbles/input bar/bottom strip to be \
         filled with the theme's panel-tier background"
    );
}

/// T5 guardrail: at a total width where the default (37%) sidebar leaves a
/// detail content width that clears `DETAIL_CENTERING_ENABLE_WIDTH` (110),
/// the newly-created side gutters must be painted with the theme's detail
/// surface (via `paint_detail_gutters`/`detail_surface_bg`), never left as
/// `Color::Reset` terminal bleed-through. The existing 140-wide test above is
/// confirmed unaffected (detail width ~88, below the enable threshold) — this
/// test at 220 total cols (default sidebar ~81 cols, detail width ~138) is
/// what actually exercises the new gutter-paint code path end to end.
#[test]
fn gruvbox_warm_session_detail_centered_column_paints_no_reset_bg() {
    // Poison-tolerant: a sibling's assertion failure must not mask this
    // test's own result (every test sets the same theme, so the recovered
    // guard is still sound).
    let _guard = probe_theme_guard();
    let mut app = probe_app(1, "gruvbox-warm");
    let session_id = app.session_order[0];

    {
        let state = app
            .sessions
            .get_mut(&session_id)
            .expect("probe session should exist");
        push_event(
            state,
            1,
            EventType::Message,
            Some(Role::User),
            "please probe the centered-column gutter fill",
            None,
        );
        push_event(
            state,
            2,
            EventType::Message,
            Some(Role::Assistant),
            "assistant reply long enough to wrap across the centered column \
             and exercise both the content fill and the side-gutter fill",
            None,
        );
    }

    let pane_id = PaneId(0);
    app.tabs[0].layout = SplitNode::Leaf {
        pane: Pane::SessionDetail { session_id },
        id: pane_id,
    };
    app.tabs[0].focused_pane = pane_id;

    let buffer = support::render_app(&mut app, 220, 45);

    assert_no_reset_bg(&buffer, "session detail centered column");
    // Representative surface sanity: the side gutters (and the centered
    // content itself) are filled with the theme's panel-tier background —
    // the same source `render_session_detail` already fills its own content
    // with (detail_surface_bg), so gutters read as a continuation of the
    // panel surface rather than a new, distinct void color.
    let panel_cells = support::find_cells_with_bg(&buffer, rsi::ui::theme::tier_panel());
    assert!(
        !panel_cells.is_empty(),
        "session detail (centered): expected the centered column and its \
         side gutters to be filled with the theme's panel-tier background"
    );
}

#[test]
fn gruvbox_warm_file_viewer_paints_no_reset_bg() {
    let _guard = probe_theme_guard();
    let mut app = probe_app(1, "gruvbox-warm");
    let session_id = app.session_order[0];

    app.sessions
        .get_mut(&session_id)
        .expect("probe session should exist")
        .file_viewer = Some(FileViewerState::new(
        PathBuf::from("/tmp/rsi-theme-probe.md"),
        "# Theme probe\n\nFile viewer background must remain opaque.".to_owned(),
    ));

    let pane_id = PaneId(0);
    app.tabs[0].layout = SplitNode::Leaf {
        pane: Pane::SessionDetail { session_id },
        id: pane_id,
    };
    app.tabs[0].focused_pane = pane_id;

    let buffer = support::render_app(&mut app, 220, 45);

    assert_no_reset_bg(&buffer, "file viewer");
}

#[test]
fn truly_transparent_session_list_yields_passive_rows_to_terminal() {
    let _guard = probe_theme_guard();
    let mut app = probe_app(3, "truly-transparent");

    let buffer = support::render_app(&mut app, 140, 45);

    assert_substantial_terminal_default_area(&buffer, "transparent session list");
    assert_text_background(
        &buffer,
        "probe session 1",
        Color::Reset,
        "transparent unselected session row",
    );
    assert_visible_active_accents(&buffer, "transparent session list");
    assert!(
        !support::find_cells_with_bg(&buffer, rsi::ui::theme::tier_selected()).is_empty(),
        "transparent session list must retain a visible selected-row fill"
    );
}

#[test]
fn truly_transparent_detail_clears_code_bubble_and_composer_mode_backgrounds() {
    let _guard = probe_theme_guard();
    let mut app = probe_app(1, "truly-transparent");
    let session_id = app.session_order[0];

    {
        let state = app
            .sessions
            .get_mut(&session_id)
            .expect("probe session should exist");
        push_event(
            state,
            1,
            EventType::Message,
            Some(Role::Assistant),
            "```rust\n    let gem = \"diamond\";\n```",
            None,
        );
        push_event(
            state,
            2,
            EventType::Message,
            Some(Role::Assistant),
            "cursor target after the code block",
            None,
        );
    }

    let pane_id = PaneId(0);
    app.tabs[0].layout = SplitNode::Leaf {
        pane: Pane::SessionDetail { session_id },
        id: pane_id,
    };
    app.tabs[0].focused_pane = pane_id;

    let buffer = support::render_app(&mut app, 140, 45);

    assert_substantial_terminal_default_area(&buffer, "transparent session detail");
    assert_text_background(
        &buffer,
        "let gem = \"diamond\";",
        Color::Reset,
        "transparent syntax line",
    );
    let (code_x, code_y) = find_text(&buffer, "let gem")
        .expect("transparent detail should render the indented syntax line");
    for x in code_x.saturating_sub(4)..code_x {
        assert_eq!(
            buffer[(x, code_y)].style().bg,
            Some(Color::Reset),
            "transparent code indentation should not paint a code-block rectangle"
        );
    }
    assert_text_background(&buffer, "NORMAL", Color::Reset, "transparent composer mode");
    assert_visible_active_accents(&buffer, "transparent session detail");
}

#[test]
fn truly_transparent_settings_root_uses_terminal_default() {
    let _guard = probe_theme_guard();
    let mut app = probe_app(0, "truly-transparent");
    let pane_id = PaneId(0);
    app.tabs[0].layout = SplitNode::Leaf {
        pane: Pane::Settings,
        id: pane_id,
    };
    app.tabs[0].focused_pane = pane_id;

    let buffer = support::render_app(&mut app, 140, 45);

    assert_substantial_terminal_default_area(&buffer, "transparent settings");
    assert_text_background(
        &buffer,
        "Settings",
        Color::Reset,
        "transparent settings title",
    );
    assert_visible_active_accents(&buffer, "transparent settings");
}

#[test]
fn truly_transparent_theme_picker_keeps_only_active_row_filled() {
    let _guard = probe_theme_guard();
    let mut app = probe_app(2, "truly-transparent");
    app.overlay = OverlayState::ThemePicker {
        selected_index: 0,
        original_index: rsi::ui::theme::active_theme_index(),
    };

    let buffer = support::render_app(&mut app, 90, 24);

    assert_substantial_terminal_default_area(&buffer, "transparent theme picker");
    assert_text_background(
        &buffer,
        "Junk Yard",
        Color::Reset,
        "transparent unselected picker row",
    );
    assert_text_background(
        &buffer,
        "Goth",
        rsi::ui::theme::surface2(),
        "transparent selected picker row",
    );
    assert_visible_active_accents(&buffer, "transparent theme picker");
}
