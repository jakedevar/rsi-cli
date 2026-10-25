//! Render tests for the global dashboard (#3317): a fixture snapshot draws
//! its rows in order with the right numbers, the TOTAL row, the window and the
//! `incomplete` marker, and the pane shows the top-level manager's
//! conversation or the start prompt.

use super::*;
use crate::app::App;
use crate::app::app_test_helpers::baseline_session;
use crate::overlay::global_dashboard::tests::{app_with, fixture, loaded};
use crate::overlay::global_dashboard::{DashboardFocus, TopSeat};
use crate::types::SessionState;
use chrono::Utc;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rsi_common::global_dashboard::DashboardWindow;
use rsi_common::types::{ConversationEvent, EventType, Role, SessionKind, SessionStatus};
use uuid::Uuid;

/// Each line of the buffer, right-trimmed.
fn screen_lines(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let _theme = crate::ui::theme::pin_theme_state();
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| crate::ui::overlay::render_overlay(frame, frame.area(), app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn screen(app: &mut App, width: u16, height: u16) -> String {
    screen_lines(app, width, height).join("\n")
}

fn line_with<'a>(lines: &'a [String], needle: &str) -> &'a str {
    lines
        .iter()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no line contains {needle:?}:\n{}", lines.join("\n")))
}

fn position(lines: &[String], needle: &str) -> usize {
    lines
        .iter()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no line contains {needle:?}:\n{}", lines.join("\n")))
}

fn message(session_id: Uuid, sequence: i32, role: Role, content: &str) -> ConversationEvent {
    ConversationEvent {
        id: i64::from(sequence),
        session_id,
        sequence,
        event_type: EventType::Message,
        role: Some(role),
        content: content.into(),
        created_at: Utc::now(),
        tool_name: None,
        tool_input: None,
        offload_id: None,
        tool_use_id: None,
        metadata: None,
    }
}

fn seat_session(id: Uuid) -> SessionState {
    let mut session = baseline_session(id, SessionKind::Standard);
    session.title = Some("Global manager".into());
    session.status = SessionStatus::Completed;
    let mut state = SessionState::new(session);
    state.events = vec![
        message(id, 1, Role::User, "Give me a portfolio digest."),
        message(
            id,
            2,
            Role::Assistant,
            "rsi is healthy; dictate-agent has two red QA Issues.",
        ),
    ];
    state.last_sequence = Some(2);
    state
}

#[test]
fn rows_render_in_order_with_the_right_numbers_and_a_total_row() {
    let f = fixture();
    let mut app = app_with(&f, loaded(f.snapshot.clone()));
    let lines = screen_lines(&mut app, 200, 40);

    let header = line_with(&lines, "RSI / Dashboard");
    assert!(header.contains("24h window"), "{header}");
    assert!(header.contains("as of "), "{header}");
    assert!(header.contains(" ago"), "{header}");
    assert!(header.contains("sort agents"), "{header}");

    let columns = line_with(&lines, "PROJECT");
    for head in [
        "PM",
        "AGT",
        "MGR",
        "LEAD",
        "WRK",
        "REV",
        "PROVIDERS",
        "TOK IN",
        "TOK OUT",
    ] {
        assert!(columns.contains(head), "{head}: {columns}");
    }
    for head in [
        "METERED", "OPEN", "WIP", "NEW24", "DON24", "REQ", "LAND", "QA",
    ] {
        assert!(columns.contains(head), "{head}: {columns}");
    }

    // Home first, then by live agents, then the TOTAL row.
    let home = position(&lines, "rsi (home)");
    let busy = position(&lines, "dictate-agent ");
    let idle = position(&lines, "notes ");
    let total = position(&lines, "TOTAL");
    assert!(
        home < busy && busy < idle && idle < total,
        "{home} {busy} {idle} {total}"
    );

    let home_row = &lines[home];
    for cell in ["Claude 4 · Codex 2", "1.2M", "80k", "$3.20", "3h"] {
        assert!(home_row.contains(cell), "{cell}: {home_row}");
    }
    // The agent total and its role split sit in order on the row.
    let tokens: Vec<&str> = home_row.split_whitespace().collect();
    let agt = tokens.iter().position(|t| *t == "●").map_or(0, |i| i + 1);
    assert_eq!(
        &tokens[agt..agt + 5],
        ["6", "1", "1", "3", "1"],
        "{home_row}"
    );

    let busy_row = &lines[busy];
    for cell in ["Claude 9", "900k", "40k", "$11", "2 red"] {
        assert!(busy_row.contains(cell), "{cell}: {busy_row}");
    }
    let total_row = &lines[total];
    for cell in ["16", "2.1M", "120k", "$14", "2 red"] {
        assert!(total_row.contains(cell), "{cell}: {total_row}");
    }
    // The last row inside the frame names the keys.
    let footer = &lines[lines.len() - 2];
    for hint in ["j/k move", "w window", "s sort", "t pane", "Esc close"] {
        assert!(footer.contains(hint), "{hint}: {footer}");
    }
}

