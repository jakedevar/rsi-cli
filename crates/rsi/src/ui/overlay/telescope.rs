//! Telescope fuzzy file picker overlay rendering.

use ratatui::prelude::*;
use ratatui::widgets::{Clear, Paragraph};
use std::path::PathBuf;

use crate::ui::theme;

pub(super) fn render_telescope(
    frame: &mut Frame,
    area: Rect,
    query: &str,
    file_cache: &[PathBuf],
    results: &[usize],
    selected: usize,
) {
    use fuzzy_matcher::FuzzyMatcher;
    use fuzzy_matcher::skim::SkimMatcherV2;

    // Use centered_top_third_rect for telescope positioning
    let popup_area = super::centered_top_third_rect(area);

    frame.render_widget(Clear, popup_area);

    let block = theme::overlay_block().title(" Find File ");
    let block_inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    // Add internal padding: 2 left, 1 top, 1 bottom (~5px equivalent)
    let pad_left: u16 = 2;
    let pad_top: u16 = 1;
    let pad_bottom: u16 = 1;
    let inner = Rect::new(
        block_inner.x + pad_left,
        block_inner.y + pad_top,
        block_inner.width.saturating_sub(pad_left),
        block_inner.height.saturating_sub(pad_top + pad_bottom),
    );

    if inner.height < 3 {
        return;
    }

    // Row 0: query input with cursor
    let query_line = Line::from(vec![
        Span::styled("/ ", Style::default().fg(theme::blue())),
        Span::styled(query, Style::default().fg(theme::text())),
        Span::styled("█", Style::default().fg(theme::text())),
    ]);
    let query_area = Rect::new(inner.x, inner.y, inner.width, 1);
    frame.render_widget(Paragraph::new(query_line), query_area);

    // Row 1: separator
    let sep = "─".repeat(inner.width as usize);
    let sep_area = Rect::new(inner.x, inner.y + 1, inner.width, 1);
    frame.render_widget(
        Paragraph::new(sep).style(Style::default().fg(theme::surface2())),
        sep_area,
    );

    // Rows 2+: results with match highlighting
    let list_start_y = inner.y + 2;
    let list_height = inner.height.saturating_sub(3) as usize; // query + sep + hint

    if results.is_empty() {
        let msg = if query.is_empty() {
            "Type to search files..."
        } else {
            "No matches"
        };
        frame.render_widget(
            Paragraph::new(msg).style(Style::default().fg(theme::subtext0())),
            Rect::new(inner.x, list_start_y, inner.width, 1),
        );
    } else {
        let matcher = SkimMatcherV2::default();
        let path_style = Style::default().fg(theme::text());
        let match_style = Style::default()
            .fg(theme::yellow())
            .add_modifier(Modifier::BOLD);
        let selected_bg = Style::default().add_modifier(Modifier::REVERSED);

        // Scroll to keep selection visible
        let scroll_offset = if selected >= list_height {
            selected - list_height + 1
        } else {
            0
        };

        for (row_idx, &cache_idx) in results
            .iter()
            .skip(scroll_offset)
            .take(list_height)
            .enumerate()
        {
            let y = list_start_y + row_idx as u16;
            let path = &file_cache[cache_idx];
            let path_str = path.to_string_lossy();
            let is_selected = scroll_offset + row_idx == selected;

            // Build spans with match highlighting
            let spans = if !query.is_empty() {
                if let Some((_score, indices)) = matcher.fuzzy_indices(&path_str, query) {
                    build_highlighted_spans(&path_str, &indices, path_style, match_style)
                } else {
                    vec![Span::styled(path_str.to_string(), path_style)]
                }
            } else {
                vec![Span::styled(path_str.to_string(), path_style)]
            };

            let mut line = Line::from(spans);
            if is_selected {
                line = line.patch_style(selected_bg);
            }

            let row_area = Rect::new(inner.x, y, inner.width, 1);
            frame.render_widget(Paragraph::new(line), row_area);
        }
    }

    // Bottom hint bar
    let hint_y = inner.y + inner.height.saturating_sub(1);
    let count_text = format!("{}/{}", results.len(), file_cache.len());
    let hint = Line::from(vec![
        Span::styled(
            "Enter",
            Style::default()
                .fg(theme::green())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" open  ", Style::default().fg(theme::subtext0())),
        Span::styled(
            "Esc",
            Style::default()
                .fg(theme::green())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" close  ", Style::default().fg(theme::subtext0())),
        Span::styled(count_text, Style::default().fg(theme::subtext0())),
    ]);
    frame.render_widget(
        Paragraph::new(hint),
        Rect::new(inner.x, hint_y, inner.width, 1),
    );
}

/// Build a vector of Spans with matched characters highlighted.
fn build_highlighted_spans<'a>(
    text: &str,
    indices: &[usize],
    normal_style: Style,
    match_style: Style,
) -> Vec<Span<'a>> {
    let mut spans = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let index_set: std::collections::HashSet<usize> = indices.iter().copied().collect();

    let mut i = 0;
    let mut last_end = 0;
    while i < chars.len() {
        if index_set.contains(&i) {
            // Flush non-match segment
            if last_end < i {
                let segment: String = chars[last_end..i].iter().collect();
                spans.push(Span::styled(segment, normal_style));
            }
            // Collect consecutive matches
            let match_start = i;
            while i < chars.len() && index_set.contains(&i) {
                i += 1;
            }
            let segment: String = chars[match_start..i].iter().collect();
            spans.push(Span::styled(segment, match_style));
            last_end = i;
        } else {
            i += 1;
        }
    }
    // Flush trailing non-match
    if last_end < chars.len() {
        let segment: String = chars[last_end..].iter().collect();
        spans.push(Span::styled(segment, normal_style));
    }

    spans
}
