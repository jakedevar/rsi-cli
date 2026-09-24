//! Overlay rendering — floating popups drawn on top of the main layout.

mod ai_chat;
mod ai_command;
mod budget_policy_form;
mod card_editor;
mod cohort_settlement;
mod color_customizer;
mod command_palette;
mod create_entity_form;
mod diagnostics;
mod dialectic;
mod esp_square;
mod file_explorer;
pub(crate) mod graph;
pub(crate) mod graph_layout;
mod harness_manager;
mod hook_form;
mod input_modal;
pub(crate) mod keybindings_help;
mod label_form;
mod label_picker;
mod manager_v2;
mod memory_search;
mod message_bridge_form;
mod notification_browser;
mod parent_picker;
mod project_form;
mod project_picker;
mod prompt;
mod prompt_preview;
mod provider_form;
mod question_modal;
mod rating;
pub(crate) mod recursive_dag;
mod rename_session;
mod schedule_browser;
mod schedule_form;
mod session_info;
mod sort_picker;
mod suggestion_dropdown;
mod telescope;
mod terminal;
mod text_area_bg_editor;
mod theme_picker;
mod theme_role_editor;
mod topology_preview;
mod trash_browser;

#[cfg(test)]
mod tests;

use crate::app::App;
use crate::types::{ModalGeometry, OverlayState, PromptPurpose};
use ratatui::Frame;
use ratatui::layout::Rect;
use std::collections::HashMap;

/// Compute a centered rectangle for the popup.
/// Uses 80% width and 70% height, clamped to [40..120] x [10..30].
#[cfg(test)]
fn centered_rect(area: Rect) -> Rect {
    let popup_width = (area.width * 80 / 100).clamp(40, 120);
    let popup_height = (area.height * 70 / 100).clamp(10, 30);
    let x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_height)) / 2;
    Rect::new(x, y, popup_width, popup_height)
}

/// Compute a centered rectangle with a fixed size.
pub(crate) fn fixed_centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

/// Compute a centered rectangle aligned to the top third of the screen.
/// Horizontally centered, vertically positioned at ~1/3 from top.
pub(crate) fn centered_top_third_rect(area: Rect) -> Rect {
    let popup_width = (area.width * 50 / 100).clamp(40, 100);
    let popup_height = (area.height * 50 / 100).clamp(8, 25);
    let x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    // Position at top third: y = ~1/3 of available height, clamped to at least 1 row from top
    let y = area.y + (area.height / 3).saturating_sub(popup_height / 2).max(1);
    Rect::new(x, y, popup_width, popup_height)
}

/// Compute a rectangle with a given width and 75% of screen height,
/// pinned so the top border sits immediately below the status bar (y = area.y + 1).
/// Horizontally centered. Width is clamped to available area.
pub(crate) fn tall_centered_rect(area: Rect, width: u16) -> Rect {
    let w = width.min(area.width);
    let h = (area.height * 75 / 100).max(10).min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    // Pin top to row 1 — status bar occupies row 0 of the full frame area.
    let y = area.y + 1;
    Rect::new(x, y, w, h)
}

/// Count the number of visual (wrapped) lines a textarea would produce at a given content width.
///
/// Mirrors the wrapping logic in `render_wrapped_textarea` (indent-aware word-boundary breaks)
/// so that height predictions match what actually renders.
fn count_visual_lines(textarea: &tui_textarea::TextArea<'_>, content_width: usize) -> usize {
    if content_width == 0 {
        return 0;
    }
    textarea
        .lines()
        .iter()
        .map(|line| {
            if line.is_empty() {
                return 1;
            }
            let indent_char_len = line.len() - line.trim_start().len();
            let mut count = 0usize;
            let mut remaining = line.as_str();
            let mut is_first = true;
            while !remaining.is_empty() {
                let effective_width = if is_first {
                    content_width
                } else {
                    content_width.saturating_sub(indent_char_len).max(1)
                };
                let chunk_len = if remaining.len() <= effective_width {
                    remaining.len()
                } else {
                    super::session::word_boundary_break(remaining, effective_width)
                };
                if chunk_len == 0 {
                    break;
                }
                count += 1;
                is_first = false;
                remaining = &remaining[chunk_len..];
            }
            count.max(1)
        })
        .sum::<usize>()
        .max(1)
}

/// Count the number of visual (wrapped) lines a plain string would produce at a given width.
///
/// Uses simple word-boundary wrapping (no indent awareness). Suitable for
/// prompt compiler preview text rendered via `Paragraph::new(text).wrap(Wrap { trim: false })`.
fn count_visual_lines_str(text: &str, content_width: usize) -> usize {
    if content_width == 0 {
        return 0;
    }
    text.lines()
        .chain(if text.ends_with('\n') { Some("") } else { None })
        .map(|line| {
            if line.is_empty() {
                return 1;
            }
            let mut count = 0usize;
            let mut remaining = line;
            while !remaining.is_empty() {
                let chunk_len = if remaining.len() <= content_width {
                    remaining.len()
                } else {
                    super::session::word_boundary_break(remaining, content_width)
                };
                if chunk_len == 0 {
                    break;
                }
                count += 1;
                remaining = &remaining[chunk_len..];
            }
            count.max(1)
        })
        .sum::<usize>()
        .max(1)
}

