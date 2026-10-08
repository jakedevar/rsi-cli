//! Settings workspace rendering.
//!
//! The Settings pane reads like a game settings menu:
//!
//! - a title bar with the breadcrumb (`≡ Settings › ◐ Appearance › Theme &
//!   Colors`) and the `/` search state;
//! - a category rail with one row per [`SettingsGroup`] (`j` / `k` while the
//!   rail is focused), each with its own glyph and accent color;
//! - the selected category's settings list with one tab per
//!   [`SettingsSection`] (`Tab` / `Shift-Tab` or `]` / `[`). Values render as
//!   widgets — switches, `◂ choice ▸` selectors with position pips, color
//!   swatches, pickers and buttons — joined to their labels by dot leaders;
//! - an info card that explains the selected setting: what it does, what
//!   Enter does, when a change applies and where it is stored;
//! - a footer of key prompts for the focused panel and the selected row.
//!
//! The rail and info-card widths are operator-resizable (`<` / `>`, `=`
//! resets), persist in `UserSettings`, and are clamped to the pane here.
//! Category, section and row order still come from the settings registry.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use rsi_common::daemon_config_catalog::ApplyClass;
use rsi_common::provider_credentials::{CredentialState, ProviderCredentialSlot};
use rsi_common::types::SessionProvider;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::App;
use crate::model_control_budgets::{budget_row_label_value, budget_rows};
use crate::model_control_stats::{
    StatsGroup, StatsRow, StatsRowAction, StatsTone, stats_rows, status_label,
};
use crate::settings::{
    ActivityIndicatorStyle, DaemonFeatureEntry, DaemonFeatureValue, DetailColumnAlignment,
    FORMULATION_ANIM_DURATIONS, SystemPromptPreset, UserSettings,
};
use crate::settings_keys::{
    DAEMON_FEATURE_SECTIONS, daemon_feature_rows_for_section, item_count,
    search_match_count_in_section, search_row_matches,
};
use crate::settings_registry::{
    SETTINGS, SettingApply, SettingOwner, SettingSpec, SettingsGroup, SettingsSection,
};
use crate::types::{NavigatorPreset, SettingsFocus, SettingsRenderedLayout};
use crate::ui::{glyphs, theme};

/// Narrowest category rail the resize keys allow.
pub(crate) const RAIL_MIN_WIDTH: u16 = 18;
const RAIL_MAX_WIDTH: u16 = 44;
/// Narrowest beside-the-list info card the resize keys allow.
pub(crate) const INFO_MIN_WIDTH: u16 = 30;
const INFO_MAX_WIDTH: u16 = 90;
/// Cells one `<` / `>` press moves a panel divider.
pub(crate) const RESIZE_STEP: u16 = 2;
/// The settings list never shrinks below this while panels are resized.
const LIST_MIN_WIDTH: u16 = 44;
/// Below this pane width one panel is shown at a time, following focus.
const SINGLE_PANEL_BREAKPOINT: u16 = 72;
/// From this pane width the rail gets its roomy default width.
const WIDE_RAIL_BREAKPOINT: u16 = 110;
/// From this pane width the info card sits beside the list.
const SIDE_INFO_BREAKPOINT: u16 = 150;
/// A list column at least this tall stacks the info card beneath it.
const STACKED_INFO_MIN_HEIGHT: u16 = 22;
const STACKED_INFO_HEIGHT: u16 = 9;
const LABEL_MIN_WIDTH: u16 = 12;
/// Icon-only rail shown beside the list on narrow panes while editing.
const COMPACT_RAIL_WIDTH: u16 = 5;
/// Widest centered value a `◂ value ▸` selector pads to.
const CHOICE_MAX_WIDTH: usize = 24;
/// Longest option set the info card lists.
const OPTIONS_LIST_MAX: usize = 16;
/// The key that activates the selected row (Enter).
const ENTER_KEY: &str = "↵";

/// Activity indicator styles in `ActivityIndicatorStyle::next` order.
const ACTIVITY_STYLES: [ActivityIndicatorStyle; 6] = [
    ActivityIndicatorStyle::Semantic,
    ActivityIndicatorStyle::RainbowClassic,
    ActivityIndicatorStyle::RainbowCompact,
    ActivityIndicatorStyle::RainbowClassicCompact,
    ActivityIndicatorStyle::SonicSpeedUp,
    ActivityIndicatorStyle::RainbowStarlight,
];

/// Detail column alignments in `DetailColumnAlignment::next` order.
const DETAIL_ALIGNMENTS: [DetailColumnAlignment; 3] = [
    DetailColumnAlignment::Dynamic,
    DetailColumnAlignment::Left,
    DetailColumnAlignment::Center,
];

// ---------------------------------------------------------------------------
// Category identity
// ---------------------------------------------------------------------------

/// Rail glyph of a category: single-cell, text-presentation codepoints (see
/// `ui::glyphs`), each a distinct shape so the rail reads without color.
#[must_use]
pub(crate) const fn group_glyph(group: SettingsGroup) -> &'static str {
    match group {
        SettingsGroup::Appearance => "◐",
        SettingsGroup::Workspace => "▤",
        SettingsGroup::Models => "✦",
        SettingsGroup::SafetyAndSpend => "$",
        SettingsGroup::AgentAutomation => "↺",
        SettingsGroup::ProvidersAndSandboxes => "⊡",
        SettingsGroup::Integrations => "⇄",
    }
}

/// Accent color of a category: its glyph, tabs, selection bar and value
/// widgets use it, so each category keeps one recognizable color.
#[must_use]
pub(crate) fn group_color(group: SettingsGroup) -> Color {
    match group {
        SettingsGroup::Appearance => theme::mauve(),
        SettingsGroup::Workspace => theme::blue(),
        SettingsGroup::Models => theme::peach(),
        SettingsGroup::SafetyAndSpend => theme::yellow(),
        SettingsGroup::AgentAutomation => theme::green(),
        SettingsGroup::ProvidersAndSandboxes => theme::teal(),
        SettingsGroup::Integrations => theme::sky(),
    }
}

/// Title-case category name for the rail and breadcrumb.
#[must_use]
pub(crate) const fn group_title(group: SettingsGroup) -> &'static str {
    match group {
        SettingsGroup::Appearance => "Appearance",
        SettingsGroup::Workspace => "Workspace",
        SettingsGroup::Models => "Models",
        SettingsGroup::SafetyAndSpend => "Safety & Spend",
        SettingsGroup::AgentAutomation => "Agent Automation",
        SettingsGroup::ProvidersAndSandboxes => "Providers & Sandboxes",
        SettingsGroup::Integrations => "Integrations",
    }
}

// ---------------------------------------------------------------------------
// Row model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueTone {
    Neutral,
    Positive,
    Warning,
    Destructive,
    Muted,
}

/// How a row's value is drawn.
#[derive(Debug, Clone, PartialEq)]
enum ValueWidget {
    /// An on/off switch: `━━● ON` / `○── OFF`.
    Toggle(bool),
    /// One value of a fixed set, `◂ value ▸`, with position pips (or a
    /// slider for long sets) when the position is known.
    Choice {
        value: String,
        position: Option<(usize, usize)>,
    },
    /// Opens a picker (`▾`); `glyph` marks the provider or kind.
    Picker {
        value: String,
        glyph: Option<(&'static str, Color)>,
    },
    /// A color value with its live swatch (`None`: no color of its own).
    Swatch { color: Option<Color>, text: String },
    /// An explicit operator action as a button, with its latest result.
    Action { button: String, detail: String },
    /// A state dot with its text.
    Badge {
        glyph: &'static str,
        color: Color,
        text: String,
    },
    /// One model call: status, optional stop button, then model and purpose.
    Activity {
        status: String,
        color: Color,
        detail: String,
        stop: bool,
    },
    /// Plain text: an editable string or read-only telemetry.
    Text(String),
    /// Empty-state rows show their first key prompt instead of a value.
    Empty,
}

#[cfg(test)]
impl ValueWidget {
    /// The value without widget chrome.
    fn plain(&self) -> String {
        match self {
            Self::Toggle(true) => "ON".to_string(),
            Self::Toggle(false) => "OFF".to_string(),
            Self::Choice { value, .. } | Self::Picker { value, .. } => value.clone(),
            Self::Swatch { text, .. } | Self::Badge { text, .. } | Self::Text(text) => text.clone(),
            Self::Action { button, detail } if detail.is_empty() => button.clone(),
            Self::Action { button, detail } => format!("{button} {detail}"),
            Self::Activity { status, detail, .. } => format!("{status} {detail}"),
            Self::Empty => String::new(),
        }
    }
}

/// A key the selected row accepts besides Enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyHint {
    key: &'static str,
    label: &'static str,
}

const fn key_hint(key: &'static str, label: &'static str) -> KeyHint {
    KeyHint { key, label }
}

const REFRESH_KEY: KeyHint = key_hint("R", "refresh");

#[derive(Debug, Clone)]
struct SettingsRow {
    group: Option<StatsGroup>,
    label: String,
    widget: ValueWidget,
    /// What Enter (and Space) does; empty for read-only rows.
    action: String,
    /// Further keys the row accepts, in prompt order.
    keys: Vec<KeyHint>,
    description: String,
    /// A caution shown in the info card.
    note: Option<&'static str>,
    tone: ValueTone,
    destructive: bool,
    /// The change applies only after a daemon restart (`↻` badge).
    restart: bool,
    empty: bool,
    /// Every value a choice row cycles through, with the current one, for
    /// the info card's option list.
    options: Vec<String>,
    option_index: Option<usize>,
}

#[derive(Debug, Clone)]
struct SettingsContract {
    scope: String,
    owner: String,
    persistence: String,
    apply: String,
    restart: String,
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SettingsWorkspaceLayout {
    rail: Option<Rect>,
    list: Option<Rect>,
    info: Option<Rect>,
    /// The info card sits beside the list rather than below it.
    info_beside: bool,
    /// Widest rail this pane allows (0 when one panel shows at a time).
    rail_max: u16,
    /// Widest beside-the-list info card this pane allows.
    info_max: u16,
}

/// Body and footer rows of the Settings pane; the title bar is row 0.
fn pane_regions(area: Rect) -> Option<(Rect, Option<Rect>)> {
    if area.width == 0 || area.height < 3 {
        return None;
    }
    let has_footer = area.height >= 6;
    let body_height = area.height - 1 - u16::from(has_footer);
    let body = Rect::new(area.x, area.y + 1, area.width, body_height);
    let footer = has_footer.then(|| Rect::new(area.x, area.y + area.height - 1, area.width, 1));
    Some((body, footer))
}

/// Panel geometry for the pane body.
///
/// Narrow panes show one panel at a time, following keyboard focus. Wider
/// panes show the rail beside the list; the info card stacks beneath the
/// list, or sits beside it from `SIDE_INFO_BREAKPOINT`. Operator widths from
/// `UserSettings` are clamped so the list keeps `LIST_MIN_WIDTH`.
fn settings_workspace_layout(
    body: Rect,
    focus: SettingsFocus,
    settings: &UserSettings,
) -> SettingsWorkspaceLayout {
    let inner = Rect::new(
        body.x + 1.min(body.width),
        body.y,
        body.width.saturating_sub(2),
        body.height,
    );
    if body.width < SINGLE_PANEL_BREAKPOINT {
        let (rail, list, info) = match focus {
            SettingsFocus::Categories => (Some(inner), None, None),
            SettingsFocus::Items => {
                let (list, info) = stack_info(inner);
                (None, Some(list), info)
            }
        };
        return SettingsWorkspaceLayout {
            rail,
            list,
            info,
            info_beside: false,
            rail_max: 0,
            info_max: 0,
        };
    }

    // Narrow panes collapse the rail to its icons while the list is focused,
    // so long labels and values keep their room; `h` brings it back.
    let compact_rail = focus == SettingsFocus::Items && body.width < WIDE_RAIL_BREAKPOINT;
    let rail_auto = if body.width >= WIDE_RAIL_BREAKPOINT {
        30
    } else {
        26
    };
    let rail_max = if compact_rail {
        0
    } else {
        RAIL_MAX_WIDTH
            .min(inner.width.saturating_sub(LIST_MIN_WIDTH + 1))
            .max(RAIL_MIN_WIDTH)
    };
    let rail_width = if compact_rail {
        COMPACT_RAIL_WIDTH
    } else {
        settings
            .settings_rail_width
            .unwrap_or(rail_auto)
            .clamp(RAIL_MIN_WIDTH, rail_max)
    };
    let rail = Rect::new(inner.x, inner.y, rail_width, inner.height);
    let right_x = rail.x + rail.width + 1;
    let right_width = (inner.x + inner.width).saturating_sub(right_x);

    if body.width >= SIDE_INFO_BREAKPOINT {
        let info_max = INFO_MAX_WIDTH
            .min(right_width.saturating_sub(LIST_MIN_WIDTH + 1))
            .max(INFO_MIN_WIDTH);
        let info_auto = u16::try_from(u32::from(right_width) * 38 / 100)
            .unwrap_or(u16::MAX)
            .clamp(40, 72);
        let info_width = settings
            .settings_info_width
            .unwrap_or(info_auto)
            .clamp(INFO_MIN_WIDTH, info_max);
        let list = Rect::new(
            right_x,
            inner.y,
            right_width.saturating_sub(info_width + 1),
            inner.height,
        );
        let info = Rect::new(list.x + list.width + 1, inner.y, info_width, inner.height);
        return SettingsWorkspaceLayout {
            rail: Some(rail),
            list: Some(list),
            info: Some(info),
            info_beside: true,
            rail_max,
            info_max,
        };
    }

    let (list, info) = stack_info(Rect::new(right_x, inner.y, right_width, inner.height));
    SettingsWorkspaceLayout {
        rail: Some(rail),
        list: Some(list),
        info,
        info_beside: false,
        rail_max,
        info_max: 0,
    }
}

/// Split a list column into the list and, when tall enough, an info card
/// beneath it.
fn stack_info(column: Rect) -> (Rect, Option<Rect>) {
    if column.height < STACKED_INFO_MIN_HEIGHT {
        return (column, None);
    }
    let list_height = column.height - STACKED_INFO_HEIGHT;
    (
        Rect::new(column.x, column.y, column.width, list_height),
        Some(Rect::new(
            column.x,
            column.y + list_height,
            column.width,
            STACKED_INFO_HEIGHT,
        )),
    )
}

/// Record the geometry the resize keys step from (`SettingsRenderedLayout`).
pub fn record_rendered_layout(app: &mut App, area: Rect) {
    crate::settings_keys::clamp_settings_selection(app);
    let rendered = pane_regions(area)
        .map(|(body, _)| {
            let layout = settings_workspace_layout(body, app.settings_state.focus, &app.settings);
            SettingsRenderedLayout {
                rail_width: if layout.rail_max == 0 {
                    0
                } else {
                    layout.rail.map_or(0, |rail| rail.width)
                },
                rail_max: layout.rail_max,
                info_width: layout
                    .info
                    .filter(|_| layout.info_beside)
                    .map(|info| info.width),
                info_max: layout.info_max,
            }
        })
        .unwrap_or_default();
    app.settings_state.rendered_layout = rendered;
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the Settings workspace.
pub fn render_settings(app: &App, frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::root_bg())),
        area,
    );
    render_title_bar(frame, Rect::new(area.x, area.y, area.width, 1), app);

    let Some((body, footer)) = pane_regions(area) else {
        return;
    };
    let layout = settings_workspace_layout(body, app.settings_state.focus, &app.settings);
    let rows = section_rows(app);
    let selected = app
        .settings_state
        .selected_index
        .min(rows.len().saturating_sub(1));
    let fallback = unknown_row();
    let selected_row = rows.get(selected).unwrap_or(&fallback);

    if let Some(rail) = layout.rail {
        render_rail(frame, rail, app);
    }
    let dropdown_anchor = layout.list.and_then(|list| {
        render_list_panel(frame, list, app, &rows, selected, layout.info.is_none())
    });
    if let Some(info) = layout.info {
        render_info_panel(frame, info, app, selected_row, selected);
    }
    if let Some(footer) = footer {
        render_footer(frame, footer, app, selected_row);
    }

    // Keep the model-picker interaction contract: one physical row per
    // Model Roles item, a 50-cell popup, and an anchor immediately below the
    // active row's value. Rendering last preserves painter-order overlap.
    if app.settings_state.model_dropdown.open
        && app.settings_state.section == SettingsSection::ModelRoles
        && let Some(anchor) = dropdown_anchor
    {
        let item_idx = app.settings_state.active_dropdown_item.unwrap_or(selected);
        let dropdown = &app.settings_state.model_dropdown;
        let current_model: Option<&str> = match item_idx {
            // Browsing another provider previews it; only the default's own
            // provider and endpoint carry its check mark.
            0 => app.default_model_in(dropdown),
            1 => Some(app.settings.title_model_local.as_str()),
            2 => Some(app.settings.prompt_processor.model.as_str()),
            3 => Some(app.settings.memory_model_fallback.as_str()),
            4 => Some(app.settings.dream_model.as_str()),
            5 => DaemonFeatureEntry::display_value(&app.daemon_features, "stall_classifier_model"),
            _ => None,
        };
        crate::ui::widget::model_dropdown::render_model_dropdown(
            frame,
            area,
            anchor,
            dropdown,
            current_model,
            app.is_provider_available(dropdown.provider),
        );
    }
}

