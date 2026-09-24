//! Rendering functions.

pub mod content;
pub mod file_renderer;
pub mod glyphs;
pub mod height;
pub mod highlight;
pub mod issues;
pub mod navigator_layout;
pub mod overlay;
pub mod prompt_creator;
pub mod session;
pub mod settings;
pub mod status;
pub mod theme;
pub mod theme_roles;
pub mod tool_projection;
pub mod treesitter;
pub mod widget;

use crate::app::App;
use crate::profiling;
use crate::types::{OverlayState, Pane, SplitDirection, SplitNode};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

/// Parse a hex color string (#RRGGBB) into a ratatui Color.
pub(crate) fn parse_hex_color(hex: &str) -> Option<Color> {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

const LEGACY_LAYOUT_X_OFFSET: u16 = 33;
const MIN_LAYOUT_X_OFFSET: u16 = 5;
const MAX_LAYOUT_X_OFFSET: u16 = 55;

/// Legacy sidebar-width seed used by existing grow/shrink keybindings when
/// migrating an offset-tuned tab into percentage mode. Rendering no longer
/// uses this for centering.
pub(crate) fn effective_layout_x_offset(tab: &crate::types::Tab) -> u16 {
    let raw = LEGACY_LAYOUT_X_OFFSET as i16 + tab.layout_x_offset_adj;
    raw.clamp(MIN_LAYOUT_X_OFFSET as i16, MAX_LAYOUT_X_OFFSET as i16) as u16
}

/// Width source for layout actions outside a live draw call.
///
/// The event loop updates `last_terminal_size` from the backend before drawing
/// and from resize events when they arrive. Falling back to crossterm is only
/// for pre-first-render state.
pub(crate) fn terminal_width_for_layout(app: &App) -> u16 {
    let width = app.last_terminal_size.0;
    if width > 0 {
        width
    } else {
        crossterm::terminal::size().map(|(w, _)| w).unwrap_or(120)
    }
}

/// Top-level render function.
pub fn render(frame: &mut Frame, app: &mut App) {
    // Zero-size guard: i3 can momentarily report 0×0 during workspace transitions.
    // Rendering into a zero-area frame causes u16 underflows and wasted CPU.
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }

    let overlay_was_visible = app.overlay_visible_last_frame;
    let overlay_is_visible =
        !matches!(app.overlay, OverlayState::None) || !app.input_overlays.is_empty();
    if overlay_was_visible && !overlay_is_visible {
        frame.render_widget(Clear, frame.area());
    }
    // NOTE: pane_switch_clear is handled in event.rs via terminal.clear()
    // before draw(), which resets ratatui's previous buffer for a full repaint.

    app.has_loading_bar = false;
    app.has_formulation_animation = false;
    let render_timer = profiling::start_timer();
    if profiling::enabled() {
        profiling::reset_cache_counters();
    }

    let mut layout_node = app.tabs[app.active_tab].layout.clone();
    let has_detail = split_tree_contains_detail(&layout_node);

    // HUD rails have been moved into the bottom container — always show status line.
    app.hud_rails_visible = false;

    frame.render_widget(
        ratatui::widgets::Block::default().style(Style::default().bg(theme::root_bg())),
        frame.area(),
    );

    let has_transient_footer = !has_detail && should_render_root_footer(app);
    let constraints = if !has_transient_footer {
        vec![Constraint::Length(1), Constraint::Fill(1)]
    } else {
        vec![
            Constraint::Length(1), // Top chrome
            Constraint::Fill(1),   // Main area
            Constraint::Length(1), // Full-list command/footer bar
        ]
    };

    let areas = Layout::vertical(constraints).split(frame.area());
    let status_area = areas[0];
    let main_area = areas[1];
    let cmd_area = has_transient_footer.then(|| areas[2]);

    let focused_pane = app.tabs[app.active_tab].focused_pane;
    render_split_node(frame, main_area, &mut layout_node, focused_pane, app);

    app.model_segment_rect = render_top_chrome(frame, status_area, app);

    if let Some(cmd_area) = cmd_area {
        render_command_bar(frame, cmd_area, app);
    }

    // Overlay rendered last — drawn on top of everything (painter's algorithm)
    overlay::render_overlay(frame, frame.area(), app);
    app.overlay_visible_last_frame = overlay_is_visible;

    // Model dropdown widget (rendered after overlays for painter's algorithm)
    if app.model_dropdown.open {
        if let Some(anchor) = app.model_segment_rect {
            widget::model_dropdown::render_model_dropdown(
                frame,
                frame.area(),
                anchor,
                &app.model_dropdown,
                app.selected_model.as_deref(),
                app.is_provider_available(app.model_dropdown.provider),
            );
        }
    }

    if let Some(start) = render_timer {
        let counters = profiling::drain_cache_counters();
        profiling::log_duration("render_frame", start, |elapsed| {
            app.metrics.record_render(elapsed, counters);
        });
    }
}

fn split_tree_contains_detail(node: &SplitNode) -> bool {
    match node {
        SplitNode::Leaf { pane, .. } => matches!(pane, Pane::SessionDetail { .. }),
        SplitNode::Split { first, second, .. } => {
            split_tree_contains_detail(first) || split_tree_contains_detail(second)
        }
    }
}

fn render_top_chrome(frame: &mut Frame, area: Rect, app: &App) -> Option<Rect> {
    if area.width == 0 || area.height == 0 {
        return None;
    }

    let bg = theme::status_line_bg();
    frame.render_widget(
        ratatui::widgets::Block::default().style(Style::default().bg(bg)),
        area,
    );
    // Keep chrome controls clear of terminal edges while retaining a usable
    // one-cell layout at narrow widths.
    let content_area = if area.width > 2 {
        Rect::new(area.x + 1, area.y, area.width - 2, area.height)
    } else {
        area
    };

    let active = app
        .sessions
        .values()
        .filter(|s| {
            matches!(
                s.session.status,
                rsi_common::types::SessionStatus::Running
                    | rsi_common::types::SessionStatus::Starting
            )
        })
        .count();
    let left_spans = vec![top_kv("\u{25CF}", &active.to_string(), theme::green(), bg)];
    let right_spans = project_tab_spans(app, bg);
    let left_width = spans_width(&left_spans).min(content_area.width);
    let right_width = spans_width(&right_spans).min(
        content_area
            .width
            .saturating_sub(left_width.saturating_add(1)),
    );

    frame.render_widget(
        Paragraph::new(Line::from(left_spans))
            .style(Style::default().bg(bg))
            .alignment(Alignment::Left),
        Rect::new(
            content_area.x,
            content_area.y,
            left_width,
            content_area.height,
        ),
    );

    if let Some(notification) = active_transient_notification(app)
        && let Some(notification_area) = centered_top_lane(content_area, left_width, right_width)
    {
        use crate::types::NotificationPriority;
        let fg = match notification.priority {
            NotificationPriority::High => theme::error_status(),
            _ => theme::toast_text(),
        };
        frame.render_widget(
            Paragraph::new(notification.message.as_str())
                .style(Style::default().fg(fg).bg(bg))
                .alignment(Alignment::Center),
            notification_area,
        );
    }

    if right_width > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(right_spans))
                .style(Style::default().bg(bg))
                .alignment(Alignment::Right),
            Rect::new(
                content_area.x + content_area.width.saturating_sub(right_width),
                content_area.y,
                right_width,
                content_area.height,
            ),
        );
    }

    model_dropdown_anchor(content_area)
}

