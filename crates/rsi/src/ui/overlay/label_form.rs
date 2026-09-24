//! Label form rendering.

use crate::overlay::label_form::LABEL_COLORS;
use crate::ui::{parse_hex_color, theme};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;

/// Render the label form popup (create/edit).
#[allow(clippy::too_many_arguments)]
pub(super) fn render_label_form(
    frame: &mut Frame,
    area: Rect,
    focused_field: usize,
    name: &str,
    description: &str,
    color_index: usize,
    is_editing: bool,
) {
    let popup_height: u16 = 12;
    let popup_area = fixed_centered_rect(area, 55, popup_height);

    frame.render_widget(Clear, popup_area);

    let title = if is_editing {
        " Edit Label "
    } else {
        " New Label "
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

    let fields = [("Name", name), ("Desc", description)];

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

        let cursor = if is_focused { "\u{2588}" } else { "" };
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

    let safe_index = color_index.min(LABEL_COLORS.len().saturating_sub(1));
    let (color_name, color_hex) = LABEL_COLORS[safe_index];
    let color_val = parse_hex_color(color_hex).unwrap_or(theme::mauve());

    let color_line = Line::from(vec![
        Span::styled(" Color: ", color_label_style),
        Span::styled("\u{25a0} ", Style::default().fg(color_val)),
        Span::styled(color_name, Style::default().fg(theme::text())),
        if is_color_focused {
            Span::styled(
                "  \u{25c4} \u{25ba}",
                Style::default().fg(theme::overlay_hint()),
            )
        } else {
            Span::raw("")
        },
    ]);
    let color_area = Rect::new(inner.x, color_y, inner.width, 1);
    frame.render_widget(Paragraph::new(color_line), color_area);

    // Hint bar at the bottom
    let hint_y = inner.y + inner.height - 1;
    let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
    let hint = Line::from(vec![Span::styled(
        "Tab: next field  Enter: save  Esc: cancel",
        Style::default().fg(theme::overlay_hint()),
    )]);
    frame.render_widget(Paragraph::new(hint), hint_area);
}
