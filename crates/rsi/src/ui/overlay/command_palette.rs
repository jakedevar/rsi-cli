//! Fuzzy command palette, using the telescope popup geometry.

use crate::action_registry::{ActionId, HelpOrigin, command_descriptor};
use crate::ui::theme;
use ratatui::prelude::*;
use ratatui::widgets::{Clear, Paragraph};

#[allow(clippy::too_many_arguments)]
pub(super) fn render_command_palette(
    frame: &mut Frame,
    area: Rect,
    query: &str,
    results: &[ActionId],
    selected: usize,
    argument_edit: bool,
    argument_input: &str,
    origin: HelpOrigin,
) {
    let popup = super::centered_top_third_rect(area);
    frame.render_widget(Clear, popup);
    let block = theme::overlay_block().title(format!(" Commands · {} ", origin.title()));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let inner = Rect::new(
        inner.x.saturating_add(2),
        inner.y.saturating_add(1),
        inner.width.saturating_sub(2),
        inner.height.saturating_sub(2),
    );
    if inner.height < 3 {
        return;
    }

    let prompt = if argument_edit {
        results
            .get(selected)
            .and_then(|id| command_descriptor(*id))
            .map_or("", |descriptor| descriptor.command_aliases[0])
    } else {
        ""
    };
    let input = if argument_edit { argument_input } else { query };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(":{prompt} "), Style::default().fg(theme::blue())),
            Span::styled(input.to_string(), Style::default().fg(theme::text())),
            Span::styled("█", Style::default().fg(theme::text())),
        ])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    frame.render_widget(
        Paragraph::new("─".repeat(inner.width as usize))
            .style(Style::default().fg(theme::surface2())),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );
    let list_height = inner.height.saturating_sub(3) as usize;
    if results.is_empty() {
        frame.render_widget(
            Paragraph::new("No matching commands").style(Style::default().fg(theme::subtext0())),
            Rect::new(inner.x, inner.y + 2, inner.width, 1),
        );
    } else {
        let offset = selected.saturating_sub(list_height.saturating_sub(1));
        for (row, id) in results.iter().skip(offset).take(list_height).enumerate() {
            let Some(descriptor) = command_descriptor(*id) else {
                continue;
            };
            let text = format!(
                ":{:<20} {}",
                descriptor.command_aliases[0], descriptor.label
            );
            let style = if offset + row == selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(theme::text())
            };
            frame.render_widget(
                Paragraph::new(text).style(style),
                Rect::new(
                    inner.x,
                    inner.y + 2 + u16::try_from(row).unwrap_or(u16::MAX),
                    inner.width,
                    1,
                ),
            );
        }
    }
    let hint = if argument_edit {
        "Enter run · Esc results"
    } else {
        "Enter run · Tab arguments · Esc close"
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(theme::subtext0())),
        Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        buffer
            .content
            .chunks(buffer.area.width as usize)
            .map(|row| {
                row.iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn command_palette_renders_query_rows_selection_origin_and_empty_state() {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
        terminal
            .draw(|frame| {
                render_command_palette(
                    frame,
                    frame.area(),
                    "rat",
                    &[ActionId::ManagerPolicy, ActionId::Rate],
                    1,
                    false,
                    "",
                    HelpOrigin::SessionList,
                );
            })
            .expect("render");
        let text = screen(&terminal);
        assert!(text.contains("Commands · Session List"));
        assert!(text.contains(": rat"));
        assert!(text.contains(":manager policy"));
        assert!(text.contains(":rate"));
        let buffer = terminal.backend().buffer();
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.symbol() == "r" && cell.modifier.contains(Modifier::REVERSED))
        );

        terminal
            .draw(|frame| {
                render_command_palette(
                    frame,
                    frame.area(),
                    "zzzz",
                    &[],
                    0,
                    false,
                    "",
                    HelpOrigin::SessionList,
                );
            })
            .expect("empty render");
        assert!(screen(&terminal).contains("No matching commands"));
    }
}
