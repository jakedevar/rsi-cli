//! Global Dashboard (#3317): render the RSI-wide metrics view in the session
//! list's visual language. A scope header (`RSI / Dashboard`, the window, the
//! snapshot age and an `incomplete` marker) sits over a column header; one row
//! per project (accent rail and selected-row band, zero counts dim) with a
//! pinned TOTAL row follows; a detail band for the selected row sits under the
//! table and the last row names the keys. When the terminal is wide enough the
//! top-level manager's live conversation sits on the right, drawn by the shared
//! session-detail renderer, exactly as in the gm console.

use crate::app::App;
use crate::overlay::global_dashboard::{
    Col, DashboardFocus, DashboardSort, GlobalDashboardState, RowView, START_HINT, TopSeat,
    cell_text, fit, layout, live_seat, snapshot_age,
};
use crate::types::OverlayState;
use crate::ui::overlay::global_manager_workspace::{
    hint_footer, render_band, safe, wrapped_height,
};
use crate::ui::theme;
use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph, Wrap};
use rsi_common::global_dashboard::DashboardWindow;
use uuid::Uuid;

/// Below this inner width the pane replaces the table instead of sitting
/// beside it.
const SPLIT_MIN_WIDTH: u16 = 130;

/// The most rows the detail band takes.
const DETAIL_MAX_HEIGHT: u16 = 8;

/// Where each part of the dashboard goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DashAreas {
    pub header: Rect,
    /// The project table, column header included; `None` when the pane has
    /// the whole body.
    pub table: Option<Rect>,
    /// The vertical rule between the table and the pane (split layout only).
    pub rule: Option<Rect>,
    /// The conversation pane, when shown.
    pub pane: Option<Rect>,
    pub status: Option<Rect>,
    pub footer: Option<Rect>,
}

/// The pane's width in the split layout.
#[must_use]
pub(crate) fn pane_width(inner_width: u16) -> u16 {
    (inner_width * 36 / 100).clamp(48, 84)
}

pub(crate) fn areas(inner: Rect, state: &GlobalDashboardState) -> DashAreas {
    let header_height = 1u16.min(inner.height);
    let footer_height = u16::from(inner.height >= 8);
    let has_status = state.error.is_some() || state.notice.is_some();
    let status_height = u16::from(has_status && inner.height > header_height + footer_height + 2);
    let top = header_height;
    let body = Rect::new(
        inner.x,
        inner.y + top,
        inner.width,
        inner
            .height
            .saturating_sub(top + footer_height + status_height),
    );
    let header = Rect::new(inner.x, inner.y, inner.width, header_height);
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
    if !state.pane_visible(inner.width) {
        return DashAreas {
            header,
            table: Some(body),
            rule: None,
            pane: None,
            status,
            footer,
        };
    }
    if inner.width >= SPLIT_MIN_WIDTH {
        let pane_w = pane_width(inner.width);
        let table_w = inner.width - pane_w - 2;
        return DashAreas {
            header,
            table: Some(Rect::new(body.x, body.y, table_w, body.height)),
            rule: Some(Rect::new(body.x + table_w, body.y, 1, body.height)),
            pane: Some(Rect::new(body.x + table_w + 2, body.y, pane_w, body.height)),
            status,
            footer,
        };
    }
    DashAreas {
        header,
        table: None,
        rule: None,
        pane: Some(body),
        status,
        footer,
    }
}

