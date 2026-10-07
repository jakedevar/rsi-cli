//! Prompt popup rendering.
//!
//! The front of a launch prompt is its text editor under a one-line header
//! of launch facts (working directory, model, effort, sandbox and a planned
//! manager appointment). The footer shows only the editing mode: every key
//! lives in the prompt's contextual help (`?` in normal mode). A flipped
//! launch prompt renders its settings side in place of the editor.

use crate::app::App;
use crate::input_surface::InputSurface;
use crate::overlay::launch_settings::{self, LaunchSettingRow};
use crate::suggestions::CommandSuggestion;
use crate::types::{
    ModalGeometry, ModelDropdownState, PopupMode, PromptLaunchSettings, PromptPurpose,
};
use crate::ui::{glyphs, session, theme};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};
use rsi_common::types::SessionProvider;

use super::suggestion_dropdown;

/// Launch facts a prompt renders: header chips and, when flipped, its
/// settings side. Built from `App` by [`launch_view`].
#[derive(Debug, Default)]
pub(crate) struct PromptLaunchView {
    /// Resolved effort level, and whether the operator picked it explicitly
    /// (an implicit model default renders dim).
    pub effort: Option<(String, bool)>,
    /// Sandbox state for the header chip. `None` for continue prompts and
    /// when the daemon does not advertise sandbox support.
    pub sandbox: Option<bool>,
    /// A planned manager appointment.
    pub manager: Option<ManagerChip>,
    /// The settings side, when the prompt is flipped.
    pub settings: Option<SettingsSide>,
}

#[derive(Debug)]
pub(crate) struct ManagerChip {
    pub preset: &'static str,
    /// The appointment replaces the project's current manager.
    pub replaces: bool,
}

#[derive(Debug)]
pub(crate) struct SettingsSide {
    pub rows: Vec<SettingsRowView>,
    pub selected: usize,
    pub description: &'static str,
}

#[derive(Debug)]
pub(crate) struct SettingsRowView {
    pub label: &'static str,
    pub value: Vec<Span<'static>>,
    pub cycles: bool,
}

fn effective_model_and_provider<'a>(
    app: &'a App,
    model_override: Option<&'a str>,
    provider_override: Option<SessionProvider>,
) -> (Option<&'a str>, SessionProvider) {
    let provider = if model_override.is_some() {
        provider_override.unwrap_or(app.selected_provider)
    } else {
        app.selected_provider
    };
    (model_override.or(app.selected_model.as_deref()), provider)
}

/// Resolved effort label for the effective model: the operator's selection
/// when the model's ladder has it, else the model default.
fn effort_label(
    app: &App,
    model_override: Option<&str>,
    provider_override: Option<SessionProvider>,
) -> Option<(String, bool)> {
    let (model, provider) = effective_model_and_provider(app, model_override, provider_override);
    app.resolved_launch_effort(provider, model?)
}

/// Build what a prompt renders about its launch from the app state.
#[allow(clippy::too_many_arguments)]
pub(super) fn launch_view(
    app: &App,
    purpose: &PromptPurpose,
    launch: &PromptLaunchSettings,
    working_dir: &std::path::Path,
    model_override: Option<&str>,
    provider_override: Option<SessionProvider>,
    sandbox_enabled: bool,
    effort_bar_counts: (usize, usize),
) -> PromptLaunchView {
    if !launch_settings::has_settings_side(purpose) {
        return PromptLaunchView::default();
    }
    let sandbox_supported = app.poll.sandbox_supported;
    let effort = effort_label(app, model_override, provider_override);
    let manager = launch.manager.map(|plan| ManagerChip {
        preset: plan.preset.label(),
        replaces: launch_settings::replaced_manager_name(app, plan.project_id).is_some(),
    });
    let settings = launch.open.then(|| {
        let rows = launch_settings::setting_rows(purpose, launch);
        let selected = launch.selected.min(rows.len().saturating_sub(1));
        let description = rows.get(selected).map_or("", |row| row.description());
        SettingsSide {
            rows: rows
                .iter()
                .map(|row| SettingsRowView {
                    label: row.label(),
                    value: setting_value(
                        app,
                        *row,
                        launch,
                        working_dir,
                        model_override,
                        provider_override,
                        sandbox_enabled,
                        effort_bar_counts,
                        effort.as_ref(),
                    ),
                    cycles: row.cycles(),
                })
                .collect(),
            selected,
            description,
        }
    });
    PromptLaunchView {
        effort,
        sandbox: sandbox_supported.then_some(sandbox_enabled),
        manager,
        settings,
    }
}

fn dim(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(theme::subtext0()))
}

