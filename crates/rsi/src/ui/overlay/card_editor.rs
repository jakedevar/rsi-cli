//! Card editor overlay rendering.
//!
//! Renders a centered popup showing the entity card facts with navigation
//! and inline editing. Information-dense, vim-grammar compatible.

use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;

/// Render the card editor overlay.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_card_editor(
    frame: &mut Frame,
    area: Rect,
    entity_type: &str,
    display_name: &str,
    facts: &[String],
    selected_index: usize,
    scroll_offset: usize,
    editing: Option<&str>,
    loading: bool,
) {
    let popup_width = 70u16.min(area.width.saturating_sub(4));
    // Height: header(1) + separator(1) + facts area(up to 22) + separator(1) + status(1) + hint(1) = 27 max
    let facts_area_height = 22u16.min(area.height.saturating_sub(10));
    let popup_height = (6 + facts_area_height).min(area.height.saturating_sub(2));
    let popup_area = fixed_centered_rect(area, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    let title = if entity_type == "user" {
        " User Card "
    } else {
        " Project Card "
    };

    let block = theme::overlay_block()
        .title(Line::from(vec![Span::styled(
            title,
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )]))
        .padding(Padding::horizontal(1));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if inner.height < 4 {
        return;
    }

    let mut y = inner.y;

    // Header: entity name
    let header_line = Line::from(vec![
        Span::styled(
            if entity_type == "user" {
                "User Preferences"
            } else {
                "Project: "
            },
            Style::default().fg(theme::overlay_hint()),
        ),
        if entity_type != "user" {
            Span::styled(
                display_name,
                Style::default()
                    .fg(theme::mauve())
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("")
        },
    ]);
    let header_area = Rect::new(inner.x, y, inner.width, 1);
    frame.render_widget(Paragraph::new(header_line), header_area);
    y += 1;

    // Separator
    let sep = Line::from(Span::styled(
        "\u{2500}".repeat(inner.width as usize),
        Style::default().fg(theme::surface1()),
    ));
    let sep_area = Rect::new(inner.x, y, inner.width, 1);
    frame.render_widget(Paragraph::new(sep), sep_area);
    y += 1;

    // Available rows for facts
    let facts_rows = (inner.y + inner.height).saturating_sub(y + 2) as usize; // -2 for status+hint

    if loading {
        let loading_line = Line::from(Span::styled(
            "Loading...",
            Style::default().fg(theme::overlay_hint()),
        ));
        let loading_area = Rect::new(inner.x, y, inner.width, 1);
        frame.render_widget(Paragraph::new(loading_line), loading_area);
    } else if facts.is_empty() {
        let empty_line = Line::from(Span::styled(
            "(empty) Press 'a' to add a fact",
            Style::default().fg(theme::overlay_hint()),
        ));
        let empty_area = Rect::new(inner.x, y, inner.width, 1);
        frame.render_widget(Paragraph::new(empty_line), empty_area);
    } else {
        // Render facts with scroll
        let visible_end = (scroll_offset + facts_rows).min(facts.len());
        for (vis_idx, fact_idx) in (scroll_offset..visible_end).enumerate() {
            let row_y = y + vis_idx as u16;
            if row_y >= inner.y + inner.height - 2 {
                break;
            }

            let is_selected = fact_idx == selected_index;
            let is_editing_this = is_selected && editing.is_some();

            // Index number
            let idx_str = format!("{:>2}. ", fact_idx + 1);

            // Available width for fact text
            let text_width = inner.width.saturating_sub(idx_str.len() as u16) as usize;

            if is_editing_this {
                // Render edit mode: show the editing buffer with cursor
                let edit_text = editing.unwrap_or("");
                let display_text = if edit_text.len() > text_width.saturating_sub(1) {
                    &edit_text[edit_text.len().saturating_sub(text_width.saturating_sub(1))..]
                } else {
                    edit_text
                };
                let line = Line::from(vec![
                    Span::styled(
                        &idx_str,
                        Style::default()
                            .fg(theme::mauve())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(display_text, Style::default().fg(theme::text())),
                    Span::styled("\u{2588}", Style::default().fg(theme::text())),
                ]);
                let row_area = Rect::new(inner.x, row_y, inner.width, 1);
                frame.render_widget(Paragraph::new(line), row_area);

                // Set cursor position for blinking
                let cursor_x = inner.x + idx_str.len() as u16 + display_text.len() as u16;
                if cursor_x < inner.x + inner.width {
                    frame.set_cursor_position(Position::new(cursor_x, row_y));
                }
            } else {
                // Normal fact display
                let fact = &facts[fact_idx];
                let display_fact = if fact.len() > text_width {
                    format!("{}...", &fact[..text_width.saturating_sub(3)])
                } else {
                    fact.to_string()
                };

                let (idx_style, text_style) = if is_selected {
                    (
                        Style::default()
                            .fg(theme::mauve())
                            .add_modifier(Modifier::BOLD),
                        Style::default()
                            .fg(theme::text())
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    (
                        Style::default().fg(theme::overlay_hint()),
                        Style::default().fg(theme::subtext0()),
                    )
                };

                let mut spans = vec![
                    Span::styled(&idx_str, idx_style),
                    Span::styled(display_fact, text_style),
                ];

                // Selection indicator
                if is_selected {
                    spans.insert(
                        0,
                        Span::styled("\u{25b8} ", Style::default().fg(theme::mauve())),
                    );
                } else {
                    spans.insert(0, Span::raw("  "));
                }

                let line = Line::from(spans);
                let row_area = Rect::new(inner.x, row_y, inner.width, 1);
                frame.render_widget(Paragraph::new(line), row_area);
            }
        }
    }

    // Status bar: fact count
    let status_y = inner.y + inner.height - 2;
    let sep2 = Line::from(Span::styled(
        "\u{2500}".repeat(inner.width as usize),
        Style::default().fg(theme::surface1()),
    ));
    let sep2_area = Rect::new(inner.x, status_y, inner.width, 1);
    frame.render_widget(Paragraph::new(sep2), sep2_area);

    // Hint bar
    let hint_y = inner.y + inner.height - 1;
    let mode_str = if editing.is_some() {
        "EDITING"
    } else {
        "NAVIGATE"
    };
    let count_str = format!("{}/40", facts.len());

    let hint_spans = if editing.is_some() {
        vec![
            Span::styled(
                format!(" {} ", mode_str),
                Style::default()
                    .fg(theme::base())
                    .bg(theme::green())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " Enter:save  Esc:cancel",
                Style::default().fg(theme::overlay_hint()),
            ),
            Span::styled(
                format!("  {}", count_str),
                Style::default().fg(theme::overlay_hint()),
            ),
        ]
    } else {
        vec![
            Span::styled(
                format!(" {} ", mode_str),
                Style::default()
                    .fg(theme::base())
                    .bg(theme::blue())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " a:add  e:edit  dd:del  J/K:move  Esc:close",
                Style::default().fg(theme::overlay_hint()),
            ),
            Span::styled(
                format!("  {}", count_str),
                Style::default().fg(theme::overlay_hint()),
            ),
        ]
    };

    let hint_line = Line::from(hint_spans);
    let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
    frame.render_widget(Paragraph::new(hint_line), hint_area);
}