/// The scope header: `RSI / Dashboard` on the left, the window, sort, the
/// snapshot age and the `incomplete` marker on the right.
fn scope_header(state: &GlobalDashboardState, width: u16) -> Line<'static> {
    let sep = Style::default().fg(theme::overlay1());
    let left = vec![
        Span::raw(" "),
        Span::styled(
            "RSI",
            Style::default()
                .fg(theme::green())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" / ", sep),
        Span::styled(
            "Dashboard",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
    ];
    let dim = Style::default().fg(theme::dim_metadata());
    let bold = Style::default()
        .fg(theme::text())
        .add_modifier(Modifier::BOLD);
    let age = match (&state.snapshot, &state.error) {
        (Some(_), Some(_)) => "stale".to_string(),
        (Some(snapshot), None) => format!("as of {}", snapshot_age(snapshot.as_of, Utc::now())),
        (None, _) => "loading".to_string(),
    };
    let window = state.snapshot.as_ref().map_or_else(
        || state.window.label().to_string(),
        |snapshot| snapshot.window.clone(),
    );
    let mut right = vec![
        Span::styled(window, bold),
        Span::styled(" window · ", dim),
        Span::styled(format!("sort {}", state.sort.label()), dim),
        Span::styled(format!(" · {age}"), dim),
    ];
    if state.incomplete() {
        right.push(Span::styled(" · ", dim));
        right.push(Span::styled(
            "incomplete",
            Style::default()
                .fg(theme::warning_status())
                .add_modifier(Modifier::BOLD),
        ));
    }
    right.push(Span::raw(" "));
    let used = Line::from(left.clone()).width() + Line::from(right.clone()).width();
    let pad = usize::from(width).saturating_sub(used);
    let mut spans = left;
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
        spans.extend(right);
    }
    Line::from(spans)
}

fn pm_color(status: Option<&str>) -> Color {
    match status {
        None => theme::dim_metadata(),
        Some("Starting" | "Running") => theme::status_running(),
        Some("WaitingApproval") => theme::status_waiting(),
        Some("Failed" | "Interrupted") => theme::status_failed(),
        Some(_) => theme::status_completed(),
    }
}

/// The style of one cell: zero counts dim, hot counts bold.
fn cell_style(col: Col, row: &RowView<'_>, text: &str) -> Style {
    let dim = Style::default().fg(theme::dim_metadata());
    let count = Style::default().fg(theme::count_text());
    let zero = text.trim() == "0" || text.trim() == "$0" || text.trim() == "-";
    match col {
        Col::Project => {
            if row.is_total {
                Style::default()
                    .fg(theme::table_header_text())
                    .add_modifier(Modifier::BOLD)
            } else if row.is_home {
                Style::default().fg(theme::accent())
            } else {
                Style::default().fg(theme::text())
            }
        }
        Col::Pm => Style::default().fg(pm_color(row.pm_status)),
        Col::Agents => {
            if zero {
                dim
            } else {
                Style::default()
                    .fg(theme::status_running())
                    .add_modifier(Modifier::BOLD)
            }
        }
        Col::Providers => {
            if zero {
                dim
            } else {
                Style::default().fg(theme::model_text())
            }
        }
        Col::Requests => {
            if zero {
                dim
            } else {
                Style::default()
                    .fg(theme::warning_status())
                    .add_modifier(Modifier::BOLD)
            }
        }
        Col::Qa => {
            if row.red_qa > 0 {
                Style::default()
                    .fg(theme::error_status())
                    .add_modifier(Modifier::BOLD)
            } else if zero || text.is_empty() {
                dim
            } else {
                Style::default().fg(theme::status_completed())
            }
        }
        _ => {
            if zero {
                dim
            } else {
                count
            }
        }
    }
}

/// The sort's headline columns, underlined in the column header.
fn sorted_by(sort: DashboardSort, col: Col) -> bool {
    matches!(
        (sort, col),
        (DashboardSort::Agents, Col::Agents)
            | (DashboardSort::Tokens, Col::TokensIn | Col::TokensOut)
            | (DashboardSort::Cost, Col::Cost)
            | (DashboardSort::Issues, Col::Open | Col::InProgress)
    )
}

fn column_header(
    cols: &[(Col, usize)],
    window: DashboardWindow,
    sort: DashboardSort,
) -> Line<'static> {
    let base = Style::default()
        .fg(theme::table_header_text())
        .add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::raw("  ")];
    for (index, (col, width)) in cols.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        let style = if sorted_by(sort, *col) {
            base.add_modifier(Modifier::UNDERLINED)
        } else {
            base
        };
        spans.push(Span::styled(
            fit(col.header(window), *width, col.right_aligned()),
            style,
        ));
    }
    Line::from(spans)
}

