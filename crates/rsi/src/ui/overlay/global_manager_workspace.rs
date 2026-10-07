//! Global manager workspace (#1213, #1231): render the full-screen operator
//! console. The seat list sits on the left and the selected seat's live
//! conversation on the right, drawn by the shared session-detail renderer
//! (`ui::prepare_session_detail` / `ui::render_session_conversation`). Below
//! `SPLIT_MIN_WIDTH` the two take turns: the list while it has focus, the
//! conversation while it or its input bar does.

use crate::app::App;
use crate::overlay::global_manager_workspace::{
    APPOINT_HINT, GlobalManagerWorkspaceState, LAUNCH_HINT, SPLIT_MIN_WIDTH, SeatEntry, SeatHealth,
    WorkspaceFocus, seat_header, seat_health, seat_row_text, seat_summary,
};
use crate::overlay::global_manager_workspace_launch::{LaunchField, LaunchForm, LaunchRole};
use crate::types::OverlayState;
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph, Wrap};
use uuid::Uuid;

fn safe(value: &str) -> String {
    value.chars().filter(|ch| !ch.is_control()).collect()
}

fn health_color(health: SeatHealth) -> Color {
    match health {
        SeatHealth::Revoked | SeatHealth::Missing => theme::error_status(),
        SeatHealth::Paused => theme::warning_status(),
        SeatHealth::Waiting => theme::status_waiting(),
        SeatHealth::Stopped => theme::status_failed(),
        SeatHealth::Active => theme::status_running(),
        SeatHealth::Idle => theme::status_completed(),
    }
}

/// Where each part of the console goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConsoleAreas {
    pub preamble: Rect,
    /// The seat list, when shown.
    pub seats: Option<Rect>,
    /// The vertical rule between the halves (split layout only).
    pub rule: Option<Rect>,
    /// The conversation or launch form, when shown.
    pub detail: Option<Rect>,
    /// The error/notice line, only while there is one.
    pub status: Option<Rect>,
}

/// The seat list's width in the split layout.
#[must_use]
pub(crate) fn seat_list_width(inner_width: u16) -> u16 {
    (inner_width * 43 / 100).clamp(46, 96)
}

pub(crate) fn console_areas(
    inner: Rect,
    state: &GlobalManagerWorkspaceState,
    preamble_height: u16,
) -> ConsoleAreas {
    let has_status = state.error.is_some() || state.notice.is_some();
    let preamble_height = preamble_height.min(inner.height);
    let status_height = u16::from(has_status && inner.height > preamble_height + 1);
    let body = Rect::new(
        inner.x,
        inner.y + preamble_height,
        inner.width,
        inner.height.saturating_sub(preamble_height + status_height),
    );
    let status = (status_height == 1)
        .then(|| Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1));
    let preamble = Rect::new(inner.x, inner.y, inner.width, preamble_height);
    // Without a grant (or a node snapshot) there are no seats: the empty
    // state (or the launch form) takes the whole body.
    let no_grant = state.snapshot.is_some() && !state.has_seats();
    if no_grant || state.snapshot.is_none() {
        let form = state.launch.is_some();
        return ConsoleAreas {
            preamble,
            seats: (!form).then_some(body),
            rule: None,
            detail: form.then_some(body),
            status,
        };
    }
    if inner.width >= SPLIT_MIN_WIDTH {
        let list = seat_list_width(inner.width);
        let seats = Rect::new(body.x, body.y, list, body.height);
        let rule = Rect::new(body.x + list, body.y, 1, body.height);
        let detail = Rect::new(
            body.x + list + 2,
            body.y,
            body.width.saturating_sub(list + 2),
            body.height,
        );
        return ConsoleAreas {
            preamble,
            seats: Some(seats),
            rule: Some(rule),
            detail: Some(detail),
            status,
        };
    }
    let detail_first = state.launch.is_some() || state.focus != WorkspaceFocus::Seats;
    ConsoleAreas {
        preamble,
        seats: (!detail_first).then_some(body),
        rule: None,
        detail: detail_first.then_some(body),
        status,
    }
}