fn spans_width(spans: &[Span<'_>]) -> u16 {
    spans.iter().fold(0, |width, span| {
        width.saturating_add(span.width().min(u16::MAX as usize) as u16)
    })
}

/// Build the historical color-coded project tab circles for the top-right.
/// The active tab uses a ring; inactive tabs use solid circles.
fn project_tab_spans(app: &App, bg: Color) -> Vec<Span<'static>> {
    if app.tabs.len() <= 1 {
        return Vec::new();
    }

    let mut spans = Vec::with_capacity(app.tabs.len().saturating_mul(2).saturating_sub(1));
    for (index, tab) in app.tabs.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" ", Style::default().bg(bg)));
        }
        let project_color = tab
            .project_id
            .and_then(|project_id| app.projects.iter().find(|project| project.id == project_id))
            .map(|project| parse_hex_color(&project.color).unwrap_or_else(theme::blue))
            .unwrap_or_else(theme::subtext0);
        let symbol = if index == app.active_tab {
            "\u{25C9}"
        } else {
            "\u{25CF}"
        };
        spans.push(Span::styled(
            symbol,
            Style::default().fg(project_color).bg(bg),
        ));
    }
    spans
}

/// Return the widest odd-width lane centered on the whole top bar that keeps
/// a one-column gap from the left counters and right project tabs.
fn centered_top_lane(area: Rect, left_width: u16, right_width: u16) -> Option<Rect> {
    if area.width == 0 || area.height == 0 {
        return None;
    }

    let safe_left = left_width.saturating_add(1).min(area.width);
    let safe_right = area
        .width
        .saturating_sub(right_width.min(area.width))
        .saturating_sub(1);
    let center = area.width.saturating_sub(1) / 2;
    if center < safe_left || center >= safe_right {
        return None;
    }

    let half = (center - safe_left).min(safe_right.saturating_sub(center + 1));
    Some(Rect::new(
        area.x + center - half,
        area.y,
        half.saturating_mul(2).saturating_add(1),
        area.height,
    ))
}

fn top_kv(label: &str, value: &str, value_color: Color, bg: Color) -> Span<'static> {
    Span::styled(
        format!("{label} {value} "),
        Style::default().fg(value_color).bg(bg),
    )
}

/// Fixed anchor for the global model dropdown (`M`). Post-header-cut there
/// is no rendered chip left to measure — `render_model_dropdown`
/// (`ui/widget/model_dropdown.rs`) only ever reads `anchor.x` / `anchor.y` /
/// `anchor.height` (confirmed: `anchor.width` has zero reads in that file),
/// so a stable 1-wide rect is sufficient. Anchored at the header's right
/// edge. The anchor is geometry-only; the project dots may occupy the header
/// cell while the dropdown itself opens below it.
fn model_dropdown_anchor(area: Rect) -> Option<Rect> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let x = area.x + area.width.saturating_sub(1);
    Some(Rect::new(x, area.y, 1, 1))
}

/// Recursively render a split tree node.
///
/// Takes `&mut App` so that `render_pane` can update the height cache
/// (mutable) before rendering each session detail (immutable reborrow).
fn render_split_node(
    frame: &mut Frame,
    area: Rect,
    node: &mut SplitNode,
    focused_pane: crate::types::PaneId,
    app: &mut App,
) {
    match node {
        SplitNode::Leaf { pane, id } => {
            let is_focused = *id == focused_pane;
            render_pane(frame, area, *id, pane, is_focused, app);
        }
        SplitNode::Split {
            direction,
            first,
            second,
            ..
        } => {
            let chunks = match direction {
                SplitDirection::Horizontal => {
                    Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(area)
                }
                SplitDirection::Vertical => {
                    Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(area)
                }
            };
            render_split_node(frame, chunks[0], first, focused_pane, app);
            render_split_node(frame, chunks[1], second, focused_pane, app);
        }
    }
}

