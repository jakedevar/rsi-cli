//! Settings workspace rendering.
//!
//! The surface is owned by the focused pane. Its composition changes at explicit
//! width breakpoints while category order, item order, and key behavior remain
//! unchanged.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::app::App;
use crate::model_control_budgets::{budget_row_label_value, budget_rows};
use crate::model_control_stats::{StatsRowAction, stats_row_label_value, stats_rows};
use crate::settings::{DaemonFeatureEntry, DaemonFeatureValue};
use crate::settings_keys::{
    DAEMON_FEATURE_SECTIONS, daemon_feature_rows_for_section, item_count, search_row_matches,
};
use crate::settings_registry::{
    SETTINGS, SettingApply, SettingOwner, SettingsGroup, SettingsSection,
};
use crate::types::SettingsFocus;
use crate::ui::theme;

const CATEGORY_RAIL_WIDTH: u16 = 32;
const NARROW_CATEGORY_RAIL_WIDTH: u16 = 27;
const ITEMS_WIDTH: u16 = 94;
const CONTEXT_WIDTH: u16 = 64;
const WIDE_CORE_WIDTH: u16 = CATEGORY_RAIL_WIDTH + 1 + ITEMS_WIDTH + 1 + CONTEXT_WIDTH;
const IMPACT_BREAKPOINT: u16 = 237;
const TRIPLE_BREAKPOINT: u16 = 160;
const DUAL_BREAKPOINT: u16 = 96;
const ITEMS_START_ROW: u16 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SettingsWorkspaceLayout {
    categories: Option<Rect>,
    items: Option<Rect>,
    context: Option<Rect>,
    impact: Option<Rect>,
}

#[derive(Debug, Clone, Copy)]
enum CategoryRailRow {
    Group(SettingsGroup),
    Section(SettingsSection),
}

/// The rail's row order: a group header followed by its sections, in
/// `SettingsGroup::ALL` / `SettingsSection::ALL` order (Epic M design D.1).
fn section_rail_rows() -> Vec<CategoryRailRow> {
    let mut rows = Vec::with_capacity(SettingsGroup::ALL.len() + SettingsSection::ALL.len());
    for group in SettingsGroup::ALL {
        rows.push(CategoryRailRow::Group(*group));
        for section in SettingsSection::ALL
            .iter()
            .filter(|section| section.group() == *group)
        {
            rows.push(CategoryRailRow::Section(*section));
        }
    }
    rows
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueTone {
    Neutral,
    Positive,
    Warning,
    Destructive,
    Muted,
}

#[derive(Debug, Clone)]
struct SettingsRow {
    label: String,
    value: String,
    short_action: String,
    selected_action: String,
    description: String,
    tone: ValueTone,
    destructive: bool,
    empty: bool,
}

#[derive(Debug, Clone)]
struct SettingsContract {
    default_value: Option<&'static str>,
    scope: String,
    owner: String,
    persistence: String,
    apply: String,
    restart: String,
}

/// Render the breakpoint-aware Settings workspace.
pub fn render_settings(app: &App, frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::root_bg())),
        area,
    );

    let title_area = Rect::new(area.x, area.y, area.width, 1);
    render_workspace_title(frame, title_area, app);

    if area.height < 2 {
        return;
    }

    let workspace = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
    let layout = settings_workspace_layout(workspace, app.settings_state.focus);
    let total_items = settings_item_count(app).max(1);
    let selected_index = app
        .settings_state
        .selected_index
        .min(total_items.saturating_sub(1));
    let selected_row = settings_row(app, selected_index);

    if let Some(categories) = layout.categories {
        render_categories(frame, categories, app);
    }

    let dropdown_anchor = layout.items.and_then(|items| {
        render_items(
            frame,
            items,
            app,
            total_items,
            selected_index,
            layout.context.is_some(),
        )
    });

    if let Some(context) = layout.context {
        render_context_panel(frame, context, app, selected_index, &selected_row);
    }

    if let Some(impact) = layout.impact {
        render_impact_panel(frame, impact, app, selected_index, &selected_row);
    }

    // Keep the existing model-picker interaction contract: one physical row per
    // Agent Actors item, a 50-cell popup, and an anchor immediately below the
    // active row. Rendering last preserves painter-order overlap.
    if app.settings_state.model_dropdown.open
        && app.settings_state.section == SettingsSection::ModelRoles
        && let Some(anchor) = dropdown_anchor
    {
        let item_idx = app
            .settings_state
            .active_dropdown_item
            .unwrap_or(selected_index);
        let current_model: Option<&str> = match item_idx {
            0 => app.selected_model.as_deref(),
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
            &app.settings_state.model_dropdown,
            current_model,
            app.is_provider_available(app.settings_state.model_dropdown.provider),
        );
    }
}

fn settings_workspace_layout(area: Rect, focus: SettingsFocus) -> SettingsWorkspaceLayout {
    if area.width < DUAL_BREAKPOINT {
        return match focus {
            SettingsFocus::Categories => SettingsWorkspaceLayout {
                categories: Some(area),
                items: None,
                context: None,
                impact: None,
            },
            SettingsFocus::Items => SettingsWorkspaceLayout {
                categories: None,
                items: Some(area),
                context: None,
                impact: None,
            },
        };
    }

    if area.width < TRIPLE_BREAKPOINT {
        let gutter = 2;
        let categories = Rect::new(
            area.x + gutter,
            area.y,
            NARROW_CATEGORY_RAIL_WIDTH,
            area.height,
        );
        let items_x = categories.x + categories.width + 1;
        let right = area.x + area.width - gutter;
        let items = Rect::new(items_x, area.y, right.saturating_sub(items_x), area.height);
        return SettingsWorkspaceLayout {
            categories: Some(categories),
            items: Some(items),
            context: None,
            impact: None,
        };
    }

    if area.width >= IMPACT_BREAKPOINT {
        let gutter = 4;
        let start = area.x + gutter;
        let categories = Rect::new(start, area.y, CATEGORY_RAIL_WIDTH, area.height);
        let items = Rect::new(
            categories.x + categories.width + 1,
            area.y,
            ITEMS_WIDTH,
            area.height,
        );
        let context = Rect::new(
            items.x + items.width + 1,
            area.y,
            CONTEXT_WIDTH,
            area.height,
        );
        let impact_x = context.x + context.width + 1;
        let impact_right = area.x + area.width - gutter;
        let impact = Rect::new(
            impact_x,
            area.y,
            impact_right.saturating_sub(impact_x),
            area.height,
        );
        return SettingsWorkspaceLayout {
            categories: Some(categories),
            items: Some(items),
            context: Some(context),
            impact: Some(impact),
        };
    }

    if area.width >= WIDE_CORE_WIDTH {
        let start = area.x + (area.width - WIDE_CORE_WIDTH) / 2;
        let categories = Rect::new(start, area.y, CATEGORY_RAIL_WIDTH, area.height);
        let items = Rect::new(
            categories.x + categories.width + 1,
            area.y,
            ITEMS_WIDTH,
            area.height,
        );
        let context = Rect::new(
            items.x + items.width + 1,
            area.y,
            CONTEXT_WIDTH,
            area.height,
        );
        return SettingsWorkspaceLayout {
            categories: Some(categories),
            items: Some(items),
            context: Some(context),
            impact: None,
        };
    }

    let gutter = 2;
    let inner_width = area.width.saturating_sub(gutter * 2);
    let category_width = CATEGORY_RAIL_WIDTH.min(inner_width / 4);
    let context_width = 48.min(inner_width.saturating_sub(category_width + 2) / 3);
    let item_width = inner_width
        .saturating_sub(category_width)
        .saturating_sub(context_width)
        .saturating_sub(2);
    let categories = Rect::new(area.x + gutter, area.y, category_width, area.height);
    let items = Rect::new(
        categories.x + categories.width + 1,
        area.y,
        item_width,
        area.height,
    );
    let context = Rect::new(
        items.x + items.width + 1,
        area.y,
        context_width,
        area.height,
    );
    SettingsWorkspaceLayout {
        categories: Some(categories),
        items: Some(items),
        context: Some(context),
        impact: None,
    }
}

