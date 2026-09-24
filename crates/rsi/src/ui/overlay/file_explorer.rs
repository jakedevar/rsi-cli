//! File explorer drawer rendering.

use crate::types::FileExplorerEntry;
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

/// Render the file explorer drawer — a left-anchored panel showing a directory tree.
/// Width is ~40% of terminal, clamped 30..50 cols, full height.
pub(super) fn drawer_width(area_width: u16) -> u16 {
    // Drawer geometry must be independent of centered transcript gutters. On
    // compact terminals, the gutter can be only a few columns wide, leaving
    // the explorer unable to show paths or its own key hints.
    (area_width * 2 / 5).clamp(30, 50).min(area_width)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_file_explorer(
    frame: &mut Frame,
    area: Rect,
    root: &std::path::Path,
    entries: &[FileExplorerEntry],
    selected_index: usize,
    scroll_offset: usize,
    _show_hidden: bool,
    finder_active: bool,
    finder_query: &str,
    finder_cache: &[std::path::PathBuf],
    finder_results: &[usize],
    finder_selected: usize,
    explorer_focused: bool,
) {
    let drawer_w = drawer_width(area.width);
    let drawer_area = Rect::new(area.x, area.y, drawer_w, area.height);

    frame.render_widget(Clear, drawer_area);

    let title = format!(
        " {} ",
        root.file_name()
            .unwrap_or(root.as_os_str())
            .to_string_lossy()
    );
    let title_color = if explorer_focused {
        theme::overlay_title()
    } else {
        theme::subtext0()
    };
    // This drawer is an overlay, but it is a persistent split-style surface.
    // Give its boundary the same neutral chrome color as ordinary pane rules,
    // rather than the primary-colored modal border from `overlay_block`.
    let border_style = Style::default().fg(theme::neutral_border());
    let block = theme::overlay_block()
        .border_style(border_style)
        .title(Line::from(Span::styled(
            title,
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        )))
        .padding(Padding::horizontal(1));

    let inner = block.inner(drawer_area);
    frame.render_widget(block, drawer_area);

    if inner.height < 2 {
        return;
    }

    if finder_active {
        render_finder(
            frame,
            inner,
            finder_query,
            finder_cache,
            finder_results,
            finder_selected,
        );
        return;
    }

    let list_height = inner.height.saturating_sub(1) as usize; // reserve 1 row for hint bar

    if entries.is_empty() {
        let empty = Paragraph::new("Empty directory").style(Style::default().fg(theme::subtext0()));
        frame.render_widget(empty, inner);

        // Hint bar
        let hint_y = inner.y + inner.height - 1;
        let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
        let hint = Line::from(Span::styled(
            "/:find .:hidden q:close",
            Style::default().fg(theme::overlay_hint()),
        ));
        frame.render_widget(Paragraph::new(hint), hint_area);
        return;
    }

    // Compute scroll to keep selection visible.
    let scroll = if selected_index >= scroll_offset + list_height {
        selected_index - list_height + 1
    } else if selected_index < scroll_offset {
        selected_index
    } else {
        scroll_offset
    };

    for (i, entry) in entries.iter().enumerate().skip(scroll).take(list_height) {
        let vis_y = inner.y + (i - scroll) as u16;
        let is_selected = i == selected_index;

        let indent = "  ".repeat(entry.depth());
        let (icon, name_str, name_color) = match entry {
            FileExplorerEntry::Directory { path, expanded, .. } => {
                let icon = if *expanded { "▾ " } else { "▸ " };
                let name = format!(
                    "{}/",
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
                (icon, name, theme::overlay_title())
            }
            FileExplorerEntry::File { path, .. } => {
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                ("  ", name, theme::text())
            }
        };

        let row_style = if is_selected {
            Style::default().bg(theme::surface2())
        } else {
            Style::default()
        };

        let line = Line::from(vec![
            Span::styled(&indent, Style::default()),
            Span::styled(icon, Style::default().fg(theme::subtext0())),
            Span::styled(name_str, Style::default().fg(name_color)),
        ]);

        let row_area = Rect::new(inner.x, vis_y, inner.width, 1);
        frame.render_widget(Paragraph::new(line).style(row_style), row_area);
    }

    // Hint bar at the bottom
    let hint_y = inner.y + inner.height - 1;
    let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
    let hint_text = if explorer_focused {
        "j/k:nav l:open h:up /:find ^l:viewer q:close"
    } else {
        "^h:explorer q:close"
    };
    let hint = Line::from(Span::styled(
        hint_text,
        Style::default().fg(theme::overlay_hint()),
    ));
    frame.render_widget(Paragraph::new(hint), hint_area);
}

/// Render the fuzzy finder sub-mode: search input at top, scored results below, hint bar.
fn render_finder(
    frame: &mut Frame,
    inner: Rect,
    query: &str,
    cache: &[std::path::PathBuf],
    results: &[usize],
    selected: usize,
) {
    if inner.height < 3 {
        return;
    }

    // Row 0: search input
    let input_area = Rect::new(inner.x, inner.y, inner.width, 1);
    let input_line = Line::from(vec![
        Span::styled("/ ", Style::default().fg(theme::blue())),
        Span::raw(query),
        Span::styled("█", Style::default().fg(theme::blue())),
    ]);
    frame.render_widget(Paragraph::new(input_line), input_area);

    // Row 1..n-1: results
    let list_height = inner.height.saturating_sub(2) as usize; // -1 input, -1 hint
    let list_y = inner.y + 1;

    if results.is_empty() {
        let msg = if query.is_empty() {
            "Type to search files..."
        } else {
            "No matches"
        };
        let msg_area = Rect::new(inner.x, list_y, inner.width, 1);
        frame.render_widget(
            Paragraph::new(msg).style(Style::default().fg(theme::subtext0())),
            msg_area,
        );
    } else {
        // Scroll to keep selection visible
        let scroll = if selected >= list_height {
            selected - list_height + 1
        } else {
            0
        };

        for (vi, &cache_idx) in results.iter().enumerate().skip(scroll).take(list_height) {
            let vis_y = list_y + (vi - scroll) as u16;
            let is_selected = vi == selected;

            let path_str = cache
                .get(cache_idx)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();

            let row_style = if is_selected {
                Style::default().bg(theme::surface2())
            } else {
                Style::default()
            };

            let row_area = Rect::new(inner.x, vis_y, inner.width, 1);
            // Truncate path to fit drawer width
            let display: String = path_str.chars().take(inner.width as usize).collect();
            frame.render_widget(
                Paragraph::new(Span::styled(display, Style::default().fg(theme::text())))
                    .style(row_style),
                row_area,
            );
        }
    }

    // Hint bar
    let hint_y = inner.y + inner.height - 1;
    let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
    let count_str = format!("{}/{}", results.len(), cache.len());
    let hint = Line::from(vec![
        Span::styled(
            "j/k:nav Enter:open Esc:back ",
            Style::default().fg(theme::overlay_hint()),
        ),
        Span::styled(count_str, Style::default().fg(theme::subtext0())),
    ]);
    frame.render_widget(Paragraph::new(hint), hint_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn drawer_keeps_usable_width_on_compact_wide_terminals() {
        // A 160-column terminal has only a 14-column transcript gutter when
        // the transcript is capped at 129 columns. The drawer must retain its
        // documented usable maximum instead of inheriting that gutter width.
        assert_eq!(drawer_width(160), 50);
    }

    #[test]
    fn drawer_width_stays_within_documented_bounds() {
        assert_eq!(drawer_width(80), 32);
        assert_eq!(drawer_width(120), 48);
        assert_eq!(drawer_width(200), 50);
    }

    #[test]
    fn drawer_border_uses_neutral_pane_chrome() {
        theme::with_theme_state(|| {
            let mut terminal =
                Terminal::new(TestBackend::new(80, 12)).expect("test terminal should initialize");
            terminal
                .draw(|frame| {
                    render_file_explorer(
                        frame,
                        frame.area(),
                        std::path::Path::new("/tmp/project"),
                        &[],
                        0,
                        0,
                        false,
                        false,
                        "",
                        &[],
                        &[],
                        0,
                        true,
                    );
                })
                .expect("file explorer should render");

            assert_eq!(
                terminal.backend().buffer()[(0, 1)].fg,
                theme::neutral_border(),
                "drawer boundary must use normal pane chrome, not modal focus color"
            );
        });
    }
}