fn key_style() -> Style {
    Style::default()
        .fg(theme::focused_border())
        .add_modifier(Modifier::BOLD)
}

fn key_cap(key: impl Into<String>) -> Span<'static> {
    Span::styled(key.into(), key_style())
}

fn dim_style() -> Style {
    Style::default().fg(theme::dim_metadata())
}

/// Search hits (query, match counts, matching rows) share one bright color.
fn search_hit_style() -> Style {
    Style::default()
        .fg(theme::yellow())
        .add_modifier(Modifier::BOLD)
}

fn render_title_bar(frame: &mut Frame, area: Rect, app: &App) {
    let section = app.settings_state.section;
    let group = section.group();
    let separator = dim_style();
    let left = Line::from(vec![
        Span::styled(" ≡ ", key_style()),
        Span::styled(
            "Settings",
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ›  ", separator),
        Span::styled(
            format!("{} {}", group_glyph(group), group_title(group)),
            Style::default()
                .fg(group_color(group))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ›  ", separator),
        Span::styled(section.label(), Style::default().fg(theme::text())),
    ]);
    let right = Line::from(search_spans(app));
    let right_width = u16::try_from(right.width())
        .unwrap_or(u16::MAX)
        .min(area.width);
    frame.render_widget(
        Paragraph::new(left),
        Rect::new(
            area.x,
            area.y,
            area.width.saturating_sub(right_width + 1),
            1,
        ),
    );
    frame.render_widget(
        Paragraph::new(right),
        Rect::new(area.x + area.width - right_width, area.y, right_width, 1),
    );
}

fn search_spans(app: &App) -> Vec<Span<'static>> {
    let state = &app.settings_state;
    if !state.query_active && state.query.is_empty() {
        return vec![key_cap("/"), Span::styled(" search  ", dim_style())];
    }
    let mut spans = if state.query_active && crate::field_edit::standard_frame() {
        // Standard editing draws the query's cursor and selection in place.
        let mut spans = vec![Span::styled("/", search_hit_style())];
        spans.extend(crate::field_edit::draw(&state.query, search_hit_style()));
        spans
    } else {
        vec![Span::styled(
            format!(
                "/{}{}",
                state.query,
                if state.query_active { "▏" } else { "" }
            ),
            search_hit_style(),
        )]
    };
    if !state.query.trim().is_empty() {
        let matches: usize = SettingsSection::ALL
            .iter()
            .map(|section| search_match_count_in_section(*section, &state.query))
            .sum();
        let (text, color) = match matches {
            0 => ("  no match".to_string(), theme::warning_status()),
            1 => ("  1 match".to_string(), theme::dim_metadata()),
            n => (format!("  {n} matches"), theme::dim_metadata()),
        };
        spans.push(Span::styled(text, Style::default().fg(color)));
    }
    spans.push(Span::raw("  "));
    spans
}

/// Draw a rounded panel with a left title (and an optional right title) on
/// its top border; returns the area inside the border.
fn render_panel(
    frame: &mut Frame,
    area: Rect,
    focused: bool,
    title: Line<'static>,
    right_title: Option<Line<'static>>,
) -> Rect {
    let border = if focused {
        theme::focused_border()
    } else {
        theme::browser_decorative_separator()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .style(Style::default().bg(theme::glass_panel_bg()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if area.width > 4 && area.height > 0 {
        let title_width = (area.width - 4).min(u16::try_from(title.width()).unwrap_or(u16::MAX));
        frame.render_widget(
            Paragraph::new(title),
            Rect::new(area.x + 2, area.y, title_width, 1),
        );
        if let Some(right) = right_title {
            let right_width = u16::try_from(right.width()).unwrap_or(u16::MAX);
            if title_width + right_width + 6 <= area.width {
                frame.render_widget(
                    Paragraph::new(right),
                    Rect::new(
                        area.x + area.width - 2 - right_width,
                        area.y,
                        right_width,
                        1,
                    ),
                );
            }
        }
    }
    inner
}

fn panel_title_style(focused: bool) -> Style {
    Style::default()
        .fg(if focused {
            theme::text()
        } else {
            theme::subtext0()
        })
        .add_modifier(Modifier::BOLD)
}

/// Settings rows of `group` matching the active `/` query.
fn group_match_count(group: SettingsGroup, query: &str) -> usize {
    if query.trim().is_empty() {
        return 0;
    }
    group
        .sections()
        .map(|section| search_match_count_in_section(section, query))
        .sum()
}

fn render_rail(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.settings_state.focus == SettingsFocus::Categories;
    let compact = area.width <= COMPACT_RAIL_WIDTH;
    let title = if compact {
        Line::default()
    } else {
        Line::from(Span::styled(" CATEGORIES ", panel_title_style(focused)))
    };
    let inner = render_panel(frame, area, focused, title, None);
    if inner.height == 0 || inner.width < 2 {
        return;
    }

    let active = app.settings_state.section.group();
    let groups = SettingsGroup::ALL;
    let bottom = inner.y + inner.height;
    // A roomy rail separates categories with blank lines and shows the
    // selected category's question beneath it.
    let roomy = usize::from(inner.height) >= groups.len() * 2 + 2;
    if roomy {
        let mut y = inner.y + 1;
        for group in groups {
            if y >= bottom {
                break;
            }
            let selected = *group == active;
            let row = Rect::new(inner.x, y, inner.width, 1);
            render_rail_row(frame, row, app, *group, selected, focused, compact);
            y += 1;
            if selected && !compact && y < bottom {
                let width = inner.width.saturating_sub(5);
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        truncate_to_width(group.summary(), usize::from(width)),
                        dim_style().add_modifier(Modifier::ITALIC),
                    )),
                    Rect::new(inner.x + 4, y, width, 1),
                );
                y += 1;
            }
            y += 1;
        }
        return;
    }
    let (start, end) = visible_window(groups.len(), active.position(), usize::from(inner.height));
    for (offset, group) in groups[start..end].iter().enumerate() {
        let row = Rect::new(inner.x, inner.y + offset as u16, inner.width, 1);
        render_rail_row(frame, row, app, *group, *group == active, focused, compact);
    }
}

fn render_rail_row(
    frame: &mut Frame,
    row: Rect,
    app: &App,
    group: SettingsGroup,
    selected: bool,
    focused: bool,
    compact: bool,
) {
    let accent = group_color(group);
    if selected {
        let bg = if focused {
            theme::selected_row_bg()
        } else {
            theme::viewed_session_row_bg()
        };
        frame.render_widget(Block::default().style(Style::default().bg(bg)), row);
        // The selection bar is a filled accent cell, not a glyph.
        frame.render_widget(
            Block::default().style(Style::default().bg(accent)),
            Rect::new(row.x, row.y, 1, 1),
        );
    }
    let glyph = Span::styled(
        group_glyph(group),
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    );
    if compact {
        frame.render_widget(Paragraph::new(Line::from(vec![Span::raw(" "), glyph])), row);
        return;
    }
    let matches = group_match_count(group, &app.settings_state.query);
    let badge = if matches > 0 {
        format!("•{matches} ")
    } else {
        String::new()
    };
    let badge_width = u16::try_from(UnicodeWidthStr::width(badge.as_str())).unwrap_or(0);
    let label_room = usize::from(row.width.saturating_sub(4 + badge_width));
    let label_style = if selected {
        Style::default()
            .fg(theme::text())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::subtext1())
    };
    let line = Line::from(vec![
        Span::raw("  "),
        glyph,
        Span::raw(" "),
        Span::styled(
            truncate_to_width(group_title(group), label_room),
            label_style,
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line),
        Rect::new(row.x, row.y, row.width.saturating_sub(badge_width), 1),
    );
    if badge_width > 0 && badge_width < row.width {
        frame.render_widget(
            Paragraph::new(Span::styled(badge, search_hit_style())),
            Rect::new(row.x + row.width - badge_width, row.y, badge_width, 1),
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowColumns {
    bar_x: u16,
    label_x: u16,
    label_width: u16,
    value_x: u16,
    value_width: u16,
}

/// Row columns inside the list panel. The label column fits the section's
/// longest label (at most 55% of the row) and every value starts in one
/// column, so values line up and never hide behind a fixed-width label.
fn row_columns(inner: Rect, rows: &[SettingsRow]) -> RowColumns {
    let bar_x = inner.x;
    let label_x = inner.x.saturating_add(2);
    let end = inner.x + inner.width.saturating_sub(1);
    let available = end.saturating_sub(label_x);
    let longest = rows
        .iter()
        .map(|row| UnicodeWidthStr::width(row.label.as_str()))
        .max()
        .unwrap_or(0);
    let longest = u16::try_from(longest).unwrap_or(u16::MAX);
    let cap = u16::try_from(u32::from(available) * 55 / 100).unwrap_or(available);
    let label_width = longest.max(LABEL_MIN_WIDTH.min(cap)).min(cap);
    let value_x = (label_x + label_width + 3).min(end);
    RowColumns {
        bar_x,
        label_x,
        label_width,
        value_x,
        value_width: end.saturating_sub(value_x),
    }
}

/// Headings are visual separators, not selectable rows. Actions keep their
/// original row index even while headings are inserted or pinned on scroll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsListLine {
    Heading(StatsGroup, usize),
    Row(usize),
}

fn settings_list_lines(rows: &[SettingsRow]) -> Vec<SettingsListLine> {
    let mut lines = Vec::new();
    let mut previous = None;
    for (idx, row) in rows.iter().enumerate() {
        if row.group != previous {
            if let Some(group) = row.group {
                let count = rows
                    .iter()
                    .filter(|row| {
                        row.group == Some(group)
                            && matches!(row.widget, ValueWidget::Activity { .. })
                    })
                    .count();
                lines.push(SettingsListLine::Heading(group, count));
            }
            previous = row.group;
        }
        lines.push(SettingsListLine::Row(idx));
    }
    lines
}

fn visible_settings_lines(
    rows: &[SettingsRow],
    selected: usize,
    capacity: usize,
) -> (Vec<SettingsListLine>, usize, usize) {
    let lines = settings_list_lines(rows);
    let position = lines
        .iter()
        .position(|line| *line == SettingsListLine::Row(selected))
        .unwrap_or(0);
    let (mut start, mut end) = visible_window(lines.len(), position, capacity);
    let mut visible = Vec::new();
    if start > 0 && capacity > 1 && matches!(lines.get(start), Some(SettingsListLine::Row(_))) {
        // Reserve a line for the current group's heading when scrolled inside it.
        let narrower = visible_window(lines.len(), position, capacity - 1);
        if let Some(SettingsListLine::Row(idx)) = lines.get(narrower.0)
            && let Some(group) = rows[*idx].group
        {
            start = narrower.0;
            end = narrower.1;
            let count = rows
                .iter()
                .filter(|row| {
                    row.group == Some(group) && matches!(row.widget, ValueWidget::Activity { .. })
                })
                .count();
            visible.push(SettingsListLine::Heading(group, count));
        }
    }
    visible.extend_from_slice(&lines[start..end]);
    (visible, start, lines.len())
}

fn render_stats_heading(frame: &mut Frame, area: Rect, group: StatsGroup, count: usize) {
    let color = stats_group_color(group);
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::card_bg())),
        area,
    );
    let count = match group {
        StatsGroup::Active | StatsGroup::Recent | StatsGroup::Denied => format!("  {count}"),
        _ => String::new(),
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            truncate_to_width(
                &format!(" {}{}", group.label(), count),
                usize::from(area.width),
            ),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )),
        area,
    );
}

