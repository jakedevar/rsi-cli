//! Text-area background hex editor overlay rendering.

use crate::overlay::text_area_bg_editor::parse_hex_color;
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;

/// Render the single-field hex editor overlay.
pub(super) fn render_text_area_bg_editor(frame: &mut Frame, area: Rect, input: &str, error: bool) {
    let popup_height: u16 = 7;
    let popup_area = fixed_centered_rect(area, 54, popup_height);

    frame.render_widget(Clear, popup_area);

    let block = theme::overlay_block()
        .title(Line::from(vec![Span::styled(
            " Text Area Background ",
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

    let mut y = inner.y;

    // Label
    if y < inner.y + inner.height {
        let label_line = Line::from(vec![Span::styled(
            "Background hex (#RRGGBB) — empty = theme defaults",
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )]);
        let label_area = Rect::new(inner.x, y, inner.width, 1);
        frame.render_widget(Paragraph::new(label_line), label_area);
        y += 1;
    }

    // Input row
    if y < inner.y + inner.height {
        let (input_fg, input_bg) = if error {
            (theme::red(), theme::surface0())
        } else {
            (theme::text(), theme::surface2())
        };

        let preview_span = if let Some([r, g, b]) = parse_hex_color(input) {
            Span::styled(
                "██ ",
                Style::default().fg(ratatui::style::Color::Rgb(r, g, b)),
            )
        } else {
            Span::raw("   ")
        };

        let display = if input.is_empty() {
            "#______█".to_string()
        } else {
            format!("{}█", input)
        };

        let input_line = Line::from(vec![
            preview_span,
            Span::styled(display, Style::default().fg(input_fg).bg(input_bg)),
        ]);
        let input_area = Rect::new(inner.x, y, inner.width, 1);
        frame.render_widget(
            Paragraph::new(input_line).style(Style::default().bg(theme::overlay_bg())),
            input_area,
        );
        y += 1;
    }

    // Hint line at bottom
    let hint_y = inner.y + inner.height.saturating_sub(1);
    if hint_y > y {
        let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
        let hint = Line::from(vec![Span::styled(
            "type #RRGGBB  Enter: apply  Del: clear  Esc: close",
            Style::default().fg(theme::overlay_hint()),
        )]);
        frame.render_widget(Paragraph::new(hint), hint_area);
    }
}
