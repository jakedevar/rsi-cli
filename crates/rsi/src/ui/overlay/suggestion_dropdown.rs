//! Suggestion dropdown rendering.

use crate::suggestions::{CommandSuggestion, SuggestionMode};
use crate::ui::{session, theme};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, List, ListItem};

/// Render the suggestion dropdown below the first line of the textarea.
///
/// Supports both command mode (`/{name}  {description}`) and file mode (`@{path}`).
pub(crate) fn render_suggestion_dropdown(
    frame: &mut Frame,
    textarea_area: Rect,
    available_commands: &[CommandSuggestion],
    file_paths: &[String],
    filtered_indices: &[usize],
    selected_suggestion: usize,
    mode: SuggestionMode,
) {
    let max_visible: usize = match mode {
        SuggestionMode::File => 7,
        _ => 8,
    };

    let viewport = frame.area();
    let width = textarea_area
        .width
        .min(viewport.right().saturating_sub(textarea_area.x));
    let total = filtered_indices.len();
    if total == 0 || width < 3 {
        return;
    }

    // Reserve a full frame, reducing visible rows to fit below the cursor.
    // Near the bottom edge, place the menu above the textarea instead.
    let below_y = textarea_area.y.saturating_add(1);
    let below_space = viewport.bottom().saturating_sub(below_y);
    let above_space = textarea_area.y.saturating_sub(viewport.y);
    let desired_height = total.min(max_visible) as u16 + 2;
    let (dropdown_y, dropdown_height) = if below_space >= 3 {
        (below_y, desired_height.min(below_space))
    } else if above_space >= 3 {
        let height = desired_height.min(above_space);
        (textarea_area.y - height, height)
    } else {
        return;
    };
    let visible_count = (dropdown_height - 2) as usize;

    // Compute scroll offset to keep selected item visible
    let scroll_offset = selected_suggestion
        .saturating_sub(visible_count - 1)
        .min(total.saturating_sub(visible_count));

    let dropdown_area = Rect::new(textarea_area.x, dropdown_y, width, dropdown_height);

    // Clear the dropdown area
    frame.render_widget(Clear, dropdown_area);
    let block = theme::overlay_block()
        .border_style(Style::default().fg(theme::suggestion_border()))
        .style(Style::default().bg(theme::suggestion_bg()));
    let list_width = block.inner(dropdown_area).width as usize;

    // Build list items
    let items: Vec<ListItem> = filtered_indices
        .iter()
        .skip(scroll_offset)
        .take(visible_count)
        .enumerate()
        .map(|(visible_idx, &item_idx)| {
            let is_selected = visible_idx + scroll_offset == selected_suggestion;

            let (fg, bg) = if is_selected {
                (
                    theme::suggestion_selected_fg(),
                    theme::suggestion_selected_bg(),
                )
            } else {
                (theme::suggestion_fg(), theme::suggestion_bg())
            };

            match mode {
                SuggestionMode::File => {
                    let path = &file_paths[item_idx];
                    let display = format!("@{path}");
                    // Truncate if wider than dropdown
                    let max_w = list_width;
                    let text = if display.len() > max_w {
                        let end = session::floor_char_boundary(&display, max_w);
                        &display[..end]
                    } else {
                        &display
                    };
                    let span = Span::styled(
                        text.to_string(),
                        Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
                    );
                    // Fill remaining width with bg color
                    let remaining = max_w.saturating_sub(text.len());
                    let line = Line::from(vec![
                        span,
                        Span::styled(" ".repeat(remaining), Style::default().bg(bg)),
                    ]);
                    ListItem::new(line)
                }
                _ => {
                    let cmd = &available_commands[item_idx];
                    // Format: "/name  description" with name bold, description dimmed
                    let name_span = Span::styled(
                        format!("/{}", cmd.name),
                        Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
                    );

                    // Fill remaining width with description
                    let name_width = cmd.name.len() + 1; // +1 for the "/"
                    let padding = 2;
                    let desc_width = list_width.saturating_sub(name_width + padding);
                    let desc_text: String = if desc_width > 0 {
                        let d = &cmd.description;
                        if d.len() > desc_width {
                            let end = session::floor_char_boundary(d, desc_width);
                            d[..end].to_string()
                        } else {
                            d.clone()
                        }
                    } else {
                        String::new()
                    };

                    let line = Line::from(vec![
                        name_span,
                        Span::styled(
                            format!("{:>pad$}", "", pad = padding),
                            Style::default().bg(bg),
                        ),
                        Span::styled(
                            desc_text,
                            Style::default().fg(theme::suggestion_desc()).bg(bg),
                        ),
                    ]);

                    ListItem::new(line)
                }
            }
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .style(Style::default().bg(theme::suggestion_bg()));

    frame.render_widget(list, dropdown_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn suggestion_popup_has_complete_input_colored_frame_and_keeps_selection_visible() {
        theme::with_theme_state(|| {
            theme::set_theme_by_name("truly-transparent");
            let paths: Vec<String> = (0..12).map(|i| format!("file{i}.rs")).collect();
            let indices: Vec<usize> = (0..paths.len()).collect();
            // Normal placement, clipped below, and fallback above the bottom row.
            for (anchor_y, top, bottom) in [(1, 2, 10), (8, 9, 11), (11, 2, 10)] {
                let mut terminal = Terminal::new(TestBackend::new(30, 12)).unwrap();
                terminal
                    .draw(|frame| {
                        render_suggestion_dropdown(
                            frame,
                            Rect::new(2, anchor_y, 20, 1),
                            &[],
                            &paths,
                            &indices,
                            11,
                            SuggestionMode::File,
                        );
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                for (x, y, symbol) in [
                    (2, top, "┌"),
                    (21, top, "┐"),
                    (2, bottom, "└"),
                    (21, bottom, "┘"),
                ] {
                    assert_eq!(buffer[(x, y)].symbol(), symbol);
                    assert_eq!(buffer[(x, y)].fg, theme::input_bar_border(true));
                    assert_eq!(buffer[(x, y)].bg, ratatui::style::Color::Reset);
                }
                for y in top + 1..bottom {
                    for x in [2, 21] {
                        assert_eq!(buffer[(x, y)].symbol(), "│");
                        assert_eq!(buffer[(x, y)].fg, theme::input_bar_border(true));
                    }
                }
                let selected_row: String =
                    (3..21).map(|x| buffer[(x, bottom - 1)].symbol()).collect();
                assert!(selected_row.contains("@file11.rs"), "{selected_row}");
                assert_eq!(buffer[(3, bottom - 1)].bg, theme::suggestion_selected_bg());
            }
        });
    }
}
