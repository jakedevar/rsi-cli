//! Global manager workspace (#1213, #1231, #2237): render the full-screen
//! operator console in the session list's visual language. A scope header
//! (breadcrumb and count) sits over a signal band that carries the grant (or
//! the node's grant, fleet and escalations); the seat list below it uses the
//! list's column header, `★ SECTION n ────` rules, status glyphs, the accent
//! rail and selected-row band; the selected seat's live conversation sits on
//! the right under its own signal band, drawn by the shared session-detail
//! renderer (`ui::prepare_session_detail` / `ui::render_session_conversation`).
//! The last row names the keys. Below `SPLIT_MIN_WIDTH` the list and the
//! conversation take turns: the list while it has focus, the conversation while
//! it or its input bar does.

use crate::app::App;
use crate::overlay::global_manager_workspace::{
    APPOINT_HINT, GlobalManagerWorkspaceState, LAUNCH_HINT, SPLIT_MIN_WIDTH, SeatColumn, SeatEntry,
    SeatHealth, SeatLevel, WorkspaceFocus, WorkspaceRow, seat_cells, seat_header, seat_health,
    seat_summary,
};
use crate::overlay::global_manager_workspace_launch::{LaunchField, LaunchForm, LaunchRole};
use crate::types::OverlayState;
use crate::ui::{glyphs, session, theme};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph, Wrap};
use uuid::Uuid;

pub(super) fn safe(value: &str) -> String {
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

/// The one-cell status glyph of a seat, from the session list's vocabulary.
fn health_glyph(health: SeatHealth) -> &'static str {
    match health {
        SeatHealth::Active => "●",
        SeatHealth::Idle => glyphs::DONE,
        SeatHealth::Waiting => glyphs::NEEDS_YOU,
        SeatHealth::Paused => glyphs::SOFT_PAUSE,
        SeatHealth::Stopped => glyphs::FAILED,
        SeatHealth::Missing => "○",
        SeatHealth::Revoked => glyphs::ROTATION_OFF,
    }
}

/// Where each part of the console goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConsoleAreas {
    /// The breadcrumb and count row.
    pub header: Rect,
    /// The signal band: the grant, or the node's grant, fleet and escalations.
    pub signal: Rect,
    /// The seat list, when shown.
    pub seats: Option<Rect>,
    /// The vertical rule between the halves (split layout only).
    pub rule: Option<Rect>,
    /// The conversation or launch form, when shown.
    pub detail: Option<Rect>,
    /// The error/notice line, only while there is one.
    pub status: Option<Rect>,
    /// The key hints, the last row.
    pub footer: Option<Rect>,
}

/// The seat list's width in the split layout.
#[must_use]
pub(crate) fn seat_list_width(inner_width: u16) -> u16 {
    (inner_width * 43 / 100).clamp(47, 96)
}

pub(crate) fn console_areas(
    inner: Rect,
    state: &GlobalManagerWorkspaceState,
    signal_height: u16,
) -> ConsoleAreas {
    let header_height = 1u16.min(inner.height);
    let footer_height = u16::from(inner.height >= 8);
    let has_status = state.error.is_some() || state.notice.is_some();
    let status_height = u16::from(has_status && inner.height > header_height + footer_height + 2);
    let signal_height = signal_height.min(
        inner
            .height
            .saturating_sub(header_height + footer_height + status_height),
    );
    let top = header_height + signal_height;
    let body = Rect::new(
        inner.x,
        inner.y + top,
        inner.width,
        inner
            .height
            .saturating_sub(top + footer_height + status_height),
    );
    let header = Rect::new(inner.x, inner.y, inner.width, header_height);
    let signal = Rect::new(inner.x, inner.y + header_height, inner.width, signal_height);
    let status = (status_height == 1).then(|| {
        Rect::new(
            inner.x,
            inner.y + inner.height - footer_height - 1,
            inner.width,
            1,
        )
    });
    let footer = (footer_height == 1)
        .then(|| Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1));
    // Without a grant (or a node snapshot) there are no seats: the empty
    // state (or the launch form) takes the whole body.
    let no_grant = state.snapshot.is_some() && !state.has_seats();
    if no_grant || state.snapshot.is_none() {
        let form = state.launch.is_some();
        return ConsoleAreas {
            header,
            signal,
            seats: (!form).then_some(body),
            rule: None,
            detail: form.then_some(body),
            status,
            footer,
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
            header,
            signal,
            seats: Some(seats),
            rule: Some(rule),
            detail: Some(detail),
            status,
            footer,
        };
    }
    let detail_first = state.launch.is_some() || state.focus != WorkspaceFocus::Seats;
    ConsoleAreas {
        header,
        signal,
        seats: (!detail_first).then_some(body),
        rule: None,
        detail: detail_first.then_some(body),
        status,
        footer,
    }
}

