use crate::overlay::manager_v2::{
    ManagerSection, ManagerSurface, board,
    catalog::{ManagerLaunchPickerState, PickerStage},
    policy,
};
use crate::ui::theme;
use ratatui::{
    prelude::*,
    widgets::{Clear, List, ListItem, ListState, Padding, Paragraph, Wrap},
};

pub(super) fn render(frame: &mut Frame, area: Rect, view: &ManagerSurface) {
    let popup = super::fixed_centered_rect(area, 120, area.height.saturating_sub(2).max(8));
    frame.render_widget(Clear, popup);
    let block = theme::overlay_block()
        .title(" Manager · operator ")
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 6 {
        return;
    }
    let layout = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(inner);
    let tabs = [
        ManagerSection::Board,
        ManagerSection::Decisions,
        ManagerSection::Inbox,
        ManagerSection::Inspect(rsi_common::harness_manager_v2::ManagerInspectSectionV2::Workers),
        ManagerSection::Policy,
    ]
    .iter()
    .enumerate()
    .map(|(i, s)| {
        let label = match i {
            0 => "1 Board",
            1 => "2 Decisions",
            2 => "3 Inbox",
            3 => "4 Inspect",
            _ => "5 Policy",
        };
        if *s == view.section
            || matches!(
                (*s, view.section),
                (ManagerSection::Inspect(_), ManagerSection::Inspect(_))
            )
        {
            format!("[{label}]")
        } else {
            label.to_string()
        }
    })
    .collect::<Vec<_>>()
    .join("  ");
    frame.render_widget(Paragraph::new(tabs).style(hints()), layout[0]);
    match view.section {
        ManagerSection::Policy => {
            if let Some(state) = &view.policy {
                render_policy(frame, layout[1], state);
            }
        }
        ManagerSection::Board if view.ledger.composite.is_some() => {
            render_composite_board(frame, layout[1], view);
        }
        section => render_board(frame, layout[1], &view.ledger, section),
    }
}
fn hints() -> Style {
    Style::default().fg(theme::overlay_hint())
}
fn selection() -> Style {
    Style::default().bg(theme::surface2()).bold()
}
/// Band header accent; theme role functions only.
fn band_color(band: board::Band) -> Color {
    match band {
        board::Band::Decisions => theme::status_waiting(),
        board::Band::Requests => theme::status_running(),
        board::Band::Leads => theme::teal(),
        board::Band::Health => theme::status_stalled(),
        board::Band::Signals => theme::warning_status(),
    }
}

/// Identity and status fields are separate, individually bounded rows: a long
/// identity wraps (at most two rows) without ever pushing the status line, and
/// coverage leads the status line so it stays visible even when that line
/// itself wraps on a narrow terminal.
fn board_header(
    view: &ManagerSurface,
    policy: &str,
    width: u16,
) -> (Paragraph<'static>, u16, Paragraph<'static>, u16) {
    let state = &view.ledger;
    let identity = Paragraph::new(Line::from(Span::styled(
        state.identity.clone(),
        Style::default().bold(),
    )))
    .wrap(Wrap { trim: false });
    let identity_rows = u16::try_from(identity.line_count(width).clamp(1, 2)).unwrap_or(2);
    // #669: the seat state leads the status line so a down manager stays
    // visible even when the line wraps on a narrow terminal.
    let mut spans = Vec::new();
    if let Some((seat, down)) = state.seat_label() {
        let style = if down {
            Style::default().fg(theme::error_status()).bold()
        } else {
            hints()
        };
        spans.push(Span::styled(seat, style));
        spans.push(Span::styled(" · ", hints()));
    }
    spans.push(Span::styled(
        format!(
            "coverage {} · scope {:?} · {} Epics · {policy} · {}",
            state.coverage_label(),
            view.config.scope_mode,
            view.config.epic_ids.len(),
            state.inspection.observed_at.format("%Y-%m-%d %H:%M:%S UTC"),
        ),
        hints(),
    ));
    let status = Paragraph::new(Line::from(spans)).wrap(Wrap { trim: false });
    let status_rows = u16::try_from(status.line_count(width).clamp(1, 3)).unwrap_or(3);
    (identity, identity_rows, status, status_rows)
}

