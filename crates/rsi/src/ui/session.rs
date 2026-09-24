//! Session pane rendering.

use super::{content, glyphs, navigator_layout, status, theme};
use crate::app::App;
use crate::profiling;
use crate::settings::ActivityIndicatorStyle;
use crate::types::{
    FailureEvidenceSource, Pane, RecursiveDagSessionContext, SessionActivityViewModel,
    SessionFocusEntry, SessionFocusGroup, SessionInspectorBody, SessionInspectorViewModel,
    SessionState, WaitingRequirement,
};
use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Padding, Paragraph, Wrap};
use rsi_common::types::{ConversationEvent, EventType, Role, Session, SessionKind, SessionStatus};
use std::collections::{HashMap, HashSet};

/// Maximum number of visual lines the input bar can grow to.
const MAX_INPUT_BAR_LINES: u16 = 10;

/// Dashed border set used for tool call events (ToolUse / ToolResult).
/// Uses light triple-dash box-drawing characters for a dotted appearance.
const TOOL_CALL_BORDER: symbols::border::Set = symbols::border::Set {
    top_left: "┌",
    top_right: "┐",
    bottom_left: "└",
    bottom_right: "┘",
    vertical_left: "┆",
    vertical_right: "┆",
    horizontal_top: "┄",
    horizontal_bottom: "┄",
};

/// Height in terminal rows of the classic rainbow loading indicator.
pub const LOADING_CONTAINER_HEIGHT: u16 = 2;

/// Height in terminal rows of the compact rainbow loading indicator.
pub const COMPACT_RAINBOW_HEIGHT: u16 = 1;

/// Title, compact metadata, and divider rows above the transcript.
pub const SESSION_DETAIL_HEADER_HEIGHT: u16 = 3;

/// Duration in milliseconds of the message-formulation grow animation.
pub const FORMULATION_ANIMATION_MS: i64 = 500;

/// Shrink an event's full allocated rect down to the rows its card actually
/// occupies (border + content), excluding the trailing `EVENT_CARD_GAP`
/// separator row baked into `event_height` by `ui::height::update_event_heights`.
///
/// The separator row is left unpainted by the card so it keeps showing the
/// pane's base background (filled once per frame before any card renders) —
/// that's what visually separates two consecutive cards instead of letting
/// same-colored bubbles fuse into one block.
fn card_rect_within(
    event_rect: Rect,
    event_height: usize,
    lines_to_skip: usize,
    remaining_viewport: usize,
) -> Rect {
    let card_rows = event_height.saturating_sub(crate::ui::height::EVENT_CARD_GAP);
    let card_height = (card_rows.saturating_sub(lines_to_skip)).min(remaining_viewport);
    Rect::new(
        event_rect.x,
        event_rect.y,
        event_rect.width,
        card_height as u16,
    )
}

pub(crate) fn fill_area(frame: &mut Frame, area: Rect, bg: Color) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(Block::default().style(Style::default().bg(bg)), area);
}

/// Vertical offset from the top of the session detail content area to the first
/// transcript row. This must mirror the rows rendered above conversation events.
pub(crate) fn session_detail_content_y_offset(
    at_top: bool,
    has_active_task: bool,
    has_pending_archive: bool,
    has_issue_url: bool,
    has_sandbox_info: bool,
    has_terminal_info: bool,
    has_recursive_dag: bool,
) -> u16 {
    if !at_top {
        return SESSION_DETAIL_HEADER_HEIGHT;
    }

    let active_task_h = u16::from(has_active_task);
    let pending_archive_h = u16::from(has_pending_archive);
    let issue_url_h = u16::from(has_issue_url);
    let sandbox_info_h = u16::from(has_sandbox_info);
    let terminal_info_h = u16::from(has_terminal_info);
    let recursive_dag_h = u16::from(has_recursive_dag);

    SESSION_DETAIL_HEADER_HEIGHT
        + active_task_h
        + pending_archive_h
        + issue_url_h
        + sandbox_info_h
        + terminal_info_h
        + recursive_dag_h
}

fn event_bubble_bg(event: &ConversationEvent, settings: &crate::settings::UserSettings) -> Color {
    let fallback = match event.event_type {
        EventType::ToolUse | EventType::ToolResult => theme::tool_bubble_bg(),
        EventType::System | EventType::Thinking | EventType::Compressed => {
            theme::system_bubble_bg()
        }
        EventType::Message => match event.role {
            Some(Role::User) => theme::user_bubble_bg(),
            Some(Role::Assistant) => theme::assistant_bubble_bg(),
            _ => theme::system_bubble_bg(),
        },
        _ => theme::system_bubble_bg(),
    };
    if theme::is_transparent_theme() {
        fallback
    } else {
        crate::settings::text_area_bg_color(settings, fallback)
    }
}

// ── Shared session metadata helpers ──────────────────────────────────────

/// Compute right-aligned time text and color for a session card.
///
/// PI-5: uniformly "time since last event" — `format_relative_time` of
/// `updated_at`, regardless of status. Previously this branched on
/// `Running`/`Starting` to show elapsed-since-`created_at` instead, which
/// conflated "how long has this session existed" with "time since last
/// event" (F-019). The former question is now answered by the dedicated
/// created + accumulated-work-time columns; this fn now has a single,
/// uniform meaning consumed by every density tier and the last-event column.
pub(crate) fn compute_time_display(
    session: &rsi_common::types::Session,
) -> (String, ratatui::style::Color) {
    let color = status_color(session.status);
    let text = format_relative_time(session.updated_at);
    (text, color)
}

/// Map session recency to a title color for heat visualization.
pub(crate) fn compute_heat_color(updated_at: DateTime<Utc>) -> ratatui::style::Color {
    let secs = Utc::now()
        .signed_duration_since(updated_at)
        .num_seconds()
        .max(0);
    if secs < 300 {
        theme::text()
    } else if secs < 3600 {
        theme::subtext1()
    } else if secs < 21600 {
        theme::subtext0()
    } else {
        theme::overlay1()
    }
}

/// Return a short display label for a model string, e.g. "claude-opus-4-5" → "opus".
pub(crate) fn short_model_label(model: Option<&str>) -> Option<String> {
    let m = model?;
    let m_lower = m.to_lowercase();
    // Match common model families
    if m_lower.contains("opus") {
        Some("opus".to_string())
    } else if m_lower.contains("sonnet") {
        Some("sonnet".to_string())
    } else if m_lower.contains("haiku") {
        Some("haiku".to_string())
    } else if m_lower.contains("gemini-2.5-pro") || m_lower.contains("gemini-2-5-pro") {
        Some("gem-2.5p".to_string())
    } else if m_lower.contains("gemini-2.5-flash") || m_lower.contains("gemini-2-5-flash") {
        Some("gem-2.5f".to_string())
    } else if m_lower.contains("gemini-3") {
        Some("gem-3".to_string())
    } else if m_lower.contains("gemini") {
        Some("gemini".to_string())
    } else if m_lower.contains("codex") {
        Some("codex".to_string())
    } else if m_lower.contains("gpt-oss") {
        Some("gpt-oss".to_string())
    } else if m_lower.contains("gpt-4o") {
        Some("gpt-4o".to_string())
    } else if m_lower.contains("gpt-4") {
        Some("gpt-4".to_string())
    } else if m_lower.contains("gpt") {
        Some("gpt".to_string())
    } else if m_lower.contains("deepseek") {
        Some("deepseek".to_string())
    } else if m_lower.contains("qwen") {
        Some("qwen".to_string())
    } else {
        // Fallback: first 8 chars of model name
        Some(m.chars().take(8).collect())
    }
}

/// Build effort-level bars: filled = yellow ▮, empty = dimmed ▯.
/// Returns empty vec if model doesn't support effort.
pub(crate) fn build_effort_bars(effort: Option<&str>, model: Option<&str>) -> Vec<Span<'static>> {
    let (total_bars, filled) = effort_bar_counts(effort, model);
    if total_bars == 0 {
        return vec![];
    }

    let mut spans = vec![Span::styled("  ", Style::default())];
    for i in 0..total_bars {
        if i < filled {
            spans.push(Span::styled(
                "▮",
                Style::default()
                    .fg(theme::yellow())
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled("▯", Style::default().fg(theme::surface1())));
        }
    }
    spans
}

/// Returns (total_bars, filled_bars) for the given effort level and model.
/// (0, 0) if effort bars should not be displayed.
pub(crate) fn effort_bar_counts(effort: Option<&str>, model: Option<&str>) -> (usize, usize) {
    let m = match model {
        Some(m) => m,
        None => return (0, 0),
    };
    let ladder = rsi_common::model_utils::effort_ladder(m);
    if ladder.is_empty() {
        return (0, 0);
    }
    let position = |level: &str| {
        ladder
            .iter()
            .position(|candidate| *candidate == level)
            .map(|index| index + 1)
    };
    let default_filled = rsi_common::model_utils::default_effort_level(m)
        .and_then(position)
        .unwrap_or(ladder.len());
    let filled = effort.and_then(position).unwrap_or(default_filled);
    (ladder.len(), filled)
}

/// TD1 accumulated work-time display. Deliberately h/m-combining, matching
/// `compute_time_display`'s pre-PI-5 elapsed-time style (F-026 precedent) —
/// NOT "compute time" or "CPU time": per TD1's carried-forward review note,
/// this includes idle-but-Running app-server wait time for multi-turn
/// sessions awaiting the next user prompt, so the label/format must not
/// imply pure model-compute time. Callers use the plain header "WORK"
/// (short-column) and "Accumulated work time" (settings label) — never
/// "compute"/"CPU".
pub(crate) fn format_work_time_ms(ms: u64) -> String {
    let secs = ms / 1000;
    if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        "<1m".to_string()
    }
}

/// Absolute created-at display, local time, matching the dominant compact
/// date+time convention used elsewhere in the TUI (F-027,
/// `overlay/recursive_dag.rs`).
pub(crate) fn created_text(created_at: DateTime<Utc>) -> String {
    created_at
        .with_timezone(&chrono::Local)
        .format("%m-%d %H:%M")
        .to_string()
}

// ── Dense session list table rendering ──────────────────────────────────

const FULL_LIST_MIN_WIDTH: u16 = 110;
const OPERATIONAL_TABLE_MIN_WIDTH: u16 = 120;
const RELAY_MIN_WIDTH: u16 = 174;
const ACTIVITY_MIN_WIDTH: u16 = 228;
const RELAY_MIN_HEIGHT: u16 = 24;
const NAVIGATOR_MAX_WIDTH: u16 = 100;
const INSPECTOR_MAX_WIDTH: u16 = 72;
const ACTIVITY_MAX_WIDTH: u16 = 56;
const RELAY_GAP: u16 = 2;
const OPERATIONAL_TABLE_MAX_BODY_HEIGHT: u16 = 31;
const NAVIGATOR_MAX_BODY_HEIGHT: u16 = 30;
const EMBEDDED_INSPECTOR_MIN_WIDTH: u16 = 34;
const EMBEDDED_INSPECTOR_MIN_HEIGHT: u16 = 18;
const EMBEDDED_EXPANDED_INSPECTOR_MIN_HEIGHT: u16 = 44;
const EMBEDDED_COMPACT_INSPECTOR_HEIGHT: u16 = 7;
const EMBEDDED_NAVIGATOR_HEIGHT: u16 = 32;

/// Where the list is being rendered. The full browser may opt into the
/// navigator + inspector composition; a sufficiently tall embedded browser
/// uses a bounded operational table with selected-session details below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionListSurface {
    Full,
    Embedded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionListDensity {
    Full,
    Compact,
}

impl SessionListDensity {
    fn for_width(width: u16) -> Self {
        if width >= FULL_LIST_MIN_WIDTH {
            Self::Full
        } else {
            Self::Compact
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionListPresentation {
    Table(SessionListDensity),
    OperationalTable,
    Navigator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionBrowserMode {
    LegacyTable,
    OperationalTable,
    EmbeddedInspector,
    Relay,
    RelayWithActivity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SessionBrowserLayout {
    mode: SessionBrowserMode,
    selected_signal: Option<Rect>,
    navigator: Rect,
    tether: Option<Rect>,
    inspector: Option<Rect>,
    embedded_inspector: Option<Rect>,
    activity: Option<Rect>,
}

fn resolve_session_browser_layout(
    content: Rect,
    surface: SessionListSurface,
    active_zone: crate::types::SessionListZone,
) -> SessionBrowserLayout {
    let legacy = || SessionBrowserLayout {
        mode: SessionBrowserMode::LegacyTable,
        selected_signal: None,
        navigator: content,
        tether: None,
        inspector: None,
        embedded_inspector: None,
        activity: None,
    };
    if active_zone != crate::types::SessionListZone::Main {
        return legacy();
    }

    if surface == SessionListSurface::Embedded {
        if content.width < EMBEDDED_INSPECTOR_MIN_WIDTH
            || content.height < EMBEDDED_INSPECTOR_MIN_HEIGHT
        {
            return legacy();
        }
        let gap = 1;
        let navigator_height = if content.height >= EMBEDDED_EXPANDED_INSPECTOR_MIN_HEIGHT {
            EMBEDDED_NAVIGATOR_HEIGHT.min(content.height.saturating_sub(11))
        } else {
            content
                .height
                .saturating_sub(EMBEDDED_COMPACT_INSPECTOR_HEIGHT)
                .saturating_sub(gap)
        };
        return SessionBrowserLayout {
            mode: SessionBrowserMode::EmbeddedInspector,
            selected_signal: None,
            navigator: Rect::new(content.x, content.y, content.width, navigator_height),
            tether: None,
            inspector: None,
            embedded_inspector: Some(Rect::new(
                content.x,
                content
                    .y
                    .saturating_add(navigator_height)
                    .saturating_add(gap),
                content.width,
                content
                    .height
                    .saturating_sub(navigator_height)
                    .saturating_sub(gap),
            )),
            activity: None,
        };
    }

    if content.width < OPERATIONAL_TABLE_MIN_WIDTH {
        return legacy();
    }

    if content.height < RELAY_MIN_HEIGHT || content.width < RELAY_MIN_WIDTH {
        let width = content.width.saturating_sub(4).min(156);
        let left = content.width.saturating_sub(width) / 2;
        let signal_height = if content.height >= RELAY_MIN_HEIGHT {
            2
        } else {
            0
        };
        let signal_gap = u16::from(signal_height > 0);
        let selected_signal = (signal_height > 0).then(|| {
            Rect::new(
                content.x.saturating_add(left),
                content.y,
                width,
                signal_height.min(content.height),
            )
        });
        return SessionBrowserLayout {
            mode: SessionBrowserMode::OperationalTable,
            selected_signal,
            navigator: Rect::new(
                content.x.saturating_add(left),
                content
                    .y
                    .saturating_add(signal_height)
                    .saturating_add(signal_gap),
                width,
                content
                    .height
                    .saturating_sub(signal_height)
                    .saturating_sub(signal_gap),
            ),
            tether: None,
            inspector: None,
            embedded_inspector: None,
            activity: None,
        };
    }

    let (navigator_width, inspector_width, activity_width, mode) = if content.width < 200 {
        let delta = content.width.saturating_sub(RELAY_MIN_WIDTH);
        (
            84 + delta.saturating_mul(15) / 25,
            68 + delta.saturating_mul(3) / 25,
            0,
            SessionBrowserMode::Relay,
        )
    } else if content.width < ACTIVITY_MIN_WIDTH {
        (
            NAVIGATOR_MAX_WIDTH,
            INSPECTOR_MAX_WIDTH,
            0,
            SessionBrowserMode::Relay,
        )
    } else if content.width < 240 {
        (
            NAVIGATOR_MAX_WIDTH,
            INSPECTOR_MAX_WIDTH,
            content.width.saturating_sub(184),
            SessionBrowserMode::RelayWithActivity,
        )
    } else {
        (
            NAVIGATOR_MAX_WIDTH,
            INSPECTOR_MAX_WIDTH,
            ACTIVITY_MAX_WIDTH,
            SessionBrowserMode::RelayWithActivity,
        )
    };
    let composition_width = navigator_width
        .saturating_add(RELAY_GAP)
        .saturating_add(inspector_width)
        .saturating_add(if activity_width > 0 {
            RELAY_GAP.saturating_add(activity_width)
        } else {
            0
        });
    let left = content.width.saturating_sub(composition_width) / 2;
    let navigator = Rect::new(
        content.x.saturating_add(left),
        content.y,
        navigator_width,
        content.height,
    );
    let tether = Rect::new(
        navigator.x.saturating_add(navigator.width),
        content.y,
        RELAY_GAP,
        content.height,
    );
    let inspector = Rect::new(
        tether.x.saturating_add(tether.width),
        content.y,
        inspector_width,
        content.height,
    );
    let activity = (activity_width > 0).then(|| {
        Rect::new(
            inspector
                .x
                .saturating_add(inspector.width)
                .saturating_add(RELAY_GAP),
            content.y,
            activity_width,
            content.height,
        )
    });
    SessionBrowserLayout {
        mode,
        selected_signal: None,
        navigator,
        tether: Some(tether),
        inspector: Some(inspector),
        embedded_inspector: None,
        activity,
    }
}

fn list_surface_bg(settings: &crate::settings::UserSettings) -> Color {
    if theme::is_transparent_theme() {
        theme::glass_panel_bg()
    } else {
        crate::settings::text_area_bg_color(settings, theme::glass_panel_bg())
    }
}

pub(crate) fn detail_surface_bg(settings: &crate::settings::UserSettings) -> Color {
    if theme::is_transparent_theme() {
        theme::glass_panel_bg()
    } else {
        crate::settings::text_area_bg_color(settings, theme::base())
    }
}

fn short_display_id(id: impl std::fmt::Display) -> String {
    id.to_string().chars().take(8).collect()
}

fn truncate_detail_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    if max_chars <= 3 {
        return value.chars().take(max_chars).collect();
    }
    let mut out: String = value.chars().take(max_chars - 3).collect();
    out.push_str("...");
    out
}

fn recursive_dag_detail_text(context: &RecursiveDagSessionContext, width: u16) -> String {
    let mut parts = vec![
        format!(
            "{} {}",
            short_display_id(context.graph_id),
            context.graph_title
        ),
        format!("{:?}", context.graph_status),
        format!("{:?}", context.execution_mode),
        context.relation.to_string(),
    ];
    if let Some(status) = context.task_status {
        let task = context.task_title.as_deref().unwrap_or("task");
        parts.push(format!("task {:?}: {}", status, task));
    }
    if let Some(run_id) = context.run_id {
        let status = context
            .run_status
            .map(|status| format!("{:?}", status))
            .unwrap_or_else(|| "run".to_string());
        parts.push(format!("run {} {}", short_display_id(run_id), status));
    }
    if context.quarantined {
        parts.push("quarantined".to_string());
    }
    if context.malformed {
        parts.push("malformed".to_string());
    }
    if context.open_cancellation_count > 0 {
        parts.push(format!("cancel {}", context.open_cancellation_count));
    }
    if let Some(label) = context.recovery_label {
        parts.push(label.to_string());
    }
    if context.live_disabled {
        parts.push("LIVE off".to_string());
    }
    if context.warning_count > 0 {
        parts.push(format!("W{}", context.warning_count));
    }
    if context.error_count > 0 {
        parts.push(format!("E{}", context.error_count));
    }

    truncate_detail_chars(&parts.join(" | "), width as usize)
}

pub(crate) fn recursive_dag_detail_line(
    app: &App,
    session: &Session,
    width: u16,
) -> Option<Line<'static>> {
    let context = app
        .recursive_dag_browser_state()?
        .session_context(session)?;
    let tone = if context.error_count > 0 || context.malformed {
        theme::red()
    } else if context.warning_count > 0
        || context.quarantined
        || context.open_cancellation_count > 0
        || context.recovery_label.is_some()
    {
        theme::yellow()
    } else {
        theme::teal()
    };
    let label_width = 6usize;
    let text_width = (width as usize).saturating_sub(label_width);
    Some(Line::from(vec![
        Span::styled(
            "[dag] ",
            Style::default().fg(tone).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            recursive_dag_detail_text(&context, text_width as u16),
            Style::default().fg(theme::subtext0()),
        ),
    ]))
}

fn zone_empty_message(zone: crate::types::SessionListZone) -> &'static str {
    match zone {
        crate::types::SessionListZone::TaskRabbit => "No TaskRabbit sessions",
        crate::types::SessionListZone::Main => "No sessions. Press 'n' to create one.",
        crate::types::SessionListZone::Archive => "No archived sessions",
        crate::types::SessionListZone::Jobs => "No scheduled job sessions",
    }
}

fn compute_table_geometry(
    order: &[uuid::Uuid],
    sessions: &std::collections::HashMap<uuid::Uuid, crate::types::SessionState>,
    labels: &[rsi_common::types::SessionLabel],
    _selected_index: usize,
    presentation: SessionListPresentation,
    content_width: u16,
    zone: &mut crate::types::ZoneRenderState,
    sort_order: crate::app::SortOrder,
    active_zone: crate::types::SessionListZone,
    focus_index: &HashMap<uuid::Uuid, SessionFocusEntry>,
    manager_ids: &HashSet<uuid::Uuid>,
    descent_head: Option<uuid::Uuid>,
    search_active: bool,
) {
    zone.card_heights.clear();
    zone.card_offsets.clear();
    zone.label_headers.clear();

    let mut offset = 0usize;
    let mut prev_label_id: Option<Option<uuid::Uuid>> = None;
    let by_label = sort_order == crate::app::SortOrder::ByLabel;
    let focus_groups = active_zone == crate::types::SessionListZone::Main && !by_label;
    let mut previous_focus_group: Option<SessionFocusGroup> = None;
    let mut in_managers = false;

    for (idx, id) in order.iter().enumerate() {
        if by_label {
            let current_label_id = sessions.get(id).and_then(|s| s.session.group_id);
            let is_new_label = match prev_label_id {
                None => true,
                Some(prev) => prev != current_label_id,
            };
            if is_new_label {
                let (header_name, header_color) = match current_label_id {
                    Some(gid) => {
                        let label = labels.iter().find(|g| g.id == gid);
                        (
                            label
                                .map(|g| g.name.clone())
                                .unwrap_or_else(|| "Unknown".to_string()),
                            label
                                .map(|g| g.color.clone())
                                .unwrap_or_else(|| "#cba6f7".to_string()),
                        )
                    }
                    None => ("Unlabeled".to_string(), "#6c7086".to_string()),
                };
                let count = order[idx..]
                    .iter()
                    .take_while(|candidate| {
                        sessions
                            .get(candidate)
                            .and_then(|state| state.session.group_id)
                            == current_label_id
                    })
                    .count();
                zone.label_headers
                    .push((idx, header_name, header_color, count));
                offset += 1;
            }
            prev_label_id = Some(current_label_id);
        } else if focus_groups {
            let operational = matches!(
                presentation,
                SessionListPresentation::OperationalTable | SessionListPresentation::Navigator
            );
            if search_active && idx == 0 {
                zone.label_headers
                    .push((idx, "RESULTS".to_string(), String::new(), order.len()));
                offset += if operational { 2 } else { 1 };
            } else if !search_active && descent_head.is_none() && manager_ids.contains(id) {
                if !in_managers {
                    let count = order[idx..]
                        .iter()
                        .take_while(|candidate| manager_ids.contains(candidate))
                        .count();
                    zone.label_headers
                        .push((idx, "MANAGERS".to_string(), String::new(), count));
                    offset += if operational { 2 } else { 1 };
                    in_managers = true;
                }
            } else if !search_active
                && sessions
                    .get(id)
                    .is_some_and(|state| state.session.parent_id == descent_head)
            {
                let group = focus_index
                    .get(id)
                    .map(|entry| entry.group)
                    .unwrap_or(SessionFocusGroup::Recent);
                if previous_focus_group != Some(group) {
                    let count = order[idx..]
                        .iter()
                        .take_while(|candidate| {
                            sessions
                                .get(candidate)
                                .is_some_and(|state| state.session.parent_id == descent_head)
                                && focus_index
                                    .get(candidate)
                                    .map(|entry| entry.group)
                                    .unwrap_or(SessionFocusGroup::Recent)
                                    == group
                        })
                        .count();
                    zone.label_headers
                        .push((idx, group.label().to_string(), String::new(), count));
                    // Navigator sections are separated by one blank row. The
                    // first focus section gets one too when the MANAGERS
                    // section sits above it, matching every other boundary.
                    offset += match presentation {
                        SessionListPresentation::OperationalTable => 2,
                        SessionListPresentation::Navigator
                            if previous_focus_group.is_some() || in_managers =>
                        {
                            2
                        }
                        _ => 1,
                    };
                    previous_focus_group = Some(group);
                }
            }
        }

        zone.card_offsets.push(offset);
        // Slice 3 has one physical navigator row in every mode. Keep the
        // persisted expansion state, but do not let it allocate row height.
        let height = 1;
        zone.card_heights.push(height);
        offset += height;
    }

    zone.total_content_height = offset;
    zone.last_render_width = content_width;
    zone.last_session_count = order.len();
}

fn ensure_selected_row_visible(
    selected_index: usize,
    viewport_h: usize,
    zone: &mut crate::types::ZoneRenderState,
) {
    if viewport_h == 0 {
        zone.scroll_offset = 0;
        return;
    }

    let mut scroll = zone.scroll_offset;
    if selected_index < zone.card_offsets.len() {
        let row_top = zone.card_offsets[selected_index];
        let row_bottom = row_top + zone.card_heights[selected_index].max(1);
        if row_bottom > scroll + viewport_h {
            scroll = row_bottom.saturating_sub(viewport_h);
        }
        if row_top < scroll {
            scroll = row_top;
        }
    }
    let max_scroll = zone.total_content_height.saturating_sub(viewport_h);
    zone.scroll_offset = scroll.min(max_scroll);
}

fn column_header_height(_presentation: SessionListPresentation) -> u16 {
    1
}

fn render_scope_header(
    frame: &mut Frame,
    area: Rect,
    active_zone: crate::types::SessionListZone,
    count: usize,
    project_name: &str,
    descent_path: &[uuid::Uuid],
    sessions: &HashMap<uuid::Uuid, SessionState>,
    search_query: &str,
    bg: Color,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let mut left = vec![
        Span::styled(" ", Style::default().bg(bg)),
        Span::styled(
            project_name.to_string(),
            Style::default()
                .fg(theme::green())
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" / ", Style::default().fg(theme::overlay1()).bg(bg)),
        Span::styled(
            title_case_zone(active_zone),
            Style::default()
                .fg(zone_accent(active_zone))
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    for id in descent_path {
        left.push(Span::styled(
            " / ",
            Style::default().fg(theme::overlay1()).bg(bg),
        ));
        let segment = scope_breadcrumb_title(*id, sessions);
        left.push(Span::styled(
            truncate_chars(&segment, 22),
            Style::default().fg(theme::mauve()).bg(bg),
        ));
    }
    if !search_query.is_empty() {
        left.push(Span::styled(
            " / ",
            Style::default().fg(theme::overlay1()).bg(bg),
        ));
        left.push(Span::styled(
            format!("“{}”", truncate_chars(search_query, 24)),
            Style::default().fg(theme::blue()).bg(bg),
        ));
    }

    let right = Line::from(vec![
        Span::styled(
            count.to_string(),
            Style::default()
                .fg(theme::text())
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if count == 1 {
                " session "
            } else {
                " sessions "
            },
            Style::default().fg(theme::dim_metadata()).bg(bg),
        ),
    ]);
    let right_w = right.width() as u16;
    let chunks = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(right_w.min(area.width)),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(left)).style(Style::default().bg(bg)),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(right)
            .style(Style::default().bg(bg))
            .alignment(ratatui::layout::Alignment::Right),
        chunks[1],
    );
}

fn scope_breadcrumb_title(
    session_id: uuid::Uuid,
    sessions: &HashMap<uuid::Uuid, SessionState>,
) -> String {
    sessions
        .get(&session_id)
        .map(|state| {
            crate::types::resolve_session_display_identity(&state.session, sessions).effective_title
        })
        .unwrap_or_else(|| "Unknown".to_string())
}

fn title_case_zone(zone: crate::types::SessionListZone) -> &'static str {
    match zone {
        crate::types::SessionListZone::Main => "Sessions",
        crate::types::SessionListZone::TaskRabbit => "TaskRabbit",
        crate::types::SessionListZone::Archive => "Archive",
        crate::types::SessionListZone::Jobs => "Jobs",
    }
}

fn zone_accent(zone: crate::types::SessionListZone) -> Color {
    match zone {
        crate::types::SessionListZone::Main => theme::accent(),
        crate::types::SessionListZone::TaskRabbit => theme::teal(),
        crate::types::SessionListZone::Archive => theme::peach(),
        crate::types::SessionListZone::Jobs => theme::blue(),
    }
}

fn operational_ordinal_width(
    order: &[uuid::Uuid],
    sessions: &HashMap<uuid::Uuid, SessionState>,
) -> usize {
    let order_width = order.len().max(1).to_string().len();
    let semantic_width = order
        .iter()
        .filter_map(|id| sessions.get(id))
        .filter_map(|state| {
            crate::types::resolve_session_display_identity(&state.session, sessions)
                .effective_epic_ordinal
        })
        .map(|ordinal| ordinal.to_string().len())
        .max()
        .unwrap_or(1);
    order_width.max(semantic_width).max(2)
}

fn render_navigator_header(
    frame: &mut Frame,
    area: Rect,
    bg: Color,
    ordinal_width: usize,
    preset: crate::types::NavigatorPreset,
    overrides: Option<&[crate::types::NavigatorOptionalColumn]>,
) {
    if area.height == 0 {
        return;
    }
    let layout = navigator_layout::resolve(area.width as usize, ordinal_width, preset, overrides);
    let mut spans = vec![fixed_span("", 1, header_style(bg))];
    if layout.leading_gap > 0 {
        spans.push(fixed_span("", layout.leading_gap, header_style(bg)));
    }
    let column_count = layout.columns.len();
    for (index, cell) in layout.columns.iter().enumerate() {
        let label = match cell.column {
            navigator_layout::NavigatorColumn::Ordinal => "#",
            navigator_layout::NavigatorColumn::Pin => "∞",
            navigator_layout::NavigatorColumn::Status => "S",
            navigator_layout::NavigatorColumn::Attention => "!",
            navigator_layout::NavigatorColumn::Function => "FUNCTION",
            navigator_layout::NavigatorColumn::Context => glyphs::CONTEXT,
            navigator_layout::NavigatorColumn::Turns => glyphs::TURNS,
            navigator_layout::NavigatorColumn::Age => "AGE",
            // The provider glyph sits directly left of the model; one label
            // spans both.
            navigator_layout::NavigatorColumn::Provider => "",
            navigator_layout::NavigatorColumn::Model => "MODEL",
            navigator_layout::NavigatorColumn::Effort => glyphs::EFFORT,
            navigator_layout::NavigatorColumn::Retry => "RETRY",
            navigator_layout::NavigatorColumn::Cost => "COST",
            navigator_layout::NavigatorColumn::Work => "WORK",
            navigator_layout::NavigatorColumn::Rotation => "ROT",
            navigator_layout::NavigatorColumn::Project => "PROJECT",
            navigator_layout::NavigatorColumn::Created => "CREATED",
        };
        // Symbol headers sit over their values, so they follow the column's
        // own alignment; word headers keep reading left to right.
        let align = if label.is_ascii() {
            navigator_layout::Alignment::Left
        } else {
            cell.align
        };
        spans.push(Span::styled(
            navigator_layout::align_cells(label, cell.width, align),
            header_style(bg),
        ));
        if index + 1 < column_count {
            let next = &layout.columns[index + 1];
            spans.push(fixed_span(
                "",
                next.start - cell.start - cell.width,
                header_style(bg),
            ));
        }
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(bg)),
        area,
    );
}

fn header_style(bg: Color) -> Style {
    Style::default()
        .fg(theme::table_header_text())
        .bg(bg)
        .add_modifier(Modifier::BOLD)
}

fn visible_session_range(
    zone: &crate::types::ZoneRenderState,
    viewport_h: usize,
    order_len: usize,
) -> (usize, usize) {
    if order_len == 0 || viewport_h == 0 || zone.card_offsets.is_empty() {
        return (0, 0);
    }
    let start = zone.scroll_offset;
    let end = start.saturating_add(viewport_h);
    let first = zone
        .card_offsets
        .iter()
        .zip(zone.card_heights.iter())
        .position(|(&off, &height)| off + height > start)
        .unwrap_or(0);
    let last = zone
        .card_offsets
        .iter()
        .position(|&off| off >= end)
        .map(|idx| idx.saturating_sub(1))
        .unwrap_or(order_len.saturating_sub(1));
    (first + 1, last.max(first) + 1)
}

fn render_dense_rows(
    frame: &mut Frame,
    area: Rect,
    order: &[uuid::Uuid],
    sessions: &std::collections::HashMap<uuid::Uuid, crate::types::SessionState>,
    projects: &[rsi_common::types::Project],
    settings: &crate::settings::UserSettings,
    selected_index: usize,
    is_focused: bool,
    zone_render: &crate::types::ZoneRenderState,
    accent_color: Color,
    presentation: SessionListPresentation,
    operational_ordinal_width: usize,
    viewed_session_id: Option<uuid::Uuid>,
    focus_index: &HashMap<uuid::Uuid, SessionFocusEntry>,
    manager_ids: &HashSet<uuid::Uuid>,
    _descent_head: Option<uuid::Uuid>,
    bg: Color,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let viewport_h = area.height as usize;
    let visible_start = zone_render.scroll_offset;
    let visible_end = visible_start + viewport_h;

    let header_map: std::collections::HashMap<usize, (String, String, usize)> = zone_render
        .label_headers
        .iter()
        .map(|(idx, name, color, count)| (*idx, (name.clone(), color.clone(), *count)))
        .collect();

    for (idx, id) in order.iter().enumerate() {
        if idx >= zone_render.card_offsets.len() {
            break;
        }

        if let Some((name, color_hex, count)) = header_map.get(&idx) {
            let header_offset = zone_render.card_offsets[idx].saturating_sub(1);
            if header_offset >= visible_start && header_offset < visible_end {
                let y = area.y + (header_offset - visible_start) as u16;
                let color = if color_hex.is_empty() {
                    focus_group_header_color(name)
                } else {
                    super::parse_hex_color(color_hex).unwrap_or_else(theme::section_label_text)
                };
                let displayed_count = if matches!(
                    presentation,
                    SessionListPresentation::OperationalTable | SessionListPresentation::Navigator
                ) {
                    visible_operational_section_count(
                        zone_render,
                        idx,
                        visible_start,
                        visible_end,
                        order.len(),
                    )
                } else {
                    *count
                };
                render_group_header(
                    frame,
                    Rect::new(area.x, y, area.width, 1),
                    name,
                    displayed_count,
                    color,
                    bg,
                    matches!(
                        presentation,
                        SessionListPresentation::OperationalTable
                            | SessionListPresentation::Navigator
                    ),
                );
            }
        }

        let row_offset = zone_render.card_offsets[idx];
        let row_height = zone_render.card_heights[idx];
        if row_offset + row_height <= visible_start {
            continue;
        }
        if row_offset >= visible_end {
            break;
        }

        let Some(state) = sessions.get(id) else {
            continue;
        };
        let mut row = crate::types::row::compute_session_row_for_state_with_focus(
            state,
            sessions,
            projects,
            settings,
            focus_index.get(id),
        );
        row.is_manager = manager_ids.contains(id);
        let is_selected = idx == selected_index;
        let is_viewed = Some(*id) == viewed_session_id;
        let y = area.y + row_offset.saturating_sub(visible_start) as u16;
        let visible_height = (area.y + area.height).saturating_sub(y);
        let row_rect = Rect::new(
            area.x,
            y,
            area.width,
            (row_height as u16).min(visible_height),
        );
        render_navigator_row(
            frame,
            row_rect,
            &row,
            Some(&state.session),
            settings,
            operational_ordinal_width,
            is_selected,
            is_viewed,
            is_focused,
            accent_color,
            bg,
        );
    }
}

fn container_function_parts(
    row: &crate::types::row::SessionRowViewModel,
    width: usize,
) -> (String, Option<String>) {
    let full_title_width = navigator_layout::display_width(&row.display_title);
    let minimum_title_width = full_title_width.min(6).min(width);
    let Some(counts) = row
        .container_counts
        .filter(|counts| counts.running_agents > 0)
    else {
        return (row.display_title.clone(), None);
    };
    let agents = counts.running_agents;
    let agent_word = if agents == 1 { "agent" } else { "agents" };
    let agent_long = format!("{agents} {agent_word}");
    let agent_short = format!("{agents}A");
    let (long, compact) = if row.session_kind == SessionKind::Group {
        let epics = counts.running_epics;
        let epic_word = if epics == 1 { "epic" } else { "epics" };
        (
            format!("{epics} {epic_word} · {agent_long}"),
            format!("{epics}E·{agent_short}"),
        )
    } else {
        (agent_long, agent_short.clone())
    };
    let fits = |title_width: usize, suffix: &str| {
        title_width + 2 + navigator_layout::display_width(suffix) <= width
    };
    let suffix = if fits(full_title_width, &long) {
        Some(long)
    } else if fits(full_title_width, &compact) {
        Some(compact)
    } else if fits(full_title_width, &agent_short) {
        Some(agent_short)
    } else if fits(minimum_title_width, &compact) {
        Some(compact)
    } else if fits(minimum_title_width, &agent_short) {
        Some(agent_short)
    } else {
        None
    };
    let Some(suffix) = suffix else {
        return (
            navigator_layout::truncate_cells_with_ellipsis(&row.display_title, width),
            None,
        );
    };
    let title_width = width - 2 - navigator_layout::display_width(&suffix);
    let title = navigator_layout::truncate_cells_with_ellipsis(&row.display_title, title_width);
    (title, Some(format!("  {suffix}")))
}

fn render_navigator_row(
    frame: &mut Frame,
    area: Rect,
    row: &crate::types::row::SessionRowViewModel,
    session: Option<&Session>,
    settings: &crate::settings::UserSettings,
    ordinal_width: usize,
    is_selected: bool,
    is_viewed: bool,
    is_focused: bool,
    accent_color: Color,
    bg: Color,
) {
    if area.height == 0 {
        return;
    }
    let row_bg = if is_selected {
        theme::selected_row_bg()
    } else if is_viewed {
        theme::viewed_session_row_bg()
    } else {
        bg
    };
    fill_area(frame, area, row_bg);
    let rail = if is_selected {
        if is_focused { "▌" } else { "│" }
    } else if is_viewed {
        "│"
    } else {
        " "
    };
    let rail_style = Style::default()
        .fg(if is_selected {
            theme::active_row_rail()
        } else if is_viewed {
            theme::viewed_session_row_rail()
        } else {
            theme::overlay0()
        })
        .bg(row_bg)
        .add_modifier(Modifier::BOLD);
    let layout = navigator_layout::resolve(
        area.width as usize,
        ordinal_width,
        settings.navigator_preset,
        settings.navigator_optional_columns.as_deref(),
    );
    let mut spans = vec![fixed_span(rail, 1, rail_style)];
    if layout.leading_gap > 0 {
        spans.push(fixed_span(
            "",
            layout.leading_gap,
            Style::default().bg(row_bg),
        ));
    }
    let column_count = layout.columns.len();
    for (index, cell) in layout.columns.iter().enumerate() {
        if cell.column == navigator_layout::NavigatorColumn::Function {
            spans.extend(function_cell_spans(
                row,
                cell.width,
                title_style(row, is_selected, row_bg, accent_color),
                row_bg,
            ));
            if index + 1 < column_count {
                let next = &layout.columns[index + 1];
                spans.push(fixed_span(
                    "",
                    next.start - cell.start - cell.width,
                    Style::default().bg(row_bg),
                ));
            }
            continue;
        }
        let (text, style) = match cell.column {
            navigator_layout::NavigatorColumn::Ordinal => {
                if row.is_manager {
                    (
                        "M".to_string(),
                        Style::default()
                            .fg(theme::accent())
                            .bg(row_bg)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    (
                        row.effective_epic_ordinal
                            .map(|value| value.to_string())
                            .unwrap_or_default(),
                        Style::default().fg(theme::dim_metadata()).bg(row_bg),
                    )
                }
            }
            navigator_layout::NavigatorColumn::Pin => (
                (if row.is_pinned { "◆" } else { " " }).to_string(),
                Style::default()
                    .fg(if row.is_pinned {
                        theme::pin()
                    } else {
                        theme::overlay0()
                    })
                    .bg(row_bg),
            ),
            navigator_layout::NavigatorColumn::Status => (
                row.status_icon.clone(),
                Style::default().fg(row.status_color).bg(row_bg),
            ),
            navigator_layout::NavigatorColumn::Attention => (
                row.attention_glyph.to_string(),
                Style::default()
                    .fg(attention_glyph_color(row.attention_glyph))
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            navigator_layout::NavigatorColumn::Function => unreachable!("rendered above"),
            navigator_layout::NavigatorColumn::Context => (
                context_text(&row.context_pct),
                Style::default()
                    .fg(context_color(&row.context_pct))
                    .bg(row_bg),
            ),
            navigator_layout::NavigatorColumn::Turns => (
                row.turns_text.clone().unwrap_or_default(),
                Style::default().fg(theme::dim_metadata()).bg(row_bg),
            ),
            navigator_layout::NavigatorColumn::Age => (
                operational_age(&time_text(&row.time)),
                Style::default().fg(theme::dim_metadata()).bg(row_bg),
            ),
            navigator_layout::NavigatorColumn::Provider => {
                let provider = session
                    .filter(|session| rsi_common::is_leaf_kind(session.session_kind))
                    .map(|session| session.provider);
                (
                    provider
                        .map(|provider| glyphs::provider_glyph(provider).to_string())
                        .unwrap_or_default(),
                    Style::default()
                        .fg(provider.map_or_else(theme::subtext1, glyphs::provider_color))
                        .bg(row_bg)
                        .add_modifier(Modifier::BOLD),
                )
            }
            navigator_layout::NavigatorColumn::Model => (
                session
                    .filter(|session| rsi_common::is_leaf_kind(session.session_kind))
                    .and_then(|session| session.model.as_deref())
                    .map(|model| glyphs::list_model_label(model).to_string())
                    .unwrap_or_default(),
                Style::default().fg(theme::subtext1()).bg(row_bg),
            ),
            navigator_layout::NavigatorColumn::Effort => {
                let effort = session
                    .filter(|session| rsi_common::is_leaf_kind(session.session_kind))
                    .and_then(|session| {
                        glyphs::effort_glyph(session.effort.as_deref(), session.model.as_deref())
                    });
                (
                    effort
                        .map(|(glyph, _)| glyph.to_string())
                        .unwrap_or_default(),
                    Style::default()
                        .fg(effort.map_or_else(theme::subtext1, |(_, color)| color))
                        .bg(row_bg),
                )
            }
            navigator_layout::NavigatorColumn::Retry => (
                row.retry_info
                    .map(|(attempt, max)| format!("{attempt}/{max}"))
                    .unwrap_or_default(),
                Style::default().fg(theme::dim_metadata()).bg(row_bg),
            ),
            navigator_layout::NavigatorColumn::Cost => (
                cost_text(&row.cost),
                Style::default().fg(theme::dim_metadata()).bg(row_bg),
            ),
            navigator_layout::NavigatorColumn::Work => (
                row.work_time_text.clone().unwrap_or_default(),
                Style::default().fg(theme::dim_metadata()).bg(row_bg),
            ),
            navigator_layout::NavigatorColumn::Rotation => (
                row.rotation_suffix.trim().to_string(),
                Style::default().fg(theme::dim_metadata()).bg(row_bg),
            ),
            navigator_layout::NavigatorColumn::Project => (
                project_text(&row.project_name),
                Style::default().fg(theme::dim_metadata()).bg(row_bg),
            ),
            navigator_layout::NavigatorColumn::Created => (
                row.created_text.clone().unwrap_or_default(),
                Style::default().fg(theme::dim_metadata()).bg(row_bg),
            ),
        };
        let text = navigator_layout::align_cells(&text, cell.width, cell.align);
        spans.push(Span::styled(text, style));
        if index + 1 < column_count {
            let next = &layout.columns[index + 1];
            spans.push(fixed_span(
                "",
                next.start - cell.start - cell.width,
                Style::default().bg(row_bg),
            ));
        }
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(row_bg)),
        Rect::new(area.x, area.y, area.width, 1),
    );
}

fn visible_operational_section_count(
    zone: &crate::types::ZoneRenderState,
    section_start: usize,
    visible_start: usize,
    visible_end: usize,
    order_len: usize,
) -> usize {
    let section_end = zone
        .label_headers
        .iter()
        .map(|(idx, ..)| *idx)
        .find(|idx| *idx > section_start)
        .unwrap_or(order_len);
    (section_start..section_end)
        .filter(|idx| {
            let Some((&offset, &height)) =
                zone.card_offsets.get(*idx).zip(zone.card_heights.get(*idx))
            else {
                return false;
            };
            offset < visible_end && offset.saturating_add(height) > visible_start
        })
        .count()
}

fn render_group_header(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    count: usize,
    color: Color,
    bg: Color,
    operational: bool,
) {
    let available = area.width as usize;
    if !operational {
        let label = truncate_chars(&format!(" {name} "), available);
        let pad = available.saturating_sub(label.chars().count());
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    label,
                    Style::default()
                        .fg(color)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "─".repeat(pad),
                    Style::default()
                        .fg(color)
                        .bg(bg)
                        .add_modifier(Modifier::DIM),
                ),
            ]))
            .style(Style::default().bg(bg)),
            area,
        );
        return;
    }
    // Section glyphs repeat the lifecycle vocabulary used by the activity
    // pane's FLOW counts, so a section and its count read as the same thing.
    let glyph = focus_group_header_glyph(name);
    let prefix = if glyph.is_empty() {
        "  ".to_string()
    } else {
        format!("  {glyph} ")
    };
    let label = truncate_chars(name, available.saturating_sub(prefix.chars().count()));
    let count_text = if name == "MANAGERS" {
        format!(" {count}  ")
    } else {
        format!("  {count}  ")
    };
    let used = prefix.chars().count() + label.chars().count() + count_text.chars().count();
    let rule_width = available.saturating_sub(used).min(14);
    let line = Line::from(vec![
        Span::styled(
            prefix,
            Style::default()
                .fg(color)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            label,
            Style::default()
                .fg(theme::subtext1())
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            count_text,
            Style::default()
                .fg(color)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "─".repeat(rule_width),
            Style::default()
                .fg(theme::browser_decorative_separator())
                .bg(bg),
        ),
    ]);
    frame.render_widget(Paragraph::new(line).style(Style::default().bg(bg)), area);
}

fn focus_group_header_glyph(name: &str) -> &'static str {
    match name {
        "MANAGERS" => glyphs::MANAGERS,
        "NEEDS YOU" => glyphs::NEEDS_YOU,
        "IN FLIGHT" => "●",
        "RECENT" => "✓",
        "QUIET" => "·",
        _ => "",
    }
}

fn focus_group_header_color(name: &str) -> Color {
    match name {
        "NEEDS YOU" => theme::status_waiting(),
        "IN FLIGHT" => theme::status_running(),
        "RECENT" => theme::status_completed(),
        "QUIET" => theme::dim_metadata(),
        _ => theme::section_label_text(),
    }
}

/// Compact relative age for fixed narrow cells: `5m ago` → `5m`, and
/// `just now` → `now` so it is never truncated to `jus~`.
fn operational_age(age: &str) -> String {
    if age == "just now" {
        return "now".to_string();
    }
    age.strip_suffix(" ago").unwrap_or(age).to_string()
}

fn fixed_span(text: &str, width: usize, style: Style) -> Span<'static> {
    Span::styled(navigator_layout::span_cells(text, width), style)
}

fn truncate_chars(text: &str, max: usize) -> String {
    navigator_layout::truncate_cells(text, max)
}

/// Builds the stacked-prefix title text. `include_sandbox_glyph` is `false`
/// for the Wide/Standard tiers (sandbox has its own dedicated column there —
/// PI-2/PI-3) and `true` for the Compact row (no room for a dedicated
/// column at that density — the glyph folds into the title prefix instead,
/// replacing the old "S " 2-char text prefix with a 1-char glyph "⊡ ",
/// mitigating F-010's prefix-stacking pressure).

/// The pin remains a text glyph for monochrome terminals, but owns its semantic
/// color independently of the title so a Pin-role override is visibly live.

fn title_style(
    row: &crate::types::row::SessionRowViewModel,
    is_selected: bool,
    bg: Color,
    accent_color: Color,
) -> Style {
    let fg = if is_selected {
        theme::text()
    } else {
        row.heat_color
    };
    let mut style = Style::default().fg(fg).bg(bg);
    if is_selected || matches!(row.status, SessionStatus::Running | SessionStatus::Starting) {
        style = style.add_modifier(Modifier::BOLD);
    }
    if row.is_pinned {
        style = style.add_modifier(Modifier::BOLD); // idempotent with the above; pinned = "don't lose track"
    }
    if row.is_testing_needed {
        style = style.add_modifier(Modifier::ITALIC); // "waiting on a human"
    }
    if row.status == SessionStatus::Interrupted {
        style = style.add_modifier(Modifier::DIM);
    }
    if row.is_new_message {
        style = style.add_modifier(Modifier::BOLD); // brightness/bold per locked color budget
    }
    if row.is_stalled {
        style = style.fg(theme::warning_status());
    }
    if row.is_pending_archive {
        style = style.fg(accent_color).add_modifier(Modifier::DIM);
    }
    style
}

/// Navigator FUNCTION cell: a leaf's `Role: subject` title renders as a
/// colored two-letter role code plus the subject, returning the ~10 cells the
/// role word and colon used to cost. Every other title renders unchanged.
fn function_cell_spans(
    row: &crate::types::row::SessionRowViewModel,
    width: usize,
    title_style: Style,
    row_bg: Color,
) -> Vec<Span<'static>> {
    const BADGE_CELLS: usize = 3;
    // Group/Epic rows with running work append a dim running-count suffix
    // after their OWN title; the suffix degrades before the title does.
    if let (title, Some(suffix)) = container_function_parts(row, width) {
        let used =
            navigator_layout::display_width(&title) + navigator_layout::display_width(&suffix);
        return vec![
            Span::styled(title, title_style),
            Span::styled(
                suffix,
                Style::default().fg(theme::dim_metadata()).bg(row_bg),
            ),
            fixed_span("", width.saturating_sub(used), Style::default().bg(row_bg)),
        ];
    }
    match crate::types::split_role_title(row.session_kind, &row.display_title) {
        Some(role) if width > BADGE_CELLS + 1 => {
            let mut badge_style = Style::default()
                .fg(glyphs::role_color(role.role))
                .bg(row_bg)
                .add_modifier(Modifier::BOLD);
            if row.status == SessionStatus::Interrupted {
                badge_style = badge_style.add_modifier(Modifier::DIM);
            }
            vec![
                Span::styled(
                    navigator_layout::span_cells(&glyphs::role_code(role.role), 2),
                    badge_style,
                ),
                Span::styled(" ", Style::default().bg(row_bg)),
                Span::styled(
                    navigator_layout::span_cells(role.subject, width - BADGE_CELLS),
                    title_style,
                ),
            ]
        }
        _ => vec![Span::styled(
            navigator_layout::span_cells(&row.display_title, width),
            title_style,
        )],
    }
}

fn attention_glyph_color(glyph: &str) -> Color {
    match glyph {
        glyphs::NEEDS_YOU => theme::warning_status(),
        glyphs::RETRY => theme::yellow(),
        glyphs::STALLED => theme::status_stalled(),
        glyphs::UNREAD => theme::accent(),
        _ => theme::warning_status(),
    }
}

/// The recorded effort and, when the model's ladder knows it, its
/// `(filled, total)` bar counts. Never invents a default effort.
fn actual_effort_parts(
    effort: Option<&str>,
    model: Option<&str>,
) -> Option<(String, Option<(usize, usize)>)> {
    let effort = effort?;
    let ladder = model.and_then(|model| {
        rsi_common::model_utils::known_codex_effort_ladder(model).or_else(|| {
            rsi_common::model_utils::parse_model_version(model)
                .map(|_| rsi_common::model_utils::effort_ladder(model))
        })
    });
    let bars = ladder.and_then(|ladder| {
        ladder
            .iter()
            .position(|candidate| *candidate == effort)
            .map(|index| (index + 1, ladder.len()))
    });
    Some((effort.to_string(), bars))
}

#[cfg(test)]
fn actual_effort_indicator(effort: Option<&str>, model: Option<&str>) -> Option<String> {
    let (effort, bars) = actual_effort_parts(effort, model)?;
    Some(match bars {
        Some((filled, total)) => format!(
            "{effort}  {}{}",
            "▮".repeat(filled),
            "▯".repeat(total.saturating_sub(filled))
        ),
        None => effort,
    })
}

fn project_text(row: &crate::types::row::ProjectCell) -> String {
    match row {
        crate::types::row::ProjectCell::Named(name) => name.clone(),
        crate::types::row::ProjectCell::Unassigned => String::new(),
    }
}

fn context_text(row: &crate::types::row::ContextCell) -> String {
    match row {
        crate::types::row::ContextCell::Display { pct: Some(pct), .. } => format!("{pct:.0}%"),
        // Unknown fill is a dim `◌`, never a false `0%`; the provenance behind
        // it (`—?·R`, `—?·L`, …) stays in the inspector's context line.
        crate::types::row::ContextCell::Display { pct: None, .. } => {
            glyphs::CONTEXT_UNKNOWN.to_string()
        }
        crate::types::row::ContextCell::Missing => String::new(),
    }
}

fn context_color(row: &crate::types::row::ContextCell) -> Color {
    match row {
        crate::types::row::ContextCell::Display { pct: Some(pct), .. } => {
            theme::context_border_color(*pct)
        }
        crate::types::row::ContextCell::Display { pct: None, .. } => theme::dim_metadata(),
        crate::types::row::ContextCell::Missing => theme::overlay0(),
    }
}

fn cost_text(row: &crate::types::row::CostCell) -> String {
    match row {
        crate::types::row::CostCell::Amount(amount) => amount.clone(),
        crate::types::row::CostCell::BelowThreshold => "<.01".to_string(),
        crate::types::row::CostCell::Missing => String::new(),
    }
}

fn time_text(row: &crate::types::row::TimeCell) -> String {
    match row {
        crate::types::row::TimeCell::Value { text, .. } => text.clone(),
        crate::types::row::TimeCell::Missing => "—".to_string(),
    }
}

fn render_session_inspector(
    frame: &mut Frame,
    area: Rect,
    inspector: Option<&SessionInspectorViewModel>,
    source_session: Option<&Session>,
    bg: Color,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(
            Style::default()
                .fg(theme::active_row_rail())
                .add_modifier(Modifier::BOLD),
        )
        .padding(Padding::new(2, 1, 1, 0))
        .style(Style::default().bg(bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(inspector) = inspector else {
        frame.render_widget(
            Paragraph::new("No session selected")
                .style(Style::default().fg(theme::dim_metadata()).bg(bg)),
            inner,
        );
        return;
    };

    let width = inner.width as usize;
    let mut lines = vec![
        inspector_header_line(inspector, width, bg),
        Line::from(Span::styled(
            "─".repeat(width),
            Style::default()
                .fg(theme::browser_decorative_separator())
                .bg(bg),
        )),
    ];
    lines.extend(inspector_title_lines(inspector, bg));
    if let Some(chips) = inspector_signal_line(&inspector.signals, bg) {
        lines.push(chips);
    }
    lines.push(Line::default());

    render_inspector_body(&mut lines, &inspector.body, width, bg);
    render_inspector_facts(&mut lines, inspector, source_session, width, bg);

    // Blocks are pre-wrapped so glyph gutters and quote rails survive
    // wrapping; the paragraph only wraps lines that are still too long.
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(bg))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

/// `01  ● running                  Rsi › Root`
fn inspector_header_line(
    inspector: &SessionInspectorViewModel,
    width: usize,
    bg: Color,
) -> Line<'static> {
    let status_color = status_color(inspector.status);
    let ordinal = format!("{:02}", inspector.ordinal);
    let status = format!(
        "{} {}",
        crate::types::row::navigator_lifecycle_icon(inspector.status),
        glyphs::status_word(inspector.status)
    );
    let used =
        navigator_layout::display_width(&ordinal) + 2 + navigator_layout::display_width(&status);
    let location = truncate_chars(
        &inspector_location_text(&inspector.location),
        width.saturating_sub(used + 2),
    );
    let gap = width.saturating_sub(used + navigator_layout::display_width(&location));
    Line::from(vec![
        Span::styled(
            ordinal,
            Style::default()
                .fg(theme::active_row_rail())
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default().bg(bg)),
        Span::styled(
            status,
            Style::default()
                .fg(status_color)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ".repeat(gap), Style::default().bg(bg)),
        Span::styled(location, Style::default().fg(theme::dim_metadata()).bg(bg)),
    ])
}

fn inspector_location_text(location: &str) -> String {
    location.replace(" / ", &format!(" {} ", glyphs::LOCATION))
}

/// Title (role code + subject) and a dim identity line (full role word and
/// session kind), so the role code is always learnable from the inspector.
fn inspector_title_lines(inspector: &SessionInspectorViewModel, bg: Color) -> Vec<Line<'static>> {
    let title_style = Style::default()
        .fg(theme::text())
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let role = crate::types::split_role_title(inspector.session_kind, &inspector.title);
    let mut title = Vec::new();
    let mut identity = Vec::new();
    match role {
        Some(role) => {
            title.push(Span::styled(
                glyphs::role_code(role.role),
                Style::default()
                    .fg(glyphs::role_color(role.role))
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ));
            title.push(Span::styled(" ", Style::default().bg(bg)));
            title.push(Span::styled(role.subject.to_string(), title_style));
            identity.push(role.role.to_string());
        }
        None => title.push(Span::styled(inspector.title.clone(), title_style)),
    }
    if let Some(kind) = &inspector.kind {
        identity.push(kind.to_lowercase());
    }
    let mut lines = vec![Line::from(title)];
    if !identity.is_empty() {
        lines.push(Line::from(Span::styled(
            identity.join(" · "),
            Style::default().fg(theme::dim_metadata()).bg(bg),
        )));
    }
    lines
}

/// `• unread   ◆ pinned   ↺ retry 1/3`
fn inspector_signal_line(
    signals: &[crate::types::InspectorSignal],
    bg: Color,
) -> Option<Line<'static>> {
    if signals.is_empty() {
        return None;
    }
    let mut spans = Vec::new();
    for (index, signal) in signals.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("   ", Style::default().bg(bg)));
        }
        let (glyph, color) = glyphs::signal_glyph(*signal);
        spans.push(Span::styled(
            format!("{glyph} "),
            Style::default()
                .fg(color)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            signal.label(),
            Style::default().fg(theme::subtext1()).bg(bg),
        ));
    }
    Some(Line::from(spans))
}

fn render_embedded_session_inspector(
    frame: &mut Frame,
    area: Rect,
    inspector: Option<&SessionInspectorViewModel>,
    source_session: Option<&Session>,
    bg: Color,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let title = inspector
        .map(|inspector| format!(" {:02} ", inspector.ordinal))
        .unwrap_or_default();
    let block = Block::default()
        .borders(Borders::TOP)
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme::active_row_rail())
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(theme::browser_decorative_separator()))
        .padding(Padding::horizontal(1))
        .style(Style::default().bg(bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(inspector) = inspector else {
        frame.render_widget(
            Paragraph::new("No session selected")
                .style(Style::default().fg(theme::dim_metadata()).bg(bg)),
            inner,
        );
        return;
    };
    let width = inner.width as usize;

    if area.height <= EMBEDDED_COMPACT_INSPECTOR_HEIGHT {
        let lines = inspector_execution_line(inspector, source_session, bg)
            .into_iter()
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().bg(bg))
                .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }

    let status_color = status_color(inspector.status);
    let mut header = vec![
        Span::styled(
            format!(
                "{} {}",
                crate::types::row::navigator_lifecycle_icon(inspector.status),
                glyphs::status_word(inspector.status)
            ),
            Style::default()
                .fg(status_color)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default().bg(bg)),
    ];
    for line in inspector_title_lines(inspector, bg).into_iter().take(1) {
        header.extend(line.spans);
    }
    let mut lines = vec![Line::from(header), Line::default()];
    if let Some(execution) = inspector_execution_line(inspector, source_session, bg) {
        lines.push(execution);
    }
    push_glyph_block(
        &mut lines,
        glyphs::NEXT,
        status_color,
        &inspector_primary_text(inspector),
        theme::subtext1(),
        width,
        bg,
    );
    if let Some(context) = inspector.runtime.context.compact_label() {
        push_glyph_line(
            &mut lines,
            glyphs::CONTEXT,
            theme::subtext0(),
            context,
            theme::subtext1(),
            bg,
        );
    }
    push_glyph_line(
        &mut lines,
        glyphs::LOCATION,
        theme::subtext0(),
        inspector_location_text(&inspector.location),
        theme::dim_metadata(),
        bg,
    );

    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(bg))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn render_inspector_body(
    lines: &mut Vec<Line<'static>>,
    body: &SessionInspectorBody,
    width: usize,
    bg: Color,
) {
    match body {
        SessionInspectorBody::Waiting {
            requirement,
            next_action,
            why,
            latest_state,
        } => {
            push_glyph_block(
                lines,
                glyphs::NEXT,
                theme::status_waiting(),
                next_action,
                theme::text(),
                width,
                bg,
            );
            push_glyph_block(
                lines,
                glyphs::REQUIRED,
                theme::status_waiting(),
                match requirement {
                    WaitingRequirement::Reply => "reply required",
                    WaitingRequirement::Approval => "approval required",
                },
                theme::status_waiting(),
                width,
                bg,
            );
            push_prose_block(lines, why, theme::subtext0(), width, bg);
            if let Some(latest) = latest_state {
                push_quote_block(lines, latest, theme::overlay0(), width, bg);
            }
        }
        SessionInspectorBody::Running {
            current_work,
            current_work_is_placeholder,
            summary,
            description,
            latest_state,
            warning,
        } => {
            // The header already says "running"; only a reported task adds
            // information here.
            if !current_work_is_placeholder {
                push_glyph_block(
                    lines,
                    glyphs::NEXT,
                    theme::status_running(),
                    current_work,
                    theme::text(),
                    width,
                    bg,
                );
            }
            render_inspector_summary_description(lines, summary, description, width, bg);
            if let Some(latest) = latest_state {
                push_quote_block(lines, latest, theme::overlay0(), width, bg);
            }
            if let Some(warning) = warning {
                push_prose_block(lines, warning, theme::warning_status(), width, bg);
            }
        }
        SessionInspectorBody::Failed {
            evidence,
            evidence_source,
            retry,
            summary,
            description,
            last_good_state,
            artifact,
        } => {
            let source = match evidence_source {
                FailureEvidenceSource::StopReason => "daemon stop reason",
                FailureEvidenceSource::CachedEventHeuristic => "cached event heuristic",
                FailureEvidenceSource::Unavailable => "unavailable",
            };
            push_glyph_block(
                lines,
                glyphs::FAILED,
                theme::status_failed(),
                &format!("{evidence}\nvia {source}"),
                theme::text(),
                width,
                bg,
            );
            let recovery = retry
                .map(|retry| format!("retry attempt {} of {}", retry.attempt, retry.max))
                .unwrap_or_else(|| "no automatic retry state cached".to_string());
            push_glyph_block(
                lines,
                glyphs::RETRY,
                theme::yellow(),
                &recovery,
                theme::subtext1(),
                width,
                bg,
            );
            render_inspector_summary_description(lines, summary, description, width, bg);
            if let Some(last_good) = last_good_state {
                push_quote_block(lines, last_good, theme::status_completed(), width, bg);
            }
            if let Some(artifact) = artifact {
                push_glyph_block(
                    lines,
                    glyphs::ARTIFACT,
                    theme::subtext0(),
                    artifact,
                    theme::subtext1(),
                    width,
                    bg,
                );
            }
        }
        SessionInspectorBody::Completed {
            outcome,
            summary,
            description,
            test_passed,
            clippy_passed,
            artifact,
            handoff,
            follow_up,
        } => {
            push_glyph_block(
                lines,
                glyphs::DONE,
                theme::status_completed(),
                outcome,
                theme::text(),
                width,
                bg,
            );
            render_inspector_summary_description(lines, summary, description, width, bg);
            lines.push(Line::from(
                [("tests", *test_passed), ("clippy", *clippy_passed)]
                    .into_iter()
                    .enumerate()
                    .flat_map(|(index, (name, value))| {
                        let (glyph, color) = verification_glyph(value);
                        [
                            Span::styled(
                                format!("{}{name} ", if index > 0 { "   " } else { "" }),
                                Style::default().fg(theme::subtext0()).bg(bg),
                            ),
                            Span::styled(
                                glyph,
                                Style::default()
                                    .fg(color)
                                    .bg(bg)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]
                    })
                    .collect::<Vec<_>>(),
            ));
            lines.push(Line::default());
            if let Some(artifact) = artifact {
                push_glyph_block(
                    lines,
                    glyphs::ARTIFACT,
                    theme::subtext0(),
                    artifact,
                    theme::subtext1(),
                    width,
                    bg,
                );
            }
            if let Some(handoff) = handoff {
                push_glyph_block(
                    lines,
                    glyphs::HANDOFF,
                    theme::subtext0(),
                    handoff,
                    theme::subtext1(),
                    width,
                    bg,
                );
            }
            if let Some(follow_up) = follow_up {
                push_glyph_block(
                    lines,
                    glyphs::FOLLOW_UP,
                    theme::warning_status(),
                    follow_up,
                    theme::subtext1(),
                    width,
                    bg,
                );
            }
        }
        SessionInspectorBody::Container {
            rollup,
            next_descendant,
            active_descendants,
            direct_children,
            lead,
        } => {
            let counts = [
                (
                    glyphs::NEEDS_YOU,
                    rollup.attention_count,
                    theme::status_waiting(),
                ),
                ("●", rollup.active_count, theme::status_running()),
                (
                    glyphs::DESCENDANTS,
                    rollup.descendant_count,
                    theme::subtext0(),
                ),
            ];
            lines.push(Line::from(
                counts
                    .into_iter()
                    .enumerate()
                    .flat_map(|(index, (glyph, count, color))| {
                        [
                            Span::styled(
                                format!("{}{glyph} ", if index > 0 { "   " } else { "" }),
                                Style::default()
                                    .fg(color)
                                    .bg(bg)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                count.to_string(),
                                Style::default().fg(theme::subtext1()).bg(bg),
                            ),
                        ]
                    })
                    .collect::<Vec<_>>(),
            ));
            lines.push(Line::default());
            if let Some(next) = next_descendant {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} ", glyphs::NEXT),
                        Style::default()
                            .fg(theme::status_waiting())
                            .bg(bg)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(
                            "{} ",
                            crate::types::row::navigator_lifecycle_icon(next.status)
                        ),
                        Style::default().fg(status_color(next.status)).bg(bg),
                    ),
                    Span::styled(
                        next.title.clone(),
                        Style::default().fg(theme::text()).bg(bg),
                    ),
                ]));
                lines.push(Line::default());
            }
            if !active_descendants.is_empty() {
                for descendant in active_descendants {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!(
                                "{} ",
                                crate::types::row::navigator_lifecycle_icon(descendant.status)
                            ),
                            Style::default().fg(status_color(descendant.status)).bg(bg),
                        ),
                        Span::styled(
                            descendant.title.clone(),
                            Style::default().fg(theme::subtext1()).bg(bg),
                        ),
                    ]));
                }
                lines.push(Line::default());
            }
            push_glyph_block(
                lines,
                glyphs::LEAD,
                theme::mauve(),
                &match lead {
                    Some(lead) => format!("{lead} · {direct_children} direct children"),
                    None => format!("no lead assigned · {direct_children} direct children"),
                },
                theme::subtext1(),
                width,
                bg,
            );
        }
        SessionInspectorBody::Inactive {
            reason,
            outcome,
            summary,
            description,
        } => {
            push_glyph_block(
                lines,
                "■",
                theme::status_interrupted(),
                reason,
                theme::subtext1(),
                width,
                bg,
            );
            if let Some(outcome) = outcome {
                push_glyph_block(
                    lines,
                    glyphs::PREVIOUS,
                    theme::subtext0(),
                    outcome,
                    theme::subtext1(),
                    width,
                    bg,
                );
            }
            render_inspector_summary_description(lines, summary, description, width, bg);
        }
    }
}

/// Summary then description, as prose without section labels: the first
/// prose under the title is self-evidently the description.
fn render_inspector_summary_description(
    lines: &mut Vec<Line<'static>>,
    summary: &Option<String>,
    description: &Option<String>,
    width: usize,
    bg: Color,
) {
    if let Some(summary) = summary {
        push_prose_block(lines, summary, theme::text(), width, bg);
    }
    if let Some(description) = description {
        push_prose_block(lines, description, theme::subtext1(), width, bg);
    }
}

/// Execution, runtime, context, timestamps, then identity paths — five to
/// eight glyph-led lines instead of eleven labelled sections.
fn render_inspector_facts(
    lines: &mut Vec<Line<'static>>,
    inspector: &SessionInspectorViewModel,
    source_session: Option<&Session>,
    width: usize,
    bg: Color,
) {
    let runtime = &inspector.runtime;
    let glyph_style = Style::default().fg(theme::subtext0()).bg(bg);
    let value_style = Style::default().fg(theme::subtext1()).bg(bg);
    let dim_style = Style::default().fg(theme::dim_metadata()).bg(bg);

    if let Some(execution) = inspector_execution_line(inspector, source_session, bg) {
        lines.push(execution);
    }

    if runtime.provider.is_some() {
        let mut parts: Vec<Vec<Span<'static>>> = Vec::new();
        if let Some(turns) = runtime.turns {
            parts.push(vec![
                Span::styled(format!("{} ", glyphs::TURNS), glyph_style),
                Span::styled(turns.to_string(), value_style),
            ]);
        }
        if let Some(cost) = runtime.cost_usd {
            parts.push(vec![Span::styled(
                if cost < 0.01 {
                    "$<.01".to_string()
                } else {
                    format!("${cost:.2}")
                },
                value_style,
            )]);
        }
        let times = [
            runtime
                .work_time_ms
                .map(|work| format!("{} work", format_work_time_ms(work))),
            runtime
                .duration_ms
                .map(|duration| format!("{} run", format_work_time_ms(duration))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if !times.is_empty() {
            parts.push(vec![
                Span::styled(format!("{} ", glyphs::WORK_TIME), glyph_style),
                Span::styled(times.join(" · "), value_style),
            ]);
        }
        if !parts.is_empty() {
            let mut spans = Vec::new();
            for (index, part) in parts.into_iter().enumerate() {
                if index > 0 {
                    spans.push(Span::styled("   ", Style::default().bg(bg)));
                }
                spans.extend(part);
            }
            lines.push(Line::from(spans));
        }
    }

    render_inspector_context(lines, inspector, width, bg);

    lines.push(Line::from(vec![
        Span::styled(format!("{} ", glyphs::CREATED), glyph_style),
        Span::styled(
            runtime
                .created_at
                .with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string(),
            dim_style,
        ),
        Span::styled("   ", Style::default().bg(bg)),
        Span::styled(format!("{} ", glyphs::UPDATED), glyph_style),
        Span::styled(format_relative_time(runtime.updated_at), dim_style),
    ]));
    lines.push(Line::default());

    push_glyph_line(
        lines,
        glyphs::SESSION_ID,
        theme::subtext0(),
        inspector.session_id.to_string(),
        theme::subtext1(),
        bg,
    );
    push_glyph_line(
        lines,
        glyphs::WORKING_DIR,
        theme::subtext0(),
        glyphs::home_relative(&inspector.working_dir),
        theme::subtext1(),
        bg,
    );
    match (&inspector.sandbox_root, &inspector.sandbox_branch) {
        (Some(root), Some(branch)) if glyphs::shared_sandbox_identifier(root, branch).is_some() => {
            // Worktree and branch share the session UUID: one line, both marks.
            push_glyph_line(
                lines,
                &format!("{} {}", glyphs::SANDBOX, glyphs::BRANCH),
                theme::subtext0(),
                glyphs::home_relative(root),
                theme::subtext1(),
                bg,
            );
        }
        (root, branch) => {
            if let Some(root) = root {
                push_glyph_line(
                    lines,
                    glyphs::SANDBOX,
                    theme::subtext0(),
                    glyphs::home_relative(root),
                    theme::subtext1(),
                    bg,
                );
            }
            if let Some(branch) = branch {
                push_glyph_line(
                    lines,
                    glyphs::BRANCH,
                    theme::subtext0(),
                    branch.clone(),
                    theme::subtext1(),
                    bg,
                );
            }
        }
    }
}

/// `✻ claude-opus-5-5  ▮▮▮▮▮ max` — the inspector keeps the full canonical
/// model ID; the list shows only its versioned suffix.
fn inspector_execution_line(
    inspector: &SessionInspectorViewModel,
    source_session: Option<&Session>,
    bg: Color,
) -> Option<Line<'static>> {
    let provider = inspector.runtime.provider?;
    let mut spans = vec![
        Span::styled(
            format!("{} ", glyphs::provider_glyph(provider)),
            Style::default()
                .fg(glyphs::provider_color(provider))
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            inspector
                .runtime
                .model
                .clone()
                .unwrap_or_else(|| inspector_provider_label(provider).to_string()),
            Style::default().fg(theme::subtext1()).bg(bg),
        ),
    ];
    if let Some((effort, bars)) = source_session.and_then(|session| {
        actual_effort_parts(session.effort.as_deref(), session.model.as_deref())
    }) {
        spans.push(Span::styled("  ", Style::default().bg(bg)));
        if let Some((filled, total)) = bars {
            spans.push(Span::styled(
                "▮".repeat(filled),
                Style::default()
                    .fg(theme::yellow())
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                "▯".repeat(total.saturating_sub(filled)),
                Style::default().fg(theme::surface1()).bg(bg),
            ));
            spans.push(Span::styled(" ", Style::default().bg(bg)));
        }
        spans.push(Span::styled(
            effort,
            Style::default().fg(theme::dim_metadata()).bg(bg),
        ));
    }
    Some(Line::from(spans))
}

/// `◔ ▰▰▰▰▱▱▱▱▱▱ 42%  421k / 1m repository fallback` plus one dim line with
/// the remaining provenance (source, version, freshness, …).
fn render_inspector_context(
    lines: &mut Vec<Line<'static>>,
    inspector: &SessionInspectorViewModel,
    width: usize,
    bg: Color,
) {
    let context = &inspector.runtime.context;
    let rows = context.detail_rows();
    if rows.is_empty() && context.percent.is_none() {
        return;
    }
    let mut spans = vec![Span::styled(
        format!("{} ", glyphs::CONTEXT),
        Style::default().fg(theme::subtext0()).bg(bg),
    )];
    match context.percent {
        Some(percent) => {
            spans.push(Span::styled(
                glyphs::percent_gauge(percent, 10),
                Style::default()
                    .fg(theme::context_border_color(percent))
                    .bg(bg),
            ));
            spans.push(Span::styled(
                format!(" {:.0}%", percent.clamp(0.0, 100.0)),
                Style::default().fg(theme::subtext1()).bg(bg),
            ));
        }
        None => spans.push(Span::styled(
            glyphs::CONTEXT_UNKNOWN,
            Style::default().fg(theme::dim_metadata()).bg(bg),
        )),
    }
    let headline = rows.iter().find(|row| row.label == "context");
    if let Some(row) = headline {
        // The gauge already shows the percentage; keep only what it adds.
        let value = context
            .percent
            .and_then(|percent| {
                row.value
                    .strip_prefix(&format!("{:.0}% ", percent.clamp(0.0, 100.0)))
            })
            .unwrap_or(&row.value);
        spans.push(Span::styled(
            format!("  {value}"),
            Style::default().fg(theme::subtext1()).bg(bg),
        ));
    }
    lines.push(Line::from(spans));

    // One provenance row per line: joining them would let provider capacity
    // (`default · max · factor`) read as part of the active budget.
    for row in rows
        .iter()
        .filter(|row| row.label != "context")
        // The headline already names the active budget's source.
        .filter(|row| {
            !(row.label == "source"
                && headline.is_some_and(|head| head.value.ends_with(&row.value)))
        })
    {
        for line in wrap_plain(
            &format!("{}: {}", row.label, row.value),
            width.saturating_sub(2).max(1),
        ) {
            lines.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(theme::dim_metadata()).bg(bg),
            )));
        }
    }
}

/// A glyph gutter followed by wrapped text; continuation lines keep the
/// gutter's indent. Ends with a blank separator line.
fn push_glyph_block(
    lines: &mut Vec<Line<'static>>,
    glyph: &str,
    glyph_color: Color,
    text: &str,
    text_color: Color,
    width: usize,
    bg: Color,
) {
    let gutter = navigator_layout::display_width(glyph) + 1;
    let mut first = true;
    for paragraph in text.lines() {
        for line in wrap_plain(paragraph, width.saturating_sub(gutter).max(1)) {
            let lead = if first {
                Span::styled(
                    format!("{glyph} "),
                    Style::default()
                        .fg(glyph_color)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(" ".repeat(gutter), Style::default().bg(bg))
            };
            first = false;
            lines.push(Line::from(vec![
                lead,
                Span::styled(line, Style::default().fg(text_color).bg(bg)),
            ]));
        }
    }
    lines.push(Line::default());
}

/// One unwrapped glyph-led fact line (IDs and paths must never be split).
fn push_glyph_line(
    lines: &mut Vec<Line<'static>>,
    glyph: &str,
    glyph_color: Color,
    text: String,
    text_color: Color,
    bg: Color,
) {
    lines.push(Line::from(vec![
        Span::styled(format!("{glyph} "), Style::default().fg(glyph_color).bg(bg)),
        Span::styled(text, Style::default().fg(text_color).bg(bg)),
    ]));
}

/// Quoted cached output: every wrapped line carries the `▎` rail.
fn push_quote_block(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    rail_color: Color,
    width: usize,
    bg: Color,
) {
    for paragraph in text.lines() {
        for line in wrap_plain(paragraph, width.saturating_sub(2).max(1)) {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{} ", glyphs::QUOTE_RAIL),
                    Style::default().fg(rail_color).bg(bg),
                ),
                Span::styled(line, Style::default().fg(theme::subtext0()).bg(bg)),
            ]));
        }
    }
    lines.push(Line::default());
}

fn push_prose_block(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    color: Color,
    width: usize,
    bg: Color,
) {
    for paragraph in text.lines() {
        for line in wrap_plain(paragraph, width.max(1)) {
            lines.push(Line::from(Span::styled(
                line,
                Style::default().fg(color).bg(bg),
            )));
        }
    }
    lines.push(Line::default());
}

/// Word wrap by display cells; words longer than a line are hard-broken.
fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    use unicode_segmentation::UnicodeSegmentation;
    let mut out = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let word_width = navigator_layout::display_width(word);
        let current_width = navigator_layout::display_width(&current);
        if !current.is_empty() && current_width + 1 + word_width <= width {
            current.push(' ');
            current.push_str(word);
            continue;
        }
        if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        if word_width <= width {
            current.push_str(word);
            continue;
        }
        for grapheme in word.graphemes(true) {
            if navigator_layout::display_width(&current) + navigator_layout::display_width(grapheme)
                > width
                && !current.is_empty()
            {
                out.push(std::mem::take(&mut current));
            }
            current.push_str(grapheme);
        }
    }
    if !current.is_empty() || out.is_empty() {
        out.push(current);
    }
    out
}

fn verification_glyph(value: Option<bool>) -> (&'static str, Color) {
    match value {
        Some(true) => (glyphs::DONE, theme::status_completed()),
        Some(false) => (glyphs::FAILED, theme::status_failed()),
        None => ("—", theme::dim_metadata()),
    }
}

fn inspector_provider_label(provider: rsi_common::types::SessionProvider) -> &'static str {
    use rsi_common::types::SessionProvider;
    match provider {
        SessionProvider::Claude => "Claude",
        SessionProvider::Codex => "Codex",
        SessionProvider::Pioneer => "Pioneer",
        SessionProvider::OpenRouter => "OpenRouter",
        SessionProvider::Bedrock => "Bedrock",
        SessionProvider::Local => "Local",
        SessionProvider::Antigravity => "Antigravity",
        SessionProvider::CodexAppServer => "Codex AS",
        SessionProvider::Harness => "Harness",
        _ => "Unknown",
    }
}

fn inspector_section_label(label: &str, color: Color, bg: Color) -> Line<'static> {
    let color = if color == theme::browser_section_label() {
        theme::subtext0()
    } else {
        color
    };
    Line::from(Span::styled(
        label.to_string(),
        Style::default().fg(color).bg(bg),
    ))
}

fn inspector_primary_text(inspector: &SessionInspectorViewModel) -> String {
    match &inspector.body {
        SessionInspectorBody::Waiting { next_action, .. } => next_action.clone(),
        SessionInspectorBody::Running { current_work, .. } => current_work.clone(),
        SessionInspectorBody::Failed { evidence, .. } => evidence.clone(),
        SessionInspectorBody::Completed { outcome, .. } => outcome.clone(),
        SessionInspectorBody::Container {
            next_descendant: Some(descendant),
            ..
        } => format!("Next: {}", descendant.title),
        SessionInspectorBody::Container { rollup, .. } => format!(
            "{} need · {} active · {} descendants",
            rollup.attention_count, rollup.active_count, rollup.descendant_count
        ),
        SessionInspectorBody::Inactive { reason, .. } => reason.clone(),
    }
}

fn render_selected_signal(
    frame: &mut Frame,
    area: Rect,
    inspector: Option<&SessionInspectorViewModel>,
    bg: Color,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let Some(inspector) = inspector else {
        fill_area(frame, area, bg);
        return;
    };
    let signal_bg = theme::selected_row_bg();
    fill_area(frame, area, signal_bg);
    let requirement = match inspector.body {
        SessionInspectorBody::Waiting {
            requirement: WaitingRequirement::Reply,
            ..
        } => " · reply required",
        SessionInspectorBody::Waiting {
            requirement: WaitingRequirement::Approval,
            ..
        } => " · approval required",
        _ => "",
    };
    let first = Line::from(vec![
        Span::styled(
            "▌",
            Style::default()
                .fg(theme::active_row_rail())
                .bg(signal_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {:02}  ", inspector.ordinal),
            Style::default()
                .fg(theme::active_row_rail())
                .bg(signal_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "{} {}{}",
                crate::types::row::navigator_lifecycle_icon(inspector.status),
                glyphs::status_word(inspector.status),
                requirement
            ),
            Style::default()
                .fg(status_color(inspector.status))
                .bg(signal_bg)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(first).style(Style::default().bg(signal_bg)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if area.height > 1 {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "▌",
                    Style::default().fg(theme::active_row_rail()).bg(signal_bg),
                ),
                Span::styled(
                    format!(
                        " {}",
                        truncate_chars(
                            &inspector_primary_text(inspector),
                            area.width.saturating_sub(2) as usize
                        )
                    ),
                    Style::default()
                        .fg(theme::text())
                        .bg(signal_bg)
                        .add_modifier(Modifier::BOLD),
                ),
            ]))
            .style(Style::default().bg(signal_bg)),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
    }
}

fn render_session_activity(
    frame: &mut Frame,
    area: Rect,
    activity: Option<&SessionActivityViewModel>,
    selected_id: Option<uuid::Uuid>,
    bg: Color,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(theme::browser_decorative_separator()))
        .padding(Padding::new(2, 1, 1, 0))
        .style(Style::default().bg(bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(activity) = activity else {
        return;
    };
    let width = inner.width as usize;
    let mut lines = vec![
        Line::from(Span::styled(
            "LIVE ACTIVITY",
            Style::default()
                .fg(theme::browser_section_label())
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "─".repeat(width),
            Style::default()
                .fg(theme::browser_decorative_separator())
                .bg(bg),
        )),
        Line::default(),
    ];
    let queue = activity
        .operator_queue
        .iter()
        .filter(|item| Some(item.session_id) != selected_id)
        .take(4)
        .collect::<Vec<_>>();
    if !queue.is_empty() {
        lines.push(activity_section_label(
            glyphs::QUEUE,
            theme::status_waiting(),
            "QUEUE",
            bg,
        ));
        for item in queue {
            lines.push(activity_item_line(item, inner.width, bg));
        }
        lines.push(Line::default());
    }
    let changes = activity
        .recent_changes
        .iter()
        .filter(|item| Some(item.session_id) != selected_id)
        .take(6)
        .collect::<Vec<_>>();
    if !changes.is_empty() {
        lines.push(activity_section_label(
            glyphs::CHANGES,
            theme::accent(),
            "CHANGES",
            bg,
        ));
        for item in changes {
            lines.push(activity_item_line(item, inner.width, bg));
        }
        lines.push(Line::default());
    }
    lines.push(inspector_section_label(
        "FLOW",
        theme::browser_section_label(),
        bg,
    ));
    lines.push(flow_bar_line(&activity.flow, width, bg));
    lines.push(flow_counts_line(&activity.flow, bg));
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "─".repeat(width),
        Style::default()
            .fg(theme::browser_decorative_separator())
            .bg(bg),
    )));
    lines.extend(legend_lines(width, bg));
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(bg))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn activity_section_label(glyph: &str, color: Color, name: &str, bg: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{glyph} "),
            Style::default()
                .fg(color)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            name.to_string(),
            Style::default().fg(theme::subtext0()).bg(bg),
        ),
    ])
}

/// FLOW segments: needs you, in flight, recent, quiet. Each segment has its
/// own fill shape as well as its own color so the bar reads without color.
fn flow_segments(flow: &crate::types::SessionFlowSummary) -> [(usize, &'static str, Color); 4] {
    [
        (flow.needs_you, "█", theme::status_waiting()),
        (flow.in_flight, "▓", theme::status_running()),
        (flow.recent, "▒", theme::status_completed()),
        (flow.quiet, "░", theme::dim_metadata()),
    ]
}

/// Proportional one-line FLOW bar. Every non-empty group keeps at least one
/// cell; the remaining cells go by largest remainder, so the bar always fills
/// exactly `width` cells when anything is counted.
fn flow_bar_cells(counts: [usize; 4], width: usize) -> [usize; 4] {
    let total = counts.iter().sum::<usize>();
    let nonzero = counts.iter().filter(|count| **count > 0).count();
    if total == 0 || width < nonzero {
        return [0; 4];
    }
    let spare = width - nonzero;
    let mut cells = [0usize; 4];
    let mut remainders = [(0usize, 0usize); 4];
    for (index, count) in counts.iter().enumerate() {
        if *count == 0 {
            continue;
        }
        let exact = count * spare;
        cells[index] = 1 + exact / total;
        remainders[index] = (exact % total, index);
    }
    let mut leftover = width - cells.iter().sum::<usize>();
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, index) in remainders {
        if leftover == 0 {
            break;
        }
        if counts[index] > 0 {
            cells[index] += 1;
            leftover -= 1;
        }
    }
    cells
}

fn flow_bar_line(
    flow: &crate::types::SessionFlowSummary,
    width: usize,
    bg: Color,
) -> Line<'static> {
    let segments = flow_segments(flow);
    let cells = flow_bar_cells(segments.map(|(count, ..)| count), width);
    Line::from(
        segments
            .iter()
            .zip(cells)
            .filter(|(_, cells)| *cells > 0)
            .map(|((_, fill, color), cells)| {
                Span::styled(fill.repeat(cells), Style::default().fg(*color).bg(bg))
            })
            .collect::<Vec<_>>(),
    )
}

/// `! 6   ● 1   ✓ 29   · 19   × 1`
fn flow_counts_line(flow: &crate::types::SessionFlowSummary, bg: Color) -> Line<'static> {
    let counts = [
        (glyphs::NEEDS_YOU, flow.needs_you, theme::status_waiting()),
        ("●", flow.in_flight, theme::status_running()),
        (glyphs::DONE, flow.recent, theme::status_completed()),
        ("·", flow.quiet, theme::dim_metadata()),
        (glyphs::FAILED, flow.failed, theme::status_failed()),
    ];
    let mut spans = Vec::new();
    for (index, (glyph, count, color)) in counts.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("   ", Style::default().bg(bg)));
        }
        spans.push(Span::styled(
            format!("{glyph} "),
            Style::default()
                .fg(color)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            count.to_string(),
            Style::default().fg(color).bg(bg),
        ));
    }
    Line::from(spans)
}

/// Dim symbol key packed into the activity pane's spare rows: lifecycle,
/// attention, provider, then role families.
fn legend_lines(width: usize, bg: Color) -> Vec<Line<'static>> {
    use rsi_common::types::SessionProvider;
    let lifecycle = [
        (SessionStatus::Running, "run"),
        (SessionStatus::Completed, "done"),
        (SessionStatus::Failed, "fail"),
        (SessionStatus::Interrupted, "stop"),
        (SessionStatus::Starting, "start"),
        (SessionStatus::WaitingApproval, "wait"),
        (SessionStatus::Archived, "arch"),
    ]
    .map(|(status, word)| {
        (
            crate::types::row::navigator_lifecycle_icon(status).to_string(),
            status_color(status),
            word,
        )
    });
    let attention = [
        (glyphs::NEEDS_YOU, theme::warning_status(), "you"),
        (glyphs::UNREAD, theme::accent(), "unread"),
        (glyphs::RETRY, theme::yellow(), "retry"),
        (glyphs::STALLED, theme::status_stalled(), "stall"),
        (glyphs::PIN, theme::pin(), "pin"),
        (glyphs::ROTATION, theme::subtext0(), "rotated"),
        (glyphs::CONTEXT_UNKNOWN, theme::dim_metadata(), "ctx ?"),
    ]
    .map(|(glyph, color, word)| (glyph.to_string(), color, word));
    let providers = [
        (SessionProvider::Claude, "claude"),
        (SessionProvider::Codex, "codex"),
        (SessionProvider::CodexAppServer, "codex as"),
        (SessionProvider::OpenRouter, "openrouter"),
        (SessionProvider::Local, "local"),
        (SessionProvider::Antigravity, "antigravity"),
        (SessionProvider::Pioneer, "pioneer"),
        (SessionProvider::Harness, "harness"),
    ]
    .map(|(provider, word)| {
        (
            glyphs::provider_glyph(provider).to_string(),
            glyphs::provider_color(provider),
            word,
        )
    });
    let roles = [
        ("Manager", "lead"),
        ("Planner", "plan"),
        ("Implementer", "build"),
        ("Debugger", "debug"),
        ("Reviewer", "check"),
    ]
    .map(|(role, word)| (glyphs::role_code(role), glyphs::role_color(role), word));

    let mut lines = Vec::new();
    for group in [
        lifecycle.as_slice(),
        attention.as_slice(),
        providers.as_slice(),
        roles.as_slice(),
    ] {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut used = 0usize;
        for (glyph, color, word) in group {
            let entry_width =
                navigator_layout::display_width(glyph) + 1 + navigator_layout::display_width(word);
            let gap = if used == 0 { 0 } else { 2 };
            if used > 0 && used + gap + entry_width > width {
                lines.push(Line::from(std::mem::take(&mut spans)));
                used = 0;
            }
            if used > 0 {
                spans.push(Span::styled("  ", Style::default().bg(bg)));
                used += 2;
            }
            spans.push(Span::styled(
                format!("{glyph} "),
                Style::default().fg(*color).bg(bg),
            ));
            spans.push(Span::styled(
                word.to_string(),
                Style::default().fg(theme::dim_metadata()).bg(bg),
            ));
            used += entry_width;
        }
        if !spans.is_empty() {
            lines.push(Line::from(spans));
        }
    }
    lines
}

/// `05 × Db 68861f41-c2ac-4a4f-b87…   4m`
fn activity_item_line(
    item: &crate::types::SessionActivityItem,
    width: u16,
    bg: Color,
) -> Line<'static> {
    const PREFIX_CELLS: usize = 5;
    const AGE_CELLS: usize = 5;
    let title_width = (width as usize).saturating_sub(PREFIX_CELLS + 1 + AGE_CELLS);
    let mut spans = vec![
        Span::styled(
            format!("{:02} ", item.ordinal),
            Style::default().fg(theme::dim_metadata()).bg(bg),
        ),
        Span::styled(
            format!(
                "{} ",
                crate::types::row::navigator_lifecycle_icon(item.status)
            ),
            Style::default()
                .fg(status_color(item.status))
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    match crate::types::split_role_title(item.session_kind, &item.title) {
        Some(role) if title_width > 4 => {
            spans.push(Span::styled(
                format!("{} ", glyphs::role_code(role.role)),
                Style::default()
                    .fg(glyphs::role_color(role.role))
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(fixed_span(
                role.subject,
                title_width - 3,
                Style::default().fg(theme::subtext1()).bg(bg),
            ));
        }
        _ => spans.push(fixed_span(
            &item.title,
            title_width,
            Style::default().fg(theme::subtext1()).bg(bg),
        )),
    }
    spans.push(fixed_span("", 1, Style::default().bg(bg)));
    spans.push(Span::styled(
        navigator_layout::align_cells(
            &operational_age(&format_relative_time(item.updated_at)),
            AGE_CELLS,
            navigator_layout::Alignment::Right,
        ),
        Style::default().fg(theme::dim_metadata()).bg(bg),
    ));
    Line::from(spans)
}

/// Render a session list pane using dense row layout.
pub fn render_session_list(
    frame: &mut Frame,
    area: Rect,
    pane: &Pane,
    focused: bool,
    app: &mut App,
    surface: SessionListSurface,
    viewed_session_id: Option<uuid::Uuid>,
) {
    #[cfg(test)]
    let _theme_render_guard = theme::test_render_guard();
    let (
        selected_index,
        active_zone,
        tr_selected_index,
        archive_selected_index,
        jobs_selected_index,
    ) = match pane {
        Pane::SessionList {
            selected_index,
            active_zone,
            taskrabbit_selected_index,
            archive_selected_index,
            jobs_selected_index,
            ..
        } => (
            *selected_index,
            *active_zone,
            *taskrabbit_selected_index,
            *archive_selected_index,
            *jobs_selected_index,
        ),
        _ => return,
    };
    if area.height == 0 || area.width == 0 {
        return;
    }

    let descent_path: Vec<uuid::Uuid> = app
        .tabs
        .get(app.active_tab)
        .map(|t| t.descent_path.clone())
        .unwrap_or_default();
    let descent_head = descent_path.last().copied();
    let sort_order = app.settings.sort_order;
    let (order, selected_index, accent_color, effective_sort, labels) = match active_zone {
        crate::types::SessionListZone::Main => (
            app.filtered_session_order.clone(),
            selected_index,
            theme::accent(),
            sort_order,
            app.labels.clone(),
        ),
        crate::types::SessionListZone::TaskRabbit => (
            app.filtered_taskrabbit_order.clone(),
            tr_selected_index,
            theme::teal(),
            crate::app::SortOrder::StalestFirst,
            Vec::new(),
        ),
        crate::types::SessionListZone::Archive => (
            app.filtered_archived_order.clone(),
            archive_selected_index,
            theme::peach(),
            crate::app::SortOrder::StalestFirst,
            Vec::new(),
        ),
        crate::types::SessionListZone::Jobs => (
            app.filtered_jobs_order.clone(),
            jobs_selected_index,
            theme::blue(),
            crate::app::SortOrder::StalestFirst,
            Vec::new(),
        ),
    };

    app.refresh_session_focus_index();

    let bg = list_surface_bg(&app.settings);
    fill_area(frame, area, bg);
    let project_name = app
        .active_tab()
        .project_id
        .and_then(|project_id| app.projects.iter().find(|project| project.id == project_id))
        .map(|project| project.name.clone())
        .unwrap_or_else(|| "All projects".to_string());
    let scope_h = 1u16.min(area.height);
    let scope_area = Rect::new(area.x, area.y, area.width, scope_h);
    render_scope_header(
        frame,
        scope_area,
        active_zone,
        order.len(),
        &project_name,
        &descent_path,
        &app.sessions,
        &app.search_query,
        bg,
    );

    let content_area = Rect::new(
        area.x,
        area.y + scope_h,
        area.width,
        area.height.saturating_sub(scope_h),
    );
    if content_area.height == 0 {
        app.session_list_render.last_list_area = None;
        app.session_list_render.cards_area = None;
        return;
    }

    let browser_layout = resolve_session_browser_layout(content_area, surface, active_zone);
    let presentation = match browser_layout.mode {
        SessionBrowserMode::LegacyTable => SessionListPresentation::Table(
            SessionListDensity::for_width(browser_layout.navigator.width),
        ),
        SessionBrowserMode::OperationalTable | SessionBrowserMode::EmbeddedInspector => {
            SessionListPresentation::OperationalTable
        }
        SessionBrowserMode::Relay | SessionBrowserMode::RelayWithActivity => {
            SessionListPresentation::Navigator
        }
    };
    let selected_id = order.get(selected_index).copied();
    let selected_session = selected_id
        .and_then(|id| app.sessions.get(&id))
        .map(|state| state.session.clone());
    let inspector = selected_id.and_then(|id| {
        crate::types::compute_session_inspector(app, id, selected_index.saturating_add(1))
    });
    if browser_layout.activity.is_some() {
        app.refresh_session_activity();
    }
    let activity = app.session_list_render.activity.clone();

    if let Some(signal_area) = browser_layout.selected_signal {
        render_selected_signal(frame, signal_area, inspector.as_ref(), bg);
    }

    let empty_message = if active_zone == crate::types::SessionListZone::Main {
        app.session_list_empty_message()
    } else {
        zone_empty_message(active_zone).to_string()
    };
    let sessions = &app.sessions;
    let projects = &app.projects;
    let settings = &app.settings;
    let manager_ids = app.manager_roster.member_ids();
    let search_active = !app.search_query.is_empty()
        && app.search_target == crate::types::SearchTarget::SessionList;
    let render_state = &mut app.session_list_render;
    let crate::types::SessionListRenderState {
        main,
        taskrabbit,
        archive,
        jobs,
        focus_index,
        last_list_area,
        cards_area,
        ..
    } = render_state;
    let zone_render = match active_zone {
        crate::types::SessionListZone::Main => main,
        crate::types::SessionListZone::TaskRabbit => taskrabbit,
        crate::types::SessionListZone::Archive => archive,
        crate::types::SessionListZone::Jobs => jobs,
    };
    let list_render_area = if matches!(
        browser_layout.mode,
        SessionBrowserMode::Relay | SessionBrowserMode::RelayWithActivity
    ) {
        Rect::new(
            browser_layout.navigator.x,
            browser_layout.navigator.y.saturating_add(1),
            browser_layout.navigator.width,
            browser_layout.navigator.height.saturating_sub(1),
        )
    } else {
        browser_layout.navigator
    };
    let body_area = render_zone_table(
        frame,
        list_render_area,
        &order,
        sessions,
        projects,
        settings,
        selected_index,
        focused,
        zone_render,
        active_zone,
        accent_color,
        effective_sort,
        &labels,
        presentation,
        viewed_session_id,
        focus_index,
        &manager_ids,
        descent_head,
        search_active,
        &empty_message,
    );
    let selected_row_y = zone_render
        .card_offsets
        .get(selected_index)
        .copied()
        .filter(|offset| {
            *offset >= zone_render.scroll_offset
                && *offset < zone_render.scroll_offset + body_area.height as usize
        })
        .map(|offset| body_area.y + offset.saturating_sub(zone_render.scroll_offset) as u16);
    *last_list_area = Some(browser_layout.navigator);
    *cards_area = Some(body_area);

    if let Some(inspector_area) = browser_layout.inspector {
        render_session_inspector(
            frame,
            inspector_area,
            inspector.as_ref(),
            selected_session.as_ref(),
            bg,
        );
    }
    if let Some(inspector_area) = browser_layout.embedded_inspector {
        render_embedded_session_inspector(
            frame,
            inspector_area,
            inspector.as_ref(),
            selected_session.as_ref(),
            bg,
        );
    }
    if let (Some(tether_area), Some(y)) = (browser_layout.tether, selected_row_y)
        && y < tether_area.y.saturating_add(tether_area.height)
    {
        frame.render_widget(
            Paragraph::new("─".repeat(tether_area.width as usize)).style(
                Style::default()
                    .fg(theme::active_row_rail())
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Rect::new(tether_area.x, y, tether_area.width, 1),
        );
    }
    if let Some(activity_area) = browser_layout.activity {
        render_session_activity(frame, activity_area, activity.as_ref(), selected_id, bg);
    }
}

fn render_zone_table(
    frame: &mut Frame,
    area: Rect,
    order: &[uuid::Uuid],
    sessions: &std::collections::HashMap<uuid::Uuid, crate::types::SessionState>,
    projects: &[rsi_common::types::Project],
    settings: &crate::settings::UserSettings,
    selected_index: usize,
    is_focused: bool,
    zone_render: &mut crate::types::ZoneRenderState,
    active_zone: crate::types::SessionListZone,
    accent_color: ratatui::style::Color,
    sort_order: crate::app::SortOrder,
    labels: &[rsi_common::types::SessionLabel],
    presentation: SessionListPresentation,
    viewed_session_id: Option<uuid::Uuid>,
    focus_index: &HashMap<uuid::Uuid, SessionFocusEntry>,
    manager_ids: &HashSet<uuid::Uuid>,
    descent_head: Option<uuid::Uuid>,
    search_active: bool,
    empty_message: &str,
) -> Rect {
    let bg = list_surface_bg(settings);
    fill_area(frame, area, bg);

    let operational_ordinal_width = operational_ordinal_width(order, sessions);

    let header_h = column_header_height(presentation).min(area.height);
    let available_body_h = area.height.saturating_sub(header_h);
    let body_h = match presentation {
        SessionListPresentation::OperationalTable => {
            available_body_h.min(OPERATIONAL_TABLE_MAX_BODY_HEIGHT)
        }
        SessionListPresentation::Navigator => available_body_h.min(NAVIGATOR_MAX_BODY_HEIGHT),
        SessionListPresentation::Table(_) => available_body_h,
    };

    if header_h > 0 {
        render_navigator_header(
            frame,
            Rect::new(area.x, area.y, area.width, 1),
            bg,
            operational_ordinal_width,
            settings.navigator_preset,
            settings.navigator_optional_columns.as_deref(),
        );
    }

    let body_area = Rect::new(area.x, area.y + header_h, area.width, body_h);
    compute_table_geometry(
        order,
        sessions,
        labels,
        selected_index,
        presentation,
        body_area.width,
        zone_render,
        sort_order,
        active_zone,
        focus_index,
        manager_ids,
        descent_head,
        search_active,
    );
    ensure_selected_row_visible(selected_index, body_area.height as usize, zone_render);

    if order.is_empty() {
        if body_area.height > 0 {
            frame.render_widget(
                Paragraph::new(empty_message.to_string())
                    .style(Style::default().fg(theme::subtext0()).bg(bg)),
                body_area,
            );
        }
    } else {
        render_dense_rows(
            frame,
            body_area,
            order,
            sessions,
            projects,
            settings,
            selected_index,
            is_focused,
            zone_render,
            accent_color,
            presentation,
            operational_ordinal_width,
            viewed_session_id,
            focus_index,
            manager_ids,
            descent_head,
            bg,
        );
    }

    if matches!(
        presentation,
        SessionListPresentation::OperationalTable | SessionListPresentation::Navigator
    ) {
        render_operational_summary(
            frame,
            area,
            order.len(),
            zone_render,
            body_area.height as usize,
            presentation,
            bg,
        );
    }
    body_area
}

fn render_operational_summary(
    frame: &mut Frame,
    area: Rect,
    order_len: usize,
    zone: &crate::types::ZoneRenderState,
    viewport_h: usize,
    presentation: SessionListPresentation,
    bg: Color,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let (first, last) = visible_session_range(zone, viewport_h, order_len);
    let visible = if first == 0 { 0 } else { last - first + 1 };
    let below = order_len.saturating_sub(last);
    let summary = format!("{visible}/{order_len}");
    if presentation == SessionListPresentation::OperationalTable {
        frame.render_widget(
            Paragraph::new(summary).style(Style::default().fg(theme::dim_metadata()).bg(bg)),
            Rect::new(
                area.x.saturating_add(2),
                area.y.saturating_add(area.height.saturating_sub(1)),
                area.width.saturating_sub(4),
                1,
            ),
        );
        return;
    }
    if area.height < 7 {
        return;
    }
    let rule_y = area.y.saturating_add(area.height.saturating_sub(5));
    frame.render_widget(
        Paragraph::new("─".repeat(area.width.saturating_sub(4) as usize)).style(
            Style::default()
                .fg(theme::browser_decorative_separator())
                .bg(bg),
        ),
        Rect::new(
            area.x.saturating_add(2),
            rule_y,
            area.width.saturating_sub(4),
            1,
        ),
    );
    let right = if below > 0 {
        format!("{}{below}", glyphs::BELOW)
    } else {
        String::new()
    };
    let gap = area.width.saturating_sub(4).saturating_sub(
        (navigator_layout::display_width(&summary) + navigator_layout::display_width(&right))
            as u16,
    ) as usize;
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(summary, Style::default().fg(theme::dim_metadata()).bg(bg)),
            Span::styled(" ".repeat(gap), Style::default().bg(bg)),
            Span::styled(right, Style::default().fg(theme::dim_metadata()).bg(bg)),
        ]))
        .style(Style::default().bg(bg)),
        Rect::new(
            area.x.saturating_add(2),
            rule_y.saturating_add(2),
            area.width.saturating_sub(4),
            1,
        ),
    );
}

fn log_render_timer(
    timer: Option<std::time::Instant>,
    session_id: uuid::Uuid,
    events: usize,
    width: u16,
) {
    if let Some(start) = timer {
        let elapsed = start.elapsed();
        tracing::trace!(
            target = "rsi::profile",
            session_id = %session_id,
            events,
            width,
            ms = elapsed.as_secs_f64() * 1000.0,
            "render_session_detail"
        );
    }
}

fn detail_title(session: &Session) -> String {
    session
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .or_else(|| session.query.lines().find(|line| !line.trim().is_empty()))
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or("Untitled session")
        .to_string()
}

fn detail_header_metadata(app: &App, state: &SessionState) -> Line<'static> {
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            "ID",
            Style::default()
                .fg(theme::sapphire())
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ),
        Span::styled(
            format!("  {}  ", status_icon(state.session.status)),
            Style::default()
                .fg(status_color(state.session.status))
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(meta) = status::render_session_meta_segment_for(app, state.session.id) {
        spans.extend(meta);
    }
    if let Some(context) = status::render_context_percent_segment_for(app, state.session.id) {
        spans.push(Span::styled(
            "  ·  ",
            Style::default().fg(theme::dim_metadata()),
        ));
        spans.extend(context);
    }
    // Plan-window utilization sits beside context fill: both answer "how much
    // budget is left" — one per session, one per account. This pair previously
    // rendered in the bottom strip; when that strip was simplified the context
    // segment moved here, so its companion follows it rather than being
    // resurrected in the old location.
    if let Some(rate) = status::render_rate_limit_segment_for(app, state.session.id) {
        spans.push(Span::styled(
            "  ·  ",
            Style::default().fg(theme::dim_metadata()),
        ));
        spans.extend(rate);
    }
    Line::from(spans)
}

fn terminal_reason_span(session: &Session) -> Option<Span<'static>> {
    let label = match session.status {
        SessionStatus::Failed => "failed",
        SessionStatus::Interrupted => "interrupted",
        SessionStatus::Completed if session.terminal_reason.is_some() => "completed",
        _ => return None,
    };
    let mut text = format!("[terminal: {label}]");
    if let Some(reason) = session
        .terminal_reason
        .as_deref()
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
    {
        text.push_str(" ");
        text.push_str(reason);
    }
    if let Some(stop_reason) = session
        .stop_reason
        .as_deref()
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
    {
        text.push_str(" · stop: ");
        text.push_str(stop_reason);
    }
    let color = if session.status == SessionStatus::Completed {
        theme::metadata_text()
    } else {
        theme::red()
    };
    Some(Span::styled(
        text,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ))
}

pub fn is_error_event(event: &rsi_common::types::ConversationEvent) -> bool {
    if let Some(value) = event
        .metadata
        .as_deref()
        .and_then(|metadata| metadata.get("is_error"))
        .and_then(serde_json::Value::as_bool)
    {
        return value;
    }
    event.event_type == EventType::System
        && (event.content.to_lowercase().contains("error")
            || event.content.to_lowercase().contains("failed"))
}

pub fn has_terminal_error(state: &SessionState) -> bool {
    matches!(
        state.session.status,
        SessionStatus::Failed | SessionStatus::Interrupted
    ) && state.events.iter().rev().any(is_error_event)
}

fn last_error_event_span(state: &SessionState) -> Option<Span<'static>> {
    if !matches!(
        state.session.status,
        SessionStatus::Failed | SessionStatus::Interrupted
    ) {
        return None;
    }
    state
        .events
        .iter()
        .rev()
        .find(|event| is_error_event(event))
        .map(|event| {
            let content = event.content.trim().chars().take(120).collect::<String>();
            Span::styled(
                format!("[error] {content}"),
                Style::default().fg(theme::red()),
            )
        })
}

/// Find the range of event indices visible at the given scroll offset.
/// Returns (first_visible_idx, last_visible_idx) inclusive, or None if no events.
///
/// Uses binary search on the sorted `event_offsets` for O(log N) lookup.
/// An event is visible if its vertical span [offset, offset+height) overlaps
/// the viewport [scroll_offset, scroll_offset+viewport_height).
pub(crate) fn find_visible_events(
    event_offsets: &[usize],
    event_heights: &[usize],
    scroll_offset: usize,
    viewport_height: usize,
) -> Option<(usize, usize)> {
    if event_offsets.is_empty() {
        return None;
    }

    let visible_end = scroll_offset + viewport_height;

    // Binary search: find the first event whose TOP edge is <= scroll_offset.
    // partition_point returns the first index where offset > scroll_offset,
    // so we subtract 1 to get the last event starting at or before scroll_offset.
    let candidate = event_offsets.partition_point(|&off| off <= scroll_offset);
    let first = if candidate == 0 { 0 } else { candidate - 1 };

    // Scan forward from `first` to skip events fully above the viewport
    // (height-0 events or events whose bottom edge <= scroll_offset)
    let first = (first..event_offsets.len())
        .find(|&i| event_offsets[i] + event_heights[i] > scroll_offset)?;

    // Binary search for last event whose top edge is before visible_end.
    let last_candidate = event_offsets.partition_point(|&off| off < visible_end);
    let last = if last_candidate == 0 {
        first
    } else {
        (last_candidate - 1).max(first)
    };

    Some((first, last))
}

/// Count consecutive thinking events starting at event_idx.
/// Only called when thinking events are hidden (!show_thinking_events),
/// so the check is a simple EventType match.
fn count_thinking_run(state: &SessionState, start_idx: usize) -> usize {
    state.events[start_idx..]
        .iter()
        .take_while(|e| e.event_type == EventType::Thinking)
        .count()
}

/// Render a session detail pane.
pub fn render_session_detail(
    frame: &mut Frame,
    area: Rect,
    session_id: uuid::Uuid,
    focused: bool,
    app: &App,
) {
    #[cfg(test)]
    let _theme_render_guard = theme::test_render_guard();
    let Some(state) = app.sessions.get(&session_id) else {
        let empty = Paragraph::new("Session not found.")
            .style(Style::default().fg(theme::empty_state()))
            .block(theme::pane_block(focused));
        frame.render_widget(empty, area);
        return;
    };
    let render_timer = profiling::start_timer();

    let session = &state.session;

    let detail_bg = detail_surface_bg(&app.settings);
    fill_area(frame, area, detail_bg);

    let header_h = area.height.min(SESSION_DETAIL_HEADER_HEIGHT);
    if header_h > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {}", detail_title(session)),
                Style::default()
                    .fg(theme::text())
                    .add_modifier(Modifier::BOLD),
            )))
            .style(Style::default().bg(detail_bg)),
            Rect::new(area.x, area.y, area.width, 1),
        );
    }
    if header_h > 1 {
        frame.render_widget(
            Paragraph::new(detail_header_metadata(app, state))
                .style(Style::default().bg(detail_bg)),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
    }
    if header_h > 2 {
        let divider = "─".repeat(area.width as usize);
        frame.render_widget(
            Paragraph::new(divider).style(
                Style::default()
                    .fg(theme::session_detail_border())
                    .bg(detail_bg),
            ),
            Rect::new(area.x, area.y + 2, area.width, 1),
        );
    }

    let inner = Rect::new(
        area.x.saturating_add(2),
        area.y.saturating_add(header_h),
        area.width.saturating_sub(4),
        area.height.saturating_sub(header_h),
    );

    // Use inner.width as content_width — this is the exact width ratatui renders the
    // Paragraph into after applying the explicit transcript inset above.
    // mod.rs pre-computes heights with the same effective width via
    // area.width.saturating_sub(theme::SESSION_DETAIL_HORIZ_INSET), which must equal
    // inner.width.
    let content_width = inner.width.max(1);

    // Render active_task indicator at top of detail view when scrolled to top
    let active_task_height = if let Some(ref task) = session.active_task {
        if state.scroll_offset == 0 {
            let truncated = if task.len() > (content_width as usize).saturating_sub(7) {
                format!(
                    "{}...",
                    &task[..task.len().min((content_width as usize).saturating_sub(10))]
                )
            } else {
                task.clone()
            };
            let task_line = Line::from(vec![
                Span::styled("[ctx] ", Style::default().fg(theme::metadata_text())),
                Span::styled(truncated, Style::default().fg(theme::empty_state())),
            ]);
            frame.render_widget(
                Paragraph::new(task_line),
                Rect {
                    x: inner.x,
                    y: inner.y,
                    width: inner.width,
                    height: 1,
                },
            );
            1u16
        } else {
            0
        }
    } else {
        0
    };
    // Render pending-archive indicator below active_task when session is marked
    let pending_archive_height = if session.pending_archive
        && matches!(
            session.status,
            SessionStatus::Running | SessionStatus::Starting
        )
        && state.scroll_offset == 0
    {
        let y = inner.y + active_task_height;
        let label = Line::from(Span::styled(
            "[pending archive]",
            Style::default().fg(theme::overlay1()),
        ));
        frame.render_widget(
            Paragraph::new(label),
            Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: 1,
            },
        );
        1u16
    } else {
        0
    };
    // Render issue URL below pending-archive indicator when session has an issue link
    let issue_url_height = if let Some(ref url) = session.issue_url {
        if state.scroll_offset == 0 {
            let y = inner.y + active_task_height + pending_archive_height;
            let mut spans = Vec::new();
            if let Some(ref id) = session.issue_identifier {
                spans.push(Span::styled(
                    format!("[{}] ", id),
                    Style::default()
                        .fg(theme::sapphire())
                        .add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(
                url.as_str(),
                Style::default().fg(theme::subtext0()),
            ));
            let url_line = Line::from(spans);
            frame.render_widget(
                Paragraph::new(url_line),
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
            );
            1u16
        } else {
            0
        }
    } else {
        0
    };
    let terminal_error = last_error_event_span(state);
    let terminal_reason = terminal_reason_span(session);
    let has_terminal_info =
        state.scroll_offset == 0 && (terminal_error.is_some() || terminal_reason.is_some());
    let terminal_info_height = if has_terminal_info { 1u16 } else { 0 };

    // Render sandbox path + branch below issue URL when session is sandboxed
    let sandbox_info_height = if session.sandbox_root.is_some() && state.scroll_offset == 0 {
        let y = inner.y
            + active_task_height
            + pending_archive_height
            + issue_url_height
            + terminal_info_height;
        let mut spans = Vec::new();
        spans.push(Span::styled(
            "\u{22A1} sandbox: ".to_string(), // ⊡
            Style::default().fg(theme::teal()),
        ));
        if let Some(ref root) = session.sandbox_root {
            let root_display = root.to_string_lossy().replacen(
                &dirs::home_dir().map_or(String::new(), |h| h.to_string_lossy().to_string()),
                "~",
                1,
            );
            spans.push(Span::styled(
                root_display.to_string(),
                Style::default().fg(theme::subtext0()),
            ));
        }
        if let Some(ref branch) = session.sandbox_branch {
            spans.push(Span::styled(
                format!(" ({})", branch),
                Style::default().fg(theme::overlay2()),
            ));
        }
        let sandbox_line = Line::from(spans);
        frame.render_widget(
            Paragraph::new(sandbox_line),
            Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: 1,
            },
        );
        1u16
    } else {
        0
    };
    if has_terminal_info {
        let y = inner.y + active_task_height + pending_archive_height + issue_url_height;
        let mut spans = Vec::new();
        if let Some(reason) = &terminal_reason {
            spans.push(reason.clone());
        }
        if terminal_reason.is_some() && terminal_error.is_some() {
            spans.push(Span::styled("  ", Style::default()));
        }
        if let Some(error) = &terminal_error {
            spans.push(error.clone());
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: 1,
            },
        );
    }
    let recursive_dag_height = if state.scroll_offset == 0 {
        let y = inner.y
            + active_task_height
            + pending_archive_height
            + issue_url_height
            + terminal_info_height
            + sandbox_info_height;
        if let Some(line) = recursive_dag_detail_line(app, session, inner.width) {
            frame.render_widget(
                Paragraph::new(line),
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
            );
            1u16
        } else {
            0
        }
    } else {
        0
    };
    let header_height = active_task_height
        + pending_archive_height
        + issue_url_height
        + sandbox_info_height
        + terminal_info_height
        + recursive_dag_height;
    let inner = if header_height > 0 {
        Rect {
            x: inner.x,
            y: inner.y + header_height,
            width: inner.width,
            height: inner.height.saturating_sub(header_height),
        }
    } else {
        inner
    };
    let is_active = matches!(
        session.status,
        SessionStatus::Running | SessionStatus::Starting
    );
    let formulation_active = state
        .formulation
        .map(|f| chrono::Utc::now().timestamp_millis() - f.started_at_ms < FORMULATION_ANIMATION_MS)
        .unwrap_or(false);
    let show_loading_bar = is_active && area.height > 5 && !formulation_active;

    let mut viewport_height = inner.height as usize;
    if show_loading_bar {
        viewport_height = viewport_height.saturating_sub(activity_indicator_height(
            app.settings.activity_indicator_style,
        ) as usize);
    }

    // Empty session handling
    if state.events.is_empty() {
        let is_active = matches!(
            session.status,
            SessionStatus::Running | SessionStatus::Starting
        );
        let msg = if is_active {
            Line::from(Span::styled(
                format!(
                    "  {} Waiting for output...",
                    animated_status_icon(session.status)
                ),
                Style::default().fg(theme::empty_state()),
            ))
        } else {
            Line::from(Span::styled(
                "No output.",
                Style::default().fg(theme::empty_state()),
            ))
        };
        frame.render_widget(Paragraph::new(msg), inner);
        log_render_timer(render_timer, session_id, state.events.len(), area.width);
        return;
    }

    // Find visible event range
    let Some((first_visible, last_visible)) = find_visible_events(
        &state.event_offsets,
        &state.event_heights,
        state.scroll_offset,
        viewport_height,
    ) else {
        log_render_timer(render_timer, session_id, state.events.len(), area.width);
        return;
    };

    // Build model transition map: from_sequence -> (old_model, new_model)
    let transitions: HashMap<i32, (String, String)> = {
        let mut map = HashMap::new();
        let segs = &state.model_segments;
        for window in segs.windows(2) {
            let prev = &window[0];
            let next = &window[1];
            map.insert(
                next.from_sequence,
                (prev.model_id.clone(), next.model_id.clone()),
            );
        }
        map
    };

    // --- Formulation animation ---
    // If a grow animation is active for the last event, render it at the bottom
    // of the inner area and skip it in the main loop below.
    let formulation_skip_idx: Option<usize> = if let Some(form) = state.formulation {
        if form.target_height > 0 {
            let elapsed = chrono::Utc::now().timestamp_millis() - form.started_at_ms;
            let anim_ms = app.settings.formulation_anim_ms.max(1) as i64;
            let progress = (elapsed as f32 / anim_ms as f32).clamp(0.0, 1.0);
            let growth =
                (form.target_height.saturating_sub(LOADING_CONTAINER_HEIGHT)) as f32 * progress;
            let current_height = (LOADING_CONTAINER_HEIGHT as f32 + growth).round() as u16;
            render_formulating_event(
                frame,
                inner,
                state,
                form.event_index,
                current_height,
                progress,
            );
            Some(form.event_index)
        } else {
            None
        }
    } else {
        None
    };

    // Render each visible event as its own Paragraph.
    //
    // The loop integrates both core rendering and thinking fold indicators.
    // The thinking indicator check MUST come BEFORE the generic `event_height == 0`
    // guard, because hidden thinking events use height 0 (absorbed into previous
    // indicator) and the first thinking event in a run uses height 2 (for the indicator).
    for event_idx in first_visible..=last_visible {
        if event_idx >= state.events.len() {
            break;
        }

        let event_height = state.event_heights[event_idx];
        let event_offset = state.event_offsets[event_idx];
        let event = &state.events[event_idx];
        let is_cursor = state.current_event_index == Some(event_idx);

        // Skip the event being rendered by the formulation animation.
        if Some(event_idx) == formulation_skip_idx {
            continue;
        }

        // --- Thinking fold indicator ---
        // Must come BEFORE the height-0 guard below.
        if !state.show_thinking_events && event.event_type == EventType::Thinking {
            if event_height == 2 {
                // First thinking event in a run — render the fold indicator
                let thinking_count = count_thinking_run(state, event_idx);
                let label = if thinking_count == 1 {
                    "  \u{1F4AD} 1 thinking event".to_string()
                } else {
                    format!("  \u{1F4AD} {} thinking events", thinking_count)
                };
                let indicator_lines = vec![
                    Line::from(Span::styled(
                        label,
                        Style::default()
                            .fg(theme::fold_indicator())
                            .add_modifier(Modifier::ITALIC),
                    )),
                    Line::default(),
                ];

                // Position the indicator
                let event_y = if event_idx == first_visible {
                    0
                } else {
                    event_offset.saturating_sub(state.scroll_offset)
                };
                let remaining_viewport = viewport_height.saturating_sub(event_y);
                if remaining_viewport == 0 {
                    break;
                }
                let rect_height = 2usize.min(remaining_viewport);
                let event_rect = Rect::new(
                    inner.x,
                    inner.y + event_y as u16,
                    inner.width,
                    rect_height as u16,
                );
                frame.render_widget(Paragraph::new(indicator_lines), event_rect);
                continue;
            }
            // event_height == 0: absorbed into previous indicator, skip
            continue;
        }

        // --- Tool group indicator ---
        // Must come BEFORE the height-0 guard below.
        // A collapsed tool group (see `ui::tool_projection`) renders one summary
        // card on its leader row; every other member of the group has height 0.
        // Group membership is id-based pairing, so the summary counts *calls*,
        // not transcript rows.
        if let Some(group) = state.tool_projection.group_of(event_idx) {
            let is_tool_collapsed = crate::ui::height::is_event_effectively_collapsed(state, event);

            if is_tool_collapsed {
                if crate::ui::height::group_is_collapsed(state, group) {
                    // Leadership comes from the same projection `update_event_heights`
                    // used, so the row that reserved `1 + EVENT_CARD_GAP` lines is
                    // exactly the row that paints them. (Testing `event_height == 4`
                    // instead would alias with an individually-collapsed card, which
                    // also measures 4.)
                    if group.leader_idx() == event_idx {
                        debug_assert_eq!(
                            event_height,
                            1 + crate::ui::height::EVENT_CARD_GAP,
                            "collapsed tool-group leader height must match its rendered card"
                        );
                        let tool_count = group.call_count();
                        let pending = group.pending_count();
                        // An unresolved call is real state (still running, or the
                        // session was interrupted), so say so rather than implying
                        // the call completed.
                        let pending_note = if pending == 0 {
                            String::new()
                        } else if pending == tool_count {
                            " · pending".to_string()
                        } else {
                            format!(" · {} pending", pending)
                        };
                        let label = if tool_count == 1 {
                            format!("1 tool call{pending_note}")
                        } else {
                            format!("{tool_count} tool calls{pending_note}")
                        };
                        let indicator_lines = vec![Line::from(vec![
                            Span::styled(
                                if is_cursor { "▎ " } else { "│ " },
                                Style::default().fg(if is_cursor {
                                    theme::tool_call_selected_border()
                                } else {
                                    theme::tool_call_border()
                                }),
                            ),
                            Span::styled(
                                label,
                                Style::default()
                                    .fg(theme::fold_indicator())
                                    .add_modifier(Modifier::ITALIC),
                            ),
                        ])];

                        let event_y = if event_idx == first_visible {
                            0
                        } else {
                            event_offset.saturating_sub(state.scroll_offset)
                        };
                        let remaining_viewport = viewport_height.saturating_sub(event_y);
                        if remaining_viewport == 0 {
                            break;
                        }
                        let rect_height = event_height.min(remaining_viewport);
                        let event_rect = Rect::new(
                            inner.x,
                            inner.y + event_y as u16,
                            inner.width,
                            rect_height as u16,
                        );
                        // The trailing EVENT_CARD_GAP row stays unpainted.
                        let card_rect =
                            card_rect_within(event_rect, event_height, 0, remaining_viewport);
                        frame.render_widget(Paragraph::new(indicator_lines), card_rect);
                        continue;
                    }
                    // event_height == 0: absorbed into group summary, skip
                    continue;
                }
                // Group is expanded — fall through to normal collapsed rendering
            }
        }

        // --- Skip hidden events (height 0) ---
        if event_height == 0 {
            continue;
        }

        // Every event uses the same four-column horizontal inset. Conversation
        // turns spend it on the role rail; operational cards spend it on border
        // and padding. This keeps wrapping stable across event types.
        let render_width = content_width.saturating_sub(4);

        // Get cached lines or build fresh.
        let lines = if let Some(entry) =
            crate::ui::height::cached_render_event(state, event_idx, render_width, is_cursor)
        {
            entry.lines.clone()
        } else {
            let is_collapsed = crate::ui::height::is_event_effectively_collapsed(state, event);
            let is_expanded = state.expanded_events.contains(&event.sequence);
            let is_last_event = !state.events.is_empty() && event_idx == state.events.len() - 1;
            let model_name = content::model_for_sequence(
                &state.model_segments,
                event.sequence,
                state.session.model.as_deref(),
            );
            let ctx = content::EventRenderContext {
                is_collapsed,
                is_expanded,
                is_cursor,
                is_last_event,
                model_name,
                pipeline_commands: state.docregblock_contents.clone(),
                max_width: render_width,
            };
            content::build_event_lines(event, &ctx)
        };

        // Calculate positioning within viewport
        let lines_to_skip;
        let event_y;

        if event_idx == first_visible {
            // First visible event may be partially scrolled
            lines_to_skip = state.scroll_offset.saturating_sub(event_offset);
            event_y = 0;
        } else {
            lines_to_skip = 0;
            event_y = event_offset.saturating_sub(state.scroll_offset);
        }

        let remaining_viewport = viewport_height.saturating_sub(event_y);
        if remaining_viewport == 0 {
            break;
        }

        // event_y is bounded by viewport_height (which derives from inner.height: u16),
        // so the u16 cast is safe.
        debug_assert!(
            event_y <= u16::MAX as usize,
            "event_y exceeds u16: {}",
            event_y
        );

        // The height we give this event's rect — enough for its full content,
        // clamped to remaining viewport. Paragraph::wrap + Rect clipping handles the rest.
        // event_height was computed at effective_width (bubble-aware), so this is exact.
        let rect_height = (event_height.saturating_sub(lines_to_skip)).min(remaining_viewport);

        let event_rect = Rect::new(
            inner.x,
            inner.y + event_y as u16,
            inner.width,
            rect_height as u16,
        );

        let is_tool_event = matches!(event.event_type, EventType::ToolUse | EventType::ToolResult);
        let is_message_event = event.event_type == EventType::Message;

        if is_compact_tool_event(event) {
            // Card occupies border+label only — the trailing EVENT_CARD_GAP row stays
            // unpainted (base pane bg) to separate it from the next card.
            let card_rect =
                card_rect_within(event_rect, event_height, lines_to_skip, remaining_viewport);
            render_compact_tool_event(
                frame,
                card_rect,
                event,
                is_cursor,
                focused,
                event_bubble_bg(event, &app.settings),
            );

            if event_y + rect_height >= viewport_height {
                break;
            }
            continue;
        }

        if is_message_event {
            let card_rect =
                card_rect_within(event_rect, event_height, lines_to_skip, remaining_viewport);
            let text_rect = Rect::new(
                card_rect.x.saturating_add(2),
                card_rect.y,
                card_rect.width.saturating_sub(4),
                card_rect.height,
            );
            let rail_color = if is_cursor && focused {
                theme::focused_border()
            } else {
                match event.role {
                    Some(Role::User) => theme::user_role(),
                    _ => theme::assistant_role(),
                }
            };
            if card_rect.height > 0 {
                let rail = (0..card_rect.height)
                    .map(|_| {
                        Line::from(Span::styled(
                            if is_cursor { "▎" } else { "│" },
                            Style::default().fg(rail_color),
                        ))
                    })
                    .collect::<Vec<_>>();
                frame.render_widget(
                    Paragraph::new(rail),
                    Rect::new(card_rect.x, card_rect.y, 1, card_rect.height),
                );
            }

            let mut paragraph = Paragraph::new(lines)
                .style(Style::default().bg(detail_bg).fg(theme::text()))
                .wrap(Wrap { trim: false });
            if lines_to_skip > 0 {
                debug_assert!(
                    lines_to_skip <= u16::MAX as usize,
                    "Single event exceeds u16 line limit: {} lines",
                    lines_to_skip
                );
                paragraph = paragraph.scroll((lines_to_skip as u16, 0));
            }
            frame.render_widget(paragraph, text_rect);

            if let Some(next_event) = state.events.get(event_idx + 1) {
                if let Some((old_model, new_model)) = transitions.get(&next_event.sequence) {
                    let divider_y = event_y + rect_height;
                    if divider_y < viewport_height {
                        let divider_rect = Rect::new(
                            inner.x,
                            inner.y + divider_y.saturating_sub(1) as u16,
                            inner.width,
                            1,
                        );
                        let divider =
                            render_model_switch_divider(old_model, new_model, inner.width);
                        frame.render_widget(Paragraph::new(divider), divider_rect);
                    }
                }
            }

            if event_y + rect_height >= viewport_height {
                break;
            }
            continue;
        }

        let bubble_bg = event_bubble_bg(event, &app.settings);

        // Top border is visible only when the event starts at the top of the viewport
        // (lines_to_skip == 0). When scrolled in, omit it to avoid a floating top border.
        let borders = if lines_to_skip == 0 {
            Borders::ALL
        } else {
            Borders::LEFT | Borders::RIGHT | Borders::BOTTOM
        };
        // D3: tool-call bubbles build on `session_detail_block` (adopts the
        // BorderPolicy-aware Plain border type + horizontal padding + bg
        // convention), then override borders/glyph/color/bg. The dashed
        // glyph is now unconditional for tool events (previously suppressed
        // on cursor selection — the inconsistency F-011 flags); selection is
        // instead carried by border color alone
        // (`tool_call_selected_border()` vs. `tool_call_border()`).
        // Non-tool bubbles are byte-identical to before this slice: plain
        // border, `session_detail_border()` color, unaffected by selection
        // (Decision D3 — scope stays tool-call-only).
        let block = if is_tool_event {
            let border_color = if is_cursor {
                theme::tool_call_selected_border()
            } else {
                theme::tool_call_border()
            };
            theme::session_detail_block(focused, None)
                .borders(borders)
                .border_set(TOOL_CALL_BORDER)
                .border_style(Style::default().fg(border_color))
                .style(Style::default().bg(bubble_bg))
        } else {
            Block::default()
                .borders(borders)
                .border_style(Style::default().fg(theme::session_detail_border()))
                .padding(Padding::horizontal(1))
                .style(Style::default().bg(bubble_bg))
        };
        // Card occupies border+content only — the trailing EVENT_CARD_GAP row stays
        // unpainted (base pane bg) to separate it from the next card instead of the
        // two bubbles' fills touching directly.
        let card_rect =
            card_rect_within(event_rect, event_height, lines_to_skip, remaining_viewport);
        let para_rect = block.inner(card_rect);
        frame.render_widget(block, card_rect);

        // When partially scrolled, the first skipped row is the top border row —
        // the paragraph content skip is therefore lines_to_skip - 1.
        let content_skip = lines_to_skip.saturating_sub(1);
        let mut paragraph = Paragraph::new(lines)
            .style(Style::default().bg(bubble_bg).fg(theme::text()))
            .wrap(Wrap { trim: false });
        if content_skip > 0 {
            // Safe: content_skip is bounded by a single event's height,
            // which virtually never approaches u16::MAX (65535 lines).
            debug_assert!(
                content_skip <= u16::MAX as usize,
                "Single event exceeds u16 line limit: {} lines",
                content_skip
            );
            paragraph = paragraph.scroll((content_skip as u16, 0));
        }
        frame.render_widget(paragraph, para_rect);

        // Render model switch divider if the next event starts a new segment.
        // Overlaps the trailing blank line of this event for zero-cost rendering.
        if let Some(next_event) = state.events.get(event_idx + 1) {
            if let Some((old_model, new_model)) = transitions.get(&next_event.sequence) {
                let divider_y = event_y + rect_height;
                if divider_y < viewport_height {
                    let divider_rect = Rect::new(
                        inner.x,
                        inner.y + divider_y.saturating_sub(1) as u16,
                        inner.width,
                        1,
                    );
                    let divider = render_model_switch_divider(old_model, new_model, inner.width);
                    frame.render_widget(Paragraph::new(divider), divider_rect);
                }
            }
        }

        if event_y + rect_height >= viewport_height {
            break;
        }
    }

    log_render_timer(render_timer, session_id, state.events.len(), area.width);
}

pub(crate) fn is_compact_tool_event(event: &ConversationEvent) -> bool {
    event.event_type == EventType::ToolUse && event.tool_input.is_none()
}

fn render_compact_tool_event(
    frame: &mut Frame,
    area: Rect,
    event: &ConversationEvent,
    is_cursor: bool,
    _focused: bool,
    _bg: Color,
) {
    if area.width < 4 || area.height == 0 {
        return;
    }

    let tool = event.tool_name.as_deref().unwrap_or("Tool");
    let path = event.content.trim().replace('\n', " ");
    let time = event
        .created_at
        .with_timezone(&chrono::Local)
        .format("%H:%M")
        .to_string();
    let fixed_w = tool.chars().count() + time.chars().count() + 8;
    let path_w = area.width as usize;
    let path_w = path_w.saturating_sub(fixed_w).max(8);
    let line = Line::from(vec![
        Span::styled(
            if is_cursor { "▎ " } else { "│ " },
            Style::default().fg(if is_cursor {
                theme::tool_call_selected_border()
            } else {
                theme::tool_call_border()
            }),
        ),
        Span::styled(
            tool.to_string(),
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            truncate_chars(&path, path_w),
            Style::default().fg(theme::blue()),
        ),
        Span::raw("  "),
        Span::styled(time, Style::default().fg(theme::dim_metadata())),
    ]);
    frame.render_widget(
        Paragraph::new(line),
        Rect::new(area.x, area.y, area.width, 1),
    );
}

/// Format a DateTime<Utc> as a relative time string (e.g., "5m ago", "2h ago").
pub(crate) fn format_relative_time(dt: DateTime<Utc>) -> String {
    let now = Utc::now();
    let duration = now.signed_duration_since(dt);
    let secs = duration.num_seconds();
    if secs < 0 {
        "just now".to_string()
    } else if secs < 60 {
        "<1m ago".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

pub(crate) fn status_icon(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Starting => "\u{25CB}",
        SessionStatus::Running => "\u{25CF}",
        SessionStatus::WaitingApproval => "!",
        SessionStatus::Completed => "\u{2713}",
        SessionStatus::Failed => "\u{2717}",
        SessionStatus::Interrupted => "\u{2298}",
        SessionStatus::Archived => "\u{25A3}",
        SessionStatus::Deleted => "\u{2715}",
        _ => "?",
    }
}

const BRAILLE_FRAMES: &[&str] = &[
    "\u{280B}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283C}", "\u{2834}", "\u{2826}", "\u{2827}",
];

/// Animated status icon for the session detail title bar.
/// Active statuses (Starting, Running) cycle through Braille frames.
/// Inactive statuses return the static icon.
fn animated_status_icon(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Starting | SessionStatus::Running => {
            let millis = chrono::Utc::now().timestamp_millis();
            let frame = (millis / 100) as usize % BRAILLE_FRAMES.len();
            BRAILLE_FRAMES[frame]
        }
        _ => status_icon(status),
    }
}

/// Thin delegate to the single source of truth, [`theme::status_color`].
/// Retained so existing `session::status_color` callers (row builder, trash and
/// archive browsers) keep resolving without import churn.
pub(crate) fn status_color(status: SessionStatus) -> ratatui::style::Color {
    theme::status_color(status)
}

/// Map session kind to a theme color for pills and borders.
/// Returns None for kinds that should use the default accent color.
pub(crate) fn kind_color(kind: rsi_common::types::SessionKind) -> Option<Color> {
    use rsi_common::types::SessionKind;
    match kind {
        SessionKind::Group => Some(theme::group_powder_blue()),
        SessionKind::Epic => Some(theme::epic_purple()),
        SessionKind::Story => Some(theme::story_yellow_orange()),
        SessionKind::Task => Some(theme::task_gray()),
        SessionKind::Bug => Some(theme::red()),
        SessionKind::Feature => Some(theme::green()),
        SessionKind::Refactor => Some(theme::teal()),
        SessionKind::Research => Some(theme::sapphire()),
        SessionKind::Standard => Some(theme::raw_session_magenta()),
        SessionKind::TaskRabbit => None,
        _ => None,
    }
}

pub(crate) fn status_text(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Starting => "Starting",
        SessionStatus::Running => "Running",
        SessionStatus::WaitingApproval => "Waiting Approval",
        SessionStatus::Completed => "Completed",
        SessionStatus::Failed => "Failed",
        SessionStatus::Interrupted => "Interrupted",
        SessionStatus::Archived => "Archived",
        SessionStatus::Deleted => "Deleted",
        _ => "Unknown",
    }
}

/// Returns (display_query, rotation_count) for a session.
/// Follows the continued_from chain back to root.
pub(crate) fn resolve_session_display(
    session: &rsi_common::types::Session,
    all_sessions: &std::collections::HashMap<uuid::Uuid, SessionState>,
) -> (String, u32) {
    let identity = crate::types::resolve_session_display_identity(session, all_sessions);
    (identity.effective_title, identity.rotation_depth)
}

/// Find the largest byte index <= `idx` that is a valid UTF-8 char boundary.
pub(crate) fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Find the best byte offset to break a line at, preferring word boundaries (spaces).
/// Returns the byte length of the chunk to take from `s`, at most `max_width` characters.
/// If a space is found within the width limit, breaks after the last space (including the space
/// in the current chunk). Falls back to `floor_char_boundary` if no space exists.
pub(crate) fn word_boundary_break(s: &str, max_width: usize) -> usize {
    let char_limit = floor_char_boundary(s, max_width);
    if char_limit >= s.len() {
        return s.len();
    }
    match s[..char_limit].rfind(' ') {
        Some(space_pos) => space_pos + 1,
        None => char_limit,
    }
}

// Both NORMAL and INSERT occupy six cells, plus a separating space.
const INPUT_MODE_WIDTH: u16 = 7;
const INPUT_PROMPT_WIDTH: u16 = INPUT_MODE_WIDTH + 2;

/// Compute the total height needed for the input bar, including borders.
///
/// Wraps each logical line of the textarea content to `available_width` and sums
/// the visual line count. Returns `2 (top+bottom border) + clamp(1, wrapped_lines, MAX_INPUT_BAR_LINES)`.
pub fn compute_input_bar_height(
    textarea: &tui_textarea::TextArea<'_>,
    available_width: u16,
) -> u16 {
    // Two borders, mode/prompt, and one cell of breathing room on each side.
    let content_width = available_width
        .saturating_sub(INPUT_PROMPT_WIDTH + 4)
        .max(1) as usize;

    let wrapped_lines: usize = textarea
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
                    word_boundary_break(remaining, effective_width)
                };
                // Zero-break guard: prevent infinite loop
                if chunk_len == 0 {
                    break;
                }
                count += 1;
                is_first = false;
                remaining = &remaining[chunk_len..];
            }
            count.max(1)
        })
        .sum();

    // 2 for top+bottom border + at least 1 content line; grows as text wraps
    let content_lines = (wrapped_lines as u16).clamp(1, MAX_INPUT_BAR_LINES);
    2 + content_lines
}

/// Render textarea content as a wrapped Paragraph with manual cursor placement.
///
/// Instead of rendering tui-textarea's widget (which doesn't wrap), we:
/// 1. Extract text from textarea.lines()
/// 2. Hard-wrap each logical line at the available width
/// 3. Render as a Paragraph
/// 4. Place the terminal cursor at the correct visual position
/// How to display the cursor in the textarea.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorStyle {
    /// No cursor shown (unfocused).
    Hidden,
    /// Terminal beam cursor via set_cursor_position (insert mode).
    Beam,
    /// Block cursor rendered as reversed-color character (normal mode).
    Block,
}

pub fn render_wrapped_textarea(
    frame: &mut Frame,
    area: Rect,
    textarea: &tui_textarea::TextArea<'_>,
    cursor_style: CursorStyle,
    visual_selection: Option<((usize, usize), (usize, usize))>,
    background: Color,
) {
    let width = area.width as usize;
    if width == 0 || area.height == 0 {
        return;
    }

    let logical_lines = textarea.lines();
    let (cursor_row, cursor_col) = textarea.cursor();

    let mut visual_lines: Vec<Line<'static>> = Vec::new();
    let mut cursor_visual_row: Option<usize> = None;
    let mut cursor_visual_col: Option<usize> = None;

    for (line_idx, logical_line) in logical_lines.iter().enumerate() {
        if logical_line.is_empty() {
            if line_idx == cursor_row {
                cursor_visual_row = Some(visual_lines.len());
                cursor_visual_col = Some(0);
            }
            visual_lines.push(Line::from(""));
            continue;
        }

        // Detect leading whitespace for hanging-indent on continuation lines
        let indent_byte_len = logical_line.len() - logical_line.trim_start().len();
        let indent_str: &str = &logical_line[..indent_byte_len];
        let indent_char_len = indent_str.chars().count();

        // Wrap the logical line into visual chunks
        let mut remaining = logical_line.as_str();
        let mut col_offset: usize = 0;
        let mut is_first_chunk = true;

        while !remaining.is_empty() {
            // Continuation chunks have reduced width to account for indent prefix
            let effective_width = if is_first_chunk {
                width
            } else {
                width.saturating_sub(indent_char_len).max(1)
            };

            let chunk_len = if remaining.len() <= effective_width {
                remaining.len()
            } else {
                word_boundary_break(remaining, effective_width)
            };

            // Zero-break guard: prevent infinite loop
            if chunk_len == 0 {
                break;
            }

            let chunk = &remaining[..chunk_len];

            // Visual indent offset for continuation lines
            let visual_indent = if is_first_chunk { 0 } else { indent_char_len };

            // Check if cursor falls within this chunk
            if line_idx == cursor_row {
                let cursor_byte = byte_offset_of_col(logical_line, cursor_col);
                if cursor_byte >= col_offset && cursor_byte < col_offset + chunk_len {
                    cursor_visual_row = Some(visual_lines.len());
                    let chars_before = logical_line[col_offset..cursor_byte].chars().count();
                    cursor_visual_col = Some(chars_before + visual_indent);
                } else if cursor_byte == col_offset + chunk_len
                    && remaining.len() <= effective_width
                {
                    // Cursor at end of last chunk of this line
                    cursor_visual_row = Some(visual_lines.len());
                    cursor_visual_col = Some(chunk.chars().count() + visual_indent);
                }
            }

            // Build spans with visual selection highlighting if applicable
            if let Some(((sel_start_row, sel_start_col), (sel_end_row, sel_end_col))) =
                visual_selection
            {
                let base_style = Style::default().fg(theme::text()).bg(background);
                let sel_style = Style::default()
                    .fg(theme::text())
                    .bg(theme::visual_selection_bg());
                let chunk_chars: Vec<char> = chunk.chars().collect();
                let chunk_col_start = logical_line[..col_offset].chars().count();
                let mut spans: Vec<Span<'static>> = Vec::new();
                let mut current = String::new();
                let mut in_sel = false;

                // Prepend indent for continuation lines
                if !is_first_chunk && indent_char_len > 0 {
                    spans.push(Span::styled(indent_str.to_string(), base_style));
                }

                for (ci, &ch) in chunk_chars.iter().enumerate() {
                    let abs_col = chunk_col_start + ci;
                    let char_selected = is_in_selection(
                        line_idx,
                        abs_col,
                        sel_start_row,
                        sel_start_col,
                        sel_end_row,
                        sel_end_col,
                    );
                    if char_selected != in_sel {
                        if !current.is_empty() {
                            spans.push(Span::styled(
                                current.clone(),
                                if in_sel { sel_style } else { base_style },
                            ));
                            current.clear();
                        }
                        in_sel = char_selected;
                    }
                    current.push(ch);
                }
                if !current.is_empty() {
                    spans.push(Span::styled(
                        current,
                        if in_sel { sel_style } else { base_style },
                    ));
                }
                visual_lines.push(Line::from(spans));
            } else {
                // Prepend indent for continuation lines
                let display_text = if is_first_chunk {
                    chunk.to_string()
                } else {
                    format!("{}{}", indent_str, chunk)
                };
                visual_lines.push(Line::from(Span::styled(
                    display_text,
                    Style::default().fg(theme::text()).bg(background),
                )));
            }

            is_first_chunk = false;
            col_offset += chunk_len;
            remaining = &remaining[chunk_len..];
        }
    }

    // Scroll with a 3-row buffer (scrolloff) between the cursor and the bottom
    // edge of the textarea. The cursor is locked at row `height - 1 - scrolloff`
    // (3 rows of empty space below it) until the cursor is within `scrolloff`
    // of the last visual line, at which point the scroll clamps to max_scroll
    // and the cursor "catches up" to the very bottom. The same lock applies
    // when scrolling back up: as crow decreases, scroll_offset decreases in
    // lock-step so the cursor stays at the same visual row until it falls
    // within `scrolloff` of the first line, where scroll_offset clamps to 0
    // and the cursor visible position equals crow.
    const SCROLLOFF: usize = 3;
    let scroll_offset = if let Some(crow) = cursor_visual_row {
        let height = area.height as usize;
        let total = visual_lines.len();
        if height == 0 || total <= height {
            0u16
        } else {
            let max_scroll = total.saturating_sub(height);
            // Lock cursor `scrolloff` rows above the bottom edge.
            let lock_visible_row = height.saturating_sub(1).saturating_sub(SCROLLOFF);
            let desired = crow.saturating_sub(lock_visible_row);
            desired.min(max_scroll) as u16
        }
    } else {
        0u16
    };

    // For block cursor (normal mode), highlight the character at cursor position
    // by splitting existing spans at the cursor column, preserving their styles.
    if cursor_style == CursorStyle::Block {
        if let (Some(vrow), Some(vcol)) = (cursor_visual_row, cursor_visual_col) {
            if vrow < visual_lines.len() {
                let line = &visual_lines[vrow];
                let block_style = Style::default()
                    .fg(theme::base())
                    .bg(theme::cursor_normal_bg());

                // Flatten spans into per-character (char, style) pairs
                let char_styles: Vec<(char, Style)> = line
                    .spans
                    .iter()
                    .flat_map(|span| span.content.chars().map(move |ch| (ch, span.style)))
                    .collect();

                let col = vcol;

                if col < char_styles.len() {
                    // Rebuild spans preserving styles, overriding only the cursor char
                    let mut spans: Vec<Span<'static>> = Vec::new();
                    let mut current = String::new();
                    let mut current_style =
                        char_styles.first().map(|&(_, s)| s).unwrap_or_default();

                    for (i, &(ch, style)) in char_styles.iter().enumerate() {
                        let effective_style = if i == col { block_style } else { style };
                        if effective_style != current_style {
                            if !current.is_empty() {
                                spans.push(Span::styled(current.clone(), current_style));
                                current.clear();
                            }
                            current_style = effective_style;
                        }
                        current.push(ch);
                    }
                    if !current.is_empty() {
                        spans.push(Span::styled(current, current_style));
                    }
                    visual_lines[vrow] = Line::from(spans);
                } else if col >= width && !char_styles.is_empty() {
                    // Cursor past end of a line that fills the full render width —
                    // an appended space would be clipped by the paragraph renderer.
                    // Highlight the last character instead (vim-correct $ behavior).
                    let last_idx = char_styles.len() - 1;
                    let mut spans: Vec<Span<'static>> = Vec::new();
                    let mut current = String::new();
                    let mut current_style =
                        char_styles.first().map(|&(_, s)| s).unwrap_or_default();

                    for (i, &(ch, style)) in char_styles.iter().enumerate() {
                        let effective_style = if i == last_idx { block_style } else { style };
                        if effective_style != current_style {
                            if !current.is_empty() {
                                spans.push(Span::styled(current.clone(), current_style));
                                current.clear();
                            }
                            current_style = effective_style;
                        }
                        current.push(ch);
                    }
                    if !current.is_empty() {
                        spans.push(Span::styled(current, current_style));
                    }
                    visual_lines[vrow] = Line::from(spans);
                } else {
                    // Cursor past end of line — append a block space, keep existing spans
                    let mut spans: Vec<Span<'static>> = line.spans.iter().cloned().collect();
                    spans.push(Span::styled(" ", block_style));
                    visual_lines[vrow] = Line::from(spans);
                }
            }
        }
    }

    // Place terminal beam cursor (insert mode only).
    // Also render a bg highlight under the cursor char so the color is customizable.
    if cursor_style == CursorStyle::Beam {
        if let (Some(vrow), Some(vcol)) = (cursor_visual_row, cursor_visual_col) {
            let insert_bg = theme::cursor_insert_bg();
            let insert_style = Style::default().bg(insert_bg);
            if vrow < visual_lines.len() {
                let line = &visual_lines[vrow];
                let char_styles: Vec<(char, Style)> = line
                    .spans
                    .iter()
                    .flat_map(|span| span.content.chars().map(move |ch| (ch, span.style)))
                    .collect();
                let col = vcol;
                if col < char_styles.len() {
                    let mut spans: Vec<Span<'static>> = Vec::new();
                    let mut current = String::new();
                    let mut current_style =
                        char_styles.first().map(|&(_, s)| s).unwrap_or_default();
                    for (i, &(ch, style)) in char_styles.iter().enumerate() {
                        let effective_style = if i == col {
                            style.patch(insert_style)
                        } else {
                            style
                        };
                        if effective_style != current_style {
                            if !current.is_empty() {
                                spans.push(Span::styled(current.clone(), current_style));
                                current.clear();
                            }
                            current_style = effective_style;
                        }
                        current.push(ch);
                    }
                    if !current.is_empty() {
                        spans.push(Span::styled(current, current_style));
                    }
                    visual_lines[vrow] = Line::from(spans);
                } else {
                    // Cursor past end of line — append a highlighted space
                    let mut spans: Vec<Span<'static>> = line.spans.iter().cloned().collect();
                    spans.push(Span::styled(" ", insert_style));
                    visual_lines[vrow] = Line::from(spans);
                }
            }
        }
    }

    let paragraph = Paragraph::new(visual_lines)
        .style(Style::default().bg(background))
        .scroll((scroll_offset, 0));
    frame.render_widget(paragraph, area);

    // Place terminal beam cursor position
    if cursor_style == CursorStyle::Beam {
        if let (Some(vrow), Some(vcol)) = (cursor_visual_row, cursor_visual_col) {
            let screen_row = (vrow as u16).saturating_sub(scroll_offset);
            if screen_row < area.height {
                let vcol_u16 = vcol as u16;
                frame.set_cursor_position(Position::new(
                    area.x + vcol_u16.min(area.width.saturating_sub(1)),
                    area.y + screen_row,
                ));
            }
        }
    }
}

/// Check if a (row, col) position is within a selection range.
/// Selection range is normalized: start <= end.
fn is_in_selection(
    row: usize,
    col: usize,
    sel_start_row: usize,
    sel_start_col: usize,
    sel_end_row: usize,
    sel_end_col: usize,
) -> bool {
    // Normalize: ensure start <= end
    let (sr, sc, er, ec) = if (sel_start_row, sel_start_col) <= (sel_end_row, sel_end_col) {
        (sel_start_row, sel_start_col, sel_end_row, sel_end_col)
    } else {
        (sel_end_row, sel_end_col, sel_start_row, sel_start_col)
    };

    if row < sr || row > er {
        return false;
    }
    if row == sr && row == er {
        return col >= sc && col <= ec;
    }
    if row == sr {
        return col >= sc;
    }
    if row == er {
        return col <= ec;
    }
    true
}

/// Convert a character-based column index to a byte offset within a string.
pub(crate) fn byte_offset_of_col(s: &str, col: usize) -> usize {
    s.char_indices()
        .nth(col)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(s.len())
}

/// Render the persistent input bar for a session detail view.
pub fn render_input_bar(
    frame: &mut Frame,
    area: Rect,
    session_id: uuid::Uuid,
    focused: bool,
    app: &App,
) {
    use crate::types::PopupMode;
    use ratatui::layout::{Constraint, Layout};
    use ratatui::style::Modifier;
    use ratatui::widgets::BorderType;

    // No horizontal inset — input bar spans the full session_area width to
    // match the session detail message view.

    let Some(state) = app.sessions.get(&session_id) else {
        return;
    };

    let is_insert = focused && state.input_bar.surface.mode == PopupMode::Insert;

    // T5/Decision 4: adopt the same AccentFocusOnly-vs-FullBorders policy the
    // pane frame already uses (`theme::pane_block`), instead of an
    // unconditionally-colored border regardless of theme/focus.
    let border_color = match theme::active_border_policy() {
        theme::BorderPolicy::FullBorders => theme::session_detail_border(),
        theme::BorderPolicy::AccentFocusOnly => {
            if focused {
                theme::focused_border()
            } else {
                theme::tier_panel()
            }
        }
    };
    let border_style = if is_insert {
        Style::default()
            .fg(border_color)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(border_color)
    };
    // Composer shares transcript surface across every theme, including
    // transparent themes and custom text-area colors.
    let input_bg = detail_surface_bg(&app.settings);

    // Plain (square-corner) border on all four sides; Thick when in Insert mode
    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(border_style)
        .style(Style::default().bg(input_bg));
    let inner = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    // Archived sessions: show read-only indicator instead of normal input bar
    if state.session.status == rsi_common::types::SessionStatus::Archived {
        let archived_line = Line::from(vec![
            Span::styled(
                "  Archived",
                Style::default()
                    .fg(theme::overlay1())
                    .add_modifier(ratatui::style::Modifier::ITALIC),
            ),
            Span::styled("  ·  read-only", Style::default().fg(theme::surface2())),
        ]);
        let v_offset = inner.height / 2;
        let centered = Rect::new(inner.x, inner.y + v_offset, inner.width, 1);
        frame.render_widget(
            Paragraph::new(archived_line).style(Style::default().bg(input_bg)),
            centered,
        );
        return;
    }

    let surface = &state.input_bar.surface;

    // Keep the mode beside the prompt, inside the composer.
    let chunks = Layout::horizontal([
        Constraint::Length(INPUT_PROMPT_WIDTH),
        Constraint::Length(1),
        Constraint::Fill(1), // Textarea / hint
        Constraint::Length(1),
    ])
    .split(inner);

    // While a global leader sequence (e.g. Space or G prefix) is awaiting its
    // continuation in a focused normal-mode input bar, surface that pending
    // state as a distinct positive mode label instead of ordinary NORMAL.
    // All three labels occupy the same six cells, so the fixed prompt-area
    // layout is unaffected. The display clears on its own because
    // `vim_machine_pending` is derived from the same KeyManager dispatch that
    // completes or cancels the sequence.
    let mode = if focused && app.vim_machine_pending && surface.mode == PopupMode::Normal {
        "LEADER"
    } else {
        match surface.mode {
            PopupMode::Normal => "NORMAL",
            PopupMode::Insert => "INSERT",
        }
    };
    let prompt = surface
        .vim_state
        .pending_operator
        .map(|operator| format!("{operator}·"))
        .unwrap_or_else(|| "› ".to_string());
    let prompt_area = chunks[0];
    let v_offset = prompt_area.height / 2;
    let centered_prompt_area = Rect::new(
        prompt_area.x,
        prompt_area.y + v_offset,
        prompt_area.width,
        1,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{mode} {prompt}"),
            Style::default()
                .fg(theme::accent())
                .bg(input_bg)
                .add_modifier(Modifier::BOLD),
        )))
        .style(Style::default().bg(input_bg)),
        centered_prompt_area,
    );

    // Render textarea content with wrapping (visible in both modes — shows draft when in normal)
    let has_content = surface.has_content();
    let cursor_style = if !focused {
        CursorStyle::Hidden
    } else if surface.mode == PopupMode::Insert {
        CursorStyle::Beam
    } else {
        CursorStyle::Block
    };

    if surface.correction_in_flight && !(surface.mode == PopupMode::Insert || has_content) {
        // Show in-flight badge in the textarea area when there's nothing else to render there
        let badge_line = Line::from(Span::styled(
            "Compiling…",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ));
        let v_offset = chunks[2].height / 2;
        let badge_area = Rect::new(chunks[2].x, chunks[2].y + v_offset, chunks[2].width, 1);
        frame.render_widget(
            Paragraph::new(badge_line).style(Style::default().bg(input_bg)),
            badge_area,
        );
    } else if has_content {
        let visual_sel = if surface.vim_state.visual.is_some() {
            surface.textarea.selection_range()
        } else {
            None
        };
        surface.wrap_width.set(chunks[2].width as usize);
        render_wrapped_textarea(
            frame,
            chunks[2],
            &surface.textarea,
            cursor_style,
            visual_sel,
            input_bg,
        );
    } else {
        let hint = Line::from(Span::styled(
            "Type your message…",
            Style::default().fg(theme::subtext0()).bg(input_bg),
        ));
        let v_offset = chunks[2].height / 2;
        let hint_area = Rect::new(chunks[2].x, chunks[2].y + v_offset, chunks[2].width, 1);
        frame.render_widget(
            Paragraph::new(hint).style(Style::default().bg(input_bg)),
            hint_area,
        );
    }

    // Render correction preview above the input bar (if available)
    if surface.corrected_preview.is_some() {
        render_input_bar_correction_preview(
            frame,
            area, // inset area (aligns with narrower bar)
            surface.corrected_preview.as_deref().unwrap(),
        );
    } else if surface.suggestions_visible && !surface.filtered_indices.is_empty() {
        // Render suggestion dropdown above the input bar (if visible and no correction preview)
        render_input_bar_suggestions(
            frame,
            area, // inset area (dropdown aligns with narrower bar)
            &app.available_commands,
            &surface.file_paths,
            &surface.filtered_indices,
            surface.selected_suggestion,
            surface.suggestion_mode,
        );
    }
}

/// Render the suggestion dropdown above the input bar.
///
/// Supports both command mode (`/{name}  {description}`) and file mode (`@{path}`).
fn render_input_bar_suggestions(
    frame: &mut Frame,
    input_bar_area: Rect,
    available_commands: &[crate::suggestions::CommandSuggestion],
    file_paths: &[String],
    filtered_indices: &[usize],
    selected_suggestion: usize,
    mode: crate::suggestions::SuggestionMode,
) {
    let max_visible: usize = match mode {
        crate::suggestions::SuggestionMode::File => 7,
        _ => 8,
    };

    let total = filtered_indices.len();
    let visible_count = total.min(max_visible);

    // Compute scroll offset to keep selected item visible
    let scroll_offset = if selected_suggestion >= max_visible {
        selected_suggestion - max_visible + 1
    } else {
        0
    };

    let dropdown_height = visible_count as u16;

    // Position above the input bar
    let dropdown_y = input_bar_area.y.saturating_sub(dropdown_height);
    if dropdown_y == 0 && dropdown_height > input_bar_area.y {
        return; // Not enough room above
    }

    let dropdown_area = Rect::new(
        input_bar_area.x,
        dropdown_y,
        input_bar_area.width,
        dropdown_height,
    );

    // Clear the dropdown area
    frame.render_widget(Clear, dropdown_area);

    // Build list items
    let items: Vec<ListItem> = filtered_indices
        .iter()
        .skip(scroll_offset)
        .take(max_visible)
        .enumerate()
        .map(|(visible_idx, &item_idx)| {
            let is_selected = visible_idx + scroll_offset == selected_suggestion;

            let (fg, bg) = if is_selected {
                (
                    theme::suggestion_selected_fg(),
                    theme::suggestion_selected_bg(),
                )
            } else {
                (theme::suggestion_fg(), theme::suggestion_bg())
            };

            match mode {
                crate::suggestions::SuggestionMode::File => {
                    let path = &file_paths[item_idx];
                    let display = format!("@{path}");
                    let max_w = input_bar_area.width as usize;
                    let text = if display.len() > max_w {
                        let end = floor_char_boundary(&display, max_w);
                        &display[..end]
                    } else {
                        &display
                    };
                    let span = Span::styled(
                        text.to_string(),
                        Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
                    );
                    let remaining = max_w.saturating_sub(text.len());
                    let line = Line::from(vec![
                        span,
                        Span::styled(" ".repeat(remaining), Style::default().bg(bg)),
                    ]);
                    ListItem::new(line)
                }
                _ => {
                    let cmd = &available_commands[item_idx];
                    let name_span = Span::styled(
                        format!("/{}", cmd.name),
                        Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
                    );

                    let name_width = cmd.name.len() + 1;
                    let padding = 2;
                    let desc_width =
                        (input_bar_area.width as usize).saturating_sub(name_width + padding);
                    let desc_text: String = if desc_width > 0 {
                        let d = &cmd.description;
                        if d.len() > desc_width {
                            let end = floor_char_boundary(d, desc_width);
                            d[..end].to_string()
                        } else {
                            d.clone()
                        }
                    } else {
                        String::new()
                    };

                    let line = Line::from(vec![
                        name_span,
                        Span::styled(
                            format!("{:>pad$}", "", pad = padding),
                            Style::default().bg(bg),
                        ),
                        Span::styled(
                            desc_text,
                            Style::default().fg(theme::suggestion_desc()).bg(bg),
                        ),
                    ]);

                    ListItem::new(line)
                }
            }
        })
        .collect();

    let list = List::new(items).style(Style::default().bg(theme::suggestion_bg()));

    frame.render_widget(list, dropdown_area);
}

/// Render the correction preview panel above the input bar.
/// Shows corrected/clarified text with accept/discard hints.
fn render_input_bar_correction_preview(frame: &mut Frame, input_bar_area: Rect, preview: &str) {
    let is_clarify = preview.starts_with("CLARIFY:");

    // Compute height: content lines + 2 for border, capped at 8 + 2 = 10
    let content_lines = preview.lines().count().max(1);
    let inner_height = (content_lines as u16).min(8);
    // Reserve 1 line for the hint row inside the block
    let block_height = inner_height + 1 + 2; // content + hint + top/bottom borders

    // Position above the input bar
    let preview_y = input_bar_area.y.saturating_sub(block_height);
    if preview_y == 0 && block_height > input_bar_area.y {
        return; // Not enough room above
    }

    let preview_area = Rect::new(
        input_bar_area.x,
        preview_y,
        input_bar_area.width,
        block_height,
    );

    frame.render_widget(Clear, preview_area);

    let (title_text, title_color) = if is_clarify {
        (" clarify ", theme::peach())
    } else {
        (" compiled ", theme::accent())
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::overlay_border()))
        .title(Span::styled(
            title_text,
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::ITALIC),
        ))
        .style(Style::default().bg(theme::overlay_bg()));

    let inner = block.inner(preview_area);
    frame.render_widget(block, preview_area);

    // Split inner into content area and hint line
    let content_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    let hint_area = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );

    // Render preview text
    frame.render_widget(
        Paragraph::new(preview)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(theme::text()).bg(theme::overlay_bg())),
        content_area,
    );

    // Render hint line
    let hint_line = if is_clarify {
        Line::from(vec![
            Span::styled(
                "d",
                Style::default()
                    .fg(theme::overlay_hint())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": dismiss  ", Style::default().fg(theme::overlay_hint())),
            Span::styled(
                "Ctrl+Y",
                Style::default()
                    .fg(theme::overlay_hint())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                ": retry after editing",
                Style::default().fg(theme::overlay_hint()),
            ),
        ])
    } else {
        Line::from(vec![
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
}

/// Render the rainbow chevron loading bar for active sessions.
///
/// Large chevron shapes span the full bar height, formed by diagonal stripes.
/// Two sets of chevrons travel in opposite directions, overlapping via
/// fg/bg color blending to create a layered, psychedelic effect.
pub fn render_loading_bar(frame: &mut Frame, area: Rect) {
    let width = area.width as usize;
    let height = area.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let palette = theme::rainbow_palette();
    let plen = palette.len();
    let millis = chrono::Utc::now().timestamp_millis();

    // Each chevron is a > shape spanning `height` rows.
    // The chevron period (horizontal repeat) = height * 4 gives wide chevrons.
    let period = height * 4;
    let mid = height / 2; // middle row = the chevron's point

    // Reduce tick to within [0, period) to prevent overflow in subtraction math.
    // All modular arithmetic stays correct since we only care about (tick % period).
    let tick = (millis / 60) as usize % period;

    let mut lines: Vec<Line> = Vec::with_capacity(height);

    for row in 0..height {
        let mut spans: Vec<Span> = Vec::with_capacity(width);

        // Distance from center row — forms the V shape of the chevron.
        let arm = mid.abs_diff(row);

        for col in 0..width {
            // Left-moving chevron (> shape): travels left over time.
            // The arm offset creates the V shape pointing right.
            let right_diag = (period * 2 + col + arm * 2 - tick) % period;
            let right_color_idx = (right_diag * plen / period) % plen;

            // Right-moving chevron (< shape): travels right over time.
            // Subtracting arm*2 mirrors the V so these point left,
            // facing the left-moving chevrons — they converge toward each other.
            let left_diag = (col + tick + period * 2 - arm * 2) % period;
            let left_color_idx = (left_diag * plen / period) % plen;

            let right_color = palette[right_color_idx];
            let left_color = palette[left_color_idx];

            // Determine which chevron is "in front" at this cell.
            // Use the right-moving diagonal to create large alternating bands
            // where one chevron set is fg and the other is bg.
            let band = right_diag * 2 / period; // 0 or 1
            let (ch, fg, bg) = if band == 0 {
                ('\u{2580}', right_color, left_color) // ▀ right chevron in front
            } else {
                ('\u{2580}', left_color, right_color) // ▀ left chevron in front
            };

            spans.push(Span::styled(ch.to_string(), Style::default().fg(fg).bg(bg)));
        }

        lines.push(Line::from(spans));
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, area);
}

/// Render the formulating event (last visible event) at a fixed bottom position
/// with `current_height` rows, growing from the classic loader height to `target_height`.
///
/// Phase 2: renders the bubble shape only (no crossfade — Phase 3 adds that).
/// The bubble is anchored to the bottom of `inner` so it appears to grow upward
/// from where the loading container sat.
/// Re-colour every character in `lines` with a cycling rainbow palette.
///
/// `tick` is a time-based phase offset (from `timestamp_millis() / 60`) so the
/// colours scroll left-to-right each render frame, giving a flowing rather than
/// static rainbow. Characters wrap through the 14-colour palette so all colours
/// appear as densely as possible regardless of message length.
fn apply_rainbow_text(
    lines: Vec<Line<'static>>,
    tick: usize,
    palette: &[Color],
) -> Vec<Line<'static>> {
    if palette.is_empty() {
        return lines;
    }
    let plen = palette.len();
    let mut char_idx = 0usize;
    let mut result = Vec::with_capacity(lines.len());
    for line in lines {
        let mut new_spans: Vec<Span<'static>> = Vec::new();
        for span in line.spans {
            for ch in span.content.chars() {
                let color = palette[char_idx.wrapping_add(tick) % plen];
                char_idx = char_idx.wrapping_add(1);
                new_spans.push(Span::styled(ch.to_string(), Style::default().fg(color)));
            }
        }
        result.push(Line::from(new_spans));
    }
    result
}

fn render_formulating_event(
    frame: &mut Frame,
    inner: Rect,
    state: &crate::types::SessionState,
    event_idx: usize,
    current_height: u16,
    progress: f32,
) {
    let Some(event) = state.events.get(event_idx) else {
        return;
    };
    if inner.height < current_height || current_height == 0 {
        return;
    }

    // Anchor to the bottom of inner (same y position the loading container occupied).
    let bubble_y = inner.y + inner.height - current_height;
    let event_rect = Rect::new(inner.x, bubble_y, inner.width, current_height);

    // Choose border style matching the event type.
    let is_tool_event = matches!(
        event.event_type,
        rsi_common::types::EventType::ToolUse | rsi_common::types::EventType::ToolResult
    );

    // Blend border color from tool-call-grey toward role border as progress increases.
    let target_border_color = if is_tool_event {
        theme::tool_call_border()
    } else {
        match event.role {
            Some(rsi_common::types::Role::User) => theme::user_message_border(),
            _ => theme::assistant_message_border(),
        }
    };
    let start_border_color = theme::tool_call_border();
    let border_color = theme::lerp_color(start_border_color, target_border_color, progress);

    // Border set: switch from dashed to solid at 50%.
    let block = if progress < 0.5 {
        Block::default()
            .borders(Borders::ALL)
            .border_set(TOOL_CALL_BORDER)
            .border_style(Style::default().fg(border_color))
    } else {
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
    };

    let para_rect = block.inner(event_rect);
    frame.render_widget(block, event_rect);

    // Render event content if we have interior space.
    if para_rect.height > 0 && para_rect.width > 0 {
        let render_width = para_rect.width;
        let lines = if let Some(entry) =
            crate::ui::height::cached_render_event(state, event_idx, render_width, false)
        {
            entry.lines.clone()
        } else {
            let is_collapsed = crate::ui::height::is_event_effectively_collapsed(state, event);
            let model_name = content::model_for_sequence(
                &state.model_segments,
                event.sequence,
                state.session.model.as_deref(),
            );
            let ctx = content::EventRenderContext {
                is_collapsed,
                is_expanded: state.expanded_events.contains(&event.sequence),
                is_cursor: false,
                is_last_event: true,
                model_name,
                pipeline_commands: state.docregblock_contents.clone(),
                max_width: render_width,
            };
            content::build_event_lines(event, &ctx)
        };
        // Rainbow per-character text: every letter gets a different palette color,
        // cycling through the full 14-color rainbow. The time tick shifts the
        // phase each frame so the colors flow visually (not static). The bubble
        // content is legible from frame 1 — no chevron overlay, no 50% gate.
        let palette = theme::rainbow_palette();
        let tick = (chrono::Utc::now().timestamp_millis() / 60) as usize;
        let rainbow_lines = apply_rainbow_text(lines, tick, &palette);
        frame.render_widget(Paragraph::new(rainbow_lines), para_rect);
    }
}

/// Render a horizontally-flowing rainbow strip into `area`.
///
/// Uses the same time-tick source as `render_loading_bar` so they stay
/// phase-coherent if both appear on screen simultaneously during a transition.
/// Designed for a 1-row interior but works with any height > 0.
pub fn render_loading_strip(frame: &mut Frame, area: Rect) {
    let width = area.width as usize;
    let height = area.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let palette = theme::rainbow_palette();
    let plen = palette.len();
    let millis = chrono::Utc::now().timestamp_millis();
    let tick = (millis / 60) as usize;

    let mut lines: Vec<Line> = Vec::with_capacity(height);
    for row in 0..height {
        let mut spans: Vec<Span> = Vec::with_capacity(width);
        for col in 0..width {
            // Slant factor per row gives a subtle diagonal even for 1-row strips.
            let phase = (col + tick + row * 2) % plen;
            let bg_phase = (col + tick + row * 2 + plen / 2) % plen;
            let fg = palette[phase];
            let bg = palette[bg_phase];
            spans.push(Span::styled(
                "\u{2580}".to_string(), // ▀
                Style::default().fg(fg).bg(bg),
            ));
        }
        lines.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// Render the 3-row classic rainbow loading indicator for active sessions.
///
/// The animated strip fills its reserved area without a surrounding box.
pub fn render_loading_container(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    render_loading_strip(frame, area);
}

/// Height reserved by the selected active-session indicator.
pub const fn activity_indicator_height(style: ActivityIndicatorStyle) -> u16 {
    match style {
        ActivityIndicatorStyle::Semantic => 1,
        ActivityIndicatorStyle::RainbowClassic => LOADING_CONTAINER_HEIGHT,
        ActivityIndicatorStyle::RainbowCompact => COMPACT_RAINBOW_HEIGHT,
    }
}

/// Render the selected active-session indicator.
pub fn render_activity_indicator(
    frame: &mut Frame,
    area: Rect,
    state: &SessionState,
    style: ActivityIndicatorStyle,
) {
    match style {
        ActivityIndicatorStyle::Semantic => render_semantic_activity_indicator(frame, area, state),
        ActivityIndicatorStyle::RainbowClassic => render_loading_container(frame, area),
        ActivityIndicatorStyle::RainbowCompact => render_compact_rainbow_container(frame, area),
    }
}

fn render_semantic_activity_indicator(frame: &mut Frame, area: Rect, state: &SessionState) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let tool_count = state
        .events
        .iter()
        .rev()
        .take_while(|event| {
            !(event.event_type == EventType::Message && event.role == Some(Role::User))
        })
        .filter(|event| event.event_type == EventType::ToolUse)
        .count();

    let mut parts = vec!["working".to_string()];
    if tool_count > 0 {
        parts.push(format!(
            "{tool_count} {}",
            if tool_count == 1 { "tool" } else { "tools" }
        ));
    }
    if let Some(work_time_ms) = state.session.work_time_ms {
        let seconds = work_time_ms / 1_000;
        let elapsed = if seconds >= 3_600 {
            format!("{}h {}m", seconds / 3_600, (seconds % 3_600) / 60)
        } else if seconds >= 60 {
            format!("{}m {}s", seconds / 60, seconds % 60)
        } else {
            format!("{seconds}s")
        };
        parts.push(elapsed);
    }

    let line = Line::from(vec![
        Span::styled(
            format!("{} ", animated_status_icon(state.session.status)),
            Style::default().fg(theme::accent()),
        ),
        Span::styled(parts.join("  ·  "), Style::default().fg(theme::subtext0())),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_compact_rainbow_container(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let palette = theme::rainbow_palette();
    let tick = (chrono::Utc::now().timestamp_millis() / 90) as usize;
    let lines = (0..area.height as usize)
        .map(|row| {
            let spans = (0..area.width as usize)
                .map(|column| {
                    Span::styled(
                        "█",
                        Style::default().fg(palette[((column / 2) + row + tick) % palette.len()]),
                    )
                })
                .collect::<Vec<_>>();
            Line::from(spans)
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use rsi_common::types::{ConversationEvent, Role};

    struct RenderedContainerRow {
        text: String,
        status_glyph: String,
        status_color: Color,
        suffix_color: Option<Color>,
    }

    fn render_container_row_for_test(
        row: &crate::types::row::SessionRowViewModel,
        session: &Session,
        settings: &crate::settings::UserSettings,
        width: u16,
        suffix: Option<&str>,
    ) -> Result<RenderedContainerRow, Box<dyn std::error::Error>> {
        let status_x =
            navigator_layout::resolve(usize::from(width), 2, settings.navigator_preset, None)
                .columns
                .iter()
                .find(|cell| cell.column == navigator_layout::NavigatorColumn::Status)
                .ok_or_else(|| std::io::Error::other("status column"))?
                .start;
        let status_x = u16::try_from(status_x)?;
        let mut terminal = Terminal::new(TestBackend::new(width, 1))?;
        terminal.draw(|frame| {
            render_navigator_row(
                frame,
                frame.area(),
                row,
                Some(session),
                settings,
                2,
                false,
                false,
                false,
                theme::blue(),
                theme::tier_panel(),
            );
        })?;
        let buffer = terminal.backend().buffer();
        Ok(RenderedContainerRow {
            text: buffer_text(buffer),
            status_glyph: buffer[(status_x, 0)].symbol().to_string(),
            status_color: buffer[(status_x, 0)].fg,
            suffix_color: suffix.and_then(|value| {
                buffer_text_position(buffer, value).map(|(x, y)| buffer[(x, y)].fg)
            }),
        })
    }

    #[test]
    fn container_function_degrades_counts_before_title() {
        let mut row = crate::types::row::SessionRowViewModel::placeholder(uuid::Uuid::new_v4());
        row.session_kind = SessionKind::Group;
        row.display_title = "Group Alpha".into();
        row.container_counts = Some(crate::types::row::ContainerCounts {
            running_epics: 12,
            running_agents: 140,
        });
        for width in [7, 12, 22, 50] {
            let (title, suffix) = container_function_parts(&row, width);
            assert!(title.starts_with("Group"), "width {width}: {title}");
            let combined = format!("{title}{}", suffix.unwrap_or_default());
            let expected = match width {
                12 => "140A",
                22 => "12E·140A",
                50 => "12 epics · 140 agents",
                _ => "Group",
            };
            assert!(combined.contains(expected), "width {width}: {combined}");
        }
        row.session_kind = SessionKind::Epic;
        row.display_title = "Epic Beta".into();
        row.container_counts = Some(crate::types::row::ContainerCounts {
            running_epics: 0,
            running_agents: 7,
        });
        let (title, suffix) = container_function_parts(&row, 50);
        assert_eq!(title, "Epic Beta");
        assert_eq!(suffix.as_deref(), Some("  7 agents"));
    }

    #[test]
    fn direct_group_leaf_renders_running_indicator_and_degraded_counts()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::app::app_test_helpers::baseline_session;

        let _pinned_theme = crate::ui::theme::pin_theme_state();
        let settings = crate::settings::UserSettings::default();
        for leaf_status in [SessionStatus::Running, SessionStatus::Starting] {
            let group_id = uuid::Uuid::new_v4();
            let leaf_id = uuid::Uuid::new_v4();
            let mut group = baseline_session(group_id, SessionKind::Group);
            group.title = Some("Group Alpha".into());
            group.status = SessionStatus::Completed;
            let mut leaf = baseline_session(leaf_id, SessionKind::Standard);
            leaf.parent_id = Some(group_id);
            leaf.status = leaf_status;
            let sessions = HashMap::from([
                (group_id, SessionState::new(group)),
                (leaf_id, SessionState::new(leaf)),
            ]);
            let focus = crate::types::compute_session_focus_index(&sessions, chrono::Utc::now());
            let row = crate::types::row::compute_session_row_for_state_with_focus(
                &sessions[&group_id],
                &sessions,
                &[],
                &settings,
                focus.get(&group_id),
            );
            assert_eq!(row.status_icon, "◉");
            assert_eq!(
                row.container_counts.map(|counts| counts.running_epics),
                Some(0)
            );
            for (width, title, count, override_counts) in [
                (20, "Group", "1A", None),
                (120, "Group Alpha", "0 epics · 1 agent", None),
                (
                    20,
                    "Group",
                    "140A",
                    Some(crate::types::row::ContainerCounts {
                        running_epics: 12,
                        running_agents: 140,
                    }),
                ),
            ] {
                let mut render_row = row.clone();
                if let Some(counts) = override_counts {
                    render_row.container_counts = Some(counts);
                }
                let rendered = render_container_row_for_test(
                    &render_row,
                    &sessions[&group_id].session,
                    &settings,
                    width,
                    Some(count),
                )?;
                assert!(
                    rendered.text.contains(title),
                    "width {width}: {}",
                    rendered.text
                );
                assert!(
                    rendered.text.contains(count),
                    "width {width}: {}",
                    rendered.text
                );
                assert_eq!(rendered.status_glyph, "◉");
                assert_eq!(rendered.status_color, theme::status_running());
                assert_eq!(rendered.suffix_color, Some(theme::dim_metadata()));
            }
        }
        Ok(())
    }

    #[test]
    fn container_rows_render_titles_running_counts_and_lifecycle_change()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::app::app_test_helpers::baseline_session;

        let _pinned_theme = crate::ui::theme::pin_theme_state();
        let settings = crate::settings::UserSettings::default();
        let group_id = uuid::Uuid::new_v4();
        let epic_id = uuid::Uuid::new_v4();
        let leaf_id = uuid::Uuid::new_v4();
        let mut group = baseline_session(group_id, SessionKind::Group);
        group.title = Some("Group Alpha".into());
        group.status = SessionStatus::Completed;
        let mut epic = baseline_session(epic_id, SessionKind::Epic);
        epic.title = Some("Epic Beta".into());
        epic.status = SessionStatus::Completed;
        epic.parent_id = Some(group_id);
        let mut leaf = baseline_session(leaf_id, SessionKind::Task);
        leaf.parent_id = Some(epic_id);
        leaf.status = SessionStatus::Running;
        let mut sessions = HashMap::from([
            (group_id, SessionState::new(group)),
            (epic_id, SessionState::new(epic)),
            (leaf_id, SessionState::new(leaf)),
        ]);
        for (id, title, counts, icon) in [
            (group_id, "Group Alpha", "1 epic · 1 agent", "◉"),
            (epic_id, "Epic Beta", "1 agent", "◉"),
        ] {
            let focus = crate::types::compute_session_focus_index(&sessions, chrono::Utc::now());
            let row = crate::types::row::compute_session_row_for_state_with_focus(
                &sessions[&id],
                &sessions,
                &[],
                &settings,
                focus.get(&id),
            );
            let rendered = render_container_row_for_test(
                &row,
                &sessions[&id].session,
                &settings,
                120,
                Some(counts),
            )?;
            assert!(rendered.text.contains(title), "title: {}", rendered.text);
            assert!(rendered.text.contains(counts), "counts: {}", rendered.text);
            assert_eq!(rendered.suffix_color, Some(theme::dim_metadata()));
            assert_eq!(rendered.status_glyph, icon);
            assert_eq!(rendered.status_color, theme::status_running());
        }

        sessions
            .get_mut(&leaf_id)
            .ok_or_else(|| std::io::Error::other("running leaf"))?
            .session
            .status = SessionStatus::Completed;
        for (id, title) in [(group_id, "Group Alpha"), (epic_id, "Epic Beta")] {
            let focus = crate::types::compute_session_focus_index(&sessions, chrono::Utc::now());
            let row = crate::types::row::compute_session_row_for_state_with_focus(
                &sessions[&id],
                &sessions,
                &[],
                &settings,
                focus.get(&id),
            );
            let rendered =
                render_container_row_for_test(&row, &sessions[&id].session, &settings, 120, None)?;
            assert!(rendered.text.contains(title));
            assert_eq!(rendered.status_glyph, "✓");
            assert_eq!(
                row.container_counts.map(|counts| counts.running_agents),
                Some(0)
            );
        }
        Ok(())
    }

    #[test]
    fn scope_breadcrumb_delegates_to_canonical_session_identity() {
        use crate::app::app_test_helpers::baseline_session;
        use rsi_common::types::SessionKind;

        let epic_id = uuid::Uuid::new_v4();
        let child_id = uuid::Uuid::new_v4();
        let mut epic = baseline_session(epic_id, SessionKind::Epic);
        epic.lead_session_id = Some(child_id);
        let mut child = baseline_session(child_id, SessionKind::Task);
        child.parent_id = Some(epic_id);
        child.title = Some("raw breadcrumb title".into());
        child.agent_role = Some("Reviewer".into());
        child.epic_spawn_ordinal = Some(9);
        let sessions = HashMap::from([
            (epic_id, SessionState::new(epic)),
            (child_id, SessionState::new(child)),
        ]);

        assert_eq!(scope_breadcrumb_title(child_id, &sessions), "Demiurge");
        assert_eq!(
            scope_breadcrumb_title(uuid::Uuid::new_v4(), &sessions),
            "Unknown"
        );
    }

    #[test]
    fn session_browser_layout_matches_exact_acceptance_canvases() {
        let at_120 = resolve_session_browser_layout(
            Rect::new(0, 1, 120, 39),
            SessionListSurface::Full,
            crate::types::SessionListZone::Main,
        );
        assert_eq!(at_120.mode, SessionBrowserMode::OperationalTable);
        assert_eq!(at_120.selected_signal, Some(Rect::new(2, 1, 116, 2)));
        assert_eq!(at_120.navigator, Rect::new(2, 4, 116, 36));

        let at_200 = resolve_session_browser_layout(
            Rect::new(0, 1, 200, 57),
            SessionListSurface::Full,
            crate::types::SessionListZone::Main,
        );
        assert_eq!(at_200.mode, SessionBrowserMode::Relay);
        assert_eq!(at_200.navigator, Rect::new(13, 1, 100, 57));
        assert_eq!(at_200.tether, Some(Rect::new(113, 1, 2, 57)));
        assert_eq!(at_200.inspector, Some(Rect::new(115, 1, 72, 57)));
        assert_eq!(at_200.activity, None);

        let at_240 = resolve_session_browser_layout(
            Rect::new(0, 1, 240, 69),
            SessionListSurface::Full,
            crate::types::SessionListZone::Main,
        );
        assert_eq!(at_240.mode, SessionBrowserMode::RelayWithActivity);
        assert_eq!(at_240.navigator, Rect::new(4, 1, 100, 69));
        assert_eq!(at_240.tether, Some(Rect::new(104, 1, 2, 69)));
        assert_eq!(at_240.inspector, Some(Rect::new(106, 1, 72, 69)));
        assert_eq!(at_240.activity, Some(Rect::new(180, 1, 56, 69)));
    }

    #[test]
    fn session_browser_layout_covers_breakpoints_and_caps() {
        let widths = [119, 120, 173, 174, 199, 200, 227, 228, 239, 240, 300];
        let modes = [
            SessionBrowserMode::LegacyTable,
            SessionBrowserMode::OperationalTable,
            SessionBrowserMode::OperationalTable,
            SessionBrowserMode::Relay,
            SessionBrowserMode::Relay,
            SessionBrowserMode::Relay,
            SessionBrowserMode::Relay,
            SessionBrowserMode::RelayWithActivity,
            SessionBrowserMode::RelayWithActivity,
            SessionBrowserMode::RelayWithActivity,
            SessionBrowserMode::RelayWithActivity,
        ];
        for (width, expected_mode) in widths.into_iter().zip(modes) {
            let layout = resolve_session_browser_layout(
                Rect::new(0, 0, width, 40),
                SessionListSurface::Full,
                crate::types::SessionListZone::Main,
            );
            assert_eq!(layout.mode, expected_mode, "width {width}");
            let mut rects = vec![layout.navigator];
            rects.extend(layout.inspector);
            rects.extend(layout.activity);
            for pair in rects.windows(2) {
                assert!(
                    pair[0].x + pair[0].width <= pair[1].x,
                    "overlap at width {width}: {pair:?}"
                );
            }
            assert!(
                rects.last().unwrap().x + rects.last().unwrap().width <= width,
                "out of bounds at width {width}"
            );
        }

        let ultra = resolve_session_browser_layout(
            Rect::new(0, 0, 300, 40),
            SessionListSurface::Full,
            crate::types::SessionListZone::Main,
        );
        assert_eq!(ultra.navigator.width, NAVIGATOR_MAX_WIDTH);
        assert_eq!(ultra.inspector.unwrap().width, INSPECTOR_MAX_WIDTH);
        assert_eq!(ultra.activity.unwrap().width, ACTIVITY_MAX_WIDTH);
        assert_eq!(ultra.navigator.x, 34);
    }

    #[test]
    fn session_browser_layout_preserves_low_height_detail_and_non_main_paths() {
        let low = resolve_session_browser_layout(
            Rect::new(0, 0, 240, 23),
            SessionListSurface::Full,
            crate::types::SessionListZone::Main,
        );
        assert_eq!(low.mode, SessionBrowserMode::OperationalTable);
        assert_eq!(low.selected_signal, None);
        assert_eq!(low.navigator, Rect::new(42, 0, 156, 23));

        for surface in [SessionListSurface::Full, SessionListSurface::Embedded] {
            for zone in [
                crate::types::SessionListZone::TaskRabbit,
                crate::types::SessionListZone::Archive,
                crate::types::SessionListZone::Jobs,
            ] {
                let layout =
                    resolve_session_browser_layout(Rect::new(4, 7, 240, 40), surface, zone);
                assert_eq!(layout.mode, SessionBrowserMode::LegacyTable);
                assert_eq!(layout.navigator, Rect::new(4, 7, 240, 40));
            }
        }
        let embedded = resolve_session_browser_layout(
            Rect::new(4, 7, 240, 40),
            SessionListSurface::Embedded,
            crate::types::SessionListZone::Main,
        );
        assert_eq!(embedded.mode, SessionBrowserMode::EmbeddedInspector);
        assert_eq!(embedded.navigator, Rect::new(4, 7, 240, 32));
        assert_eq!(embedded.embedded_inspector, Some(Rect::new(4, 40, 240, 7)));
    }

    #[test]
    fn embedded_browser_bounds_the_list_and_assigns_remaining_height_to_selected_details() {
        let layout = resolve_session_browser_layout(
            Rect::new(0, 1, 64, 67),
            SessionListSurface::Embedded,
            crate::types::SessionListZone::Main,
        );
        assert_eq!(layout.mode, SessionBrowserMode::EmbeddedInspector);
        assert_eq!(layout.navigator, Rect::new(0, 1, 64, 32));
        assert_eq!(layout.embedded_inspector, Some(Rect::new(0, 34, 64, 34)));

        let short = resolve_session_browser_layout(
            Rect::new(0, 1, 64, 24),
            SessionListSurface::Embedded,
            crate::types::SessionListZone::Main,
        );
        assert_eq!(short.mode, SessionBrowserMode::EmbeddedInspector);
        assert_eq!(short.navigator, Rect::new(0, 1, 64, 16));
        assert_eq!(short.embedded_inspector, Some(Rect::new(0, 18, 64, 7)));
    }

    #[test]
    fn actual_effort_indicator_never_invents_a_default() {
        assert_eq!(
            actual_effort_indicator(Some("high"), Some("gpt-6-astra")),
            Some("high  ▮▮▮▯▯▯".to_string())
        );
        assert_eq!(
            actual_effort_indicator(Some("future"), Some("gpt-6-astra")),
            Some("future".to_string())
        );
        assert_eq!(
            actual_effort_indicator(Some("high"), Some("gpt-future-custom")),
            Some("high".to_string())
        );
        assert_eq!(actual_effort_indicator(None, Some("gpt-6-astra")), None);
        assert_eq!(
            actual_effort_indicator(Some("0"), None),
            Some("0".to_string())
        );
    }

    #[test]
    fn t19_rendered_rows_keep_later_offsets_for_ascii_cjk_emoji_and_combining_titles() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use crate::app::app_test_helpers::baseline_session;
        use ratatui::{Terminal, backend::TestBackend};
        use rsi_common::types::{SessionKind, SessionProvider};

        let settings = crate::settings::UserSettings::default();
        let layout = navigator_layout::resolve(120, 2, settings.navigator_preset, None);
        let context = layout
            .columns
            .iter()
            .find(|cell| cell.column == navigator_layout::NavigatorColumn::Context)
            .expect("context column");
        let effort = layout
            .columns
            .iter()
            .find(|cell| cell.column == navigator_layout::NavigatorColumn::Effort)
            .expect("effort column");
        let mut source = baseline_session(uuid::Uuid::new_v4(), SessionKind::Task);
        source.provider = SessionProvider::Codex;
        source.model = Some("gpt-6-astra".to_string());
        source.effort = Some("high".to_string());
        for title in ["ascii title", "界界 title", "🦀 title", "e\u{301} title"] {
            let mut row = crate::types::row::SessionRowViewModel::placeholder(uuid::Uuid::new_v4());
            row.display_title = title.to_string();
            row.context_pct = crate::types::row::ContextCell::Display {
                label: "42%".to_string(),
                pct: Some(42.0),
            };
            row.turns_text = Some("7".to_string());
            let mut terminal = Terminal::new(TestBackend::new(120, 1)).expect("terminal");
            terminal
                .draw(|frame| {
                    render_navigator_row(
                        frame,
                        Rect::new(0, 0, 120, 1),
                        &row,
                        Some(&source),
                        &settings,
                        2,
                        false,
                        false,
                        false,
                        theme::blue(),
                        theme::tier_panel(),
                    );
                })
                .expect("render row");
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer.area.width, 120);
            let text = buffer_text(buffer);
            let provider = layout
                .columns
                .iter()
                .find(|cell| cell.column == navigator_layout::NavigatorColumn::Provider)
                .expect("provider column");
            assert_eq!(
                buffer[(provider.start as u16, 0)].symbol(),
                glyphs::provider_glyph(SessionProvider::Codex),
                "provider is a separate glyph cell: {text}"
            );
            assert!(
                text.contains("6-astra"),
                "model keeps its versioned canonical suffix: {text}"
            );
            let (high_glyph, _) =
                glyphs::effort_glyph(Some("high"), Some("gpt-6-astra")).expect("known effort");
            assert_eq!(
                buffer[(effort.start as u16, 0)].symbol(),
                high_glyph,
                "actual effort is a separate gauge cell: {text}"
            );
            assert_eq!(effort.start + effort.width, 120);
            assert_eq!(
                buffer_text_position(buffer, "42%").map(|(x, _)| x as usize),
                Some(context.start + context.width - 3),
                "later context offset changed for {title:?}"
            );
        }
    }

    #[test]
    fn embedded_selected_panel_preserves_full_execution_identity_and_actual_zeroes() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use crate::app::app_test_helpers::with_session_list;
        use crate::types::SplitNode;
        use ratatui::{Terminal, backend::TestBackend};
        use rsi_common::types::SessionProvider;

        let mut app = with_session_list(35);
        let selected_id = app.filtered_session_order[0];
        let selected = app
            .sessions
            .get_mut(&selected_id)
            .expect("selected session");
        selected.session.title = Some("Model identity probe".to_string());
        selected.session.provider = SessionProvider::OpenRouter;
        selected.session.model = Some("openrouter/anthropic/claude-opus-4-6".to_string());
        selected.session.effort = Some("high".to_string());
        selected.session.num_turns = Some(0);
        selected.session.context_fill_pct = Some(0.0);
        let pane = match &app.tabs[0].layout {
            SplitNode::Leaf { pane, .. } => pane.clone(),
            _ => unreachable!("fixture should use one list pane"),
        };

        let mut terminal = Terminal::new(TestBackend::new(64, 68)).expect("embedded browser");
        terminal
            .draw(|frame| {
                render_session_list(
                    frame,
                    frame.area(),
                    &pane,
                    true,
                    &mut app,
                    SessionListSurface::Embedded,
                    None,
                );
            })
            .expect("embedded browser render");
        let text = buffer_text(terminal.backend().buffer());
        for value in [
            " 01 ",
            "Model identity probe",
            "⋈ openrouter/anthropic/claude-opus-4-6",
            "▮▮▮▯ high",
            "0%",
        ] {
            assert!(text.contains(value), "missing {value:?}:\n{text}");
        }
        assert_eq!(
            app.session_list_render
                .cards_area
                .expect("bounded cards area")
                .height,
            OPERATIONAL_TABLE_MAX_BODY_HEIGHT
        );

        let mut short_terminal = Terminal::new(TestBackend::new(64, 24)).expect("short browser");
        short_terminal
            .draw(|frame| {
                render_session_list(
                    frame,
                    frame.area(),
                    &pane,
                    true,
                    &mut app,
                    SessionListSurface::Embedded,
                    None,
                );
            })
            .expect("short browser render");
        let short_text = buffer_text(short_terminal.backend().buffer());
        for value in ["⋈ openrouter/anthropic/claude-opus-4-6", "▮▮▮▯ high"] {
            assert!(
                short_text.contains(value),
                "missing {value:?}:\n{short_text}"
            );
        }
    }

    #[test]
    fn t20_rendered_fixed_glyphs_are_one_cell_and_keep_context_offset() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use ratatui::{Terminal, backend::TestBackend};

        let settings = crate::settings::UserSettings::default();
        let layout = navigator_layout::resolve(120, 2, settings.navigator_preset, None);
        let status = layout
            .columns
            .iter()
            .find(|cell| cell.column == navigator_layout::NavigatorColumn::Status)
            .expect("status column");
        let context = layout
            .columns
            .iter()
            .find(|cell| cell.column == navigator_layout::NavigatorColumn::Context)
            .expect("context column");
        for glyph in ["∞", "◆", "◐", "●", "?", "✓", "×", "■", "·"] {
            let mut row = crate::types::row::SessionRowViewModel::placeholder(uuid::Uuid::new_v4());
            row.status_icon = glyph.to_string();
            row.context_pct = crate::types::row::ContextCell::Display {
                label: "42%".to_string(),
                pct: Some(42.0),
            };
            let mut terminal = Terminal::new(TestBackend::new(120, 1)).expect("terminal");
            terminal
                .draw(|frame| {
                    render_navigator_row(
                        frame,
                        Rect::new(0, 0, 120, 1),
                        &row,
                        None,
                        &settings,
                        2,
                        false,
                        false,
                        false,
                        theme::blue(),
                        theme::tier_panel(),
                    );
                })
                .expect("render row");
            let buffer = terminal.backend().buffer();
            assert_eq!(navigator_layout::display_width(glyph), 1);
            assert_eq!(buffer[(status.start as u16, 0)].symbol(), glyph);
            assert_eq!(
                buffer_text_position(buffer, "42%").map(|(x, _)| x as usize),
                Some(context.start + context.width - 3)
            );
        }
    }

    #[test]
    fn t20_pinned_marker_follows_ordinal_with_reserved_unpinned_cell() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use ratatui::{Terminal, backend::TestBackend};

        let settings = crate::settings::UserSettings::default();
        let layout = navigator_layout::resolve(120, 2, settings.navigator_preset, None);
        let ordinal = layout
            .columns
            .iter()
            .find(|cell| cell.column == navigator_layout::NavigatorColumn::Ordinal)
            .expect("ordinal column");
        let pin = layout
            .columns
            .iter()
            .find(|cell| cell.column == navigator_layout::NavigatorColumn::Pin)
            .expect("pin column");
        let status = layout
            .columns
            .iter()
            .find(|cell| cell.column == navigator_layout::NavigatorColumn::Status)
            .expect("status column");

        let render = |is_pinned| {
            let mut row = crate::types::row::SessionRowViewModel::placeholder(uuid::Uuid::new_v4());
            row.effective_epic_ordinal = Some(7);
            row.status_icon = "◐".to_string();
            row.is_pinned = is_pinned;
            let mut terminal = Terminal::new(TestBackend::new(120, 1)).expect("terminal");
            terminal
                .draw(|frame| {
                    render_navigator_row(
                        frame,
                        Rect::new(0, 0, 120, 1),
                        &row,
                        None,
                        &settings,
                        2,
                        false,
                        false,
                        false,
                        theme::blue(),
                        theme::tier_panel(),
                    );
                })
                .expect("render navigator row");
            terminal.backend().buffer().clone()
        };

        let pinned_buffer = render(true);
        assert_eq!(pinned_buffer[(pin.start as u16, 0)].symbol(), "◆");
        assert_eq!(pin.start, ordinal.start + ordinal.width + 2);
        assert_eq!(pinned_buffer[(status.start as u16, 0)].symbol(), "◐");

        let unpinned_buffer = render(false);
        assert_eq!(unpinned_buffer[(pin.start as u16, 0)].symbol(), " ");
        assert_eq!(unpinned_buffer[(status.start as u16, 0)].symbol(), "◐");
    }

    #[test]
    fn pin_role_override_repaints_the_production_navigator_pin_cell() {
        use crate::ui::theme_roles::ThemeRole;
        use ratatui::{Terminal, backend::TestBackend};

        crate::ui::theme::with_theme_state(|| {
            let settings = crate::settings::UserSettings::default();
            let mut row = crate::types::row::SessionRowViewModel::placeholder(uuid::Uuid::new_v4());
            row.is_pinned = true;
            let mut terminal = Terminal::new(TestBackend::new(120, 1)).expect("terminal");
            terminal
                .draw(|frame| {
                    render_navigator_row(
                        frame,
                        frame.area(),
                        &row,
                        None,
                        &settings,
                        2,
                        true,
                        false,
                        true,
                        theme::accent(),
                        theme::tier_panel(),
                    );
                })
                .expect("baseline frame");
            let (x, y) = buffer_text_position(terminal.backend().buffer(), "◆").expect("pin");
            let before = terminal.backend().buffer()[(x, y)].fg;
            crate::ui::theme::set_theme_role_override(ThemeRole::Pin, Some([10, 20, 30]));
            terminal
                .draw(|frame| {
                    render_navigator_row(
                        frame,
                        frame.area(),
                        &row,
                        None,
                        &settings,
                        2,
                        true,
                        false,
                        true,
                        theme::accent(),
                        theme::tier_panel(),
                    );
                })
                .expect("override frame");
            assert_ne!(before, terminal.backend().buffer()[(x, y)].fg);
            assert_eq!(
                theme::test_rgb_channels(terminal.backend().buffer()[(x, y)].fg),
                Some((10, 20, 30))
            );
        });
    }

    #[test]
    fn t11_navigator_header_and_row_share_required_replacement_columns_and_offsets() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use ratatui::{Terminal, backend::TestBackend};

        let settings = crate::settings::UserSettings::default();
        let layout = navigator_layout::resolve(120, 2, settings.navigator_preset, None);
        let mut row = crate::types::row::SessionRowViewModel::placeholder(uuid::Uuid::new_v4());
        row.effective_epic_ordinal = Some(7);
        row.is_pinned = true;
        row.status_icon = "●".to_string();
        row.attention_glyph = "!";
        row.display_title = "positive function destination".to_string();
        row.context_pct = crate::types::row::ContextCell::Display {
            label: "42%".to_string(),
            pct: Some(42.0),
        };
        row.turns_text = Some("7t".to_string());
        let mut terminal = Terminal::new(TestBackend::new(120, 2)).expect("terminal");
        terminal
            .draw(|frame| {
                render_navigator_header(
                    frame,
                    Rect::new(0, 0, 120, 1),
                    theme::tier_panel(),
                    2,
                    settings.navigator_preset,
                    None,
                );
                render_navigator_row(
                    frame,
                    Rect::new(0, 1, 120, 1),
                    &row,
                    None,
                    &settings,
                    2,
                    false,
                    false,
                    false,
                    theme::blue(),
                    theme::tier_panel(),
                );
            })
            .expect("render navigator");
        let buffer = terminal.backend().buffer();
        for (column, header, row_value) in [
            (navigator_layout::NavigatorColumn::Ordinal, "#", "7"),
            (navigator_layout::NavigatorColumn::Pin, "∞", "◆"),
            (navigator_layout::NavigatorColumn::Status, "S", "●"),
            (navigator_layout::NavigatorColumn::Attention, "!", "!"),
            (
                navigator_layout::NavigatorColumn::Function,
                "FUNCTION",
                "positive function destination",
            ),
        ] {
            let cell = layout
                .columns
                .iter()
                .find(|cell| cell.column == column)
                .expect("required cell");
            assert_eq!(
                buffer_text_position(buffer, header),
                Some((cell.start as u16, 0))
            );
            assert_eq!(
                buffer_text_position(buffer, row_value).map(|(x, _)| x as usize),
                Some(match cell.align {
                    navigator_layout::Alignment::Left => cell.start,
                    navigator_layout::Alignment::Right => {
                        cell.start + cell.width - navigator_layout::display_width(row_value)
                    }
                    navigator_layout::Alignment::Center => {
                        cell.start + (cell.width - navigator_layout::display_width(row_value)) / 2
                    }
                }),
                "{column:?} row start diverged from shared layout"
            );
        }
        for (column, text) in [
            (navigator_layout::NavigatorColumn::Context, "42%"),
            (navigator_layout::NavigatorColumn::Turns, "7t"),
        ] {
            let cell = layout
                .columns
                .iter()
                .find(|cell| cell.column == column)
                .expect("required cell");
            assert_eq!(
                buffer_text_position(buffer, text).map(|(x, _)| x as usize),
                Some(cell.start + cell.width - navigator_layout::display_width(text))
            );
        }
    }

    #[test]
    fn t12_selected_inspector_renders_known_uuid_cwd_sandbox_root_and_branch() {
        use crate::app::app_test_helpers::{baseline_session, with_session_list};
        use ratatui::{Terminal, backend::TestBackend};
        use rsi_common::types::{SandboxKind, SessionKind};

        let mut app = with_session_list(1);
        let session_id = app.filtered_session_order[0];
        let mut session = baseline_session(session_id, SessionKind::Task);
        session.working_dir = std::path::PathBuf::from("/known/cwd");
        session.sandbox_kind = Some(SandboxKind::GitWorktree);
        session.sandbox_root = Some(std::path::PathBuf::from("/known/sandbox/root"));
        session.sandbox_branch = Some("feature/known-branch".to_string());
        session.pinned_at = Some(Utc::now());
        app.sessions.insert(session_id, SessionState::new(session));
        let inspector = crate::types::compute_session_inspector(&app, session_id, 1)
            .expect("selected inspector");
        assert!(
            inspector
                .signals
                .contains(&crate::types::InspectorSignal::Pinned),
            "signals preserve the pinned flag"
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).expect("terminal");
        terminal
            .draw(|frame| {
                render_session_inspector(
                    frame,
                    Rect::new(0, 0, 100, 32),
                    Some(&inspector),
                    app.sessions.get(&session_id).map(|state| &state.session),
                    theme::tier_panel(),
                );
            })
            .expect("render inspector");
        let text = buffer_text(terminal.backend().buffer());
        for expected in [
            session_id.to_string(),
            "/known/cwd".to_string(),
            "/known/sandbox/root".to_string(),
            "feature/known-branch".to_string(),
        ] {
            assert!(
                text.contains(&expected),
                "inspector missing {expected:?}: {text}"
            );
        }
    }

    #[test]
    fn t10_each_session_status_renders_through_one_physical_navigator_row() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use crate::app::app_test_helpers::baseline_session;
        use ratatui::{Terminal, backend::TestBackend};
        use rsi_common::types::SessionKind;

        let settings = crate::settings::UserSettings::default();
        for status in [
            SessionStatus::Starting,
            SessionStatus::Running,
            SessionStatus::WaitingApproval,
            SessionStatus::Completed,
            SessionStatus::Failed,
            SessionStatus::Interrupted,
            SessionStatus::Archived,
            SessionStatus::Deleted,
        ] {
            let session_id = uuid::Uuid::new_v4();
            let mut session = baseline_session(session_id, SessionKind::Task);
            session.status = status;
            let sessions = HashMap::from([(session_id, SessionState::new(session))]);
            let row = crate::types::row::compute_session_row_for_state(
                sessions.get(&session_id).expect("state"),
                &sessions,
                &[],
                &settings,
            );
            let mut terminal = Terminal::new(TestBackend::new(120, 1)).expect("terminal");
            terminal
                .draw(|frame| {
                    render_navigator_row(
                        frame,
                        Rect::new(0, 0, 120, 1),
                        &row,
                        None,
                        &settings,
                        2,
                        false,
                        false,
                        false,
                        theme::blue(),
                        theme::tier_panel(),
                    );
                })
                .expect("one-row navigator render");
            assert_eq!(terminal.backend().buffer().area.height, 1, "{status:?}");
            assert!(
                buffer_text(terminal.backend().buffer()).contains(&row.status_icon),
                "{status:?} icon must render in its one physical row"
            );
        }
    }

    #[test]
    fn t15_source_derived_attention_survives_every_semantic_width_and_preset() {
        use crate::app::app_test_helpers::baseline_session;
        use ratatui::{Terminal, backend::TestBackend};
        use rsi_common::types::{PendingQuestion, QuestionItem, SessionKind};

        let settings = crate::settings::UserSettings::default();
        let cases: Vec<(&str, &str, Box<dyn Fn(&mut SessionState)>)> = vec![
            (
                "waiting approval",
                "!",
                Box::new(|state| state.session.status = SessionStatus::WaitingApproval),
            ),
            (
                "waiting input",
                "!",
                Box::new(|state| {
                    state.session.pending_question = Some(PendingQuestion {
                        questions: vec![QuestionItem {
                            question: "known input".to_string(),
                            header: "Known input".to_string(),
                            options: Vec::new(),
                            multi_select: false,
                        }],
                    })
                }),
            ),
            (
                "failed",
                "!",
                Box::new(|state| state.session.status = SessionStatus::Failed),
            ),
            (
                "retrying",
                "↺",
                Box::new(|state| {
                    state.session.retry_attempt = Some(1);
                    state.session.max_retries = Some(3);
                }),
            ),
            ("stalled", "⧗", Box::new(|state| state.is_stalled = true)),
            (
                "unread",
                "•",
                Box::new(|state| {
                    state.events_generation = 2;
                    state.last_seen_events_generation = 1;
                }),
            ),
            ("quiet", "", Box::new(|_| {})),
        ];
        for (name, expected, configure) in cases {
            let id = uuid::Uuid::new_v4();
            let session = baseline_session(id, SessionKind::Task);
            let mut state = SessionState::new(session);
            configure(&mut state);
            let sessions = HashMap::from([(id, state)]);
            let row = crate::types::row::compute_session_row_for_state(
                sessions.get(&id).expect("state"),
                &sessions,
                &[],
                &settings,
            );
            assert_eq!(row.attention_glyph, expected, "{name}");
            for ordinal_width in 2..=4 {
                for preset in [
                    crate::types::NavigatorPreset::Dense,
                    crate::types::NavigatorPreset::Operations,
                    crate::types::NavigatorPreset::Cost,
                ] {
                    for width in navigator_layout::required_floor(ordinal_width)..=240 {
                        assert!(
                            navigator_layout::resolve(width, ordinal_width, preset, None)
                                .columns
                                .iter()
                                .any(|cell| cell.column
                                    == navigator_layout::NavigatorColumn::Attention),
                            "attention dropped for {name}, width {width}, ordinal {ordinal_width}, {preset:?}"
                        );
                    }
                }
            }
        }

        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let id = app.filtered_session_order[0];
        for entry in &mut app.settings.card_fields {
            if entry.field == crate::types::CardField::RetryInfo {
                entry.enabled = false;
            }
        }
        let state = app.sessions.get_mut(&id).expect("retry source");
        state.session.retry_attempt = Some(1);
        state.session.max_retries = Some(3);
        let row = crate::types::row::compute_session_row_for_state(
            app.sessions.get(&id).expect("retry state"),
            &app.sessions,
            &app.projects,
            &app.settings,
        );
        let inspector =
            crate::types::compute_session_inspector(&app, id, 1).expect("retry inspector source");
        assert_eq!(row.attention_glyph, "↺");
        assert!(
            row.retry_info.is_none(),
            "legacy optional retry count remains hidden"
        );
        assert!(
            row.attention_reasons
                .iter()
                .any(|reason| reason == "Retry pending")
        );
        assert!(
            inspector
                .signals
                .contains(&crate::types::InspectorSignal::Retry { attempt: 1, max: 3 })
        );
        for ordinal_width in 2..=4 {
            for preset in [
                crate::types::NavigatorPreset::Dense,
                crate::types::NavigatorPreset::Operations,
                crate::types::NavigatorPreset::Cost,
            ] {
                for width in navigator_layout::required_floor(ordinal_width)..=240 {
                    let layout = navigator_layout::resolve(width, ordinal_width, preset, None);
                    let attention = layout
                        .columns
                        .iter()
                        .find(|cell| cell.column == navigator_layout::NavigatorColumn::Attention)
                        .expect("retry-only attention remains required");
                    let mut render_settings = app.settings.clone();
                    render_settings.navigator_preset = preset;
                    let mut terminal = Terminal::new(TestBackend::new(width as u16, 1))
                        .expect("one-row retry navigator");
                    terminal
                        .draw(|frame| {
                            render_navigator_row(
                                frame,
                                Rect::new(0, 0, width as u16, 1),
                                &row,
                                None,
                                &render_settings,
                                ordinal_width,
                                false,
                                false,
                                false,
                                theme::blue(),
                                theme::tier_panel(),
                            );
                        })
                        .expect("render retry-only navigator");
                    assert_eq!(
                        terminal.backend().buffer()[(attention.start as u16, 0)].symbol(),
                        "↺",
                        "retry-only attention rendered at width {width}, ordinal {ordinal_width}, {preset:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn t18_rendered_identity_matches_canonical_lead_role_and_non_epic_resolution() {
        use crate::app::app_test_helpers::baseline_session;
        use rsi_common::types::SessionKind;

        let epic_id = uuid::Uuid::new_v4();
        let lead_id = uuid::Uuid::new_v4();
        let role_id = uuid::Uuid::new_v4();
        let ordinary_id = uuid::Uuid::new_v4();
        let mut epic = baseline_session(epic_id, SessionKind::Epic);
        epic.lead_session_id = Some(lead_id);
        let mut lead = baseline_session(lead_id, SessionKind::Task);
        lead.parent_id = Some(epic_id);
        lead.agent_role = Some("Lead role".to_string());
        lead.epic_spawn_ordinal = Some(8);
        let mut role = baseline_session(role_id, SessionKind::Task);
        role.parent_id = Some(epic_id);
        role.agent_role = Some("Reviewer role".to_string());
        role.epic_spawn_ordinal = Some(9);
        let mut ordinary = baseline_session(ordinary_id, SessionKind::Task);
        ordinary.title = Some("Ordinary title".to_string());
        let sessions = HashMap::from([
            (epic_id, SessionState::new(epic)),
            (lead_id, SessionState::new(lead)),
            (role_id, SessionState::new(role)),
            (ordinary_id, SessionState::new(ordinary)),
        ]);
        let settings = crate::settings::UserSettings::default();
        for id in [lead_id, role_id, ordinary_id] {
            let source = &sessions.get(&id).expect("source").session;
            let identity = crate::types::resolve_session_display_identity(source, &sessions);
            let row = crate::types::row::compute_session_row_for_state(
                sessions.get(&id).expect("state"),
                &sessions,
                &[],
                &settings,
            );
            assert_eq!(row.display_title, identity.effective_title);
            assert_eq!(row.effective_epic_ordinal, identity.effective_epic_ordinal);
        }
    }

    #[test]
    fn t13_t14_t15_t16_required_pin_status_attention_context_and_turn_cells_are_positive_and_fixed()
    {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use ratatui::{Terminal, backend::TestBackend};

        let settings = crate::settings::UserSettings::default();
        let layout = navigator_layout::resolve(120, 2, settings.navigator_preset, None);
        let cell = |column| {
            layout
                .columns
                .iter()
                .find(|cell| cell.column == column)
                .expect("required cell")
        };
        let pin_start = cell(navigator_layout::NavigatorColumn::Pin).start;
        let status_start = cell(navigator_layout::NavigatorColumn::Status).start;
        let attention_start = cell(navigator_layout::NavigatorColumn::Attention).start;
        let function_start = cell(navigator_layout::NavigatorColumn::Function).start;
        let context = cell(navigator_layout::NavigatorColumn::Context);
        let turns = cell(navigator_layout::NavigatorColumn::Turns);

        for (pinned, icon, attention, context_value, expected_context) in [
            (true, "◐", "!", None, None),
            (false, "●", "*", Some(42.0), Some("42%")),
            (false, "?", "!", Some(7.0), Some("7%")),
            (false, "✓", "", Some(9.0), Some("9%")),
            (false, "×", "!", Some(11.0), Some("11%")),
            (false, "■", "*", Some(17.0), Some("17%")),
            (false, "·", "", Some(23.0), Some("23%")),
        ] {
            let mut row = crate::types::row::SessionRowViewModel::placeholder(uuid::Uuid::new_v4());
            row.is_pinned = pinned;
            row.status_icon = icon.to_string();
            row.attention_glyph = attention;
            row.display_title = "fixed destination".to_string();
            row.context_pct = match context_value {
                Some(pct) => crate::types::row::ContextCell::Display {
                    label: format!("{pct:.0}%"),
                    pct: Some(pct),
                },
                None => crate::types::row::ContextCell::Missing,
            };
            row.turns_text = Some("7t".to_string());
            let mut terminal = Terminal::new(TestBackend::new(120, 1)).expect("terminal");
            terminal
                .draw(|frame| {
                    render_navigator_row(
                        frame,
                        Rect::new(0, 0, 120, 1),
                        &row,
                        None,
                        &settings,
                        2,
                        false,
                        false,
                        false,
                        theme::blue(),
                        theme::tier_panel(),
                    );
                })
                .expect("render row");
            let buffer = terminal.backend().buffer();
            assert_eq!(
                buffer[(pin_start as u16, 0)].symbol(),
                if pinned { "◆" } else { " " }
            );
            assert_eq!(buffer[(status_start as u16, 0)].symbol(), icon);
            assert_eq!(
                buffer[(attention_start as u16, 0)].symbol(),
                if attention.is_empty() { " " } else { attention }
            );
            assert_eq!(
                buffer_text_position(buffer, "fixed destination").map(|(x, _)| x as usize),
                Some(function_start)
            );
            if let Some(expected_context) = expected_context {
                assert_eq!(
                    buffer_text_position(buffer, expected_context).map(|(x, _)| x as usize),
                    Some(
                        context.start + context.width
                            - navigator_layout::display_width(expected_context)
                    )
                );
            } else {
                let quiet_context = (context.start..context.start + context.width)
                    .map(|x| buffer[(x as u16, 0)].symbol())
                    .collect::<String>();
                assert_eq!(quiet_context, " ".repeat(context.width));
            }
            assert_eq!(
                buffer_text_position(buffer, "7t").map(|(x, _)| x as usize),
                Some(turns.start + turns.width - 2)
            );
        }
    }

    #[test]
    fn t14_source_lifecycle_projection_renders_each_normative_status_in_one_fixed_cell() {
        use crate::app::app_test_helpers::baseline_session;
        use ratatui::{Terminal, backend::TestBackend};
        use rsi_common::types::SessionKind;

        let settings = crate::settings::UserSettings::default();
        let status_start = navigator_layout::resolve(120, 2, settings.navigator_preset, None)
            .columns
            .iter()
            .find(|cell| cell.column == navigator_layout::NavigatorColumn::Status)
            .expect("status cell")
            .start as u16;
        for (status, expected) in [
            (SessionStatus::Starting, "◐"),
            (SessionStatus::Running, "●"),
            (SessionStatus::WaitingApproval, "?"),
            (SessionStatus::Completed, "✓"),
            (SessionStatus::Failed, "×"),
            (SessionStatus::Interrupted, "■"),
            (SessionStatus::Archived, "·"),
        ] {
            let id = uuid::Uuid::new_v4();
            let mut session = baseline_session(id, SessionKind::Task);
            session.status = status;
            let sessions = HashMap::from([(id, SessionState::new(session))]);
            let row = crate::types::row::compute_session_row_for_state(
                sessions.get(&id).expect("status source"),
                &sessions,
                &[],
                &settings,
            );
            let mut terminal = Terminal::new(TestBackend::new(120, 1)).expect("terminal");
            terminal
                .draw(|frame| {
                    render_navigator_row(
                        frame,
                        frame.area(),
                        &row,
                        None,
                        &settings,
                        2,
                        false,
                        false,
                        false,
                        theme::blue(),
                        theme::tier_panel(),
                    );
                })
                .expect("source-driven row render");
            let buffer = terminal.backend().buffer();
            assert_eq!(row.status_icon, expected, "{status:?} projection");
            assert_eq!(
                buffer[(status_start, 0)].symbol(),
                expected,
                "{status:?} cell"
            );
            assert_eq!(navigator_layout::display_width(expected), 1, "{status:?}");
        }

        let id = uuid::Uuid::new_v4();
        let mut pending = baseline_session(id, SessionKind::Task);
        pending.status = SessionStatus::Running;
        pending.pending_archive = true;
        let sessions = HashMap::from([(id, SessionState::new(pending))]);
        let row = crate::types::row::compute_session_row_for_state(
            sessions.get(&id).expect("pending source"),
            &sessions,
            &[],
            &settings,
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 1)).expect("terminal");
        terminal
            .draw(|frame| {
                render_navigator_row(
                    frame,
                    frame.area(),
                    &row,
                    None,
                    &settings,
                    2,
                    false,
                    false,
                    false,
                    theme::blue(),
                    theme::tier_panel(),
                );
            })
            .expect("pending archive row render");
        assert_eq!(row.status_icon, "●");
        assert_eq!(
            terminal.backend().buffer()[(status_start, 0)].symbol(),
            "●",
            "pending archive retains the fixed lifecycle cell"
        );
    }

    #[test]
    fn t18_table_ordinal_width_uses_real_dataset_width_on_every_applicable_surface() {
        use crate::app::app_test_helpers::{baseline_session, with_session_list};
        use crate::types::SplitNode;
        use ratatui::{Terminal, backend::TestBackend};
        use rsi_common::types::SessionKind;

        let cases = [
            (
                "full-main-legacy",
                SessionListSurface::Full,
                crate::types::SessionListZone::Main,
                119,
                40,
                SessionBrowserMode::LegacyTable,
            ),
            (
                "embedded-main-inspector",
                SessionListSurface::Embedded,
                crate::types::SessionListZone::Main,
                240,
                40,
                SessionBrowserMode::EmbeddedInspector,
            ),
            (
                "full-archive-legacy",
                SessionListSurface::Full,
                crate::types::SessionListZone::Archive,
                240,
                40,
                SessionBrowserMode::LegacyTable,
            ),
        ];
        for (case, surface, zone, width, height, expected_mode) in cases {
            let mut app = with_session_list(1);
            let child_id = app.filtered_session_order[0];
            let epic_id = uuid::Uuid::new_v4();
            let epic = baseline_session(epic_id, SessionKind::Epic);
            let child = app.sessions.get_mut(&child_id).expect("child");
            child.session.parent_id = Some(epic_id);
            child.session.agent_role = Some("t18-ordinal-function".to_string());
            child.session.epic_spawn_ordinal = Some(100);
            app.sessions.insert(epic_id, SessionState::new(epic));
            let order = vec![child_id];
            app.filtered_session_order = order.clone();
            app.filtered_taskrabbit_order = order.clone();
            app.filtered_archived_order = order.clone();
            app.filtered_jobs_order = order;
            if let Pane::SessionList {
                active_zone,
                selected_index,
                selected_session,
                ..
            } = app.session_list_pane_mut()
            {
                *active_zone = zone;
                *selected_index = 0;
                *selected_session = Some(child_id);
            }
            let pane = match &app.tabs[0].layout {
                SplitNode::Leaf { pane, .. } => pane.clone(),
                _ => unreachable!("single list pane"),
            };
            let content = Rect::new(0, 1, width, height - 1);
            let browser = resolve_session_browser_layout(content, surface, zone);
            assert_eq!(browser.mode, expected_mode, "{case}");
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal
                .draw(|frame| {
                    render_session_list(frame, frame.area(), &pane, true, &mut app, surface, None);
                })
                .expect("table production render");
            let state = match zone {
                crate::types::SessionListZone::Main => &app.session_list_render.main,
                crate::types::SessionListZone::TaskRabbit => &app.session_list_render.taskrabbit,
                crate::types::SessionListZone::Archive => &app.session_list_render.archive,
                crate::types::SessionListZone::Jobs => &app.session_list_render.jobs,
            };
            assert_eq!(
                operational_ordinal_width(&[child_id], &app.sessions),
                3,
                "{case}"
            );
            let layout = navigator_layout::resolve(
                state.last_render_width as usize,
                3,
                app.settings.navigator_preset,
                app.settings.navigator_optional_columns.as_deref(),
            );
            let function_start = layout
                .columns
                .iter()
                .find(|cell| cell.column == navigator_layout::NavigatorColumn::Function)
                .expect("FUNCTION")
                .start;
            let buffer = terminal.backend().buffer();
            assert_eq!(
                buffer_text_position(buffer, "100").map(|(x, _)| x),
                Some(2),
                "{case}"
            );
            assert_eq!(
                buffer_text_position(buffer, "t18-ordinal-function").map(|(x, _)| x as usize),
                Some(function_start),
                "{case} uses R(3) header/row geometry"
            );
            assert_eq!(
                buffer_text_position(buffer, "FUNCTION").map(|(x, _)| x as usize),
                Some(function_start),
                "{case} header shares the R(3) FUNCTION offset"
            );
            assert_eq!(
                navigator_layout::required_floor(3),
                navigator_layout::required_floor(2) + 1,
                "{case} observes the exact R(3) ordinal shift"
            );
            assert_eq!(state.card_heights, vec![1], "{case}");
        }
    }

    #[test]
    fn t18_t37_rotated_function_title_and_optional_rotation_cell_stay_separate() {
        use crate::app::app_test_helpers::{baseline_session, with_session_list};
        use ratatui::{Terminal, backend::TestBackend};
        use rsi_common::types::SessionKind;

        let mut app = with_session_list(1);
        let session_id = app.filtered_session_order[0];
        let predecessor_id = uuid::Uuid::new_v4();
        let mut predecessor = baseline_session(predecessor_id, SessionKind::Task);
        predecessor.title = Some("zr title".to_string());
        let state = app.sessions.get_mut(&session_id).expect("rotated session");
        state.session.title = Some("zr title".to_string());
        state.session.continued_from = Some(predecessor_id);
        app.sessions
            .insert(predecessor_id, crate::types::SessionState::new(predecessor));
        let row = crate::types::row::compute_session_row_for_state(
            app.sessions.get(&session_id).expect("rotated state"),
            &app.sessions,
            &app.projects,
            &app.settings,
        );
        let canonical_title = crate::types::resolve_session_display_identity(
            &app.sessions
                .get(&session_id)
                .expect("rotated state")
                .session,
            &app.sessions,
        )
        .effective_title;
        assert_eq!(canonical_title, "zr title");
        assert_eq!(row.display_title, canonical_title);
        assert_eq!(row.rotation_suffix.trim(), "↻1");

        let render_cell = |settings: &crate::settings::UserSettings,
                           width: u16,
                           column: navigator_layout::NavigatorColumn| {
            let layout = navigator_layout::resolve(
                width as usize,
                2,
                settings.navigator_preset,
                settings.navigator_optional_columns.as_deref(),
            );
            let cell = layout
                .columns
                .iter()
                .find(|cell| cell.column == column)
                .expect("visible navigator column");
            let mut terminal = Terminal::new(TestBackend::new(width, 1)).expect("navigator");
            terminal
                .draw(|frame| {
                    render_navigator_row(
                        frame,
                        Rect::new(0, 0, width, 1),
                        &row,
                        None,
                        settings,
                        2,
                        false,
                        false,
                        false,
                        theme::blue(),
                        theme::tier_panel(),
                    );
                })
                .expect("rotated navigator render");
            let text = (cell.start..cell.start + cell.width)
                .map(|x| terminal.backend().buffer()[(x as u16, 0)].symbol())
                .collect::<String>();
            (layout, text.trim_end().to_string())
        };

        for preset in [
            crate::types::NavigatorPreset::Dense,
            crate::types::NavigatorPreset::Cost,
        ] {
            let mut settings = crate::settings::UserSettings::default();
            settings.navigator_preset = preset;
            let (_, function) =
                render_cell(&settings, 82, navigator_layout::NavigatorColumn::Function);
            assert_eq!(function, row.display_title, "{preset:?} FUNCTION title");
        }

        let mut all_enabled = crate::settings::UserSettings::default();
        all_enabled.navigator_optional_columns = Some(navigator_layout::OPTIONAL_ORDER.to_vec());
        let (sub_rotation, function) = render_cell(
            &all_enabled,
            104,
            navigator_layout::NavigatorColumn::Function,
        );
        assert_eq!(
            function, row.display_title,
            "sub-ROT breakpoint FUNCTION title"
        );
        assert_eq!(
            sub_rotation
                .columns
                .iter()
                .map(|cell| cell.column)
                .collect::<Vec<_>>(),
            vec![
                navigator_layout::NavigatorColumn::Ordinal,
                navigator_layout::NavigatorColumn::Pin,
                navigator_layout::NavigatorColumn::Status,
                navigator_layout::NavigatorColumn::Attention,
                navigator_layout::NavigatorColumn::Function,
                navigator_layout::NavigatorColumn::Context,
                navigator_layout::NavigatorColumn::Turns,
                navigator_layout::NavigatorColumn::Age,
                navigator_layout::NavigatorColumn::Provider,
                navigator_layout::NavigatorColumn::Model,
                navigator_layout::NavigatorColumn::Effort,
                navigator_layout::NavigatorColumn::Retry,
                navigator_layout::NavigatorColumn::Cost,
                navigator_layout::NavigatorColumn::Work,
            ],
            "sub-ROT breakpoint keeps the expected visible-column set"
        );

        let (with_rotation, function) = render_cell(
            &all_enabled,
            105,
            navigator_layout::NavigatorColumn::Function,
        );
        assert_eq!(function, row.display_title, "ROT-enabled FUNCTION title");
        let (_, rotation_cell) = render_cell(
            &all_enabled,
            105,
            navigator_layout::NavigatorColumn::Rotation,
        );
        assert_eq!(
            rotation_cell, " ↻1",
            "ROT displays the continuation depth exactly once in its right-aligned cell"
        );
        assert_eq!(
            with_rotation
                .columns
                .iter()
                .map(|cell| cell.column)
                .collect::<Vec<_>>(),
            vec![
                navigator_layout::NavigatorColumn::Ordinal,
                navigator_layout::NavigatorColumn::Pin,
                navigator_layout::NavigatorColumn::Status,
                navigator_layout::NavigatorColumn::Attention,
                navigator_layout::NavigatorColumn::Function,
                navigator_layout::NavigatorColumn::Context,
                navigator_layout::NavigatorColumn::Turns,
                navigator_layout::NavigatorColumn::Age,
                navigator_layout::NavigatorColumn::Provider,
                navigator_layout::NavigatorColumn::Model,
                navigator_layout::NavigatorColumn::Effort,
                navigator_layout::NavigatorColumn::Retry,
                navigator_layout::NavigatorColumn::Cost,
                navigator_layout::NavigatorColumn::Work,
                navigator_layout::NavigatorColumn::Rotation,
            ],
            "ROT appears at its exact cumulative breakpoint"
        );

        let mut rotation_disabled = all_enabled.clone();
        rotation_disabled.navigator_optional_columns = Some(
            navigator_layout::OPTIONAL_ORDER
                .into_iter()
                .filter(|column| *column != crate::types::NavigatorOptionalColumn::Rotation)
                .collect(),
        );
        let (disabled_layout, function) = render_cell(
            &rotation_disabled,
            240,
            navigator_layout::NavigatorColumn::Function,
        );
        assert_eq!(
            function, row.display_title,
            "rotation-disabled FUNCTION title"
        );
        assert_eq!(
            disabled_layout
                .columns
                .iter()
                .map(|cell| cell.column)
                .collect::<Vec<_>>(),
            vec![
                navigator_layout::NavigatorColumn::Ordinal,
                navigator_layout::NavigatorColumn::Pin,
                navigator_layout::NavigatorColumn::Status,
                navigator_layout::NavigatorColumn::Attention,
                navigator_layout::NavigatorColumn::Function,
                navigator_layout::NavigatorColumn::Context,
                navigator_layout::NavigatorColumn::Turns,
                navigator_layout::NavigatorColumn::Age,
                navigator_layout::NavigatorColumn::Provider,
                navigator_layout::NavigatorColumn::Model,
                navigator_layout::NavigatorColumn::Effort,
                navigator_layout::NavigatorColumn::Retry,
                navigator_layout::NavigatorColumn::Cost,
                navigator_layout::NavigatorColumn::Work,
                navigator_layout::NavigatorColumn::Project,
                navigator_layout::NavigatorColumn::Created,
            ],
            "advanced Rotation disable retains the expected visible-column set"
        );
    }

    #[test]
    fn test_format_relative_time() {
        let now = Utc::now();
        assert_eq!(
            format_relative_time(now - chrono::Duration::seconds(30)),
            "<1m ago"
        );
        assert_eq!(
            format_relative_time(now - chrono::Duration::minutes(5)),
            "5m ago"
        );
        assert_eq!(
            format_relative_time(now - chrono::Duration::hours(2)),
            "2h ago"
        );
        assert_eq!(
            format_relative_time(now - chrono::Duration::days(3)),
            "3d ago"
        );
    }

    #[test]
    fn short_model_label_family_table() {
        // Map's "model name mapping table tested" verify bullet — F-011:
        // `short_model_label` itself had no direct unit test prior to this
        // (only the longer-form `abbreviate_model` was covered, in
        // `rsi-common/model_utils.rs`). Pins the family-detection order
        // (opus checked before sonnet, etc.) so a `claude-sonnet-5` session
        // can never again silently render "opus".
        let cases: &[(&str, &str)] = &[
            ("claude-opus-4-5", "opus"),
            ("claude-sonnet-5", "sonnet"),
            ("claude-haiku-4-5", "haiku"),
            ("gemini-2.5-pro", "gem-2.5p"),
            ("gemini-2.5-flash", "gem-2.5f"),
            ("gemini-3-pro", "gem-3"),
            ("gemini-1.5-pro", "gemini"),
            ("codex-2", "codex"),
            ("gpt-oss-120b", "gpt-oss"),
            ("gpt-4o", "gpt-4o"),
            ("gpt-4-turbo", "gpt-4"),
            ("gpt-3.5-turbo", "gpt"),
            ("deepseek-v3", "deepseek"),
            ("qwen2.5-72b", "qwen"),
            ("mistral-large-2", "mistral-"), // unrecognized -> first-8-chars fallback
        ];
        for (model, expected) in cases {
            assert_eq!(
                short_model_label(Some(model)).as_deref(),
                Some(*expected),
                "short_model_label({model:?}) mismatch"
            );
        }
        assert_eq!(short_model_label(None), None);
    }

    #[test]
    fn effort_bar_counts_follow_ordered_model_ladders() {
        assert_eq!(
            effort_bar_counts(Some("xhigh"), Some("claude-opus-5")),
            (5, 4)
        );
        assert_eq!(
            effort_bar_counts(Some("max"), Some("claude-opus-5")),
            (5, 5)
        );
        assert_eq!(
            effort_bar_counts(Some("max"), Some("claude-opus-4-6")),
            (4, 4)
        );
        assert_eq!(
            effort_bar_counts(Some("xhigh"), Some("gpt-6-astra")),
            (6, 4)
        );
        assert_eq!(effort_bar_counts(Some("max"), Some("gpt-6-astra")), (6, 5));
        assert_eq!(
            effort_bar_counts(Some("ultra"), Some("gpt-6-astra")),
            (6, 6)
        );
        assert_eq!(effort_bar_counts(None, Some("claude-opus-5")), (5, 4));
        assert_eq!(effort_bar_counts(None, Some("claude-sonnet-4-6")), (4, 3));
        assert_eq!(
            effort_bar_counts(Some("unknown"), Some("claude-opus-5")),
            (5, 4)
        );
        assert_eq!(effort_bar_counts(Some("high"), None), (0, 0));
    }

    #[test]
    fn test_session_detail_content_y_offset_includes_all_header_rows() {
        assert_eq!(
            session_detail_content_y_offset(true, true, true, true, true, true, true),
            9
        );
        assert_eq!(
            session_detail_content_y_offset(false, true, true, true, true, true, true),
            SESSION_DETAIL_HEADER_HEIGHT
        );
    }

    // --- find_visible_events tests ---

    #[test]
    fn test_find_visible_empty() {
        assert_eq!(find_visible_events(&[], &[], 0, 40), None);
    }

    #[test]
    fn test_find_visible_single_event() {
        let offsets = vec![0];
        let heights = vec![10];
        assert_eq!(find_visible_events(&offsets, &heights, 0, 40), Some((0, 0)));
    }

    #[test]
    fn test_find_visible_all_visible() {
        // 3 events, total height 30, viewport 40 — all visible
        let offsets = vec![0, 10, 20];
        let heights = vec![10, 10, 10];
        assert_eq!(find_visible_events(&offsets, &heights, 0, 40), Some((0, 2)));
    }

    #[test]
    fn test_find_visible_scrolled_middle() {
        // 5 events of height 10 each, viewport 25, scrolled to 15
        let offsets = vec![0, 10, 20, 30, 40];
        let heights = vec![10, 10, 10, 10, 10];
        // Visible: events starting at 10 (partially), 20, 30 (partially)
        let result = find_visible_events(&offsets, &heights, 15, 25);
        assert_eq!(result, Some((1, 3)));
    }

    #[test]
    fn test_find_visible_scrolled_to_end() {
        let offsets = vec![0, 10, 20, 30, 40];
        let heights = vec![10, 10, 10, 10, 10];
        // Scrolled to 40, viewport 10 — only last event visible
        assert_eq!(
            find_visible_events(&offsets, &heights, 40, 10),
            Some((4, 4))
        );
    }

    #[test]
    fn test_find_visible_with_zero_height_events() {
        // Events: 10, 0, 0, 10, 10
        let offsets = vec![0, 10, 10, 10, 20];
        let heights = vec![10, 0, 0, 10, 10];
        // Scrolled to 5, viewport 20 — should skip zero-height events
        let result = find_visible_events(&offsets, &heights, 5, 20);
        assert_eq!(result, Some((0, 4)));
    }

    #[test]
    fn test_find_visible_all_zero_height() {
        let offsets = vec![0, 0, 0];
        let heights = vec![0, 0, 0];
        assert_eq!(find_visible_events(&offsets, &heights, 0, 40), None);
    }

    #[test]
    fn test_find_visible_partial_first_event() {
        // Single tall event, scrolled partway through it
        let offsets = vec![0];
        let heights = vec![100];
        let result = find_visible_events(&offsets, &heights, 50, 40);
        assert_eq!(result, Some((0, 0)));
    }

    // --- count_thinking_run tests ---

    fn make_event(event_type: EventType) -> ConversationEvent {
        ConversationEvent {
            id: 0,
            session_id: uuid::Uuid::new_v4(),
            sequence: 1,
            event_type,
            role: Some(Role::Assistant),
            content: String::new(),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    fn make_test_state_with_events(events: Vec<ConversationEvent>) -> SessionState {
        use rsi_common::types::{
            ContextUsageConfidence, Session, SessionKind, SessionProvider, SessionStatus,
        };
        let session = Session {
            context_fill_pct: None,
            id: uuid::Uuid::new_v4(),
            provider: SessionProvider::Claude,
            claude_session_id: None,
            query: "test".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            pending_question: None,
            pending_archive: false,
            working_dir: std::path::PathBuf::from("/tmp"),
            git_branch: None,
            status: SessionStatus::Running,
            project_id: None,
            session_kind: SessionKind::Standard,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            context_usage_confidence: ContextUsageConfidence::default(),
            continued_from: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            scheduled_job_id: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: None,
            work_time_ms: None,
            approval_started_at: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: None,
            lead_session_id: None,
            is_eval: false,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        };
        let mut state = SessionState::new(session);
        state.events = events;
        state
    }

    fn make_table_session(
        id: uuid::Uuid,
        created_days_ago: i64,
        updated_days_ago: i64,
    ) -> SessionState {
        let mut state = make_test_state_with_events(Vec::new());
        state.session.id = id;
        state.session.created_at = chrono::Utc::now() - chrono::Duration::days(created_days_ago);
        state.session.updated_at = chrono::Utc::now() - chrono::Duration::days(updated_days_ago);
        state.session.status = rsi_common::types::SessionStatus::Completed;
        state
    }

    #[test]
    fn non_main_zones_do_not_emit_focus_headers() {
        use std::collections::HashMap;

        let newer_stale = uuid::Uuid::new_v4();
        let older_recent = uuid::Uuid::new_v4();
        let order = vec![newer_stale, older_recent];
        let sessions = HashMap::from([
            (newer_stale, make_table_session(newer_stale, 1, 11)),
            (older_recent, make_table_session(older_recent, 6, 1)),
        ]);
        let mut zone = crate::types::ZoneRenderState::default();

        compute_table_geometry(
            &order,
            &sessions,
            &[],
            0,
            SessionListPresentation::Table(SessionListDensity::Full),
            120,
            &mut zone,
            crate::app::SortOrder::NewestCreated,
            crate::types::SessionListZone::Archive,
            &HashMap::new(),
            &HashSet::new(),
            None,
            false,
        );

        assert!(
            zone.label_headers.is_empty(),
            "focus sections belong to the main operational navigator"
        );
    }

    #[test]
    fn main_zone_emits_operational_focus_headers() {
        use std::collections::HashMap;

        let recent = uuid::Uuid::new_v4();
        let stale = uuid::Uuid::new_v4();
        let order = vec![recent, stale];
        let sessions = HashMap::from([
            (recent, make_table_session(recent, 10, 1)),
            (stale, make_table_session(stale, 1, 11)),
        ]);
        let mut zone = crate::types::ZoneRenderState::default();
        let focus_index = crate::types::compute_session_focus_index(&sessions, chrono::Utc::now());

        compute_table_geometry(
            &order,
            &sessions,
            &[],
            0,
            SessionListPresentation::Navigator,
            120,
            &mut zone,
            crate::app::SortOrder::FreshestFirst,
            crate::types::SessionListZone::Main,
            &focus_index,
            &HashSet::new(),
            None,
            false,
        );

        let names: Vec<&str> = zone
            .label_headers
            .iter()
            .map(|(_, name, _, _)| name.as_str())
            .collect();
        assert_eq!(names, vec!["RECENT", "QUIET"]);
    }

    #[test]
    fn test_count_thinking_run_all_thinking() {
        let state = make_test_state_with_events(vec![
            make_event(EventType::Thinking),
            make_event(EventType::Thinking),
            make_event(EventType::Thinking),
        ]);
        assert_eq!(count_thinking_run(&state, 0), 3);
    }

    #[test]
    fn test_count_thinking_run_mixed() {
        let state = make_test_state_with_events(vec![
            make_event(EventType::Thinking),
            make_event(EventType::Thinking),
            make_event(EventType::Message),
            make_event(EventType::Thinking),
        ]);
        assert_eq!(count_thinking_run(&state, 0), 2);
    }

    #[test]
    fn test_count_thinking_run_from_middle() {
        let state = make_test_state_with_events(vec![
            make_event(EventType::Message),
            make_event(EventType::Thinking),
            make_event(EventType::Thinking),
        ]);
        assert_eq!(count_thinking_run(&state, 1), 2);
    }

    #[test]
    fn test_count_thinking_run_single() {
        let state = make_test_state_with_events(vec![
            make_event(EventType::Thinking),
            make_event(EventType::Message),
        ]);
        assert_eq!(count_thinking_run(&state, 0), 1);
    }

    // --- Sandbox indicator tests ---

    #[test]
    fn session_row_sandbox_indicator() {
        use crate::settings::UserSettings;
        use rsi_common::types::{
            ContextUsageConfidence, SandboxKind, Session, SessionKind, SessionProvider,
            SessionStatus,
        };
        use std::collections::HashMap;

        let make_session = |kind: Option<SandboxKind>| Session {
            context_fill_pct: None,
            id: uuid::Uuid::new_v4(),
            provider: SessionProvider::Claude,
            claude_session_id: None,
            query: "test".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            pending_question: None,
            pending_archive: false,
            working_dir: std::path::PathBuf::from("/tmp"),
            git_branch: None,
            status: SessionStatus::Running,
            project_id: None,
            session_kind: SessionKind::Standard,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            context_usage_confidence: ContextUsageConfidence::default(),
            continued_from: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            scheduled_job_id: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: None,
            work_time_ms: None,
            approval_started_at: None,
            sandbox_kind: kind,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: None,
            lead_session_id: None,
            is_eval: false,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        };

        let sessions_map: HashMap<uuid::Uuid, crate::types::SessionState> = HashMap::new();
        let projects = Vec::new();
        let settings = UserSettings::default();

        // No sandbox: is_sandboxed = false
        let no_sandbox = make_session(None);
        let state_no_sandbox = crate::types::SessionState::new(no_sandbox);
        let row_none = crate::types::row::compute_session_row_for_state(
            &state_no_sandbox,
            &sessions_map,
            &projects,
            &settings,
        );
        assert!(
            !row_none.is_sandboxed,
            "is_sandboxed should be false when sandbox_kind is None"
        );

        // Sandboxed: is_sandboxed = true
        let sandboxed = make_session(Some(SandboxKind::GitWorktree));
        let state_sandboxed = crate::types::SessionState::new(sandboxed);
        let row_some = crate::types::row::compute_session_row_for_state(
            &state_sandboxed,
            &sessions_map,
            &projects,
            &settings,
        );
        assert!(
            row_some.is_sandboxed,
            "is_sandboxed should be true when sandbox_kind is Some"
        );
    }

    // --- kind_pill + kind_color tests ---

    #[test]
    fn kind_pill_all_variants() {
        use rsi_common::types::SessionKind;
        // Verify pill text for each kind
        let cases: &[(SessionKind, Option<&str>)] = &[
            (SessionKind::Standard, None),
            (SessionKind::TaskRabbit, None),
            (SessionKind::Bug, Some("BUG")),
            (SessionKind::Group, Some("GRP")),
            (SessionKind::Epic, Some("EPC")),
            (SessionKind::Story, Some("STY")),
            (SessionKind::Task, Some("TSK")),
            (SessionKind::Feature, Some("FEAT")),
            (SessionKind::Refactor, Some("REF")),
            (SessionKind::Research, Some("RES")),
        ];
        // Note: kind_pill is now Option<String>; compare via as_deref() to allow
        // comparing against Option<&str> in the test cases.
        use crate::settings::UserSettings;
        use std::collections::HashMap;
        let sessions_map: HashMap<uuid::Uuid, crate::types::SessionState> = HashMap::new();
        let projects = Vec::new();
        let settings = UserSettings::default();

        for (kind, expected_pill) in cases {
            let session = rsi_common::types::Session {
                context_fill_pct: None,
                id: uuid::Uuid::new_v4(),
                provider: rsi_common::types::SessionProvider::Claude,
                claude_session_id: None,
                query: "test".to_string(),
                title: None,
                agent_role: None,
                epic_spawn_ordinal: None,
                description: None,
                short_summary: None,
                pending_question: None,
                pending_archive: false,
                working_dir: std::path::PathBuf::from("/tmp"),
                git_branch: None,
                status: rsi_common::types::SessionStatus::Running,
                project_id: None,
                session_kind: *kind,
                pinned_at: None,
                testing_needed_at: None,
                rotation_disabled_at: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                cost_usd: None,
                duration_ms: None,
                num_turns: None,
                model: None,
                input_tokens: None,
                output_tokens: None,
                context_window: None,
                resolved_context_budget: None,
                total_input_tokens: None,
                total_output_tokens: None,
                total_cache_creation_tokens: None,
                total_cache_read_tokens: None,
                stop_reason: None,
                context_usage_confidence: rsi_common::types::ContextUsageConfidence::default(),
                continued_from: None,
                handoff_filepath: None,
                active_task: None,
                group_id: None,
                scheduled_job_id: None,
                pipeline_artifact: None,
                workflow_id: None,
                workflow_id_override: None,
                rotation_depth: 0,
                retry_attempt: None,
                max_retries: None,
                daemon_input_tokens: None,
                daemon_output_tokens: None,
                effort: None,
                issue_identifier: None,
                issue_url: None,
                issue_tracker_id: None,
                rating: None,
                harness_version_hash: None,
                test_passed: None,
                clippy_passed: None,
                turn_count: None,
                retry_count: None,
                approval_wait_ms: None,
                work_time_ms: None,
                approval_started_at: None,
                sandbox_kind: None,
                sandbox_root: None,
                sandbox_branch: None,
                sandbox_cleanup_state: None,
                tag: String::new(),
                tags: Vec::new(),
                parent_id: None,
                lead_session_id: None,
                is_eval: false,
                capability_class: None,
                topology_node_id: None,
                topology_iteration: 0,
                provider_cli_version: None,
                provider_capabilities: Vec::new(),
                thinking_tokens: None,
                service_tier: None,
                cache_creation_1h_tokens: None,
                cache_creation_5m_tokens: None,
                permission_denial_count: None,
                subagent_stats_json: None,
                queued_turn_count: None,
                terminal_reason: None,
            };
            let mut state = crate::types::SessionState::new(session);
            // Expand to get pills
            state.list_card_expanded = true;
            let row = crate::types::row::compute_session_row_for_state(
                &state,
                &sessions_map,
                &projects,
                &settings,
            );
            assert_eq!(
                row.kind_pill.as_deref(),
                *expected_pill,
                "kind_pill mismatch for {kind:?}"
            );
        }
    }

    #[test]
    fn kind_color_all_variants() {
        use rsi_common::types::SessionKind;
        // Standard and TaskRabbit have defined colors (Standard -> raw_session_magenta, TR -> None)
        assert!(kind_color(SessionKind::Standard).is_some());
        assert!(kind_color(SessionKind::TaskRabbit).is_none());
        assert!(kind_color(SessionKind::Group).is_some());
        assert!(kind_color(SessionKind::Epic).is_some());
        assert!(kind_color(SessionKind::Story).is_some());
        assert!(kind_color(SessionKind::Task).is_some());
        assert!(kind_color(SessionKind::Bug).is_some());
        assert!(kind_color(SessionKind::Feature).is_some());
        assert!(kind_color(SessionKind::Refactor).is_some());
        assert!(kind_color(SessionKind::Research).is_some());
    }

    #[test]
    fn t10_every_session_status_has_a_distinct_one_cell_row_status_glyph() {
        // Map's "all SessionStatus variants render distinct" verify bullet
        // (F-037): pin icon-level distinctness as a real automated test,
        // not just an informal research observation. Color is NOT fully
        // distinct today (Interrupted/Archived share overlay0() —
        // deliberately deferred, theme.rs out of scope for T3), so this
        // test pins the glyph, which IS already 8-way distinct.
        let statuses = [
            SessionStatus::Starting,
            SessionStatus::Running,
            SessionStatus::WaitingApproval,
            SessionStatus::Completed,
            SessionStatus::Failed,
            SessionStatus::Interrupted,
            SessionStatus::Archived,
            SessionStatus::Deleted,
        ];
        let icons: Vec<&str> = statuses.iter().map(|&s| status_icon(s)).collect();
        let unique: std::collections::HashSet<&&str> = icons.iter().collect();
        assert_eq!(
            unique.len(),
            statuses.len(),
            "expected {} pairwise-distinct status icons, got {:?}",
            statuses.len(),
            icons
        );
    }

    // --- render_loading_strip tests ---

    #[test]
    fn render_loading_strip_zero_size_no_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(10, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                // Zero-area rect — must not panic.
                render_loading_strip(frame, Rect::new(0, 0, 0, 0));
                render_loading_strip(frame, Rect::new(0, 0, 1, 0));
                render_loading_strip(frame, Rect::new(0, 0, 0, 1));
            })
            .unwrap();
    }

    #[test]
    fn render_loading_strip_one_row_full_width() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let width: u16 = 20;
        let backend = TestBackend::new(width, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_loading_strip(frame, Rect::new(0, 0, width, 1));
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        // Every cell in row 0 should have a non-Reset fg color (palette Rgb).
        for x in 0..width {
            let cell = &buffer[(x, 0)];
            assert!(
                theme::test_rgb_channels(cell.fg).is_some(),
                "cell ({x},0) fg should be Rgb but was {:?}",
                cell.fg
            );
        }
    }

    #[test]
    fn render_loading_strip_palette_cycle() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::collections::HashSet;
        let palette = theme::rainbow_palette();
        let plen = palette.len() as u16;
        let backend = TestBackend::new(plen * 2, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_loading_strip(frame, Rect::new(0, 0, plen, 1));
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let fg_colors: HashSet<(u8, u8, u8)> = (0..plen)
            .filter_map(|x| theme::test_rgb_channels(buffer[(x, 0)].fg))
            .collect();
        // Should see at least plen/2 distinct fg colors across the strip.
        assert!(
            fg_colors.len() >= (plen as usize) / 2,
            "expected at least {} distinct fg colors, got {}",
            plen / 2,
            fg_colors.len()
        );
    }

    // --- render_loading_container tests ---

    #[test]
    fn render_loading_container_fills_reserved_area_without_border() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(40, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_loading_container(frame, Rect::new(0, 0, 40, LOADING_CONTAINER_HEIGHT));
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        for y in 0..LOADING_CONTAINER_HEIGHT {
            for x in 0..40 {
                let cell = &buffer[(x, y)];
                assert!(
                    theme::test_rgb_channels(cell.fg).is_some(),
                    "cell ({x},{y}) should be part of unboxed rainbow strip, got {:?}",
                    cell.fg
                );
            }
        }
    }

    #[test]
    fn render_loading_container_inner_fills_strip() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        // The strip fills all reserved rows and columns.
        let backend = TestBackend::new(20, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_loading_container(frame, Rect::new(0, 0, 20, LOADING_CONTAINER_HEIGHT));
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        for y in 0..LOADING_CONTAINER_HEIGHT {
            for x in 0..20u16 {
                let cell = &buffer[(x, y)];
                assert!(
                    theme::test_rgb_channels(cell.fg).is_some(),
                    "cell ({x},{y}) fg should be Rgb from rainbow strip, got {:?}",
                    cell.fg
                );
            }
        }
    }

    #[test]
    fn activity_indicator_styles_reserve_expected_heights() {
        assert_eq!(
            activity_indicator_height(ActivityIndicatorStyle::Semantic),
            1
        );
        assert_eq!(
            activity_indicator_height(ActivityIndicatorStyle::RainbowClassic),
            LOADING_CONTAINER_HEIGHT
        );
        assert_eq!(
            activity_indicator_height(ActivityIndicatorStyle::RainbowCompact),
            COMPACT_RAINBOW_HEIGHT
        );
    }

    #[test]
    fn semantic_activity_indicator_reports_work_without_a_box() {
        use crate::app::app_test_helpers;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (mut app, session_id) = app_test_helpers::with_session_detail();
        let state = app.sessions.get_mut(&session_id).expect("fixture state");
        state.session.status = SessionStatus::Running;
        state.session.work_time_ms = Some(193_000);
        state.events.push(ConversationEvent {
            id: 0,
            session_id,
            sequence: 2,
            event_type: EventType::ToolUse,
            role: Some(Role::Assistant),
            content: "read fixture".to_string(),
            tool_name: Some("Read".to_string()),
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        });

        let backend = TestBackend::new(60, 1);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render_activity_indicator(
                    frame,
                    Rect::new(0, 0, 60, 1),
                    state,
                    ActivityIndicatorStyle::Semantic,
                );
            })
            .expect("semantic indicator render");
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("working"), "missing semantic state:\n{text}");
        assert!(text.contains("1 tool"), "missing tool count:\n{text}");
        assert!(text.contains("3m 13s"), "missing work time:\n{text}");
        assert!(
            !text.contains('┌'),
            "semantic default should be unboxed:\n{text}"
        );
    }

    #[test]
    fn failed_session_detail_renders_terminal_reason_and_stop_reason() {
        use crate::app::app_test_helpers;
        use ratatui::{Terminal, backend::TestBackend};

        let (mut app, session_id) = app_test_helpers::with_session_detail();
        {
            let state = app
                .sessions
                .get_mut(&session_id)
                .expect("fixture session state");
            state.session.status = SessionStatus::Failed;
            state.session.terminal_reason = Some("provider exit".to_string());
            state.session.stop_reason = Some("max_tokens".to_string());
        }

        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render_session_detail(frame, Rect::new(0, 0, 120, 24), session_id, true, &app);
            })
            .expect("failed session detail render");

        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("[terminal: failed] provider exit"),
            "terminal reason should render prominently for Failed sessions:\n{text}"
        );
        assert!(
            text.contains("stop: max_tokens"),
            "stop reason should render alongside terminal reason:\n{text}"
        );
    }

    #[test]
    fn interrupted_session_detail_renders_terminal_reason() {
        use crate::app::app_test_helpers;
        use ratatui::{Terminal, backend::TestBackend};

        let (mut app, session_id) = app_test_helpers::with_session_detail();
        {
            let state = app
                .sessions
                .get_mut(&session_id)
                .expect("fixture session state");
            state.session.status = SessionStatus::Interrupted;
            state.session.terminal_reason = Some("user interrupt".to_string());
        }

        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render_session_detail(frame, Rect::new(0, 0, 120, 24), session_id, true, &app);
            })
            .expect("interrupted session detail render");

        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("[terminal: interrupted] user interrupt"),
            "terminal reason should render prominently for Interrupted sessions:\n{text}"
        );
    }

    #[test]
    fn terminal_session_detail_renders_last_error_event() {
        use crate::app::app_test_helpers;
        use ratatui::{Terminal, backend::TestBackend};

        let (mut app, session_id) = app_test_helpers::with_session_detail();
        {
            let state = app
                .sessions
                .get_mut(&session_id)
                .expect("fixture session state");
            state.session.status = SessionStatus::Failed;
            state.events.push(ConversationEvent {
                id: 2,
                session_id,
                sequence: 2,
                event_type: EventType::System,
                role: None,
                content: "provider stream failed: connection reset".to_string(),
                tool_name: None,
                tool_input: None,
                created_at: chrono::Utc::now(),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            });
            state.event_offsets = vec![0, 4];
            state.event_heights = vec![4, 4];
            state.total_content_height = 8;
        }

        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render_session_detail(frame, Rect::new(0, 0, 120, 24), session_id, true, &app);
            })
            .expect("terminal error render");

        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("[error] provider stream failed: connection reset"),
            "last available error event should render inline:\n{text}"
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn completed_session_with_failed_count_renders_clean_terminal_reason() {
        use crate::app::app_test_helpers;
        use ratatui::{Terminal, backend::TestBackend};

        let (mut app, session_id) = app_test_helpers::with_session_detail();
        let state = app.sessions.get_mut(&session_id).unwrap();
        state.session.status = SessionStatus::Completed;
        state.session.terminal_reason = Some("normal exit".into());
        state.events.push(ConversationEvent {
            id: 2,
            session_id,
            sequence: 2,
            event_type: EventType::ToolResult,
            role: None,
            content: "test result: ok. 37 passed; 0 failed".into(),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        });
        state.event_offsets = vec![0, 4];
        state.event_heights = vec![4, 4];
        state.total_content_height = 8;

        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| {
                render_session_detail(frame, Rect::new(0, 0, 120, 24), session_id, true, &app);
            })
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert_eq!(
            text.lines()
                .find(|line| line.contains("[terminal: completed]"))
                .unwrap()
                .trim(),
            "[terminal: completed] normal exit"
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn failed_session_prefers_structured_tool_error() {
        use crate::app::app_test_helpers;
        use ratatui::{Terminal, backend::TestBackend};

        let (mut app, session_id) = app_test_helpers::with_session_detail();
        let state = app.sessions.get_mut(&session_id).unwrap();
        state.session.status = SessionStatus::Failed;
        state.events.push(ConversationEvent {
            id: 2,
            session_id,
            sequence: 2,
            event_type: EventType::ToolResult,
            role: None,
            content: "connection reset".into(),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: Some(Box::new(serde_json::json!({"is_error": true}))),
        });
        state.event_offsets = vec![0, 4];
        state.event_heights = vec![4, 4];
        state.total_content_height = 8;

        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| {
                render_session_detail(frame, Rect::new(0, 0, 120, 24), session_id, true, &app);
            })
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("[error] connection reset"), "{text}");
    }

    #[test]
    fn compact_rainbow_indicator_fills_reserved_area_without_border() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use crate::app::app_test_helpers;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (app, session_id) = app_test_helpers::with_session_detail();
        let state = app.sessions.get(&session_id).expect("fixture state");
        let backend = TestBackend::new(20, COMPACT_RAINBOW_HEIGHT);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render_activity_indicator(
                    frame,
                    Rect::new(0, 0, 20, COMPACT_RAINBOW_HEIGHT),
                    state,
                    ActivityIndicatorStyle::RainbowCompact,
                );
            })
            .expect("compact rainbow render");
        let buffer = terminal.backend().buffer();
        for y in 0..COMPACT_RAINBOW_HEIGHT {
            for x in 0..20 {
                let cell = &buffer[(x, y)];
                assert_eq!(cell.symbol(), "█");
                assert!(
                    theme::rainbow_palette().contains(&cell.fg),
                    "compact cell ({x},{y}) should use the theme-relative rainbow palette"
                );
            }
        }
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        let area = buffer.area;
        let mut out = String::new();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn buffer_text_position(buffer: &ratatui::buffer::Buffer, needle: &str) -> Option<(u16, u16)> {
        let needle = needle.chars().map(|ch| ch.to_string()).collect::<Vec<_>>();
        if needle.is_empty() || needle.len() > buffer.area.width as usize {
            return None;
        }
        for y in buffer.area.y..buffer.area.y + buffer.area.height {
            for x in buffer.area.x..=buffer.area.x + buffer.area.width - needle.len() as u16 {
                if needle
                    .iter()
                    .enumerate()
                    .all(|(offset, symbol)| buffer[(x + offset as u16, y)].symbol() == symbol)
                {
                    return Some((x, y));
                }
            }
        }
        None
    }

    #[test]
    fn manager_band_renders_header_and_m_marker_with_title() {
        use crate::app::app_test_helpers::{baseline_session, with_session_list};
        use crate::app::manager_roster::{ManagerRosterEntry, ManagerTier};
        use crate::types::SplitNode;
        use ratatui::{Terminal, backend::TestBackend};
        use rsi_common::types::SessionKind;

        let mut app = with_session_list(0);
        let manager_id = uuid::Uuid::new_v4();
        let epic_id = uuid::Uuid::new_v4();
        let worker_id = uuid::Uuid::new_v4();
        let mut manager = baseline_session(manager_id, SessionKind::Standard);
        manager.title = Some("Coordination desk".into());
        let mut epic = baseline_session(epic_id, SessionKind::Epic);
        epic.title = Some("Build feature".into());
        let mut worker = baseline_session(worker_id, SessionKind::Feature);
        worker.title = Some("Compile worker".into());
        worker.parent_id = Some(epic_id);
        worker.epic_spawn_ordinal = Some(3);
        app.update_sessions(vec![manager, epic, worker]);
        let Some(epic_state) = app.sessions.get_mut(&epic_id) else {
            panic!("fixture Epic must be present");
        };
        epic_state.list_card_expanded = true;
        app.manager_roster.by_project.insert(
            uuid::Uuid::new_v4(),
            ManagerRosterEntry {
                session_id: manager_id,
                tier: ManagerTier::Project,
            },
        );
        app.recalculate_filtered_order();
        let SplitNode::Leaf { pane, .. } = &app.tabs[0].layout else {
            panic!("fixture must have one list pane");
        };
        let pane = pane.clone();
        let Ok(mut terminal) = Terminal::new(TestBackend::new(120, 30)) else {
            panic!("test terminal must initialize");
        };
        assert!(
            terminal
                .draw(|frame| {
                    render_session_list(
                        frame,
                        Rect::new(0, 0, 120, 30),
                        &pane,
                        true,
                        &mut app,
                        SessionListSurface::Full,
                        None,
                    );
                })
                .is_ok()
        );
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("MANAGERS 1"), "{text}");
        let Some(manager_line) = text.lines().find(|line| line.contains("Coordination desk"))
        else {
            panic!("manager row must be rendered: {text}");
        };
        assert!(manager_line.contains('M'), "{manager_line}");
        let Some(worker_line) = text.lines().find(|line| line.contains("Compile worker")) else {
            panic!("worker row must be rendered: {text}");
        };
        assert!(worker_line.contains('3'), "{worker_line}");
        assert!(text.contains("IN FLIGHT"), "{text}");
    }

    #[test]
    fn manager_section_is_followed_by_the_same_blank_separator_as_other_sections() {
        use crate::app::app_test_helpers::{baseline_session, with_session_list};
        use crate::app::manager_roster::{ManagerRosterEntry, ManagerTier};
        use crate::types::SplitNode;
        use ratatui::{Terminal, backend::TestBackend};
        use rsi_common::types::SessionKind;

        let mut app = with_session_list(0);
        let manager_id = uuid::Uuid::new_v4();
        let mut manager = baseline_session(manager_id, SessionKind::Standard);
        manager.title = Some("Coordination desk".into());
        let mut worker = baseline_session(uuid::Uuid::new_v4(), SessionKind::Feature);
        worker.title = Some("Compile worker".into());
        app.update_sessions(vec![manager, worker]);
        app.manager_roster.by_project.insert(
            uuid::Uuid::new_v4(),
            ManagerRosterEntry {
                session_id: manager_id,
                tier: ManagerTier::Project,
            },
        );
        app.recalculate_filtered_order();
        let SplitNode::Leaf { pane, .. } = &app.tabs[0].layout else {
            panic!("fixture must have one list pane");
        };
        let pane = pane.clone();
        // 260 columns selects the three-pane relay navigator, whose sections
        // use blank-row separators (the operational table always does).
        let Ok(mut terminal) = Terminal::new(TestBackend::new(260, 40)) else {
            panic!("test terminal must initialize");
        };
        assert!(
            terminal
                .draw(|frame| {
                    render_session_list(
                        frame,
                        Rect::new(0, 0, 260, 40),
                        &pane,
                        true,
                        &mut app,
                        SessionListSurface::Full,
                        None,
                    );
                })
                .is_ok()
        );
        let text = buffer_text(terminal.backend().buffer());
        // Only the navigator pane: everything left of the inspector border.
        let lines = text
            .lines()
            .map(|line| line.split('│').next().unwrap_or(line))
            .collect::<Vec<_>>();
        let Some(manager_row) = lines
            .iter()
            .position(|line| line.contains("Coordination desk"))
        else {
            panic!("manager row must be rendered: {text}");
        };
        assert_eq!(
            lines.get(manager_row + 1).map(|line| line.trim()),
            Some(""),
            "a blank separator follows the MANAGERS section:\n{text}"
        );
        assert!(
            lines
                .get(manager_row + 2)
                .is_some_and(|line| line.contains("IN FLIGHT")),
            "the next section header sits below that separator:\n{text}"
        );
    }

    #[test]
    fn leaf_role_titles_render_a_role_code_and_containers_keep_their_names() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use ratatui::{Terminal, backend::TestBackend};
        use rsi_common::types::SessionKind;

        let settings = crate::settings::UserSettings::default();
        let render = |kind: SessionKind, title: &str| {
            let mut row = crate::types::row::SessionRowViewModel::placeholder(uuid::Uuid::new_v4());
            row.session_kind = kind;
            row.display_title = title.to_string();
            let mut terminal = Terminal::new(TestBackend::new(120, 1)).expect("terminal");
            terminal
                .draw(|frame| {
                    render_navigator_row(
                        frame,
                        Rect::new(0, 0, 120, 1),
                        &row,
                        None,
                        &settings,
                        2,
                        false,
                        false,
                        false,
                        theme::blue(),
                        theme::tier_panel(),
                    );
                })
                .expect("render row");
            let buffer = terminal.backend().buffer().clone();
            (buffer_text(&buffer), buffer)
        };

        let (leaf, buffer) = render(SessionKind::Standard, "Refactorer: Keybinding Cleanup");
        assert!(leaf.contains("Rf Keybinding Cleanup"), "{leaf}");
        let (x, y) = buffer_text_position(&buffer, "Rf").expect("role code cell");
        assert_eq!(buffer[(x, y)].fg, glyphs::role_color("Refactorer"));

        let (reviewer, _) = render(SessionKind::Standard, "Reviewer: RSI-014");
        assert!(reviewer.contains("Rv RSI-014"), "{reviewer}");

        let (group, _) = render(SessionKind::Group, "Manager: Reliability");
        assert!(
            group.contains("Manager: Reliability"),
            "containers keep their own full name: {group}"
        );
    }

    #[test]
    fn title_named_manager_is_classified_by_roster_not_title() {
        use crate::app::app_test_helpers::{baseline_session, with_session_list};
        use crate::types::SplitNode;
        use ratatui::{Terminal, backend::TestBackend};
        use rsi_common::types::SessionKind;

        let mut app = with_session_list(0);
        let id = uuid::Uuid::new_v4();
        let mut session = baseline_session(id, SessionKind::Standard);
        session.title = Some("Manager".into());
        app.update_sessions(vec![session]);
        let SplitNode::Leaf { pane, .. } = &app.tabs[0].layout else {
            panic!("fixture must have one list pane");
        };
        let pane = pane.clone();
        let Ok(mut terminal) = Terminal::new(TestBackend::new(120, 20)) else {
            panic!("test terminal must initialize");
        };
        assert!(
            terminal
                .draw(|frame| {
                    render_session_list(
                        frame,
                        Rect::new(0, 0, 120, 20),
                        &pane,
                        true,
                        &mut app,
                        SessionListSurface::Full,
                        None,
                    );
                })
                .is_ok()
        );
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("IN FLIGHT"), "{text}");
        let Some(row) = text.lines().find(|line| line.contains("Manager")) else {
            panic!("title row must be rendered: {text}");
        };
        assert!(row.contains("Manager"), "{row}");
        assert_eq!(app.filtered_session_order, vec![id]);
    }

    #[test]
    fn render_fixture_session_list_without_daemon() {
        use crate::app::app_test_helpers;
        use crate::types::SplitNode;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app_test_helpers::with_session_list(30);
        let focus_index =
            crate::types::compute_session_focus_index(&app.sessions, chrono::Utc::now());
        app.filtered_session_order.sort_by_key(|id| {
            focus_index
                .get(id)
                .map_or(SessionFocusGroup::Quiet, |entry| entry.group)
        });
        let pane = match &app.tabs[0].layout {
            SplitNode::Leaf { pane, .. } => pane.clone(),
            _ => unreachable!("fixture should use a single session-list pane"),
        };
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                render_session_list(
                    frame,
                    Rect::new(0, 0, 120, 40),
                    &pane,
                    true,
                    &mut app,
                    SessionListSurface::Full,
                    None,
                );
            })
            .unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("Fixture session"),
            "session list fixture should render populated rows:\n{text}"
        );
        let rendered_rows = text.matches("Fixture session").count();
        assert!(
            rendered_rows >= 22,
            "operational table should preserve one-row triage density, rendered {rendered_rows}:\n{text}"
        );
        assert!(
            text.contains("All projects / Sessions"),
            "scope header should identify project and zone:\n{text}"
        );
        assert!(
            text.contains("#   ∞  S  !  FUNCTION")
                && text.contains(glyphs::CONTEXT)
                && text.contains(glyphs::TURNS),
            "navigator table should render the required fixed-column replacement:\n{text}"
        );
        assert!(
            text.contains("\u{258C}"),
            "selected row rail should be visible:\n{text}"
        );
        assert!(
            !text.contains("j/k move"),
            "normal session list must not reserve a persistent hint row:\n{text}"
        );
        assert!(
            app.session_list_render.cards_area.is_some(),
            "fixture render should populate list geometry for future structural tests"
        );
    }

    #[test]
    fn t09_selected_session_inspector_renders_positive_summary_and_description_destinations() {
        use crate::app::app_test_helpers::{baseline_session, with_session_list};
        use ratatui::{Terminal, backend::TestBackend};
        use rsi_common::types::SessionKind;

        for (status, summary, description) in [
            (
                SessionStatus::Completed,
                "t09-completed-summary",
                "t09-completed-description",
            ),
            (
                SessionStatus::Running,
                "t09-running-summary",
                "t09-running-description",
            ),
            (
                SessionStatus::Failed,
                "t09-failed-summary",
                "t09-failed-description",
            ),
            (
                SessionStatus::Archived,
                "t09-inactive-summary",
                "t09-inactive-description",
            ),
        ] {
            let mut app = with_session_list(1);
            let session_id = app.filtered_session_order[0];
            let mut session = baseline_session(session_id, SessionKind::Task);
            session.status = status;
            session.title = Some(format!("t09-selected-{status:?}"));
            session.short_summary = Some(summary.to_string());
            session.description = Some(description.to_string());
            if status == SessionStatus::Failed {
                session.stop_reason = Some("t09-failed-stop-reason".to_string());
            }
            app.sessions
                .insert(session_id, crate::types::SessionState::new(session));
            let inspector = crate::types::compute_session_inspector(&app, session_id, 1)
                .expect("selected inspector projection");
            let mut terminal = Terminal::new(TestBackend::new(120, 80)).expect("inspector");
            terminal
                .draw(|frame| {
                    render_session_inspector(
                        frame,
                        Rect::new(0, 0, 120, 80),
                        Some(&inspector),
                        app.sessions.get(&session_id).map(|state| &state.session),
                        theme::tier_panel(),
                    );
                })
                .expect("selected inspector render");
            let text = buffer_text(terminal.backend().buffer());
            assert!(
                text.contains(summary) && text.contains(description),
                "{status:?} inspector must render its distinct summary and description: {text}"
            );
            // Summary and description are unlabeled prose: summary first in
            // primary text, description after it in muted text.
            let buffer = terminal.backend().buffer();
            let Some((summary_x, summary_y)) = buffer_text_position(buffer, summary) else {
                panic!("{status:?} rendered summary prose: {text}");
            };
            let Some((description_x, description_y)) = buffer_text_position(buffer, description)
            else {
                panic!("{status:?} rendered description prose: {text}");
            };
            assert!(
                summary_y < description_y,
                "{status:?} summary precedes description: {text}"
            );
            assert_eq!(buffer[(summary_x, summary_y)].fg, theme::text());
            assert_eq!(buffer[(description_x, description_y)].fg, theme::subtext1());
            if status == SessionStatus::Running {
                assert_eq!(
                    text.matches(summary).count(),
                    1,
                    "Running inspector must render its known summary once: {text}"
                );
                assert_eq!(
                    text.matches(description).count(),
                    1,
                    "Running inspector must render its known description once: {text}"
                );
            }
        }

        let mut app = with_session_list(1);
        let session_id = app.filtered_session_order[0];
        let mut session = baseline_session(session_id, SessionKind::Task);
        session.status = SessionStatus::Running;
        session.title = Some("t09-normalized-duplicate-session".to_string());
        session.short_summary = Some("t09 normalized retained value".to_string());
        session.description = Some(" T09   NORMALIZED retained VALUE ".to_string());
        app.sessions
            .insert(session_id, crate::types::SessionState::new(session));
        let inspector = crate::types::compute_session_inspector(&app, session_id, 1)
            .expect("normalized duplicate inspector projection");
        let mut terminal = Terminal::new(TestBackend::new(120, 80)).expect("inspector");
        terminal
            .draw(|frame| {
                render_session_inspector(
                    frame,
                    Rect::new(0, 0, 120, 80),
                    Some(&inspector),
                    app.sessions.get(&session_id).map(|state| &state.session),
                    theme::tier_panel(),
                );
            })
            .expect("normalized duplicate inspector render");
        let text = buffer_text(terminal.backend().buffer());
        assert_eq!(
            text.matches("t09 normalized retained value").count(),
            1,
            "normalized duplicate preserves one positive inspector destination: {text}"
        );
    }

    #[test]
    fn wide_relay_keeps_a_scrolled_selection_tethered_to_the_inspector() {
        use crate::app::app_test_helpers;
        use crate::types::SplitNode;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app_test_helpers::with_session_list(45);
        let focus_index =
            crate::types::compute_session_focus_index(&app.sessions, chrono::Utc::now());
        app.filtered_session_order.sort_by_key(|id| {
            focus_index
                .get(id)
                .map_or(SessionFocusGroup::Quiet, |entry| entry.group)
        });
        let selected_index = app.filtered_session_order.len() - 2;
        let selected_id = app.filtered_session_order[selected_index];
        if let SplitNode::Leaf { pane, .. } = &mut app.tabs[0].layout
            && let Pane::SessionList {
                selected_index: pane_index,
                selected_session,
                ..
            } = pane
        {
            *pane_index = selected_index;
            *selected_session = Some(selected_id);
        }
        let pane = match &app.tabs[0].layout {
            SplitNode::Leaf { pane, .. } => pane.clone(),
            _ => unreachable!("fixture should use a single session-list pane"),
        };
        let backend = TestBackend::new(200, 40);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                render_session_list(
                    frame,
                    Rect::new(0, 0, 200, 40),
                    &pane,
                    true,
                    &mut app,
                    SessionListSurface::Full,
                    None,
                );
            })
            .unwrap();

        let cards = app
            .session_list_render
            .cards_area
            .expect("navigator cards area");
        let zone = &app.session_list_render.main;
        assert!(zone.scroll_offset > 0, "bottom selection should scroll");
        let selected_offset: u16 = zone.card_offsets[selected_index]
            .saturating_sub(zone.scroll_offset)
            .try_into()
            .expect("selected row y fits u16");
        let selected_y = cards.y + selected_offset;
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(113, selected_y)].symbol(), "─");
        assert_eq!(buffer[(114, selected_y)].symbol(), "─");
        assert_eq!(
            buffer[(115, selected_y)].symbol(),
            "│",
            "the tether should meet the inspector focus rail"
        );
    }

    #[test]
    fn bounded_browser_uses_neutral_labels_and_muted_decorative_chrome() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use crate::app::app_test_helpers;
        use crate::types::SplitNode;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app_test_helpers::with_session_list(30);
        let waiting_id = app.filtered_session_order[1];
        app.sessions
            .get_mut(&waiting_id)
            .expect("waiting fixture session")
            .session
            .status = SessionStatus::WaitingApproval;
        let pane = match &app.tabs[0].layout {
            SplitNode::Leaf { pane, .. } => pane.clone(),
            _ => unreachable!("fixture should use a single session-list pane"),
        };
        let backend = TestBackend::new(240, 70);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                render_session_list(
                    frame,
                    Rect::new(0, 0, 240, 70),
                    &pane,
                    true,
                    &mut app,
                    SessionListSurface::Full,
                    None,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        for (label, expected_color) in [
            (glyphs::WORKING_DIR, theme::subtext0()),
            (glyphs::CREATED, theme::subtext0()),
            ("LIVE ACTIVITY", theme::browser_section_label()),
            ("QUEUE", theme::subtext0()),
            ("CHANGES", theme::subtext0()),
            ("FLOW", theme::subtext0()),
        ] {
            let (x, y) = buffer_text_position(buffer, label)
                .unwrap_or_else(|| panic!("missing browser label {label}"));
            assert_eq!(
                buffer[(x, y)].fg,
                expected_color,
                "{label} must use its browser label role"
            );
        }

        let layout = resolve_session_browser_layout(
            Rect::new(0, 1, 240, 69),
            SessionListSurface::Full,
            crate::types::SessionListZone::Main,
        );
        let inspector = layout.inspector.expect("240-wide inspector");
        let tether = layout.tether.expect("240-wide tether");
        let activity = layout.activity.expect("240-wide activity");
        let inspector_inner_x = inspector.x + 3;
        let inspector_header_y = inspector.y + 1;
        let navigator_rule_x = layout.navigator.x + 2;
        let navigator_rule_y = layout.navigator.y + layout.navigator.height.saturating_sub(5);
        let selected_row_y = app
            .session_list_render
            .cards_area
            .expect("navigator cards area")
            .y
            + app.session_list_render.main.card_offsets[0] as u16
            - app.session_list_render.main.scroll_offset as u16;
        assert_eq!(
            buffer[(inspector.x, inspector.y)].fg,
            theme::active_row_rail(),
            "the selected inspector rail must retain the active-row role"
        );
        assert_eq!(
            buffer[(tether.x, selected_row_y)].fg,
            theme::active_row_rail(),
            "the selected inspector tether must retain the active-row role"
        );
        assert_eq!(
            buffer[(navigator_rule_x, navigator_rule_y)].fg,
            theme::browser_decorative_separator(),
            "the navigator bottom rule must use muted decorative chrome"
        );
        assert_eq!(
            buffer[(inspector_inner_x, inspector_header_y + 1)].fg,
            theme::browser_decorative_separator(),
            "the inspector header rule must use muted decorative chrome"
        );
        assert_eq!(
            buffer[(activity.x, activity.y)].fg,
            theme::browser_decorative_separator(),
            "the passive activity divider must use muted decorative chrome"
        );
        assert_eq!(
            buffer[(activity.x + 3, activity.y + 2)].fg,
            theme::browser_decorative_separator(),
            "the activity header rule must use muted decorative chrome"
        );
    }

    #[test]
    fn render_fixture_session_list_wide_tier_renders_new_columns() {
        // NEW — closes F-035: no existing test (snapshot or direct) ever
        // rendered the session list at width >= 160, so the title-width bug
        // (F-006/F-007) and the hardcoded "Claude" last-event label
        // (F-017/F-018) both shipped with zero automated coverage. This test
        // exercises the production navigator renderer at wide width directly.
        use crate::app::app_test_helpers::{self, baseline_session};
        use crate::types::{PaneId, SplitNode};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use rsi_common::types::{SandboxKind, SessionKind, SessionProvider, SessionStatus};

        // Start from the established list-fixture scaffolding (Tab/Pane
        // wiring), then replace its sessions with this test's specific rows.
        let mut app = app_test_helpers::with_session_list(1);
        app.sessions.clear();
        app.filtered_session_order.clear();

        let mut order = Vec::new();

        // claude-sonnet-5 session — pins F-011's correctness at the
        // previously-uncovered width tier (must render "sonnet", not "opus").
        let sonnet_id = uuid::Uuid::new_v4();
        let mut sonnet = baseline_session(sonnet_id, SessionKind::Standard);
        sonnet.title = Some("Sonnet fixture row".to_string());
        sonnet.model = Some("claude-sonnet-5".to_string());
        app.sessions.insert(sonnet_id, SessionState::new(sonnet));
        order.push(sonnet_id);

        // Sandboxed session — asserts the sandbox glyph renders in its own
        // dedicated column, not crowded out (F-020/F-010).
        let sandboxed_id = uuid::Uuid::new_v4();
        let mut sandboxed = baseline_session(sandboxed_id, SessionKind::Standard);
        sandboxed.title = Some("Sandboxed fixture row".to_string());
        sandboxed.sandbox_kind = Some(SandboxKind::GitWorktree);
        app.sessions
            .insert(sandboxed_id, SessionState::new(sandboxed));
        order.push(sandboxed_id);

        // Group → Epic with 2 sessions, one running. The Group must report
        // the scoped 1/3 count (Epic plus both descendant sessions), not just
        // its direct completed Epic child.
        let group_id = uuid::Uuid::new_v4();
        let mut group = baseline_session(group_id, SessionKind::Group);
        group.title = Some("Group fixture row".to_string());
        app.sessions.insert(group_id, SessionState::new(group));
        order.push(group_id);

        let epic_id = uuid::Uuid::new_v4();
        let mut epic = baseline_session(epic_id, SessionKind::Epic);
        epic.parent_id = Some(group_id);
        epic.status = SessionStatus::Completed;
        app.sessions.insert(epic_id, SessionState::new(epic));

        let running_child_id = uuid::Uuid::new_v4();
        let mut running_child = baseline_session(running_child_id, SessionKind::Standard);
        running_child.parent_id = Some(epic_id);
        running_child.status = SessionStatus::Running;
        app.sessions
            .insert(running_child_id, SessionState::new(running_child));

        let done_child_id = uuid::Uuid::new_v4();
        let mut done_child = baseline_session(done_child_id, SessionKind::Standard);
        done_child.parent_id = Some(epic_id);
        done_child.status = SessionStatus::Completed;
        app.sessions
            .insert(done_child_id, SessionState::new(done_child));

        // Provider != Claude — asserts the last-event column shows that
        // provider's glyph, never the literal word "Claude" (F-017).
        let codex_id = uuid::Uuid::new_v4();
        let mut codex = baseline_session(codex_id, SessionKind::Standard);
        codex.title = Some("Codex fixture row".to_string());
        codex.provider = SessionProvider::Codex;
        codex.status = SessionStatus::Completed;
        app.sessions.insert(codex_id, SessionState::new(codex));
        order.push(codex_id);

        let selected_session = order.first().copied();
        app.filtered_session_order = order;
        app.tabs[0].layout = SplitNode::Leaf {
            pane: Pane::SessionList {
                selected_index: 0,
                selected_session,
                scroll_offset: 0,
                active_zone: Default::default(),
                taskrabbit_selected_index: 0,
                archive_selected_index: 0,
                jobs_selected_index: 0,
            },
            id: PaneId(0),
        };

        let pane = match &app.tabs[0].layout {
            SplitNode::Leaf { pane, .. } => pane.clone(),
            _ => unreachable!("fixture should use a single session-list pane"),
        };
        // width=180, comfortably above the >=160 wide-tier threshold.
        let backend = TestBackend::new(180, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                render_session_list(
                    frame,
                    Rect::new(0, 0, 180, 20),
                    &pane,
                    true,
                    &mut app,
                    SessionListSurface::Embedded,
                    None,
                );
            })
            .unwrap();

        let text = buffer_text(terminal.backend().buffer());

        // Wide-tier's KIND/MODEL column renders the long abbreviated form
        // (`abbreviate_model`, "Sonnet 5"),
        // not `short_model_label`'s short form ("sonnet") — that's the
        // Compact row's job (already pinned by `snap_session_list_initial`,
        // F-012). Verified directly by running this fixture: this asserts
        // the family-correctness invariant (F-011) in the form that's
        // actually rendered at this width tier.
        assert!(
            text.contains("Sonnet"),
            "wide-tier row must render a Sonnet-family model label for \
             claude-sonnet-5:\n{text}"
        );
        assert!(
            text.contains("#   ∞  S  !  FUNCTION")
                && text.contains(glyphs::CONTEXT)
                && text.contains(glyphs::TURNS),
            "wide-tier header must expose the fixed navigator columns:\n{text}"
        );
        for header in ["MODEL", glyphs::EFFORT] {
            assert!(
                text.contains(header),
                "wide-tier navigator admits separate {header} after AGE:\n{text}"
            );
        }
        assert!(
            text.contains(&format!(
                "{} sonnet-5",
                glyphs::provider_glyph(rsi_common::types::SessionProvider::Claude)
            )),
            "wide-tier row pairs the provider glyph with the versioned model suffix:\n{text}"
        );

        let mut standard_terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        standard_terminal
            .draw(|frame| {
                render_session_list(
                    frame,
                    Rect::new(0, 0, 120, 20),
                    &pane,
                    true,
                    &mut app,
                    SessionListSurface::Embedded,
                    None,
                );
            })
            .unwrap();
        let standard_text = buffer_text(standard_terminal.backend().buffer());
        assert!(
            standard_text.contains("#   ∞  S  !  FUNCTION"),
            "standard-tier navigator keeps required columns:\n{standard_text}"
        );

        let mut compact_terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        compact_terminal
            .draw(|frame| {
                render_session_list(
                    frame,
                    Rect::new(0, 0, 40, 20),
                    &pane,
                    true,
                    &mut app,
                    SessionListSurface::Embedded,
                    None,
                );
            })
            .unwrap();
        let compact_text = buffer_text(compact_terminal.backend().buffer());
        assert!(
            compact_text.contains("#   ∞  S  !"),
            "compact navigator keeps its one-line required prefix:\n{compact_text}"
        );
    }

    #[test]
    fn t08_embedded_navigator_rows_are_one_line_with_adjacent_row_offsets() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use crate::app::app_test_helpers;
        use crate::types::SplitNode;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app_test_helpers::with_session_list(30);
        let viewed_session_id = app.filtered_session_order[1];
        let pane = match &app.tabs[0].layout {
            SplitNode::Leaf { pane, .. } => pane.clone(),
            _ => unreachable!("fixture should use a single session-list pane"),
        };
        let backend = TestBackend::new(40, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                render_session_list(
                    frame,
                    Rect::new(0, 0, 40, 24),
                    &pane,
                    true,
                    &mut app,
                    SessionListSurface::Embedded,
                    Some(viewed_session_id),
                );
            })
            .unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("#   ∞  S  !  FUNCTION"),
            "sidebar has the shared fixed column sequence:\n{text}"
        );

        let cards_area = app
            .session_list_render
            .cards_area
            .expect("compact list cards area");
        let cursor_y = cards_area.y + app.session_list_render.main.card_offsets[0] as u16;
        let viewed_y = cards_area.y + app.session_list_render.main.card_offsets[1] as u16;
        assert_eq!(
            app.session_list_render.main.card_heights[0], 1,
            "selected navigator session occupies one physical row"
        );
        assert_eq!(
            app.session_list_render.main.card_heights[1], 1,
            "viewed navigator session occupies one physical row"
        );
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer[(0, cursor_y)].bg,
            theme::selected_row_bg(),
            "the list cursor should retain the strong selection color"
        );
        assert_eq!(
            buffer[(0, viewed_y)].bg,
            theme::viewed_session_row_bg(),
            "the open detail session should retain a subdued selection color"
        );
        assert_ne!(
            buffer[(0, cursor_y)].bg,
            buffer[(0, viewed_y)].bg,
            "cursor and viewed-session treatments must remain visually distinct"
        );
    }

    #[test]
    fn t08_two_known_rows_stay_one_line_across_expansion_zones_and_surfaces() {
        use crate::app::app_test_helpers::with_session_list;
        use ratatui::{Terminal, backend::TestBackend};

        let main_mode_cases: [(
            &str,
            SessionListSurface,
            crate::types::SessionListZone,
            u16,
            u16,
            SessionBrowserMode,
        ); 4] = [
            (
                "legacy-main-full",
                SessionListSurface::Full,
                crate::types::SessionListZone::Main,
                119,
                40,
                SessionBrowserMode::LegacyTable,
            ),
            (
                "operational-main-full",
                SessionListSurface::Full,
                crate::types::SessionListZone::Main,
                120,
                40,
                SessionBrowserMode::OperationalTable,
            ),
            (
                "relay-main-full",
                SessionListSurface::Full,
                crate::types::SessionListZone::Main,
                174,
                25,
                SessionBrowserMode::Relay,
            ),
            (
                "relay-activity-main-full",
                SessionListSurface::Full,
                crate::types::SessionListZone::Main,
                228,
                25,
                SessionBrowserMode::RelayWithActivity,
            ),
        ];
        let legacy_surface_zone_cases = [
            (
                "full-taskrabbit",
                SessionListSurface::Full,
                crate::types::SessionListZone::TaskRabbit,
            ),
            (
                "full-archive",
                SessionListSurface::Full,
                crate::types::SessionListZone::Archive,
            ),
            (
                "full-jobs",
                SessionListSurface::Full,
                crate::types::SessionListZone::Jobs,
            ),
            (
                "embedded-main",
                SessionListSurface::Embedded,
                crate::types::SessionListZone::Main,
            ),
            (
                "embedded-taskrabbit",
                SessionListSurface::Embedded,
                crate::types::SessionListZone::TaskRabbit,
            ),
            (
                "embedded-archive",
                SessionListSurface::Embedded,
                crate::types::SessionListZone::Archive,
            ),
            (
                "embedded-jobs",
                SessionListSurface::Embedded,
                crate::types::SessionListZone::Jobs,
            ),
        ];

        for expanded in [false, true] {
            for (case, surface, zone, width, height, expected_mode) in main_mode_cases
                .iter()
                .copied()
                .chain(
                    legacy_surface_zone_cases
                        .iter()
                        .map(|(case, surface, zone)| {
                            let expected_mode = if *surface == SessionListSurface::Embedded
                                && *zone == crate::types::SessionListZone::Main
                            {
                                SessionBrowserMode::EmbeddedInspector
                            } else {
                                SessionBrowserMode::LegacyTable
                            };
                            (*case, *surface, *zone, 240, 40, expected_mode)
                        }),
                )
            {
                let mut app = with_session_list(2);
                let ids = app.filtered_session_order[..2].to_vec();
                for (index, (id, title)) in ids
                    .iter()
                    .zip(["t08-function-alpha", "t08-function-beta"])
                    .enumerate()
                {
                    let state = app.sessions.get_mut(id).expect("known session");
                    state.session.status = SessionStatus::Completed;
                    state.session.title = Some(title.to_string());
                    if index == 0 {
                        state.session.short_summary = Some("t08-selected-summary".to_string());
                        state.session.description = Some("t08-selected-description".to_string());
                    }
                    state.list_card_expanded = expanded;
                }
                app.filtered_session_order = ids.clone();
                app.filtered_taskrabbit_order = ids.clone();
                app.filtered_archived_order = ids.clone();
                app.filtered_jobs_order = ids;
                let selected_id = app.filtered_session_order[0];
                if let Pane::SessionList {
                    active_zone,
                    selected_index,
                    selected_session,
                    ..
                } = app.session_list_pane_mut()
                {
                    *active_zone = zone;
                    *selected_index = 0;
                    *selected_session = Some(selected_id);
                }
                let pane = app.focused_pane().expect("list pane").clone();
                let content = Rect::new(0, 1, width, height.saturating_sub(1));
                assert_eq!(
                    resolve_session_browser_layout(content, surface, zone).mode,
                    expected_mode,
                    "{case} resolves its intended browser mode"
                );
                let mut terminal =
                    Terminal::new(TestBackend::new(width, height)).expect("terminal");
                terminal
                    .draw(|frame| {
                        render_session_list(
                            frame,
                            frame.area(),
                            &pane,
                            true,
                            &mut app,
                            surface,
                            None,
                        )
                    })
                    .expect("render list");
                let state = match zone {
                    crate::types::SessionListZone::Main => &app.session_list_render.main,
                    crate::types::SessionListZone::TaskRabbit => {
                        &app.session_list_render.taskrabbit
                    }
                    crate::types::SessionListZone::Archive => &app.session_list_render.archive,
                    crate::types::SessionListZone::Jobs => &app.session_list_render.jobs,
                };
                assert_eq!(
                    &state.card_heights[..2],
                    &[1, 1],
                    "{expanded:?} {surface:?} {zone:?}"
                );
                assert_eq!(
                    state.card_offsets[1],
                    state.card_offsets[0] + 1,
                    "{case} {expanded:?}"
                );
                let buffer = terminal.backend().buffer();
                let first_function = buffer_text_position(buffer, "t08-function-alpha")
                    .expect("selected known FUNCTION text renders exactly once");
                let second_function = buffer_text_position(buffer, "t08-function-beta")
                    .expect("second known FUNCTION text renders exactly once");
                assert_eq!(
                    second_function.0, first_function.0,
                    "{case} {expanded:?}: known FUNCTION cells share their x-offset"
                );
                assert_eq!(
                    second_function.1,
                    first_function.1 + 1,
                    "{case} {expanded:?}: second FUNCTION text begins one rendered row below"
                );
            }
        }
    }

    #[test]
    fn render_fixture_session_detail_without_daemon() {
        use crate::app::app_test_helpers;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (app, session_id) = app_test_helpers::with_session_detail();
        let backend = TestBackend::new(120, 32);
        let mut terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => panic!("test terminal should initialize: {error}"),
        };

        if let Err(error) = terminal.draw(|frame| {
            render_session_detail(frame, Rect::new(0, 0, 120, 32), session_id, true, &app);
        }) {
            panic!("session detail render should succeed: {error}");
        }

        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("Fixture assistant response"),
            "session detail fixture should render populated content:\n{text}"
        );
    }

    #[test]
    fn render_plain_assistant_partial_scroll_does_not_leak_hidden_border() {
        use crate::app::app_test_helpers;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use rsi_common::types::SessionStatus;

        let (mut app, session_id) = app_test_helpers::with_session_detail();
        let area = Rect::new(0, 0, 80, 16);
        let content = (0..40)
            .map(|idx| format!("assistant line {idx:02}"))
            .collect::<Vec<_>>()
            .join("\n");

        {
            let state = app
                .sessions
                .get_mut(&session_id)
                .expect("fixture session state");
            state.session.status = SessionStatus::Completed;
            state.events[0].content = content;
            state.events_generation += 1;
            state.current_event_index = None;
            state.follow_tail = false;
            state.scroll_offset = 4;
            crate::ui::height::update_event_heights(
                state,
                area.width.saturating_sub(theme::SESSION_DETAIL_HORIZ_INSET),
            );
        }

        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_session_detail(frame, area, session_id, true, &app);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let text = buffer_text(buffer);
        for y in SESSION_DETAIL_HEADER_HEIGHT..area.height {
            for x in 2..area.width.saturating_sub(2) {
                let symbol = buffer[(x, y)].symbol();
                assert!(
                    !matches!(symbol, "┌" | "┐" | "└" | "┘" | "─"),
                    "plain assistant border glyph leaked at ({x},{y}): {symbol:?}\n{text}"
                );
            }
        }
    }

    #[test]
    fn render_session_detail_leaves_gap_row_between_consecutive_tool_result_cards() {
        // Regression for the "messages blend into one another" report: two
        // adjacent bordered cards sharing the same bubble_bg (both ToolResult
        // -> tool_bubble_bg) must have a visible pane-bg row between them,
        // not just rely on border glyphs to separate same-colored fills.
        use crate::app::app_test_helpers;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (mut app, session_id) = app_test_helpers::with_session_detail();
        let area = Rect::new(0, 0, 80, 30);

        {
            let state = app
                .sessions
                .get_mut(&session_id)
                .expect("fixture session state");
            state.events = vec![
                ConversationEvent {
                    id: 1,
                    session_id,
                    sequence: 1,
                    event_type: EventType::ToolResult,
                    role: None,
                    content: "ok".to_string(),
                    tool_name: Some("Read".to_string()),
                    tool_input: None,
                    created_at: chrono::Utc::now(),
                    offload_id: None,
                    tool_use_id: None,
                    metadata: None,
                },
                ConversationEvent {
                    id: 2,
                    session_id,
                    sequence: 2,
                    event_type: EventType::ToolResult,
                    role: None,
                    content: "ok too".to_string(),
                    tool_name: Some("Read".to_string()),
                    tool_input: None,
                    created_at: chrono::Utc::now(),
                    offload_id: None,
                    tool_use_id: None,
                    metadata: None,
                },
            ];
            state.expanded_events.insert(1);
            state.expanded_events.insert(2);
            state.events_generation += 1;
            state.current_event_index = None;
            state.follow_tail = false;
            state.scroll_offset = 0;
            crate::ui::height::update_event_heights(
                state,
                area.width.saturating_sub(theme::SESSION_DETAIL_HORIZ_INSET),
            );
        }

        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_session_detail(frame, area, session_id, true, &app);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let text = buffer_text(buffer);
        let state = app.sessions.get(&session_id).expect("state");
        let header_h = area.height.min(SESSION_DETAIL_HEADER_HEIGHT);
        let gap_row_content = state.event_heights[0] - 1;
        let gap_y = area.y + header_h + gap_row_content as u16;
        let detail_bg = detail_surface_bg(&app.settings);

        for x in (area.x + 2)..(area.x + area.width - 2) {
            let cell = &buffer[(x, gap_y)];
            assert_eq!(
                cell.symbol(),
                " ",
                "expected blank separator row at ({x},{gap_y}), got {:?}\n{text}",
                cell.symbol()
            );
            assert_eq!(
                cell.bg, detail_bg,
                "expected separator row painted with pane bg (not a card's bubble bg) \
                 at ({x},{gap_y})\n{text}"
            );
        }
    }

    #[test]
    fn render_session_detail_keeps_header_focused_on_session_state() {
        use crate::app::app_test_helpers;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (mut app, session_id) = app_test_helpers::with_session_detail();
        let project_id = uuid::Uuid::new_v4();
        let now = chrono::Utc::now();
        app.projects.push(rsi_common::types::Project {
            id: project_id,
            name: "rsi".to_string(),
            path: None,
            description: None,
            color: rsi_common::types::Project::DEFAULT_COLOR.to_string(),
            context_files: None,
            created_at: now,
            updated_at: now,
        });
        if let Some(state) = app.sessions.get_mut(&session_id) {
            state.session.project_id = Some(project_id);
        }

        let area = Rect::new(0, 0, 100, 20);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_session_detail(frame, area, session_id, true, &app);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let text = buffer_text(buffer);
        assert!(text.contains("Fixture session"), "missing title:\n{text}");
        assert!(
            text.contains("ID  ✓  Claude"),
            "ID control must precede status icon and provider metadata:\n{text}"
        );
        assert!(
            !text.contains("project: rsi"),
            "secondary project metadata should stay in the info panel:\n{text}"
        );
    }

    #[test]
    fn render_session_detail_shows_cached_recursive_dag_context() {
        use crate::app::app_test_helpers;
        use crate::types::{OverlayState, RecursiveDagBrowserState};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use rsi_common::{
            RecursiveExecutionMode, RecursiveGraphStatus, RecursiveTaskGraphId,
            RecursiveTaskGraphSummary, RecursiveTaskId,
        };

        let (mut app, session_id) = app_test_helpers::with_session_detail();
        app.set_project_filter(None);
        let now = chrono::Utc::now();
        let graph = RecursiveTaskGraphSummary {
            id: RecursiveTaskGraphId::new(),
            root_task_id: RecursiveTaskId::new(),
            title: "cached recursive graph".to_string(),
            objective: "objective".to_string(),
            status: RecursiveGraphStatus::Active,
            project_id: None,
            workflow_id: None,
            topology_id: None,
            parent_session_id: Some(session_id),
            source_execution_id: None,
            source_eval_id: None,
            execution_mode: RecursiveExecutionMode::Fake,
            max_depth: 4,
            max_fanout: 4,
            max_descendants: 16,
            step_limit: 32,
            last_stop_reason: None,
            malformed_reason: None,
            created_at: now,
            updated_at: now,
            recovered_at: None,
            quarantined_at: None,
            quarantine_reason: None,
            recovery_checked_at: None,
        };
        app.recursive_dag_cache = Some(RecursiveDagBrowserState::ready(
            None,
            rsi_common::rpc::DaemonCapabilities::default(),
            vec![graph],
            None,
            None,
        ));
        app.overlay = OverlayState::None;
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                render_session_detail(frame, Rect::new(0, 0, 120, 32), session_id, true, &app);
            })
            .unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("[dag]") && text.contains("cached recursive graph"),
            "session detail should surface cached recursive DAG context:\n{text}"
        );
    }

    #[test]
    fn render_session_detail_truncates_long_recursive_dag_labels() {
        use crate::app::app_test_helpers;
        use crate::types::{OverlayState, RecursiveDagBrowserState};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use rsi_common::{
            RecursiveExecutionMode, RecursiveGraphStatus, RecursiveTaskGraphId,
            RecursiveTaskGraphSummary, RecursiveTaskId,
        };

        let (mut app, session_id) = app_test_helpers::with_session_detail();
        app.set_project_filter(None);
        let now = chrono::Utc::now();
        let long_title = format!("recursive-dag-{}", "verylonglabel".repeat(12));
        let graph = RecursiveTaskGraphSummary {
            id: RecursiveTaskGraphId::new(),
            root_task_id: RecursiveTaskId::new(),
            title: long_title.clone(),
            objective: "objective".to_string(),
            status: RecursiveGraphStatus::Active,
            project_id: None,
            workflow_id: None,
            topology_id: None,
            parent_session_id: Some(session_id),
            source_execution_id: None,
            source_eval_id: None,
            execution_mode: RecursiveExecutionMode::Fake,
            max_depth: 4,
            max_fanout: 4,
            max_descendants: 16,
            step_limit: 32,
            last_stop_reason: None,
            malformed_reason: None,
            created_at: now,
            updated_at: now,
            recovered_at: None,
            quarantined_at: None,
            quarantine_reason: None,
            recovery_checked_at: None,
        };
        app.recursive_dag_cache = Some(RecursiveDagBrowserState::ready(
            None,
            rsi_common::rpc::DaemonCapabilities::default(),
            vec![graph],
            None,
            None,
        ));
        app.overlay = OverlayState::None;
        let backend = TestBackend::new(64, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                render_session_detail(frame, Rect::new(0, 0, 64, 24), session_id, true, &app);
            })
            .unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("[dag]"),
            "DAG detail line should render:\n{text}"
        );
        assert!(
            text.contains("..."),
            "long DAG labels should be truncated with an ellipsis:\n{text}"
        );
        assert!(
            !text.contains(&long_title),
            "full long DAG label should not spill into the rendered detail:\n{text}"
        );
    }

    // --- T5 (DETAIL region): additive tests only, appended after every
    // existing test in this shared LIST+DETAIL `mod tests` block per the
    // region guard's "interleaved, not partitioned by line range" note. ---

    #[test]
    fn render_session_detail_header_has_no_persistent_key_hints() {
        use crate::app::app_test_helpers;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (app, session_id) = app_test_helpers::with_session_detail();
        let backend = TestBackend::new(120, 32);
        let mut terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => panic!("test terminal should initialize: {error}"),
        };

        if let Err(error) = terminal.draw(|frame| {
            render_session_detail(frame, Rect::new(0, 0, 120, 32), session_id, true, &app);
        }) {
            panic!("session detail render should succeed: {error}");
        }

        let text = buffer_text(terminal.backend().buffer());
        assert!(
            !text.contains("chat v"),
            "detail header must not advertise the dead views stub:\n{text}"
        );
        assert!(!text.contains("info:"), "stale info label leaked:\n{text}");
        assert!(!text.contains("F3"), "key hint leaked into header:\n{text}");
        assert!(
            text.contains("Claude"),
            "provider metadata missing:\n{text}"
        );
    }

    #[test]
    fn render_input_bar_border_color_follows_active_border_policy_and_focus() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use crate::app::app_test_helpers;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (app, session_id) = app_test_helpers::with_session_detail();

        // Read the CURRENT ambient theme's policy rather than mutating global
        // theme state: theme.rs's own tests mutate the same process-wide
        // `ACTIVE_THEME_INDEX` under a THEME_TEST_GUARD private to that
        // module, which this test cannot share (and per this slice's
        // constraints must not modify theme.rs to expose). Deriving
        // "expected" from the same live global state `render_input_bar`
        // reads keeps this test theme-agnostic, deterministic, and race-free
        // regardless of test execution order/interleaving — the same reason
        // `theme_opaque_bg.rs`'s theme-mutating probes live in their own
        // separate integration-test binary rather than this lib's test set.
        let policy = theme::active_border_policy();

        let render_top_left_fg = |focused: bool| -> Option<Color> {
            let backend = TestBackend::new(40, 5);
            let mut terminal = Terminal::new(backend).expect("test terminal should initialize");
            terminal
                .draw(|frame| {
                    render_input_bar(frame, Rect::new(0, 0, 40, 5), session_id, focused, &app);
                })
                .expect("input bar render should succeed");
            terminal.backend().buffer()[(0, 0)].style().fg
        };

        let focused_color = render_top_left_fg(true);
        let unfocused_color = render_top_left_fg(false);

        let expected_focused = Some(match policy {
            theme::BorderPolicy::FullBorders => theme::session_detail_border(),
            theme::BorderPolicy::AccentFocusOnly => theme::focused_border(),
        });
        let expected_unfocused = Some(match policy {
            theme::BorderPolicy::FullBorders => theme::session_detail_border(),
            theme::BorderPolicy::AccentFocusOnly => theme::tier_panel(),
        });

        assert_eq!(
            focused_color, expected_focused,
            "focused input-bar border must match active_border_policy()'s focused arm"
        );
        assert_eq!(
            unfocused_color, expected_unfocused,
            "unfocused input-bar border must match active_border_policy()'s unfocused arm"
        );

        if policy == theme::BorderPolicy::AccentFocusOnly {
            assert_ne!(
                focused_color, unfocused_color,
                "AccentFocusOnly must visually distinguish focused from unfocused"
            );
        }
    }

    #[test]
    fn render_input_bar_uses_detail_surface_background() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use crate::app::app_test_helpers;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (app, session_id) = app_test_helpers::with_session_detail();
        let backend = TestBackend::new(40, 5);
        let mut terminal = Terminal::new(backend).expect("test terminal should initialize");
        terminal
            .draw(|frame| {
                render_input_bar(frame, Rect::new(0, 0, 40, 5), session_id, true, &app);
            })
            .expect("input bar render should succeed");

        assert_eq!(
            terminal.backend().buffer()[(2, 2)].bg,
            detail_surface_bg(&app.settings),
            "composer background must match transcript detail surface"
        );
    }

    /// Feed one normal-mode key through the same `KeyManager` dispatch the
    /// event loop uses for the input bar after it passes a leader key through,
    /// then recompute `App.vim_machine_pending` exactly like the event loop
    /// (`actions.is_empty()`). Actions are read but not executed, matching the
    /// leader-sequence window where the queue is what drives the label.
    fn feed_leader_key(app: &mut App, key: crossterm::event::KeyCode) {
        use keybindings::BindingMachine;
        use modalkit::key::TerminalKey;

        let key_event = crossterm::event::KeyEvent::new(key, crossterm::event::KeyModifiers::NONE);
        app.key_manager.input_key(TerminalKey::from(key_event));

        let mut actions = Vec::new();
        while let Some((action, _ctx)) = app.key_manager.pop() {
            actions.push(action);
        }
        app.vim_machine_pending = actions.is_empty();
    }

    /// Render the focused input bar for `app` at the fixture geometry and
    /// return the full buffer text (same shape used by the neighboring
    /// `render_input_bar_*` tests).
    fn render_input_bar_text(app: &App, session_id: uuid::Uuid) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(40, 5);
        let mut terminal = Terminal::new(backend).expect("test terminal should initialize");
        terminal
            .draw(|frame| {
                render_input_bar(frame, Rect::new(0, 0, 40, 5), session_id, true, app);
            })
            .expect("input bar render should succeed");
        buffer_text(terminal.backend().buffer())
    }

    #[test]
    fn render_input_bar_shows_leader_label_while_leader_sequence_pending() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use crate::app::app_test_helpers;

        let (mut app, session_id) = app_test_helpers::with_session_detail();
        // Fixture input bar starts in normal mode, matching the focused
        // normal-input condition the leader indicator requires.
        assert_eq!(
            app.sessions[&session_id].input_bar.surface.mode,
            crate::types::PopupMode::Normal
        );

        let baseline = render_input_bar_text(&app, session_id);
        assert!(
            baseline.contains("NORMAL"),
            "before any leader key, the focused normal-mode input bar must show NORMAL:\n{baseline}"
        );

        // Real prefix: Space is the leader prefix; the vim machine consumes
        // it without producing an action, so the sequence is pending.
        feed_leader_key(&mut app, crossterm::event::KeyCode::Char(' '));
        assert!(
            app.vim_machine_pending,
            "Space alone must leave the vim machine pending a leader continuation"
        );

        let pending = render_input_bar_text(&app, session_id);
        assert!(
            pending.contains("LEADER"),
            "pending leader state must render a distinct positive mode label after the prefix:\n{pending}"
        );

        // Follow-up key completes the sequence into a real action, clearing
        // the pending state and restoring the ordinary NORMAL label.
        // A migrated launcher chord (<Space>n) also completes the leader and
        // clears the pending indicator.
        feed_leader_key(&mut app, crossterm::event::KeyCode::Char('n'));
        assert!(
            !app.vim_machine_pending,
            "completing a leader chord must clear the pending vim-machine state"
        );

        let completed = render_input_bar_text(&app, session_id);
        assert!(
            completed.contains("NORMAL"),
            "completing the leader sequence must restore the NORMAL mode label:\n{completed}"
        );
    }

    #[test]
    fn render_input_bar_esc_cancels_pending_leader_label() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use crate::app::app_test_helpers;

        let (mut app, session_id) = app_test_helpers::with_session_detail();

        feed_leader_key(&mut app, crossterm::event::KeyCode::Char(' '));
        assert!(app.vim_machine_pending, "Space prefix must be pending");
        let pending = render_input_bar_text(&app, session_id);
        assert!(
            pending.contains("LEADER"),
            "pending leader state must render before cancellation:\n{pending}"
        );

        // Esc cancels the pending leader continuation; the label clears with it.
        feed_leader_key(&mut app, crossterm::event::KeyCode::Esc);
        let cancelled = render_input_bar_text(&app, session_id);
        assert!(
            cancelled.contains("NORMAL"),
            "cancelling the leader sequence must restore the NORMAL mode label:\n{cancelled}"
        );
    }

    // --- T6 (DETAIL region, tool-call border sites): PI-10 — selected vs.
    // unselected tool-call bubble border colors differ at all 3 sites
    // (group-summary, main bubble, compact-tool-event); non-tool bubble
    // border color is unaffected (Decision D3, F-011/F-013/F-014). ---

    /// Render `render_session_detail` for `session_id` with the cursor at
    /// `cursor_idx` (or nowhere, if `None`) and return the top-left border
    /// cell's fg color of the first visible event. Geometry is fixed
    /// by the 80x20 render area: header consumes 3 rows, so `inner` starts
    /// at (2, 3), and the first visible event starts
    /// there (`event_y == 0` for the first visible event).
    fn render_first_event_border_fg(
        app: &mut App,
        session_id: uuid::Uuid,
        cursor_idx: Option<usize>,
        width: u16,
    ) -> Option<Color> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        if let Some(state) = app.sessions.get_mut(&session_id) {
            state.current_event_index = cursor_idx;
            crate::ui::height::update_event_heights(state, width.saturating_sub(4));
        }

        let backend = TestBackend::new(width, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal should initialize");
        terminal
            .draw(|frame| {
                render_session_detail(frame, Rect::new(0, 0, width, 20), session_id, true, app);
            })
            .expect("session detail render should succeed");
        terminal.backend().buffer()[(2, 3)].style().fg
    }

    #[test]
    fn group_summary_bubble_border_color_differs_selected_vs_unselected() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use crate::app::app_test_helpers;

        let build_two_tool_result_run = || {
            let (mut app, session_id) = app_test_helpers::with_session_detail();
            if let Some(state) = app.sessions.get_mut(&session_id) {
                state.events = (0..2)
                    .map(|i| ConversationEvent {
                        id: 0,
                        session_id,
                        sequence: 20 + i,
                        event_type: EventType::ToolResult,
                        role: Some(Role::Assistant),
                        content: "tool result content".to_string(),
                        tool_name: Some("Bash".to_string()),
                        tool_input: None,
                        created_at: chrono::Utc::now(),
                        offload_id: None,
                        tool_use_id: None,
                        metadata: None,
                    })
                    .collect();
                state.events_generation += 1;
                state.collapsed_events.clear();
                state.expanded_events.clear();
                state.expanded_tool_groups.clear();
            }
            (app, session_id)
        };

        let (mut selected_app, sid1) = build_two_tool_result_run();
        let selected_fg = render_first_event_border_fg(&mut selected_app, sid1, Some(0), 80);
        let (mut unselected_app, sid2) = build_two_tool_result_run();
        let unselected_fg = render_first_event_border_fg(&mut unselected_app, sid2, None, 80);

        assert_eq!(
            selected_fg,
            Some(theme::tool_call_selected_border()),
            "selected group-summary bubble border should use tool_call_selected_border()"
        );
        assert_eq!(
            unselected_fg,
            Some(theme::tool_call_border()),
            "unselected group-summary bubble border should use tool_call_border()"
        );
        assert_ne!(
            selected_fg, unselected_fg,
            "selected vs. unselected group-summary border colors must differ"
        );
    }

    #[test]
    fn main_bubble_tool_call_border_color_differs_selected_vs_unselected() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use crate::app::app_test_helpers;

        let build_expanded_tool_use = || {
            let (mut app, session_id) = app_test_helpers::with_session_detail();
            if let Some(state) = app.sessions.get_mut(&session_id) {
                state.events = vec![ConversationEvent {
                    id: 0,
                    session_id,
                    sequence: 30,
                    event_type: EventType::ToolUse,
                    role: Some(Role::Assistant),
                    content: "expanded tool call content".to_string(),
                    tool_name: Some("Read".to_string()),
                    tool_input: Some(Box::new(serde_json::json!({"path": "/tmp/x"}))),
                    created_at: chrono::Utc::now(),
                    offload_id: None,
                    tool_use_id: None,
                    metadata: None,
                }];
                state.events_generation += 1;
                state.collapsed_events.clear();
                state.expanded_events.clear();
                state.expanded_events.insert(30);
                state.expanded_tool_groups.clear();
            }
            (app, session_id)
        };

        let (mut selected_app, sid1) = build_expanded_tool_use();
        let selected_fg = render_first_event_border_fg(&mut selected_app, sid1, Some(0), 80);
        let (mut unselected_app, sid2) = build_expanded_tool_use();
        let unselected_fg = render_first_event_border_fg(&mut unselected_app, sid2, None, 80);

        assert_eq!(
            selected_fg,
            Some(theme::tool_call_selected_border()),
            "selected main tool-call bubble border should use tool_call_selected_border()"
        );
        assert_eq!(
            unselected_fg,
            Some(theme::tool_call_border()),
            "unselected main tool-call bubble border should use tool_call_border()"
        );
        assert_ne!(
            selected_fg, unselected_fg,
            "selected vs. unselected main tool-call bubble border colors must differ"
        );
    }

    #[test]
    fn compact_tool_event_border_color_differs_selected_vs_unselected() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use crate::app::app_test_helpers;

        let build_compact_tool_use = || {
            let (mut app, session_id) = app_test_helpers::with_session_detail();
            if let Some(state) = app.sessions.get_mut(&session_id) {
                state.events = vec![ConversationEvent {
                    id: 0,
                    session_id,
                    sequence: 40,
                    event_type: EventType::ToolUse,
                    role: Some(Role::Assistant),
                    content: "echo hi".to_string(),
                    tool_name: Some("Bash".to_string()),
                    tool_input: None, // input-less -> compact-row treatment
                    created_at: chrono::Utc::now(),
                    offload_id: None,
                    tool_use_id: None,
                    metadata: None,
                }];
                state.events_generation += 1;
                state.collapsed_events.clear();
                state.expanded_events.clear();
                state.expanded_tool_groups.clear();
            }
            (app, session_id)
        };

        let (mut selected_app, sid1) = build_compact_tool_use();
        let selected_fg = render_first_event_border_fg(&mut selected_app, sid1, Some(0), 80);
        let (mut unselected_app, sid2) = build_compact_tool_use();
        let unselected_fg = render_first_event_border_fg(&mut unselected_app, sid2, None, 80);

        assert_eq!(
            selected_fg,
            Some(theme::tool_call_selected_border()),
            "selected compact tool-call row border should use tool_call_selected_border()"
        );
        assert_eq!(
            unselected_fg,
            Some(theme::tool_call_border()),
            "unselected compact tool-call row border should use tool_call_border()"
        );
        assert_ne!(
            selected_fg, unselected_fg,
            "selected vs. unselected compact tool-call border colors must differ"
        );
    }

    #[test]
    fn message_role_rail_emphasizes_selection_without_a_bubble() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        use crate::app::app_test_helpers;

        // Conversation turns use role-colored rails; the selected turn adopts
        // the focused accent without bringing back a full message box.
        let build_user_message = || {
            let (mut app, session_id) = app_test_helpers::with_session_detail();
            if let Some(state) = app.sessions.get_mut(&session_id) {
                state.events = vec![ConversationEvent {
                    id: 0,
                    session_id,
                    sequence: 50,
                    event_type: EventType::Message,
                    role: Some(Role::User),
                    content: "a plain user message".to_string(),
                    tool_name: None,
                    tool_input: None,
                    created_at: chrono::Utc::now(),
                    offload_id: None,
                    tool_use_id: None,
                    metadata: None,
                }];
                state.events_generation += 1;
            }
            (app, session_id)
        };

        let (mut selected_app, sid1) = build_user_message();
        let selected_fg = render_first_event_border_fg(&mut selected_app, sid1, Some(0), 80);
        let (mut unselected_app, sid2) = build_user_message();
        let unselected_fg = render_first_event_border_fg(&mut unselected_app, sid2, None, 80);

        assert_eq!(
            selected_fg,
            Some(theme::focused_border()),
            "selected message rail should use the focused accent"
        );
        assert_eq!(
            unselected_fg,
            Some(theme::user_role()),
            "unselected message rail should retain its role color"
        );
        assert_ne!(
            selected_fg, unselected_fg,
            "selection should be visible on a borderless message rail"
        );
    }
}

/// Render a model switch divider line.
/// Format: "─── Sonnet 5 → Opus 4.6 ───"
fn render_model_switch_divider(old_model: &str, new_model: &str, width: u16) -> Line<'static> {
    let old_abbrev = abbreviate_model_name(old_model);
    let new_abbrev = abbreviate_model_name(new_model);
    let label = format!(" {} → {} ", old_abbrev, new_abbrev);
    let label_len = label.len();

    // Fill remaining width with ─ on both sides
    let total_width = width as usize;
    let dash_space = total_width.saturating_sub(label_len);
    let left_dashes = dash_space / 2;
    let right_dashes = dash_space.saturating_sub(left_dashes);

    let dim_style = Style::default()
        .fg(theme::surface2())
        .add_modifier(Modifier::DIM);

    Line::from(vec![
        Span::styled("─".repeat(left_dashes), dim_style),
        Span::styled(label, dim_style),
        Span::styled("─".repeat(right_dashes), dim_style),
    ])
}

/// Abbreviate model names for compact display.
/// "claude-sonnet-5" → "Sonnet 5"
/// "claude-opus-4-6" → "Opus 4.6 (1M)"
/// "claude-opus-4-6-200k" → "Opus 4.6 (200K)"
/// "claude-opus-4-7" → "Opus 4.7 (1M)"
/// "claude-opus-4-7-200k" → "Opus 4.7 (200K)"
/// Works for any version 4.6–5.9 (and beyond) for Opus, Sonnet, and Haiku.
pub(crate) fn abbreviate_model_name(model_id: &str) -> String {
    rsi_common::model_utils::abbreviate_model(model_id)
}

/// Render the file viewer/editor in the session detail area.
/// Takes the full session_area (before content/input-bar split) so the viewer
/// occupies the same total space as the session detail container.
pub fn render_file_viewer(
    frame: &mut Frame,
    area: Rect,
    session_id: uuid::Uuid,
    focused: bool,
    app: &mut App,
) {
    let Some(state) = app.sessions.get_mut(&session_id) else {
        return;
    };
    let Some(viewer) = state.file_viewer.as_mut() else {
        return;
    };

    // Build title: file name + dirty indicator + mode
    let file_name = viewer
        .file_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "untitled".to_string());
    let dirty_marker = if viewer.dirty { " [+]" } else { "" };
    let preview_marker = if viewer.markdown_preview && viewer.is_markdown() {
        " [PREVIEW]"
    } else {
        ""
    };
    let mode_str = match viewer.surface.mode {
        crate::types::PopupMode::Insert => " -- INSERT --",
        crate::types::PopupMode::Normal => "",
    };
    let title = format!(" {file_name}{dirty_marker}{preview_marker}{mode_str} ");

    // Build bottom title: full file path
    let bottom_title = format!(" {} ", viewer.file_path.display());

    let border_color = if focused {
        if viewer.dirty {
            theme::yellow() // Yellow for dirty files
        } else {
            theme::session_detail_border()
        }
    } else {
        theme::session_detail_border()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(detail_surface_bg(&app.settings)))
        .title(title)
        .title_bottom(Line::from(bottom_title))
        .padding(ratatui::widgets::Padding::horizontal(1));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Disable tui_textarea's built-in cursor rendering — we handle it ourselves
    viewer
        .surface
        .textarea
        .set_cursor_line_style(Style::default());
    viewer.surface.textarea.set_cursor_style(Style::default());

    // Custom renderer: line numbers + tree-sitter highlighting + block cursor
    super::file_renderer::render_file_viewer_content(frame, inner, viewer, focused);
}