/// `text` split at ` · ` into styled facts: the first bold, `active` and
/// `revoked` states coloured, the separators dim. The characters are exactly
/// `text`'s, so the line reads the same as the plain string.
fn facts_line(text: &str, bg: Color) -> Line<'static> {
    let base = Style::default().fg(theme::text()).bg(bg);
    let sep = Style::default().fg(theme::dim_metadata()).bg(bg);
    let mut spans = Vec::new();
    for (index, segment) in safe(text).split(" · ").enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", sep));
        }
        let style = if segment.ends_with(" active") {
            base.fg(theme::status_running())
                .add_modifier(Modifier::BOLD)
        } else if segment.ends_with(" revoked") {
            base.fg(theme::error_status()).add_modifier(Modifier::BOLD)
        } else if index == 0 {
            base.add_modifier(Modifier::BOLD)
        } else {
            base
        };
        spans.push(Span::styled(segment.to_string(), style));
    }
    Line::from(spans)
}

/// The signal band's lines: the grant for the global console; the node's
/// location, grant, fleet and escalations for a node console.
fn signal_lines(app: &App, state: &GlobalManagerWorkspaceState) -> Vec<Line<'static>> {
    let bg = theme::selected_row_bg();
    let mut lines = Vec::new();
    if state.target.is_some() {
        // #1240: a node console.
        for line in state.node_lines(&app.projects) {
            let text = safe(&line);
            if text.contains("escalation") {
                lines.push(Line::from(Span::styled(
                    text,
                    Style::default().fg(theme::warning_status()).bg(bg),
                )));
            } else {
                lines.push(facts_line(&text, bg));
            }
        }
        if state
            .node
            .as_ref()
            .is_some_and(|node| node.state == "revoked")
        {
            lines.push(Line::from(Span::styled(
                "Revoked: this node holds no authority.".to_string(),
                Style::default()
                    .fg(theme::error_status())
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        return lines;
    }
    if let Some(snapshot) = &state.snapshot
        && let Some(grant) = &snapshot.grant
    {
        lines.push(facts_line(&state.grant_line(&app.projects), bg));
        if seat_health(grant, snapshot.seat.as_ref()) == SeatHealth::Revoked {
            lines.push(Line::from(Span::styled(
                format!("Revoked: the seat holds no authority. Appoint a new one: {LAUNCH_HINT}"),
                Style::default()
                    .fg(theme::error_status())
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            )));
        }
    }
    lines
}

/// The breadcrumb row, like the session list's scope header: where the view
/// sits on the left, the count and freshness on the right.
fn scope_header(state: &GlobalManagerWorkspaceState, width: u16) -> Line<'static> {
    let name = if state.target.is_some() {
        state
            .node
            .as_ref()
            .map_or_else(|| "Manager node".to_string(), |node| safe(&node.label))
    } else {
        "Above all projects".to_string()
    };
    let freshness = match (&state.loaded_at, &state.error) {
        (_, Some(_)) if state.snapshot.is_some() => "stale".to_string(),
        (Some(at), _) => format!("as of {}", at.format("%H:%M:%S")),
        (None, _) => "loading".to_string(),
    };
    let count = state.seats().len();
    let sep = Style::default().fg(theme::overlay1());
    let left = vec![
        Span::raw(" "),
        Span::styled(
            name,
            Style::default()
                .fg(theme::green())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" / ", sep),
        Span::styled(
            "Seats",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
    ];
    let hint = if state.target.is_some() {
        " · Backspace goes up"
    } else {
        " · same view from every tab"
    };
    let right = vec![
        Span::styled(
            count.to_string(),
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "{} · {freshness}{hint} ",
                if count == 1 { " seat" } else { " seats" }
            ),
            Style::default().fg(theme::dim_metadata()),
        ),
    ];
    let used = Line::from(left.clone()).width() + Line::from(right.clone()).width();
    let pad = usize::from(width).saturating_sub(used);
    let mut spans = left;
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
        spans.extend(right);
    }
    Line::from(spans)
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
            "and the projects it oversees, set what it may do (the grant), then talk to it in this view.",
            text,
        )),
        Line::default(),
        Line::from(Span::styled(
            format!("To appoint an existing session instead, focus it and run {APPOINT_HINT}."),
            Style::default().fg(theme::dim_metadata()),
        )),
    ]
}