fn render_workspace_title(frame: &mut Frame, area: Rect, app: &App) {
    let gutter = if area.width >= TRIPLE_BREAKPOINT {
        4
    } else {
        2
    };
    let mut title = vec![
        Span::styled(
            "Settings",
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" / {}", app.settings_state.section.label()),
            Style::default().fg(theme::dim_metadata()),
        ),
    ];
    if app.settings_state.query_active || !app.settings_state.query.is_empty() {
        title.push(Span::styled(
            format!(
                "  /{}{}",
                app.settings_state.query,
                if app.settings_state.query_active {
                    "▏"
                } else {
                    ""
                }
            ),
            Style::default()
                .fg(theme::overlay_hint())
                .add_modifier(Modifier::BOLD),
        ));
    }
    let left = Line::from(title);
    let left_area = Rect::new(
        area.x + gutter.min(area.width),
        area.y,
        area.width.saturating_sub(gutter * 2),
        1,
    );
    frame.render_widget(Paragraph::new(left), left_area);

    if area.width >= TRIPLE_BREAKPOINT {
        frame.render_widget(
            Paragraph::new("CONTEXT WORKBENCH")
                .style(Style::default().fg(theme::overlay_hint()))
                .alignment(Alignment::Right),
            left_area,
        );
    }
}

fn render_categories(frame: &mut Frame, area: Rect, app: &App) {
    render_panel_surface(frame, area);
    render_panel_rule(
        frame,
        area,
        app.settings_state.focus == SettingsFocus::Categories,
    );
    render_header_pair(
        frame,
        area,
        "SECTIONS",
        if app.settings_state.focus == SettingsFocus::Categories {
            "FOCUS"
        } else {
            "21"
        },
        app.settings_state.focus == SettingsFocus::Categories,
    );

    let rows = section_rail_rows();
    let capacity = area.height.saturating_sub(5) as usize;
    if capacity == 0 {
        return;
    }
    let selected = rows
        .iter()
        .position(|row| matches!(row, CategoryRailRow::Section(section) if *section == app.settings_state.section))
        .unwrap_or(0);
    let (start, end) = visible_window(rows.len(), selected, capacity);
    for (entry, local_y) in rows[start..end].iter().zip(3_u16..) {
        let row_area = Rect::new(
            area.x + 1,
            area.y + local_y,
            area.width.saturating_sub(2),
            1,
        );
        match entry {
            CategoryRailRow::Group(group) => {
                frame.render_widget(
                    Paragraph::new(group.label()).style(
                        Style::default()
                            .fg(theme::dim_metadata())
                            .add_modifier(Modifier::BOLD),
                    ),
                    inset_horiz(row_area, 1),
                );
            }
            CategoryRailRow::Section(section) => {
                let active = *section == app.settings_state.section;
                if active {
                    frame.render_widget(
                        Block::default().style(Style::default().bg(theme::selected_row_bg())),
                        row_area,
                    );
                    if app.settings_state.focus == SettingsFocus::Categories {
                        frame.render_widget(
                            Block::default().style(Style::default().bg(theme::active_row_rail())),
                            Rect::new(row_area.x, row_area.y, 1, 1),
                        );
                    }
                }
                let marker = if active { "▶ " } else { "  " };
                let style = if active {
                    Style::default()
                        .fg(theme::text())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme::subtext1())
                };
                let match_suffix = if app.settings_state.query.is_empty() {
                    String::new()
                } else {
                    let matches = crate::settings_keys::search_match_count_in_section(
                        *section,
                        &app.settings_state.query,
                    );
                    if matches > 0 {
                        format!(" ({matches})")
                    } else {
                        String::new()
                    }
                };
                frame.render_widget(
                    Paragraph::new(format!("{marker}{}{match_suffix}", section.label()))
                        .style(style),
                    inset_horiz(row_area, 1),
                );
            }
        }
    }
}

/// Render the items panel and return the active model-picker anchor when visible.
fn render_items(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    total_items: usize,
    selected_index: usize,
    has_context: bool,
) -> Option<Rect> {
    render_panel_surface(frame, area);
    let focused = app.settings_state.focus == SettingsFocus::Items;
    render_panel_rule(frame, area, focused);
    render_header_pair(
        frame,
        area,
        app.settings_state.section.label().to_ascii_uppercase(),
        if app.settings_state.model_dropdown.open {
            "PICKER OPEN".to_string()
        } else if focused {
            "ITEMS FOCUS".to_string()
        } else {
            format!("{}/{}", selected_index + 1, total_items)
        },
        focused || app.settings_state.model_dropdown.open,
    );

    if area.height > 2 {
        frame.render_widget(
            Paragraph::new(app.settings_state.section.summary())
                .style(Style::default().fg(theme::dim_metadata())),
            Rect::new(area.x + 2, area.y + 2, area.width.saturating_sub(4), 1),
        );
    }

    let columns = item_columns(area);
    if area.height > 5 {
        render_item_column_headers(frame, area, columns);
    }

    let available = area.height.saturating_sub(ITEMS_START_ROW) as usize;
    if available == 0 {
        return None;
    }
    let summary_height = if total_items <= 8 && available >= total_items + 6 {
        5
    } else {
        0
    };
    let viewport_rows = available.saturating_sub(summary_height).max(1);
    let (start, end) = visible_window(total_items, selected_index, viewport_rows);

    for (visible_index, idx) in (start..end).enumerate() {
        let local_y = ITEMS_START_ROW + visible_index as u16;
        let row_area = Rect::new(
            area.x + 1,
            area.y + local_y,
            area.width.saturating_sub(2),
            1,
        );
        let row = settings_row(app, idx);
        let matched =
            search_row_matches(app.settings_state.section, idx, &app.settings_state.query);
        render_setting_row(
            frame,
            row_area,
            area,
            columns,
            &row,
            idx == selected_index,
            matched,
        );
    }

    if total_items > viewport_rows && area.height > 2 {
        let position = format!(
            "{}/{} · rows {}–{}",
            selected_index + 1,
            total_items,
            start + 1,
            end
        );
        frame.render_widget(
            Paragraph::new(position)
                .style(Style::default().fg(theme::dim_metadata()))
                .alignment(Alignment::Right),
            Rect::new(
                area.x + area.width / 2,
                area.y + 2,
                area.width.saturating_sub(area.width / 2 + 2),
                1,
            ),
        );
    }

    if summary_height > 0 {
        let summary_y = area.y + ITEMS_START_ROW + (end - start) as u16 + 2;
        if summary_y + 3 <= area.y + area.height {
            let selected_row = settings_row(app, selected_index);
            render_selected_summary(
                frame,
                Rect::new(
                    area.x + 2,
                    summary_y,
                    area.width.saturating_sub(4),
                    if has_context { 3 } else { 4 },
                ),
                &selected_row,
            );
        }
    }

    let active_idx = app
        .settings_state
        .active_dropdown_item
        .unwrap_or(selected_index);
    if app.settings_state.section == SettingsSection::ModelRoles
        && app.settings_state.model_dropdown.open
        && (start..end).contains(&active_idx)
    {
        let row_y = area.y + ITEMS_START_ROW + (active_idx - start) as u16;
        Some(Rect::new(
            area.x + 1,
            row_y,
            area.width.saturating_sub(2),
            1,
        ))
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy)]
struct ItemColumns {
    label_x: u16,
    label_width: u16,
    value_x: u16,
    value_width: u16,
    action_x: u16,
    action_width: u16,
}

