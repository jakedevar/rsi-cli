//! Prompt preview overlay rendering.

use crate::app::App;
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph, Wrap};

use super::fixed_centered_rect;

/// Render the prompt preview popup (shows full query for selected session).
pub(super) fn render_prompt_preview(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    scroll_offset: usize,
) {
    // Look up selected session
    let session_id = match app.selected_session_id() {
        Some(id) => id,
        None => return,
    };
    let state = match app.sessions.get(&session_id) {
        Some(s) => s,
        None => return,
    };

    // Build display: title (or query fallback) + description paragraph if available
    let title_text = crate::types::resolve_session_display_identity(&state.session, &app.sessions)
        .effective_title;
    let content = if let Some(desc) = state.session.description.as_deref() {
        if desc.is_empty() {
            title_text.clone()
        } else {
            format!("{}\n\n{}", title_text, desc)
        }
    } else {
        title_text
    };
    let content = content.as_str();

    // Build pipeline command button lines from session state
    let button_lines: Vec<Line<'static>> =
        super::super::content::build_docregblock_button_lines(&state.docregblock_contents);
    let button_count = button_lines.len() as u16;

    // Dynamic width: based on longest line, clamped to [30..100] and max 80% of terminal
    let max_line_width = content.lines().map(|l| l.len()).max().unwrap_or(0) as u16;
    let desired_width = (max_line_width + 6)
        .clamp(30, 100)
        .min(area.width * 80 / 100);

    // Dynamic height: estimate wrapped line count at the chosen width
    let inner_width = desired_width.saturating_sub(4); // borders + padding
    let wrapped_lines: usize = content
        .lines()
        .map(|line| {
            if line.is_empty() {
                1
            } else {
                (line.len() as u16).div_ceil(inner_width.max(1)) as usize
            }
        })
        .sum();
    // Add 4 for borders (2) + title row padding (1) + hint bar (1) + button lines
    let desired_height =
        ((wrapped_lines as u16) + 4 + button_count).clamp(5, area.height * 70 / 100);

    let popup_area = fixed_centered_rect(area, desired_width, desired_height);

    frame.render_widget(Clear, popup_area);

    let block = theme::overlay_block()
        .title(Line::from(vec![Span::styled(
            " Preview ",
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

    // Reserve space: hint bar (1 row) + button lines at the bottom
    let reserved_bottom = 1 + button_count;
    let content_height = inner.height.saturating_sub(reserved_bottom);
    let content_area = Rect::new(inner.x, inner.y, inner.width, content_height);

    // Clamp scroll offset
    let max_scroll = wrapped_lines.saturating_sub(content_height as usize);
    let clamped_offset = scroll_offset.min(max_scroll);

    let paragraph = Paragraph::new(content)
        .style(Style::default().fg(theme::text()))
        .wrap(Wrap { trim: false })
        .scroll((clamped_offset as u16, 0));

    frame.render_widget(paragraph, content_area);

    // Docregblock button lines (between content and hint bar)
    let buttons_y = inner.y + content_height;
    for (i, btn_line) in button_lines.into_iter().enumerate() {
        let btn_area = Rect::new(inner.x, buttons_y + i as u16, inner.width, 1);
        frame.render_widget(Paragraph::new(btn_line), btn_area);
    }

    // Hint bar at the bottom
    let hint_y = inner.y + inner.height - 1;
    let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
    let hint_text = if button_count > 0 {
        "j/k: navigate  p/Esc: close  Enter: open  Space+x: execute  Ctrl+d/u: scroll"
    } else {
        "j/k: navigate  p/Esc: close  Enter: open  Ctrl+d/u: scroll"
    };
    let hint = Line::from(vec![Span::styled(
        hint_text,
        Style::default().fg(theme::overlay_hint()),
    )]);
    frame.render_widget(Paragraph::new(hint), hint_area);
}
