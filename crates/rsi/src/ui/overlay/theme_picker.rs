//! Theme picker popup rendering.

use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};
use std::ops::Range;

use super::fixed_centered_rect;

const MARKER_WIDTH: usize = 2;
const SWATCH_COUNT: usize = 8;
const SWATCH_CELL_WIDTH: usize = 3;
const POPUP_HORIZONTAL_OVERHEAD: usize = 4; // two borders + horizontal padding

fn theme_name_width() -> usize {
    (0..theme::theme_count())
        .map(|index| theme::theme_display_name(index).chars().count())
        .max()
        .unwrap_or(0)
}

fn theme_number_width(theme_count: usize) -> usize {
    theme_count.max(1).to_string().len()
}

fn picker_hint(theme_count: usize) -> String {
    format!(
        "j/k: navigate  Enter: select  1-{}: direct  Esc: cancel",
        theme_count.min(9)
    )
}

fn picker_row_width(theme_count: usize) -> usize {
    MARKER_WIDTH
        + theme_number_width(theme_count)
        + 1
        + theme_name_width()
        + 1
        + SWATCH_COUNT * SWATCH_CELL_WIDTH
}

fn picker_popup_width(theme_count: usize) -> u16 {
    picker_row_width(theme_count)
        .max(picker_hint(theme_count).chars().count())
        .saturating_add(POPUP_HORIZONTAL_OVERHEAD)
        .min(u16::MAX as usize) as u16
}

fn visible_theme_range(theme_count: usize, selected_index: usize, capacity: usize) -> Range<usize> {
    if theme_count == 0 || capacity == 0 {
        return 0..0;
    }
    let capacity = capacity.min(theme_count);
    let selected = selected_index.min(theme_count - 1);
    let start = selected
        .saturating_sub(capacity / 2)
        .min(theme_count.saturating_sub(capacity));
    start..start + capacity
}

/// Render the theme picker popup.
pub(super) fn render_theme_picker(
    frame: &mut Frame,
    area: Rect,
    selected_index: usize,
    original_index: usize,
) {
    let theme_count = theme::theme_count();
    let popup_height = theme_count as u16 + 4;
    let popup_area = fixed_centered_rect(area, picker_popup_width(theme_count), popup_height);

    frame.render_widget(Clear, popup_area);

    let block = theme::overlay_block()
        .title(Line::from(vec![Span::styled(
            " Theme ",
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

    let capacity = inner.height.saturating_sub(1) as usize;
    let visible = visible_theme_range(theme_count, selected_index, capacity);
    let start = visible.start;
    let number_width = theme_number_width(theme_count);
    let name_width = theme_name_width();

    for i in visible {
        let is_selected = i == selected_index;
        let is_current = i == original_index;

        let marker = if is_current { "✓ " } else { "  " };
        let number = format!("{:>width$} ", i + 1, width = number_width);

        let row_style = if is_selected {
            Style::default()
                .fg(theme::text())
                .bg(theme::surface2())
                .add_modifier(Modifier::REVERSED)
        } else {
            Style::default().bg(theme::overlay_bg())
        };

        let mut spans = vec![
            Span::styled(marker, Style::default().fg(theme::green())),
            Span::styled(number, Style::default().fg(theme::overlay_hint())),
            Span::styled(
                format!(
                    "{:<width$}",
                    theme::theme_display_name(i),
                    width = name_width
                ),
                Style::default()
                    .fg(theme::text())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ];

        for color in theme::theme_swatch(i) {
            spans.push(Span::styled("██", Style::default().fg(color)));
            spans.push(Span::raw(" "));
        }

        let line = Line::from(spans);
        let row_area = Rect::new(inner.x, inner.y + (i - start) as u16, inner.width, 1);
        frame.render_widget(Paragraph::new(line).style(row_style), row_area);
    }

    let hint_y = inner.y + inner.height - 1;
    let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
    let hint = Line::from(vec![Span::styled(
        picker_hint(theme_count),
        Style::default().fg(theme::overlay_hint()),
    )]);
    frame.render_widget(Paragraph::new(hint), hint_area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_picker_layout_fits_longest_name_swatches_and_hint() {
        assert_eq!(theme::theme_count(), 14);
        assert_eq!(theme_name_width(), 24);
        assert_eq!(theme_number_width(14), 2);
        assert_eq!(picker_row_width(14), 54);
        assert_eq!(picker_hint(14).chars().count(), 54);
        assert_eq!(picker_popup_width(14), 58);
    }

    #[test]
    fn viewport_keeps_top_middle_and_bottom_selections_visible() {
        assert_eq!(visible_theme_range(13, 0, 5), 0..5);
        assert_eq!(visible_theme_range(13, 6, 5), 4..9);
        assert_eq!(visible_theme_range(13, 12, 5), 8..13);
        assert_eq!(visible_theme_range(13, usize::MAX, 5), 8..13);
    }

    #[test]
    fn viewport_handles_full_and_zero_capacity() {
        assert_eq!(visible_theme_range(13, 12, 20), 0..13);
        assert_eq!(visible_theme_range(13, 4, 0), 0..0);
        assert_eq!(visible_theme_range(0, 0, 5), 0..0);
    }
}