#[allow(clippy::too_many_arguments)]
fn setting_value(
    app: &App,
    row: LaunchSettingRow,
    launch: &PromptLaunchSettings,
    working_dir: &std::path::Path,
    model_override: Option<&str>,
    provider_override: Option<SessionProvider>,
    sandbox_enabled: bool,
    effort_bar_counts: (usize, usize),
    effort: Option<&(String, bool)>,
) -> Vec<Span<'static>> {
    match row {
        LaunchSettingRow::Model => {
            let (model, provider) =
                effective_model_and_provider(app, model_override, provider_override);
            vec![
                Span::styled(
                    format!("{} ", glyphs::provider_glyph(provider)),
                    Style::default()
                        .fg(glyphs::provider_color(provider))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    model.unwrap_or("default").to_string(),
                    Style::default().fg(if model_override.is_some() {
                        theme::green()
                    } else {
                        theme::model_text()
                    }),
                ),
            ]
        }
        LaunchSettingRow::Effort => {
            let (total, filled) = effort_bar_counts;
            if total == 0 {
                return vec![dim("not adjustable for this model")];
            }
            let mut spans = vec![
                Span::styled(
                    glyphs::EFFORT.repeat(filled),
                    Style::default()
                        .fg(theme::yellow())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "▯".repeat(total.saturating_sub(filled)),
                    Style::default().fg(theme::surface1()),
                ),
            ];
            if let Some((level, explicit)) = effort {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    if *explicit {
                        level.clone()
                    } else {
                        format!("{level} (default)")
                    },
                    Style::default().fg(theme::effort_text()),
                ));
            }
            spans
        }
        LaunchSettingRow::Sandbox => {
            if !app.poll.sandbox_supported {
                return vec![dim("unavailable: the daemon has no sandbox support")];
            }
            if sandbox_enabled {
                vec![
                    Span::styled(
                        format!("{} on", glyphs::SANDBOX),
                        Style::default()
                            .fg(theme::green())
                            .add_modifier(Modifier::BOLD),
                    ),
                    dim("  isolated git worktree"),
                ]
            } else {
                vec![
                    Span::styled(
                        format!("{} off", glyphs::SANDBOX),
                        Style::default()
                            .fg(theme::peach())
                            .add_modifier(Modifier::BOLD),
                    ),
                    dim(format!(
                        "  works directly in {}",
                        glyphs::home_relative(&working_dir.to_string_lossy())
                    )),
                ]
            }
        }
        LaunchSettingRow::Manager => match launch.manager {
            None if app.current_project_id.is_none() => vec![
                dim(format!("{} off", glyphs::MANAGERS)),
                dim("  needs a project (:projects)"),
            ],
            None => vec![dim(format!("{} off", glyphs::MANAGERS))],
            Some(plan) => {
                let mut spans = vec![Span::styled(
                    format!("{} appoint on launch", glyphs::MANAGERS),
                    Style::default()
                        .fg(theme::yellow())
                        .add_modifier(Modifier::BOLD),
                )];
                if let Some(name) = launch_settings::replaced_manager_name(app, plan.project_id) {
                    spans.push(Span::styled(
                        format!("  replaces {name}"),
                        Style::default().fg(theme::peach()),
                    ));
                }
                spans
            }
        },
        LaunchSettingRow::ManagerScope => launch.manager.map_or_else(Vec::new, |plan| {
            vec![Span::styled(
                launch_settings::manager_scope_label(app, plan.scope),
                Style::default().fg(theme::text()),
            )]
        }),
        LaunchSettingRow::ManagerPolicy => launch.manager.map_or_else(Vec::new, |plan| {
            vec![Span::styled(
                plan.preset.label().to_string(),
                Style::default().fg(theme::text()),
            )]
        }),
    }
}

/// Body rows the prompt needs: its minimum textarea height on the front, or
/// every settings row plus the description on the settings side.
pub(super) fn body_min_rows(purpose: &PromptPurpose, launch: &PromptLaunchSettings) -> u16 {
    if launch.open && launch_settings::has_settings_side(purpose) {
        let rows = launch_settings::setting_rows(purpose, launch).len();
        u16::try_from(rows + 2).unwrap_or(u16::MAX)
    } else {
        3
    }
}

/// Right-aligned launch chips for the header: sandbox and manager.
fn header_chips(view: &PromptLaunchView) -> Vec<Span<'static>> {
    let mut chips = Vec::new();
    if let Some(on) = view.sandbox {
        chips.push(if on {
            Span::styled(
                format!("{} sandbox", glyphs::SANDBOX),
                Style::default().fg(theme::green()),
            )
        } else {
            Span::styled(
                format!("{} no sandbox", glyphs::SANDBOX),
                Style::default().fg(theme::peach()),
            )
        });
    }
    if let Some(manager) = &view.manager {
        if !chips.is_empty() {
            chips.push(Span::raw("  "));
        }
        chips.push(Span::styled(
            format!("{} manager · {}", glyphs::MANAGERS, manager.preset),
            Style::default()
                .fg(if manager.replaces {
                    theme::peach()
                } else {
                    theme::yellow()
                })
                .add_modifier(Modifier::BOLD),
        ));
    }
    chips
}

