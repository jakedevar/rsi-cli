//! Sort picker rendering.

use crate::app::SortOrder;
use crate::ui::navigator_layout::{display_width, span_cells};
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;

const TITLE: &str = " Sort sessions ";
const HINT: &str = "j/k choose · Enter apply · Esc cancel";
/// `✓ ` before the current order, two blanks before the others.
const MARKER_CELLS: usize = 2;
/// Gap between the name column and its description.
const GAP_CELLS: usize = 2;
/// Borders plus the one-cell horizontal padding on each side.
const CHROME_CELLS: usize = 4;

fn label_column_cells() -> usize {
    SortOrder::ALL
        .iter()
        .map(|order| display_width(order.label()))
        .max()
        .unwrap_or(0)
}

/// Popup size that fits every name, its description, the hint and the title,
/// so no option is truncated when the terminal is wide enough to hold it.
/// Height: borders, one row per option, a blank row, then the hint.
pub(super) fn sort_picker_size() -> (u16, u16) {
    let description_cells = SortOrder::ALL
        .iter()
        .map(|order| display_width(order.description()))
        .max()
        .unwrap_or(0);
    let row_cells = MARKER_CELLS + label_column_cells() + GAP_CELLS + description_cells;
    let inner_cells = row_cells
        .max(display_width(HINT))
        .max(display_width(TITLE) + 2);
    let width = u16::try_from(inner_cells + CHROME_CELLS).unwrap_or(u16::MAX);
    let height = u16::try_from(SortOrder::ALL.len() + 4).unwrap_or(u16::MAX);
    (width, height)
}

/// Render the sort order picker popup.
pub(super) fn render_sort_picker(
    frame: &mut Frame,
    area: Rect,
    selected_index: usize,
    current_order: SortOrder,
) {
    let options = SortOrder::ALL;
    let (popup_width, popup_height) = sort_picker_size();
    let popup_area = fixed_centered_rect(area, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    let block = theme::overlay_block()
        .title(Line::from(vec![Span::styled(
            TITLE,
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

    let label_cells = label_column_cells();
    for (i, order) in options.iter().enumerate() {
        if i as u16 >= inner.height.saturating_sub(1) {
            break;
        }

        let is_selected = i == selected_index;
        let is_current = *order == current_order;

        let marker = if is_current { "✓ " } else { "  " };

        let row_style = if is_selected {
            Style::default()
                .fg(theme::text())
                .bg(theme::surface2())
                .add_modifier(Modifier::REVERSED)
        } else {
            Style::default().bg(theme::overlay_bg())
        };

        let line = Line::from(vec![
            Span::styled(marker, Style::default().fg(theme::green())),
            Span::styled(
                span_cells(order.label(), label_cells),
                Style::default()
                    .fg(theme::text())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(GAP_CELLS)),
            Span::styled(
                order.description(),
                Style::default().fg(theme::overlay_hint()),
            ),
        ]);

        let row_area = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
        frame.render_widget(Paragraph::new(line).style(row_style), row_area);
    }

    // Hint bar
    let hint_y = inner.y + inner.height - 1;
    let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
    let hint = Line::from(vec![Span::styled(
        HINT,
        Style::default().fg(theme::overlay_hint()),
    )]);
    frame.render_widget(Paragraph::new(hint), hint_area);
}