/// One row in the session list's shape: accent rail, then the aligned cells;
/// the selected row sits on the list's selected band.
fn row_line(
    cols: &[(Col, usize)],
    row: &RowView<'_>,
    selected: bool,
    focused: bool,
) -> Line<'static> {
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
        Span::styled(" ", on(Style::default())),
    ];
    for (index, (col, width)) in cols.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" ", on(Style::default())));
        }
        let text = safe(&cell_text(*col, row));
        let mut style = cell_style(*col, row, &text);
        if selected && *col == Col::Project {
            style = style.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(
            fit(&text, *width, col.right_aligned()),
            on(style),
        ));
    }
    let line = Line::from(spans);
    match bg {
        Some(bg) => line.style(Style::default().bg(bg)),
        None => line,
    }
}

/// The table: column header, scrolled project rows, a rule and the pinned
/// TOTAL row, then the selected row's detail band.
fn render_table(frame: &mut Frame, area: Rect, state: &GlobalDashboardState) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    if state.snapshot.is_none() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " Loading the dashboard… (r refreshes)",
                Style::default().fg(theme::dim_metadata()),
            ))),
            area,
        );
        return;
    }
    let cols = layout(usize::from(area.width));
    let focused = state.focus == DashboardFocus::Table;
    let rows = state.rows();
    let (total, projects) = rows.split_last().expect("TOTAL row");

    let detail: Vec<Line<'static>> = detail_lines(state);
    let band_height = if detail.is_empty() || area.height < 10 {
        0
    } else {
        wrapped_height(&detail, area.width.saturating_sub(2)).min(DETAIL_MAX_HEIGHT)
    };
    let gap = u16::from(band_height > 0);
    // Column header, the rows, the rule and TOTAL, a gap, then the detail
    // band; the rows scroll when they do not all fit.
    let fixed = 1 + 2 + gap + band_height;
    let rows_height = area.height.saturating_sub(fixed);
    frame.render_widget(
        Paragraph::new(column_header(&cols, state.window, state.sort)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let height = usize::from(rows_height);
    let offset = if state.selected < projects.len() {
        state.selected.saturating_sub(height.saturating_sub(1))
    } else {
        0
    };
    let lines: Vec<Line<'static>> = projects
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(index, row)| row_line(&cols, row, index == state.selected, focused))
        .collect();
    let shown = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(area.x, area.y + 1, area.width, shown),
    );
    let below = area.y + 1 + shown;
    if area.height >= 4 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(usize::from(area.width)),
                Style::default().fg(theme::browser_decorative_separator()),
            ))),
            Rect::new(area.x, below, area.width, 1),
        );
        frame.render_widget(
            Paragraph::new(row_line(
                &cols,
                total,
                state.selected == projects.len(),
                focused,
            )),
            Rect::new(area.x, below + 1, area.width, 1),
        );
    }
    if band_height > 0 {
        render_band(
            frame,
            Rect::new(area.x, below + 2 + gap, area.width, band_height),
            detail,
        );
    }
}

/// The detail band's lines, the first one bold.
fn detail_lines(state: &GlobalDashboardState) -> Vec<Line<'static>> {
    let bg = theme::selected_row_bg();
    state
        .detail_lines()
        .into_iter()
        .enumerate()
        .map(|(index, text)| {
            let style = if index == 0 {
                Style::default()
                    .fg(theme::text())
                    .bg(bg)
                    .add_modifier(Modifier::BOLD)
            } else if text.starts_with("Incomplete") || text.contains(" red") {
                Style::default().fg(theme::warning_status()).bg(bg)
            } else {
                Style::default().fg(theme::text()).bg(bg)
            };
            Line::from(Span::styled(safe(&text), style))
        })
        .collect()
}