fn spans_width(spans: &[Span<'_>]) -> u16 {
    u16::try_from(spans.iter().map(Span::width).sum::<usize>()).unwrap_or(u16::MAX)
}

/// Keep the last `max` characters of `text`, marking a cut with a leading `…`.
fn elide_start(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let keep = max.saturating_sub(1);
    let tail: String = text.chars().skip(count - keep).collect();
    format!("…{tail}")
}

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
    effort_bar_counts: (usize, usize),
    popup_rect_override: Option<Rect>,
    focused: bool,
    session_list_width: Option<u16>,
    geom: &ModalGeometry,
    model_override: Option<&str>,
    model_dropdown: Option<&ModelDropdownState>,
    view: &PromptLaunchView,
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
        let min_rows = view
            .settings
            .as_ref()
            .map_or(3, |side| u16::try_from(side.rows.len() + 2).unwrap_or(3));
        super::compute_dynamic_popup_rect_with_manual_height(
            area,
            &surface.textarea,
            6,
            min_rows,
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

    // Layout inside the popup: header (1), blank (1), body (fill), blank (1), mode row (1)
    if inner.height < 4 {
        return; // Too small to render anything meaningful
    }

    // Row 0: working directory, model and effort on the left; launch chips
    // (sandbox, manager) right-aligned.
    let cwd_display = working_dir.to_string_lossy().replacen(
        &dirs::home_dir().map_or(String::new(), |h| h.to_string_lossy().to_string()),
        "~",
        1,
    );

    // Model indicator next to the working directory for Blank
    // popups. Per-session override takes precedence over global selection.
    let effective_model = model_override.or(selected_model);
    let mut model_spans = Vec::new();
    if matches!(purpose, PromptPurpose::Blank) {
        let model_display = effective_model.unwrap_or("default");
        model_spans.push(Span::styled("  ", Style::default()));
        model_spans.push(Span::styled(
            model_display.to_string(),
            Style::default()
                .fg(if model_override.is_some() {
                    theme::green() // visual indicator that override is active
                } else {
                    theme::mauve()
                })
                .add_modifier(Modifier::BOLD),
        ));
        // Add effort bars and level for reasoning-capable models.
        model_spans.extend(crate::ui::session::build_effort_bars_from_counts(
            effort_bar_counts,
        ));
        if effort_bar_counts.0 > 0
            && let Some((level, explicit)) = &view.effort
        {
            model_spans.push(Span::styled(
                format!(" {level}"),
                Style::default().fg(if *explicit {
                    theme::effort_text()
                } else {
                    theme::subtext0()
                }),
            ));
        }
    }

    let chips = header_chips(view);
    let chips_width = spans_width(&chips).min(inner.width);
    let left_width = if chips.is_empty() {
        inner.width
    } else {
        inner.width.saturating_sub(chips_width.saturating_add(2))
    };
    // A long working directory gives up its leading characters first so the
    // model and effort stay readable.
    let cwd_room = usize::from(left_width)
        .saturating_sub("cwd: ".len())
        .saturating_sub(usize::from(spans_width(&model_spans)))
        .max(8);
    let mut cwd_spans = vec![
        Span::styled("cwd: ", Style::default().fg(theme::overlay_hint())),
        Span::styled(
            elide_start(&cwd_display, cwd_room),
            Style::default().fg(theme::overlay_cwd()),
        ),
    ];
    cwd_spans.extend(model_spans);
    frame.render_widget(
        Paragraph::new(Line::from(Vec::<Span>::new()))
            .style(Style::default().bg(theme::overlay_bg())),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    frame.render_widget(
        Paragraph::new(Line::from(cwd_spans)).style(Style::default().bg(theme::overlay_bg())),
        Rect::new(inner.x, inner.y, left_width, 1),
    );
    if !chips.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(chips))
                .alignment(Alignment::Right)
                .style(Style::default().bg(theme::overlay_bg())),
            Rect::new(inner.x + inner.width - chips_width, inner.y, chips_width, 1),
        );
    }

    // Rows 2..(height-2): textarea, or the settings side when flipped
    let textarea_y = inner.y + 2;
    let textarea_height = inner.height.saturating_sub(4);
    if textarea_height == 0 {
        return;
    }
    let textarea_area = Rect::new(inner.x, textarea_y, inner.width, textarea_height);
    let hint_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);

    if let Some(side) = &view.settings {
        render_settings_side(frame, textarea_area, side);
        render_mode_row(
            frame,
            hint_area,
            " SETTINGS ",
            theme::overlay_mode_normal_fg(),
            theme::accent(),
            true,
        );
        render_model_dropdown(frame, area, inner, model_dropdown, effective_model);
        return;
    }

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

    // Last row: mode indicator. A compiled preview still names its decision
    // keys because they replace normal editing until accepted or discarded.
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
        let hint_line = Line::from(vec![Span::styled(
            " COMPILING... ",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        )]);
        frame.render_widget(
            Paragraph::new(hint_line).style(Style::default().bg(theme::overlay_bg())),
            hint_area,
        );
    } else {
        let (mode_label, mode_fg, mode_bg) = if let Some(visual) = surface.vim_state.visual {
            let label = match visual {
                crate::vim_textarea::VisualMode::Char => " VISUAL ",
                crate::vim_textarea::VisualMode::Line => " V-LINE ",
            };
            (
                label,
                theme::overlay_mode_normal_fg(),
                theme::overlay_mode_normal_bg(),
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
            )
        } else {
            match surface.mode {
                PopupMode::Insert => (
                    " INSERT ",
                    theme::overlay_mode_insert_fg(),
                    theme::overlay_mode_insert_bg(),
                ),
                PopupMode::Normal => (
                    " NORMAL ",
                    theme::overlay_mode_normal_fg(),
                    theme::overlay_mode_normal_bg(),
                ),
            }
        };
        // `?` opens help only when a plain key starts a command.
        let help_reachable = surface.mode == PopupMode::Normal && surface.vim_state.is_idle();
        render_mode_row(
            frame,
            hint_area,
            mode_label,
            mode_fg,
            mode_bg,
            help_reachable,
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

    render_model_dropdown(frame, area, inner, model_dropdown, effective_model);
}

/// Bottom row: the mode badge, plus a dim `? help` pointer when `?` opens
/// help from the current state.
fn render_mode_row(
    frame: &mut Frame,
    area: Rect,
    label: &'static str,
    fg: ratatui::style::Color,
    bg: ratatui::style::Color,
    help_reachable: bool,
) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            label,
            Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
        )))
        .style(Style::default().bg(theme::overlay_bg())),
        area,
    );
    if help_reachable {
        let hint = "? help";
        let width = u16::try_from(hint.len()).unwrap_or(0);
        let label_width = u16::try_from(label.len()).unwrap_or(0);
        if area.width > width + label_width + 1 {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    hint,
                    Style::default().fg(theme::overlay_hint()),
                ))
                .alignment(Alignment::Right)
                .style(Style::default().bg(theme::overlay_bg())),
                Rect::new(area.x + area.width - width, area.y, width, 1),
            );
        }
    }
}