/// The fixed lines above both halves.
fn preamble(app: &App, state: &GlobalManagerWorkspaceState) -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut lines = Vec::new();
    let freshness = match (&state.loaded_at, &state.error) {
        (_, Some(_)) if state.snapshot.is_some() => "stale".to_string(),
        (Some(at), _) => format!("as of {}", at.format("%H:%M:%S")),
        (None, _) => "loading".to_string(),
    };
    if state.target.is_some() {
        // #1240: a node console.
        lines.push(Line::from(vec![
            Span::styled(
                state
                    .node
                    .as_ref()
                    .map_or_else(|| "Manager node".to_string(), |node| safe(&node.label)),
                bold,
            ),
            Span::styled(
                format!(" · {freshness} · Enter opens a child · Backspace goes up · ? keys"),
                Style::default().fg(theme::dim_metadata()),
            ),
        ]));
        for line in state.node_lines(&app.projects) {
            lines.push(Line::from(Span::styled(
                safe(&line),
                Style::default().fg(theme::text()),
            )));
        }
        if state
            .node
            .as_ref()
            .is_some_and(|node| node.state == "revoked")
        {
            lines.push(Line::from(Span::styled(
                "Revoked: this node holds no authority.".to_string(),
                Style::default().fg(theme::error_status()),
            )));
        }
        return lines;
    }
    lines.push(Line::from(vec![
        Span::styled("Above all projects", bold),
        Span::styled(
            format!(" · same view from every tab · {freshness} · ? keys"),
            Style::default().fg(theme::dim_metadata()),
        ),
    ]));
    if let Some(snapshot) = &state.snapshot
        && let Some(grant) = &snapshot.grant
    {
        lines.push(Line::from(Span::styled(
            safe(&state.grant_line(&app.projects)),
            Style::default().fg(theme::text()),
        )));
        if seat_health(grant, snapshot.seat.as_ref()) == SeatHealth::Revoked {
            lines.push(Line::from(Span::styled(
                format!("Revoked: the seat holds no authority. Appoint a new one: {LAUNCH_HINT}"),
                Style::default().fg(theme::error_status()),
            )));
        }
    }
    lines
}

/// The empty state (no grant): how to appoint the first manager.
fn empty_lines() -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let text = Style::default().fg(theme::text());
    vec![
        Line::from(Span::styled("No global manager is appointed.", bold)),
        Line::default(),
        Line::from(Span::styled(
            "Press n to launch and appoint one here: pick its provider, model, effort",
            text,
        )),
        Line::from(Span::styled(
            "and the projects it oversees, then talk to it in this view.",
            text,
        )),
        Line::default(),
        Line::from(Span::styled(
            format!("To appoint an existing session instead, focus it and run {APPOINT_HINT}."),
            Style::default().fg(theme::dim_metadata()),
        )),
    ]
}

/// Every line of the seat list and the line index of the selected row.
pub(crate) fn seat_lines(
    state: &GlobalManagerWorkspaceState,
    width: usize,
    focused: bool,
) -> (Vec<Line<'static>>, Option<usize>) {
    if state.snapshot.is_none() {
        return (Vec::new(), None);
    }
    if !state.has_seats() {
        return (empty_lines(), None);
    }
    let mut lines = vec![Line::from(Span::styled(
        seat_header(width),
        Style::default()
            .fg(theme::table_header_text())
            .add_modifier(Modifier::BOLD),
    ))];
    let mut selected_line = None;
    for (index, seat) in state.seats().iter().enumerate() {
        let selected = index == state.selected;
        let marker = if selected { "> " } else { "  " };
        let style = if selected && focused {
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::REVERSED)
        } else if selected {
            Style::default()
                .fg(health_color(seat.health))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(health_color(seat.health))
        };
        if selected {
            selected_line = Some(lines.len());
        }
        lines.push(Line::from(Span::styled(
            format!("{marker}{}", safe(&seat_row_text(seat, width))),
            style,
        )));
    }
    (lines, selected_line)
}

fn render_lines(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    selected_line: Option<usize>,
) {
    let height = usize::from(area.height);
    let offset = selected_line.map_or(0, |line| line.saturating_sub(height.saturating_sub(1)));
    let out: Vec<Line> = lines.into_iter().skip(offset).take(height).collect();
    frame.render_widget(Paragraph::new(out), area);
}

