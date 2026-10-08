//! Render the AI command input overlay — a small floating bar for text transformation.

use crate::ui::theme;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::fixed_centered_rect;

pub(super) fn render_ai_command(frame: &mut Frame, area: Rect, command: &str, in_flight: bool) {
    let popup_width = 70u16.min(area.width.saturating_sub(4));
    let popup_height = 3;
    let popup_area = fixed_centered_rect(area, popup_width, popup_height);

    let title = if in_flight {
        " AI Command (processing…) "
    } else {
        " AI Command "
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::overlay_border()))
        .style(Style::default().bg(theme::overlay_bg()));

    let inner = block.inner(popup_area);
    frame.render_widget(Clear, popup_area);
    frame.render_widget(block, popup_area);

    // Command text with cursor
    let mut display_spans = vec![Span::styled("› ", Style::default().fg(theme::accent()))];
    if in_flight {
        display_spans.push(Span::raw(command));
    } else {
        display_spans.extend(crate::field_edit::draw_with_caret(
            command,
            Style::default(),
            Style::default().fg(theme::accent()),
        ));
    }
    let display = Line::from(display_spans);
    frame.render_widget(Paragraph::new(display), inner);
}
