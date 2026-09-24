//! Memory search rendering.

use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, List, ListItem, Paragraph};

use super::fixed_centered_rect;

pub(super) fn render_memory_search(
    frame: &mut Frame,
    area: Rect,
    query: &str,
    results: &[rsi_common::rpc::MemorySearchResult],
    selected: usize,
    loading: bool,
) {
    let popup_area = fixed_centered_rect(
        area,
        (area.width * 80 / 100).min(120),
        (area.height * 80 / 100).min(40),
    );
    frame.render_widget(Clear, popup_area);

    let title = if loading {
        " Memory Search (searching...) "
    } else {
        " Memory Search "
    };
    let block = theme::overlay_block().title(title);
    frame.render_widget(block, popup_area);

    let inner = {
        let b = theme::overlay_block();
        b.inner(popup_area)
    };

    let chunks = Layout::vertical([
        Constraint::Length(1), // query input line
        Constraint::Min(0),    // results list
    ])
    .split(inner);

    // Query line
    let query_line = Line::from(vec![
        Span::styled("/ ", Style::default().fg(theme::blue())),
        Span::raw(query),
        Span::styled("█", Style::default().fg(theme::blue())),
    ]);
    frame.render_widget(Paragraph::new(query_line), chunks[0]);

    // Results
    if results.is_empty() {
        let msg = if query.is_empty() {
            "Type to search memory..."
        } else if loading {
            "Searching..."
        } else {
            "No results"
        };
        frame.render_widget(
            Paragraph::new(msg).style(Style::default().fg(theme::subtext0())),
            chunks[1],
        );
        return;
    }

    let items: Vec<ListItem> = results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let is_selected = i == selected;
            let row_style = if is_selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let header = format!("{} [{:.2}]", r.path, r.score);
            let snippet: String = r
                .snippet
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(80)
                .collect();
            ListItem::new(vec![
                Line::from(Span::styled(header, row_style.add_modifier(Modifier::BOLD))),
                Line::from(Span::styled(snippet, row_style.fg(theme::subtext0()))),
            ])
        })
        .collect();

    frame.render_widget(List::new(items), chunks[1]);
}
