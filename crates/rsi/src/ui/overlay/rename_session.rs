//! Rename session overlay rendering.

use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::fixed_centered_rect;

/// Render the session rename overlay (F2 from session list).
/// Centered popup: 60 wide x 3 tall with a text input and blinking cursor.
pub(super) fn render_rename_session_overlay(frame: &mut Frame, area: Rect, title: &str) {
    let popup_width = 60u16.min(area.width.saturating_sub(4));
    let popup_height = 3;
    let popup_area = fixed_centered_rect(area, popup_width, popup_height);

    let block = Block::default()
        .title(" Rename Session ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::accent()))
        .style(Style::default().bg(theme::overlay_bg()));

    let inner = block.inner(popup_area);
    frame.render_widget(Clear, popup_area);
    frame.render_widget(block, popup_area);

    // Truncate display text from the left if it overflows the inner area
    let display = if title.len() > inner.width as usize {
        let overflow = title.len() - inner.width as usize + 3; // +3 for "..."
        format!("...{}", &title[overflow..])
    } else {
        title.to_string()
    };
    let text = Paragraph::new(display.as_str()).style(Style::default().fg(theme::text()));
    frame.render_widget(text, inner);

    // Position cursor at end of text
    let cursor_x = inner.x + title.len().min(inner.width as usize) as u16;
    frame.set_cursor_position(Position::new(cursor_x, inner.y));
}