fn item_columns(area: Rect) -> ItemColumns {
    let label_x = area.x + 4;
    let usable = area.width.saturating_sub(6);
    let value_width = (area.width / 4).clamp(10, 22);
    let label_width = usable.saturating_sub(value_width + 1 + 22);
    let value_x = label_x + label_width;
    let action_x = value_x + value_width + 1;
    let right = area.x + area.width.saturating_sub(2);
    ItemColumns {
        label_x,
        label_width,
        value_x,
        value_width,
        action_x,
        action_width: right.saturating_sub(action_x),
    }
}

fn render_item_column_headers(frame: &mut Frame, area: Rect, columns: ItemColumns) {
    let style = Style::default()
        .fg(theme::dim_metadata())
        .add_modifier(Modifier::BOLD);
    frame.render_widget(
        Paragraph::new("SETTING").style(style),
        Rect::new(columns.label_x, area.y + 4, columns.label_width, 1),
    );
    frame.render_widget(
        Paragraph::new("VALUE").style(style),
        Rect::new(columns.value_x, area.y + 4, columns.value_width, 1),
    );
    frame.render_widget(
        Paragraph::new("ACTION").style(style),
        Rect::new(columns.action_x, area.y + 4, columns.action_width, 1),
    );
    frame.render_widget(
        Paragraph::new("─".repeat(area.width.saturating_sub(4) as usize))
            .style(Style::default().fg(theme::browser_decorative_separator())),
        Rect::new(area.x + 2, area.y + 5, area.width.saturating_sub(4), 1),
    );
}

fn render_setting_row(
    frame: &mut Frame,
    row_area: Rect,
    panel_area: Rect,
    columns: ItemColumns,
    row: &SettingsRow,
    selected: bool,
    matched: bool,
) {
    if selected {
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::selected_row_bg())),
            row_area,
        );
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::active_row_rail())),
            Rect::new(row_area.x, row_area.y, 1, 1),
        );
    } else if matched {
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::operations_deck_cell_bg())),
            row_area,
        );
    }

    let marker_style = Style::default().fg(if selected || matched {
        theme::active_row_rail()
    } else {
        theme::browser_decorative_separator()
    });
    frame.render_widget(
        Paragraph::new(if selected {
            "▌"
        } else if matched {
            "•"
        } else {
            " "
        })
        .style(marker_style),
        Rect::new(row_area.x + 1, row_area.y, 1, 1),
    );

    let label_style = if matched {
        Style::default()
            .fg(theme::overlay_hint())
            .add_modifier(Modifier::BOLD)
    } else if selected {
        Style::default()
            .fg(theme::text())
            .add_modifier(Modifier::BOLD)
    } else if row.empty {
        Style::default().fg(theme::dim_metadata())
    } else {
        Style::default().fg(theme::subtext1())
    };
    frame.render_widget(
        Paragraph::new(row.label.clone()).style(label_style),
        Rect::new(
            columns.label_x,
            row_area.y,
            columns.label_width.min(panel_area.width),
            1,
        ),
    );

    let value_style = Style::default()
        .fg(value_tone_color(row.tone))
        .add_modifier(if row.tone == ValueTone::Positive || row.destructive {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });
    frame.render_widget(
        Paragraph::new(row.value.clone()).style(value_style),
        Rect::new(columns.value_x, row_area.y, columns.value_width, 1),
    );

    let action_style = if row.destructive {
        Style::default()
            .fg(theme::error_status())
            .add_modifier(Modifier::BOLD)
    } else if selected {
        Style::default()
            .fg(theme::text())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::dim_metadata())
    };
    let action = if selected {
        row.selected_action.as_str()
    } else {
        row.short_action.as_str()
    };
    frame.render_widget(
        Paragraph::new(action).style(action_style),
        Rect::new(columns.action_x, row_area.y, columns.action_width, 1),
    );
}

