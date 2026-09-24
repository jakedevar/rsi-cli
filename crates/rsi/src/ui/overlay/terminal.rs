//! Renderer for the embedded terminal overlay.

use crate::app::App;
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

/// Convert a vt100 color to a ratatui color.
fn vt100_color_to_ratatui(color: vt100::Color) -> Option<Color> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

/// Render the terminal overlay as a floating panel.
pub(super) fn render_terminal_overlay(frame: &mut Frame, area: Rect, app: &App) {
    // --- Compute popup rect: 90% width, 80% height, centered ---
    let popup_width = (area.width * 90 / 100).max(20);
    let popup_height = (area.height * 80 / 100).max(5);
    let popup_x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let popup_y = area.y + (area.height.saturating_sub(popup_height)) / 2;
    let popup_rect = Rect::new(popup_x, popup_y, popup_width, popup_height);

    frame.render_widget(Clear, popup_rect);

    // --- Determine title and border color ---
    let title_text = " TERMINAL ";
    let border_color = theme::accent();

    // Default background follows the session detail text-area background
    // setting (theme fallback: overlay_bg). Also used below as the default
    // cell background for the vt100 grid.
    let default_bg = crate::settings::text_area_bg_color(&app.settings, theme::overlay_bg());

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(border_color))
        .title(Line::from(vec![Span::styled(
            title_text,
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )]));
    // T1.1: paint the block (border ring + title row) so the `Clear` above
    // cannot leave Reset-bg cells visible under a translucent terminal on an
    // opaque theme. Transparent keeps its historical unpainted (Reset) ring —
    // locked baseline.
    if !theme::is_transparent_theme() {
        block = block.style(Style::default().bg(default_bg));
    }

    let inner = block.inner(popup_rect);
    frame.render_widget(block, popup_rect);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // --- Dead shell / no terminal: show exit message ---
    let terminal = match &app.terminal {
        Some(t) if t.alive => t,
        _ => {
            let msg = "[shell exited - Ctrl+\\ or :term to close]";
            let msg_len = msg.len() as u16;
            let msg_x = inner.x + inner.width.saturating_sub(msg_len) / 2;
            let msg_y = inner.y + inner.height / 2;
            let msg_area = Rect::new(msg_x, msg_y, msg_len.min(inner.width), 1);
            frame.render_widget(
                Paragraph::new(msg).style(Style::default().fg(theme::overlay_hint())),
                msg_area,
            );
            return;
        }
    };

    // --- Render vt100 cell grid into the ratatui buffer ---
    let screen = terminal.screen();
    let buf = frame.buffer_mut();

    for row in 0..inner.height {
        for col in 0..inner.width {
            if let Some(cell) = screen.cell(row, col) {
                let buf_cell = &mut buf[(inner.x + col, inner.y + row)];

                // Character content
                let ch = cell.contents().chars().next().unwrap_or(' ');
                buf_cell.set_char(ch);

                // Build style from vt100 attributes. Default background follows the
                // session detail text-area background setting; explicit ANSI bg wins.
                let mut style = Style::default().bg(default_bg);

                if let Some(fg) = vt100_color_to_ratatui(cell.fgcolor()) {
                    style = style.fg(fg);
                }
                if let Some(bg) = vt100_color_to_ratatui(cell.bgcolor()) {
                    style = style.bg(bg);
                }
                if cell.bold() {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if cell.italic() {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if cell.underline() {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }

                buf_cell.set_style(style);
            }
        }
    }

    // --- Show cursor ---
    let (cur_row, cur_col) = screen.cursor_position();
    // Clamp cursor to inner area bounds
    if cur_row < inner.height && cur_col < inner.width {
        frame.set_cursor_position((inner.x + cur_col, inner.y + cur_row));
    }
}