/// Bottom margin (rows) reserved below input/preview popups so they never render
/// flush against the absolute bottom of the screen. Keeps the bottom dashboard /
/// footer visible even when the popup grows to its content-driven max height,
/// and prevents the "expanding off-screen" feel when pasting long prompts.
const MODAL_BOTTOM_MARGIN: u16 = 2;

/// Compute a dynamic-height centered rectangle for text input overlays.
///
/// Height starts at `min_textarea_rows` (typically 3) and grows with content,
/// capped by the smaller of `max_h` and `area.height - y_pin - MODAL_BOTTOM_MARGIN`.
/// `chrome_rows` = fixed overhead rows (borders, CWD, spacers, hint bar).
/// `y_pin` = y coordinate to pin the popup's top edge.
/// `max_h` = maximum allowed height (caller-controlled, typically a percent of viewport).
pub(crate) fn compute_dynamic_popup_rect(
    area: Rect,
    textarea: &tui_textarea::TextArea<'_>,
    chrome_rows: u16,
    min_textarea_rows: u16,
    y_pin: u16,
    max_h: u16,
    _purpose: Option<&crate::types::PromptPurpose>,
    session_list_width: Option<u16>,
) -> Rect {
    let w = if let Some(list_w) = session_list_width {
        (list_w / 2).clamp(40, 100)
    } else {
        (area.width * 50 / 100).clamp(40, 100)
    };
    // Inner width = popup width minus borders (2) minus horizontal padding (2)
    let inner_w = w.saturating_sub(4) as usize;
    let visual_lines = count_visual_lines(textarea, inner_w) as u16;
    let textarea_rows = visual_lines.max(min_textarea_rows);
    // saturating_add: a pathological paste (visual_lines near u16::MAX) cannot
    // wrap around and produce a too-small total_h.
    let total_h = chrome_rows.saturating_add(textarea_rows);
    // Reserve MODAL_BOTTOM_MARGIN rows below the popup so it never touches the
    // absolute screen edge. saturating_sub degrades gracefully on tiny terminals.
    let available = (area.y + area.height)
        .saturating_sub(y_pin)
        .saturating_sub(MODAL_BOTTOM_MARGIN);
    let h = total_h.min(max_h).min(available);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    Rect::new(x, y_pin, w, h)
}

/// Compute the prompt popup's base rect with manual-height semantics.
///
/// A persisted height delta means the user has chosen the popup height. Keep
/// the content-driven floor stable so typing another line does not grow the
/// manually sized modal before `apply_geometry_deltas` reapplies the delta.
pub(crate) fn compute_dynamic_popup_rect_with_manual_height(
    area: Rect,
    textarea: &tui_textarea::TextArea<'_>,
    chrome_rows: u16,
    min_textarea_rows: u16,
    y_pin: u16,
    max_h: u16,
    purpose: Option<&crate::types::PromptPurpose>,
    session_list_width: Option<u16>,
    geom: &ModalGeometry,
) -> Rect {
    let mut rect = compute_dynamic_popup_rect(
        area,
        textarea,
        chrome_rows,
        min_textarea_rows,
        y_pin,
        max_h,
        purpose,
        session_list_width,
    );
    if geom.dh != 0 {
        let available = (area.y + area.height)
            .saturating_sub(y_pin)
            .saturating_sub(MODAL_BOTTOM_MARGIN);
        let manual_floor = manual_height_floor(chrome_rows, min_textarea_rows, max_h, available);
        rect.height = manual_floor;
    }
    rect
}

fn manual_height_floor(
    chrome_rows: u16,
    min_textarea_rows: u16,
    max_h: u16,
    available: u16,
) -> u16 {
    chrome_rows
        .saturating_add(min_textarea_rows)
        .min(max_h)
        .min(available)
}

fn manual_height_marker_key(geom_key: &str) -> String {
    format!("__manual_height_base:{geom_key}")
}

