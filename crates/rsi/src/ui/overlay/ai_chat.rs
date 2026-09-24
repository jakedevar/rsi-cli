//! Render the AI chat overlay — conversation window with source text Q&A.

use crate::ui::theme;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::fixed_centered_rect;

pub(super) fn render_ai_chat(
    frame: &mut Frame,
    area: Rect,
    messages: &[(String, String)],
    input: &str,
    in_flight: bool,
    scroll_offset: usize,
) {
    let popup_width = (area.width * 60 / 100).clamp(50, 100);
    let popup_height = (area.height * 60 / 100).clamp(12, 30);
    let popup_area = fixed_centered_rect(area, popup_width, popup_height);

    let title = if in_flight {
        " AI Chat (thinking…) "
    } else {
        " AI Chat "
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if in_flight {
            theme::status_waiting()
        } else {
            theme::accent()
        }))
        .style(Style::default().bg(theme::overlay_bg()));

    let inner = block.inner(popup_area);
    frame.render_widget(Clear, popup_area);
    frame.render_widget(block, popup_area);

    // Layout: conversation area + separator + input line + hint line
    let chunks = Layout::vertical([
        Constraint::Min(3),    // conversation
        Constraint::Length(1), // separator
        Constraint::Length(1), // input
        Constraint::Length(1), // hint
    ])
    .split(inner);

    // Conversation display
    let mut lines: Vec<Line> = Vec::new();
    for (role, content) in messages {
        let (prefix, color) = if role == "user" {
            ("You: ", theme::user_role())
        } else {
            ("AI: ", theme::assistant_role())
        };
        for (i, line) in content.lines().enumerate() {
            if i == 0 {
                lines.push(Line::from(vec![
                    Span::styled(
                        prefix,
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(line, Style::default().fg(theme::metadata_text())),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(line, Style::default().fg(theme::metadata_text())),
                ]));
            }
        }
        lines.push(Line::from("")); // spacing between messages
    }
    if in_flight {
        lines.push(Line::from(Span::styled(
            "AI: thinking…",
            Style::default()
                .fg(theme::status_waiting())
                .add_modifier(Modifier::ITALIC),
        )));
    }

    // Clamp scroll offset
    let visible_height = chunks[0].height as usize;
    let max_scroll = lines.len().saturating_sub(visible_height);
    let effective_scroll = scroll_offset.min(max_scroll);

    let para = Paragraph::new(lines).scroll((effective_scroll as u16, 0));
    frame.render_widget(para, chunks[0]);

    // Separator
    let sep = Paragraph::new("─".repeat(chunks[1].width as usize))
        .style(Style::default().fg(theme::overlay_border()));
    frame.render_widget(sep, chunks[1]);

    // Input line
    let input_line = Line::from(vec![
        Span::styled("› ", Style::default().fg(theme::accent())),
        Span::raw(input),
        if !in_flight {
            Span::styled("█", Style::default().fg(theme::accent()))
        } else {
            Span::raw("")
        },
    ]);
    frame.render_widget(Paragraph::new(input_line), chunks[2]);

    // Hint line
    let hint = if in_flight {
        "Esc cancel"
    } else {
        "Enter send  ↑↓ scroll  Esc close"
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(theme::overlay_hint())),
        chunks[3],
    );
}