fn render_selected_summary(frame: &mut Frame, area: Rect, row: &SettingsRow) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::operations_deck_cell_bg())),
        area,
    );
    frame.render_widget(
        Paragraph::new(format!("SELECTED · {}", row.label)).style(
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::BOLD),
        ),
        Rect::new(area.x + 1, area.y, area.width.saturating_sub(2), 1),
    );
    if area.height > 1 {
        frame.render_widget(
            Paragraph::new(row.description.clone()).style(Style::default().fg(theme::subtext1())),
            Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), 1),
        );
    }
    if area.height > 2 {
        frame.render_widget(
            Paragraph::new(row.selected_action.clone()).style(
                Style::default()
                    .fg(if row.destructive {
                        theme::error_status()
                    } else {
                        theme::green()
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Rect::new(area.x + 1, area.y + 2, area.width.saturating_sub(2), 1),
        );
    }
}

fn render_context_panel(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    selected_index: usize,
    row: &SettingsRow,
) {
    render_panel_surface(frame, area);
    render_panel_rule(frame, area, false);
    render_header_pair(frame, area, "CONTEXT", "DERIVED", false);

    if area.height <= 3 {
        return;
    }
    render_text(
        frame,
        area,
        3,
        row.label.to_ascii_uppercase(),
        Style::default()
            .fg(theme::active_row_rail())
            .add_modifier(Modifier::BOLD),
    );

    if area.height > 8 {
        let action_area = Rect::new(area.x + 2, area.y + 5, area.width.saturating_sub(4), 4);
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::operations_deck_cell_bg())),
            action_area,
        );
        frame.render_widget(
            Paragraph::new("ENTER RESULT").style(
                Style::default()
                    .fg(theme::text())
                    .add_modifier(Modifier::BOLD),
            ),
            Rect::new(
                action_area.x + 1,
                action_area.y,
                action_area.width.saturating_sub(2),
                1,
            ),
        );
        frame.render_widget(
            Paragraph::new(row.selected_action.clone()).style(
                Style::default()
                    .fg(if row.destructive {
                        theme::error_status()
                    } else {
                        theme::green()
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Rect::new(
                action_area.x + 1,
                action_area.y + 1,
                action_area.width.saturating_sub(2),
                1,
            ),
        );
    }

    if area.height > 13 {
        render_text(frame, area, 10, "DESCRIPTION", section_label_style());
        frame.render_widget(
            Paragraph::new(row.description.clone())
                .style(Style::default().fg(theme::subtext1()))
                .wrap(Wrap { trim: true }),
            Rect::new(area.x + 2, area.y + 11, area.width.saturating_sub(4), 3),
        );
    }

    if area.height > 18 {
        render_text(frame, area, 16, "CONTRACT", section_label_style());
        let contract = settings_contract(app, app.settings_state.section, selected_index);
        let mut contract_row = 17;
        if let Some(default_value) = contract.default_value {
            render_key_value(
                frame,
                area,
                contract_row,
                "Default",
                default_value,
                ValueTone::Neutral,
            );
            contract_row += 1;
        }
        for (key, value, tone) in [
            ("Scope", contract.scope.as_str(), ValueTone::Neutral),
            ("Owner", contract.owner.as_str(), ValueTone::Neutral),
            (
                "Persistence",
                contract.persistence.as_str(),
                ValueTone::Neutral,
            ),
            (
                "Apply",
                contract.apply.as_str(),
                if contract.apply == "immediately" {
                    ValueTone::Positive
                } else {
                    ValueTone::Neutral
                },
            ),
            (
                "Restart",
                contract.restart.as_str(),
                if contract.restart == "required" {
                    ValueTone::Warning
                } else {
                    ValueTone::Neutral
                },
            ),
        ] {
            if contract_row >= area.height.saturating_sub(2) {
                break;
            }
            render_key_value(frame, area, contract_row, key, value, tone);
            contract_row += 1;
        }
    }

    if area.height > 33
        && app.settings_state.section == SettingsSection::InputPrompts
        && selected_index == 0
    {
        render_text(
            frame,
            area,
            31,
            "TERMINAL NOTE",
            Style::default()
                .fg(theme::warning_status())
                .add_modifier(Modifier::BOLD),
        );
        frame.render_widget(
            Paragraph::new(
                "Toggle OFF when the terminal cannot deliver Shift+Enter as a distinct key.",
            )
            .style(Style::default().fg(theme::subtext1()))
            .wrap(Wrap { trim: true }),
            Rect::new(area.x + 2, area.y + 32, area.width.saturating_sub(4), 3),
        );
    }

    render_panel_footer(frame, area, "Context has no focus or hidden actions.");
}

fn render_impact_panel(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    selected_index: usize,
    row: &SettingsRow,
) {
    render_panel_surface(frame, area);
    render_panel_rule(frame, area, false);
    render_header_pair(frame, area, "LIVE IMPACT", "DERIVED", false);

    if app.settings_state.section == SettingsSection::InputPrompts && selected_index == 0 {
        render_text(frame, area, 3, "INPUT CONTRACT", section_label_style());
        let card = Rect::new(area.x + 2, area.y + 5, area.width.saturating_sub(4), 7);
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::operations_deck_cell_bg())),
            card,
        );
        render_impact_pair(frame, card, 1, "Enter", "SUBMIT", ValueTone::Positive);
        render_impact_pair(frame, card, 3, "Shift+Enter", "NEWLINE", ValueTone::Neutral);
        render_text(
            frame,
            card,
            5,
            "Current setting: ON",
            Style::default().fg(theme::dim_metadata()),
        );

        render_text(frame, area, 14, "AFFECTED SURFACES", section_label_style());
        render_text(
            frame,
            area,
            16,
            "• session input bar",
            Style::default().fg(theme::subtext1()),
        );
        render_text(
            frame,
            area,
            17,
            "• multiline forms",
            Style::default().fg(theme::subtext1()),
        );
        render_text(
            frame,
            area,
            18,
            "• prompt editors",
            Style::default().fg(theme::subtext1()),
        );

        render_text(frame, area, 22, "DEPENDENCY", section_label_style());
        frame.render_widget(
            Paragraph::new("Terminal must deliver Shift+Enter distinctly.")
                .style(Style::default().fg(theme::subtext1()))
                .wrap(Wrap { trim: true }),
            Rect::new(area.x + 2, area.y + 24, area.width.saturating_sub(4), 3),
        );
        render_text(
            frame,
            area,
            28,
            "If not, toggle OFF.",
            Style::default().fg(theme::warning_status()),
        );

        render_text(frame, area, 32, "PROVENANCE", section_label_style());
        render_text(
            frame,
            area,
            34,
            "Owner: UserSettings",
            Style::default().fg(theme::subtext1()),
        );
        render_text(
            frame,
            area,
            35,
            "Source: state.json",
            Style::default().fg(theme::subtext1()),
        );
        render_text(
            frame,
            area,
            36,
            "Applied: immediate",
            Style::default().fg(theme::green()),
        );
    } else {
        render_text(
            frame,
            area,
            3,
            row.label.to_ascii_uppercase(),
            section_label_style(),
        );
        frame.render_widget(
            Paragraph::new(row.description.clone())
                .style(Style::default().fg(theme::subtext1()))
                .wrap(Wrap { trim: true }),
            Rect::new(area.x + 2, area.y + 5, area.width.saturating_sub(4), 4),
        );
        render_text(frame, area, 11, "CURRENT", section_label_style());
        render_text(
            frame,
            area,
            13,
            if row.value.is_empty() {
                "(no value)"
            } else {
                row.value.as_str()
            },
            Style::default().fg(value_tone_color(row.tone)),
        );
        render_text(frame, area, 17, "PRIMARY ACTION", section_label_style());
        render_text(
            frame,
            area,
            19,
            row.selected_action.as_str(),
            Style::default().fg(if row.destructive {
                theme::error_status()
            } else {
                theme::text()
            }),
        );
    }

    render_panel_footer(frame, area, "Suppress when no useful impact exists.");
}

fn render_impact_pair(
    frame: &mut Frame,
    area: Rect,
    local_y: u16,
    key: &str,
    value: &str,
    tone: ValueTone,
) {
    if local_y >= area.height {
        return;
    }
    let key_width = (area.width / 2).max(1);
    frame.render_widget(
        Paragraph::new(key).style(Style::default().fg(theme::text())),
        Rect::new(area.x + 1, area.y + local_y, key_width.saturating_sub(1), 1),
    );
    frame.render_widget(
        Paragraph::new(value).style(
            Style::default()
                .fg(value_tone_color(tone))
                .add_modifier(Modifier::BOLD),
        ),
        Rect::new(
            area.x + key_width,
            area.y + local_y,
            area.width.saturating_sub(key_width + 1),
            1,
        ),
    );
}

fn render_panel_surface(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::glass_panel_bg())),
        area,
    );
}

fn render_panel_rule(frame: &mut Frame, area: Rect, focused: bool) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new("─".repeat(area.width as usize)).style(Style::default().fg(if focused {
            theme::focused_border()
        } else {
            theme::browser_decorative_separator()
        })),
        Rect::new(area.x, area.y, area.width, 1),
    );
}