fn render_composite_board(frame: &mut Frame, area: Rect, view: &ManagerSurface) {
    let state = &view.ledger;
    let kpis = state.kpi_lines();
    let policy = match &state.inspection.policy {
        None => "policy none".to_string(),
        Some(p) if p.revoked => "policy revoked".to_string(),
        Some(p) if p.policy.paused => format!("policy {:?} (paused)", p.policy.mode),
        Some(p) => format!("policy {:?}", p.policy.mode),
    };
    let (identity, identity_rows, status, status_rows) = board_header(view, &policy, area.width);
    let parts = Layout::vertical([
        Constraint::Length(identity_rows),
        Constraint::Length(status_rows),
        Constraint::Length(u16::try_from(kpis.len().clamp(1, 3)).unwrap_or(3)),
        Constraint::Min(2),
        Constraint::Length(4),
        Constraint::Length(1),
    ])
    .split(area);
    frame.render_widget(identity, parts[0]);
    frame.render_widget(status, parts[1]);
    let parts = [parts[0], parts[2], parts[3], parts[4], parts[5]];
    frame.render_widget(
        Paragraph::new(kpis.join("\n")).style(Style::default().fg(theme::text())),
        parts[1],
    );
    let columns = Layout::horizontal([Constraint::Percentage(46), Constraint::Percentage(54)])
        .split(parts[2]);
    let mut items = Vec::new();
    let mut selected_item = None;
    let mut entry = 0usize;
    for view in state.bands() {
        let page = state.origin_page(view.band.source());
        items.push(ListItem::new(Line::from(Span::styled(
            format!(
                "{} {}{}",
                view.band.label(),
                view.total,
                if view.more_pages { "+" } else { "" }
            ),
            Style::default().fg(band_color(view.band)).bold(),
        ))));
        if view.total == 0 && !view.more_pages {
            items.push(ListItem::new("  none on this page").style(hints()));
        }
        for index in &view.rows {
            if entry == state.selected {
                selected_item = Some(items.len());
            }
            entry += 1;
            let label = page
                .and_then(|p| p.rows.get(*index))
                .map(|row| board::band_row_label(view.band, row))
                .unwrap_or_default();
            items.push(ListItem::new(format!("  {label}")));
        }
        if view.truncated() {
            if entry == state.selected {
                selected_item = Some(items.len());
            }
            entry += 1;
            items.push(ListItem::new(format!("  {}", view.band.more_hint())).style(hints()));
        }
    }
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(selection())
            .highlight_symbol("› "),
        columns[0],
        &mut ListState::default().with_selected(selected_item),
    );
    if let Some(row) = state.selected_row() {
        frame.render_widget(
            Paragraph::new(board::row_details(row).join("\n"))
                .scroll((state.detail_scroll, 0))
                .wrap(Wrap { trim: false }),
            columns[1],
        );
    }
    render_answer_and_message(frame, parts[3], state);
    frame.render_widget(
        Paragraph::new("1-5/Tab section · j/k row · Enter full section · a answer · o session · r refresh · PgUp/PgDn detail · Esc close")
            .style(hints()),
        parts[4],
    );
}

fn render_answer_and_message(frame: &mut Frame, area: Rect, state: &board::BoardState) {
    let bottom = Layout::vertical([Constraint::Length(2), Constraint::Length(2)]).split(area);
    if let Some(draft) = &state.answer {
        // The full question is in the scrollable details. Keep the target and
        // answer separate from errors even when the question is very long.
        frame.render_widget(
            Paragraph::new(format!(
                "{} · version {} · {}\nAnswer: {}▏",
                draft.target.decision_key,
                draft.target.expected_row_version,
                draft.question,
                draft.target.answer,
            )),
            bottom[0],
        );
    }
    let message = state.error.as_deref().unwrap_or_else(|| {
        if state.answer.is_some() {
            "Enter submits to this exact target · Esc leaves pending"
        } else {
            &state.notice
        }
    });
    frame.render_widget(
        Paragraph::new(message)
            .wrap(Wrap { trim: false })
            .style(if state.error.is_some() {
                Style::default().fg(theme::red())
            } else {
                hints()
            }),
        bottom[1],
    );
}

