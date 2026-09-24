//! Dialectic query overlay rendering.

use crate::ui::theme;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use rsi_common::rpc::DialecticSource;

use super::fixed_centered_rect;

pub(super) fn render_dialectic(
    frame: &mut Frame,
    area: Rect,
    messages: &[(String, String)],
    sources: &[DialecticSource],
    input: &str,
    in_flight: bool,
    scroll_offset: usize,
    sources_expanded: bool,
    tool_calls: u32,
) {
    let popup_width = (area.width * 85 / 100).clamp(50, 120);
    let popup_height = (area.height * 80 / 100).clamp(12, 40);
    let popup_area = fixed_centered_rect(area, popup_width, popup_height);

    let title = if in_flight {
        " Dialectic (thinking...) ".to_string()
    } else if tool_calls > 0 {
        format!(" Dialectic ({tool_calls} tools used) [g?] ")
    } else {
        " Dialectic [g?] ".to_string()
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

    // Layout: messages area + optional sources + separator + input line + hint line
    let source_height = if sources_expanded && !sources.is_empty() {
        (sources.len() as u16 + 1).min(6)
    } else if !sources.is_empty() {
        1
    } else {
        0
    };

    let chunks = Layout::vertical([
        Constraint::Min(3),                // messages
        Constraint::Length(source_height), // sources
        Constraint::Length(1),             // separator
        Constraint::Length(1),             // input
        Constraint::Length(1),             // hint
    ])
    .split(inner);

    // Messages
    if messages.is_empty() {
        let hint_text = "Ask a question about your sessions, projects, or memory...";
        frame.render_widget(
            Paragraph::new(hint_text)
                .style(Style::default().fg(theme::overlay_hint()))
                .wrap(Wrap { trim: false }),
            chunks[0],
        );
    } else {
        let mut lines: Vec<Line> = Vec::new();
        for (role, content) in messages {
            let (prefix, color) = if role == "user" {
                ("You: ", theme::user_role())
            } else {
                ("  ", theme::assistant_role())
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
                "  thinking...",
                Style::default()
                    .fg(theme::status_waiting())
                    .add_modifier(Modifier::ITALIC),
            )));
        }

        // Clamp scroll offset
        let visible_height = chunks[0].height as usize;
        let max_scroll = lines.len().saturating_sub(visible_height);
        let effective_scroll = scroll_offset.min(max_scroll);

        let para = Paragraph::new(lines)
            .scroll((effective_scroll as u16, 0))
            .wrap(Wrap { trim: false });
        frame.render_widget(para, chunks[0]);
    }

    // Sources footer
    if !sources.is_empty() {
        if sources_expanded {
            let mut source_lines: Vec<Line> = Vec::new();
            source_lines.push(Line::from(Span::styled(
                format!(" Sources ({}) [s to collapse]:", sources.len()),
                Style::default().fg(theme::overlay_hint()),
            )));
            for s in sources {
                let detail = s.detail.as_deref().unwrap_or("");
                source_lines.push(Line::from(Span::styled(
                    format!("  {} {} {}", s.kind, s.label, detail),
                    Style::default().fg(theme::overlay_hint()),
                )));
            }
            frame.render_widget(Paragraph::new(source_lines), chunks[1]);
        } else {
            let summary = format!(" Sources: {} [s to expand]", sources.len());
            frame.render_widget(
                Paragraph::new(summary).style(Style::default().fg(theme::overlay_hint())),
                chunks[1],
            );
        }
    }

    // Separator
    let sep_char = "\u{2500}"; // box-drawing horizontal
    let sep = Paragraph::new(sep_char.repeat(chunks[2].width as usize))
        .style(Style::default().fg(theme::overlay_border()));
    frame.render_widget(sep, chunks[2]);

    // Input line
    let input_line = Line::from(vec![
        Span::styled("\u{203a} ", Style::default().fg(theme::accent())),
        Span::raw(input),
        if !in_flight {
            Span::styled("\u{2588}", Style::default().fg(theme::accent()))
        } else {
            Span::raw("")
        },
    ]);
    frame.render_widget(Paragraph::new(input_line), chunks[3]);

    // Hint line
    let hint = if in_flight {
        "Esc cancel"
    } else if !messages.is_empty() {
        "Enter send  s sources  \u{2191}\u{2193} scroll  Esc clear/close  q close"
    } else {
        "Enter send  Esc close  q close"
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(theme::overlay_hint())),
        chunks[4],
    );
}