/// The section a seat opens in the list, when it opens one: a rule line like
/// the session list's `★ MANAGERS 1 ────`.
fn section_of(seat: &SeatEntry) -> Option<(String, &'static str, Color)> {
    if matches!(seat.row, WorkspaceRow::Predecessor { .. }) || seat.depth >= 2 {
        return None;
    }
    Some(match seat.level {
        SeatLevel::Global => ("GLOBAL".to_string(), glyphs::MANAGERS, theme::accent()),
        SeatLevel::Portfolio if seat.depth == 0 => (
            seat.tier
                .as_deref()
                .map_or_else(|| "NODE".to_string(), str::to_uppercase),
            glyphs::MANAGERS,
            theme::accent(),
        ),
        SeatLevel::Portfolio => (
            "CHILD NODES".to_string(),
            glyphs::GROUP_CONTAINER,
            theme::accent(),
        ),
        SeatLevel::Project => ("PROJECT MANAGERS".to_string(), "●", theme::status_running()),
        SeatLevel::Area => ("AREAS".to_string(), glyphs::EPIC_CONTAINER, theme::blue()),
    })
}

fn cell_style(column: SeatColumn, seat: &SeatEntry, text: &str) -> Style {
    let dim = Style::default().fg(theme::dim_metadata());
    let count = |n: Option<i64>, hot: Color| match n {
        Some(n) if n > 0 => Style::default().fg(hot).add_modifier(Modifier::BOLD),
        _ => dim,
    };
    match column {
        SeatColumn::Name => {
            if matches!(seat.row, WorkspaceRow::Predecessor { .. }) {
                dim
            } else {
                Style::default().fg(theme::text())
            }
        }
        SeatColumn::Health => Style::default()
            .fg(health_color(seat.health))
            .add_modifier(Modifier::BOLD),
        SeatColumn::Context => {
            if text.trim() == "-" {
                dim
            } else if seat.context_fill_pct.is_some_and(|pct| pct >= 85.0) {
                Style::default().fg(theme::warning_status())
            } else {
                Style::default().fg(theme::count_text())
            }
        }
        SeatColumn::Issues => {
            if seat.issues.is_some_and(|(_, _, requests)| requests > 0) {
                Style::default().fg(theme::warning_status())
            } else if seat.issues.is_some() {
                Style::default().fg(theme::count_text())
            } else {
                dim
            }
        }
        SeatColumn::Model => Style::default().fg(theme::model_text()),
        SeatColumn::Running => count(seat.activity.map(|a| a.0), theme::status_running()),
        SeatColumn::Waiting => count(seat.activity.map(|a| a.1), theme::status_waiting()),
        SeatColumn::Questions => count(seat.activity.map(|a| a.2), theme::status_waiting()),
        SeatColumn::Approvals => count(seat.activity.map(|a| a.3), theme::status_waiting()),
        SeatColumn::Updated => Style::default().fg(theme::time_text()),
    }
}