/// Render the settings list panel and return the model-picker anchor when
/// the picker's row is visible.
fn render_list_panel(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    rows: &[SettingsRow],
    selected: usize,
    describe_selected: bool,
) -> Option<Rect> {
    let state = &app.settings_state;
    let section = state.section;
    let group = section.group();
    let accent = group_color(group);
    let focused = state.focus == SettingsFocus::Items;
    let highlighted = focused || state.model_dropdown.open;
    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            format!("{} {}", group_glyph(group), group.label()),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ]);
    let position = if state.model_dropdown.open {
        " PICKER ".to_string()
    } else {
        format!(" {}/{} ", selected + 1, rows.len().max(1))
    };
    let right = Line::from(Span::styled(position, panel_title_style(highlighted)));
    let inner = render_panel(frame, area, highlighted, title, Some(right));
    let content = inset_horiz(inner, 1);
    if content.height == 0 || content.width < 8 {
        return None;
    }

    render_tabs(frame, content, group, section, &state.query);
    let mut header: u16 = if content.height > 1 { 2 } else { 1 };
    if content.height > 3 {
        let summary = if describe_selected && focused {
            rows.get(selected)
                .map_or(section.summary(), |row| row.description.as_str())
        } else if section == SettingsSection::Usage {
            "Lifetime costs and live model work. Select a row for details; Enter activates its stop button."
        } else {
            section.summary()
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate_to_width(summary, usize::from(content.width)),
                dim_style().add_modifier(Modifier::ITALIC),
            )),
            Rect::new(content.x, content.y + 2, content.width, 1),
        );
        header = 3;
    }
    if content.height >= 12 {
        header = 4;
    }
    let items_y = content.y + header;
    let items_height = content.height.saturating_sub(header);
    if items_height == 0 {
        return None;
    }

    let (visible, start, total_lines) =
        visible_settings_lines(rows, selected, usize::from(items_height));
    let columns = row_columns(inner, rows);
    // Selectors share one width per tab so their arrows and pips line up.
    let choice_width = rows
        .iter()
        .filter_map(|row| match &row.widget {
            ValueWidget::Choice { value, .. } => Some(UnicodeWidthStr::width(value.as_str())),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        .min(CHOICE_MAX_WIDTH);
    for (offset, line) in visible.iter().enumerate() {
        let row_area = Rect::new(inner.x, items_y + offset as u16, inner.width, 1);
        match *line {
            SettingsListLine::Heading(group, count) => {
                render_stats_heading(frame, row_area, group, count)
            }
            SettingsListLine::Row(idx) => render_setting_row(
                frame,
                row_area,
                columns,
                &rows[idx],
                RowState {
                    selected: idx == selected,
                    focused,
                    matched: search_row_matches(section, idx, &state.query),
                },
                accent,
                choice_width,
            ),
        }
    }
    if total_lines > usize::from(items_height) && area.width > 0 {
        let border = if highlighted {
            theme::focused_border()
        } else {
            theme::browser_decorative_separator()
        };
        render_scrollbar(
            frame,
            Rect::new(area.x + area.width - 1, items_y, 1, items_height),
            start,
            total_lines,
            accent,
            border,
        );
    }

    let active_idx = state.active_dropdown_item.unwrap_or(selected);
    let offset = visible
        .iter()
        .position(|line| *line == SettingsListLine::Row(active_idx));
    if section == SettingsSection::ModelRoles && state.model_dropdown.open {
        offset.map(|offset| {
            Rect::new(
                columns.value_x,
                items_y + offset as u16,
                columns.value_width.max(1),
                1,
            )
        })
    } else {
        None
    }
}

/// The visible window of tabs: the active tab plus as many neighbours as
/// fit, leaving room for `‹` / `›` overflow markers.
fn tab_window(widths: &[u16], active: usize, available: u16) -> (usize, usize) {
    if widths.is_empty() {
        return (0, 0);
    }
    let active = active.min(widths.len() - 1);
    let fits = |start: usize, end: usize| {
        let tabs: u32 = widths[start..end]
            .iter()
            .map(|width| u32::from(*width))
            .sum::<u32>()
            + u32::try_from(end - start - 1).unwrap_or(u32::MAX);
        let markers = if start > 0 { 2 } else { 0 } + if end < widths.len() { 2 } else { 0 };
        tabs + markers <= u32::from(available)
    };
    let (mut start, mut end) = (active, active + 1);
    loop {
        let mut grew = false;
        if end < widths.len() && fits(start, end + 1) {
            end += 1;
            grew = true;
        }
        if start > 0 && fits(start - 1, end) {
            start -= 1;
            grew = true;
        }
        if !grew {
            return (start, end);
        }
    }
}

/// Section tabs of the selected category (row 0) and their underline (row 1).
fn render_tabs(
    frame: &mut Frame,
    content: Rect,
    group: SettingsGroup,
    active: SettingsSection,
    query: &str,
) {
    let accent = group_color(group);
    let tabs: Vec<SettingsSection> = group.sections().collect();
    let matches: Vec<usize> = tabs
        .iter()
        .map(|section| {
            if query.trim().is_empty() {
                0
            } else {
                search_match_count_in_section(*section, query)
            }
        })
        .collect();
    let labels: Vec<String> = tabs
        .iter()
        .zip(&matches)
        .map(|(section, matches)| {
            if *matches > 0 {
                format!(" {} •{matches} ", section.label())
            } else {
                format!(" {} ", section.label())
            }
        })
        .collect();
    let widths: Vec<u16> = labels
        .iter()
        .map(|label| u16::try_from(UnicodeWidthStr::width(label.as_str())).unwrap_or(u16::MAX))
        .collect();
    let active_position = tabs
        .iter()
        .position(|section| *section == active)
        .unwrap_or(0);
    let (start, end) = tab_window(&widths, active_position, content.width);
    let show_underline = content.height > 1;
    let underline_y = content.y + 1;
    if show_underline {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "─".repeat(usize::from(content.width)),
                Style::default().fg(theme::browser_decorative_separator()),
            )),
            Rect::new(content.x, underline_y, content.width, 1),
        );
    }

    let right_edge = content.x + content.width;
    let mut x = content.x;
    if start > 0 {
        frame.render_widget(
            Paragraph::new(Span::styled("‹", dim_style())),
            Rect::new(x, content.y, 1, 1),
        );
        x += 2;
    }
    for position in start..end {
        let width = widths[position].min(right_edge.saturating_sub(x));
        if width == 0 {
            break;
        }
        let style = if position == active_position {
            Style::default()
                .fg(theme::crust())
                .bg(accent)
                .add_modifier(Modifier::BOLD)
        } else if matches[position] > 0 {
            search_hit_style()
        } else {
            Style::default().fg(theme::subtext0())
        };
        frame.render_widget(
            Paragraph::new(Span::styled(labels[position].clone(), style)),
            Rect::new(x, content.y, width, 1),
        );
        if show_underline && position == active_position {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "━".repeat(usize::from(width)),
                    Style::default().fg(accent),
                )),
                Rect::new(x, underline_y, width, 1),
            );
        }
        x = x.saturating_add(width + 1);
    }
    if end < tabs.len() && right_edge > content.x {
        frame.render_widget(
            Paragraph::new(Span::styled("›", dim_style())),
            Rect::new(right_edge - 1, content.y, 1, 1),
        );
    }
}

/// Selection and search state of one list row.
#[derive(Debug, Clone, Copy)]
struct RowState {
    selected: bool,
    focused: bool,
    matched: bool,
}

fn render_setting_row(
    frame: &mut Frame,
    row_area: Rect,
    columns: RowColumns,
    row: &SettingsRow,
    state: RowState,
    accent: Color,
    choice_width: usize,
) {
    let RowState {
        selected,
        focused,
        matched,
    } = state;
    let y = row_area.y;
    if selected {
        let bg = if focused {
            theme::selected_row_bg()
        } else {
            theme::viewed_session_row_bg()
        };
        frame.render_widget(Block::default().style(Style::default().bg(bg)), row_area);
    } else if matched {
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::operations_deck_cell_bg())),
            row_area,
        );
    }

    let (marker, marker_color) = if selected {
        ("▌", accent)
    } else if matched {
        ("•", theme::yellow())
    } else {
        (" ", accent)
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            marker,
            Style::default()
                .fg(marker_color)
                .add_modifier(Modifier::BOLD),
        )),
        Rect::new(columns.bar_x, y, 1, 1),
    );

    let label_style = if selected {
        Style::default()
            .fg(theme::text())
            .add_modifier(Modifier::BOLD)
    } else if matched {
        search_hit_style()
    } else if row.empty {
        dim_style().add_modifier(Modifier::ITALIC)
    } else {
        Style::default().fg(theme::subtext1())
    };
    let label = truncate_to_width(&row.label, usize::from(columns.label_width));
    let label_len = u16::try_from(UnicodeWidthStr::width(label.as_str())).unwrap_or(0);
    frame.render_widget(
        Paragraph::new(Span::styled(label, label_style)),
        Rect::new(columns.label_x, y, columns.label_width, 1),
    );

    // Dot leaders tie each label to its value across the gap. Two spaces
    // always separate the label from the leaders.
    let dots_x = columns.label_x + label_len + 2;
    let dots_end = columns.value_x.saturating_sub(1);
    if dots_end > dots_x {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "·".repeat(usize::from(dots_end - dots_x)),
                Style::default().fg(theme::browser_decorative_separator()),
            )),
            Rect::new(dots_x, y, dots_end - dots_x, 1),
        );
    }

    if columns.value_width == 0 {
        return;
    }
    let badge_width = if row.restart { 3 } else { 0 };
    let mut spans = fit_spans(
        widget_spans(row, accent, choice_width),
        usize::from(columns.value_width).saturating_sub(badge_width),
    );
    if row.restart {
        spans.push(Span::styled(
            "  ↻",
            Style::default()
                .fg(theme::warning_status())
                .add_modifier(Modifier::BOLD),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(columns.value_x, y, columns.value_width, 1),
    );
}

/// Position pips (`○●○`) for short option sets, a slider (`━━━●────`) for
/// long ones.
fn position_spans(index: usize, count: usize, accent: Color) -> Vec<Span<'static>> {
    if count <= 1 {
        return Vec::new();
    }
    let index = index.min(count - 1);
    if count <= 8 {
        return (0..count)
            .map(|position| {
                if position == index {
                    Span::styled(
                        "●",
                        Style::default().fg(accent).add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::styled("○", Style::default().fg(theme::overlay0()))
                }
            })
            .collect();
    }
    const CELLS: usize = 8;
    let knob = index * (CELLS - 1) / (count - 1);
    let mut spans = Vec::new();
    if knob > 0 {
        spans.push(Span::styled("━".repeat(knob), Style::default().fg(accent)));
    }
    spans.push(Span::styled(
        "●",
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    ));
    if knob + 1 < CELLS {
        spans.push(Span::styled(
            "─".repeat(CELLS - knob - 1),
            Style::default().fg(theme::overlay0()),
        ));
    }
    spans
}

/// The value widget of a row as styled spans. Choice values are centered
/// in `choice_width` cells so a tab's selectors line up.
fn widget_spans(row: &SettingsRow, accent: Color, choice_width: usize) -> Vec<Span<'static>> {
    let tone = value_tone_color(row.tone);
    match &row.widget {
        ValueWidget::Toggle(true) => {
            let on = Style::default().fg(theme::green());
            vec![
                Span::styled("━━", on),
                Span::styled("●", on.add_modifier(Modifier::BOLD)),
                Span::styled(" ON", on.add_modifier(Modifier::BOLD)),
            ]
        }
        ValueWidget::Toggle(false) => vec![
            Span::styled("○", Style::default().fg(theme::overlay1())),
            Span::styled("──", Style::default().fg(theme::overlay0())),
            Span::styled(" OFF", Style::default().fg(theme::subtext0())),
        ],
        ValueWidget::Choice { value, position } => {
            let value_width = UnicodeWidthStr::width(value.as_str());
            let padding = choice_width.saturating_sub(value_width);
            let left = padding / 2;
            let centered = format!("{}{value}{}", " ".repeat(left), " ".repeat(padding - left));
            let mut spans = vec![
                Span::styled("◂ ", Style::default().fg(accent)),
                Span::styled(
                    centered,
                    Style::default()
                        .fg(if row.tone == ValueTone::Neutral {
                            theme::text()
                        } else {
                            tone
                        })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ▸", Style::default().fg(accent)),
            ];
            if let Some((index, count)) = position {
                let pips = position_spans(*index, *count, accent);
                if !pips.is_empty() {
                    spans.push(Span::raw("  "));
                    spans.extend(pips);
                }
            }
            spans
        }
        ValueWidget::Picker { value, glyph } => {
            let mut spans = Vec::new();
            if let Some((glyph, color)) = glyph {
                spans.push(Span::styled(
                    format!("{glyph} "),
                    Style::default().fg(*color).add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(
                value.clone(),
                Style::default().fg(theme::text()),
            ));
            spans.push(Span::styled(
                " ▾",
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ));
            spans
        }
        ValueWidget::Swatch { color, text } => vec![
            match color {
                Some(color) => Span::styled("██", Style::default().fg(*color)),
                None => Span::styled("░░", Style::default().fg(theme::overlay0())),
            },
            Span::raw(" "),
            Span::styled(text.clone(), Style::default().fg(tone)),
        ],
        ValueWidget::Action { button, detail } => {
            let bg = if row.destructive {
                theme::error_status()
            } else if row.tone == ValueTone::Warning {
                theme::warning_status()
            } else {
                accent
            };
            let mut spans = vec![Span::styled(
                format!(" {button} "),
                Style::default()
                    .fg(theme::crust())
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            )];
            if !detail.is_empty() {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    detail.clone(),
                    Style::default().fg(theme::subtext1()),
                ));
            }
            spans
        }
        ValueWidget::Badge { glyph, color, text } => vec![
            Span::styled(
                format!("{glyph} "),
                Style::default().fg(*color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                text.clone(),
                Style::default().fg(if row.group.is_some() { *color } else { tone }),
            ),
        ],
        ValueWidget::Activity {
            status,
            color,
            detail,
            stop,
        } => {
            let mut spans = vec![Span::styled(
                format!("● {status}"),
                Style::default().fg(*color).add_modifier(Modifier::BOLD),
            )];
            if *stop {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    " Stop ",
                    Style::default()
                        .fg(theme::crust())
                        .bg(theme::error_status())
                        .add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(
                format!("  {detail}"),
                Style::default().fg(theme::subtext1()),
            ));
            spans
        }
        ValueWidget::Text(text) => vec![Span::styled(text.clone(), Style::default().fg(tone))],
        ValueWidget::Empty => row
            .keys
            .first()
            .map(|hint| {
                vec![
                    key_cap(hint.key),
                    Span::styled(format!(" {}", hint.label), dim_style()),
                ]
            })
            .unwrap_or_default(),
    }
}

/// Truncate spans to `width` cells, ending in `…` when anything was cut.
fn fit_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(Span::width).sum();
    if total <= width {
        return spans;
    }
    if width == 0 {
        return Vec::new();
    }
    let budget = width - 1;
    let mut used = 0;
    let mut fitted = Vec::new();
    for span in spans {
        let span_width = span.width();
        if used + span_width <= budget {
            used += span_width;
            fitted.push(span);
            continue;
        }
        let kept = take_width(span.content.as_ref(), budget - used);
        let style = span.style;
        if !kept.is_empty() {
            fitted.push(Span::styled(kept, style));
        }
        fitted.push(Span::styled("…", style));
        return fitted;
    }
    fitted
}

/// The longest prefix of `text` at most `width` cells wide.
fn take_width(text: &str, width: usize) -> String {
    let mut used = 0;
    let mut kept = String::new();
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > width {
            break;
        }
        used += ch_width;
        kept.push(ch);
    }
    kept
}

