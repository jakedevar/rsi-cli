//! Session-detail model/effort picker rendering (Issue #681).

use crate::overlay::model_switch::{ModelSwitchState, tuple_label};
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;

/// Most model rows shown at once; the list scrolls with the highlight.
const MAX_MODEL_ROWS: usize = 8;
/// Rows of chrome around the model list: border, current, queued, context
/// note, separator, effort row, hint and border.
const CHROME_ROWS: u16 = 8;

/// The model rows to draw for a highlight at `selected` in a list of `len`.
fn visible_window(selected: usize, len: usize) -> std::ops::Range<usize> {
    let rows = len.min(MAX_MODEL_ROWS);
    let start = (selected + 1).saturating_sub(rows);
    start..start + rows
}

pub(super) fn render_model_switch(frame: &mut Frame, area: Rect, state: &ModelSwitchState) {
    let window = visible_window(state.model_index, state.models.len());
    let height = window.len() as u16 + CHROME_ROWS;
    let popup_area = fixed_centered_rect(area, 72, height);
    frame.render_widget(Clear, popup_area);

    let block = theme::overlay_block()
        .title(Line::from(vec![Span::styled(
            " Switch Model / Effort ",
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )]))
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);
    if inner.height < 6 {
        return;
    }

    let label_style = Style::default().fg(theme::overlay_hint());
    let text_style = Style::default().fg(theme::text());
    let mut row = 0u16;
    let mut push = |frame: &mut Frame, line: Line<'static>| {
        if row < inner.height {
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(inner.x, inner.y + row, inner.width, 1),
            );
            row += 1;
        }
    };

    let current = state.options.model.as_deref().map_or_else(
        || "unknown".to_string(),
        |model| tuple_label(model, state.options.effort.as_deref()),
    );
    push(
        frame,
        Line::from(vec![
            Span::styled("Current  ", label_style),
            Span::styled(current, text_style),
        ]),
    );
    let queued = state.options.pending.as_ref().map_or_else(
        || "none".to_string(),
        |pending| tuple_label(&pending.model, pending.effort.as_deref()),
    );
    push(
        frame,
        Line::from(vec![
            Span::styled("Queued   ", label_style),
            Span::styled(queued, text_style),
        ]),
    );
    push(
        frame,
        Line::from(vec![
            Span::styled("Context  ", label_style),
            Span::styled(state.options.context_note.clone(), text_style),
        ]),
    );
    push(
        frame,
        Line::from(Span::styled(
            "\u{2500}".repeat(inner.width as usize),
            Style::default().fg(theme::surface2()),
        )),
    );

    for index in window {
        let (id, name) = &state.models[index];
        let selected = index == state.model_index;
        let is_current = state.options.model.as_deref() == Some(id.as_str());
        let marker = if is_current { "\u{25CF}" } else { " " };
        let style = if selected {
            Style::default()
                .fg(theme::overlay_title())
                .bg(theme::surface2())
                .add_modifier(Modifier::BOLD)
        } else {
            text_style
        };
        let label = if name == id {
            id.clone()
        } else {
            format!("{name}  ({id})")
        };
        push(
            frame,
            Line::from(vec![
                Span::styled(format!("{marker} "), label_style),
                Span::styled(label, style),
            ]),
        );
    }

    let mut effort_spans = vec![Span::styled("Effort   ", label_style)];
    for (index, choice) in state.efforts.iter().enumerate() {
        let name = choice.as_deref().unwrap_or("default");
        if index == state.effort_index {
            effort_spans.push(Span::styled(
                format!("[{name}]"),
                Style::default()
                    .fg(theme::overlay_title())
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            ));
        } else {
            effort_spans.push(Span::styled(format!(" {name} "), label_style));
        }
    }
    push(frame, Line::from(effort_spans));
    push(
        frame,
        Line::from(Span::styled(
            "j/k: model  h/l: effort  Enter: queue switch  Esc: cancel",
            label_style,
        )),
    );
}

#[cfg(test)]
mod tests {
    use super::visible_window;

    #[test]
    fn window_follows_the_highlight_and_never_exceeds_the_row_cap() {
        assert_eq!(visible_window(0, 3), 0..3);
        assert_eq!(visible_window(2, 20), 0..8);
        assert_eq!(visible_window(8, 20), 1..9);
        assert_eq!(visible_window(19, 20), 12..20);
    }
}