/// One seat's list row in the session list's shape: accent rail, status glyph,
/// then the aligned cells; the selected row sits on the list's selected band.
fn seat_row_line(seat: &SeatEntry, width: usize, selected: bool, focused: bool) -> Line<'static> {
    let bg = selected.then(theme::selected_row_bg);
    let on = |style: Style| bg.map_or(style, |bg| style.bg(bg));
    let rail = match (selected, focused) {
        (true, true) => "▌",
        (true, false) => "│",
        _ => " ",
    };
    let mut spans = vec![
        Span::styled(
            rail,
            on(Style::default()
                .fg(theme::active_row_rail())
                .add_modifier(Modifier::BOLD)),
        ),
        Span::styled(
            health_glyph(seat.health),
            on(Style::default().fg(health_color(seat.health))),
        ),
        // A space between the glyph and the name, like the list's columns.
        Span::styled(" ", on(Style::default())),
    ];
    for (column, text) in seat_cells(seat, width) {
        let mut style = cell_style(column, seat, &text);
        if selected && column == SeatColumn::Name {
            style = style.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(safe(&text), on(style)));
    }
    let line = Line::from(spans);
    match bg {
        Some(bg) => line.style(Style::default().bg(bg)),
        None => line,
    }
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
    // One column wider than the plain header: rail, glyph and a gap lead each row.
    let mut lines = vec![Line::from(Span::styled(
        format!(" {}", seat_header(width)),
        Style::default()
            .fg(theme::table_header_text())
            .add_modifier(Modifier::BOLD),
    ))];
    let seats = state.seats();
    // How many seats each section holds, for its header's count.
    let mut sections: Vec<(String, usize)> = Vec::new();
    for seat in &seats {
        match section_of(seat) {
            Some((name, ..)) if sections.last().is_none_or(|(last, _)| *last != name) => {
                sections.push((name, 1));
            }
            _ => {
                if let Some(last) = sections.last_mut() {
                    last.1 += 1;
                }
            }
        }
    }
    let mut section_index = 0;
    let mut opened: Option<String> = None;
    let mut selected_line = None;
    for (index, seat) in seats.iter().enumerate() {
        if let Some((name, glyph, color)) = section_of(seat)
            && opened.as_deref() != Some(name.as_str())
        {
            let count = sections.get(section_index).map_or(0, |(_, n)| *n);
            section_index += 1;
            lines.push(session::section_header_line(
                glyph,
                &name,
                &format!("  {count}  "),
                color,
                theme::overlay_bg(),
                width,
            ));
            opened = Some(name);
        }
        let selected = index == state.selected;
        if selected {
            selected_line = Some(lines.len());
        }
        lines.push(seat_row_line(seat, width, selected, focused));
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
                "j/k field · h/l change (seat, host) · Space/a scope · Enter launch (on Grant: edit) · Esc cancel"
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
                let body = if focused && crate::field_edit::standard_frame() {
                    crate::field_edit::mark(&form.prompt, "")
                } else {
                    let cursor = if focused { "▏" } else { "" };
                    format!("{}{cursor}", safe(&form.prompt))
                };
                let mut in_selection = false;
                for row in wrap_words(&body, width.saturating_sub(4).max(10)) {
                    let mut spans = vec![Span::styled("    ", text)];
                    spans.extend(crate::field_edit::marked_row(&row, &mut in_selection, text));
                    lines.push(Line::from(spans));
                }
            }
            LaunchField::Grant => {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{marker}{:<9} {}", field.label(), safe(&form.value(field))),
                        style,
                    ),
                    Span::styled(if focused { "   Enter edits" } else { "" }, dim),
                ]));
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
            "  Scope: the whole project; the policy comes from Grant (Execute by default; change later with :manager policy)",
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
    if let Some(note) = &form.prefill_note {
        lines.push(Line::default());
        for row in wrap_words(&safe(note), width.saturating_sub(2).max(10)) {
            lines.push(Line::from(Span::styled(format!("  {row}"), dim)));
        }
    }
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

/// The grant editor's heading above the shared editor view.
fn grant_heading(form: &LaunchForm) -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    vec![
        Line::from(Span::styled(
            format!("Grant · {}", safe(&form.role_label())),
            bold,
        )),
        Line::from(Span::styled(
            "j/k row · h/l change · Space toggle · digits type a number · Esc back to the form",
            Style::default().fg(theme::dim_metadata()),
        )),
        Line::default(),
    ]
}

fn render_form(frame: &mut Frame, area: Rect, form: &LaunchForm) {
    if form.grant_open {
        let heading = grant_heading(form);
        let used = u16::try_from(heading.len()).unwrap_or(3).min(area.height);
        frame.render_widget(
            Paragraph::new(heading),
            Rect::new(area.x, area.y, area.width, used),
        );
        super::grant_editor::render_grant(
            frame,
            Rect::new(area.x, area.y + used, area.width, area.height - used),
            &form.grant,
            theme::overlay_bg(),
        );
        return;
    }
    let lines = form_lines(form, usize::from(area.width));
    let selected = lines.iter().position(|line| {
        line.spans
            .first()
            .is_some_and(|span| span.content.starts_with("> "))
    });
    render_lines(frame, area, lines, selected.map(|line| line + 4));
}