fn normalize_prompt_manual_height(
    modal_geometries: &mut HashMap<String, ModalGeometry>,
    geom_key: &str,
    area: Rect,
    textarea: &tui_textarea::TextArea<'_>,
    chrome_rows: u16,
    min_textarea_rows: u16,
    y_pin: u16,
    max_h: u16,
    purpose: Option<&PromptPurpose>,
    session_list_width: Option<u16>,
) -> bool {
    let marker_key = manual_height_marker_key(geom_key);
    let Some(geom) = modal_geometries.get(geom_key).cloned() else {
        return modal_geometries.remove(&marker_key).is_some();
    };
    if geom.dh == 0 {
        return modal_geometries.remove(&marker_key).is_some();
    }

    if modal_geometries.contains_key(&marker_key) {
        return false;
    }

    let dynamic = compute_dynamic_popup_rect(
        area,
        textarea,
        chrome_rows,
        min_textarea_rows,
        y_pin,
        max_h,
        purpose,
        session_list_width,
    );
    let available = (area.y + area.height)
        .saturating_sub(y_pin)
        .saturating_sub(MODAL_BOTTOM_MARGIN);
    let manual_floor = manual_height_floor(chrome_rows, min_textarea_rows, max_h, available);
    let content_extra = dynamic.height.saturating_sub(manual_floor);
    if content_extra == 0 {
        modal_geometries.insert(marker_key, ModalGeometry::default());
        return true;
    }

    if let Some(stored) = modal_geometries.get_mut(geom_key) {
        stored.dh = stored
            .dh
            .saturating_add(i16::try_from(content_extra).unwrap_or(i16::MAX));
    }
    modal_geometries.insert(marker_key, ModalGeometry::default());
    true
}

/// Compute a dynamic-height rectangle for preview mode (side-by-side layout).
///
/// Width is doubled (each half ≈ normal modal width) so the original textarea and compiled
/// preview each get the same column budget as the non-preview popup. Height is driven by
/// whichever pane has more visual lines.
pub(crate) fn compute_dynamic_preview_rect(
    area: Rect,
    textarea: &tui_textarea::TextArea<'_>,
    preview_text: &str,
    chrome_rows: u16,
    min_textarea_rows: u16,
    y_pin: u16,
    max_h: u16,
    _purpose: Option<&crate::types::PromptPurpose>,
    session_list_width: Option<u16>,
) -> Rect {
    // Base width = same formula as single-pane popup
    let base_w = if let Some(list_w) = session_list_width {
        (list_w / 2).clamp(40, 100)
    } else {
        (area.width * 50 / 100).clamp(40, 100)
    };
    // Double for side-by-side, clamp to screen
    let w = (base_w * 2).min(area.width);

    // Each half gets approximately half the total inner width
    // Inner width = popup width − borders(2) − padding(2)
    let half_inner_w = (w.saturating_sub(4) / 2) as usize;

    // Count visual lines for both panes at the half-width
    let textarea_lines = count_visual_lines(textarea, half_inner_w) as u16;
    let preview_lines = count_visual_lines_str(preview_text, half_inner_w) as u16;

    let content_lines = textarea_lines.max(preview_lines).max(min_textarea_rows);
    // saturating_add — guard against pathological content overflowing u16.
    let total_h = chrome_rows.saturating_add(content_lines);
    // Reserve MODAL_BOTTOM_MARGIN rows below the popup (see compute_dynamic_popup_rect
    // — the same off-screen-expansion bug applies in preview mode when the compiled
    // output is long).
    let available = (area.y + area.height)
        .saturating_sub(y_pin)
        .saturating_sub(MODAL_BOTTOM_MARGIN);
    let h = total_h.min(max_h).min(available);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    Rect::new(x, y_pin, w, h)
}

/// Apply geometry deltas to a base popup rect, clamping to stay within the viewport.
/// Min size: 20w x 5h. The modal is clamped to remain fully visible.
pub(crate) fn apply_geometry_deltas(base: Rect, geom: &ModalGeometry, viewport: Rect) -> Rect {
    let mut x = base.x as i16 + geom.dx;
    let mut y = base.y as i16 + geom.dy;
    let mut w = base.width as i16 + geom.dw;
    let mut h = base.height as i16 + geom.dh;

    // Minimum sizes
    w = w.max(20);
    h = h.max(5);

    // Clamp to viewport
    let vx = viewport.x as i16;
    let vy = viewport.y as i16;
    let vw = viewport.width as i16;
    let vh = viewport.height as i16;

    w = w.min(vw);
    h = h.min(vh);
    x = x.max(vx).min(vx + vw - w);
    y = y.max(vy).min(vy + vh - h);

    Rect::new(x as u16, y as u16, w as u16, h as u16)
}

