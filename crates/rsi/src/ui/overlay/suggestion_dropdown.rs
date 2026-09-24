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

    let total = filtered_indices.len();
    let visible_count = total.min(max_visible);

    // Compute scroll offset to keep selected item visible
    let scroll_offset = if selected_suggestion >= max_visible {
        selected_suggestion - max_visible + 1
    } else {
        0
    };

    // Position: starts 1 row below textarea top (below the cursor line),
    // same x and width as textarea
    let dropdown_y = textarea_area.y + 1;
    let dropdown_height = visible_count as u16;

    // Don't render if it would go off screen
    if dropdown_y + dropdown_height > frame.area().height {
        return;
    }

    let dropdown_area = Rect::new(
        textarea_area.x,
        dropdown_y,
        textarea_area.width,
        dropdown_height,
    );

    // Clear the dropdown area
    frame.render_widget(Clear, dropdown_area);

    // Build list items
    let items: Vec<ListItem> = filtered_indices
        .iter()
        .skip(scroll_offset)
        .take(max_visible)
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
                    let max_w = textarea_area.width as usize;
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
                    let desc_width =
                        (textarea_area.width as usize).saturating_sub(name_width + padding);
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

    let list = List::new(items).style(Style::default().bg(theme::suggestion_bg()));

    frame.render_widget(list, dropdown_area);
}