/// The settings side: one row per setting (selected row marked, cycling
/// values framed by `‹ ›`), then the selected row's description, wrapped.
fn render_settings_side(frame: &mut Frame, area: Rect, side: &SettingsSide) {
    const LABEL_WIDTH: usize = 9;
    let lines: Vec<Line<'static>> = side
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let selected = index == side.selected;
            let marker = if selected { "▸ " } else { "  " };
            let label_style = if selected {
                Style::default()
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::subtext1())
            };
            let mut spans = vec![
                Span::styled(marker, Style::default().fg(theme::accent())),
                Span::styled(format!("{:<LABEL_WIDTH$}", row.label), label_style),
            ];
            let framed = selected && row.cycles && !row.value.is_empty();
            if framed {
                spans.push(Span::styled("‹ ", Style::default().fg(theme::accent())));
            }
            spans.extend(row.value.iter().cloned());
            if framed {
                spans.push(Span::styled(" ›", Style::default().fg(theme::accent())));
            }
            Line::from(spans)
        })
        .collect();
    let rows_height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .min(area.height);
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme::overlay_bg())),
        Rect::new(area.x, area.y, area.width, rows_height),
    );
    let description_y = area.y + rows_height + 1;
    if !side.description.is_empty() && description_y < area.y + area.height {
        frame.render_widget(
            Paragraph::new(Span::styled(
                side.description,
                Style::default()
                    .fg(theme::subtext0())
                    .add_modifier(Modifier::ITALIC),
            ))
            .wrap(Wrap { trim: true })
            .style(Style::default().bg(theme::overlay_bg())),
            Rect::new(
                area.x,
                description_y,
                area.width,
                area.y + area.height - description_y,
            ),
        );
    }
}

/// Model dropdown rendered after everything else (painter's algorithm),
/// anchored below the header where the model is displayed.
fn render_model_dropdown(
    frame: &mut Frame,
    area: Rect,
    inner: Rect,
    model_dropdown: Option<&ModelDropdownState>,
    effective_model: Option<&str>,
) {
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
