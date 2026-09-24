//! Sort picker rendering.

use crate::app::SortOrder;
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;

/// Render the sort order picker popup.
pub(super) fn render_sort_picker(
    frame: &mut Frame,
    area: Rect,
    selected_index: usize,
    current_order: SortOrder,
) {
    let options = SortOrder::ALL;
    let popup_height = options.len() as u16 + 4; // borders + title + hint
    let popup_area = fixed_centered_rect(area, 45, popup_height);

    frame.render_widget(Clear, popup_area);

    let block = theme::overlay_block()
        .title(Line::from(vec![Span::styled(
            " Sort Order ",
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )]))
        .padding(Padding::horizontal(1));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if inner.height < 2 {
        return;
    }

    for (i, order) in options.iter().enumerate() {
        if i as u16 >= inner.height.saturating_sub(1) {
            break;
        }

        let is_selected = i == selected_index;
        let is_current = *order == current_order;

        let marker = if is_current { "✓ " } else { "  " };

        let row_style = if is_selected {
            Style::default()
                .fg(theme::text())
                .bg(theme::surface2())
                .add_modifier(Modifier::REVERSED)
        } else {
            Style::default().bg(theme::overlay_bg())
        };

        let line = Line::from(vec![
            Span::styled(marker, Style::default().fg(theme::green())),
            Span::styled(
                order.label(),
                Style::default()
                    .fg(theme::text())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                order.description(),
                Style::default().fg(theme::overlay_hint()),
            ),
        ]);

        let row_area = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
        frame.render_widget(Paragraph::new(line).style(row_style), row_area);
    }

    // Hint bar
    let hint_y = inner.y + inner.height - 1;
    let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
    let hint = Line::from(vec![Span::styled(
        "hold S + j/k: navigate  release S: apply  Esc: cancel",
        Style::default().fg(theme::overlay_hint()),
    )]);
    frame.render_widget(Paragraph::new(hint), hint_area);
}
