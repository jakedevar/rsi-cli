//! Session info panel rendering — consolidated, read-only view of session
//! metadata reachable via `F3`. Mirrors `keybindings_help.rs`'s
//! `fixed_centered_rect` + block pattern; background uses T1's tier
//! vocabulary directly (`tier_panel` for the body, `tier_raised` for the
//! footer hint strip) rather than the generic `overlay_block()`.

use crate::app::App;
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};

use super::fixed_centered_rect;

const PANEL_WIDTH: u16 = 96;
const PANEL_MIN_HEIGHT: u16 = 22;

fn field_line(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label}: "),
            Style::default().fg(theme::dim_metadata()),
        ),
        Span::styled(value, Style::default().fg(theme::text())),
    ])
}

fn short_id(id: uuid::Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

fn session_info_lines(app: &App, state: &crate::types::SessionState) -> Vec<Line<'static>> {
    let session = &state.session;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(24);
    lines.push(field_line(
        "title",
        session
            .title
            .clone()
            .unwrap_or_else(|| "(untitled)".to_string()),
    ));
    lines.push(field_line("id", session.id.to_string()));
    lines.push(field_line("kind", format!("{:?}", session.session_kind)));
    lines.push(field_line(
        "status",
        crate::ui::session::status_text(session.status).to_string(),
    ));
    lines.push(field_line("provider", format!("{:?}", session.provider)));
    lines.push(field_line(
        "model",
        session
            .model
            .clone()
            .unwrap_or_else(|| "(none)".to_string()),
    ));
    lines.push(field_line("dir", session.working_dir.display().to_string()));

    let project_name = session
        .project_id
        .and_then(|pid| app.projects.iter().find(|p| p.id == pid))
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "none".to_string());
    lines.push(field_line("project", project_name));
    lines.push(field_line(
        "created",
        session
            .created_at
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
    ));

    let context_rows = crate::types::compute_context_budget_view(state).detail_rows();
    if context_rows.is_empty() {
        lines.push(field_line("context", "n/a".to_string()));
    } else {
        for row in context_rows {
            lines.push(field_line(row.label, row.value));
        }
    }

    if let Some(parent_id) = session.parent_id {
        lines.push(field_line("parent", short_id(parent_id)));
    }
    if rsi_common::types::is_container_kind(session.session_kind)
        && let Some(lead_id) = session.lead_session_id
    {
        lines.push(field_line("lead", short_id(lead_id)));
    }
    if let Some(ref sandbox_root) = session.sandbox_root {
        lines.push(field_line("sandbox", sandbox_root.display().to_string()));
    }

    let rating_str = session
        .rating
        .map(|r| format!("{r}/10"))
        .unwrap_or_else(|| "unrated".to_string());
    lines.push(field_line("rating", rating_str));

    let label_name = session
        .group_id
        .and_then(|gid| app.labels.iter().find(|l| l.id == gid))
        .map(|l| l.name.clone())
        .unwrap_or_else(|| "none".to_string());
    lines.push(field_line("label", label_name));
    lines.push(field_line(
        "tags",
        if session.tags.is_empty() {
            "none".to_string()
        } else {
            session.tags.join(", ")
        },
    ));
    if let Some(ref description) = session.description {
        lines.push(field_line("description", description.clone()));
    }

    lines
}

/// Render the session info panel for `session_id`.
pub(super) fn render_session_info_panel(
    frame: &mut Frame,
    area: Rect,
    session_id: uuid::Uuid,
    app: &App,
) {
    let lines = app
        .sessions
        .get(&session_id)
        .map(|state| session_info_lines(app, state));
    let panel_height = lines
        .as_ref()
        .map(|lines| (lines.len() as u16).saturating_add(4))
        .unwrap_or(PANEL_MIN_HEIGHT)
        .max(PANEL_MIN_HEIGHT);
    let popup_area = fixed_centered_rect(area, PANEL_WIDTH, panel_height);
    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(theme::overlay_border()))
        .style(Style::default().bg(theme::structural_bg(theme::tier_panel())))
        .title(Line::from(vec![Span::styled(
            " Session Info ",
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )]))
        .padding(Padding::horizontal(1));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if inner.height == 0 {
        return;
    }

    let Some(lines) = lines else {
        frame.render_widget(
            Paragraph::new("Session no longer available. Press Esc to close.")
                .style(Style::default().fg(theme::empty_state())),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        return;
    };

    // Reserve the last row for the hint strip and leave one blank separator
    // row above it.
    let content_rows = inner.height.saturating_sub(2) as usize;
    for (row_idx, line) in lines.iter().take(content_rows).enumerate() {
        let row_area = Rect::new(inner.x, inner.y + row_idx as u16, inner.width, 1);
        frame.render_widget(Paragraph::new(line.clone()), row_area);
    }

    let hint_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
    let hint_line = Line::from(vec![Span::styled(
        "R: rate   G: label   Esc: close",
        Style::default().fg(theme::overlay_hint()),
    )]);
    frame.render_widget(
        Paragraph::new(hint_line)
            .style(Style::default().bg(theme::structural_bg(theme::tier_raised()))),
        hint_area,
    );
}