/// Render a single pane (session list or detail).
///
/// For session detail panes, updates the height cache with the exact pane
/// content width before rendering. The mutable borrow completes before
/// the immutable render call.
fn render_pane(
    frame: &mut Frame,
    area: Rect,
    pane_id: crate::types::PaneId,
    pane: &mut Pane,
    focused: bool,
    app: &mut App,
) {
    match pane {
        Pane::SessionList {
            selected_session, ..
        } => {
            // Check if selected session has a file viewer active
            let file_viewer_active = selected_session
                .and_then(|id| app.sessions.get(&id))
                .map(|s| s.file_viewer.is_some())
                .unwrap_or(false);

            if file_viewer_active {
                let session_id = selected_session.unwrap();
                frame.render_widget(Clear, area);
                let viewer_area = file_viewer_area_with_explorer_drawer(area, frame.area(), app);
                session::render_file_viewer(frame, viewer_area, session_id, focused, app);
            } else {
                frame.render_widget(Clear, area);
                session::render_session_list(
                    frame,
                    area,
                    pane,
                    focused,
                    &mut *app,
                    session::SessionListSurface::Full,
                    None,
                );
            }
        }
        Pane::Settings => {
            settings::render_settings(app, frame, area);
        }
        Pane::PromptCreator => {
            prompt_creator::render_prompt_creator(frame, area, focused, app);
        }
        Pane::Issues(state) => {
            frame.render_widget(Clear, area);
            issues::render_issue_workspace(frame, area, focused, state, app);
            let rendered = (
                state.focus,
                state.local.scroll_offset,
                state.local.inspector_scroll_offset,
                state.dispatched.scroll_offset,
                state.dispatched.inspector_scroll_offset,
                state.sync.scroll_offset,
                state.transient.local_viewport_rows,
                state.transient.inspector_viewport_rows,
                state.transient.dispatched_viewport_rows,
                state.transient.sync_viewport_rows,
            );
            if let Some(Pane::Issues(actual)) =
                app.tabs[app.active_tab].layout.find_pane_mut(pane_id)
            {
                actual.focus = rendered.0;
                actual.local.scroll_offset = rendered.1;
                actual.local.inspector_scroll_offset = rendered.2;
                actual.dispatched.scroll_offset = rendered.3;
                actual.dispatched.inspector_scroll_offset = rendered.4;
                actual.sync.scroll_offset = rendered.5;
                actual.transient.local_viewport_rows = rendered.6;
                actual.transient.inspector_viewport_rows = rendered.7;
                actual.transient.dispatched_viewport_rows = rendered.8;
                actual.transient.sync_viewport_rows = rendered.9;
            }
        }
        Pane::SessionDetail { session_id } => {
            // Clear the full pane area so no stale characters remain from a
            // previous pane type (e.g. SessionList → SessionDetail transition).
            frame.render_widget(Clear, area);
            let (sidebar_area, full_area) = compute_detail_layout_areas(area, app);
            if sidebar_area.width > 0 {
                let sep_x = sidebar_area.x + sidebar_area.width;
                let sep_area = Rect::new(sep_x, area.y, 1, area.height);
                frame.render_widget(
                    Paragraph::new("│".repeat(sep_area.height as usize))
                        .style(Style::default().fg(theme::surface2()).bg(theme::root_bg())),
                    sep_area,
                );
            }
            let session_area = full_area;
            let mut clear_session_area = false;

            // T5: cap the transcript/input-bar column on wide terminals. session_area
            // itself is kept UNNARROWED — it is still used below for the full-width
            // Clear on session switch, and gutter painting needs both Rects.
            let requested_detail_width = app
                .sessions
                .get_mut(session_id)
                .map(cached_table_requested_detail_width)
                .unwrap_or(DETAIL_MAX_CONTENT_WIDTH);
            let centered_area = compute_centered_detail_area(session_area, requested_detail_width);

            // Compute dynamic input bar height against the FINAL (centered) width, so
            // the height pre-computation and the actual render agree exactly.
            let input_bar_height = app
                .sessions
                .get(session_id)
                .map(|state| {
                    session::compute_input_bar_height(
                        &state.input_bar.surface.textarea,
                        centered_area.width,
                    )
                })
                .unwrap_or(3);

            let chunks = Layout::vertical([
                Constraint::Fill(1),                  // Session content (scrollable)
                Constraint::Length(input_bar_height), // Input bar (dynamic)
            ])
            .split(centered_area);

            // Both areas are split from the SAME centered_area, so they are
            // pixel-identical in x/width by construction — "input bar width == message
            // width" holds without a separate equality check anywhere.
            let content_area = chunks[0];
            let input_bar_area = chunks[1];

            // Determine if loading bar will be shown (needed for viewport calc)
            let loading_container_height =
                session::activity_indicator_height(app.settings.activity_indicator_style);
            let is_active = app
                .sessions
                .get(session_id)
                .map(|s| {
                    matches!(
                        s.session.status,
                        rsi_common::types::SessionStatus::Running
                            | rsi_common::types::SessionStatus::Starting
                    )
                })
                .unwrap_or(false);
            let formulation_active = app
                .sessions
                .get(session_id)
                .and_then(|s| s.formulation)
                .map(|f| {
                    chrono::Utc::now().timestamp_millis() - f.started_at_ms
                        < session::FORMULATION_ANIMATION_MS
                })
                .unwrap_or(false);
            let show_loading_bar = is_active && content_area.height > 5 && !formulation_active;
            // Keep redraws alive while formulation animation is running.
            if formulation_active {
                app.has_formulation_animation = true;
            }

            // Pre-render: update height cache with the actual inner rendering width.
            // Must account for the explicit transcript inset in render_session_detail.
            // See theme::SESSION_DETAIL_HORIZ_INSET for the canonical value; heights
            // are calculated against the same inner width used for rendering.
            let content_width = content_area
                .width
                .saturating_sub(theme::SESSION_DETAIL_HORIZ_INSET);
            let has_recursive_dag = app
                .sessions
                .get(session_id)
                .map(|state| {
                    crate::ui::session::recursive_dag_detail_line(
                        &*app,
                        &state.session,
                        content_width,
                    )
                    .is_some()
                })
                .unwrap_or(false);
            if let Some(state) = app.sessions.get_mut(session_id) {
                if state.clear_next_render {
                    clear_session_area = true;
                    state.clear_next_render = false;
                }
                state.last_content_area = content_area;
                height::update_event_heights(state, content_width);

                // Resolve formulation target_height sentinel and clear expired animations.
                if let Some(ref mut form) = state.formulation {
                    if form.target_height == 0 {
                        if let Some(&h) = state.event_heights.get(form.event_index) {
                            form.target_height = h as u16;
                        }
                    }
                    let elapsed = chrono::Utc::now().timestamp_millis() - form.started_at_ms;
                    if elapsed >= session::FORMULATION_ANIMATION_MS {
                        state.formulation = None;
                    }
                }

                // Compute vertical offset from content_area.y to first message line.
                // Must mirror the header rows rendered in render_session_detail:
                // title + metadata + divider (3) + active_task(0-1) + pending_archive(0-1)
                // + issue_url(0-1) + sandbox(0-1) + recursive DAG(0-1).
                let at_top = state.scroll_offset == 0;
                let is_running = matches!(
                    state.session.status,
                    rsi_common::types::SessionStatus::Running
                        | rsi_common::types::SessionStatus::Starting
                );
                let has_active_task = state.session.active_task.is_some();
                let has_pending_archive = state.session.pending_archive && is_running;
                let has_issue_url = state.session.issue_url.is_some();
                let has_sandbox_info = state.session.sandbox_root.is_some();
                let terminal_reason_visible = matches!(
                    state.session.status,
                    rsi_common::types::SessionStatus::Failed
                        | rsi_common::types::SessionStatus::Interrupted
                ) || (state.session.status
                    == rsi_common::types::SessionStatus::Completed
                    && state.session.terminal_reason.is_some());
                let has_terminal_info = at_top
                    && (terminal_reason_visible || crate::ui::session::has_terminal_error(state));
                state.last_content_y_offset = crate::ui::session::session_detail_content_y_offset(
                    at_top,
                    has_active_task,
                    has_pending_archive,
                    has_issue_url,
                    has_sandbox_info,
                    has_terminal_info,
                    has_recursive_dag,
                );

                let total_height = state.total_content_height;
                // viewport = content_area height minus the fixed detail header rows.
                // Shrink effective viewport when loading bar is active so text stays above it.
                let viewport = content_area
                    .height
                    .saturating_sub(session::SESSION_DETAIL_HEADER_HEIGHT)
                    as usize;
                let viewport = if show_loading_bar {
                    viewport.saturating_sub(loading_container_height as usize)
                } else {
                    viewport
                };
                state.last_viewport_height = viewport;
                let max_scroll = total_height.saturating_sub(viewport);

                if state.follow_tail {
                    state.scroll_offset = max_scroll;
                } else {
                    // Clamp: never scroll past the last line of content
                    state.scroll_offset = state.scroll_offset.min(max_scroll);

                    // Re-enable follow_tail when user scrolls back to the
                    // bottom (scroll-lock re-engage). This lets the view
                    // auto-follow new content again once the user reaches
                    // the end of the conversation.
                    //
                    // The hold-off flag prevents re-engage after explicit jumps
                    // (Shift+Up/Down) whose target scroll offset may land near
                    // max_scroll. The hold persists while scroll_offset stays
                    // at/near max_scroll (height recalculations can shift
                    // max_scroll between frames at ~120fps, so a single-frame
                    // hold is insufficient). Clears naturally once the user
                    // scrolls away from the bottom.
                    if state.follow_tail_hold {
                        // Only clear hold once scroll_offset drops below max_scroll
                        if state.scroll_offset < max_scroll {
                            state.follow_tail_hold = false;
                        }
                    } else if state.scroll_offset >= max_scroll && max_scroll > 0 {
                        state.follow_tail = true;
                    }
                }

                // When follow_tail is active, always select the newest event
                // so the selection tracks new messages as they arrive.
                // When NOT following tail (free-scroll), preserve the
                // user's current selection and don't reassign it.
                let prev_cursor = state.current_event_index;
                if state.follow_tail && !state.events.is_empty() {
                    state.current_event_index = Some(state.events.len() - 1);
                } else if state.current_event_index.is_none() && !state.event_offsets.is_empty() {
                    // Derive current_event_index from scroll_offset only as
                    // a fallback when no cursor is set yet.
                    state.current_event_index = state
                        .event_offsets
                        .iter()
                        .rposition(|&off| off <= state.scroll_offset)
                        .filter(|&idx| idx < state.events.len());
                }

                // Clamp cursor if events were removed (e.g., session cleared)
                if let Some(idx) = state.current_event_index {
                    if idx >= state.events.len() {
                        state.current_event_index = if state.events.is_empty() {
                            None
                        } else {
                            Some(state.events.len() - 1)
                        };
                    }
                }

                // If cursor changed, re-run height computation so the marker's width
                // is accounted for on the correct event, then re-clamp scroll.
                if state.current_event_index != prev_cursor {
                    height::invalidate_heights(state);
                    height::update_event_heights(state, content_width);
                    let max_scroll = state.total_content_height.saturating_sub(viewport);
                    if state.follow_tail {
                        state.scroll_offset = max_scroll;
                    } else {
                        state.scroll_offset = state.scroll_offset.min(max_scroll);
                    }
                }
            }

            if clear_session_area {
                frame.render_widget(Clear, session_area);
            }

            paint_detail_gutters(frame, session_area, centered_area, &app.settings);

            // Check if file viewer is active for this session
            let file_viewer_active = app
                .sessions
                .get(session_id)
                .map(|s| s.file_viewer.is_some())
                .unwrap_or(false);

            if file_viewer_active {
                // A file viewer replaces its host surface. Match the session-list
                // path by using the complete pane rather than retaining the
                // session-detail sidebar or its centered transcript geometry.
                let viewer_area = file_viewer_area_with_explorer_drawer(area, frame.area(), app);
                session::render_file_viewer(frame, viewer_area, *session_id, focused, app);
            } else {
                // Render session content
                session::render_session_detail(frame, content_area, *session_id, focused, app);

                // Render loading bar overlay on bottom row of content area (inside borders)
                // when session is active. Painted last so it sits on top of content.
                if show_loading_bar {
                    // Inset to align with tool-call group geometry:
                    // SESSION_DETAIL_HORIZ_INSET = 4 (1 border + 1 padding per side)
                    let container_area = Rect::new(
                        content_area.x + theme::SESSION_DETAIL_HORIZ_INSET / 2,
                        content_area.y + content_area.height - 1 - loading_container_height,
                        content_area
                            .width
                            .saturating_sub(theme::SESSION_DETAIL_HORIZ_INSET),
                        loading_container_height,
                    );
                    if let Some(state) = app.sessions.get(session_id) {
                        session::render_activity_indicator(
                            frame,
                            container_area,
                            state,
                            app.settings.activity_indicator_style,
                        );
                    }
                    app.has_loading_bar = true;
                }

                // Render input bar (hidden when InputModal is active)
                session::render_input_bar(frame, input_bar_area, *session_id, focused, app);

                // Resolve the current tab's SessionList pane. Check the layout tree
                // first (split layouts), then fall back to the tab-stored session
                // list state (single-pane detail view).
                let list_pane = {
                    let tab = &app.tabs[app.active_tab];
                    let mut found: Option<Pane> = None;
                    for pid in tab.layout.leaf_ids() {
                        if let Some(p) = tab.layout.find_pane(pid) {
                            if matches!(p, Pane::SessionList { .. }) {
                                found = Some(p.clone());
                                break;
                            }
                        }
                    }
                    found.unwrap_or_else(|| tab.session_list_state.clone())
                };
                // Render session list in the left sidebar,
                // spanning the full pane height (top to bottom)
                render_left_session_list(frame, sidebar_area, &list_pane, *session_id, &mut *app);
            }
        }
    }
}

