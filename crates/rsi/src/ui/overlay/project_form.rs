//! Project form rendering.

use crate::overlay::PROJECT_COLORS;
use crate::ui::{parse_hex_color, theme};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;

/// Render the project form popup (create/edit).
#[allow(clippy::too_many_arguments)]
pub(super) fn render_project_form(
    frame: &mut Frame,
    area: Rect,
    focused_field: usize,
    name: &str,
    path: &str,
    color_index: usize,
    is_editing: bool,
    workflow_status: Option<&serde_json::Value>,
) {
    // Expand popup height by 1 if we have a workflow status to show
    let has_workflow = workflow_status
        .map(|s| s.get("exists").and_then(|v| v.as_bool()).unwrap_or(false))
        .unwrap_or(false);
    let popup_height: u16 = if has_workflow { 13 } else { 12 };
    let popup_area = fixed_centered_rect(area, 55, popup_height);

    frame.render_widget(Clear, popup_area);

    let title = if is_editing {
        " Edit Project "
    } else {
        " New Project "
    };

    let block = theme::overlay_block()
        .title(Line::from(vec![Span::styled(
            title,
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )]))
        .padding(Padding::horizontal(1));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if inner.height < 6 {
        return;
    }

    let fields = [("Name", name), ("Path", path)];

    for (i, (label, value)) in fields.iter().enumerate() {
        let row_y = inner.y + i as u16;
        let is_focused = i == focused_field;

        let label_style = if is_focused {
            Style::default()
                .fg(theme::mauve())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::overlay_hint())
        };

        let cursor = if is_focused { "█" } else { "" };
        let line = Line::from(vec![
            Span::styled(format!("{:>6}: ", label), label_style),
            Span::styled(value.to_string(), Style::default().fg(theme::text())),
            Span::styled(cursor, Style::default().fg(theme::text())),
        ]);
        let row_area = Rect::new(inner.x, row_y, inner.width, 1);
        frame.render_widget(Paragraph::new(line), row_area);
    }

    // Color field (row 2)
    let color_y = inner.y + 2;
    let is_color_focused = focused_field == 2;
    let color_label_style = if is_color_focused {
        Style::default()
            .fg(theme::mauve())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::overlay_hint())
    };

    let safe_index = color_index.min(PROJECT_COLORS.len().saturating_sub(1));
    let (color_name, color_hex) = PROJECT_COLORS[safe_index];
    let color_val = parse_hex_color(color_hex).unwrap_or(theme::blue());

    let color_line = Line::from(vec![
        Span::styled(" Color: ", color_label_style),
        Span::styled("■ ", Style::default().fg(color_val)),
        Span::styled(color_name, Style::default().fg(theme::text())),
        if is_color_focused {
            Span::styled("  ◄ ►", Style::default().fg(theme::overlay_hint()))
        } else {
            Span::raw("")
        },
    ]);
    let color_area = Rect::new(inner.x, color_y, inner.width, 1);
    frame.render_widget(Paragraph::new(color_line), color_area);

    // FLYWHEEL.md status line (only for existing projects that have a workflow status)
    let hint_offset: u16 = if has_workflow {
        if let Some(status) = workflow_status {
            let healthy = status
                .get("healthy")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let last_error = status
                .get("last_error")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let loaded_at = status
                .get("loaded_at")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let rsi_y = inner.y + 3;
            let rsi_area = Rect::new(inner.x, rsi_y, inner.width, 1);

            let rsi_line = if healthy {
                // Truncate loaded_at to just date+time (drop sub-second and timezone)
                let ts = if loaded_at.len() >= 19 {
                    &loaded_at[..19]
                } else {
                    loaded_at
                };
                Line::from(vec![
                    Span::styled("MOTHERSHIP: ", Style::default().fg(theme::overlay_hint())),
                    Span::styled("loaded ", Style::default().fg(theme::green())),
                    Span::styled(ts, Style::default().fg(theme::overlay_hint())),
                ])
            } else {
                // Show first ~40 chars of error (keep the line short)
                let err_display = if last_error.len() > 40 {
                    format!("{}...", &last_error[..40])
                } else {
                    last_error.to_string()
                };
                Line::from(vec![
                    Span::styled("MOTHERSHIP: ", Style::default().fg(theme::overlay_hint())),
                    Span::styled(err_display, Style::default().fg(theme::yellow())),
                ])
            };
            frame.render_widget(Paragraph::new(rsi_line), rsi_area);
        }
        1 // pushed hint bar down by 1 row
    } else {
        0
    };

    // Hint bar at the bottom
    let hint_y = inner.y + inner.height - 1;
    let _ = hint_offset; // used via popup_height calculation above
    let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
    let hint = Line::from(vec![Span::styled(
        "Tab: next field  Enter: save  Esc: cancel",
        Style::default().fg(theme::overlay_hint()),
    )]);
    frame.render_widget(Paragraph::new(hint), hint_area);
}
