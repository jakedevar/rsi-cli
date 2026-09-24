//! Session rating overlay rendering — horizontal 1–10 picker.

use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;

/// Render the rating picker popup.
///
/// Shows the digits 1..=10 horizontally; the currently selected digit is
/// highlighted with brackets `[N]` while unselected digits are padded
/// with spaces ` N `. Bottom hint shows the keybindings.
pub(super) fn render_rating_overlay(frame: &mut Frame, area: Rect, selected: Option<u32>) {
    // 4 rows of chrome (top border + title spacer + bar + hint + bottom border)
    let popup_area = fixed_centered_rect(area, 56, 6);
    frame.render_widget(Clear, popup_area);

    let block = theme::overlay_block()
        .title(Line::from(vec![Span::styled(
            " Rate Session ",
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

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(11);
    spans.push(Span::styled(
        "Rate (1-10): ",
        Style::default()
            .fg(theme::text())
            .add_modifier(Modifier::BOLD),
    ));
    for i in 1u32..=10 {
        let is_sel = selected == Some(i);
        let label = if is_sel {
            format!("[{}]", i)
        } else {
            format!(" {} ", i)
        };
        let style = if is_sel {
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(theme::overlay_hint())
        };
        spans.push(Span::styled(label, style));
    }

    let bar_area = Rect::new(inner.x, inner.y, inner.width, 1);
    frame.render_widget(Paragraph::new(Line::from(spans)), bar_area);

    if inner.height >= 3 {
        let hint_y = inner.y + inner.height - 1;
        let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
        let hint = Line::from(vec![Span::styled(
            "1-9: pick  0: ten  h/l: nudge  Enter: confirm  Esc: cancel",
            Style::default().fg(theme::overlay_hint()),
        )]);
        frame.render_widget(Paragraph::new(hint), hint_area);
    }
}
