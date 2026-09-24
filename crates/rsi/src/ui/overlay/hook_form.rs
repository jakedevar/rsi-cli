//! Hook form rendering.

use crate::claude_config::{HookEvent, KnownHookEvent};
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;

/// Render the hook add/edit form overlay.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_hook_form(
    frame: &mut Frame,
    area: Rect,
    focused_field: usize,
    event_idx: Option<usize>,
    event_name_other: Option<&str>,
    matcher: &str,
    command: &str,
    timeout: &str,
    is_editing: bool,
) {
    let popup_height: u16 = 10;
    let popup_area = fixed_centered_rect(area, 70, popup_height);
    frame.render_widget(Clear, popup_area);

    let title = if is_editing {
        " Edit Hook "
    } else {
        " New Hook "
    };

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

    if inner.height < 6 {
        return;
    }

    // Resolve the displayed event name + whether the matcher field applies.
    let (event_label, supports_matcher) = match (event_name_other, event_idx) {
        (Some(other), _) => (
            HookEvent::Other(other.to_string()).label(),
            true, // unknown events permit a matcher (forward-compat)
        ),
        (None, Some(i)) => match KnownHookEvent::ALL.get(i) {
            Some(k) => (k.label().to_string(), k.supports_matcher()),
            None => ("?".to_string(), false),
        },
        (None, None) => ("?".to_string(), false),
    };

    let event_value = if event_name_other.is_some() {
        format!("{} (preserved)", event_label)
    } else {
        format!("{}  ↑/↓ to cycle", event_label)
    };

    let matcher_value = if supports_matcher {
        matcher.to_string()
    } else {
        format!("{} (n/a for this event)", matcher)
    };

    let fields: [(&str, String); 4] = [
        ("Event", event_value),
        ("Matcher", matcher_value),
        ("Command", command.to_string()),
        ("Timeout", format!("{} (sec)", timeout)),
    ];

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

        let cursor = if is_focused && i != 0 { "█" } else { "" };
        let line = Line::from(vec![
            Span::styled(format!("{:>8}: ", label), label_style),
            Span::styled(value.clone(), Style::default().fg(theme::text())),
            Span::styled(cursor, Style::default().fg(theme::text())),
        ]);
        let row_area = Rect::new(inner.x, row_y, inner.width, 1);
        frame.render_widget(Paragraph::new(line), row_area);
    }

    let hint_y = inner.y + inner.height - 1;
    let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
    let hint = Line::from(vec![Span::styled(
        "Tab: next  ↑/↓: cycle event  Enter: save  Esc: cancel",
        Style::default().fg(theme::overlay_hint()),
    )]);
    frame.render_widget(Paragraph::new(hint), hint_area);
}

/// Render the external-edit conflict prompt.
pub(super) fn render_hook_conflict(frame: &mut Frame, area: Rect) {
    let popup_area = fixed_centered_rect(area, 60, 6);
    frame.render_widget(Clear, popup_area);

    let block = theme::overlay_block()
        .title(Line::from(vec![Span::styled(
            " External edit detected ",
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )]))
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if inner.height < 3 {
        return;
    }

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "~/.claude/settings.json changed externally.",
            Style::default().fg(theme::text()),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "[O]verwrite   [R]eload   [C]ancel",
            Style::default()
                .fg(theme::mauve())
                .add_modifier(Modifier::BOLD),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Render the read-only SKILL.md viewer (used by Phase 2's Skills category).
pub(super) fn render_skill_preview(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    content: &str,
    scroll_offset: usize,
) {
    let popup_area = fixed_centered_rect(
        area,
        area.width.saturating_sub(8).max(60),
        area.height.saturating_sub(6).max(10),
    );
    frame.render_widget(Clear, popup_area);

    let title = format!(" SKILL: {} ", name);
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

    if inner.height < 2 {
        return;
    }

    let body_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    let lines: Vec<Line> = content
        .lines()
        .skip(scroll_offset)
        .map(|l| {
            Line::from(Span::styled(
                l.to_string(),
                Style::default().fg(theme::text()),
            ))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), body_area);

    let hint_y = inner.y + inner.height - 1;
    let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
    let hint = Line::from(vec![Span::styled(
        "j/k: scroll  G/gg: jump  q/Esc: close",
        Style::default().fg(theme::overlay_hint()),
    )]);
    frame.render_widget(Paragraph::new(hint), hint_area);
}