/// Paint the side gutters left over when `inner` (the centered content/input-bar
/// column) is narrower than `outer` (the full, unnarrowed `session_area`) with the
/// same detail-surface background `render_session_detail` fills its own content
/// with — so the gutters read as a continuation of the panel surface, never bare
/// terminal background. A no-op (paints nothing) when `inner == outer`, i.e. when
/// `compute_centered_detail_area` did not engage.
fn paint_detail_gutters(
    frame: &mut Frame,
    outer: Rect,
    inner: Rect,
    settings: &crate::settings::UserSettings,
) {
    let bg = session::detail_surface_bg(settings);
    if inner.x > outer.x {
        session::fill_area(
            frame,
            Rect::new(outer.x, outer.y, inner.x - outer.x, outer.height),
            bg,
        );
    }
    let inner_right = inner.x + inner.width;
    let outer_right = outer.x + outer.width;
    if outer_right > inner_right {
        session::fill_area(
            frame,
            Rect::new(
                inner_right,
                outer.y,
                outer_right - inner_right,
                outer.height,
            ),
            bg,
        );
    }
}

fn file_viewer_area_with_explorer_drawer(area: Rect, frame_area: Rect, app: &App) -> Rect {
    if !matches!(
        app.overlay,
        OverlayState::FileExplorer {
            explorer_focused: false,
            ..
        }
    ) {
        return area;
    }

    let drawer_right = frame_area
        .x
        .saturating_add(overlay::file_explorer_drawer_width(frame_area.width));
    let min_x = drawer_right.saturating_add(1);
    let area_right = area.x.saturating_add(area.width);

    if area.x >= min_x {
        return area;
    }

    if min_x >= area_right {
        return Rect {
            x: area_right.saturating_sub(1),
            width: 1,
            ..area
        };
    }

    Rect {
        x: min_x,
        width: area_right.saturating_sub(min_x),
        ..area
    }
}

/// Inclusive bounds (percent of viewport width) for the session-list sidebar.
///
/// The resize handlers in `action_handler::window` clamp the stored width to
/// this SAME range, so the stored width can never drift past what the renderer
/// shows here. If the grow ceiling exceeded `SIDEBAR_MAX_PCT`, a reversed resize
/// drag would waste 1–2 keypresses re-syncing the off-screen width back down to
/// the rendered ceiling before the divider moved (#43).
pub const SIDEBAR_MIN_PCT: u16 = 20;
pub const SIDEBAR_MAX_PCT: u16 = 55;