fn render_header_pair(
    frame: &mut Frame,
    area: Rect,
    left: impl Into<String>,
    right: impl Into<String>,
    focused: bool,
) {
    if area.height < 2 || area.width < 4 {
        return;
    }
    let header = Rect::new(area.x + 2, area.y + 1, area.width.saturating_sub(4), 1);
    frame.render_widget(
        Paragraph::new(left.into()).style(
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::BOLD),
        ),
        header,
    );
    frame.render_widget(
        Paragraph::new(right.into())
            .style(
                Style::default()
                    .fg(if focused {
                        theme::focused_border()
                    } else {
                        theme::dim_metadata()
                    })
                    .add_modifier(if focused {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            )
            .alignment(Alignment::Right),
        header,
    );
}

fn render_text(frame: &mut Frame, area: Rect, local_y: u16, text: impl Into<String>, style: Style) {
    if local_y >= area.height || area.width <= 4 {
        return;
    }
    frame.render_widget(
        Paragraph::new(text.into()).style(style),
        Rect::new(
            area.x + 2,
            area.y + local_y,
            area.width.saturating_sub(4),
            1,
        ),
    );
}

fn render_key_value(
    frame: &mut Frame,
    area: Rect,
    local_y: u16,
    key: &str,
    value: &str,
    tone: ValueTone,
) {
    if local_y >= area.height || area.width <= 6 {
        return;
    }
    let key_width = (area.width / 3).max(8);
    frame.render_widget(
        Paragraph::new(key).style(Style::default().fg(theme::dim_metadata())),
        Rect::new(area.x + 2, area.y + local_y, key_width.saturating_sub(2), 1),
    );
    frame.render_widget(
        Paragraph::new(value).style(Style::default().fg(value_tone_color(tone))),
        Rect::new(
            area.x + key_width,
            area.y + local_y,
            area.width.saturating_sub(key_width + 2),
            1,
        ),
    );
}

fn render_panel_footer(frame: &mut Frame, area: Rect, text: &str) {
    if area.height < 4 || area.width <= 4 {
        return;
    }
    let rule_y = area.y + area.height - 3;
    frame.render_widget(
        Paragraph::new("─".repeat(area.width.saturating_sub(4) as usize))
            .style(Style::default().fg(theme::browser_decorative_separator())),
        Rect::new(area.x + 2, rule_y, area.width.saturating_sub(4), 1),
    );
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(theme::overlay_hint())),
        Rect::new(area.x + 2, rule_y + 1, area.width.saturating_sub(4), 1),
    );
}

fn inset_horiz(area: Rect, amount: u16) -> Rect {
    Rect::new(
        area.x + amount.min(area.width),
        area.y,
        area.width.saturating_sub(amount * 2),
        area.height,
    )
}

fn section_label_style() -> Style {
    Style::default()
        .fg(theme::dim_metadata())
        .add_modifier(Modifier::BOLD)
}

fn value_tone_color(tone: ValueTone) -> ratatui::style::Color {
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
        section => item_count(section, &app.settings).max(1),
    }
}

/// Render a row backed by a flat `app.daemon_features` entry, using the
/// registry spec for label/description (N-002: per-row summaries instead of
/// one generic string) and the entry for the live value.
/// A daemon field's Epic M design D.5 restart badge: a `↻` value suffix plus
/// `restart` short-action, shown only for `DaemonRestart`-classed rows (the
/// class that never applies live). Other non-`Live` classes (next-spawn,
/// partial-live, live-off-restart-on) are distinguishable enough from the
/// row's own summary/detail text and the context panel's Applies/Restart
/// lines; only `DaemonRestart` gets the terse badge D.5 specifies.
fn restart_badge(spec: &crate::settings_registry::SettingSpec) -> bool {
    matches!(
        spec.apply,
        SettingApply::Daemon(rsi_common::daemon_config_catalog::ApplyClass::DaemonRestart)
    )
}

fn daemon_feature_row(
    entry: &DaemonFeatureEntry,
    spec: &crate::settings_registry::SettingSpec,
) -> SettingsRow {
    let mut description = spec.summary.to_string();
    if let Some(detail) = spec.detail {
        description.push(' ');
        description.push_str(detail);
    }
    let mut row = match &entry.value {
        DaemonFeatureValue::Bool(value) => bool_row(spec.label.to_string(), *value, description),
        DaemonFeatureValue::Cycle { options, current } => cycle_row(
            spec.label.to_string(),
            options.get(*current).map(String::as_str).unwrap_or("?"),
            description,
        ),
        DaemonFeatureValue::Display(_) if entry.field == "model_control_stop_all" => SettingsRow {
            label: spec.label.to_string(),
            value: "DANGER".to_string(),
            short_action: "stop all".to_string(),
            selected_action: "Enter stop all".to_string(),
            description,
            tone: ValueTone::Destructive,
            destructive: true,
            empty: false,
        },
        DaemonFeatureValue::Display(value) if entry.field == "sandbox_build_cache_dry_run" => {
            SettingsRow {
                label: spec.label.to_string(),
                value: value.clone(),
                short_action: "preview".to_string(),
                selected_action: "Enter dry-run".to_string(),
                description,
                tone: ValueTone::Neutral,
                destructive: false,
                empty: false,
            }
        }
        DaemonFeatureValue::Display(value) if entry.field == "sandbox_build_cache_reclaim_now" => {
            SettingsRow {
                label: spec.label.to_string(),
                value: value.clone(),
                short_action: "reclaim".to_string(),
                selected_action: "Enter reclaim caches".to_string(),
                description,
                tone: ValueTone::Destructive,
                destructive: true,
                empty: false,
            }
        }
        DaemonFeatureValue::Display(_) if entry.field == "source_worktree_settlement" => {
            SettingsRow {
                label: spec.label.to_string(),
                value: "Press Enter".to_string(),
                short_action: "audit".to_string(),
                selected_action: "Enter open audit".to_string(),
                description,
                tone: ValueTone::Destructive,
                destructive: true,
                empty: false,
            }
        }
        DaemonFeatureValue::Display(value) => SettingsRow {
            label: spec.label.to_string(),
            value: value.clone(),
            short_action: "read".to_string(),
            selected_action: "Read-only value".to_string(),
            description,
            tone: if value == "?" {
                ValueTone::Warning
            } else {
                ValueTone::Neutral
            },
            destructive: false,
            empty: false,
        },
    };
    if restart_badge(spec) && !row.empty {
        row.value.push_str(" ↻");
        row.short_action = "restart".to_string();
    }
    row
}

