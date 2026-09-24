//! Prompt popup rendering.

use crate::input_surface::InputSurface;
use crate::suggestions::CommandSuggestion;
use crate::types::{ModalGeometry, ModelDropdownState, PopupMode, PromptPurpose};
use crate::ui::{session, theme};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};

use super::suggestion_dropdown;

/// Render the session prompt popup.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_prompt_popup(
    frame: &mut Frame,
    area: Rect,
    surface: &InputSurface,
    working_dir: &std::path::Path,
    purpose: &PromptPurpose,
    available_commands: &[CommandSuggestion],
    selected_model: Option<&str>,
    selected_effort: Option<&str>,
    popup_rect_override: Option<Rect>,
    focused: bool,
    session_list_width: Option<u16>,
    geom: &ModalGeometry,
    model_override: Option<&str>,
    model_dropdown: Option<&ModelDropdownState>,
    sandbox_enabled: bool,
    sandbox_supported: bool,
) {
    let has_preview = surface.corrected_preview.is_some();

    let base_popup_area = if let Some(rect) = popup_rect_override {
        rect
    } else if has_preview {
        // Dynamic side-by-side layout: size reacts to both textarea and compiled output.
        // 60 % cap (was 75 %) — the popup grows with content but never feels like it
        // "takes over" the screen when the compiler output is long.
        let max_h = (area.height * 60 / 100).max(10);
        super::compute_dynamic_preview_rect(
            area,
            &surface.textarea,
            surface.corrected_preview.as_deref().unwrap_or(""),
            6,
            3,
            area.y + 1,
            max_h,
            Some(purpose),
            session_list_width,
        )
    } else {
        // Dynamic height: 3 lines minimum textarea, grows with content.
        // 50 % cap (was 70 %) so a long pasted prompt doesn't push the popup past
        // half the viewport. compute_dynamic_popup_rect's MODAL_BOTTOM_MARGIN
        // additionally guarantees the popup never reaches the absolute screen bottom.
        let max_h = (area.height * 50 / 100).max(9);
        super::compute_dynamic_popup_rect_with_manual_height(
            area,
            &surface.textarea,
            6,
            3,
            area.y + 1,
            max_h,
            Some(purpose),
            session_list_width,
            geom,
        )
    };

    // Apply user geometry deltas (resize/move offsets) clamped to viewport
    let popup_area = super::apply_geometry_deltas(base_popup_area, geom, area);

    // Clear the popup area (erase whatever was rendered underneath)
    frame.render_widget(Clear, popup_area);

    // Dynamic block style based on purpose and focus state
    let base_block = if focused {
        match purpose {
            PromptPurpose::ContinueSession(_) => theme::overlay_block(),
            PromptPurpose::TaskRabbit => theme::taskrabbit_overlay_block(),
            PromptPurpose::Blank => theme::blank_overlay_block(),
            // Phase 4: typed-leaf prompts share Blank's chrome for now.
            PromptPurpose::CreateTyped { .. } => theme::blank_overlay_block(),
        }
    } else {
        theme::unfocused_overlay_block()
    };

    // Build the outer block — no title
    let block = base_block.padding(Padding::horizontal(1));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    // Layout inside the popup: cwd line (1), blank (1), textarea (fill), blank (1), hint bar (1)
    if inner.height < 4 {
        return; // Too small to render anything meaningful
    }

    // Row 0: working directory context + model indicator for Blank popup
    let cwd_display = working_dir.to_string_lossy().replacen(
        &dirs::home_dir().map_or(String::new(), |h| h.to_string_lossy().to_string()),
        "~",
        1,
    );
    let mut cwd_spans = vec![
        Span::styled("cwd: ", Style::default().fg(theme::overlay_hint())),
        Span::styled(
            cwd_display.to_string(),
            Style::default().fg(theme::overlay_cwd()),
        ),
    ];

    // Add model indicator next to CWD for Blank and TaskRabbit popups
    // Per-session override takes precedence over global selection.
    let effective_model = model_override.or(selected_model);
    if matches!(purpose, PromptPurpose::Blank | PromptPurpose::TaskRabbit) {
        let model_display = effective_model.unwrap_or("default");
        cwd_spans.push(Span::styled("  ", Style::default()));
        cwd_spans.push(Span::styled(
            format!("[{}]", model_display),
            Style::default()
                .fg(if model_override.is_some() {
                    theme::green() // visual indicator that override is active
                } else {
                    theme::mauve()
                })
                .add_modifier(Modifier::BOLD),
        ));
        // Add effort bars for reasoning-capable models.
        cwd_spans.extend(crate::ui::session::build_effort_bars(
            selected_effort,
            effective_model,
        ));
    }

    let cwd_line = Line::from(cwd_spans);
    let cwd_area = Rect::new(inner.x, inner.y, inner.width, 1);
    frame.render_widget(
        Paragraph::new(cwd_line).style(Style::default().bg(theme::overlay_bg())),
        cwd_area,
    );

    // Rows 2..(height-2): textarea
    let textarea_y = inner.y + 2;
    let textarea_height = inner.height.saturating_sub(4);
    if textarea_height == 0 {
        return;
    }
    let textarea_area = Rect::new(inner.x, textarea_y, inner.width, textarea_height);
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
        // Split layout: left = editable textarea, right = corrected preview
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

        // Render textarea on left
        surface.wrap_width.set(left.width as usize);
        session::render_wrapped_textarea(
            frame,
            left,
            &surface.textarea,
            cursor_style,
            visual_sel,
            theme::overlay_bg(),
        );

        // Render corrected preview on right
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
                .borders(Borders::LEFT)
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
        // Normal single-column render
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

    // Last row: mode indicator + key hints
    let hint_y = inner.y + inner.height - 1;
    let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);

    // When preview is showing, override with accept/discard hints
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
                Span::styled(
                    "compile prompt → Ctrl+Y",
                    Style::default().fg(theme::overlay_hint()),
                ),
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
        // Show COMPILING... badge when request is in-flight
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
                "d: delete  c: change  y: yank  Esc: cancel".to_string(),
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
                "waiting for motion (d/w/b/e/$)".to_string(),
            )
        } else {
            let is_oneshot = matches!(purpose, PromptPurpose::TaskRabbit);
            match surface.mode {
                PopupMode::Insert => (
                    " INSERT ",
                    theme::overlay_mode_insert_fg(),
                    theme::overlay_mode_insert_bg(),
                    if is_oneshot {
                        "Ctrl+Enter: run  Esc: normal".to_string()
                    } else {
                        "Ctrl+Enter: send  Esc: normal".to_string()
                    },
                ),
                PopupMode::Normal => {
                    let base = if is_oneshot {
                        "Ctrl+Enter: run  Ctrl+Q: cancel  i: insert"
                    } else {
                        "Ctrl+Enter: send  Ctrl+Q: cancel  i: insert"
                    };
                    // Show effort hint if model supports it
                    let effort_supported =
                        crate::ui::session::effort_bar_counts(selected_effort, effective_model).0
                            > 0;
                    let mut hints_str: String = if effort_supported
                        && matches!(purpose, PromptPurpose::Blank | PromptPurpose::TaskRabbit)
                    {
                        format!("{}  Ctrl+E: effort", base)
                    } else {
                        base.to_string()
                    };
                    // Show sandbox hint when daemon advertises capability and purpose supports it
                    if sandbox_supported
                        && matches!(purpose, PromptPurpose::Blank | PromptPurpose::TaskRabbit)
                    {
                        let sandbox_label = if sandbox_enabled { "on" } else { "off" };
                        hints_str.push_str(&format!("  Ctrl+B: sandbox {}", sandbox_label));
                    }
                    (
                        " NORMAL ",
                        theme::overlay_mode_normal_fg(),
                        theme::overlay_mode_normal_bg(),
                        hints_str,
                    )
                }
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

    // Suggestion dropdown rendered LAST — painter's algorithm ensures it overlays
    // the hint bar / mode indicator when the dropdown extends to the bottom rows.
    if !has_preview && surface.suggestions_visible && !surface.filtered_indices.is_empty() {
        suggestion_dropdown::render_suggestion_dropdown(
            frame,
            textarea_area,
            available_commands,
            &surface.file_paths,
            &surface.filtered_indices,
            surface.selected_suggestion,
            surface.suggestion_mode,
        );
    }

    // Model dropdown rendered AFTER suggestion dropdown (painter's algorithm).
    // Anchored below the CWD line where the model badge is displayed.
    if let Some(dd) = model_dropdown {
        if dd.open {
            let badge_anchor = Rect::new(inner.x, inner.y, inner.width, 1);
            crate::ui::widget::model_dropdown::render_model_dropdown(
                frame,
                area,
                badge_anchor,
                dd,
                effective_model,
                true, // provider availability checked at launch time
            );
        }
    }
}