/// `text` cut to `width` cells with a trailing `…` when it does not fit.
fn truncate_to_width(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut kept = take_width(text, width - 1);
    kept.push('…');
    kept
}

fn render_scrollbar(
    frame: &mut Frame,
    track: Rect,
    start: usize,
    total: usize,
    accent: Color,
    border: Color,
) {
    let height = usize::from(track.height);
    if height == 0 || total <= height {
        return;
    }
    let thumb = (height * height / total).clamp(1, height);
    let max_start = total - height;
    let offset = start.min(max_start) * (height - thumb) / max_start.max(1);
    for row in 0..height {
        let (symbol, color) = if row >= offset && row < offset + thumb {
            ("┃", accent)
        } else {
            ("│", border)
        };
        frame.render_widget(
            Paragraph::new(Span::styled(symbol, Style::default().fg(color))),
            Rect::new(track.x, track.y + row as u16, 1, 1),
        );
    }
}

fn render_info_panel(frame: &mut Frame, area: Rect, app: &App, row: &SettingsRow, selected: usize) {
    let section = app.settings_state.section;
    let group = section.group();
    let accent = group_color(group);
    let spec = spec_for_row(app, section, selected);
    let kind = if section == SettingsSection::Usage {
        if row.action.is_empty() {
            "details"
        } else {
            "action"
        }
    } else if row.empty {
        "empty"
    } else {
        spec.map_or("info", |spec| spec.kind.label())
    };
    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            group_glyph(group),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            row.label.clone(),
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ]);
    let right = Line::from(Span::styled(format!(" {kind} "), dim_style()));
    let inner = render_panel(frame, area, false, title, Some(right));
    let content = inset_horiz(inner, 1);
    if content.height == 0 || content.width < 8 {
        return;
    }

    let roomy = content.height >= 12;
    let mut lines = vec![Line::from(Span::styled(
        row.description.clone(),
        Style::default().fg(theme::subtext1()),
    ))];
    if let Some(note) = row.note {
        if roomy {
            lines.push(Line::default());
        }
        lines.push(Line::from(vec![
            Span::styled(
                "! ",
                Style::default()
                    .fg(theme::warning_status())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(note, Style::default().fg(theme::warning_status())),
        ]));
    }
    lines.push(Line::default());
    lines.push(action_line(row));
    if roomy && !row.options.is_empty() && row.options.len() <= OPTIONS_LIST_MAX {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "OPTIONS",
            dim_style().add_modifier(Modifier::BOLD),
        )));
        for (index, option) in row.options.iter().enumerate() {
            lines.push(if row.option_index == Some(index) {
                Line::from(vec![
                    Span::styled(
                        "● ",
                        Style::default().fg(accent).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        option.clone(),
                        Style::default()
                            .fg(theme::text())
                            .add_modifier(Modifier::BOLD),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::styled("○ ", Style::default().fg(theme::overlay0())),
                    Span::styled(option.clone(), Style::default().fg(theme::subtext0())),
                ])
            });
        }
    }
    if section == SettingsSection::Usage {
        if let Some(stats) = stats_rows(app).get(selected) {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                stats.value.clone(),
                Style::default()
                    .fg(stats_tone_color(stats.tone))
                    .add_modifier(Modifier::BOLD),
            )));
            if !stats.details.is_empty() {
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    "DETAILS",
                    dim_style().add_modifier(Modifier::BOLD),
                )));
                for (key, value) in &stats.details {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{key}: "), Style::default().fg(theme::sky())),
                        Span::styled(value.clone(), Style::default().fg(theme::subtext1())),
                    ]));
                }
            }
        }
    } else if let Some(spec) = spec.filter(|_| !row.empty) {
        if roomy {
            lines.push(Line::default());
        }
        lines.push(chips_line(spec));
        if roomy {
            lines.push(Line::default());
            let contract = settings_contract(app, section, selected);
            for (key, value) in [
                ("Owner", contract.owner),
                ("Applies", contract.apply),
                ("Restart", contract.restart),
                ("Stored in", contract.persistence),
                ("Scope", contract.scope),
            ] {
                lines.push(Line::from(vec![
                    Span::styled(format!("{key:<11}"), dim_style()),
                    Span::styled(value, Style::default().fg(theme::subtext1())),
                ]));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), content);
}

/// `↵ toggle → OFF   Del reset` — what each key does on the selected row.
fn action_line(row: &SettingsRow) -> Line<'static> {
    let mut spans = Vec::new();
    if row.action.is_empty() {
        spans.push(Span::styled("◇ read-only", dim_style()));
    } else {
        spans.push(key_cap(ENTER_KEY));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            row.action.clone(),
            Style::default()
                .fg(if row.destructive {
                    theme::error_status()
                } else {
                    theme::green()
                })
                .add_modifier(Modifier::BOLD),
        ));
    }
    for hint in &row.keys {
        spans.push(Span::raw("   "));
        spans.push(key_cap(hint.key));
        spans.push(Span::styled(
            format!(" {}", hint.label),
            Style::default().fg(theme::subtext0()),
        ));
    }
    Line::from(spans)
}