/// The launch form's lines.
pub(crate) fn form_lines(form: &LaunchForm, width: usize) -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let text = Style::default().fg(theme::text());
    let dim = Style::default().fg(theme::dim_metadata());
    let mut lines = vec![
        Line::from(Span::styled("Instantiate a manager", bold)),
        Line::from(Span::styled(
            if form.cap_confirm {
                "y confirm lower project caps · n back to the form · Esc cancel"
            } else {
                "j/k field · h/l change · Space/a scope · Enter launch · Esc cancel"
            },
            dim,
        )),
        Line::default(),
    ];
    for field in form.fields() {
        let focused = field == form.field;
        let marker = if focused { "> " } else { "  " };
        let style = if focused {
            text.add_modifier(Modifier::REVERSED)
        } else {
            text
        };
        match field {
            LaunchField::Submit => {
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    format!("{marker}{}", form.value(field)),
                    if focused { style } else { bold },
                )));
            }
            LaunchField::Prompt => {
                lines.push(Line::from(Span::styled(
                    format!("{marker}{:<9}", field.label()),
                    style,
                )));
                let cursor = if focused { "▏" } else { "" };
                let body = format!("{}{cursor}", safe(&form.prompt));
                for row in wrap_words(&body, width.saturating_sub(4).max(10)) {
                    lines.push(Line::from(Span::styled(format!("    {row}"), text)));
                }
            }
            LaunchField::Scope => {
                lines.push(Line::from(Span::styled(
                    format!("{marker}{:<9} {}", field.label(), form.value(field)),
                    style,
                )));
                for (index, entry) in form.scope.iter().enumerate() {
                    let cursor = focused && index == form.scope_cursor;
                    let check = if entry.checked { "[x]" } else { "[ ]" };
                    let line_style = if cursor {
                        text.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                    } else {
                        text
                    };
                    lines.push(Line::from(Span::styled(
                        format!("    {check} {}", safe(&entry.name)),
                        line_style,
                    )));
                }
            }
            _ => {
                lines.push(Line::from(Span::styled(
                    format!(
                        "{marker}{:<9} < {} >",
                        field.label(),
                        safe(&form.value(field))
                    ),
                    style,
                )));
            }
        }
    }
    if let LaunchRole::Project(_) = form.role {
        lines.push(Line::from(Span::styled(
            "  Scope: the whole project, Execute policy (change later with :manager policy)",
            dim,
        )));
    }
    lines.push(Line::from(Span::styled(
        format!(
            "  Launch catalog: {} allowed choice{}",
            form.catalog.len(),
            if form.catalog.len() == 1 { "" } else { "s" }
        ),
        dim,
    )));
    if let Some(error) = &form.error {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            safe(error),
            Style::default().fg(theme::error_status()),
        )));
    }
    lines
}

/// Word-wrap `text` to `width` columns, splitting words longer than a row.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut rows = vec![String::new()];
    for word in text.split(' ') {
        let row = rows.last_mut().expect("one row");
        let len = row.chars().count();
        if len > 0 && len + 1 + word.chars().count() > width {
            rows.push(String::new());
        } else if len > 0 {
            row.push(' ');
        }
        let mut chars: Vec<char> = word.chars().collect();
        while !chars.is_empty() {
            let row = rows.last_mut().expect("one row");
            let room = width.saturating_sub(row.chars().count()).max(1);
            let take = room.min(chars.len());
            row.extend(chars.drain(..take));
            if !chars.is_empty() {
                rows.push(String::new());
            }
        }
    }
    rows
}

fn render_form(frame: &mut Frame, area: Rect, form: &LaunchForm) {
    let lines = form_lines(form, usize::from(area.width));
    let selected = lines.iter().position(|line| {
        line.spans
            .first()
            .is_some_and(|span| span.content.starts_with("> "))
    });
    render_lines(frame, area, lines, selected.map(|line| line + 4));
}

/// What the detail half shows for the selected seat.
enum Detail {
    Form,
    Empty(Vec<Line<'static>>),
    Conversation {
        session_id: Uuid,
        header: Vec<Line<'static>>,
        loaded: bool,
    },
}

fn seat_header_lines(seat: &SeatEntry) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = seat_summary(seat)
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let style = if index == 0 {
                Style::default()
                    .fg(theme::text())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::text())
            };
            Line::from(Span::styled(safe(&line), style))
        })
        .collect();
    if let Some(action) = seat.health.action() {
        lines.push(Line::from(Span::styled(
            action.to_string(),
            Style::default().fg(health_color(seat.health)),
        )));
    }
    lines
}

fn detail(app: &App, state: &GlobalManagerWorkspaceState) -> Detail {
    if state.launch.is_some() {
        return Detail::Form;
    }
    let Some(seat) = state.selected_seat() else {
        return Detail::Empty(if state.has_seats() {
            vec![Line::from(Span::styled(
                format!("Select a seat to read its conversation, or {LAUNCH_HINT}."),
                Style::default().fg(theme::text()),
            ))]
        } else {
            Vec::new()
        });
    };
    let mut header = seat_header_lines(&seat);
    if let Some(caps) = state.selected_cap_line() {
        header.push(Line::from(Span::styled(
            safe(&caps),
            Style::default().fg(theme::text()),
        )));
    }
    match seat.session_id {
        Some(session_id) => Detail::Conversation {
            session_id,
            header,
            loaded: app.sessions.contains_key(&session_id),
        },
        None => {
            let mut lines = header;
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                format!("This seat has no live session to show; {LAUNCH_HINT}."),
                Style::default().fg(theme::text()),
            )));
            Detail::Empty(lines)
        }
    }
}

/// Rows `lines` take when word-wrapped (as rendered) at `width`.
fn wrapped_height(lines: &[Line<'static>], width: u16) -> u16 {
    let rows = Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width.max(1));
    u16::try_from(rows).unwrap_or(u16::MAX)
}