fn compute_detail_layout_areas(area: Rect, app: &App) -> (Rect, Rect) {
    if area.width < 80 {
        return (Rect::new(area.x, area.y, 0, area.height), area);
    }

    let pct = app.active_tab().session_list_width_pct;
    let sidebar_pct = if pct > 0 {
        pct.clamp(SIDEBAR_MIN_PCT, SIDEBAR_MAX_PCT)
    } else {
        33
    };
    let sidebar_w = ((area.width as u32 * sidebar_pct as u32) / 100) as u16;
    let sidebar_max = area.width.saturating_sub(48);
    let sidebar_min = 34.min(sidebar_max);
    let sidebar_w = sidebar_w.clamp(sidebar_min, sidebar_max);
    let separator_w = 1;
    let sidebar = Rect::new(area.x, area.y, sidebar_w, area.height);
    let detail_x = area.x + sidebar_w + separator_w;
    let detail_w = area.width.saturating_sub(sidebar_w + separator_w);
    let detail = Rect::new(detail_x, area.y, detail_w, area.height);
    (sidebar, detail)
}

/// Detail-pane content width above which the transcript/input-bar column is
/// capped and centered, leaving symmetric side gutters filled with the panel
/// background. At or below this width, centering is a no-op — every column
/// counts on a narrow terminal, matching the deleted mechanism's original
/// intent (F-002) without its bug (below).
const DETAIL_CENTERING_ENABLE_WIDTH: u16 = 110;

/// Target column width once centering is enabled. Deliberately NOT equal to
/// `DETAIL_CENTERING_ENABLE_WIDTH` — the deleted `compute_centered_area` set
/// `MIN_WIDTH_FOR_CENTERING` == `MAX_CENTERED_WIDTH` == 104, which a 2026-02-08
/// handoff diagnosed as making centering "a complete no-op on any terminal
/// <= 120 columns wide" (F-003). A fixed 10-column gap between the two
/// constants guarantees the very first width that triggers centering already
/// shows a clearly-intentional, non-zero gutter, not a 0-1 column sliver.
const DETAIL_MAX_CONTENT_WIDTH: u16 = 100;

/// Horizontal width consumed between a centered detail column and a table's
/// rendered box: transcript inset (4) plus event-card border/padding (4).
const TABLE_DETAIL_HORIZ_INSET: u16 = theme::SESSION_DETAIL_HORIZ_INSET * 2;

/// Return the centered detail-column width needed for the widest rendered
/// Markdown table in this session. Prose-only sessions retain the established
/// 100-column reading measure; a table may request more when the detail pane
/// has otherwise unused width.
fn table_requested_detail_width(events: &[rsi_common::types::ConversationEvent]) -> u16 {
    events
        .iter()
        .filter_map(content::event_markdown_table_width)
        .max()
        .map(|table_width| table_width.saturating_add(TABLE_DETAIL_HORIZ_INSET))
        .unwrap_or(DETAIL_MAX_CONTENT_WIDTH)
        .max(DETAIL_MAX_CONTENT_WIDTH)
}

/// Memoize the session-wide table request until its event generation changes.
/// This keeps idle wide-detail redraws from reparsing every conversation line.
fn cached_table_requested_detail_width(state: &mut crate::types::SessionState) -> u16 {
    if let Some((generation, width)) = state.table_detail_width_cache
        && generation == state.events_generation
    {
        return width;
    }

    let width = table_requested_detail_width(&state.events);
    state.table_detail_width_cache = Some((state.events_generation, width));
    width
}

/// Center `area` to the requested transcript width once `area.width` exceeds
/// `DETAIL_CENTERING_ENABLE_WIDTH`; otherwise return `area` unchanged. The
/// requested width is clamped to the available pane, so a table can use spare
/// width but cannot force horizontal overflow.
fn compute_centered_detail_area(area: Rect, requested_width: u16) -> Rect {
    if area.width <= DETAIL_CENTERING_ENABLE_WIDTH {
        return area;
    }
    let target_width = area
        .width
        .min(requested_width.max(DETAIL_MAX_CONTENT_WIDTH));
    let gutter = (area.width - target_width) / 2;
    Rect::new(area.x + gutter, area.y, target_width, area.height)
}

/// Calculate the current width of the session list in the left gutter.
pub(crate) fn compute_session_list_width(app: &App) -> u16 {
    let term_width = terminal_width_for_layout(app);
    let pct = app.active_tab().session_list_width_pct;

    // Default width if not pct mode
    if pct == 0 {
        40
    } else {
        (term_width as u32 * pct as u32 / 100) as u16
    }
}

/// Render the session list in the left gutter sidebar.
fn render_left_session_list(
    frame: &mut Frame,
    list_area: Rect,
    list_pane: &Pane,
    viewed_session_id: uuid::Uuid,
    app: &mut App,
) {
    if list_area.width < 4 || list_area.height == 0 {
        return; // Physically can't render (zero or negative area)
    }

    let list_focused = true;
    session::render_session_list(
        frame,
        list_area,
        list_pane,
        list_focused,
        app,
        session::SessionListSurface::Embedded,
        Some(viewed_session_id),
    );
}

fn active_transient_notification(app: &App) -> Option<&crate::types::Notification> {
    use crate::types::NotificationPriority;
    app.notifications.iter().rev().find(|notification| {
        !notification.dismissed
            && notification.created_at.elapsed() < notification.ttl
            && notification.priority >= NotificationPriority::Medium
    })
}

fn should_render_root_footer(app: &App) -> bool {
    app.input_mode != crate::types::InputMode::Normal
}

fn render_command_bar(frame: &mut Frame, area: Rect, app: &App) {
    use crate::types::{InputMode, SearchTarget};

    let bg = theme::status_line_bg();

    let content = match app.input_mode {
        InputMode::Command => format!(":{}", app.command_buffer),
        InputMode::Input => format!("> {}", app.input_buffer),
        InputMode::Search => {
            if app.search_matches.is_empty()
                && !app.search_query.is_empty()
                && app.search_target == SearchTarget::SessionDetail
            {
                format!("/{} [no matches]", app.search_query)
            } else if !app.search_matches.is_empty() {
                format!(
                    "/{} [{}/{}]",
                    app.search_query,
                    app.search_match_cursor + 1,
                    app.search_matches.len()
                )
            } else {
                format!("/{}", app.search_query)
            }
        }
        InputMode::Normal => String::new(),
    };

    let style = match app.input_mode {
        InputMode::Normal => Style::default().fg(theme::command_bar_idle()),
        InputMode::Command | InputMode::Input => Style::default().fg(theme::command_bar_active()),
        InputMode::Search => Style::default().fg(theme::green()),
    };

    let bar = Paragraph::new(content).style(style.bg(bg));
    frame.render_widget(bar, area);
}

