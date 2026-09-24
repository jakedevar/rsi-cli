//! Message bridge connection form rendering.

use crate::settings::MessageBridgeKind;
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;

#[allow(clippy::too_many_arguments)]
pub(super) fn render_message_bridge_form(
    frame: &mut Frame,
    area: Rect,
    bridge: MessageBridgeKind,
    focused_field: usize,
    enabled: bool,
    account: &str,
    allow_from: &str,
    working_dir: &str,
) {
    let popup_height = match bridge {
        MessageBridgeKind::Signal => 10,
        MessageBridgeKind::Imessage => 9,
    };
    let popup_area = fixed_centered_rect(area, 74, popup_height);
    frame.render_widget(Clear, popup_area);

    let title = format!(" {} Connection ", bridge.label());
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

    let enabled_value = if enabled { "true" } else { "false" };
    let fields: Vec<(&str, &str)> = match bridge {
        MessageBridgeKind::Signal => vec![
            ("Enabled", enabled_value),
            ("Account", account),
            ("Allow from", allow_from),
            ("Work dir", working_dir),
        ],
        MessageBridgeKind::Imessage => vec![
            ("Enabled", enabled_value),
            ("Allow from", allow_from),
            ("Work dir", working_dir),
        ],
    };

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
        let cursor = if is_focused && i > 0 { "█" } else { "" };
        let line = Line::from(vec![
            Span::styled(format!("{:>10}: ", label), label_style),
            Span::styled(value.to_string(), Style::default().fg(theme::text())),
            Span::styled(cursor, Style::default().fg(theme::text())),
        ]);
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(inner.x, row_y, inner.width, 1),
        );
    }

    let hint_y = inner.y + inner.height - 1;
    let hint = Line::from(vec![Span::styled(
        "Space: toggle enabled  Tab: next field  Enter: save  Esc: cancel",
        Style::default().fg(theme::overlay_hint()),
    )]);
    frame.render_widget(
        Paragraph::new(hint),
        Rect::new(inner.x, hint_y, inner.width, 1),
    );
}
