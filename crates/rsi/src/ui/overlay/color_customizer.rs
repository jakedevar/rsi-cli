//! Color customizer overlay rendering.

use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;
use crate::overlay::color_customizer::{FIELD_LABELS, parse_hex_color};

/// Render the color customizer overlay.
pub(super) fn render_color_customizer(
    frame: &mut Frame,
    area: Rect,
    focused_field: usize,
    inputs: &[String],
    errors: &[bool],
) {
    let field_count = FIELD_LABELS.len();
    // height: title block (2) + 2 lines per field + blank line + hint (1) + padding
    let popup_height = (field_count as u16) * 2 + 5;
    let popup_area = fixed_centered_rect(area, 54, popup_height);

    frame.render_widget(Clear, popup_area);

    let block = theme::overlay_block()
        .title(Line::from(vec![Span::styled(
            " Message Border Colors ",
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

    for i in 0..field_count {
        if y + 1 >= inner.y + inner.height {
            break;
        }

        let is_focused = i == focused_field;
        let label = FIELD_LABELS[i];
        let input = inputs.get(i).map(|s| s.as_str()).unwrap_or("");
        let has_error = errors.get(i).copied().unwrap_or(false);

        // Label row
        let label_style = if is_focused {
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::subtext1())
        };
        let label_line = Line::from(vec![Span::styled(label, label_style)]);
        let label_area = Rect::new(inner.x, y, inner.width, 1);
        frame.render_widget(Paragraph::new(label_line), label_area);
        y += 1;

        if y >= inner.y + inner.height {
            break;
        }

        // Input row
        let (input_fg, input_bg) = if has_error {
            (theme::red(), theme::surface0())
        } else if is_focused {
            (theme::text(), theme::surface2())
        } else {
            (theme::subtext0(), theme::surface0())
        };

        // Color swatch preview (if valid hex)
        let preview_span = if let Some([r, g, b]) = parse_hex_color(input) {
            Span::styled(
                "██ ",
                Style::default().fg(ratatui::style::Color::Rgb(r, g, b)),
            )
        } else {
            Span::raw("   ")
        };

        let cursor_suffix = if is_focused { "█" } else { "" };
        let display = if input.is_empty() {
            format!(
                "{:<7}",
                if is_focused {
                    "#______"
                } else {
                    "(theme default)"
                }
            )
        } else {
            format!("{}{}", input, cursor_suffix)
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
            "j/k/Tab: navigate  type #RRGGBB  Enter: apply  Del: reset  Esc: close",
            Style::default().fg(theme::overlay_hint()),
        )]);
        frame.render_widget(Paragraph::new(hint), hint_area);
    }
}