fn render_policy(frame: &mut Frame, area: Rect, state: &policy::PolicyState) {
    let parts = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(2),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(state.identity.as_str()).style(Style::default().bold()),
        parts[0],
    );
    frame.render_widget(
        Paragraph::new(state.saved_summary()).style(hints()),
        parts[1],
    );
    frame.render_widget(
        Paragraph::new(state.draft_summary())
            .style(hints())
            .wrap(Wrap { trim: false }),
        parts[2],
    );
    frame.render_widget(
        Paragraph::new(state.usage_summary()).style(Style::default().fg(theme::accent())),
        parts[3],
    );
    let rows = state.rows();
    let (items, selected) = policy_items(&rows, state.selected);
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(selection())
            .highlight_symbol("› "),
        parts[4],
        &mut ListState::default().with_selected(selected),
    );
    // One line: the edit buffer while editing, else the selected row's
    // description, range and how Enter acts on it.
    let detail = if let Some((_, text)) = &state.edit {
        Line::from(format!("Edit: {text}▏"))
    } else {
        rows.get(state.selected)
            .map(|row| Line::from(policy_row_help(row)).style(hints()))
            .unwrap_or_default()
    };
    frame.render_widget(Paragraph::new(detail), parts[5]);
    let message = state.error.as_deref().unwrap_or_else(|| {
        if state.edit.is_some() {
            "Enter apply · Esc cancel · Ctrl-u clear; blank removes optional limits"
        } else {
            &state.notice
        }
    });
    frame.render_widget(
        Paragraph::new(message)
            .style(if state.error.is_some() {
                Style::default().fg(theme::red())
            } else {
                hints()
            })
            .wrap(Wrap { trim: false }),
        parts[6],
    );
    frame.render_widget(
        Paragraph::new(if state.saved.as_ref().is_some_and(|p| p.revoked) {
            "j/k Tab move · Enter edit · s save + regrant · r discard/reload · * unsaved · Esc close"
        } else {
            "j/k Tab move · Enter/Space edit · s save · r discard/reload · * unsaved · Esc close"
        })
        .style(hints()),
        parts[7],
    );
    if let Some(picker) = &state.picker {
        render_policy_picker(frame, area, picker);
    }
}

/// Widest label column; longer labels (launch choices) extend past it.
const POLICY_LABEL_WIDTH: usize = 30;

/// Section headers are interleaved at render time only, so `rows()` stays
/// the selectable list; returns the display index of the selected row.
fn policy_items(
    rows: &[policy::PolicyRow],
    selected: usize,
) -> (Vec<ListItem<'static>>, Option<usize>) {
    let width = rows
        .iter()
        .map(|r| r.label.chars().count())
        .filter(|n| *n <= POLICY_LABEL_WIDTH)
        .max()
        .unwrap_or(0);
    let selected = selected.min(rows.len().saturating_sub(1));
    let mut items = Vec::with_capacity(rows.len() + 8);
    let mut shown = None;
    let mut section = None;
    for (i, row) in rows.iter().enumerate() {
        if section != Some(row.section) {
            section = Some(row.section);
            items.push(ListItem::new(Line::from(Span::styled(
                format!("── {} ──", row.section.title()),
                Style::default().fg(theme::accent()).bold(),
            ))));
        }
        if i == selected {
            shown = Some(items.len());
        }
        let read_only = row.kind == policy::RowKind::ReadOnly;
        let mut spans = vec![
            Span::styled(
                if row.modified { "*" } else { " " },
                Style::default().fg(theme::yellow()).bold(),
            ),
            Span::styled(
                format!("{:>width$}: ", row.label),
                if read_only { hints() } else { Style::default() },
            ),
            Span::styled(
                row.value.clone(),
                if read_only {
                    hints()
                } else {
                    Style::default().bold()
                },
            ),
        ];
        if let Some(saved) = &row.saved {
            spans.push(Span::styled(format!(" (saved {saved})"), hints()));
        }
        if let Some(error) = &row.error {
            spans.push(Span::styled(
                format!("  ✗ {error}"),
                Style::default().fg(theme::red()),
            ));
        }
        items.push(ListItem::new(Line::from(spans)));
    }
    (items, (!rows.is_empty()).then_some(shown.unwrap_or(0)))
}

fn policy_row_help(row: &policy::PolicyRow) -> String {
    let action = match row.kind {
        policy::RowKind::Edit => "Enter edits",
        policy::RowKind::Toggle => "Enter toggles",
        policy::RowKind::Action => "Enter runs",
        policy::RowKind::ReadOnly => "read-only",
    };
    let range = policy::numeric_bounds(row.field)
        .map(|(min, max)| format!(" · range {min}–{max}"))
        .unwrap_or_default();
    // Action and range first so a narrow pane truncates the prose, not them.
    format!("{action}{range} · {}", row.description)
}

