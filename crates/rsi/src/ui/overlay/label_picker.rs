//! Label picker popup rendering.

use crate::app::App;
use crate::ui::{parse_hex_color, theme};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;

/// Render the label picker popup.
pub(super) fn render_label_picker(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    filter: &str,
    selected_index: usize,
) {
    use ratatui::style::Color;

    struct PickerEntry {
        name: String,
        description: Option<String>,
        color: Color,
        session_count: usize,
    }

    let mut entries: Vec<PickerEntry> = Vec::new();

    // Count sessions per label
    let mut label_session_counts: std::collections::HashMap<uuid::Uuid, usize> =
        std::collections::HashMap::new();
    let mut unlabeled_count = 0;
    for s in app.sessions.values() {
        if let Some(gid) = s.session.group_id {
            *label_session_counts.entry(gid).or_insert(0) += 1;
        } else {
            unlabeled_count += 1;
        }
    }

    // Labels (sorted by name)
    let mut sorted_labels = app.labels.clone();
    sorted_labels.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    for label in &sorted_labels {
        let color = parse_hex_color(&label.color).unwrap_or(theme::mauve());
        entries.push(PickerEntry {
            name: label.name.clone(),
            description: label.description.clone(),
            color,
            session_count: *label_session_counts.get(&label.id).unwrap_or(&0),
        });
    }

    // "(none)" entry to unassign
    entries.push(PickerEntry {
        name: "(none)".to_string(),
        description: None,
        color: theme::overlay1(),
        session_count: unlabeled_count,
    });

    // Filter entries by fuzzy match on name (case-insensitive)
    let filter_lower = filter.to_lowercase();
    let filtered_entries: Vec<(usize, &PickerEntry)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            if filter.is_empty() {
                true
            } else {
                e.name.to_lowercase().contains(&filter_lower)
            }
        })
        .collect();

    // Popup size: width 55, height = min(filtered + 5, 20)
    let popup_height = (filtered_entries.len() as u16 + 5).min(20);
    let popup_area = fixed_centered_rect(area, 55, popup_height);

    frame.render_widget(Clear, popup_area);

    let block = theme::overlay_block()
        .title(Line::from(vec![Span::styled(
            " Assign Label ",
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

    // Row 0: filter input
    let filter_line = Line::from(vec![
        Span::styled("> ", Style::default().fg(theme::mauve())),
        Span::styled(filter, Style::default().fg(theme::text())),
        Span::styled("\u{2588}", Style::default().fg(theme::text())), // cursor block
    ]);
    let filter_area = Rect::new(inner.x, inner.y, inner.width, 1);
    frame.render_widget(Paragraph::new(filter_line), filter_area);

    // Separator
    let sep_area = Rect::new(inner.x, inner.y + 1, inner.width, 1);
    let sep_char = "\u{2500}".repeat(inner.width as usize);
    frame.render_widget(
        Paragraph::new(sep_char).style(Style::default().fg(theme::surface2())),
        sep_area,
    );

    // List entries
    let list_start_y = inner.y + 2;
    let list_height = inner.height.saturating_sub(3); // filter + sep + hint bar

    for (visible_idx, (_original_idx, entry)) in filtered_entries
        .iter()
        .enumerate()
        .take(list_height as usize)
    {
        let is_selected = visible_idx == selected_index;

        let row_style = if is_selected {
            Style::default().bg(theme::surface2())
        } else {
            Style::default().bg(theme::overlay_bg())
        };

        // Color swatch
        let swatch = Span::styled("\u{25a0} ", Style::default().fg(entry.color));

        // Name (bold if selected)
        let name_style = if is_selected {
            Style::default()
                .fg(entry.color)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(entry.color)
        };
        let name = Span::styled(&entry.name, name_style);

        // Description (dimmed, if present)
        let desc_span = if let Some(ref d) = entry.description {
            let max_desc = (inner.width as usize).saturating_sub(entry.name.len() + 15);
            let desc_display = if d.len() > max_desc && max_desc > 3 {
                format!("...{}", &d[d.len() - max_desc + 1..])
            } else {
                d.clone()
            };
            Span::styled(
                format!("  {}", desc_display),
                Style::default().fg(theme::overlay_hint()),
            )
        } else {
            Span::raw("")
        };

        // Session count (right-aligned)
        let count = Span::styled(
            format!("{:>3} sess", entry.session_count),
            Style::default().fg(theme::overlay_hint()),
        );

        let line = Line::from(vec![swatch, name, desc_span, Span::raw("  "), count]);

        let row_y = list_start_y + visible_idx as u16;
        if row_y < inner.y + inner.height - 1 {
            let row_area = Rect::new(inner.x, row_y, inner.width, 1);
            frame.render_widget(Paragraph::new(line).style(row_style), row_area);
        }
    }

    // Hint bar at the bottom
    let hint_y = inner.y + inner.height - 1;
    let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
    let hint = Line::from(vec![Span::styled(
        "j/k: navigate  Enter: assign  ^n new  ^e edit  ^d delete  Esc: cancel",
        Style::default().fg(theme::overlay_hint()),
    )]);
    frame.render_widget(Paragraph::new(hint), hint_area);
}