/// The same draft fills the console for appointment and existing-grant edits.
fn render_coverage(
    frame: &mut Frame,
    area: Rect,
    form: &crate::overlay::manager_coverage::CoverageForm,
) {
    if area.height < 10 || area.width < 30 {
        frame.render_widget(
            Paragraph::new("Resize to at least 30 x 10. Esc cancels."),
            area,
        );
        return;
    }
    let width = usize::from(area.width);
    let policy = form.editor.policy();
    let heading = vec![
        Line::from(Span::styled(
            "Manager coverage & permissions",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "{}/5 {} · nothing saved",
            form.step + 1,
            crate::overlay::manager_coverage::STEPS[form.step]
        )),
        Line::from(format!(
            "{} · {} permissions · groups {}",
            form.editor.preset().label().replace(" project control", ""),
            policy.capabilities.len(),
            if policy.allow_create_groups {
                "ON"
            } else {
                "OFF"
            }
        )),
        Line::from(if form.step == 0 {
            format!(
                "/ Search: {}{}",
                safe(&form.query),
                if form.searching { "▏" } else { "" }
            )
        } else {
            "Tab next · Shift-Tab back · r Review".into()
        }),
    ];
    frame.render_widget(
        Paragraph::new(heading),
        Rect::new(area.x, area.y, area.width, 4),
    );
    let hints = if form.searching {
        "Type search · Enter done · Esc done"
    } else if form.step == 4 {
        if form.launch.is_some() {
            "y Launch and appoint · Esc edit"
        } else {
            "y Save permissions · Esc edit"
        }
    } else {
        "j/k move · Space toggle · h/l change"
    };
    let mut footer = vec![
        "Live budget: unknown · b Budget chain".to_string(),
        hints.into(),
    ];
    if !form.searching && form.step < 4 {
        footer.push("Enter next · Esc cancel".into());
    }
    if form.step < 4 {
        if let Some(error) = &form.error {
            footer.insert(0, safe(error));
        }
    }
    let footer: Vec<Line> = footer
        .iter()
        .flat_map(|text| wrap_words(text, width))
        .map(Line::from)
        .collect();
    let footer_height = u16::try_from(footer.len())
        .unwrap_or(4)
        .min(area.height.saturating_sub(6));
    let body_height = usize::from(area.height.saturating_sub(5 + footer_height));
    let rows = form.rows();
    let mut lines = Vec::new();
    let mut selected_line = 0;
    for (index, row) in rows.iter().enumerate() {
        if index == form.selected {
            selected_line = lines.len();
        }
        let marker = if index == form.selected { "> " } else { "  " };
        for (part, text) in wrap_words(&safe(row), width.saturating_sub(2).max(1))
            .into_iter()
            .enumerate()
        {
            lines.push(Line::from(Span::styled(
                format!("{}{text}", if part == 0 { marker } else { "  " }),
                Style::default().fg(if index == form.selected {
                    theme::text()
                } else {
                    theme::subtext0()
                }),
            )));
        }
    }
    let offset = selected_line
        .saturating_sub(body_height / 2)
        .min(lines.len().saturating_sub(body_height));
    frame.render_widget(
        Paragraph::new(format!(
            "Rows {}-{} / {}",
            form.selected + usize::from(!rows.is_empty()),
            (form.selected + body_height).min(rows.len()),
            rows.len()
        ))
        .style(Style::default().fg(theme::dim_metadata())),
        Rect::new(area.x, area.y + 4, area.width, 1),
    );
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(offset)
                .take(body_height)
                .collect::<Vec<_>>(),
        ),
        Rect::new(area.x, area.y + 5, area.width, body_height as u16),
    );
    frame.render_widget(
        Paragraph::new(footer),
        Rect::new(
            area.x,
            area.y + area.height - footer_height,
            area.width,
            footer_height,
        ),
    );
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

