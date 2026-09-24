//! Provider form rendering.
//!
//! The form uses the shared vim-textarea surface for every field and expands
//! into a roomier layout on larger terminals so the textareas feel closer to
//! the new-session popup.

use crate::input_surface::InputSurface;
use crate::types::PopupMode;
use crate::ui::{session, theme};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;

/// Render the custom provider add/edit form overlay.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_provider_form(
    frame: &mut Frame,
    area: Rect,
    focused_field: usize,
    name: &InputSurface,
    base_url: &InputSurface,
    api_key: &InputSurface,
    default_model: &InputSurface,
    is_editing: bool,
) {
    let spacious_layout = area.height >= 18;
    let field_body_height: u16 = if spacious_layout { 2 } else { 1 };
    let popup_width = if spacious_layout {
        (area.width.saturating_mul(78) / 100).clamp(72, 120)
    } else {
        (area.width.saturating_mul(72) / 100).clamp(70, 110)
    };
    let popup_height: u16 = if spacious_layout { 18 } else { 14 };
    let popup_area = fixed_centered_rect(area, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    let title = if is_editing {
        " Edit Provider "
    } else {
        " New Provider "
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

    let required_inner_height = 4 * (1 + field_body_height) + 3 + 1;
    if inner.height < required_inner_height {
        return;
    }

    let mut row_y = inner.y;
    render_field_row(
        frame,
        inner,
        row_y,
        "Name",
        name,
        focused_field == 0,
        false,
        field_body_height,
    );
    row_y += 1 + field_body_height + 1;
    render_field_row(
        frame,
        inner,
        row_y,
        "URL",
        base_url,
        focused_field == 1,
        false,
        field_body_height,
    );
    row_y += 1 + field_body_height + 1;
    render_field_row(
        frame,
        inner,
        row_y,
        "API Key",
        api_key,
        focused_field == 2,
        true,
        field_body_height,
    );
    row_y += 1 + field_body_height + 1;
    render_field_row(
        frame,
        inner,
        row_y,
        "Model",
        default_model,
        focused_field == 3,
        false,
        field_body_height,
    );

    let focused_mode = match focused_field {
        0 => name.mode,
        1 => base_url.mode,
        2 => api_key.mode,
        3 => default_model.mode,
        _ => PopupMode::Normal,
    };

    let hint_y = inner.y + inner.height - 1;
    let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
    let hint_line = if focused_mode == PopupMode::Insert {
        Line::from(vec![
            Span::styled(
                " INSERT ",
                Style::default()
                    .fg(theme::overlay_mode_insert_fg())
                    .bg(theme::overlay_mode_insert_bg())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                "Tab/Shift+Tab",
                Style::default()
                    .fg(theme::overlay_hint())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": fields  ", Style::default().fg(theme::overlay_hint())),
            Span::styled(
                "Ctrl+V",
                Style::default()
                    .fg(theme::overlay_hint())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": paste  ", Style::default().fg(theme::overlay_hint())),
            Span::styled(
                "Ctrl+Enter",
                Style::default()
                    .fg(theme::overlay_hint())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": save  ", Style::default().fg(theme::overlay_hint())),
            Span::styled(
                "Esc",
                Style::default()
                    .fg(theme::overlay_hint())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": normal", Style::default().fg(theme::overlay_hint())),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                " NORMAL ",
                Style::default()
                    .fg(theme::overlay_mode_normal_fg())
                    .bg(theme::overlay_mode_normal_bg())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                "Tab/Shift+Tab",
                Style::default()
                    .fg(theme::overlay_hint())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": fields  ", Style::default().fg(theme::overlay_hint())),
            Span::styled(
                "i",
                Style::default()
                    .fg(theme::overlay_hint())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": insert  ", Style::default().fg(theme::overlay_hint())),
            Span::styled(
                "q/Esc",
                Style::default()
                    .fg(theme::overlay_hint())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": close  ", Style::default().fg(theme::overlay_hint())),
            Span::styled(
                "Ctrl+V",
                Style::default()
                    .fg(theme::overlay_hint())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": paste", Style::default().fg(theme::overlay_hint())),
        ])
    };
    frame.render_widget(
        Paragraph::new(hint_line).style(Style::default().bg(theme::overlay_bg())),
        hint_area,
    );
}

fn render_field_row(
    frame: &mut Frame,
    area: Rect,
    row_y: u16,
    label: &str,
    surface: &InputSurface,
    focused: bool,
    mask_value: bool,
    body_height: u16,
) {
    let label_style = if focused {
        Style::default()
            .fg(theme::mauve())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::overlay_hint())
    };

    let label_line = Line::from(vec![
        Span::styled(format!("{:>10}", label), label_style),
        Span::styled(" ", Style::default().fg(theme::overlay_hint())),
        Span::styled(
            if focused { "●" } else { " " },
            Style::default().fg(theme::overlay_hint()),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(label_line).style(Style::default().bg(theme::overlay_bg())),
        Rect::new(area.x, row_y, area.width, 1),
    );

    let body_area = Rect::new(area.x, row_y + 1, area.width, body_height);
    surface.wrap_width.set(body_area.width as usize);
    let cursor_style = if focused {
        if surface.mode == PopupMode::Insert {
            session::CursorStyle::Beam
        } else {
            session::CursorStyle::Block
        }
    } else {
        session::CursorStyle::Hidden
    };
    let visual_sel = if focused && surface.vim_state.visual.is_some() {
        surface.textarea.selection_range()
    } else {
        None
    };
    let display_surface = if mask_value {
        masked_surface(surface)
    } else {
        surface.clone()
    };
    session::render_wrapped_textarea(
        frame,
        body_area,
        &display_surface.textarea,
        cursor_style,
        visual_sel,
        theme::overlay_bg(),
    );
}

fn masked_surface(surface: &InputSurface) -> InputSurface {
    let content = surface.content();
    if content.is_empty() {
        return surface.clone();
    }

    let mut masked = surface.clone();
    let masked_text = mask_text(&content);
    let masked_lines: Vec<String> = if masked_text.is_empty() {
        vec![String::new()]
    } else {
        masked_text.lines().map(str::to_string).collect()
    };
    let mut textarea = tui_textarea::TextArea::new(masked_lines);
    textarea.set_cursor_line_style(Style::default());
    textarea.set_block(ratatui::widgets::Block::default());
    *masked.textarea = textarea;

    let (cursor_row, cursor_col) = surface.textarea.cursor();
    masked.textarea.move_cursor(tui_textarea::CursorMove::Top);
    masked.textarea.move_cursor(tui_textarea::CursorMove::Head);
    for _ in 0..cursor_row {
        masked.textarea.move_cursor(tui_textarea::CursorMove::Down);
    }
    for _ in 0..cursor_col {
        masked
            .textarea
            .move_cursor(tui_textarea::CursorMove::Forward);
    }

    masked
}

fn mask_text(text: &str) -> String {
    let visible_tail = 4usize;
    let visible_chars = text.chars().filter(|&c| c != '\n').count();
    if visible_chars == 0 {
        return text.to_string();
    }

    if visible_chars <= visible_tail {
        let mut out = String::with_capacity(text.len());
        for ch in text.chars() {
            if ch == '\n' {
                out.push(ch);
            } else {
                out.push('*');
            }
        }
        return out;
    }

    let mask_until = visible_chars.saturating_sub(visible_tail);
    let mut seen = 0usize;
    let mut out = String::with_capacity(text.len());

    for ch in text.chars() {
        if ch == '\n' {
            out.push(ch);
            continue;
        }
        if seen < mask_until {
            out.push('*');
        } else {
            out.push(ch);
        }
        seen += 1;
    }

    out
}
