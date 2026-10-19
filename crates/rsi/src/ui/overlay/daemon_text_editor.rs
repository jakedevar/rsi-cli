//! Daemon text-setting editor overlay rendering (#2438, #2393).

use crate::overlay::daemon_text_editor::daemon_text_field;
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph, Wrap};

use super::fixed_centered_rect;

/// Render the single-field editor of the daemon text setting `field`.
pub(super) fn render_daemon_text_editor(
    frame: &mut Frame,
    area: Rect,
    field: &str,
    input: &str,
    error: Option<&str>,
) {
    let Some(spec) = daemon_text_field(field) else {
        return;
    };
    let width = 76u16.min(area.width.saturating_sub(4)).max(20);
    let popup_area = fixed_centered_rect(area, width, 9);
    frame.render_widget(Clear, popup_area);

    let block = theme::overlay_block()
        .title(Line::from(vec![Span::styled(
            format!(" {} ", spec.title),
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )]))
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);
    if inner.height < 4 {
        return;
    }

    let label = Line::from(vec![Span::styled(
        format!("{} — empty = {}", spec.prompt, spec.default_label),
        Style::default()
            .fg(theme::overlay_title())
            .add_modifier(Modifier::BOLD),
    )]);
    frame.render_widget(
        Paragraph::new(label).wrap(Wrap { trim: true }),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let (input_fg, input_bg) = if error.is_some() {
        (theme::red(), theme::surface0())
    } else {
        (theme::text(), theme::surface2())
    };
    let input_style = Style::default().fg(input_fg).bg(input_bg);
    let input_line = if crate::field_edit::standard_frame() {
        Line::from(crate::field_edit::draw_fit(
            input,
            input_style,
            usize::from(inner.width),
        ))
    } else {
        // Keep the tail of a long path visible, with the caret block after it.
        let room = usize::from(inner.width).saturating_sub(1);
        let chars: Vec<char> = input.chars().collect();
        let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
        Line::from(Span::styled(format!("{shown}█"), input_style))
    };
    frame.render_widget(
        Paragraph::new(input_line).style(Style::default().bg(theme::overlay_bg())),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );

    if let Some(error) = error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                error.to_string(),
                Style::default().fg(theme::red()),
            )))
            .wrap(Wrap { trim: true }),
            Rect::new(
                inner.x,
                inner.y + 2,
                inner.width,
                inner.height.saturating_sub(3),
            ),
        );
    }

    let hint_y = inner.y + inner.height.saturating_sub(1);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{}  Enter: save  Del: clear  Esc: cancel", spec.typing_hint),
            Style::default().fg(theme::overlay_hint()),
        ))),
        Rect::new(inner.x, hint_y, inner.width, 1),
    );
}
