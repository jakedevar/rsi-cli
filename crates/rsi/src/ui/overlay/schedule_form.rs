use ratatui::layout::{Alignment, Rect};
use ratatui::prelude::*;
use ratatui::widgets::{Clear, Padding, Paragraph};

use crate::overlay::schedule_form::RECURRENCE_TYPES;
use crate::ui::theme;

pub fn render_schedule_form(
    frame: &mut Frame,
    area: Rect,
    focused_field: usize,
    name: &str,
    message: &str,
    recurrence_index: usize,
    interval: &str,
    anchor_date: &str,
    anchor_time: &str,
    is_editing: bool,
) {
    let width = 60.min(area.width.saturating_sub(4));
    let height = 14;
    let popup = super::fixed_centered_rect(area, width, height);

    frame.render_widget(Clear, popup);
    let title = if is_editing {
        " Edit Scheduled Job "
    } else {
        " New Scheduled Job "
    };
    let block = theme::overlay_block()
        .title(title)
        .title_alignment(Alignment::Center)
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let fields: Vec<(&str, &str, bool)> = vec![
        ("Name", name, focused_field == 0),
        ("Message", message, focused_field == 1),
    ];

    // Text fields (Name, Message)
    for (i, (label, value, focused)) in fields.iter().enumerate() {
        let y = inner.y + i as u16;
        let label_style = if *focused {
            Style::default()
                .fg(theme::mauve())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::overlay_hint())
        };
        let cursor = if *focused { "\u{2588}" } else { "" };
        let text = format!("{:>8}: {value}{cursor}", label);
        let para = Paragraph::new(Line::from(Span::styled(text, label_style)));
        frame.render_widget(para, Rect::new(inner.x, y, inner.width, 1));
    }

    // Recurrence type field (index 2) — cycle-based
    {
        let y = inner.y + 2;
        let focused = focused_field == 2;
        let label_style = if focused {
            Style::default()
                .fg(theme::mauve())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::overlay_hint())
        };
        let (type_name, _) = RECURRENCE_TYPES[recurrence_index];
        let arrows = if focused { "  \u{25C4} \u{25BA}" } else { "" };
        let text = format!("{:>8}: {type_name}{arrows}", "Repeat");
        let para = Paragraph::new(Line::from(Span::styled(text, label_style)));
        frame.render_widget(para, Rect::new(inner.x, y, inner.width, 1));
    }

    // Interval field (index 3) — only shown if recurrence is not Once
    {
        let y = inner.y + 3;
        let focused = focused_field == 3;
        let label_style = if focused {
            Style::default()
                .fg(theme::mauve())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::overlay_hint())
        };
        let dimmed = recurrence_index == 0; // Once — interval not applicable
        let display_style = if dimmed {
            Style::default().fg(theme::overlay_hint())
        } else {
            label_style
        };
        let cursor = if focused && !dimmed { "\u{2588}" } else { "" };
        let text = format!("{:>8}: {interval}{cursor}", "Every N");
        let para = Paragraph::new(Line::from(Span::styled(text, display_style)));
        frame.render_widget(para, Rect::new(inner.x, y, inner.width, 1));
    }

    // Date field (index 4)
    {
        let y = inner.y + 5; // gap for readability
        let focused = focused_field == 4;
        let label_style = if focused {
            Style::default()
                .fg(theme::mauve())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::overlay_hint())
        };
        let cursor = if focused { "\u{2588}" } else { "" };
        let text = format!("{:>8}: {anchor_date}{cursor}", "Date");
        let para = Paragraph::new(Line::from(Span::styled(text, label_style)));
        frame.render_widget(para, Rect::new(inner.x, y, inner.width, 1));
    }

    // Time field (index 5)
    {
        let y = inner.y + 6;
        let focused = focused_field == 5;
        let label_style = if focused {
            Style::default()
                .fg(theme::mauve())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::overlay_hint())
        };
        let cursor = if focused { "\u{2588}" } else { "" };
        let text = format!("{:>8}: {anchor_time}{cursor}", "Time");
        let para = Paragraph::new(Line::from(Span::styled(text, label_style)));
        frame.render_widget(para, Rect::new(inner.x, y, inner.width, 1));
    }

    // Help text
    {
        let y = inner.y + 8;
        let help = "Date: YYYY-MM-DD  Time: HH:MM:SS (defaults 00:00:00)";
        let para = Paragraph::new(Line::from(Span::styled(
            help,
            Style::default().fg(theme::overlay_hint()),
        )));
        frame.render_widget(para, Rect::new(inner.x, y, inner.width, 1));
    }

    // Hint bar
    {
        let y = inner.y + inner.height.saturating_sub(1);
        let hint = "Tab: next field  Enter: save  Esc: cancel";
        let para = Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(theme::overlay_hint()),
        )));
        frame.render_widget(para, Rect::new(inner.x, y, inner.width, 1));
    }
}