/// What the pane shows.
enum Pane {
    Empty(Vec<Line<'static>>),
    Conversation {
        session_id: Uuid,
        header: Vec<Line<'static>>,
        loaded: bool,
    },
}

fn pane(app: &App, state: &GlobalDashboardState) -> Pane {
    let bg = theme::selected_row_bg();
    let text = Style::default().fg(theme::text());
    let dim = Style::default().fg(theme::dim_metadata());
    match (state.seat, live_seat(app)) {
        (_, Some(session_id)) => {
            let session = app.sessions.get(&session_id).map(|s| &s.session);
            let status =
                session.map_or_else(|| "loading".to_string(), |s| format!("{:?}", s.status));
            let provider = session.map_or_else(String::new, |s| format!(" · {:?}", s.provider));
            Pane::Conversation {
                session_id,
                header: vec![Line::from(Span::styled(
                    format!("Top-level manager · {status}{provider}"),
                    Style::default()
                        .fg(theme::text())
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                ))],
                loaded: app.sessions.contains_key(&session_id),
            }
        }
        // Not read yet, or the seat's session is still being fetched.
        (TopSeat::Unknown, None) => Pane::Empty(vec![Line::from(Span::styled(
            "Loading the top-level manager…",
            dim,
        ))]),
        (TopSeat::Seat(id), None) if !app.sessions.contains_key(&id) => {
            Pane::Empty(vec![Line::from(Span::styled(
                "Loading the top-level manager…",
                dim,
            ))])
        }
        _ => Pane::Empty(vec![
            Line::from(Span::styled(
                "No top-level manager is live.",
                text.add_modifier(Modifier::BOLD),
            )),
            Line::default(),
            Line::from(Span::styled(
                format!(
                    "Press n to start one: it opens the appoint flow, hosted in the home project when there is one. ({START_HINT})"
                ),
                text,
            )),
            Line::default(),
            Line::from(Span::styled(
                "Once it is live, i types to it here and Enter sends, the same as in the gm console.",
                dim,
            )),
        ]),
    }
}

fn hint_segments(state: &GlobalDashboardState) -> &'static [&'static str] {
    match state.focus {
        DashboardFocus::Input => &["Enter send", "Esc stop typing"],
        DashboardFocus::Conversation => &[
            "j/k scroll",
            "Ctrl-D/U half page",
            "g/G ends",
            "i type",
            "Tab back to table",
            "? keys",
            "Esc table",
        ],
        DashboardFocus::Table => &[
            "j/k move",
            "Enter open project",
            "w window",
            "s sort",
            "t pane",
            "i type",
            "n new manager",
            "gm managers",
            "r refresh",
            "? keys",
            "Esc close",
        ],
    }
}

/// Render the dashboard. Takes `&mut App` because the conversation pane
/// updates the shown session's per-frame view state, like the detail pane.
pub(super) fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    frame.render_widget(Clear, area);
    let block = theme::overlay_block()
        .title(Line::from(Span::styled(
            " Global dashboard ",
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
    if let OverlayState::GlobalDashboard(state) = &mut app.overlay {
        state.inner_width = inner.width;
    }
    let (areas, pane, focus) = {
        let OverlayState::GlobalDashboard(state) = &app.overlay else {
            return;
        };
        let areas = areas(inner, state);
        frame.render_widget(
            Paragraph::new(scope_header(state, areas.header.width)),
            areas.header,
        );
        if let Some(table) = areas.table {
            render_table(frame, table, state);
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
            frame.render_widget(
                Paragraph::new(hint_footer(hint_segments(state), footer.width)),
                footer,
            );
        }
        (areas, pane(app, state), state.focus)
    };
    let Some(pane_area) = areas.pane else {
        return;
    };
    match pane {
        Pane::Empty(lines) => {
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), pane_area);
        }
        Pane::Conversation {
            session_id,
            header,
            loaded,
        } => {
            let header_height = wrapped_height(&header, pane_area.width.saturating_sub(2))
                .min(pane_area.height.saturating_sub(4));
            render_band(
                frame,
                Rect::new(pane_area.x, pane_area.y, pane_area.width, header_height),
                header,
            );
            let body = Rect::new(
                pane_area.x,
                pane_area.y + header_height,
                pane_area.width,
                pane_area.height.saturating_sub(header_height),
            );
            if !loaded {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        "Loading the top-level manager's conversation… (r refreshes)",
                        Style::default().fg(theme::dim_metadata()),
                    ))),
                    body,
                );
                return;
            }
            let focused = focus != DashboardFocus::Table;
            let column = crate::ui::prepare_session_detail(app, session_id, body);
            crate::ui::render_session_conversation(frame, &column, session_id, focused, app);
        }
    }
}

#[cfg(test)]
#[path = "global_dashboard_tests.rs"]
mod tests;