/// The selected seat's signal band: who it is and how it is doing, then its
/// portfolio signals and what the operator can do about it.
fn seat_header_lines(seat: &SeatEntry) -> Vec<Line<'static>> {
    let bg = theme::selected_row_bg();
    let health = seat.health.label();
    let mut lines: Vec<Line<'static>> = seat_summary(seat)
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let mut facts = facts_line(&line, bg);
            if index == 0 {
                // The health word keeps its seat colour, like the list.
                for span in &mut facts.spans {
                    if span.content == health {
                        span.style = span
                            .style
                            .fg(health_color(seat.health))
                            .add_modifier(Modifier::BOLD);
                    }
                }
            }
            facts
        })
        .collect();
    if let Some(action) = seat.health.action() {
        lines.push(Line::from(Span::styled(
            action.to_string(),
            Style::default().fg(health_color(seat.health)).bg(bg),
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
    let bg = theme::selected_row_bg();
    let mut header = seat_header_lines(&seat);
    // #1627: the global seat lives in one host project; name it.
    if seat.level == SeatLevel::Global {
        if let Some(host) = seat
            .project_id
            .and_then(|id| app.projects.iter().find(|p| p.id == id))
        {
            header.push(Line::from(Span::styled(
                safe(&format!("Host project {}", host.name)),
                Style::default().fg(theme::text()).bg(bg),
            )));
        }
    }
    if let Some(caps) = state.selected_cap_line() {
        header.push(facts_line(&caps, bg));
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
pub(super) fn wrapped_height(lines: &[Line<'static>], width: u16) -> u16 {
    let rows = Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width.max(1));
    u16::try_from(rows).unwrap_or(u16::MAX)
}

/// A signal band: the accent rail down its left edge, `lines` beside it on the
/// selected-row band (the session list's selected-signal panel).
pub(super) fn render_band(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
    if area.height == 0 || area.width < 3 {
        return;
    }
    let bg = theme::selected_row_bg();
    session::fill_area(frame, area, bg);
    frame.render_widget(
        Paragraph::new(vec![Line::from("▌"); usize::from(area.height)]).style(
            Style::default()
                .fg(theme::active_row_rail())
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
        Rect::new(area.x, area.y, 1, area.height),
    );
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .style(Style::default().bg(bg)),
        Rect::new(area.x + 2, area.y, area.width - 2, area.height),
    );
}

/// The key hints for what has focus, `key text · key text`: keys bright, text dim.
fn hint_segments(state: &GlobalManagerWorkspaceState) -> &'static [&'static str] {
    if state.launch.as_ref().is_some_and(|form| form.grant_open) {
        return &[
            "j/k row",
            "h/l change",
            "Space toggle",
            "0-9 type a number",
            "Esc back",
        ];
    }
    if state.launch.is_some() {
        return &[
            "j/k field",
            "h/l change",
            "Space scope",
            "Enter launch",
            "Esc cancel",
        ];
    }
    match state.focus {
        WorkspaceFocus::Input => &["Enter send", "Esc stop typing"],
        WorkspaceFocus::Conversation => &[
            "j/k scroll",
            "Ctrl-D/U half page",
            "g/G ends",
            "i type",
            "Tab back to seats",
            "? keys",
        ],
        WorkspaceFocus::Seats => &[
            "j/k move",
            "Enter open",
            "i type",
            "t talk",
            "n new manager",
            "e coverage & permissions",
            "b budget",
            "s status",
            "x halt",
            "p project",
            "T tree",
            "F fleet",
            "Tab conversation",
            "r refresh",
            "? keys",
            "Esc close",
        ],
    }
}

/// The last row: as many key hints as fit, always ending on `? keys` /
/// `Esc` so the way out and the help stay visible.
fn footer_line(state: &GlobalManagerWorkspaceState, width: u16) -> Line<'static> {
    hint_footer(hint_segments(state), width)
}

/// `segments` as the footer row: keys bright, text dim.
pub(super) fn hint_footer(segments: &[&str], width: u16) -> Line<'static> {
    let tail = segments.len().saturating_sub(2);
    let mut chosen: Vec<&str> = Vec::new();
    // The closing segments (help and the way out) are always kept, so their
    // room is reserved before any other hint is placed.
    let mut used = 1 + segments[tail..]
        .iter()
        .map(|segment| segment.chars().count() + 3)
        .sum::<usize>();
    let fits =
        |used: usize, segment: &str| used + segment.chars().count() + 3 <= usize::from(width);
    for segment in &segments[..tail] {
        if fits(used, segment) {
            used += segment.chars().count() + 3;
            chosen.push(*segment);
        }
    }
    chosen.extend(&segments[tail..]);
    let mut spans = vec![Span::raw(" ")];
    for (index, segment) in chosen.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(
                " · ",
                Style::default().fg(theme::dim_metadata()),
            ));
        }
        let (key, rest) = segment.split_once(' ').unwrap_or((segment, ""));
        spans.push(Span::styled(
            key.to_string(),
            Style::default()
                .fg(theme::subtext1())
                .add_modifier(Modifier::BOLD),
        ));
        if !rest.is_empty() {
            spans.push(Span::styled(
                format!(" {rest}"),
                Style::default().fg(theme::dim_metadata()),
            ));
        }
    }
    Line::from(spans)
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
    // #2502: the Budget panel replaces the console while it is open.
    if let OverlayState::GlobalManagerWorkspace(state) = &app.overlay
        && let Some(panel) = &state.budget
    {
        crate::ui::overlay::gm_budget::render(frame, inner, state, panel);
        return;
    }
    if let OverlayState::GlobalManagerWorkspace(state) = &app.overlay
        && let Some(form) = &state.coverage
    {
        render_coverage(frame, inner, form);
        return;
    }
    let (areas, detail, focus) = {
        let OverlayState::GlobalManagerWorkspace(state) = &app.overlay else {
            return;
        };
        let signal = signal_lines(app, state);
        // One spacer row below the band once a snapshot is shown.
        let signal_height = if signal.is_empty() {
            0
        } else {
            wrapped_height(&signal, inner.width.saturating_sub(2)) + 1
        };
        let areas = console_areas(inner, state, signal_height);
        frame.render_widget(
            Paragraph::new(scope_header(state, areas.header.width)),
            areas.header,
        );
        if areas.signal.height > 0 {
            let band = Rect::new(
                areas.signal.x,
                areas.signal.y,
                areas.signal.width,
                areas.signal.height.saturating_sub(1),
            );
            render_band(frame, band, signal);
        }
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
                    .style(Style::default().fg(theme::browser_decorative_separator())),
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
        if let Some(footer) = areas.footer {
            frame.render_widget(Paragraph::new(footer_line(state, footer.width)), footer);
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
            let header_height = wrapped_height(&header, detail_area.width.saturating_sub(2))
                .min(detail_area.height.saturating_sub(4));
            render_band(
                frame,
                Rect::new(
                    detail_area.x,
                    detail_area.y,
                    detail_area.width,
                    header_height,
                ),
                header,
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
                coverage: self.coverage.clone(),
                target: self.target,
                node: self.node.clone(),
                budget: None,
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
                "Host project dictate-agent",
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
    fn a_status_line_sits_above_the_key_hints_and_appears_only_when_set() {
        let f = fixture();
        let mut app = app_with(&f, state_named(&f, "appointed-notice"));
        let text = screen(&mut app, 200, 50);
        let status = text.lines().nth(47).unwrap();
        assert!(status.contains("appointed (grant v4"), "{status}");
        let hints = text.lines().nth(48).unwrap();
        assert!(
            hints.contains("j/k move") && hints.contains("? keys"),
            "{hints}"
        );
        let mut app = app_with(&f, loaded(rich(&f)));
        let text = screen(&mut app, 200, 50);
        assert!(
            !text.lines().nth(47).unwrap().contains("appointed"),
            "no status row while nothing was said:\n{text}"
        );
        assert!(text.lines().nth(48).unwrap().contains("? keys"), "{text}");
    }

    #[test]
    fn the_list_reads_like_the_session_list() {
        let f = fixture();
        let mut app = app_with(&f, loaded(rich(&f)));
        let text = screen(&mut app, 200, 50);
        for needle in [
            // The scope header: breadcrumb left, count and age right.
            "Above all projects / Seats",
            "7 seats",
            // Section rules with the list's glyphs and counts.
            "★ GLOBAL  1",
            "● PROJECT MANAGERS  6",
            // The selected row's rail and the seat's status glyph.
            "▌✓",
        ] {
            assert!(text.contains(needle), "{needle} in\n{text}");
        }
        // One status glyph per health, from the list's vocabulary.
        for glyph in ["✓", "!", "⏸", "○"] {
            assert!(text.contains(glyph), "{glyph} in\n{text}");
        }
    }

    #[test]
    fn the_selected_row_and_the_signal_band_use_the_lists_selected_band() {
        let f = fixture();
        let mut app = app_with(&f, loaded(rich(&f)));
        let _theme = crate::ui::theme::pin_theme_state();
        let mut terminal = Terminal::new(TestBackend::new(200, 50)).unwrap();
        terminal
            .draw(|frame| crate::ui::overlay::render_overlay(frame, frame.area(), &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let band = crate::ui::theme::selected_row_bg();
        let row_of = |needle: &str| {
            (0..50u16)
                .find(|y| {
                    (0..200u16)
                        .map(|x| buffer[(x, *y)].symbol())
                        .collect::<String>()
                        .contains(needle)
                })
                .unwrap_or_else(|| panic!("a row contains {needle}"))
        };
        // The selected seat row: rail then the band behind its name.
        let seat_row = row_of("▌✓");
        let rail_x = (0..200u16)
            .find(|x| buffer[(*x, seat_row)].symbol() == "▌")
            .unwrap();
        assert_eq!(buffer[(rail_x, seat_row)].bg, band);
        assert_eq!(buffer[(rail_x + 6, seat_row)].bg, band);
        // The grant sits in a signal band with the same rail and band.
        let grant_row = row_of("Grant v3 active");
        assert_eq!(buffer[(rail_x, grant_row)].symbol(), "▌");
        assert_eq!(buffer[(rail_x + 4, grant_row)].bg, band);
        // An unselected row has neither.
        let other = row_of("PAUSED");
        assert_ne!(buffer[(rail_x + 6, other)].bg, band);
    }

    #[test]
    fn the_key_hints_follow_what_has_focus() {
        let f = fixture();
        let mut app = app_with(&f, loaded(rich(&f)));
        let seats = screen(&mut app, 200, 50);
        assert!(
            seats.lines().nth(48).unwrap().contains("Enter open"),
            "{seats}"
        );
        let mut app = app_with(
            &f,
            with_focus(loaded(rich(&f)), WorkspaceFocus::Conversation),
        );
        let conversation = screen(&mut app, 200, 50);
        assert!(
            conversation.lines().nth(48).unwrap().contains("j/k scroll"),
            "{conversation}"
        );
        let mut app = app_with(&f, with_focus(loaded(rich(&f)), WorkspaceFocus::Input));
        let typing = screen(&mut app, 200, 50);
        assert!(
            typing.lines().nth(48).unwrap().contains("Enter send"),
            "{typing}"
        );
        // A narrow terminal drops hints from the middle but keeps the help.
        let mut app = app_with(&f, loaded(rich(&f)));
        let narrow = screen(&mut app, 60, 30);
        let hints = narrow.lines().nth(28).unwrap();
        assert!(
            hints.contains("? keys") && hints.contains("Esc close"),
            "{hints}"
        );
    }

    #[test]
    fn the_launch_form_offers_the_grant_and_its_editor() {
        let f = fixture();
        let mut app = app_with(&f, state_named(&f, "launch-global"));
        let text = screen(&mut app, 200, 50);
        for needle in ["Grant     ", "(defaults)"] {
            assert!(text.contains(needle), "{needle} in\n{text}");
        }
        if let OverlayState::GlobalManagerWorkspace(state) = &mut app.overlay {
            let form = state.launch.as_mut().unwrap();
            form.field = LaunchField::Grant;
            form.open_grant();
        }
        let text = screen(&mut app, 200, 50);
        for needle in [
            "Grant · Global manager",
            "GRANT · ",
            "AUTHORITY",
            "▸ Preset",
            "CAPABILITIES",
            "Esc back",
        ] {
            assert!(text.contains(needle), "{needle} in\n{text}");
        }
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
        let dir = std::path::Path::new(rsi_common::manifest_dir!())
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
        let dir = std::path::Path::new(rsi_common::manifest_dir!())
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
    fn coverage_state(f: &Fixture, step: usize) -> GlobalManagerWorkspaceState {
        let grant = rich(f).grant.unwrap();
        let request =
            crate::overlay::global_manager_command::grant_addition_request(&grant, &[]).unwrap();
        let mut form = crate::overlay::manager_coverage::CoverageForm::new(
            request,
            &[f.a.clone(), f.b.clone()],
        );
        form.new_defaults();
        form.step = step;
        let mut state = loaded(rich(f));
        state.coverage = Some(form);
        state
    }

    #[test]
    fn coverage_form_render_snapshots_keep_steps_groups_and_budget_visible() {
        let f = fixture();
        for (width, height) in [(96, 28), (72, 24), (48, 24)] {
            let mut app = app_with(&f, coverage_state(&f, 1));
            let text = screen(&mut app, width, height);
            for needle in [
                "Manager coverage & permissions",
                "2/5 Permissions",
                "groups ON",
                "[x] Create project groups",
                "Budget chain",
                "Enter next",
            ] {
                assert!(
                    text.contains(needle),
                    "{width}x{height}: {needle} in\n{text}"
                );
            }
            if let OverlayState::GlobalManagerWorkspace(state) = &mut app.overlay {
                let form = state.coverage.as_mut().unwrap();
                form.selected = form.rows().len() - 1;
            }
            let text = screen(&mut app, width, height);
            assert!(
                text.contains("Request daemon deployment"),
                "selected final permission:\n{text}"
            );
            assert!(text.contains("Budget chain"), "pinned budget:\n{text}");
        }
    }

    #[test]
    fn coverage_review_scrolls_with_confirmation_and_budget_pinned() {
        let f = fixture();
        let mut state = coverage_state(&f, 4);
        let form = state.coverage.as_mut().unwrap();
        form.selected = form.rows().len() - 1;
        let mut app = app_with(&f, state);
        let text = screen(&mut app, 48, 24);
        for needle in [
            "5/5 Review",
            "y Save permissions",
            "Esc edit",
            "Budget chain",
            "Advanced policy fields",
        ] {
            assert!(text.contains(needle), "{needle} in\n{text}");
        }
        let text = screen(&mut app, 28, 8);
        assert!(text.contains("Resize"), "{text}");
    }
}