/// Render the console. Takes `&mut App` because the conversation pane updates
/// the shown session's per-frame view state, like the detail pane does.
pub(super) fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    frame.render_widget(Clear, area);
    let title = match &app.overlay {
        OverlayState::GlobalManagerWorkspace(state) => state.node_title(),
        _ => None,
    }
    .map_or_else(
        || " Global manager workspace ".to_string(),
        |title| format!(" {} ", safe(&title)),
    );
    let block = theme::overlay_block()
        .title(Line::from(Span::styled(
            title,
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 3 {
        return;
    }
    let (areas, detail, focus) = {
        let OverlayState::GlobalManagerWorkspace(state) = &app.overlay else {
            return;
        };
        let preamble = preamble(app, state);
        // One spacer row below the preamble once a snapshot is shown.
        let spacer = u16::from(state.snapshot.is_some());
        let areas = console_areas(
            inner,
            state,
            wrapped_height(&preamble, inner.width) + spacer,
        );
        frame.render_widget(
            Paragraph::new(preamble).wrap(Wrap { trim: false }),
            areas.preamble,
        );
        if let Some(seats) = areas.seats {
            let (lines, selected) = seat_lines(
                state,
                usize::from(seats.width),
                state.focus == WorkspaceFocus::Seats && state.launch.is_none(),
            );
            if state.has_seats() {
                render_lines(frame, seats, lines, selected);
            } else {
                frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), seats);
            }
        }
        if let Some(rule) = areas.rule {
            frame.render_widget(
                Paragraph::new(vec![Line::from("│"); usize::from(rule.height)])
                    .style(Style::default().fg(theme::overlay_border())),
                rule,
            );
        }
        if let Some(status) = areas.status {
            let (text, color) = match (&state.error, &state.notice) {
                (Some(error), _) => (error.clone(), theme::error_status()),
                (None, Some(notice)) => (notice.clone(), theme::status_completed()),
                (None, None) => (String::new(), theme::text()),
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    safe(&text),
                    Style::default().fg(color),
                ))),
                status,
            );
        }
        (areas, detail(app, state), state.focus)
    };
    let Some(detail_area) = areas.detail else {
        return;
    };
    match detail {
        Detail::Form => {
            if let OverlayState::GlobalManagerWorkspace(state) = &app.overlay
                && let Some(form) = &state.launch
            {
                render_form(frame, detail_area, form);
            }
        }
        Detail::Empty(lines) => {
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }),
                detail_area,
            );
        }
        Detail::Conversation {
            session_id,
            header,
            loaded,
        } => {
            let header_height = wrapped_height(&header, detail_area.width)
                .min(detail_area.height.saturating_sub(4));
            frame.render_widget(
                Paragraph::new(header).wrap(Wrap { trim: false }),
                Rect::new(
                    detail_area.x,
                    detail_area.y,
                    detail_area.width,
                    header_height,
                ),
            );
            let body = Rect::new(
                detail_area.x,
                detail_area.y + header_height,
                detail_area.width,
                detail_area.height.saturating_sub(header_height),
            );
            if !loaded {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        "Loading the seat's conversation… (r refreshes)",
                        Style::default().fg(theme::dim_metadata()),
                    ))),
                    body,
                );
                return;
            }
            let focused = focus != WorkspaceFocus::Seats;
            let column = crate::ui::prepare_session_detail(app, session_id, body);
            crate::ui::render_session_conversation(frame, &column, session_id, focused, app);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::app::App;
    use crate::app::app_test_helpers::baseline_session;
    use crate::overlay::global_manager_workspace::tests::{
        Fixture, fixture, pm, policy, portfolio, project,
    };
    use crate::overlay::global_manager_workspace::{GlobalManagerWorkspaceState, WorkspaceFocus};
    use crate::overlay::global_manager_workspace_launch::{LaunchField, LaunchForm, LaunchRole};
    use crate::types::{OverlayState, SessionState};
    use chrono::Utc;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use rsi_common::global_manager::GlobalManagerWorkspaceV1;
    use rsi_common::types::{ConversationEvent, EventType, Role, SessionKind, SessionStatus};
    use uuid::Uuid;

    /// Each line of the buffer, right-trimmed.
    fn screen(app: &mut App, width: u16, height: u16) -> String {
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
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn at() -> chrono::DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-10-05T20:15:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn message(session_id: Uuid, sequence: i32, role: Role, content: &str) -> ConversationEvent {
        ConversationEvent {
            id: i64::from(sequence),
            session_id,
            sequence,
            event_type: EventType::Message,
            role: Some(role),
            content: content.into(),
            created_at: at(),
            tool_name: None,
            tool_input: None,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    /// A cached seat session with a short conversation.
    fn seat_session(
        id: Uuid,
        project_id: Uuid,
        title: &str,
        lines: &[(Role, &str)],
    ) -> SessionState {
        let mut session = baseline_session(id, SessionKind::Standard);
        session.project_id = Some(project_id);
        session.title = Some(title.into());
        session.status = SessionStatus::Completed;
        session.created_at = at();
        session.updated_at = at();
        let mut state = SessionState::new(session);
        state.events = lines
            .iter()
            .enumerate()
            .map(|(index, (role, text))| message(id, index as i32 + 1, *role, text))
            .collect();
        state.last_sequence = Some(lines.len() as i32);
        state
    }

    fn app_with(f: &Fixture, state: GlobalManagerWorkspaceState) -> App {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.projects = vec![f.a.clone(), f.b.clone()];
        app.projects.extend(extra_projects());
        app.sessions.insert(
            f.seat_id,
            seat_session(
                f.seat_id,
                f.b.id,
                "Global manager",
                &[
                    (Role::User, "Give me a portfolio digest."),
                    (
                        Role::Assistant,
                        "rsi: PM active, 3 Issues in progress. dictate-agent: no PM yet; I will appoint one.",
                    ),
                ],
            ),
        );
        app.sessions.insert(
            f.pm_id,
            seat_session(
                f.pm_id,
                f.a.id,
                "Project manager · rsi",
                &[
                    (Role::User, "What is landing next?"),
                    (
                        Role::Assistant,
                        "Issue 1231 is in review; 1232 lands after it.",
                    ),
                ],
            ),
        );
        app.overlay = OverlayState::GlobalManagerWorkspace(Box::new(state));
        app
    }

    fn loaded(snapshot: GlobalManagerWorkspaceV1) -> GlobalManagerWorkspaceState {
        let mut state = GlobalManagerWorkspaceState::default();
        state.install(snapshot, at());
        state
    }

    /// Fixed extra projects so the grant line can name them.
    fn extra_projects() -> Vec<rsi_common::types::Project> {
        ["satellite-fleet", "notes", "billing"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                let mut project = project(name);
                project.id = Uuid::from_u128(0x1213_0000 + index as u128);
                project
            })
            .collect()
    }

    /// A five-project portfolio covering every PM health, plus one granted
    /// project that no longer exists.
    fn rich(f: &Fixture) -> GlobalManagerWorkspaceV1 {
        let mut snapshot = f.snapshot.clone();
        let extra = extra_projects();
        let (c, d, e) = (&extra[0], &extra[1], &extra[2]);
        let gone = Uuid::from_u128(0xdead_0000);
        snapshot
            .grant
            .as_mut()
            .unwrap()
            .project_ids
            .extend([c.id, d.id, e.id, gone]);
        let mut asking = pm(Uuid::from_u128(0x1231_0001), SessionStatus::Running);
        asking.pending_question = true;
        snapshot.projects.push(portfolio(
            c,
            Some(pm(Uuid::from_u128(0x1231_0002), SessionStatus::Completed)),
            Some(policy(true, false)),
            false,
        ));
        snapshot.projects.push(portfolio(
            d,
            Some(asking),
            Some(policy(false, false)),
            false,
        ));
        snapshot.projects.push(portfolio(e, None, None, true));
        snapshot.missing_project_ids.push(gone);
        for project in &mut snapshot.projects {
            if let Some(manager) = project.overview.manager.as_mut() {
                manager.updated_at = at();
            }
        }
        if let Some(seat) = snapshot.seat.as_mut() {
            seat.updated_at = at();
        }
        snapshot
    }

    fn with_focus(
        mut state: GlobalManagerWorkspaceState,
        focus: WorkspaceFocus,
    ) -> GlobalManagerWorkspaceState {
        state.focus = focus;
        state
    }

    fn with_form(
        f: &Fixture,
        mut state: GlobalManagerWorkspaceState,
        role: LaunchRole,
        error: Option<&str>,
    ) -> GlobalManagerWorkspaceState {
        let mut projects = vec![f.a.clone(), f.b.clone()];
        projects.extend(extra_projects());
        let mut form = LaunchForm::new(role, &projects, state.grant(), Some(f.a.id));
        form.field = LaunchField::Scope;
        form.error = error.map(str::to_string);
        state.launch = Some(form);
        state
    }

    /// The named visual states (#1213, #1231 visual check).
    fn states(f: &Fixture) -> Vec<(&'static str, GlobalManagerWorkspaceState)> {
        let mut revoked = rich(f);
        revoked.grant.as_mut().unwrap().state = "revoked".into();
        let mut stale = loaded(rich(f));
        stale.error = Some(
            "Refresh failed: Not connected to daemon (showing the last snapshot); press r to retry"
                .into(),
        );
        let mut pm_selected = loaded(rich(f));
        pm_selected.selected = 1;
        let mut missing_pm = loaded(rich(f));
        missing_pm.selected = 2;
        let mut appointed = loaded(rich(f));
        appointed.notice = Some(
            "Global manager 0000abcd appointed (grant v4, 5 projects); it replaces the previous seat"
                .into(),
        );
        vec![
            ("loading", GlobalManagerWorkspaceState::default()),
            ("no-grant", loaded(GlobalManagerWorkspaceV1::default())),
            ("global-seat-conversation", loaded(rich(f))),
            ("pm-conversation", pm_selected.clone_for_dump()),
            (
                "typing-to-pm",
                with_focus(pm_selected, WorkspaceFocus::Input),
            ),
            ("missing-pm", missing_pm),
            ("revoked-grant", loaded(revoked)),
            ("stale-refresh", stale),
            ("appointed-notice", appointed),
            (
                "launch-global",
                with_form(f, loaded(rich(f)), LaunchRole::Global, None),
            ),
            (
                "launch-pm-error",
                with_form(
                    f,
                    loaded(rich(f)),
                    LaunchRole::Project(f.b.id),
                    Some(
                        "Launched session 0000abcd but the appointment was refused: manager_project_limit_reached. Enter retries the appointment; Esc keeps the session unappointed.",
                    ),
                ),
            ),
        ]
    }

    trait CloneForDump {
        fn clone_for_dump(&self) -> GlobalManagerWorkspaceState;
    }

    impl CloneForDump for GlobalManagerWorkspaceState {
        fn clone_for_dump(&self) -> GlobalManagerWorkspaceState {
            GlobalManagerWorkspaceState {
                snapshot: self.snapshot.clone(),
                selected: self.selected,
                error: self.error.clone(),
                notice: self.notice.clone(),
                loaded_at: self.loaded_at,
                last_attempt: None,
                focus: self.focus,
                launch: self.launch.clone(),
                target: self.target,
                node: self.node.clone(),
            }
        }
    }

    /// Put `session_id`'s input bar in insert mode holding `text`.
    fn type_draft(app: &mut App, session_id: Uuid, text: &str) {
        crate::input_bar::enter_insert_mode(
            app,
            session_id,
            crate::modalkit_types::InsertStyle::Insert,
        );
        let surface = &mut app.sessions.get_mut(&session_id).unwrap().input_bar.surface;
        surface.textarea.insert_str(text);
    }

    #[test]
    fn typing_shows_the_draft_in_the_seats_input_bar() {
        let f = fixture();
        let mut app = app_with(&f, state_named(&f, "typing-to-pm"));
        type_draft(&mut app, f.pm_id, "Hold until it lands");
        let text = screen(&mut app, 120, 40);
        for needle in ["INSERT", "Hold until it lands", "PM · rsi · ACTIVE"] {
            assert!(text.contains(needle), "{needle} in\n{text}");
        }
    }

    fn state_named(f: &Fixture, name: &str) -> GlobalManagerWorkspaceState {
        states(f)
            .into_iter()
            .find(|(n, _)| *n == name)
            .map(|(_, s)| s)
            .unwrap()
    }

    #[test]
    fn empty_state_tells_the_operator_how_to_launch_or_appoint() {
        let f = fixture();
        let mut app = app_with(&f, loaded(GlobalManagerWorkspaceV1::default()));
        let text = screen(&mut app, 120, 40);
        for needle in [
            "Global manager workspace",
            "Above all projects",
            "No global manager is appointed.",
            "Press n to launch and appoint one here",
            ":manager global appoint [project names...]",
        ] {
            assert!(text.contains(needle), "{needle} in\n{text}");
        }
    }

    #[test]
    fn the_split_shows_seats_beside_the_selected_managers_live_conversation() {
        let f = fixture();
        for (width, height) in [(120, 40), (200, 50)] {
            let mut app = app_with(&f, loaded(rich(&f)));
            let text = screen(&mut app, width, height);
            for needle in [
                "Grant v3 active",
                "SEAT",
                "HEALTH",
                "global manager",
                "rsi",
                "ACTIVE",
                "dictate-agent",
                "MISSING",
                "satellite-fleet",
                "PAUSED",
                "WAITING",
                "REVOKED",
                "deleted 00000000",
                // The conversation pane: the seat header and the transcript.
                "GLOBAL SEAT · IDLE",
                "Give me a portfolio digest.",
                "rsi: PM active",
            ] {
                assert!(
                    text.contains(needle),
                    "{width}x{height}: {needle} in\n{text}"
                );
            }
            // The list and the transcript share rows: side by side.
            let row = text
                .lines()
                .find(|line| line.contains("satellite-fleet") && line.contains("PAUSED"))
                .unwrap_or_else(|| panic!("{width}x{height}: seat row in\n{text}"));
            assert!(
                row.matches('│').count() >= 3,
                "the borders plus the rule between the halves: {row}"
            );
        }
    }

    #[test]
    fn switching_seats_switches_the_conversation() {
        let f = fixture();
        let mut app = app_with(&f, state_named(&f, "pm-conversation"));
        let text = screen(&mut app, 200, 50);
        for needle in [
            "PM · rsi · ACTIVE",
            "Issues 12 open, 3 in progress, 1 operator request",
            "What is landing next?",
            "Issue 1231 is in review",
        ] {
            assert!(text.contains(needle), "{needle} in\n{text}");
        }
    }

    #[test]
    fn missing_and_revoked_seats_say_what_to_do() {
        let f = fixture();
        let mut app = app_with(&f, state_named(&f, "missing-pm"));
        let text = screen(&mut app, 120, 40);
        for needle in [
            "PM · dictate-agent · MISSING",
            "No live manager in this seat. Press n to launch and appoint one.",
        ] {
            assert!(text.contains(needle), "{needle} in\n{text}");
        }
        let mut app = app_with(&f, state_named(&f, "revoked-grant"));
        let text = screen(&mut app, 200, 50);
        for needle in [
            "Grant v3 revoked",
            "Revoked: the seat holds no authority",
            "GLOBAL SEAT · REVOKED",
            "Press n to launch and appoint a new manager.",
        ] {
            assert!(text.contains(needle), "{needle} in\n{text}");
        }
        let mut app = app_with(&f, state_named(&f, "stale-refresh"));
        let text = screen(&mut app, 200, 50);
        for needle in ["stale", "showing the last snapshot", "press r to retry"] {
            assert!(text.contains(needle), "{needle} in\n{text}");
        }
    }

    #[test]
    fn the_launch_form_shows_fields_scope_and_inline_errors() {
        let f = fixture();
        let mut app = app_with(&f, state_named(&f, "launch-global"));
        let text = screen(&mut app, 120, 40);
        for needle in [
            "Instantiate a manager",
            "Seat      < Global manager >",
            "Provider  < Claude >",
            "Model     < claude-opus-5-5 >",
            "Effort    < high >",
            "Scope     5 of 5 projects",
            "[x] satellite-fleet",
            "global manager for: rsi,",
            "[ Launch and appoint ]",
        ] {
            assert!(text.contains(needle), "{needle} in\n{text}");
        }
        let mut app = app_with(&f, state_named(&f, "launch-pm-error"));
        let text = screen(&mut app, 200, 50);
        for needle in [
            "Seat      < Project manager · dictate-agent >",
            "Scope: the whole project",
            "appointment was refused: manager_project_limit_reached",
        ] {
            assert!(text.contains(needle), "{needle} in\n{text}");
        }
    }

    #[test]
    fn narrow_terminals_alternate_list_and_conversation() {
        let f = fixture();
        let mut app = app_with(&f, loaded(rich(&f)));
        let list = screen(&mut app, 80, 30);
        assert!(list.contains("satellite-fleet"), "{list}");
        let mut app = app_with(
            &f,
            with_focus(loaded(rich(&f)), WorkspaceFocus::Conversation),
        );
        let conversation = screen(&mut app, 80, 30);
        assert!(
            conversation.contains("Give me a portfolio digest."),
            "{conversation}"
        );
        assert!(conversation.contains("GLOBAL SEAT"), "{conversation}");
    }

    #[test]
    fn no_hint_bar_is_reserved_and_status_lines_appear_only_when_set() {
        let f = fixture();
        let mut app = app_with(&f, state_named(&f, "appointed-notice"));
        let text = screen(&mut app, 200, 50);
        let last_inner = text.lines().nth(48).unwrap();
        assert!(last_inner.contains("appointed (grant v4"), "{last_inner}");
        let mut app = app_with(&f, loaded(rich(&f)));
        let text = screen(&mut app, 200, 50);
        assert!(text.contains("? keys"), "{text}");
    }

    /// #1240: the node consoles, with every timestamp pinned for dumps.
    fn node_states(f: &Fixture) -> Vec<(&'static str, GlobalManagerWorkspaceState)> {
        use crate::overlay::global_manager_workspace::node_tests::{
            area_node, global_node, node_state, pinnacle_node, project_node,
        };
        let pin = |mut node: rsi_common::manager_node_workspace::ManagerNodeWorkspaceV1| {
            node.fleet.as_of = at();
            if let Some(seat) = node.seat.as_mut() {
                seat.updated_at = at();
            }
            for child in &mut node.children {
                if let Some(seat) = child.seat.as_mut() {
                    seat.updated_at = at();
                }
            }
            for project in &mut node.projects {
                if let Some(manager) = project.overview.manager.as_mut() {
                    manager.updated_at = at();
                }
            }
            for escalation in &mut node.escalations {
                escalation.created_at = at();
            }
            node_state(node, at())
        };
        let mut global_child = pin(pinnacle_node(f));
        global_child.selected = 1;
        vec![
            ("pinnacle", pin(pinnacle_node(f))),
            ("pinnacle-global-selected", global_child),
            ("global", pin(global_node(f))),
            ("project", pin(project_node(f))),
            ("area", pin(area_node(f))),
        ]
    }

    /// `app_with` plus the pinnacle's projects and the pinnacle and area
    /// seats' conversations.
    fn node_app(f: &Fixture, state: GlobalManagerWorkspaceState) -> App {
        use crate::overlay::global_manager_workspace::node_tests::{
            AREA_SEAT, PINNACLE_SEAT, pinnacle_projects,
        };
        let mut app = app_with(f, state);
        let (billing, notes) = pinnacle_projects();
        app.projects
            .retain(|p| p.name != "billing" && p.name != "notes");
        app.projects.extend([billing, notes]);
        app.sessions.insert(
            PINNACLE_SEAT,
            seat_session(
                PINNACLE_SEAT,
                f.b.id,
                "Pinnacle manager",
                &[
                    (Role::User, "Which global needs help?"),
                    (
                        Role::Assistant,
                        "global 2 has no seat; billing waits on it. I will appoint one.",
                    ),
                ],
            ),
        );
        app.sessions.insert(
            AREA_SEAT,
            seat_session(
                AREA_SEAT,
                f.a.id,
                "Area manager",
                &[(Role::Assistant, "Epic 1240 is green; reporting up.")],
            ),
        );
        app
    }

    /// #1240 acceptance: the console renders a pinnacle and a global (and
    /// a project and an area) at 120x40 and 200x50.
    #[test]
    fn node_consoles_render_a_pinnacle_and_a_global() {
        let f = fixture();
        for (width, height) in [(120, 40), (200, 50)] {
            let mut app = node_app(&f, node_states(&f).remove(0).1);
            let text = screen(&mut app, width, height);
            for needle in [
                "pinnacle manager console",
                "Reports to the operator · 3 children · 4 projects covered",
                "Fleet 20 active",
                "1 escalation waiting on this node",
                "pinnacle manager",
                "global b1240002",
                "billing",
                "notes",
                "PINNACLE SEAT · IDLE",
                "Which global needs help?",
            ] {
                assert!(
                    text.contains(needle),
                    "{width}x{height}: {needle} in\n{text}"
                );
            }
            let mut app = node_app(&f, node_states(&f).remove(2).1);
            let text = screen(&mut app, width, height);
            for needle in [
                "global manager console",
                "Reports to portfolio node a1240001",
                "Grant v3 active",
                "global manager",
                "rsi",
                "dictate-agent",
                "GLOBAL SEAT · IDLE",
                "Give me a portfolio digest.",
            ] {
                assert!(
                    text.contains(needle),
                    "{width}x{height}: {needle} in\n{text}"
                );
            }
        }
    }

    /// Writes the #1240 node console dumps at 120x40 and 200x50 to
    /// `thoughts/shared/research/2026-10-06-1240-visual/`. Run with
    /// `cargo test -p rsi --lib node_console_visual_dump -- --ignored`.
    #[test]
    #[ignore = "writes visual-check artifacts"]
    fn node_console_visual_dump() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../thoughts/shared/research/2026-10-06-1240-visual");
        std::fs::create_dir_all(&dir).unwrap();
        let f = fixture();
        for (width, height) in [(120, 40), (200, 50)] {
            for (name, state) in node_states(&f) {
                let mut app = node_app(&f, state);
                let text = screen(&mut app, width, height);
                std::fs::write(
                    dir.join(format!("{name}-{width}x{height}.txt")),
                    text + "\n",
                )
                .unwrap();
            }
        }
    }

    /// Writes the text dumps of every state at 120x40 and 200x50 to
    /// `thoughts/shared/research/2026-10-05-1231-visual/`. Run with
    /// `cargo test -p rsi --lib global_manager_workspace_visual_dump -- --ignored`.
    #[test]
    #[ignore = "writes visual-check artifacts"]
    fn global_manager_workspace_visual_dump() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../thoughts/shared/research/2026-10-05-1231-visual");
        std::fs::create_dir_all(&dir).unwrap();
        let f = fixture();
        for (width, height) in [(120, 40), (200, 50)] {
            for (name, state) in states(&f) {
                let mut app = app_with(&f, state);
                if name == "typing-to-pm" {
                    type_draft(
                        &mut app,
                        f.pm_id,
                        "Hold #1232 until #1231 lands, then report.",
                    );
                }
                let text = screen(&mut app, width, height);
                std::fs::write(
                    dir.join(format!("{name}-{width}x{height}.txt")),
                    text + "\n",
                )
                .unwrap();
            }
        }
        for (name, focus) in [
            ("narrow-list", WorkspaceFocus::Seats),
            ("narrow-conversation", WorkspaceFocus::Conversation),
        ] {
            let mut app = app_with(&f, with_focus(loaded(rich(&f)), focus));
            let text = screen(&mut app, 80, 30);
            std::fs::write(dir.join(format!("{name}-80x30.txt")), text + "\n").unwrap();
        }
    }
}
