//! Trash browser overlay rendering.

use crate::types::ArchiveListItem;
use crate::ui::{session, theme};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;

/// Render the trash browser overlay.
pub(super) fn render_trash_browser(
    frame: &mut Frame,
    area: Rect,
    sessions: &[rsi_common::types::Session],
    items: &[ArchiveListItem],
    selected_index: usize,
    scroll_offset: usize,
) {
    let max_rows: u16 = 20;
    let visible_count = (items.len() as u16).min(max_rows);
    let popup_height = visible_count + 4; // borders + hint bar + padding
    let popup_area = fixed_centered_rect(area, 70, popup_height);

    frame.render_widget(Clear, popup_area);

    let block = theme::overlay_block()
        .title(Line::from(vec![Span::styled(
            " Trash ",
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

    let list_height = inner.height.saturating_sub(1) as usize; // reserve 1 for hint

    // Compute scroll offset to keep selected item visible
    let scroll = if selected_index >= scroll_offset + list_height {
        selected_index - list_height + 1
    } else if selected_index < scroll_offset {
        selected_index
    } else {
        scroll_offset
    };

    // Render items
    for (visible_idx, item) in items.iter().skip(scroll).enumerate().take(list_height) {
        let actual_idx = visible_idx + scroll;
        let row_y = inner.y + visible_idx as u16;
        if row_y >= inner.y + inner.height - 1 {
            break;
        }

        match item {
            ArchiveListItem::Header(title) => {
                let header_line = Line::from(vec![Span::styled(
                    format!("── {} ──", title),
                    Style::default()
                        .fg(theme::subtext0())
                        .add_modifier(Modifier::BOLD),
                )]);
                frame.render_widget(
                    Paragraph::new(header_line),
                    Rect::new(inner.x, row_y, inner.width, 1),
                );
            }
            ArchiveListItem::Session(sess_idx) => {
                let sess = &sessions[*sess_idx];
                let is_selected = actual_idx == selected_index;

                let icon = session::status_icon(sess.status);
                let icon_color = session::status_color(sess.status);

                let max_query = (inner.width as usize).saturating_sub(18);
                let query = if sess.query.len() > max_query {
                    let end =
                        session::floor_char_boundary(&sess.query, max_query.saturating_sub(3));
                    format!("{}...", &sess.query[..end])
                } else {
                    sess.query.clone()
                };

                let time_str = session::format_relative_time(sess.updated_at);

                let row_style = if is_selected {
                    Style::default().bg(theme::surface2())
                } else {
                    Style::default().bg(theme::overlay_bg())
                };

                let spans = vec![
                    Span::styled(format!("{} ", icon), Style::default().fg(icon_color)),
                    Span::styled(query, row_style.fg(theme::text())),
                ];

                let row_area = Rect::new(inner.x, row_y, inner.width, 1);
                frame.render_widget(Paragraph::new(Line::from(spans)).style(row_style), row_area);

                // Right-align time
                let time_width = time_str.len() as u16;
                if inner.width > time_width + 2 {
                    let time_x = inner.x + inner.width - time_width;
                    frame.render_widget(
                        Paragraph::new(Span::styled(time_str, row_style.fg(theme::subtext0()))),
                        Rect::new(time_x, row_y, time_width, 1),
                    );
                }
            }
        }
    }

    // Hint bar at bottom
    let hint_y = inner.y + inner.height - 1;
    let hint = Line::from(vec![
        Span::styled(
            "U",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" restore  ", Style::default().fg(theme::subtext0())),
        Span::styled(
            "D",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" purge  ", Style::default().fg(theme::subtext0())),
        Span::styled(
            "Esc",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" close", Style::default().fg(theme::subtext0())),
    ]);
    frame.render_widget(
        Paragraph::new(hint),
        Rect::new(inner.x, hint_y, inner.width, 1),
    );
}
