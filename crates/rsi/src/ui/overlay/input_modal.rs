//! Input modal rendering — quarter-size centered popup for longer-form input.
//!
//! Activated by Ctrl+G from session detail. Shows only the textarea and a hint bar.
//! Does NOT display working directory or model name.

use crate::input_surface::InputSurface;
use crate::types::{ModalGeometry, PopupMode};
use crate::ui::{session, theme};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Padding, Paragraph, Wrap};

/// Render the input modal overlay.
pub(super) fn render_input_modal(
    frame: &mut Frame,
    area: Rect,
    surface: &InputSurface,
    popup_rect_override: Option<Rect>,
    geom: &ModalGeometry,
) {
    let has_preview = surface.corrected_preview.is_some();

    let base_popup_area = if let Some(rect) = popup_rect_override {
        rect
    } else if has_preview {
        // Dynamic side-by-side layout: size reacts to both textarea and compiled output.
        // 60 % cap (was 75 %) so the modal doesn't dominate the viewport.
        let max_h = (area.height * 60 / 100).max(10);
        super::compute_dynamic_preview_rect(
            area,
            &surface.textarea,
            surface.corrected_preview.as_deref().unwrap_or(""),
            3,
            3,
            area.y + 1,
            max_h,
            None,
            None,
        )
    } else {
        // Dynamic height: 3 lines minimum textarea, grows with content.
        // 50 % cap (was 70 %) — stays consistent with the prompt popup so a long
        // pasted prompt doesn't push the modal past half the viewport.
        let max_h = (area.height * 50 / 100).max(6);
        super::compute_dynamic_popup_rect(
            area,
            &surface.textarea,
            3,
            3,
            area.y + 1,
            max_h,
            None,
            None,
        )
    };
    let popup_area = super::apply_geometry_deltas(base_popup_area, geom, area);

    // Clear the popup area
    frame.render_widget(Clear, popup_area);

    // Block styling — use the overlay block style
    let block = theme::overlay_block().padding(Padding::horizontal(1));
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if inner.height < 3 {
        return; // Too small
    }

    // Layout: textarea (fill), hint bar (1 row at bottom)
    let textarea_height = inner.height.saturating_sub(1);
    if textarea_height == 0 {
        return;
    }
    let textarea_area = Rect::new(inner.x, inner.y, inner.width, textarea_height);
    let cursor_style = if surface.mode == PopupMode::Insert {
        session::CursorStyle::Beam
    } else {
        session::CursorStyle::Block
    };
    let visual_sel = if surface.vim_state.visual.is_some() {
        surface.textarea.selection_range()
    } else {
        None
    };

    if has_preview {
        // Split layout: left = textarea, right = corrected preview
        let left_width = textarea_area.width / 2;
        let right_width = textarea_area.width.saturating_sub(left_width);
        let left = Rect::new(
            textarea_area.x,
            textarea_area.y,
            left_width,
            textarea_area.height,
        );
        let right = Rect::new(
            textarea_area.x + left_width,
            textarea_area.y,
            right_width,
            textarea_area.height,
        );

        surface.wrap_width.set(left.width as usize);
        session::render_wrapped_textarea(
            frame,
            left,
            &surface.textarea,
            cursor_style,
            visual_sel,
            theme::overlay_bg(),
        );

        if let Some(ref preview) = surface.corrected_preview {
            let is_clarify = preview.starts_with("CLARIFY:");
            let title_text = if is_clarify {
                " clarify "
            } else {
                " compiled "
            };
            let title_color = if is_clarify {
                theme::peach()
            } else {
                theme::accent()
            };

            let preview_block = Block::default()
                .borders(ratatui::widgets::Borders::LEFT)
                .border_style(Style::default().fg(theme::overlay_border()))
                .title(Span::styled(
                    title_text,
                    Style::default()
                        .fg(title_color)
                        .add_modifier(Modifier::ITALIC),
                ))
                .style(Style::default().bg(theme::overlay_bg()));

            let inner_right = preview_block.inner(right);
            frame.render_widget(preview_block, right);
            frame.render_widget(
                Paragraph::new(preview.as_str())
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(theme::text()).bg(theme::overlay_bg())),
                inner_right,
            );
        }
    } else {
        surface.wrap_width.set(textarea_area.width as usize);
        session::render_wrapped_textarea(
            frame,
            textarea_area,
            &surface.textarea,
            cursor_style,
            visual_sel,
            theme::overlay_bg(),
        );
    }

    // Hint bar at bottom
    let hint_y = inner.y + inner.height - 1;
    let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);

    if has_preview {
        let is_clarify = surface
            .corrected_preview
            .as_deref()
            .is_some_and(|p| p.starts_with("CLARIFY:"));
        let hint_line = if is_clarify {
            Line::from(vec![
                Span::styled(
                    " CLARIFY ",
                    Style::default()
                        .fg(theme::overlay_mode_normal_fg())
                        .bg(theme::peach())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(
                    "d",
                    Style::default()
                        .fg(theme::overlay_hint())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(": dismiss  ", Style::default().fg(theme::overlay_hint())),
                Span::styled("Ctrl+Y", Style::default().fg(theme::overlay_hint())),
                Span::styled(" to retry", Style::default().fg(theme::overlay_hint())),
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    " PREVIEW ",
                    Style::default()
                        .fg(theme::overlay_mode_normal_fg())
                        .bg(theme::overlay_mode_normal_bg())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(
                    "a",
                    Style::default()
                        .fg(theme::accent())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(": accept  ", Style::default().fg(theme::overlay_hint())),
                Span::styled(
                    "d",
                    Style::default()
                        .fg(theme::overlay_hint())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(": discard  ", Style::default().fg(theme::overlay_hint())),
                Span::styled(
                    "Ctrl+Enter",
                    Style::default()
                        .fg(theme::overlay_hint())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    ": send original",
                    Style::default().fg(theme::overlay_hint()),
                ),
            ])
        };
        frame.render_widget(
            Paragraph::new(hint_line).style(Style::default().bg(theme::overlay_bg())),
            hint_area,
        );
    } else if surface.correction_in_flight {
        let hint_line = Line::from(vec![
            Span::styled(
                " COMPILING... ",
                Style::default()
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                "Ctrl+Enter: send now",
                Style::default().fg(theme::overlay_hint()),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(hint_line).style(Style::default().bg(theme::overlay_bg())),
            hint_area,
        );
    } else {
        let (mode_label, mode_fg, mode_bg, hints) = if surface.vim_state.visual.is_some() {
            let label = match surface.vim_state.visual {
                Some(crate::vim_textarea::VisualMode::Char) => " VISUAL ",
                Some(crate::vim_textarea::VisualMode::Line) => " V-LINE ",
                None => unreachable!(),
            };
            (
                label,
                theme::overlay_mode_normal_fg(),
                theme::overlay_mode_normal_bg(),
                "d: delete  c: change  y: yank  Esc: cancel",
            )
        } else if let Some(op) = surface.vim_state.pending_operator {
            let label = match op {
                'd' => " d... ",
                'c' => " c... ",
                'y' => " y... ",
                _ => " ?... ",
            };
            (
                label,
                theme::overlay_mode_normal_fg(),
                theme::overlay_mode_normal_bg(),
                "waiting for motion (d/w/b/e/$)",
            )
        } else {
            match surface.mode {
                PopupMode::Insert => (
                    " INSERT ",
                    theme::overlay_mode_insert_fg(),
                    theme::overlay_mode_insert_bg(),
                    "Ctrl+Enter: send  Esc: normal",
                ),
                PopupMode::Normal => (
                    " NORMAL ",
                    theme::overlay_mode_normal_fg(),
                    theme::overlay_mode_normal_bg(),
                    "Ctrl+Enter: send  q: close  i: insert",
                ),
            }
        };

        let hint_line = Line::from(vec![
            Span::styled(
                mode_label,
                Style::default()
                    .fg(mode_fg)
                    .bg(mode_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(hints, Style::default().fg(theme::overlay_hint())),
        ]);
        frame.render_widget(
            Paragraph::new(hint_line).style(Style::default().bg(theme::overlay_bg())),
            hint_area,
        );
    }
}