#[test]
fn the_selected_project_gets_a_detail_band() {
    let f = fixture();
    let mut app = app_with(&f, loaded(f.snapshot.clone()));
    let text = screen(&mut app, 200, 40);
    for expected in [
        "rsi · home project · PM seat Running",
        "Agents 6 live · 1 mgr · 1 lead · 3 wrk · 1 rev",
        "Claude claude-opus-5-5 xhigh ×1",
        "Usage 24h: Claude claude-opus-5-5 1.2M in 80k out $3.20",
        "12 open · 3 in progress · 5 created · 7 closed in 24h",
        "landings 4 published, 1 refused",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
}

#[test]
fn the_window_and_the_incomplete_marker_show_in_the_header() {
    let f = fixture();
    let mut snapshot = f.snapshot.clone();
    snapshot.window = "7d".into();
    snapshot.window_seconds = 7 * 86400;
    snapshot.totals.incomplete = true;
    snapshot.incomplete_reasons = vec!["sessions".into()];
    let mut state = loaded(snapshot);
    state.window = DashboardWindow::Week;
    let mut app = app_with(&f, state);
    let lines = screen_lines(&mut app, 200, 40);
    let header = line_with(&lines, "RSI / Dashboard");
    assert!(header.contains("7d window"), "{header}");
    assert!(header.contains("incomplete"), "{header}");
    let columns = line_with(&lines, "PROJECT");
    assert!(
        columns.contains("NEW7D") && columns.contains("DON7D"),
        "{columns}"
    );
}

#[test]
fn a_complete_snapshot_draws_without_the_marker_and_a_failed_refresh_says_stale() {
    let f = fixture();
    let mut state = loaded(f.snapshot.clone());
    state.error = Some("Refresh failed: boom (showing the last snapshot); press r to retry".into());
    let mut app = app_with(&f, state);
    let lines = screen_lines(&mut app, 200, 40);
    let header = line_with(&lines, "RSI / Dashboard");
    assert!(header.contains("stale"), "{header}");
    line_with(&lines, "Refresh failed: boom");
}

#[test]
fn the_pane_shows_the_start_prompt_when_no_top_level_seat_is_live() {
    let f = fixture();
    let mut state = loaded(f.snapshot.clone());
    state.seat = TopSeat::Absent;
    let mut app = app_with(&f, state);
    // Wide terminal: the pane is on by default, beside the table.
    let text = screen(&mut app, 200, 40);
    assert!(text.contains("No top-level manager is live."), "{text}");
    assert!(text.contains("Press n to start one"), "{text}");
    assert!(
        text.contains("rsi (home)"),
        "the table stays beside the pane"
    );
}

#[test]
fn the_pane_is_hidden_by_default_on_a_narrow_terminal_and_t_shows_it() {
    let f = fixture();
    let mut state = loaded(f.snapshot.clone());
    state.seat = TopSeat::Absent;
    let mut app = app_with(&f, state);
    let narrow = screen(&mut app, 120, 40);
    assert!(narrow.contains("rsi (home)"), "{narrow}");
    assert!(narrow.contains("PROJECT"), "{narrow}");
    if let OverlayState::GlobalDashboard(state) = &mut app.overlay {
        state.pane_override = Some(true);
    }
    // Narrow with the pane on: the pane takes the body.
    let shown = screen(&mut app, 120, 40);
    assert!(shown.contains("No top-level manager is live."), "{shown}");
}

#[test]
fn the_pane_renders_the_live_seats_conversation() {
    let f = fixture();
    let seat_id = Uuid::new_v4();
    let mut state = loaded(f.snapshot.clone());
    state.seat = TopSeat::Seat(seat_id);
    let mut app = app_with(&f, state);
    app.sessions.insert(seat_id, seat_session(seat_id));
    let text = screen(&mut app, 200, 40);
    assert!(text.contains("Top-level manager · Completed"), "{text}");
    assert!(text.contains("Give me a portfolio digest."), "{text}");
    assert!(
        text.contains("dictate-agent has two red QA Issues."),
        "{text}"
    );
    // Typing hints replace the table hints while the input bar has focus.
    if let OverlayState::GlobalDashboard(state) = &mut app.overlay {
        state.focus = DashboardFocus::Input;
    }
    let typing = screen_lines(&mut app, 200, 40);
    assert!(
        typing[typing.len() - 2].contains("Enter send"),
        "{typing:?}"
    );
}

#[test]
fn a_loading_dashboard_and_tiny_terminals_draw_without_panicking() {
    let f = fixture();
    let mut app = app_with(&f, GlobalDashboardState::default());
    let loading = screen(&mut app, 100, 30);
    assert!(loading.contains("Loading the dashboard"), "{loading}");
    let mut app = app_with(&f, loaded(f.snapshot.clone()));
    for (width, height) in [(60, 12), (40, 6), (24, 3), (80, 24), (130, 10)] {
        let text = screen(&mut app, width, height);
        assert!(!text.is_empty());
    }
    let compact = screen(&mut app, 80, 24);
    assert!(
        compact.contains("AGT") && compact.contains("METERED"),
        "{compact}"
    );
}

#[test]
fn narrow_tables_keep_the_project_agents_cost_and_issue_columns() {
    let f = fixture();
    let mut app = app_with(&f, loaded(f.snapshot.clone()));
    let lines = screen_lines(&mut app, 70, 30);
    let columns = line_with(&lines, "PROJECT");
    for head in ["AGT", "METERED", "OPEN"] {
        assert!(columns.contains(head), "{head}: {columns}");
    }
    line_with(&lines, "dictate-agent");
}
