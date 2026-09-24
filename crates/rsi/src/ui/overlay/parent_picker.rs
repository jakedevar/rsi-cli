//! Parent picker overlay rendering.

use crate::app::App;
use crate::overlay::parent_picker::filtered_indices;
use crate::ui::{session::kind_color, theme};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;

/// Render the parent picker popup for the focused session.
pub(super) fn render_parent_picker(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    candidates: &[uuid::Uuid],
    selected: usize,
    query: &str,
) {
    let visible: Vec<usize> = filtered_indices(app);
    let popup_height = (visible.len() as u16 + 5).clamp(8, 22);
    let popup_area = fixed_centered_rect(area, 60, popup_height);

    frame.render_widget(Clear, popup_area);

    let block = theme::overlay_block()
        .title(Line::from(vec![Span::styled(
            " Move To Parent ",
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

    // Row 0: query
    let q_line = Line::from(vec![
        Span::styled("> ", Style::default().fg(theme::mauve())),
        Span::styled(query.to_string(), Style::default().fg(theme::text())),
        Span::styled("\u{2588}", Style::default().fg(theme::text())),
    ]);
    frame.render_widget(
        Paragraph::new(q_line),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    // Separator
    let sep = "\u{2500}".repeat(inner.width as usize);
    frame.render_widget(
        Paragraph::new(sep).style(Style::default().fg(theme::surface2())),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );

    // List rows
    let list_y = inner.y + 2;
    let list_h = inner.height.saturating_sub(3);

    for (visible_idx, &orig_idx) in visible.iter().enumerate().take(list_h as usize) {
        let row_y = list_y + visible_idx as u16;
        if row_y >= inner.y + inner.height - 1 {
            break;
        }
        let cand_id = candidates.get(orig_idx).copied().unwrap_or_default();
        let is_selected = visible_idx == selected;
        let row_style = if is_selected {
            Style::default().bg(theme::surface2())
        } else {
            Style::default().bg(theme::overlay_bg())
        };

        let line = if cand_id.is_nil() {
            Line::from(vec![
                Span::styled(
                    "  ROOT  ",
                    Style::default()
                        .bg(theme::overlay1())
                        .fg(theme::overlay_bg())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled("[root]", Style::default().fg(theme::text())),
            ])
        } else if let Some(state) = app.sessions.get(&cand_id) {
            let kind = state.session.session_kind;
            let kind_label = format!(" {:?} ", kind);
            let kind_fg = kind_color(kind).unwrap_or_else(theme::accent);
            let label =
                crate::types::resolve_session_display_identity(&state.session, &app.sessions)
                    .effective_title;
            Line::from(vec![
                Span::styled(
                    kind_label,
                    Style::default()
                        .bg(kind_fg)
                        .fg(theme::overlay_bg())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(label, Style::default().fg(theme::text())),
            ])
        } else {
            Line::from(vec![Span::styled(
                "(missing session)",
                Style::default().fg(theme::overlay_hint()),
            )])
        };

        frame.render_widget(
            Paragraph::new(line).style(row_style),
            Rect::new(inner.x, row_y, inner.width, 1),
        );
    }

    // Hint bar
    let hint_y = inner.y + inner.height - 1;
    let hint = Line::from(vec![Span::styled(
        "j/k: navigate  Enter: assign  Esc: cancel",
        Style::default().fg(theme::overlay_hint()),
    )]);
    frame.render_widget(
        Paragraph::new(hint),
        Rect::new(inner.x, hint_y, inner.width, 1),
    );
}