fn settings_row(app: &App, idx: usize) -> SettingsRow {
    let settings = &app.settings;
    let section = app.settings_state.section;
    if DAEMON_FEATURE_SECTIONS.contains(&section) {
        let rows = daemon_feature_rows_for_section(app, section);
        return match rows.get(idx) {
            Some((spec, vec_idx)) => daemon_feature_row(&app.daemon_features[*vec_idx], spec),
            None => empty_row(
                "Daemon configuration loading",
                "R refresh",
                "Waiting for the daemon configuration response.",
            ),
        };
    }
    match section {
        SettingsSection::Screen => match idx {
            0 => bool_row(
                "Text area background",
                settings.text_area_backfill_enabled,
                "Fill message and input surfaces instead of showing the terminal background.",
            ),
            1 => edit_row(
                "Background color",
                if settings.text_area_backfill_hex.is_empty() {
                    "(theme)"
                } else {
                    settings.text_area_backfill_hex.as_str()
                },
                "Edit the optional text-surface background color.",
            ),
            2 => SettingsRow {
                label: "Formulation animation".to_string(),
                value: format!("{} ms ↻", settings.formulation_anim_ms),
                short_action: "next".to_string(),
                selected_action: "Enter next duration".to_string(),
                description: "Cycle the message-formulation grow animation duration.".to_string(),
                tone: ValueTone::Neutral,
                destructive: false,
                empty: false,
            },
            3 => cycle_row(
                "Activity indicator",
                settings.activity_indicator_style.label(),
                "Choose the working indicator shown while a provider is active.",
            ),
            _ => unknown_row(),
        },
        SettingsSection::InputPrompts => match idx {
            0 => bool_row(
                "Submit on Enter",
                settings.submit_on_enter,
                "Plain Enter submits multiline text; Shift+Enter inserts a newline.",
            ),
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
        SettingsSection::ThemeColors => match idx {
            0 => edit_row(
                "Built-in theme",
                crate::ui::theme::theme_display_name(crate::ui::theme::active_theme_index()),
                format!(
                    "Choose one of the {} built-in themes; preview is immediate.",
                    crate::ui::theme::THEME_COUNT
                ),
            ),
            1..=17 => {
                let role = crate::settings_keys::theme_role_for_settings_index(idx)
                    .expect("Theme & Colors role row must map to a role");
                let value = crate::ui::theme::get_theme_role_override(role)
                    .map(|[r, g, b]| format!("#{r:02X}{g:02X}{b:02X}"))
                    .unwrap_or_else(|| "Built-in".to_string());
                SettingsRow {
                    label: role.label().to_string(),
                    value,
                    short_action: "edit".to_string(),
                    selected_action: "Enter edit · Delete reset".to_string(),
                    description: format!(
                        "Semantic role `{}`; live preview, Escape rollback, Enter commit.",
                        role.key()
                    ),
                    tone: ValueTone::Neutral,
                    destructive: false,
                    empty: false,
                }
            }
            18 => edit_row(
                "Legacy message/editor colors",
                "7 overrides",
                "Edit the compatible message-border and editor cursor color slots.",
            ),
            19 => SettingsRow {
                label: "Reset active theme".to_string(),
                value: "Clear semantic overrides".to_string(),
                short_action: "reset".to_string(),
                selected_action: "Enter reset active theme".to_string(),
                description: "Clear all semantic overrides without changing the selected built-in theme or legacy colors."
                    .to_string(),
                tone: ValueTone::Warning,
                destructive: false,
                empty: false,
            },
            _ => unknown_row(),
        },
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
        SettingsSection::ApiProviders => {
            if let Some(entry) = settings.custom_providers.get(idx) {
                let has_key = !entry.api_key.is_empty();
                SettingsRow {
                    label: entry.name.clone(),
                    value: if entry.base_url.is_empty() {
                        if has_key {
                            "key set".to_string()
                        } else {
                            "no key".to_string()
                        }
                    } else {
                        truncate_command(&entry.base_url, 40)
                    },
                    short_action: "edit".to_string(),
                    selected_action: "Enter edit provider".to_string(),
                    description: format!(
                        "OpenAI-compatible provider endpoint; API key is {}.",
                        if has_key {
                            "configured"
                        } else {
                            "not configured"
                        }
                    ),
                    tone: if has_key {
                        ValueTone::Neutral
                    } else {
                        ValueTone::Warning
                    },
                    destructive: false,
                    empty: false,
                }
            } else {
                empty_row(
                    "No API providers configured",
                    "a add provider",
                    "Add an OpenAI-compatible provider connection.",
                )
            }
        }
        SettingsSection::SessionList => {
            if idx == 0 {
                cycle_row(
                    "Navigator preset",
                    settings.navigator_preset.label(),
                    "Cycle Dense / Operations / Cost; applies immediately.",
                )
            } else if let Some(column) = crate::types::NavigatorOptionalColumn::ALL
                .get(idx.saturating_sub(1))
                .copied()
            {
                let enabled = settings
                    .navigator_optional_columns
                    .as_ref()
                    .map_or_else(
                        || crate::ui::navigator_layout::preset_columns(settings.navigator_preset)
                            .contains(&column),
                        |columns| columns.contains(&column),
                    );
                bool_row(
                    column.label(),
                    enabled,
                    "Advanced optional navigator column; applies immediately.",
                )
            } else if let Some(entry) = settings.card_fields.get(idx.saturating_sub(
                crate::settings::NAVIGATOR_SETTINGS_ROW_COUNT,
            )) {
                bool_row(
                    entry.field.label(),
                    entry.enabled,
                    "Show this field on session-list cards.",
                )
            } else {
                empty_row(
                    "No card fields configured",
                    "no action",
                    "The session-list field configuration is empty.",
                )
            }
        }
        SettingsSection::ModelRoles => {
            let (label, value, description) = match idx {
                0 => (
                    "Default model",
                    app.selected_model
                        .as_deref()
                        .unwrap_or("default")
                        .to_string(),
                    "Default provider and model for newly launched sessions.",
                ),
                1 => (
                    "Title model",
                    provider_model_value(
                        settings.title_model_provider,
                        settings.title_model_custom_provider_id,
                        &settings.title_model_local,
                        app,
                    ),
                    "Generate session titles and descriptions.",
                ),
                2 => (
                    "Prompt compiler model",
                    provider_model_value(
                        settings.prompt_processor.provider,
                        settings.prompt_processor.custom_provider_id,
                        &settings.prompt_processor.model,
                        app,
                    ),
                    "Compile and refine prompts before submission.",
                ),
                3 => (
                    "Memory model",
                    provider_model_value(
                        settings.memory_model_fallback_provider,
                        settings.memory_model_fallback_custom_provider_id,
                        &settings.memory_model_fallback,
                        app,
                    ),
                    "Fallback model for observation extraction and summarization.",
                ),
                4 => (
                    "Dream model",
                    provider_model_value(
                        settings.dream_model_provider,
                        settings.dream_model_custom_provider_id,
                        &settings.dream_model,
                        app,
                    ),
                    "Run memory consolidation and deduction.",
                ),
                5 => (
                    "Stall classifier model",
                    provider_model_value(
                        rsi_common::types::SessionProvider::Local,
                        None,
                        DaemonFeatureEntry::display_value(
                            &app.daemon_features,
                            "stall_classifier_model",
                        )
                        .unwrap_or("?"),
                        app,
                    ),
                    "Model the stall classifier calls to judge a stalled session. \
                     The classifier is built when the daemon starts, so a new model \
                     applies after a daemon restart.",
                ),
                _ => return unknown_row(),
            };
            SettingsRow {
                label: label.to_string(),
                value: format!("{value} ▾"),
                short_action: "pick".to_string(),
                selected_action: "Enter open model picker".to_string(),
                description: description.to_string(),
                tone: ValueTone::Neutral,
                destructive: false,
                empty: false,
            }
        }
        SettingsSection::MessageBridges => match idx {
            0 => message_bridge_row("Signal bridge", &settings.message_bridges.signal),
            1 => message_bridge_row("iMessage bridge", &settings.message_bridges.imessage),
            _ => unknown_row(),
        },
        SettingsSection::SystemPrompt => match idx {
            0 => cycle_row(
                "System prompt preset",
                settings.system_prompt_preset.label(),
                "System-prompt preset applied to launches: Default, Concise, Code Only or Caveman.",
            ),
            _ => unknown_row(),
        },
        SettingsSection::ClaudeHooks => {
            if let Some(hook) = app.cached_hook_rows.get(idx) {
                let matcher = hook.matcher.as_deref().unwrap_or("");
                SettingsRow {
                    label: if matcher.is_empty() {
                        hook.event.label()
                    } else {
                        format!("{} · {matcher}", hook.event.label())
                    },
                    value: truncate_command(&hook.command, 40),
                    short_action: "edit".to_string(),
                    selected_action: "Enter edit hook".to_string(),
                    description: "Claude Code hook command stored in ~/.claude/settings.json."
                        .to_string(),
                    tone: ValueTone::Neutral,
                    destructive: false,
                    empty: false,
                }
            } else {
                empty_row(
                    "No hooks configured",
                    "a add hook",
                    "Add a Claude Code hook in ~/.claude/settings.json.",
                )
            }
        }
        SettingsSection::ClaudeSkills => {
            let skills = app.cached_user_skills.as_deref().unwrap_or(&[]);
            if let Some(skill) = skills.get(idx) {
                SettingsRow {
                    label: skill.name.clone(),
                    value: if skill.enabled {
                        "[ON]".to_string()
                    } else {
                        "[OFF]".to_string()
                    },
                    short_action: "preview".to_string(),
                    selected_action: "Enter preview · e toggle".to_string(),
                    description: skill
                        .description
                        .clone()
                        .unwrap_or_else(|| "Claude Code user skill.".to_string()),
                    tone: if skill.enabled {
                        ValueTone::Positive
                    } else {
                        ValueTone::Neutral
                    },
                    destructive: false,
                    empty: false,
                }
            } else {
                empty_row(
                    "No user skills installed",
                    "install via Claude",
                    "Install a user skill with the Claude skills command.",
                )
            }
        }
        SettingsSection::Usage => {
            if let Some(stats_row) = stats_rows(app).get(idx) {
                let (label, value) = stats_row_label_value(app, idx);
                let (short_action, selected_action, tone, destructive) = match stats_row.action {
                    Some(StatsRowAction::EmergencyStopAll) => {
                        ("stop all", "Enter stop all", ValueTone::Destructive, true)
                    }
                    Some(StatsRowAction::CancelInvocation(_)) => (
                        "cancel",
                        "Enter cancel invocation",
                        ValueTone::Warning,
                        true,
                    ),
                    None => ("read", "Read-only value", ValueTone::Neutral, false),
                };
                SettingsRow {
                    label,
                    value: value.clone(),
                    short_action: short_action.to_string(),
                    selected_action: selected_action.to_string(),
                    description: if stats_row.action.is_some() {
                        "Explicit operator action for daemon model-control state.".to_string()
                    } else {
                        "Refreshable daemon usage or model-control telemetry.".to_string()
                    },
                    tone: if value.contains("loading") {
                        ValueTone::Warning
                    } else {
                        tone
                    },
                    destructive,
                    empty: false,
                }
            } else {
                empty_row(
                    "Stats loading",
                    "R refresh",
                    "Waiting for daemon usage and model-control telemetry.",
                )
            }
        }
        SettingsSection::Budgets => {
            if budget_rows(app).get(idx).is_some() {
                let (label, value) = budget_row_label_value(app, idx);
                let empty = label == "(no budget policies)";
                SettingsRow {
                    label: if empty {
                        "No explicit budget policies".to_string()
                    } else {
                        label
                    },
                    value,
                    short_action: if empty { "a add" } else { "edit" }.to_string(),
                    selected_action: if empty {
                        "a add policy".to_string()
                    } else {
                        "Enter edit policy".to_string()
                    },
                    description: if empty {
                        "Hardcoded per-scope defaults still apply until a policy is added."
                            .to_string()
                    } else {
                        "Daemon-owned model budget policy; d deletes the selected policy."
                            .to_string()
                    },
                    tone: if empty {
                        ValueTone::Warning
                    } else {
                        ValueTone::Neutral
                    },
                    destructive: false,
                    empty,
                }
            } else {
                empty_row(
                    "Budgets loading",
                    "R refresh",
                    "Waiting for daemon model-control policies.",
                )
            }
        }
        SettingsSection::ModelControl
        | SettingsSection::RetriesRecovery
        | SettingsSection::StallDetection
        | SettingsSection::MemoryDreaming
        | SettingsSection::Orchestration
        | SettingsSection::CodeIntelligence
        | SettingsSection::ProviderIsolation
        | SettingsSection::SandboxStorage => unreachable!(
            "handled by the DAEMON_FEATURE_SECTIONS early return above"
        ),
    }
}

fn bool_row(label: impl Into<String>, value: bool, description: impl Into<String>) -> SettingsRow {
    SettingsRow {
        label: label.into(),
        value: if value { "[ON]" } else { "[OFF]" }.to_string(),
        short_action: "toggle".to_string(),
        selected_action: format!("Enter toggle → {}", if value { "OFF" } else { "ON" }),
        description: description.into(),
        tone: if value {
            ValueTone::Positive
        } else {
            ValueTone::Neutral
        },
        destructive: false,
        empty: false,
    }
}

fn cycle_row(
    label: impl Into<String>,
    value: impl AsRef<str>,
    description: impl Into<String>,
) -> SettingsRow {
    SettingsRow {
        label: label.into(),
        value: format!("‹ {} ›", value.as_ref()),
        short_action: "next".to_string(),
        selected_action: "Enter next value".to_string(),
        description: description.into(),
        tone: ValueTone::Neutral,
        destructive: false,
        empty: false,
    }
}

fn edit_row(label: impl Into<String>, value: &str, description: impl Into<String>) -> SettingsRow {
    SettingsRow {
        label: label.into(),
        value: value.to_string(),
        short_action: "edit".to_string(),
        selected_action: "Enter open editor".to_string(),
        description: description.into(),
        tone: ValueTone::Neutral,
        destructive: false,
        empty: false,
    }
}

fn empty_row(label: &str, action: &str, description: &str) -> SettingsRow {
    SettingsRow {
        label: label.to_string(),
        value: String::new(),
        short_action: action.to_string(),
        selected_action: action.to_string(),
        description: description.to_string(),
        tone: ValueTone::Muted,
        destructive: false,
        empty: true,
    }
}

fn unknown_row() -> SettingsRow {
    SettingsRow {
        label: "Unknown setting".to_string(),
        value: "?".to_string(),
        short_action: "no action".to_string(),
        selected_action: "No action available".to_string(),
        description: "No renderer metadata is available for this setting.".to_string(),
        tone: ValueTone::Warning,
        destructive: false,
        empty: false,
    }
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
    SettingsRow {
        label: label.to_string(),
        value: if settings.enabled {
            if target.is_empty() {
                "[ON]".to_string()
            } else {
                format!("[ON] {target}")
            }
        } else {
            "[OFF]".to_string()
        },
        short_action: "edit".to_string(),
        selected_action: "Enter edit connection".to_string(),
        description: format!("{label} connection, account and sender allowlist."),
        tone: if settings.enabled {
            ValueTone::Positive
        } else {
            ValueTone::Neutral
        },
        destructive: false,
        empty: false,
    }
}

fn provider_model_value(
    provider: rsi_common::types::SessionProvider,
    custom_provider_id: Option<uuid::Uuid>,
    model: &str,
    app: &App,
) -> String {
    if let Some(id) = custom_provider_id
        && let Some(entry) = app
            .settings
            .custom_providers
            .iter()
            .find(|entry| entry.id == id)
    {
        return format!("{} / {}", entry.name, model);
    }
    format!("{} / {}", App::provider_label(provider), model)
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
            19 => section_specs.get(3).copied(),
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
        | SettingsSection::Usage => section_specs.first().copied(),
        _ => section_specs
            .get(idx)
            .copied()
            .or_else(|| section_specs.first().copied()),
    }
}

