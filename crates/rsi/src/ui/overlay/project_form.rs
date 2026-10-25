//! Project form rendering.

use crate::overlay::PROJECT_COLORS;
use crate::ui::{parse_hex_color, theme};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;

/// The popup is this wide; its inner width (borders and padding off) sets how
/// far a long name or path runs before it wraps.
const POPUP_WIDTH: u16 = 55;
const NAME_MAX_ROWS: u16 = 3;
const PATH_MAX_ROWS: u16 = 4;

/// Render the project form popup (create/edit).
#[allow(clippy::too_many_arguments)]
pub(super) fn render_project_form(
    frame: &mut Frame,
    area: Rect,
    focused_field: usize,
    name: &str,
    path: &str,
    text: &crate::form_text::FormText,
    color_index: usize,
    is_editing: bool,
    workflow_status: Option<&serde_json::Value>,
) {
    use crate::ui::form_field::{Field, field_rows, mode_pill, render_field};

    // Expand popup height by 1 if we have a workflow status to show
    let has_workflow = workflow_status
        .map(|s| s.get("exists").and_then(|v| v.as_bool()).unwrap_or(false))
        .unwrap_or(false);
    let inner_width = POPUP_WIDTH.saturating_sub(4);
    let label_style = |focused: bool| {
        if focused {
            Style::default()
                .fg(theme::mauve())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::overlay_hint())
        }
    };
    let fields = [
        Field {
            label: format!("{:>6}: ", "Name"),
            label_style: label_style(focused_field == 0),
            value: name,
            surface: (focused_field == 0).then(|| text.surface_for(0)).flatten(),
            min_rows: 1,
            max_rows: NAME_MAX_ROWS,
        },
        Field {
            label: format!("{:>6}: ", "Path"),
            label_style: label_style(focused_field == 1),
            value: path,
            surface: (focused_field == 1).then(|| text.surface_for(1)).flatten(),
            min_rows: 1,
            max_rows: PATH_MAX_ROWS,
        },
    ];
    let name_rows = field_rows(&fields[0], inner_width);
    let path_rows = field_rows(&fields[1], inner_width);
    // Name, path, color, an optional workflow line, a spacer and the hint bar,
    // inside the two border rows.
    let wanted = 2 + name_rows + path_rows + 1 + u16::from(has_workflow) + 1 + 1;
    let popup_height: u16 = wanted.max(if has_workflow { 13 } else { 12 });
    let popup_area = fixed_centered_rect(area, POPUP_WIDTH, popup_height);

    frame.render_widget(Clear, popup_area);

    let title = if is_editing {
        " Edit Project "
    } else {
        " New Project "
    };

    let mut title_spans = vec![Span::styled(
        title,
        Style::default()
            .fg(theme::overlay_title())
            .add_modifier(Modifier::BOLD),
    )];
    if focused_field < 2 && text.surface_for(focused_field).is_some() {
        title_spans.push(mode_pill(text.mode_label()));
        title_spans.push(Span::raw(" "));
    }
    let block = theme::overlay_block()
        .title(Line::from(title_spans))
        .padding(Padding::horizontal(1));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if inner.height < 6 {
        return;
    }

    let background = theme::overlay_bg();
    let name_area = Rect::new(inner.x, inner.y, inner.width, name_rows.min(inner.height));
    render_field(frame, name_area, &fields[0], background);
    let path_y = inner.y + name_rows;
    let path_area = Rect::new(
        inner.x,
        path_y,
        inner.width,
        path_rows.min(inner.height.saturating_sub(name_rows)),
    );
    render_field(frame, path_area, &fields[1], background);

    // Color field (row 2)
    let color_y = inner.y + name_rows + path_rows;
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

            let rsi_y = color_y + 1;
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
        "Tab: next  Enter: save  Esc: normal, then cancel",
        Style::default().fg(theme::overlay_hint()),
    )]);
    frame.render_widget(Paragraph::new(hint), hint_area);
}