/// When a change applies and where it is stored, as two colored chips.
fn chips_line(spec: &SettingSpec) -> Line<'static> {
    let (glyph, text, color) = apply_chip(spec.apply);
    let (store_glyph, store) = storage_chip(spec.owner);
    Line::from(vec![
        Span::styled(
            format!("{glyph} {text}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw("    "),
        Span::styled(format!("{store_glyph} "), Style::default().fg(theme::sky())),
        Span::styled(store, Style::default().fg(theme::subtext0())),
    ])
}

fn apply_chip(apply: SettingApply) -> (&'static str, &'static str, Color) {
    match apply {
        SettingApply::Immediate | SettingApply::Daemon(ApplyClass::Live) => {
            ("✓", "applies now", theme::green())
        }
        SettingApply::BridgeRestart => ("↻", "after bridge restart", theme::warning_status()),
        SettingApply::Daemon(ApplyClass::NextSpawn) => ("◷", "next spawn", theme::sky()),
        SettingApply::Daemon(ApplyClass::PartialLive) => ("◐", "new launches now", theme::sky()),
        SettingApply::Daemon(ApplyClass::LiveOffRestartOn) => {
            ("↻", "on after restart", theme::warning_status())
        }
        SettingApply::Daemon(ApplyClass::DaemonRestart) => {
            ("↻", "after daemon restart", theme::warning_status())
        }
        SettingApply::Daemon(ApplyClass::NotApplied) => ("·", "stored only", theme::dim_metadata()),
    }
}

fn storage_chip(owner: SettingOwner) -> (&'static str, String) {
    match owner {
        SettingOwner::Tui => ("⌂", "state.json".to_string()),
        SettingOwner::Daemon(_) | SettingOwner::DaemonFields(_) | SettingOwner::DaemonState(_) => {
            ("◆", "daemon · SQLite".to_string())
        }
        SettingOwner::File(path) => ("▤", path.to_string()),
    }
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App, row: &SettingsRow) {
    let limit = usize::from(area.width);
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    for (key, label) in footer_hints(app, row) {
        let width =
            UnicodeWidthStr::width(key.as_str()) + 1 + UnicodeWidthStr::width(label.as_str()) + 3;
        if used + width > limit {
            break;
        }
        spans.push(key_cap(key));
        spans.push(Span::styled(format!(" {label}"), dim_style()));
        spans.push(Span::raw("   "));
        used += width;
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Key prompts for the focused panel, most specific first.
fn footer_hints(app: &App, row: &SettingsRow) -> Vec<(String, String)> {
    let state = &app.settings_state;
    let hint = |key: &str, label: &str| (key.to_string(), label.to_string());
    if state.query_active {
        return vec![
            hint("type", "filter"),
            hint(ENTER_KEY, "jump to match"),
            hint("Esc", "clear"),
        ];
    }
    if state.model_dropdown.open {
        return vec![
            hint("j/k", "choose"),
            hint(ENTER_KEY, "select"),
            hint("Tab", "provider"),
            hint("Esc", "close picker"),
        ];
    }
    let mut hints = Vec::new();
    match state.focus {
        SettingsFocus::Categories => {
            hints.push(hint("j/k", "category"));
            hints.push(hint("l", "open"));
            hints.push(hint("Tab", "section"));
        }
        SettingsFocus::Items => {
            if !row.action.is_empty() {
                hints.push((ENTER_KEY.to_string(), row.action.clone()));
            }
            hints.extend(row.keys.iter().map(|key| hint(key.key, key.label)));
            hints.push(hint("j/k", "move"));
            hints.push(hint("Tab", "section"));
            hints.push(hint("h", "categories"));
        }
    }
    hints.push(hint("/", "search"));
    if !state.query.is_empty() {
        hints.push(hint("n/N", "next match"));
    }
    hints.push(hint("< >", "resize"));
    hints.push(hint("q", "close"));
    hints
}

fn inset_horiz(area: Rect, amount: u16) -> Rect {
    Rect::new(
        area.x + amount.min(area.width),
        area.y,
        area.width.saturating_sub(amount * 2),
        area.height,
    )
}

fn value_tone_color(tone: ValueTone) -> Color {
    match tone {
        ValueTone::Neutral => theme::text(),
        ValueTone::Positive => theme::green(),
        ValueTone::Warning => theme::warning_status(),
        ValueTone::Destructive => theme::error_status(),
        ValueTone::Muted => theme::dim_metadata(),
    }
}

fn visible_window(total: usize, selected: usize, capacity: usize) -> (usize, usize) {
    if total == 0 || capacity == 0 {
        return (0, 0);
    }
    let capacity = capacity.min(total);
    let selected = selected.min(total - 1);
    let max_start = total - capacity;
    let start = if selected < capacity {
        0
    } else {
        (selected + 1 - capacity).min(max_start)
    };
    (start, start + capacity)
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

fn settings_item_count(app: &App) -> usize {
    let section = app.settings_state.section;
    if DAEMON_FEATURE_SECTIONS.contains(&section) {
        return daemon_feature_rows_for_section(app, section).len().max(1);
    }
    match section {
        SettingsSection::ClaudeHooks => app.cached_hook_rows.len().max(1),
        SettingsSection::ClaudeSkills => app
            .cached_user_skills
            .as_ref()
            .map(|skills| skills.len().max(1))
            .unwrap_or(1),
        SettingsSection::Usage => stats_rows(app).len().max(1),
        SettingsSection::Budgets => budget_rows(app).len().max(1),
        SettingsSection::ProviderKeys => ProviderCredentialSlot::ALL.len(),
        SettingsSection::McpServers => app
            .cached_mcp_servers
            .as_ref()
            .map_or(1, |list| list.servers.len().max(1)),
        section => item_count(section, &app.settings).max(1),
    }
}

/// Every row of the selected section, in page order (at least one).
fn section_rows(app: &App) -> Vec<SettingsRow> {
    let section = app.settings_state.section;
    if section == SettingsSection::Usage {
        return stats_rows(app).into_iter().map(usage_row).collect();
    }
    if DAEMON_FEATURE_SECTIONS.contains(&section) {
        let rows: Vec<SettingsRow> = daemon_feature_rows_for_section(app, section)
            .into_iter()
            .map(|(spec, vec_idx)| daemon_section_row(&app.daemon_features[vec_idx], spec))
            .collect();
        return if rows.is_empty() {
            vec![daemon_loading_row()]
        } else {
            rows
        };
    }
    (0..settings_item_count(app))
        .map(|idx| settings_row(app, idx))
        .collect()
}

/// Mark every row with a restart boundary. The info card distinguishes
/// full daemon, conditional enable/restore, and independent bridge restarts.
fn restart_badge(spec: &SettingSpec) -> bool {
    matches!(
        spec.apply,
        SettingApply::BridgeRestart
            | SettingApply::Daemon(
                ApplyClass::DaemonRestart | ApplyClass::LiveOffRestartOn | ApplyClass::PartialLive
            )
    )
}

fn base_row(
    label: impl Into<String>,
    widget: ValueWidget,
    action: impl Into<String>,
    description: impl Into<String>,
) -> SettingsRow {
    SettingsRow {
        group: None,
        label: label.into(),
        widget,
        action: action.into(),
        keys: Vec::new(),
        description: description.into(),
        note: None,
        tone: ValueTone::Neutral,
        destructive: false,
        restart: false,
        empty: false,
        options: Vec::new(),
        option_index: None,
    }
}

fn bool_row(label: impl Into<String>, value: bool, description: impl Into<String>) -> SettingsRow {
    SettingsRow {
        tone: if value {
            ValueTone::Positive
        } else {
            ValueTone::Neutral
        },
        ..base_row(
            label,
            ValueWidget::Toggle(value),
            format!("toggle → {}", if value { "OFF" } else { "ON" }),
            description,
        )
    }
}

fn cycle_row(
    label: impl Into<String>,
    value: impl Into<String>,
    position: Option<(usize, usize)>,
    description: impl Into<String>,
) -> SettingsRow {
    base_row(
        label,
        ValueWidget::Choice {
            value: value.into(),
            position,
        },
        "next value",
        description,
    )
}

/// A choice row over a known option list; the info card lists the options.
fn options_row(
    label: impl Into<String>,
    options: Vec<String>,
    current: Option<usize>,
    fallback_value: impl Into<String>,
    description: impl Into<String>,
) -> SettingsRow {
    let value = current
        .and_then(|index| options.get(index).cloned())
        .unwrap_or_else(|| fallback_value.into());
    let position = current.map(|index| (index, options.len()));
    SettingsRow {
        options,
        option_index: current,
        ..cycle_row(label, value, position, description)
    }
}

fn empty_row(label: &str, keys: Vec<KeyHint>, action: &str, description: &str) -> SettingsRow {
    SettingsRow {
        keys,
        tone: ValueTone::Muted,
        empty: true,
        ..base_row(label, ValueWidget::Empty, action, description)
    }
}

fn stats_tone_color(tone: StatsTone) -> Color {
    match tone {
        StatsTone::Neutral => theme::text(),
        StatsTone::Positive => theme::green(),
        StatsTone::Warning => theme::warning_status(),
        StatsTone::Error => theme::error_status(),
        StatsTone::Muted => theme::dim_metadata(),
        StatsTone::Accent => theme::sky(),
    }
}

fn stats_group_color(group: StatsGroup) -> Color {
    match group {
        StatsGroup::Overview | StatsGroup::ModelSpend => theme::yellow(),
        StatsGroup::Safety | StatsGroup::Alerts => theme::warning_status(),
        StatsGroup::Active => theme::green(),
        StatsGroup::Denied => theme::error_status(),
        StatsGroup::Tokens => theme::sky(),
        StatsGroup::Recent => theme::mauve(),
        StatsGroup::Efficiency => theme::teal(),
    }
}

fn usage_row(stats: StatsRow) -> SettingsRow {
    let widget = if let Some(status) = stats.status {
        ValueWidget::Activity {
            status: status_label(status).to_string(),
            color: stats_tone_color(stats.tone),
            detail: stats.value,
            stop: matches!(stats.action, Some(StatsRowAction::CancelInvocation(_))),
        }
    } else if stats.action == Some(StatsRowAction::EmergencyStopAll) {
        ValueWidget::Action {
            button: "Stop all".to_string(),
            detail: stats.value,
        }
    } else {
        ValueWidget::Text(stats.value)
    };
    let action = match stats.action {
        Some(StatsRowAction::EmergencyStopAll) => "stop all model work",
        Some(StatsRowAction::CancelInvocation(_)) => "stop selected model work",
        None => "",
    };
    let tone = match stats.tone {
        StatsTone::Positive => ValueTone::Positive,
        StatsTone::Warning => ValueTone::Warning,
        StatsTone::Error => ValueTone::Destructive,
        StatsTone::Muted => ValueTone::Muted,
        StatsTone::Neutral | StatsTone::Accent => ValueTone::Neutral,
    };
    // Text telemetry carries its own semantic color, including spend and tokens.
    let widget = match widget {
        ValueWidget::Text(text) => ValueWidget::Badge {
            glyph: "·",
            color: stats_tone_color(stats.tone),
            text,
        },
        widget => widget,
    };
    SettingsRow {
        group: Some(stats.group),
        tone,
        destructive: !action.is_empty(),
        keys: vec![REFRESH_KEY],
        ..base_row(stats.label, widget, action, stats.description)
    }
}

fn unknown_row() -> SettingsRow {
    SettingsRow {
        tone: ValueTone::Warning,
        ..base_row(
            "Unknown setting",
            ValueWidget::Text("?".to_string()),
            "",
            "No renderer metadata is available for this setting.",
        )
    }
}

fn daemon_loading_row() -> SettingsRow {
    empty_row(
        "Daemon configuration loading",
        vec![REFRESH_KEY],
        "",
        "Waiting for the daemon configuration response.",
    )
}

/// Position of `value` among `options`.
fn index_of<T: PartialEq>(options: &[T], value: &T) -> Option<usize> {
    options.iter().position(|option| option == value)
}

/// A row backed by a flat `app.daemon_features` entry: the registry spec
/// supplies label and description (N-002: per-row summaries instead of one
/// generic string) and the entry supplies the live value.
fn daemon_feature_row(entry: &DaemonFeatureEntry, spec: &SettingSpec) -> SettingsRow {
    let mut description = spec.summary.to_string();
    if let Some(detail) = spec.detail {
        description.push(' ');
        description.push_str(detail);
    }
    let action_row = |button: &str, detail: &str, action: &str| {
        base_row(
            spec.label,
            ValueWidget::Action {
                button: button.to_string(),
                detail: detail.to_string(),
            },
            action,
            description.clone(),
        )
    };
    let mut row = match &entry.value {
        DaemonFeatureValue::Bool(value) => bool_row(spec.label, *value, description.clone()),
        DaemonFeatureValue::Cycle { options, current } => options_row(
            spec.label,
            options.clone(),
            (*current < options.len()).then_some(*current),
            "?",
            description.clone(),
        ),
        DaemonFeatureValue::Display(_) if entry.field == "model_control_stop_all" => SettingsRow {
            tone: ValueTone::Destructive,
            destructive: true,
            ..action_row("Stop all", "halts every model call", "stop all model calls")
        },
        DaemonFeatureValue::Display(value) if entry.field == "sandbox_build_cache_dry_run" => {
            action_row("Preview", value, "run a dry-run preview")
        }
        DaemonFeatureValue::Display(value) if entry.field == "sandbox_build_cache_reclaim_now" => {
            SettingsRow {
                tone: ValueTone::Destructive,
                destructive: true,
                ..action_row("Reclaim", value, "reclaim caches now")
            }
        }
        DaemonFeatureValue::Display(_) if entry.field == "source_worktree_settlement" => {
            SettingsRow {
                tone: ValueTone::Destructive,
                destructive: true,
                ..action_row("Open audit", "", "open the settlement audit")
            }
        }
        DaemonFeatureValue::Display(_) if entry.field == "legacy_scratch_adoption" => SettingsRow {
            tone: ValueTone::Destructive,
            destructive: true,
            ..action_row("Open list", "", "list and adopt legacy scratch")
        },
        DaemonFeatureValue::Display(value) => SettingsRow {
            tone: if value == "?" {
                ValueTone::Warning
            } else {
                ValueTone::Neutral
            },
            ..base_row(
                spec.label,
                ValueWidget::Text(value.clone()),
                "",
                description.clone(),
            )
        },
    };
    row.restart = restart_badge(spec);
    row
}

/// A daemon-feature section row: the feature row plus the section's `R`.
fn daemon_section_row(entry: &DaemonFeatureEntry, spec: &SettingSpec) -> SettingsRow {
    let mut row = daemon_feature_row(entry, spec);
    row.keys.push(REFRESH_KEY);
    row
}

fn settings_row(app: &App, idx: usize) -> SettingsRow {
    let section = app.settings_state.section;
    if DAEMON_FEATURE_SECTIONS.contains(&section) {
        let rows = daemon_feature_rows_for_section(app, section);
        return match rows.get(idx) {
            Some((spec, vec_idx)) => daemon_section_row(&app.daemon_features[*vec_idx], spec),
            None => daemon_loading_row(),
        };
    }
    let mut row = local_settings_row(app, section, idx);
    if !row.restart
        && !row.empty
        && spec_for_row(app, section, idx).is_some_and(|spec| restart_badge(spec))
    {
        row.restart = true;
    }
    row
}

fn local_settings_row(app: &App, section: SettingsSection, idx: usize) -> SettingsRow {
    let settings = &app.settings;
    match section {
        SettingsSection::Screen => match idx {
            0 => bool_row(
                "Text area background",
                settings.text_area_backfill_enabled,
                "Fill message and input surfaces instead of showing the terminal background.",
            ),
            1 => {
                let hex = settings.text_area_backfill_hex.as_str();
                base_row(
                    "Background color",
                    ValueWidget::Swatch {
                        color: crate::ui::parse_hex_color(hex),
                        text: if hex.is_empty() {
                            "theme default".to_string()
                        } else {
                            hex.to_string()
                        },
                    },
                    "edit color",
                    "Edit the optional text-surface background color.",
                )
            }
            2 => bool_row(
                "Formulation animation",
                settings.formulation_anim_enabled,
                "Reveal each new live message with a top-to-bottom wipe.",
            ),
            3 => SettingsRow {
                action: "next duration".to_string(),
                ..options_row(
                    "Formulation speed",
                    FORMULATION_ANIM_DURATIONS
                        .iter()
                        .map(|ms| format!("{ms} ms"))
                        .collect(),
                    index_of(FORMULATION_ANIM_DURATIONS, &settings.formulation_anim_ms),
                    format!("{} ms", settings.formulation_anim_ms),
                    if settings.formulation_anim_enabled {
                        "Cycle how long the formulation reveal runs."
                    } else {
                        "Cycle how long the formulation reveal runs (animation is off)."
                    },
                )
            },
            4 => options_row(
                "Activity indicator",
                ACTIVITY_STYLES
                    .iter()
                    .map(|style| style.label().to_string())
                    .collect(),
                index_of(&ACTIVITY_STYLES, &settings.activity_indicator_style),
                settings.activity_indicator_style.label(),
                "Choose the working indicator shown while a provider is active.",
            ),
            5 => options_row(
                "Detail column",
                DETAIL_ALIGNMENTS
                    .iter()
                    .map(|alignment| alignment.label().to_string())
                    .collect(),
                index_of(&DETAIL_ALIGNMENTS, &settings.detail_column_alignment),
                settings.detail_column_alignment.label(),
                "Place the session-detail transcript column; Ctrl-Left / Ctrl-Right move it.",
            ),
            _ => unknown_row(),
        },
        SettingsSection::InputPrompts => match idx {
            0 => SettingsRow {
                note: Some(
                    "Toggle OFF when the terminal cannot deliver Shift+Enter as a distinct key.",
                ),
                ..bool_row(
                    "Submit on Enter",
                    settings.submit_on_enter,
                    "Plain Enter submits multiline text; Shift+Enter inserts a newline.",
                )
            },
            1 => bool_row(
                "Auto-open question panel",
                settings.auto_open_question_modal,
                "Open pending questions automatically when no other overlay is active.",
            ),
            2 => bool_row(
                "Prompt compiler",
                settings.prompt_processor.enabled,
                "Enables prompt compilation (Ctrl-Y in the input bar); off, compile requests return nothing.",
            ),
            _ => unknown_row(),
        },
        SettingsSection::ThemeColors => theme_colors_row(idx),
        SettingsSection::TranscriptDefaults => match idx {
            0 => bool_row(
                "Show system events",
                settings.default_show_system_events,
                "Set whether new sessions show system events by default.",
            ),
            1 => bool_row(
                "Show thinking events",
                settings.default_show_thinking_events,
                "Set whether new sessions show thinking events by default.",
            ),
            2 => bool_row(
                "Hide tool results",
                settings.default_hide_tool_results,
                "Set the global tool-result visibility default and update existing sessions.",
            ),
            _ => unknown_row(),
        },
        SettingsSection::ApiProviders => match settings.custom_providers.get(idx) {
            Some(entry) => {
                let has_key = !entry.api_key.is_empty();
                let text = if entry.base_url.is_empty() {
                    if has_key { "key set" } else { "no key" }.to_string()
                } else {
                    truncate_command(&entry.base_url, 60)
                };
                SettingsRow {
                    keys: vec![key_hint("a", "add"), key_hint("d", "delete")],
                    tone: if has_key {
                        ValueTone::Neutral
                    } else {
                        ValueTone::Warning
                    },
                    ..base_row(
                        entry.name.clone(),
                        ValueWidget::Badge {
                            glyph: if has_key { "●" } else { "○" },
                            color: if has_key {
                                theme::green()
                            } else {
                                theme::warning_status()
                            },
                            text,
                        },
                        "edit provider",
                        format!(
                            "OpenAI-compatible provider endpoint; API key is {}.",
                            if has_key {
                                "configured"
                            } else {
                                "not configured"
                            }
                        ),
                    )
                }
            }
            None => empty_row(
                "No API providers configured",
                vec![key_hint("a", "add provider")],
                "add provider",
                "Add an OpenAI-compatible provider connection.",
            ),
        },
        SettingsSection::SessionList => session_list_row(settings, idx),
        SettingsSection::ModelRoles => model_role_row(app, idx),
        SettingsSection::MessageBridges => match idx {
            0 => message_bridge_row("Signal bridge", &settings.message_bridges.signal),
            1 => message_bridge_row("iMessage bridge", &settings.message_bridges.imessage),
            _ => unknown_row(),
        },
        SettingsSection::Satellites => match idx {
            0 => base_row(
                "Satellite registry",
                ValueWidget::Action {
                    button: "Open".to_string(),
                    detail: "peers, links and remote sessions".to_string(),
                },
                "open the satellite registry",
                "Operator controls for paired peers and local socket links; remote sessions are cached read-only data.",
            ),
            1 => match (
                spec_for_row(app, section, idx),
                app.daemon_features
                    .iter()
                    .find(|entry| entry.field == "satellite_polling_enabled"),
            ) {
                (Some(spec), Some(entry)) => daemon_feature_row(entry, spec),
                _ => daemon_loading_row(),
            },
            _ => unknown_row(),
        },
        SettingsSection::Remote => match idx {
            0 => base_row(
                "Remote access",
                ValueWidget::Action {
                    button: "Open".to_string(),
                    detail: "enable, devices, projects, phone URL".to_string(),
                },
                "open the remote settings",
                "Read-only phone view over your tailnet. Starts a managed gateway and a tailscale serve route (never Funnel).",
            ),
            _ => unknown_row(),
        },
        SettingsSection::SystemPrompt => match idx {
            0 => SettingsRow {
                action: "next preset".to_string(),
                ..options_row(
                    "System prompt preset",
                    SystemPromptPreset::ALL
                        .iter()
                        .map(|preset| preset.label().to_string())
                        .collect(),
                    index_of(SystemPromptPreset::ALL, &settings.system_prompt_preset),
                    settings.system_prompt_preset.label(),
                    "System-prompt preset applied to launches: Default, Concise, Code Only or Caveman.",
                )
            },
            _ => unknown_row(),
        },
        SettingsSection::ClaudeHooks => match app.cached_hook_rows.get(idx) {
            Some(hook) => {
                let matcher = hook.matcher.as_deref().unwrap_or("");
                SettingsRow {
                    keys: vec![key_hint("a", "add"), key_hint("d", "delete")],
                    ..base_row(
                        if matcher.is_empty() {
                            hook.event.label()
                        } else {
                            format!("{} · {matcher}", hook.event.label())
                        },
                        ValueWidget::Text(truncate_command(&hook.command, 80)),
                        "edit hook",
                        "Claude Code hook command stored in ~/.claude/settings.json.",
                    )
                }
            }
            None => empty_row(
                "No hooks configured",
                vec![key_hint("a", "add hook")],
                "add hook",
                "Add a Claude Code hook in ~/.claude/settings.json.",
            ),
        },
        SettingsSection::ClaudeSkills => {
            let skills = app.cached_user_skills.as_deref().unwrap_or(&[]);
            match skills.get(idx) {
                Some(skill) => SettingsRow {
                    keys: vec![key_hint("e", "enable/disable"), key_hint("d", "delete")],
                    tone: if skill.enabled {
                        ValueTone::Positive
                    } else {
                        ValueTone::Neutral
                    },
                    ..base_row(
                        skill.name.clone(),
                        ValueWidget::Badge {
                            glyph: if skill.enabled { "●" } else { "○" },
                            color: if skill.enabled {
                                theme::green()
                            } else {
                                theme::overlay1()
                            },
                            text: if skill.enabled { "enabled" } else { "disabled" }.to_string(),
                        },
                        "preview",
                        skill
                            .description
                            .clone()
                            .unwrap_or_else(|| "Claude Code user skill.".to_string()),
                    )
                },
                None => empty_row(
                    "No user skills installed",
                    Vec::new(),
                    "",
                    "Install a user skill with the Claude skills command.",
                ),
            }
        }
        SettingsSection::Usage => stats_rows(app)
            .into_iter()
            .nth(idx)
            .map(usage_row)
            .unwrap_or_else(unknown_row),
        SettingsSection::Budgets => {
            if budget_rows(app).get(idx).is_some() {
                let (label, value) = budget_row_label_value(app, idx);
                if label == "(no budget policies)" {
                    empty_row(
                        "No explicit budget policies",
                        vec![key_hint("a", "add policy"), REFRESH_KEY],
                        "add policy",
                        "Hardcoded per-scope defaults still apply until a policy is added.",
                    )
                } else {
                    SettingsRow {
                        keys: vec![key_hint("a", "add"), key_hint("d", "delete"), REFRESH_KEY],
                        ..base_row(
                            label,
                            ValueWidget::Text(value),
                            "edit policy",
                            "Daemon-owned model budget policy; d deletes the selected policy.",
                        )
                    }
                }
            } else {
                empty_row(
                    "Budgets loading",
                    vec![REFRESH_KEY],
                    "",
                    "Waiting for daemon model-control policies.",
                )
            }
        }
        SettingsSection::ProviderKeys => {
            let rows = crate::provider_credential_rows::provider_credential_rows(app);
            match (rows.get(idx).cloned(), ProviderCredentialSlot::ALL.get(idx)) {
                (Some((label, value)), Some(slot)) => {
                    let (glyph, color) = credential_badge(app, *slot);
                    SettingsRow {
                        keys: vec![
                            key_hint("s", "set"),
                            key_hint("r", "rotate"),
                            key_hint("c", "check"),
                            key_hint("d", "clear"),
                            key_hint("i", "import"),
                            REFRESH_KEY,
                        ],
                        tone: if value == "loading…" {
                            ValueTone::Muted
                        } else {
                            ValueTone::Neutral
                        },
                        ..base_row(
                            label,
                            ValueWidget::Badge {
                                glyph,
                                color,
                                text: value,
                            },
                            "set key",
                            "Key-vault slot: s set, r rotate, c check, d clear after confirmation, i import from env.",
                        )
                    }
                }
                _ => unknown_row(),
            }
        }
        SettingsSection::McpServers => {
            let rows = crate::mcp_server_rows::mcp_server_rows(app);
            match rows.get(idx).cloned() {
                Some((label, value)) => SettingsRow {
                    keys: vec![
                        key_hint("a", "add"),
                        key_hint("t", "enable"),
                        key_hint("s", "secret"),
                        key_hint("r", "rotate"),
                        key_hint("d", "clear"),
                        REFRESH_KEY,
                    ],
                    tone: if value == "loading…" {
                        ValueTone::Muted
                    } else {
                        ValueTone::Neutral
                    },
                    ..base_row(
                        label,
                        ValueWidget::Text(value),
                        "edit server",
                        "MCP server: a add, Enter edit, t enable, s set credential, r rotate credential, d clear credential.",
                    )
                },
                None => empty_row(
                    "No MCP servers configured",
                    vec![key_hint("a", "add server"), REFRESH_KEY],
                    "add server",
                    "Add an MCP server definition.",
                ),
            }
        }
        SettingsSection::EditingMode
        | SettingsSection::ModelControl
        | SettingsSection::RetriesRecovery
        | SettingsSection::StallDetection
        | SettingsSection::MemoryDreaming
        | SettingsSection::Orchestration
        | SettingsSection::CodeIntelligence
        | SettingsSection::ProviderIsolation
        | SettingsSection::SandboxStorage => daemon_loading_row(),
    }
}

/// Theme & Colors rows: picker, 17 semantic role swatches, the legacy
/// palette and the reset button.
fn theme_colors_row(idx: usize) -> SettingsRow {
    match idx {
        0 => SettingsRow {
            options: (0..theme::THEME_COUNT)
                .map(|index| theme::theme_display_name(index).to_string())
                .collect(),
            option_index: Some(theme::active_theme_index()),
            ..base_row(
                "Built-in theme",
                ValueWidget::Picker {
                    value: theme::theme_display_name(theme::active_theme_index()).to_string(),
                    glyph: Some(("◐", theme::accent())),
                },
                "open the theme picker",
                format!(
                    "Choose one of the {} built-in themes; preview is immediate.",
                    theme::THEME_COUNT
                ),
            )
        },
        1..=17 => {
            let Some(role) = crate::settings_keys::theme_role_for_settings_index(idx) else {
                return unknown_row();
            };
            let override_rgb = theme::get_theme_role_override(role);
            SettingsRow {
                keys: vec![key_hint("Del", "reset")],
                tone: if override_rgb.is_some() {
                    ValueTone::Neutral
                } else {
                    ValueTone::Muted
                },
                ..base_row(
                    role.label(),
                    ValueWidget::Swatch {
                        color: Some(theme::semantic_color(role)),
                        text: override_rgb
                            .map(|[r, g, b]| format!("#{r:02X}{g:02X}{b:02X}"))
                            .unwrap_or_else(|| "built-in".to_string()),
                    },
                    "edit color",
                    format!(
                        "Semantic role `{}`; live preview, Escape rollback, Enter commit.",
                        role.key()
                    ),
                )
            }
        }
        18 => SettingsRow {
            tone: ValueTone::Warning,
            ..base_row(
                "Reset active theme",
                ValueWidget::Action {
                    button: "Reset".to_string(),
                    detail: "clears semantic overrides".to_string(),
                },
                "reset the active theme",
                "Clear all semantic overrides without changing the selected built-in theme.",
            )
        },
        _ => unknown_row(),
    }
}

fn session_list_row(settings: &UserSettings, idx: usize) -> SettingsRow {
    if idx == 0 {
        return SettingsRow {
            action: "next preset".to_string(),
            ..options_row(
                "Navigator preset",
                NavigatorPreset::ALL
                    .iter()
                    .map(|preset| preset.label().to_string())
                    .collect(),
                index_of(&NavigatorPreset::ALL, &settings.navigator_preset),
                settings.navigator_preset.label(),
                "Cycle Dense / Operations / Cost; applies immediately.",
            )
        };
    }
    if let Some(column) = settings.navigator_column_at_row(idx) {
        let enabled = settings.navigator_optional_columns.as_ref().map_or_else(
            || {
                crate::ui::navigator_layout::preset_columns(settings.navigator_preset)
                    .contains(&column)
            },
            |columns| columns.contains(&column),
        );
        return SettingsRow {
            keys: vec![key_hint("J/K", "reorder")],
            ..bool_row(
                column.label(),
                enabled,
                "Optional navigator column; Space toggles it, J/K move it (order is saved per preset).",
            )
        };
    }
    match settings
        .card_fields
        .get(idx.saturating_sub(crate::settings::NAVIGATOR_SETTINGS_ROW_COUNT))
    {
        Some(entry) => bool_row(
            entry.field.label(),
            entry.enabled,
            "Show this field on session-list cards.",
        ),
        None => empty_row(
            "No card fields configured",
            Vec::new(),
            "",
            "The session-list field configuration is empty.",
        ),
    }
}

/// A Model Roles row: provider glyph, source and model, opening the picker.
fn model_role_row(app: &App, idx: usize) -> SettingsRow {
    let settings = &app.settings;
    let custom_name = |id: Option<uuid::Uuid>| {
        id.and_then(|id| {
            settings
                .custom_providers
                .iter()
                .find(|entry| entry.id == id)
        })
        .map(|entry| entry.name.clone())
    };
    let (label, provider, custom, model, description) = match idx {
        0 => (
            "Default model",
            app.selected_provider,
            app.custom_provider_index
                .and_then(|index| settings.custom_providers.get(index))
                .map(|entry| entry.name.clone()),
            app.selected_model
                .clone()
                .unwrap_or_else(|| "default".to_string()),
            "Default provider and model for newly launched sessions.",
        ),
        1 => (
            "Title model",
            settings.title_model_provider,
            custom_name(settings.title_model_custom_provider_id),
            settings.title_model_local.clone(),
            "Generate session titles and descriptions.",
        ),
        2 => (
            "Prompt compiler model",
            settings.prompt_processor.provider,
            custom_name(settings.prompt_processor.custom_provider_id),
            settings.prompt_processor.model.clone(),
            "Compile and refine prompts before submission.",
        ),
        3 => (
            "Memory model",
            settings.memory_model_fallback_provider,
            custom_name(settings.memory_model_fallback_custom_provider_id),
            settings.memory_model_fallback.clone(),
            "Fallback model for observation extraction and summarization.",
        ),
        4 => (
            "Dream model",
            settings.dream_model_provider,
            custom_name(settings.dream_model_custom_provider_id),
            settings.dream_model.clone(),
            "Run memory consolidation and deduction.",
        ),
        5 => (
            "Stall classifier model",
            SessionProvider::Local,
            None,
            DaemonFeatureEntry::display_value(&app.daemon_features, "stall_classifier_model")
                .unwrap_or("?")
                .to_string(),
            "Model the stall classifier calls to judge a stalled session. \
             The next classification uses the saved model.",
        ),
        _ => return unknown_row(),
    };
    let source = custom.unwrap_or_else(|| App::provider_label(provider).to_string());
    base_row(
        label,
        ValueWidget::Picker {
            value: format!("{source} · {model}"),
            glyph: Some((
                glyphs::provider_glyph(provider),
                glyphs::provider_color(provider),
            )),
        },
        "pick a model",
        description,
    )
}

fn message_bridge_row(
    label: &str,
    settings: &crate::settings::MessageBridgeSettings,
) -> SettingsRow {
    let target = if settings.account.is_empty() {
        settings.allow_from.as_str()
    } else {
        settings.account.as_str()
    };
    let (glyph, color, text) = if settings.enabled {
        (
            "●",
            theme::green(),
            if target.is_empty() {
                "ON".to_string()
            } else {
                format!("ON · {target}")
            },
        )
    } else {
        ("○", theme::overlay1(), "OFF".to_string())
    };
    SettingsRow {
        tone: if settings.enabled {
            ValueTone::Positive
        } else {
            ValueTone::Neutral
        },
        ..base_row(
            label,
            ValueWidget::Badge { glyph, color, text },
            "edit connection",
            format!("{label} connection, account and sender allowlist."),
        )
    }
}

/// State dot of a provider key slot.
fn credential_badge(app: &App, slot: ProviderCredentialSlot) -> (&'static str, Color) {
    match crate::provider_credential_rows::credential_metadata(app, slot).map(|meta| meta.state) {
        Some(CredentialState::Vault) => ("●", theme::green()),
        Some(CredentialState::EnvCompat) => ("●", theme::yellow()),
        Some(CredentialState::Generator) => ("◆", theme::sky()),
        Some(CredentialState::Cleared | CredentialState::Absent) => ("○", theme::overlay1()),
        None => ("◌", theme::overlay0()),
    }
}

fn spec_for_row(
    app: &App,
    section: SettingsSection,
    idx: usize,
) -> Option<&'static crate::settings_registry::SettingSpec> {
    if DAEMON_FEATURE_SECTIONS.contains(&section) {
        return daemon_feature_rows_for_section(app, section)
            .get(idx)
            .map(|(spec, _)| *spec);
    }
    let section_specs: Vec<&'static crate::settings_registry::SettingSpec> = SETTINGS
        .iter()
        .filter(|spec| spec.section == section)
        .collect();
    match section {
        SettingsSection::MemoryDreaming => section_specs.get(idx).copied(),
        SettingsSection::ThemeColors => match idx {
            0 => section_specs.first().copied(),
            1..=17 => section_specs.get(1).copied(),
            18 => section_specs.get(2).copied(),
            _ => None,
        },
        SettingsSection::SessionList => {
            if idx == 0 {
                section_specs.first().copied()
            } else if idx <= crate::types::NavigatorOptionalColumn::ALL.len() {
                section_specs.get(1).copied()
            } else {
                section_specs.get(2).copied()
            }
        }
        SettingsSection::ApiProviders
        | SettingsSection::ClaudeHooks
        | SettingsSection::ClaudeSkills
        | SettingsSection::Budgets
        | SettingsSection::Usage
        | SettingsSection::ProviderKeys
        | SettingsSection::McpServers => section_specs.first().copied(),
        _ => section_specs
            .get(idx)
            .copied()
            .or_else(|| section_specs.first().copied()),
    }
}

/// The info card's contract lines, derived from the registry row backing
/// this position (Epic M design D.2: "per-row and derived from owner and
/// apply").
fn settings_contract(app: &App, section: SettingsSection, idx: usize) -> SettingsContract {
    let scope = format!("{} › {}", group_title(section.group()), section.label());
    let Some(spec) = spec_for_row(app, section, idx) else {
        return SettingsContract {
            scope,
            owner: "—".to_string(),
            persistence: "—".to_string(),
            apply: "—".to_string(),
            restart: "not required".to_string(),
        };
    };
    let persistence = match spec.owner {
        SettingOwner::Tui => "state.json".to_string(),
        SettingOwner::Daemon(_) | SettingOwner::DaemonFields(_) | SettingOwner::DaemonState(_) => {
            "SQLite".to_string()
        }
        SettingOwner::File(path) => path.to_string(),
    };
    let restart = match spec.apply {
        SettingApply::Immediate => "not required",
        SettingApply::BridgeRestart => "bridge process only",
        SettingApply::Daemon(ApplyClass::DaemonRestart) => "required",
        SettingApply::Daemon(ApplyClass::LiveOffRestartOn) => "on enable only",
        SettingApply::Daemon(ApplyClass::PartialLive) => "resumed sessions only",
        SettingApply::Daemon(ApplyClass::NextSpawn | ApplyClass::Live | ApplyClass::NotApplied) => {
            "not required"
        }
    };
    SettingsContract {
        scope,
        owner: spec.owner.label(),
        persistence,
        apply: spec.apply.label().to_string(),
        restart: restart.to_string(),
    }
}

/// Truncate a single-line string to `max` chars, appending `…` if truncated.
fn truncate_command(value: &str, max: usize) -> String {
    let mut out = String::with_capacity(max + 1);
    let mut count = 0;
    for ch in value.chars() {
        if count + 1 >= max {
            out.push('…');
            return out;
        }
        if ch == '\n' || ch == '\r' {
            out.push(' ');
        } else {
            out.push(ch);
        }
        count += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_registry::SettingId;
    use ratatui::buffer::Buffer;

    fn settings_app(section: SettingsSection, focus: SettingsFocus) -> App {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.settings = UserSettings::default();
        app.settings_state.section = section;
        app.settings_state.focus = focus;
        app
    }

    fn rendered_settings(app: &App, width: u16, height: u16) -> Buffer {
        use ratatui::{Terminal, backend::TestBackend};
        let _theme = crate::ui::theme::pin_theme_state();
        let Ok(mut terminal) = Terminal::new(TestBackend::new(width, height)) else {
            panic!("settings test terminal could not be created");
        };
        assert!(
            terminal
                .draw(|frame| render_settings(app, frame, Rect::new(0, 0, width, height)))
                .is_ok()
        );
        terminal.backend().buffer().clone()
    }

    fn buffer_row(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    fn buffer_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| buffer_row(buffer, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Top-left cell of the first occurrence of `needle` inside `region`,
    /// matching one character per cell.
    fn find_cells_in(buffer: &Buffer, needle: &str, region: Rect) -> Option<(u16, u16)> {
        let needle: Vec<char> = needle.chars().collect();
        for y in region.y..region.y + region.height {
            let cells: Vec<&str> = (region.x..region.x + region.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            for start in 0..cells.len() {
                let found = needle.iter().enumerate().all(|(offset, ch)| {
                    cells.get(start + offset).is_some_and(|cell| {
                        let mut chars = cell.chars();
                        chars.next() == Some(*ch) && chars.next().is_none()
                    })
                });
                if found {
                    return Some((region.x + start as u16, y));
                }
            }
        }
        None
    }

    fn spans_text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn rail_lists_every_category_with_its_glyph() {
        let app = settings_app(SettingsSection::ThemeColors, SettingsFocus::Categories);
        let buffer = rendered_settings(&app, 120, 40);
        let text = buffer_text(&buffer);
        for group in SettingsGroup::ALL {
            let entry = format!("{} {}", group_glyph(*group), group_title(*group));
            assert!(text.contains(&entry), "rail lists {entry}:\n{text}");
        }
        assert!(buffer_row(&buffer, 0).contains("Settings"));
    }

    /// The rail highlights the category that owns the active section, with a
    /// filled bar in that category's accent color.
    #[test]
    fn rail_highlights_the_category_of_the_active_section() {
        let _theme = crate::ui::theme::pin_theme_state();
        let app = settings_app(SettingsSection::MemoryDreaming, SettingsFocus::Categories);
        let buffer = rendered_settings(&app, 120, 24);
        // Pane (0,0,120,24): the rail is x 1..31 below the title row.
        let rail = Rect::new(1, 1, 30, 22);
        let Some((x, y)) = find_cells_in(&buffer, "Agent Automation", rail) else {
            panic!("rail row for Agent Automation:\n{}", buffer_text(&buffer));
        };
        assert_eq!(buffer[(x, y)].bg, theme::selected_row_bg());
        assert_eq!(
            buffer[(rail.x + 1, y)].bg,
            group_color(SettingsGroup::AgentAutomation),
            "selection bar uses the category accent"
        );
        assert_eq!(
            buffer[(x - 2, y)].symbol(),
            group_glyph(SettingsGroup::AgentAutomation)
        );
    }

    /// The list shows the active category's sections as tabs; the active
    /// tab is a filled pill in the category color.
    #[test]
    fn tabs_show_the_category_sections_with_a_filled_active_tab() {
        let _theme = crate::ui::theme::pin_theme_state();
        let app = settings_app(SettingsSection::StallDetection, SettingsFocus::Items);
        let buffer = rendered_settings(&app, 200, 58);
        // Pane (0,0,200,58): list panel (32,1,103,56), tabs on its first
        // inner row.
        let tabs_row = buffer_row(&buffer, 2);
        for section in SettingsGroup::AgentAutomation.sections() {
            assert!(
                tabs_row.contains(section.label()),
                "tab {} in {tabs_row}",
                section.label()
            );
        }
        let Some((x, y)) = find_cells_in(&buffer, " Stall Detection ", Rect::new(32, 2, 103, 1))
        else {
            panic!("active tab pill:\n{tabs_row}");
        };
        assert_eq!(
            buffer[(x + 1, y)].bg,
            group_color(SettingsGroup::AgentAutomation)
        );
        assert_eq!(buffer[(x + 1, y)].fg, theme::crust());
        assert_eq!(
            buffer[(x + 1, y + 1)].symbol(),
            "━",
            "active tab is underlined"
        );
    }

    #[test]
    fn tab_window_keeps_the_active_tab_visible() {
        let widths = [20, 17, 19, 15, 19];
        assert_eq!(tab_window(&widths, 2, 200), (0, 5));
        assert_eq!(tab_window(&widths, 2, 83), (1, 5));
        assert_eq!(tab_window(&widths, 2, 25), (2, 3));
        assert_eq!(tab_window(&[], 0, 80), (0, 0));
    }

    #[test]
    fn values_render_as_widgets() {
        let _theme = crate::ui::theme::pin_theme_state();
        let accent = theme::accent();
        assert_eq!(
            spans_text(&widget_spans(&bool_row("Switch", true, ""), accent, 0)),
            "━━● ON"
        );
        assert_eq!(
            spans_text(&widget_spans(&bool_row("Switch", false, ""), accent, 0)),
            "○── OFF"
        );

        let preset = options_row(
            "Navigator preset",
            vec!["Dense".into(), "Operations".into(), "Cost".into()],
            Some(1),
            "?",
            "",
        );
        assert_eq!(
            spans_text(&widget_spans(&preset, accent, 0)),
            "◂ Operations ▸  ○●○"
        );
        assert_eq!(
            spans_text(&widget_spans(&preset, accent, 14)),
            "◂   Operations   ▸  ○●○",
            "selectors center their value in the tab's selector width"
        );

        let swatch = base_row(
            "Accent",
            ValueWidget::Swatch {
                color: Some(Color::Rgb(1, 2, 3)),
                text: "#010203".to_string(),
            },
            "edit color",
            "",
        );
        let spans = widget_spans(&swatch, accent, 0);
        assert_eq!(spans_text(&spans), "██ #010203");
        assert_eq!(spans[0].style.fg, Some(Color::Rgb(1, 2, 3)));

        let picker = base_row(
            "Default model",
            ValueWidget::Picker {
                value: "Claude · claude-sonnet-5".to_string(),
                glyph: Some(("✻", theme::peach())),
            },
            "pick a model",
            "",
        );
        assert_eq!(
            spans_text(&widget_spans(&picker, accent, 0)),
            "✻ Claude · claude-sonnet-5 ▾"
        );

        let reset = SettingsRow {
            tone: ValueTone::Warning,
            ..base_row(
                "Reset active theme",
                ValueWidget::Action {
                    button: "Reset".to_string(),
                    detail: "clears semantic overrides".to_string(),
                },
                "reset the active theme",
                "",
            )
        };
        let spans = widget_spans(&reset, accent, 0);
        assert_eq!(spans_text(&spans), " Reset   clears semantic overrides");
        assert_eq!(spans[0].style.bg, Some(theme::warning_status()));
    }

    #[test]
    fn position_pips_and_slider_mark_the_current_option() {
        let _theme = crate::ui::theme::pin_theme_state();
        let accent = theme::accent();
        assert_eq!(spans_text(&position_spans(0, 3, accent)), "●○○");
        assert_eq!(spans_text(&position_spans(2, 3, accent)), "○○●");
        assert_eq!(spans_text(&position_spans(0, 100, accent)), "●───────");
        assert_eq!(spans_text(&position_spans(85, 100, accent)), "━━━━━━●─");
        assert_eq!(spans_text(&position_spans(99, 100, accent)), "━━━━━━━●");
        assert_eq!(spans_text(&position_spans(0, 1, accent)), "");
    }

    #[test]
    fn fit_spans_truncates_with_an_ellipsis() {
        let spans = vec![Span::raw("abc"), Span::raw("defgh")];
        assert_eq!(spans_text(&fit_spans(spans.clone(), 8)), "abcdefgh");
        assert_eq!(spans_text(&fit_spans(spans.clone(), 6)), "abcde…");
        assert_eq!(spans_text(&fit_spans(spans, 0)), "");
        assert_eq!(truncate_to_width("Providers & Sandboxes", 10), "Providers…");
        assert_eq!(truncate_to_width("Models", 10), "Models");
    }

    /// Selected rows keep the `▌ label` marker followed by two spaces, the
    /// shape the terminal end-to-end driver reads to find the selection.
    #[test]
    fn selected_row_shows_marker_label_and_switch() {
        let mut app = settings_app(SettingsSection::InputPrompts, SettingsFocus::Items);
        app.settings.submit_on_enter = true;
        let buffer = rendered_settings(&app, 120, 40);
        let text = buffer_text(&buffer);
        let Some(line) = text
            .lines()
            .find(|line| line.contains("▌ Submit on Enter  "))
        else {
            panic!("selected row marker:\n{text}");
        };
        assert!(line.contains("━━● ON"), "{line}");
    }

    #[test]
    fn search_query_and_matched_setting_are_visible() {
        let _theme = crate::ui::theme::pin_theme_state();
        let mut app = settings_app(SettingsSection::RetriesRecovery, SettingsFocus::Items);
        app.settings_state.query = "retry max backoff".to_string();
        app.settings_state.query_active = true;
        let buffer = rendered_settings(&app, 120, 24);
        assert!(buffer_row(&buffer, 0).contains("/retry max backoff"));
        let Some((x, y)) = find_cells_in(&buffer, "Retry max backoff", buffer.area) else {
            panic!("matching row rendered:\n{}", buffer_text(&buffer));
        };
        assert_eq!(buffer[(x - 2, y)].symbol(), "•");
        assert_eq!(buffer[(x, y)].fg, theme::yellow());
    }

    /// (c) acceptance: `every_rendered_row_comes_from_settings_registry`.
    /// Every fixed-count section's rendered row labels are exactly the
    /// registry's row labels for that section, in the registry's order —
    /// proof the page is driven by `SETTINGS`, not a parallel hardcoded
    /// table. Dynamic-list sections (`ThemeColors` role rows, `SessionList`
    /// optional columns/card fields, API providers, hooks, skills) are
    /// exempt: one registry row legitimately expands into N rendered rows
    /// there (`SettingKind::DynamicList`), so a positional 1:1 comparison
    /// does not apply.
    #[test]
    fn every_rendered_row_comes_from_settings_registry() {
        let fixed_sections = [
            SettingsSection::Screen,
            SettingsSection::TranscriptDefaults,
            SettingsSection::InputPrompts,
            SettingsSection::ModelRoles,
            SettingsSection::SystemPrompt,
            SettingsSection::MessageBridges,
            SettingsSection::Satellites,
            SettingsSection::Remote,
            SettingsSection::ModelControl,
            SettingsSection::RetriesRecovery,
            SettingsSection::StallDetection,
            SettingsSection::Orchestration,
            SettingsSection::CodeIntelligence,
            SettingsSection::ProviderIsolation,
            SettingsSection::SandboxStorage,
            SettingsSection::MemoryDreaming,
        ];
        for section in fixed_sections {
            let mut test_app = crate::app::app_test_helpers::with_session_list(0);
            test_app.settings_state.section = section;
            let registry_labels: Vec<&str> = SETTINGS
                .iter()
                .filter(|spec| spec.section == section)
                .map(|spec| spec.label)
                .collect();
            assert_eq!(
                settings_item_count(&test_app),
                registry_labels.len(),
                "{section:?} row count matches its registry rows"
            );
            let rows = section_rows(&test_app);
            for (idx, expected_label) in registry_labels.iter().enumerate() {
                let row = settings_row(&test_app, idx);
                assert_eq!(
                    &row.label, expected_label,
                    "{section:?} row {idx} comes from the registry"
                );
                assert_eq!(&rows[idx].label, expected_label);
            }
        }
    }

    /// (c) acceptance: `daemon_rows_render_their_spec_summary`. N-002: every
    /// daemon-features row's description is that row's own registry
    /// summary, not the old generic `"Daemon-owned runtime configuration
    /// persisted in SQLite."` string.
    #[test]
    fn daemon_rows_render_their_spec_summary() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.settings_state.section = SettingsSection::RetriesRecovery;
        let row = settings_row(&app, 0); // RetryOnFailure
        assert_eq!(row.label, "Retry on failure");
        assert_eq!(
            row.description,
            "Kill-switch for durable automatic retry of failed sessions."
        );
        assert!(row.keys.contains(&REFRESH_KEY));
    }

    #[test]
    fn restart_badges_match_live_and_restart_only_consumers() {
        for (id, expected) in [
            (SettingId::StallDetection, false),
            (SettingId::ClassifierModel, false),
            (SettingId::ObservationThreshold, false),
            (SettingId::DreamCooldown, false),
            (SettingId::RsidScopeMemoryHigh, true),
            (SettingId::MemorySystem, true),
            (SettingId::ContextRotation, true),
            (SettingId::SignalBridge, true),
            (SettingId::ImessageBridge, true),
        ] {
            let spec = SETTINGS.iter().find(|spec| spec.id == id).unwrap();
            assert_eq!(restart_badge(spec), expected, "{id:?}");
        }
        let mut app = settings_app(SettingsSection::Orchestration, SettingsFocus::Items);
        app.settings_state.selected_index = SETTINGS
            .iter()
            .filter(|spec| spec.section == SettingsSection::Orchestration)
            .position(|spec| spec.id == SettingId::RsidScopeMemoryHigh)
            .unwrap();
        let text = buffer_text(&rendered_settings(&app, 200, 58));
        assert!(text.contains("↻ after daemon restart"), "{text}");
        let app = settings_app(SettingsSection::MessageBridges, SettingsFocus::Items);
        let text = buffer_text(&rendered_settings(&app, 200, 58));
        assert!(text.contains("↻ after bridge restart"), "{text}");
    }

    /// (c) acceptance: `codegraph_row_lives_in_code_intelligence`. Never
    /// assert a label is absent (AGENTS.md) — assert the positive location:
    /// Codegraph indexing is the `CodeIntelligence` section's one row.
    #[test]
    fn codegraph_row_lives_in_code_intelligence() {
        let labels: Vec<&str> = SETTINGS
            .iter()
            .filter(|spec| spec.section == SettingsSection::CodeIntelligence)
            .map(|spec| spec.label)
            .collect();
        assert_eq!(labels, vec!["Codegraph indexing"]);
    }

    #[test]
    fn codegraph_indexing_settings_row_has_toggle_and_description() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.settings_state.section = SettingsSection::CodeIntelligence;
        let index = 0;
        let row = settings_row(&app, index);
        assert_eq!(row.label, "Codegraph indexing");
        assert_eq!(row.widget, ValueWidget::Toggle(false));
        assert_eq!(row.action, "toggle → ON");
        assert_eq!(
            row.description,
            "Indexes project source into the code graph used by code-intelligence views."
        );
        assert_eq!(row.description.lines().count(), 1);

        DaemonFeatureEntry::update_from_json(
            &mut app.daemon_features,
            &serde_json::json!({"codegraph_indexing_enabled": true}),
        );
        let row = settings_row(&app, index);
        assert_eq!(row.widget, ValueWidget::Toggle(true));
        assert_eq!(row.action, "toggle → OFF");
    }

    #[test]
    fn boolean_vocabulary_has_one_state_and_exact_result() {
        let row = bool_row("Submit on Enter", true, "description");
        assert_eq!(row.widget, ValueWidget::Toggle(true));
        assert_eq!(row.widget.plain(), "ON");
        assert_eq!(row.action, "toggle → OFF");
        assert_eq!(row.label, "Submit on Enter");
    }

    #[test]
    fn layout_120_stacks_the_info_card_under_the_list() {
        let layout = settings_workspace_layout(
            Rect::new(0, 2, 120, 37),
            SettingsFocus::Items,
            &UserSettings::default(),
        );
        assert_eq!(layout.rail, Some(Rect::new(1, 2, 30, 37)));
        assert_eq!(layout.list, Some(Rect::new(32, 2, 87, 28)));
        assert_eq!(layout.info, Some(Rect::new(32, 30, 87, 9)));
        assert!(!layout.info_beside);
    }

    #[test]
    fn layout_200_puts_the_info_card_beside_the_list() {
        let layout = settings_workspace_layout(
            Rect::new(0, 2, 200, 55),
            SettingsFocus::Items,
            &UserSettings::default(),
        );
        assert_eq!(layout.rail, Some(Rect::new(1, 2, 30, 55)));
        assert_eq!(layout.list, Some(Rect::new(32, 2, 103, 55)));
        assert_eq!(layout.info, Some(Rect::new(136, 2, 63, 55)));
        assert!(layout.info_beside);
        assert_eq!((layout.rail_max, layout.info_max), (44, 90));
    }

    #[test]
    fn layout_240_caps_the_automatic_info_card() {
        let layout = settings_workspace_layout(
            Rect::new(0, 2, 240, 67),
            SettingsFocus::Items,
            &UserSettings::default(),
        );
        assert_eq!(layout.list, Some(Rect::new(32, 2, 134, 67)));
        assert_eq!(layout.info, Some(Rect::new(167, 2, 72, 67)));
    }

    #[test]
    fn stored_widths_resize_the_rail_and_info_card() {
        let settings = UserSettings {
            settings_rail_width: Some(40),
            settings_info_width: Some(50),
            ..UserSettings::default()
        };
        let layout =
            settings_workspace_layout(Rect::new(0, 2, 200, 55), SettingsFocus::Items, &settings);
        assert_eq!(layout.rail, Some(Rect::new(1, 2, 40, 55)));
        assert_eq!(layout.list, Some(Rect::new(42, 2, 106, 55)));
        assert_eq!(layout.info, Some(Rect::new(149, 2, 50, 55)));
    }

    #[test]
    fn stored_widths_clamp_to_the_pane() {
        let settings = UserSettings {
            settings_rail_width: Some(200),
            settings_info_width: Some(1),
            ..UserSettings::default()
        };
        let layout = settings_workspace_layout(
            Rect::new(0, 2, 200, 55),
            SettingsFocus::Categories,
            &settings,
        );
        assert_eq!(layout.rail.map(|rail| rail.width), Some(RAIL_MAX_WIDTH));
        assert_eq!(layout.info.map(|info| info.width), Some(INFO_MIN_WIDTH));
        assert!(layout.list.is_some_and(|list| list.width >= LIST_MIN_WIDTH));
    }

    #[test]
    fn narrow_panes_collapse_the_rail_to_icons_while_editing() {
        let settings = UserSettings::default();
        let body = Rect::new(0, 2, 90, 30);
        let editing = settings_workspace_layout(body, SettingsFocus::Items, &settings);
        assert_eq!(
            editing.rail.map(|rail| rail.width),
            Some(COMPACT_RAIL_WIDTH)
        );
        assert_eq!(editing.rail_max, 0);
        let browsing = settings_workspace_layout(body, SettingsFocus::Categories, &settings);
        assert_eq!(browsing.rail.map(|rail| rail.width), Some(26));

        let app = settings_app(SettingsSection::StallDetection, SettingsFocus::Items);
        let buffer = rendered_settings(&app, 90, 30);
        let rail = Rect::new(1, 1, COMPACT_RAIL_WIDTH, 28);
        for group in SettingsGroup::ALL {
            assert!(
                find_cells_in(&buffer, group_glyph(*group), rail).is_some(),
                "compact rail shows {group:?}:\n{}",
                buffer_text(&buffer)
            );
        }
        assert!(buffer_text(&buffer).contains("Classifier confidence floor"));
    }

    #[test]
    fn single_panel_fallback_follows_keyboard_focus() {
        let body = Rect::new(0, 2, 60, 30);
        let settings = UserSettings::default();
        let categories = settings_workspace_layout(body, SettingsFocus::Categories, &settings);
        assert_eq!(categories.rail, Some(Rect::new(1, 2, 58, 30)));
        assert_eq!(categories.list, None);

        let items = settings_workspace_layout(body, SettingsFocus::Items, &settings);
        assert_eq!(items.rail, None);
        assert_eq!(items.list, Some(Rect::new(1, 2, 58, 21)));
        assert_eq!(items.info, Some(Rect::new(1, 23, 58, 9)));
    }

    #[test]
    fn rendered_layout_records_what_the_resize_keys_step_from() {
        let mut app = settings_app(SettingsSection::ThemeColors, SettingsFocus::Items);
        record_rendered_layout(&mut app, Rect::new(0, 1, 200, 57));
        assert_eq!(
            app.settings_state.rendered_layout,
            SettingsRenderedLayout {
                rail_width: 30,
                rail_max: 44,
                info_width: Some(63),
                info_max: 90,
            }
        );

        record_rendered_layout(&mut app, Rect::new(0, 1, 90, 30));
        assert_eq!(app.settings_state.rendered_layout.rail_width, 0);
        assert_eq!(app.settings_state.rendered_layout.info_width, None);
    }

    #[test]
    fn info_card_lists_choice_options_and_marks_the_current_one() {
        let mut app = settings_app(SettingsSection::Screen, SettingsFocus::Items);
        app.settings_state.selected_index = 4; // Activity indicator
        let buffer = rendered_settings(&app, 200, 58);
        let text = buffer_text(&buffer);
        for expected in [
            "OPTIONS",
            "● Semantic",
            "○ Rainbow Starlight",
            "✓ applies now",
            "⌂ state.json",
        ] {
            assert!(
                text.contains(expected),
                "info card shows {expected}:\n{text}"
            );
        }
    }

    #[test]
    fn submit_on_enter_card_carries_the_terminal_note() {
        let app = settings_app(SettingsSection::InputPrompts, SettingsFocus::Items);
        let buffer = rendered_settings(&app, 200, 58);
        let text = buffer_text(&buffer);
        assert!(
            text.contains("Toggle OFF when the terminal cannot deliver"),
            "{text}"
        );
        assert!(
            text.contains("Plain Enter submits multiline text"),
            "{text}"
        );
    }

    #[test]
    fn footer_prompts_follow_the_selected_row() {
        let mut app = settings_app(SettingsSection::InputPrompts, SettingsFocus::Items);
        app.settings.submit_on_enter = true;
        let buffer = rendered_settings(&app, 120, 40);
        let footer = buffer_row(&buffer, 39);
        for expected in [
            "↵ toggle → OFF",
            "j/k move",
            "Tab section",
            "< > resize",
            "q close",
        ] {
            assert!(
                footer.contains(expected),
                "footer shows {expected}: {footer}"
            );
        }

        app.settings_state.focus = SettingsFocus::Categories;
        let buffer = rendered_settings(&app, 120, 40);
        let footer = buffer_row(&buffer, 39);
        assert!(footer.contains("j/k category"), "{footer}");
    }

    #[test]
    fn mcp_servers_empty_list_offers_an_add_prompt() {
        let mut app = settings_app(SettingsSection::McpServers, SettingsFocus::Items);
        app.cached_mcp_servers = Some(rsi_common::mcp::ListMcpServersResult {
            servers: Vec::new(),
        });
        assert_eq!(settings_item_count(&app), 1);
        let row = settings_row(&app, 0);
        assert_eq!(row.label, "No MCP servers configured");
        assert!(row.empty);
        assert_eq!(row.keys.first(), Some(&key_hint("a", "add server")));
    }

    #[test]
    fn visible_window_keeps_dense_selection_on_screen() {
        assert_eq!(visible_window(21, 0, 8), (0, 8));
        assert_eq!(visible_window(21, 7, 8), (0, 8));
        assert_eq!(visible_window(21, 8, 8), (1, 9));
        assert_eq!(visible_window(21, 20, 8), (13, 21));
    }

    #[test]
    fn rsid_scope_controls_render_in_orchestration() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.settings_state.section = SettingsSection::Orchestration;
        let rows: Vec<String> = SETTINGS
            .iter()
            .filter(|spec| spec.section == SettingsSection::Orchestration)
            .enumerate()
            .map(|(idx, _spec)| settings_row(&app, idx).label)
            .collect();
        for expected in [
            "rsid MemoryHigh (MiB)",
            "rsid MemoryMax (MiB)",
            "rsid MemorySwapMax (MiB)",
            "rsid CPUWeight",
        ] {
            assert!(rows.iter().any(|label| label == expected), "{expected}");
        }
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn provider_keys_row_shows_positive_state_fingerprint_and_exposure_text() {
        use rsi_common::provider_credentials::{
            CliExposure, CredentialRoute, ListProviderCredentialsResult, ProviderCredentialMetadata,
        };

        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.settings_state.section = SettingsSection::ProviderKeys;
        app.cached_provider_credentials = Some(ListProviderCredentialsResult {
            env_compat: true,
            check_ttl_secs: 600,
            credentials: vec![ProviderCredentialMetadata {
                slot: ProviderCredentialSlot::Openrouter,
                state: CredentialState::Vault,
                fingerprint: Some("ab12ef34".to_string()),
                set_at: None,
                rotated_from_fingerprint: None,
                cleared_at: None,
                check: None,
                generation: 1,
                route: CredentialRoute::CodexCli,
                cli_exposure: CliExposure::Always,
                last_cli_exposure_at: None,
            }],
        });

        let idx = ProviderCredentialSlot::ALL
            .iter()
            .position(|slot| *slot == ProviderCredentialSlot::Openrouter)
            .unwrap();
        let row = settings_row(&app, idx);
        assert_eq!(row.label, "openrouter");
        let value = row.widget.plain();
        assert!(value.contains("vault"), "{value}");
        assert!(value.contains("fp:ab12ef34"), "{value}");
        assert!(value.contains("cli:always"), "{value}");
        assert!(value.contains("route:codex_cli"), "{value}");
        assert!(matches!(row.widget, ValueWidget::Badge { glyph: "●", .. }));
    }

    #[test]
    fn provider_keys_row_count_is_the_fixed_slot_count() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.settings_state.section = SettingsSection::ProviderKeys;
        assert_eq!(settings_item_count(&app), ProviderCredentialSlot::ALL.len());
    }

    #[test]
    fn provider_keys_section_renders_slots_and_set_key_hint() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.settings_state.section = SettingsSection::ProviderKeys;
        assert_eq!(settings_item_count(&app), ProviderCredentialSlot::ALL.len());
        let row = settings_row(&app, 0);
        assert_eq!(row.label, ProviderCredentialSlot::ALL[0].as_str());
        assert!(row.keys.contains(&key_hint("s", "set")));
        app.settings_state.focus = SettingsFocus::Items;
        let buffer = rendered_settings(&app, 240, 70);
        let visible = buffer_text(&buffer);
        assert!(visible.contains(ProviderCredentialSlot::ALL[0].as_str()));
        assert!(visible.contains("s set"));
    }
    #[test]
    fn usage_dashboard_renders_groups_names_and_semantic_status_color() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        let mut app = crate::model_control_stats::tests::named_work_app();
        app.settings_state.section = SettingsSection::Usage;
        app.settings_state.focus = SettingsFocus::Items;
        let rows = section_rows(&app);
        let selected = rows
            .iter()
            .position(|row| row.group == Some(StatsGroup::Active))
            .unwrap();
        app.settings_state.selected_index = selected;
        let buffer = rendered_settings(&app, 220, 55);
        let text = buffer_text(&buffer);
        assert!(text.contains("LIFETIME USAGE"));
        assert!(text.contains("SPENDING CONTROLS"));
        assert!(text.contains("LIVE MODEL WORK  1"));
        assert!(text.contains("Fix settings navigation · RSI"));
        assert!(text.contains("Start session"));
        assert!(text.contains("Interrupt the owning session"));
        let spans = widget_spans(&rows[selected], theme::yellow(), 0);
        assert_eq!(spans[0].content, "● Running");
        assert_eq!(spans[0].style.fg, Some(theme::green()));
        let stop = spans.iter().find(|span| span.content == " Stop ").unwrap();
        assert_eq!(stop.style.bg, Some(theme::error_status()));
    }

    #[test]
    fn usage_headings_preserve_row_indices_and_scroll_selection_at_small_sizes() {
        let mut app = crate::model_control_stats::tests::named_work_app();
        app.settings_state.section = SettingsSection::Usage;
        let rows = section_rows(&app);
        let lines = settings_list_lines(&rows);
        let row_indices: Vec<_> = lines
            .iter()
            .filter_map(|line| match line {
                SettingsListLine::Row(idx) => Some(*idx),
                _ => None,
            })
            .collect();
        assert_eq!(row_indices, (0..rows.len()).collect::<Vec<_>>());
        for selected in 0..rows.len() {
            for capacity in [1, 2, 3, 7, 20] {
                let (visible, _, _) = visible_settings_lines(&rows, selected, capacity);
                assert!(visible.len() <= capacity);
                assert!(visible.contains(&SettingsListLine::Row(selected)));
            }
        }
        let first_token = rows
            .iter()
            .position(|row| row.group == Some(StatsGroup::Tokens))
            .unwrap();
        let (visible, _, _) = visible_settings_lines(&rows, first_token + 3, 3);
        assert!(matches!(
            visible.first(),
            Some(SettingsListLine::Heading(StatsGroup::Tokens, _))
        ));
    }

    #[test]
    fn usage_dashboard_keeps_selected_name_visible_in_narrow_pane() {
        let mut app = crate::model_control_stats::tests::named_work_app();
        app.settings_state.section = SettingsSection::Usage;
        app.settings_state.focus = SettingsFocus::Items;
        app.settings_state.selected_index = section_rows(&app)
            .iter()
            .position(|row| row.group == Some(StatsGroup::Active))
            .unwrap();
        let buffer = rendered_settings(&app, 70, 24);
        let text = buffer_text(&buffer);
        assert!(text.contains("Fix settings navigation"));
        assert!(text.contains("Running"));
        assert!(text.contains("Stop"));
    }
}