/// Derive the context panel's Contract block from the registry row backing
/// this position, instead of a hand-maintained per-category table (Epic M
/// design D.2: "becomes per-row and is derived from owner and apply").
fn settings_contract(app: &App, section: SettingsSection, idx: usize) -> SettingsContract {
    let Some(spec) = spec_for_row(app, section, idx) else {
        return SettingsContract {
            default_value: None,
            scope: section.label().to_string(),
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
        SettingApply::Immediate => "not required".to_string(),
        SettingApply::Daemon(class) => match class {
            rsi_common::daemon_config_catalog::ApplyClass::DaemonRestart => "required".to_string(),
            rsi_common::daemon_config_catalog::ApplyClass::LiveOffRestartOn => {
                "on enable only".to_string()
            }
            rsi_common::daemon_config_catalog::ApplyClass::PartialLive => {
                "resumed sessions only".to_string()
            }
            rsi_common::daemon_config_catalog::ApplyClass::NextSpawn
            | rsi_common::daemon_config_catalog::ApplyClass::Live
            | rsi_common::daemon_config_catalog::ApplyClass::NotApplied => {
                "not required".to_string()
            }
        },
    };
    SettingsContract {
        default_value: None,
        scope: section.label().to_string(),
        owner: spec.owner.label(),
        persistence,
        apply: spec.apply.label().to_string(),
        restart,
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

    fn rendered_settings(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
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

    fn buffer_row(buffer: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        (0..width).map(|x| buffer[(x, y)].symbol()).collect()
    }

    #[test]
    fn section_rail_keeps_late_selected_section_visible_at_24_rows() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.settings_state.focus = SettingsFocus::Categories;
        app.settings_state.section = SettingsSection::MemoryDreaming;
        let buffer = rendered_settings(&app, 120, 24);
        let visible: String = (0..24).map(|y| buffer_row(&buffer, y, 27)).collect();
        assert!(visible.contains("▶ Memory & Dreaming"), "{visible}");
    }

    #[test]
    fn search_query_and_matched_setting_are_visible() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.settings_state.focus = SettingsFocus::Items;
        app.settings_state.section = SettingsSection::RetriesRecovery;
        app.settings_state.query = "retry max backoff".to_string();
        app.settings_state.query_active = true;
        let buffer = rendered_settings(&app, 120, 24);
        assert!(buffer_row(&buffer, 0, 120).contains("/retry max backoff"));
        let Some(matched_y) =
            (0..24).find(|y| buffer_row(&buffer, *y, 120).contains("Retry max backoff"))
        else {
            panic!("matching row rendered");
        };
        assert_eq!(buffer[(32, matched_y)].symbol(), "•");
        assert_eq!(buffer[(34, matched_y)].fg, theme::overlay_hint());
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
            for (idx, expected_label) in registry_labels.iter().enumerate() {
                let row = settings_row(&test_app, idx);
                assert_eq!(
                    &row.label, expected_label,
                    "{section:?} row {idx} comes from the registry"
                );
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
        assert!(
            !row.description
                .contains("Daemon-owned runtime configuration persisted in SQLite"),
            "no more shared generic daemon description (N-002)"
        );
    }

    /// (c) acceptance: `restart_badge_rendered_for_daemon_restart_rows`
    /// (Epic M design D.5). `stall_detection_enabled` is classed
    /// `DaemonRestart`; its row shows a `↻` value suffix and a `restart`
    /// short-action, matching the context panel's own Restart: required line.
    #[test]
    fn restart_badge_rendered_for_daemon_restart_rows() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.settings_state.section = SettingsSection::StallDetection;
        let row = settings_row(&app, 0); // StallDetection (stall_detection_enabled)
        assert_eq!(row.label, "Stall detection");
        assert!(
            row.value.ends_with(" ↻"),
            "value carries the restart badge: {}",
            row.value
        );
        assert_eq!(row.short_action, "restart");

        // A Live-classed row in the same section gets no badge.
        let live_row = settings_row(&app, 6); // ClassifierConfidenceFloor (Live)
        assert_eq!(live_row.label, "Classifier confidence floor");
        assert!(!live_row.value.ends_with(" ↻"));
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
        assert_eq!(row.value, "[OFF]");
        assert_eq!(row.selected_action, "Enter toggle → ON");
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
        assert_eq!(row.value, "[ON]");
        assert_eq!(row.selected_action, "Enter toggle → OFF");
    }

    #[test]
    fn layout_120x40_uses_exact_dual_geometry() {
        let layout = settings_workspace_layout(Rect::new(0, 2, 120, 37), SettingsFocus::Items);
        assert_eq!(layout.categories, Some(Rect::new(2, 2, 27, 37)));
        assert_eq!(layout.items, Some(Rect::new(30, 2, 88, 37)));
        assert_eq!(layout.context, None);
        assert_eq!(layout.impact, None);
    }

    #[test]
    fn layout_200x58_uses_exact_context_geometry() {
        let layout = settings_workspace_layout(Rect::new(0, 2, 200, 55), SettingsFocus::Items);
        assert_eq!(layout.categories, Some(Rect::new(4, 2, 32, 55)));
        assert_eq!(layout.items, Some(Rect::new(37, 2, 94, 55)));
        assert_eq!(layout.context, Some(Rect::new(132, 2, 64, 55)));
        assert_eq!(layout.impact, None);
    }

    #[test]
    fn layout_240x70_assigns_residual_width_to_impact() {
        let layout = settings_workspace_layout(Rect::new(0, 2, 240, 67), SettingsFocus::Items);
        assert_eq!(layout.categories, Some(Rect::new(4, 2, 32, 67)));
        assert_eq!(layout.items, Some(Rect::new(37, 2, 94, 67)));
        assert_eq!(layout.context, Some(Rect::new(132, 2, 64, 67)));
        assert_eq!(layout.impact, Some(Rect::new(197, 2, 39, 67)));
    }

    #[test]
    fn wide_item_columns_fit_full_sandbox_value_and_actions() {
        let columns = item_columns(Rect::new(37, 2, 94, 67));

        assert_eq!(columns.label_width, 43);
        assert_eq!(columns.value_width, 22);
        assert_eq!(columns.action_width, 22);
    }

    #[test]
    fn single_panel_fallback_follows_keyboard_focus() {
        let area = Rect::new(0, 2, 80, 30);
        let categories = settings_workspace_layout(area, SettingsFocus::Categories);
        assert_eq!(categories.categories, Some(area));
        assert_eq!(categories.items, None);

        let items = settings_workspace_layout(area, SettingsFocus::Items);
        assert_eq!(items.categories, None);
        assert_eq!(items.items, Some(area));
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
    fn boolean_vocabulary_has_one_state_and_exact_result() {
        let row = bool_row("Submit on Enter", true, "description");
        assert_eq!(row.value, "[ON]");
        assert_eq!(row.selected_action, "Enter toggle → OFF");
        assert!(!row.label.contains("[x]"));
    }
}