// `rail_label_style`, `rail_border_style`, and `render_sidebar_section` were
// the styling/widget helpers consumed by the now-deleted compact dashboard.
// Their only remaining function-call sites were each other. Deleted alongside
// the dashboard in the Phase 5 cleanup of the bottom-strip operations-deck
// redesign.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::app_test_helpers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        let area = buffer.area;
        let mut out = String::new();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
        }
        out
    }

    /// Four sessions, one Running. Other states exercise toolbar scope: only
    /// active-session count belongs in top chrome. `app.session_order` is set
    /// explicitly for test-fixture completeness.
    fn fixture_app() -> App {
        let mut app = app_test_helpers::with_session_list(4);
        let ids = app.filtered_session_order.clone();
        app.session_order = ids.clone();
        if let Some(state) = app.sessions.get_mut(&ids[0]) {
            state.session.status = rsi_common::types::SessionStatus::Running;
        }
        if let Some(state) = app.sessions.get_mut(&ids[1]) {
            state.session.status = rsi_common::types::SessionStatus::Failed;
        }
        if let Some(state) = app.sessions.get_mut(&ids[2]) {
            state.session.status = rsi_common::types::SessionStatus::WaitingApproval;
        }
        if let Some(state) = app.sessions.get_mut(&ids[3]) {
            state.session.status = rsi_common::types::SessionStatus::Completed;
            state.session.pending_question = Some(rsi_common::types::PendingQuestion {
                questions: Vec::new(),
            });
        }
        app
    }

    #[test]
    fn top_chrome_header_content_is_width_invariant() {
        let app = fixture_app();
        let _theme_render_guard = theme::test_render_guard();
        let mut contents = Vec::new();
        for width in [80u16, 111, 112, 160] {
            let backend = TestBackend::new(width, 1);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| {
                    render_top_chrome(frame, Rect::new(0, 0, width, 1), &app);
                })
                .expect("draw");
            if width == 80 {
                assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), " ");
                assert_eq!(terminal.backend().buffer()[(1, 0)].symbol(), "●");
                assert_eq!(terminal.backend().buffer()[(1, 0)].fg, theme::green());
            }
            let text = buffer_text(terminal.backend().buffer()).trim().to_string();
            assert!(
                !text.contains('$'),
                "budget chip must be gone at width {width}: {text:?}"
            );
            assert!(
                !text.contains("RSI"),
                "brand chip must be gone at width {width}: {text:?}"
            );
            assert!(
                !text.contains('✗'),
                "failed chip must be gone at width {width}: {text:?}"
            );
            contents.push(text);
        }
        assert!(
            contents.windows(2).all(|w| w[0] == w[1]),
            "header content must be identical across widths: {contents:?}"
        );
        assert_eq!(contents[0], "● 1");
    }

    #[test]
    fn top_chrome_places_color_coded_project_tabs_on_right() {
        let mut app = fixture_app();
        let _theme_render_guard = theme::test_render_guard();
        let project_id = uuid::Uuid::new_v4();
        let mut project_tab = app.tabs[0].clone();
        project_tab.project_id = Some(project_id);
        app.tabs.push(project_tab);
        app.active_tab = 1;
        app.projects.push(rsi_common::types::Project {
            id: project_id,
            name: "core".to_string(),
            path: None,
            description: None,
            color: "#010203".to_string(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        });

        let backend = TestBackend::new(40, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                render_top_chrome(frame, Rect::new(0, 0, 40, 1), &app);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(36, 0)].symbol(), "●");
        assert_eq!(buffer[(36, 0)].fg, theme::subtext0());
        assert_eq!(buffer[(38, 0)].symbol(), "◉");
        assert_eq!(buffer[(38, 0)].fg, Color::Rgb(1, 2, 3));
        assert_eq!(buffer[(38, 0)].bg, theme::status_line_bg());
        assert_eq!(buffer[(39, 0)].symbol(), " ");
    }

    #[test]
    fn top_chrome_centers_transient_notification_in_safe_lane() {
        let mut app = fixture_app();
        app.notify_success("saved");
        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                render_top_chrome(frame, Rect::new(0, 0, 80, 1), &app);
            })
            .expect("draw");
        let text = buffer_text(terminal.backend().buffer());
        let byte_x = text.find("saved").expect("centered notification");
        let cell_x = text[..byte_x].chars().count();

        assert_eq!(cell_x, 37, "notification must center on the full bar");
        assert_eq!(
            text.chars().nth(1),
            Some('●'),
            "left counters must remain visible after edge inset"
        );
    }

    #[test]
    fn model_dropdown_anchor_present_whenever_area_nonzero() {
        assert!(model_dropdown_anchor(Rect::new(0, 0, 80, 1)).is_some());
        assert!(model_dropdown_anchor(Rect::new(5, 2, 1, 1)).is_some());
        assert!(model_dropdown_anchor(Rect::new(0, 0, 0, 1)).is_none());
        assert!(model_dropdown_anchor(Rect::new(0, 0, 80, 0)).is_none());
    }

    #[test]
    fn model_dropdown_renders_at_fixed_anchor_after_header_cut() {
        let mut app = fixture_app();
        app.model_dropdown = crate::types::ModelDropdownState::new(
            app.selected_provider,
            app.available_models.clone(),
            app.selected_model.as_deref(),
        );
        app.model_dropdown.open = true;
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 80, 10);
                let header = Rect::new(0, 0, 80, 1);
                let anchor = render_top_chrome(frame, header, &app);
                if let Some(anchor) = anchor {
                    widget::model_dropdown::render_model_dropdown(
                        frame,
                        area,
                        anchor,
                        &app.model_dropdown,
                        app.selected_model.as_deref(),
                        true,
                    );
                }
            })
            .expect("draw");
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("Model ["),
            "dropdown must render post-cut: {text:?}"
        );
    }

    #[test]
    fn detail_sidebar_breakpoint_is_bounded_for_80_to_82_columns() {
        let app = fixture_app();
        let (hidden, detail) = compute_detail_layout_areas(Rect::new(0, 0, 79, 20), &app);
        assert_eq!(hidden.width, 0);
        assert_eq!(detail.width, 79);

        for width in 80..=82 {
            let area = Rect::new(0, 0, width, 20);
            let (sidebar, detail) = compute_detail_layout_areas(area, &app);
            assert!(
                sidebar.width > 0,
                "sidebar should appear at {width} columns"
            );
            assert_eq!(sidebar.width + 1 + detail.width, width);
            assert!(detail.width >= 47, "detail should retain a usable measure");
        }
    }

    #[test]
    fn compute_centered_detail_area_is_noop_at_or_below_enable_threshold() {
        for width in [80u16, 100, DETAIL_CENTERING_ENABLE_WIDTH] {
            let area = Rect::new(3, 7, width, 40);
            let result = compute_centered_detail_area(area, DETAIL_MAX_CONTENT_WIDTH);
            assert_eq!(
                result, area,
                "width {width} must be a no-op (<= enable threshold)"
            );
        }
    }

    #[test]
    fn compute_centered_detail_area_caps_and_centers_above_threshold() {
        for width in [120u16, 160, 220] {
            let area = Rect::new(0, 0, width, 40);
            let result = compute_centered_detail_area(area, DETAIL_MAX_CONTENT_WIDTH);
            assert_eq!(
                result.width, DETAIL_MAX_CONTENT_WIDTH,
                "width {width} should cap to DETAIL_MAX_CONTENT_WIDTH"
            );
            let left_gutter = result.x - area.x;
            let right_gutter = (area.x + area.width) - (result.x + result.width);
            assert!(
                left_gutter.abs_diff(right_gutter) <= 1,
                "width {width}: gutters must be symmetric within 1 column (left={left_gutter}, right={right_gutter})"
            );
            assert_eq!(left_gutter, (width - DETAIL_MAX_CONTENT_WIDTH) / 2);
            assert_eq!(result.y, area.y);
            assert_eq!(result.height, area.height);
        }
    }

    #[test]
    fn compute_centered_detail_area_expands_for_a_wide_table() {
        let area = Rect::new(0, 0, 220, 40);
        let result = compute_centered_detail_area(area, 158);
        assert_eq!(result.width, 158);
        assert_eq!(result.x, 31);

        let narrow = Rect::new(0, 0, 120, 40);
        assert_eq!(compute_centered_detail_area(narrow, 158), narrow);
    }

    #[test]
    fn table_requested_detail_width_preserves_prose_measure_until_needed() {
        use rsi_common::types::{ConversationEvent, EventType, Role};

        assert_eq!(table_requested_detail_width(&[]), DETAIL_MAX_CONTENT_WIDTH);

        let wide_cell = "x".repeat(120);
        let event = ConversationEvent {
            id: 0,
            session_id: uuid::Uuid::new_v4(),
            sequence: 1,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            content: format!("| {wide_cell} |\n|---|"),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };

        // 120 rendered cell columns + 4 table-border/padding columns + the
        // two established four-column transcript/card insets.
        assert_eq!(table_requested_detail_width(&[event]), 132);
    }

    #[test]
    fn content_area_and_input_bar_area_share_x_and_width_after_centering() {
        let area = Rect::new(0, 0, 220, 40);
        let centered = compute_centered_detail_area(area, 158);
        let chunks = Layout::vertical([Constraint::Fill(1), Constraint::Length(5)]).split(centered);
        assert_eq!(chunks[0].x, chunks[1].x, "content/input-bar x must match");
        assert_eq!(
            chunks[0].width, chunks[1].width,
            "content/input-bar width must match"
        );
        assert_eq!(chunks[0].x, centered.x);
        assert_eq!(chunks[0].width, centered.width);
    }

    #[test]
    fn root_footer_is_reserved_only_for_transient_input() {
        let mut app = fixture_app();
        app.input_mode = crate::types::InputMode::Normal;
        app.notifications.clear();
        assert!(!should_render_root_footer(&app));

        for mode in [
            crate::types::InputMode::Command,
            crate::types::InputMode::Input,
            crate::types::InputMode::Search,
        ] {
            app.input_mode = mode;
            assert!(should_render_root_footer(&app));
        }

        app.input_mode = crate::types::InputMode::Normal;
        app.notify_success("saved");
        assert!(!should_render_root_footer(&app));
    }

    #[test]
    fn render_survives_root_input_footer_disappearing_between_frames() {
        let mut app = fixture_app();
        app.input_mode = crate::types::InputMode::Command;
        app.command_buffer = "alerts".to_string();
        let backend = TestBackend::new(100, 36);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("draw with input footer");
        app.input_mode = crate::types::InputMode::Normal;
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("draw without input footer");
        assert!(!buffer_text(terminal.backend().buffer()).contains("j/k move"));
    }

    #[test]
    fn top_notification_roles_repaint_on_next_frame() {
        theme::with_theme_state(|| {
            let mut app = fixture_app();
            let backend = TestBackend::new(40, 1);
            let mut terminal = Terminal::new(backend).expect("terminal");

            app.notify_success("saved");
            theme::set_theme_role_override(
                crate::ui::theme_roles::ThemeRole::Toast,
                Some([10, 20, 30]),
            );
            terminal
                .draw(|frame| {
                    render_top_chrome(frame, frame.area(), &app);
                })
                .expect("toast draw");
            let toast_x = (0..40)
                .find(|x| terminal.backend().buffer()[(*x, 0)].symbol() == "s")
                .expect("toast text");
            assert_eq!(
                terminal.backend().buffer()[(toast_x, 0)].fg,
                ratatui::style::Color::Rgb(10, 20, 30)
            );

            app.notifications.clear();
            app.notify_error("failed");
            theme::set_theme_role_override(
                crate::ui::theme_roles::ThemeRole::Error,
                Some([40, 50, 60]),
            );
            terminal
                .draw(|frame| {
                    render_top_chrome(frame, frame.area(), &app);
                })
                .expect("error draw");
            let error_x = (0..40)
                .find(|x| terminal.backend().buffer()[(*x, 0)].symbol() == "f")
                .expect("error text");
            assert_eq!(
                terminal.backend().buffer()[(error_x, 0)].fg,
                ratatui::style::Color::Rgb(40, 50, 60)
            );
        });
    }

    #[test]
    fn elevated_surface_and_unfocused_border_overrides_repaint_their_rendered_consumers() {
        use crate::types::OverlayState;
        use crate::ui::theme_roles::ThemeRole;

        theme::with_theme_state(|| {
            let mut app = fixture_app();
            let session_id = app.selected_session_id().expect("selected session");
            app.overlay = OverlayState::SessionInfoPanel { session_id };
            let backend = TestBackend::new(120, 36);
            let mut terminal = Terminal::new(backend).expect("terminal");

            terminal
                .draw(|frame| render(frame, &mut app))
                .expect("baseline draw");
            theme::set_theme_role_override(ThemeRole::ElevatedSurface, Some([61, 71, 81]));
            terminal
                .draw(|frame| render(frame, &mut app))
                .expect("override draw");
            let hint = terminal
                .backend()
                .buffer()
                .content
                .windows(2)
                .find(|cells| cells[0].symbol() == "R" && cells[1].symbol() == ":")
                .map(|cells| &cells[0])
                .expect("Session Info hint cell");
            assert_eq!(hint.bg, ratatui::style::Color::Rgb(61, 71, 81));

            assert!(theme::set_theme_by_name("transparent"));
            let backend = TestBackend::new(20, 4);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| frame.render_widget(theme::pane_block(false), frame.area()))
                .expect("baseline border draw");
            theme::set_theme_role_override(ThemeRole::Border, Some([62, 72, 82]));
            terminal
                .draw(|frame| frame.render_widget(theme::pane_block(false), frame.area()))
                .expect("override border draw");
            assert!(
                terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .any(|cell| cell.fg == ratatui::style::Color::Rgb(62, 72, 82)),
                "custom Border role must reach the unfocused pane border"
            );
        });
    }

    #[test]
    fn target_surfaces_render_under_every_builtin_and_a_custom_role_set() {
        use crate::types::{IssueWorkspaceState, IssueWorkspaceTab, OverlayState, Pane};
        use crate::ui::theme_roles::ThemeRole;
        use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};

        theme::with_theme_state(|| {
            let render_text = |app: &mut App| {
                let backend = TestBackend::new(120, 36);
                let mut terminal = Terminal::new(backend).expect("terminal");
                terminal.draw(|frame| render(frame, app)).expect("render");
                buffer_text(terminal.backend().buffer())
            };
            let render_buffer = |app: &mut App| {
                let backend = TestBackend::new(120, 36);
                let mut terminal = Terminal::new(backend).expect("terminal");
                terminal.draw(|frame| render(frame, app)).expect("render");
                terminal.backend().buffer().clone()
            };
            let render_session_buffer = |app: &mut App| {
                let pane = app.focused_pane().cloned().expect("session list pane");
                let backend = TestBackend::new(120, 36);
                let mut terminal = Terminal::new(backend).expect("terminal");
                terminal
                    .draw(|frame| {
                        session::render_session_list(
                            frame,
                            frame.area(),
                            &pane,
                            true,
                            app,
                            session::SessionListSurface::Full,
                            None,
                        )
                    })
                    .expect("session render");
                terminal.backend().buffer().clone()
            };
            let has_fg = |buffer: &ratatui::buffer::Buffer, color| {
                buffer.content.iter().any(|cell| cell.fg == color)
            };
            let has_bg = |buffer: &ratatui::buffer::Buffer, color| {
                buffer.content.iter().any(|cell| cell.bg == color)
            };
            let install_dispatched = |app: &mut App, identifier: &str| {
                let focused = app.active_tab().focused_pane;
                let mut state = IssueWorkspaceState::new(Some(uuid::Uuid::new_v4()));
                state.active_tab = IssueWorkspaceTab::Dispatched;
                let session_id = uuid::Uuid::new_v4();
                state.dispatched.selected_session_id = Some(session_id);
                state
                    .data_state_mut(crate::types::IssueWorkspaceDataKind::LocalPage)
                    .load_state = crate::types::IssueWorkspaceLoadState::Fresh;
                state.transient.dispatched =
                    vec![rsi_common::issue_workspace::IssueDispatchRecordV1 {
                        issue_id: uuid::Uuid::new_v4().to_string(),
                        issue_identifier: identifier.to_string(),
                        tracker: "fixture".to_string(),
                        session_id,
                        dispatched_at: chrono::Utc::now(),
                        last_reconciled_at: None,
                        terminal_state: None,
                    }];
                *app.active_tab_mut()
                    .layout
                    .find_pane_mut(focused)
                    .expect("focused pane") = Pane::Issues(state);
            };
            let job = ScheduledJob {
                id: uuid::Uuid::new_v4(),
                name: "matrix-job".into(),
                message: "matrix".into(),
                schedule: ScheduleSpec {
                    recurrence: Recurrence::Once,
                    anchor: chrono::Utc::now(),
                },
                last_fired_at: None,
                next_fire_at: chrono::Utc::now(),
                enabled: true,
                working_dir: None,
                provider: None,
                model: None,
                project_id: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                wake_mode: WakeMode::Fresh,
                wake_session_id: None,
            };

            for index in 0..theme::THEME_COUNT {
                theme::clear_theme_role_overrides();
                theme::set_theme_by_index(index);
                let mut app = fixture_app();
                let selected = app.selected_session_id().expect("selected session");
                app.sessions
                    .get_mut(&selected)
                    .expect("session")
                    .session
                    .pinned_at = Some(chrono::Utc::now());
                let session_buffer = render_session_buffer(&mut app);
                let session = buffer_text(&session_buffer);
                assert!(
                    !session.trim().is_empty(),
                    "session list must retain text/glyph output for theme {index}"
                );
                assert!(
                    has_bg(&session_buffer, theme::selected_row_bg()),
                    "session selected-row role must render for theme {index}"
                );

                let focused = app.active_tab().focused_pane;
                *app.active_tab_mut()
                    .layout
                    .find_pane_mut(focused)
                    .expect("focused pane") = Pane::Settings;
                let settings = render_text(&mut app);
                assert!(settings.contains("Settings"), "settings theme {index}");
                assert!(
                    has_bg(&render_buffer(&mut app), theme::root_bg()),
                    "settings canvas role must render for theme {index}"
                );

                install_dispatched(&mut app, "RSI-1");
                let issues = render_text(&mut app);
                assert!(issues.contains("RSI-1"), "issue cue theme {index}");
                assert!(
                    has_fg(&render_buffer(&mut app), theme::status_running()),
                    "issue running role must render for theme {index}"
                );

                app.overlay = OverlayState::ScheduleBrowser {
                    jobs: vec![job.clone()],
                    selected_index: 0,
                    loading: false,
                    pending_delete: false,
                };
                let schedules = render_text(&mut app);
                assert!(
                    schedules.contains("> [+] matrix-job"),
                    "schedule cue theme {index}"
                );
                assert!(
                    has_fg(&render_buffer(&mut app), theme::accent()),
                    "schedule selected-row accent must render for theme {index}"
                );
            }

            let mut app = fixture_app();
            let selected = app.selected_session_id().expect("selected session");
            app.sessions
                .get_mut(&selected)
                .expect("session")
                .session
                .pinned_at = Some(chrono::Utc::now());
            theme::set_theme_role_override(ThemeRole::SelectedRow, Some([31, 41, 51]));
            let custom_session_buffer = render_session_buffer(&mut app);
            let custom_session = buffer_text(&custom_session_buffer);
            assert!(!custom_session.trim().is_empty());
            assert!(
                has_bg(
                    &custom_session_buffer,
                    ratatui::style::Color::Rgb(31, 41, 51)
                ),
                "custom SelectedRow role must reach Session List"
            );
            let focused = app.active_tab().focused_pane;
            *app.active_tab_mut()
                .layout
                .find_pane_mut(focused)
                .expect("focused pane") = Pane::Settings;
            theme::set_theme_role_override(ThemeRole::Canvas, Some([32, 42, 52]));
            assert!(render_text(&mut app).contains("Settings"));
            assert!(
                has_bg(
                    &render_buffer(&mut app),
                    ratatui::style::Color::Rgb(32, 42, 52)
                ),
                "custom Canvas role must reach Settings"
            );
            install_dispatched(&mut app, "RSI-custom");
            theme::set_theme_role_override(ThemeRole::Running, Some([33, 43, 53]));
            assert!(render_text(&mut app).contains("RSI-custom"));
            assert!(
                has_fg(
                    &render_buffer(&mut app),
                    ratatui::style::Color::Rgb(33, 43, 53)
                ),
                "custom Running role must reach Issue Tracker"
            );
            app.overlay = OverlayState::ScheduleBrowser {
                jobs: vec![job],
                selected_index: 0,
                loading: false,
                pending_delete: false,
            };
            theme::set_theme_role_override(ThemeRole::Accent, Some([34, 44, 54]));
            assert!(render_text(&mut app).contains("> [+] matrix-job"));
            assert!(
                has_fg(
                    &render_buffer(&mut app),
                    ratatui::style::Color::Rgb(34, 44, 54)
                ),
                "custom Accent role must reach Scheduled Jobs"
            );
        });
    }
}