fn render_policy_picker(frame: &mut Frame, area: Rect, picker: &ManagerLaunchPickerState) {
    frame.render_widget(Clear, area);
    let block = theme::overlay_block()
        .title(" Exact allowed launch · draft only ")
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let parts = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(inner);
    let retained = picker
        .original
        .as_ref()
        .map(|c| {
            format!(
                "Retained: {:?}/{} · {} (unchanged until confirmation)",
                c.provider,
                c.model,
                c.effort
                    .as_deref()
                    .unwrap_or("Default — no explicit effort")
            )
        })
        .unwrap_or_else(|| "Add exact restriction · Esc keeps the entire policy draft".into());
    frame.render_widget(
        Paragraph::new(retained).wrap(Wrap { trim: false }),
        parts[0],
    );
    frame.render_widget(
        Paragraph::new(picker.status_label())
            .wrap(Wrap { trim: false })
            .style(hints()),
        parts[1],
    );
    match picker.stage {
        PickerStage::Model => crate::ui::widget::model_dropdown::render_model_dropdown(
            frame,
            parts[2],
            Rect::new(parts[2].x, parts[2].y, parts[2].width, 0),
            &picker.model_dropdown,
            picker
                .original
                .as_ref()
                .filter(|c| c.provider == picker.model_dropdown.provider)
                .map(|c| c.model.as_str()),
            true,
        ),
        PickerStage::Effort => {
            let mut lines = vec![ListItem::new("Default (no explicit effort)")];
            lines.extend(
                picker
                    .effort_choices
                    .values()
                    .into_iter()
                    .skip(1)
                    .map(|value| ListItem::new(value.unwrap_or_default())),
            );
            frame.render_stateful_widget(
                List::new(lines)
                    .highlight_style(selection())
                    .highlight_symbol("› "),
                parts[2],
                &mut ListState::default().with_selected(Some(picker.effort_selected)),
            );
        }
    }
    let message = picker.error.clone().unwrap_or_else(|| if picker.stage == PickerStage::Effort { format!("{} · {}", picker.candidate.as_ref().map(|c| format!("{:?}/{}", c.provider, c.model)).unwrap_or_default(), picker.effort_choices.description()) } else { "Tab/Shift-Tab provider · j/k model · Enter choose model; a second Enter confirms effort. Custom endpoints use no inferred Local tuple.".into() });
    frame.render_widget(
        Paragraph::new(message)
            .wrap(Wrap { trim: false })
            .style(if picker.error.is_some() {
                Style::default().fg(theme::red())
            } else {
                hints()
            }),
        parts[3],
    );
    frame.render_widget(
        Paragraph::new("Esc cancel · r refresh · j/k choose · Enter confirm · Backspace models")
            .style(hints()),
        parts[4],
    );
}

fn render_board(frame: &mut Frame, area: Rect, state: &board::BoardState, section: ManagerSection) {
    let parts = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(2),
        Constraint::Length(4),
        Constraint::Length(1),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(state.identity.as_str()).style(Style::default().bold()),
        parts[0],
    );
    let section_name = match section {
        ManagerSection::Board => "Board",
        ManagerSection::Decisions => "Decisions",
        ManagerSection::Inbox => "Inbox",
        ManagerSection::Inspect(_) => "Inspect",
        ManagerSection::Policy => "Policy",
    };
    let header = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(parts[1]);
    frame.render_widget(
        Paragraph::new(format!(
            "{section_name} · {}",
            if let ManagerSection::Inspect(s) = section {
                format!("{s:?} · [ ] section")
            } else {
                String::new()
            }
        ))
        .style(hints()),
        header[0],
    );
    frame.render_widget(
        Paragraph::new(format!(
            "Page {} · {} · {}",
            state.previous.len() + 1,
            if state.inspection.next_cursor.is_some() {
                "more pages"
            } else if state.inspection.complete {
                "traversal complete; evidence separate"
            } else {
                "partial coverage / unknown remainder"
            },
            state.inspection.observed_at.format("%Y-%m-%d %H:%M:%S UTC"),
        ))
        .style(hints()),
        header[1],
    );
    let columns = Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(parts[2]);
    if state.inspection.rows.is_empty() {
        frame.render_widget(
            Paragraph::new("No records on this page. Follow n when more pages are available.")
                .wrap(Wrap { trim: true }),
            columns[0],
        );
    } else {
        frame.render_stateful_widget(
            List::new(
                state
                    .inspection
                    .rows
                    .iter()
                    .map(|r| ListItem::new(board::row_title(r))),
            )
            .highlight_style(selection())
            .highlight_symbol("› "),
            columns[0],
            &mut ListState::default().with_selected(Some(state.selected)),
        );
    }
    if let Some(row) = state.inspection.rows.get(state.selected) {
        frame.render_widget(
            Paragraph::new(board::row_details(row).join("\n"))
                .scroll((state.detail_scroll, 0))
                .wrap(Wrap { trim: false }),
            columns[1],
        );
    }
    render_answer_and_message(frame, parts[3], state);
    frame.render_widget(Paragraph::new("1-5 section · Tab/Shift-Tab · [ ] inspect · j/k row · n/p page · PgUp/PgDn detail · r refresh · a answer · o session · Esc close").style(hints()),parts[4]);
}