/// Render the first (stacked) overlay when TaskRabbit is displayed simultaneously.
/// Returns the `Rect` used, or `None` if the overlay type doesn't support stacked rendering.
fn render_stacked_first_overlay(
    frame: &mut Frame,
    area: Rect,
    stacked: &OverlayState,
    app: &App,
) -> Option<Rect> {
    match stacked {
        OverlayState::Prompt {
            surface,
            working_dir,
            purpose,
            available_commands,
            model_override,
            model_dropdown,
            sandbox_enabled,
            ..
        } => {
            let max_h = (area.height * 50 / 100).max(9); // 6 chrome + 3 min textarea
            let geom_key = crate::app::App::geometry_key_for_purpose(purpose);
            let geom = app
                .modal_geometries
                .get(&geom_key)
                .cloned()
                .unwrap_or_default();
            let rect = if let Some(ref preview) = surface.corrected_preview {
                compute_dynamic_preview_rect(
                    area,
                    &surface.textarea,
                    preview,
                    6,
                    3,
                    area.y + 1,
                    max_h,
                    Some(purpose),
                    None,
                )
            } else {
                compute_dynamic_popup_rect_with_manual_height(
                    area,
                    &surface.textarea,
                    6,
                    3,
                    area.y + 1,
                    max_h,
                    Some(purpose),
                    None,
                    &geom,
                )
            };
            prompt::render_prompt_popup(
                frame,
                area,
                surface,
                working_dir,
                purpose,
                available_commands,
                app.selected_model.as_deref(),
                app.selected_effort.as_deref(),
                Some(rect),
                false, // stacked = not focused
                None,
                &geom,
                model_override.as_deref(),
                Some(model_dropdown),
                *sandbox_enabled,
                app.poll.sandbox_supported,
            );
            Some(rect)
        }
        OverlayState::InputModal { surface, .. } => {
            let max_h = (area.height * 50 / 100).max(6); // 3 chrome + 3 min textarea
            let rect = if let Some(ref preview) = surface.corrected_preview {
                compute_dynamic_preview_rect(
                    area,
                    &surface.textarea,
                    preview,
                    3,
                    3,
                    area.y + 1,
                    max_h,
                    None,
                    None,
                )
            } else {
                compute_dynamic_popup_rect(
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
            input_modal::render_input_modal(
                frame,
                area,
                surface,
                Some(rect),
                &ModalGeometry::default(),
            );
            Some(rect)
        }
        _ => None,
    }
}

/// Render the active overlay on top of the main layout.
/// Call this LAST in the render function (painter's algorithm).
pub fn render_overlay(frame: &mut Frame, area: Rect, app: &mut App) {
    // --- Input overlay stack (multiple Blank/TaskRabbit prompts) ---
    if !app.input_overlays.is_empty()
        && !matches!(app.overlay, OverlayState::KeybindingsHelp { .. })
    {
        // If a regular overlay is also active, paint it first (behind the input modal).
        if !matches!(app.overlay, OverlayState::None) {
            render_regular_overlay(frame, area, app);
        }
        render_input_overlay_stack(frame, area, app);
        return;
    }

    // --- Legacy stacked rendering (InputModal + TaskRabbit via overlay_stack) ---
    if let OverlayState::Prompt {
        surface: tr_surface,
        working_dir: tr_wd,
        purpose: purpose @ PromptPurpose::TaskRabbit,
        available_commands: tr_cmds,
        model_override: tr_model_override,
        model_dropdown: tr_model_dropdown,
        sandbox_enabled: tr_sandbox_enabled,
        ..
    } = &app.overlay
    {
        let list_w = crate::ui::compute_session_list_width(app);
        if let Some(stacked) = app.overlay_stack.last() {
            let first_rect = render_stacked_first_overlay(frame, area, stacked, app);
            if let Some(first_rect) = first_rect {
                // Render TaskRabbit below with 1-row gap
                let tr_y = first_rect.y + first_rect.height + 1;
                if tr_y < area.y + area.height {
                    let max_h = (area.y + area.height).saturating_sub(tr_y);
                    let geom_key = crate::app::App::geometry_key_for_purpose(purpose);
                    let geometry_changed = if tr_surface.corrected_preview.is_none() {
                        normalize_prompt_manual_height(
                            &mut app.modal_geometries,
                            &geom_key,
                            area,
                            &tr_surface.textarea,
                            6,
                            3,
                            tr_y,
                            max_h,
                            Some(purpose),
                            None,
                        )
                    } else {
                        false
                    };
                    if geometry_changed {
                        crate::state::PersistedState::capture(app).save();
                    }
                    let geom = app
                        .modal_geometries
                        .get(&geom_key)
                        .cloned()
                        .unwrap_or_default();
                    let tr_rect = if let Some(ref preview) = tr_surface.corrected_preview {
                        compute_dynamic_preview_rect(
                            area,
                            &tr_surface.textarea,
                            preview,
                            6,
                            3,
                            tr_y,
                            max_h,
                            Some(purpose),
                            None,
                        )
                    } else {
                        compute_dynamic_popup_rect_with_manual_height(
                            area,
                            &tr_surface.textarea,
                            6,
                            3,
                            tr_y,
                            max_h,
                            Some(purpose),
                            None,
                            &geom,
                        )
                    };
                    prompt::render_prompt_popup(
                        frame,
                        area,
                        tr_surface,
                        tr_wd,
                        purpose,
                        tr_cmds,
                        app.selected_model.as_deref(),
                        app.selected_effort.as_deref(),
                        Some(tr_rect),
                        true, // focused
                        Some(list_w),
                        &geom,
                        tr_model_override.as_deref(),
                        Some(tr_model_dropdown),
                        *tr_sandbox_enabled,
                        app.poll.sandbox_supported,
                    );
                }
                return;
            }
        }
    }

    render_regular_overlay(frame, area, app);
}

pub(crate) fn file_explorer_drawer_width(area_width: u16) -> u16 {
    file_explorer::drawer_width(area_width)
}

/// Render all input overlays as a vertical stack (oldest at top, newest at bottom).
/// When many overlays are open, scrolls so the focused overlay is always visible.
fn render_input_overlay_stack(frame: &mut Frame, area: Rect, app: &mut App) {
    let count = app.input_overlays.len();
    let list_w = crate::ui::compute_session_list_width(app);
    let selected_model = app.selected_model.clone();
    let selected_effort = app.selected_effort.clone();
    let sandbox_supported = app.poll.sandbox_supported;
    let mut geometry_changed = false;

    // Ensure the focused overlay is visible by estimating how many fit on screen
    let per_overlay_min: u16 = 10; // ~9 rows minimum + 1-row gap
    let available = area.height.saturating_sub(1); // below status bar
    let estimated_fit = if per_overlay_min > 0 {
        (available / per_overlay_min) as usize
    } else {
        count
    }
    .max(1);
    let scroll_start = if app.focused_input_idx >= estimated_fit {
        (app.focused_input_idx + 1).saturating_sub(estimated_fit)
    } else {
        0
    };

    let mut y = area.y + 1; // Start below status bar

    for (i, overlay) in app.input_overlays.iter().enumerate().skip(scroll_start) {
        let is_focused = i == app.focused_input_idx;

        if let OverlayState::Prompt {
            surface,
            working_dir,
            purpose,
            available_commands,
            model_override,
            model_dropdown,
            sandbox_enabled,
            ..
        } = overlay
        {
            let remaining_h = (area.y + area.height).saturating_sub(y);
            if remaining_h < 6 {
                break; // Not enough vertical space for another overlay
            }

            // Budget: each overlay gets a fair share, capped by content. Lower caps
            // (35 % multi / 50 % single, was 45 %/70 %) so multiple stacked overlays
            // still leave room for the bottom dashboard. compute_dynamic_*_rect
            // additionally enforces MODAL_BOTTOM_MARGIN so the bottom popup never
            // reaches the absolute screen edge.
            let per_overlay_max = if count > 1 {
                (area.height * 35 / 100).max(9)
            } else {
                (area.height * 50 / 100).max(9)
            };
            let max_h = remaining_h.min(per_overlay_max);
            let geom_key = crate::app::App::geometry_key_for_purpose(purpose);
            if is_focused && surface.corrected_preview.is_none() {
                geometry_changed |= normalize_prompt_manual_height(
                    &mut app.modal_geometries,
                    &geom_key,
                    area,
                    &surface.textarea,
                    6,
                    3,
                    y,
                    max_h,
                    Some(purpose),
                    None,
                );
            }
            let geom = app
                .modal_geometries
                .get(&geom_key)
                .cloned()
                .unwrap_or_default();

            let rect = if let Some(ref preview) = surface.corrected_preview {
                compute_dynamic_preview_rect(
                    area,
                    &surface.textarea,
                    preview,
                    6,
                    3,
                    y,
                    max_h,
                    Some(purpose),
                    None,
                )
            } else {
                compute_dynamic_popup_rect_with_manual_height(
                    area,
                    &surface.textarea,
                    6,
                    3,
                    y,
                    max_h,
                    Some(purpose),
                    None,
                    &geom,
                )
            };
            prompt::render_prompt_popup(
                frame,
                area,
                surface,
                working_dir,
                purpose,
                available_commands,
                selected_model.as_deref(),
                selected_effort.as_deref(),
                Some(rect),
                is_focused,
                Some(list_w),
                &geom,
                model_override.as_deref(),
                Some(model_dropdown),
                *sandbox_enabled,
                sandbox_supported,
            );

            y = rect.y + rect.height + 1; // 1-row gap
        }
    }
    if geometry_changed {
        crate::state::PersistedState::capture(app).save();
    }
}

/// Render overlays that live in `app.overlay` (everything except the input overlay stack).
fn render_regular_overlay(frame: &mut Frame, area: Rect, app: &mut App) {
    match &app.overlay {
        OverlayState::None => {}
        OverlayState::ThemePicker {
            selected_index,
            original_index,
        } => {
            theme_picker::render_theme_picker(frame, area, *selected_index, *original_index);
        }
        OverlayState::ThemeRoleEditor {
            role,
            input,
            assessment,
            committed,
            ..
        } => {
            theme_role_editor::render_theme_role_editor(
                frame,
                area,
                *role,
                input,
                *assessment,
                *committed,
            );
        }
        OverlayState::ColorCustomizer {
            focused_field,
            inputs,
            errors,
        } => {
            color_customizer::render_color_customizer(frame, area, *focused_field, inputs, errors);
        }
        OverlayState::TextAreaBgEditor { input, error } => {
            text_area_bg_editor::render_text_area_bg_editor(frame, area, input, *error);
        }
        OverlayState::Prompt {
            surface,
            working_dir,
            purpose,
            available_commands,
            model_override,
            model_dropdown,
            sandbox_enabled,
            ..
        } => {
            let geom_key = crate::app::App::geometry_key_for_purpose(purpose);
            let max_h = (area.height * 50 / 100).max(9);
            let geometry_changed = if surface.corrected_preview.is_none() {
                normalize_prompt_manual_height(
                    &mut app.modal_geometries,
                    &geom_key,
                    area,
                    &surface.textarea,
                    6,
                    3,
                    area.y + 1,
                    max_h,
                    Some(purpose),
                    None,
                )
            } else {
                false
            };
            if geometry_changed {
                crate::state::PersistedState::capture(app).save();
            }
            let geom = app
                .modal_geometries
                .get(&geom_key)
                .cloned()
                .unwrap_or_default();
            prompt::render_prompt_popup(
                frame,
                area,
                surface,
                working_dir,
                purpose,
                available_commands,
                app.selected_model.as_deref(),
                app.selected_effort.as_deref(),
                None,
                true, // focused
                None,
                &geom,
                model_override.as_deref(),
                Some(model_dropdown),
                *sandbox_enabled,
                app.poll.sandbox_supported,
            );
        }
        OverlayState::ProjectPicker {
            filter,
            selected_index,
            context,
        } => {
            project_picker::render_project_picker(
                frame,
                area,
                app,
                filter,
                *selected_index,
                context,
            );
        }
        OverlayState::KeybindingsHelp {
            scroll_offset,
            filter,
            search_active,
            origin,
            view: _,
        } => {
            keybindings_help::render_keybindings_help(
                frame,
                area,
                app,
                *scroll_offset,
                filter,
                *search_active,
                *origin,
            );
        }
        OverlayState::SortPicker { selected_index } => {
            sort_picker::render_sort_picker(frame, area, *selected_index, app.settings.sort_order);
        }
        OverlayState::PromptPreview { scroll_offset } => {
            prompt_preview::render_prompt_preview(frame, area, app, *scroll_offset);
        }
        OverlayState::SourceWorktreeSettlement(state) => {
            cohort_settlement::render_source_worktree_settlement(frame, area, state);
        }
        OverlayState::HarnessManagerV2(state) => {
            manager_v2::render(frame, area, state);
        }
        OverlayState::HarnessManagerScope(state) => {
            harness_manager::render(frame, area, state);
        }
        OverlayState::TrashBrowser {
            sessions,
            items,
            selected_index,
            scroll_offset,
        } => {
            trash_browser::render_trash_browser(
                frame,
                area,
                sessions,
                items,
                *selected_index,
                *scroll_offset,
            );
        }
        OverlayState::RecentCompletions { .. } => {
            // Focus-mode: renders in the persistent gutter window, not as a popup.
        }
        OverlayState::NotificationBrowser {
            selected_index,
            scroll_offset: _,
        } => {
            notification_browser::render_notification_browser(frame, area, app, *selected_index);
        }
        OverlayState::ProjectForm {
            focused_field,
            name,
            path,
            color_index,
            editing_id,
        } => {
            let workflow_status = editing_id
                .as_ref()
                .and_then(|id| app.workflow_statuses.get(id));
            project_form::render_project_form(
                frame,
                area,
                *focused_field,
                name,
                path,
                *color_index,
                editing_id.is_some(),
                workflow_status,
            );
        }
        OverlayState::ProviderForm {
            focused_field,
            name,
            base_url,
            api_key,
            default_model,
            editing_id,
        } => {
            provider_form::render_provider_form(
                frame,
                area,
                *focused_field,
                name,
                base_url,
                api_key,
                default_model,
                editing_id.is_some(),
            );
        }
        OverlayState::MessageBridgeForm {
            bridge,
            focused_field,
            enabled,
            account,
            allow_from,
            working_dir,
        } => {
            message_bridge_form::render_message_bridge_form(
                frame,
                area,
                *bridge,
                *focused_field,
                *enabled,
                account,
                allow_from,
                working_dir,
            );
        }
        OverlayState::HookForm {
            focused_field,
            event_idx,
            event_name_other,
            matcher,
            command,
            timeout,
            editing,
            ..
        } => {
            hook_form::render_hook_form(
                frame,
                area,
                *focused_field,
                *event_idx,
                event_name_other.as_deref(),
                matcher,
                command,
                timeout,
                editing.is_some(),
            );
        }
        OverlayState::HookConflictPrompt { .. } => {
            hook_form::render_hook_conflict(frame, area);
        }
        OverlayState::BudgetPolicyForm {
            focused_field,
            scope_kind_idx,
            scope_id,
            purpose,
            model_tier_idx,
            max_total_tokens,
            max_concurrency,
            max_calls_per_window,
            rate_window_seconds,
            alert_threshold_ratio,
            editing,
            ..
        } => {
            budget_policy_form::render_budget_policy_form(
                frame,
                area,
                *focused_field,
                *scope_kind_idx,
                scope_id,
                purpose,
                *model_tier_idx,
                max_total_tokens,
                max_concurrency,
                max_calls_per_window,
                rate_window_seconds,
                alert_threshold_ratio,
                editing.is_some(),
            );
        }
        OverlayState::SkillPreview {
            name,
            content,
            scroll_offset,
        } => {
            hook_form::render_skill_preview(frame, area, name, content, *scroll_offset);
        }
        OverlayState::Diagnostics => {
            diagnostics::render_diagnostics(frame, area, app);
        }
        OverlayState::RecursiveDagBrowser(state) => {
            recursive_dag::render_recursive_dag_browser(frame, area, app, state);
        }
        OverlayState::MemorySearch {
            query,
            results,
            selected_index,
            loading,
        } => {
            memory_search::render_memory_search(
                frame,
                area,
                query,
                results,
                *selected_index,
                *loading,
            );
        }
        OverlayState::FileExplorer {
            root,
            entries,
            selected_index,
            scroll_offset,
            show_hidden,
            finder_active,
            finder_query,
            finder_cache,
            finder_results,
            finder_selected,
            explorer_focused,
            ..
        } => {
            file_explorer::render_file_explorer(
                frame,
                area,
                root,
                entries,
                *selected_index,
                *scroll_offset,
                *show_hidden,
                *finder_active,
                finder_query,
                finder_cache,
                finder_results,
                *finder_selected,
                *explorer_focused,
            );
        }
        OverlayState::RenameSession { title, .. } => {
            rename_session::render_rename_session_overlay(frame, area, title);
        }
        OverlayState::QuestionModal {
            questions,
            current_question,
            cursor,
            selections,
            textarea,
            mode,
            ..
        } => {
            question_modal::render_question_modal(
                frame,
                area,
                app,
                questions,
                *current_question,
                cursor,
                selections,
                textarea,
                mode,
            );
        }
        OverlayState::LabelPicker {
            filter,
            selected_index,
        } => {
            label_picker::render_label_picker(frame, area, app, filter, *selected_index);
        }
        OverlayState::LabelForm {
            focused_field,
            name,
            description,
            color_index,
            editing_id,
        } => {
            label_form::render_label_form(
                frame,
                area,
                *focused_field,
                name,
                description,
                *color_index,
                editing_id.is_some(),
            );
        }
        OverlayState::InputModal { surface, .. } => {
            let geom = app.current_overlay_geometry();
            input_modal::render_input_modal(frame, area, surface, None, &geom);
        }
        OverlayState::EspSquare {
            round,
            correct,
            interactive,
            rounds,
            message,
            flash,
            cursor,
            ..
        } => {
            esp_square::render_esp_square(
                frame,
                area,
                *round,
                *correct,
                *interactive,
                rounds,
                message,
                *flash,
                *cursor,
            );
        }
        OverlayState::AiCommand {
            command, in_flight, ..
        } => {
            ai_command::render_ai_command(frame, area, command, *in_flight);
        }
        OverlayState::AiChat {
            messages,
            input,
            in_flight,
            scroll_offset,
            ..
        } => {
            ai_chat::render_ai_chat(frame, area, messages, input, *in_flight, *scroll_offset);
        }
        OverlayState::GraphReview { .. } => {
            render_graph_review_overlay(frame, area, app);
        }
        OverlayState::Telescope {
            query,
            file_cache,
            results,
            selected,
            ..
        } => {
            telescope::render_telescope(frame, area, query, file_cache, results, *selected);
        }
        OverlayState::CommandPalette {
            query,
            results,
            selected,
            argument_edit,
            argument_input,
            origin,
        } => {
            command_palette::render_command_palette(
                frame,
                area,
                query,
                results,
                *selected,
                *argument_edit,
                argument_input,
                origin.origin,
            );
        }
        OverlayState::CardEditor {
            entity_type,
            display_name,
            facts,
            selected_index,
            scroll_offset,
            editing,
            loading,
            ..
        } => {
            card_editor::render_card_editor(
                frame,
                area,
                entity_type,
                display_name,
                facts,
                *selected_index,
                *scroll_offset,
                editing.as_deref(),
                *loading,
            );
        }
        OverlayState::Dialectic {
            messages,
            sources,
            input,
            in_flight,
            scroll_offset,
            sources_expanded,
            ..
        } => {
            dialectic::render_dialectic(
                frame,
                area,
                messages,
                sources,
                input,
                *in_flight,
                *scroll_offset,
                *sources_expanded,
                0, // tool_calls count populated from response in Phase 3
            );
        }
        OverlayState::ScheduleBrowser {
            jobs,
            selected_index,
            loading,
            ..
        } => {
            schedule_browser::render_schedule_browser(frame, area, jobs, *selected_index, *loading);
        }
        OverlayState::ScheduleForm {
            focused_field,
            name,
            message,
            recurrence_index,
            interval,
            anchor_date,
            anchor_time,
            editing_id,
        } => {
            schedule_form::render_schedule_form(
                frame,
                area,
                *focused_field,
                name,
                message,
                *recurrence_index,
                interval,
                anchor_date,
                anchor_time,
                editing_id.is_some(),
            );
        }
        OverlayState::Terminal => {
            terminal::render_terminal_overlay(frame, area, app);
        }
        OverlayState::RatingOverlay {
            selected_rating, ..
        } => {
            rating::render_rating_overlay(frame, area, *selected_rating);
        }
        OverlayState::SessionInfoPanel { session_id } => {
            session_info::render_session_info_panel(frame, area, *session_id, app);
        }
        OverlayState::CreateEntityForm {
            kind,
            name,
            tags,
            focused_field,
            insert_mode,
            parent_id,
            error,
            ..
        } => {
            create_entity_form::render_create_entity_form(
                frame,
                area,
                *kind,
                name,
                tags,
                *focused_field,
                *insert_mode,
                *parent_id,
                error.as_deref(),
                app,
            );
        }
        OverlayState::ParentPicker {
            candidates,
            selected,
            query,
            ..
        } => {
            parent_picker::render_parent_picker(frame, area, app, candidates, *selected, query);
        }
    }
}

/// Render the `GraphReview` overlay. Extracted from `render_regular_overlay`
/// because the embedded picker needs `&mut ListState`, while the surrounding
/// match needs to call read-only methods on `&App`. We use `std::mem::take` to
/// detach the `ListState` from `app.overlay`, render with mutable access, then
/// write it back. `ListState` implements `Default`, so the swap is safe.
fn render_graph_review_overlay(frame: &mut Frame, area: Rect, app: &mut App) {
    // Read-only data needed alongside the &mut to picker_list_state.
    let saved_workflows = crate::overlay::graph::saved_workflow_picker_entries(app);
    // Snapshot the recursive-graph picker cache before the &mut overlay borrow.
    let recursive_graphs = app.recursive_graphs.clone();

    // Take a snapshot of the immutable fields we need to render with.
    let draft_id = match &app.overlay {
        OverlayState::GraphReview { draft_id, .. } => *draft_id,
        _ => return,
    };

    let draft_render_data = app
        .graph_draft(&draft_id)
        .map(|draft| (draft.workflow.clone(), draft.persistence_state));
    let execution_snapshot = app.graph_execution_for_draft(&draft_id).cloned();

    // Detach the ListState from the overlay so we can pass &mut to it while
    // the surrounding match in render_regular_overlay holds &app.overlay.
    let (
        mut detached_state,
        mode,
        viewport,
        selected_node,
        selected_edge,
        collapsed,
        edit_buffer,
        view_origin,
        gv_info_dashboard,
        dashboard_focused,
        dashboard_state,
    ) = match &mut app.overlay {
        OverlayState::GraphReview {
            picker_list_state,
            mode,
            viewport,
            selected_node,
            selected_edge,
            collapsed,
            edit_buffer,
            view_origin,
            gv_info_dashboard,
            dashboard_focused,
            dashboard_state,
            ..
        } => (
            std::mem::take(picker_list_state),
            *mode,
            *viewport,
            *selected_node,
            *selected_edge,
            collapsed.clone(),
            edit_buffer.clone(),
            view_origin.clone(),
            *gv_info_dashboard,
            *dashboard_focused,
            dashboard_state.clone(),
        ),
        _ => return,
    };

    if let Some((workflow, persistence_state)) = draft_render_data {
        graph::render_graph_review(
            frame,
            area,
            &workflow,
            persistence_state,
            mode,
            viewport,
            selected_node,
            &collapsed,
            &edit_buffer,
            selected_edge,
            execution_snapshot.as_ref(),
            &saved_workflows,
            &recursive_graphs,
            &view_origin,
            &mut detached_state,
            gv_info_dashboard,
            dashboard_focused,
            dashboard_state.as_ref(),
            app,
        );
    } else {
        graph::render_missing_graph_review(frame, area);
    }

    // Write the (possibly mutated) ListState back into the overlay.
    if let OverlayState::GraphReview {
        picker_list_state, ..
    } = &mut app.overlay
    {
        *picker_list_state = detached_state;
    }
}
