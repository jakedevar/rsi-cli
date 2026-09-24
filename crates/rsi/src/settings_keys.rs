//! Key handling for the settings pane.

use crossterm::event::{KeyCode, KeyEvent};

use crate::app::App;
use crate::claude_config;
use crate::modalkit_types::LcAction;
use crate::model_control_stats::stats_row_action;
use crate::overlay::{
    open_budget_policy_form, open_hook_form, open_message_bridge_form, open_provider_form,
    open_skill_preview, open_text_area_bg_editor,
};
use crate::settings::{
    DaemonFeatureEntry, FORMULATION_ANIM_DURATIONS, MessageBridgeKind, UserSettings, cycle_u64,
};
#[cfg(test)]
use crate::settings::{DaemonFeatureValue, OBSERVATION_THRESHOLDS};
use crate::settings_registry::{SETTINGS, SettingId, SettingOwner, SettingSpec, SettingsSection};
use crate::types::{HookFormEditingTarget, Pane, SettingsFocus, SettingsState};
use rsi_common::types::SessionProvider;

/// Sections whose rows are entirely fanned out of the flat
/// `app.daemon_features` list (Epic M design D.1's daemon-owned buckets).
///
pub const DAEMON_FEATURE_SECTIONS: &[SettingsSection] = &[
    SettingsSection::ModelControl,
    SettingsSection::RetriesRecovery,
    SettingsSection::StallDetection,
    SettingsSection::MemoryDreaming,
    SettingsSection::Orchestration,
    SettingsSection::CodeIntelligence,
    SettingsSection::ProviderIsolation,
    SettingsSection::SandboxStorage,
];

/// The `app.daemon_features` field name for a registry row, for every row
/// that is represented in the flat daemon-features list — whether the
/// registry classes its owner `Daemon` (persisted runtime config) or
/// `DaemonState` (model control / sandbox storage, reached through the same
/// generic toggle path even though it is a different RPC family).
fn daemon_feature_field_for(id: SettingId) -> Option<&'static str> {
    match id {
        SettingId::ModelControlMode => Some("model_control_mode"),
        SettingId::EmergencyStop => Some("model_control_stop_all"),
        SettingId::SandboxStorageStatus => Some("sandbox_storage_status"),
        SettingId::PreviewReclaim => Some("sandbox_build_cache_dry_run"),
        SettingId::ReclaimNow => Some("sandbox_build_cache_reclaim_now"),
        SettingId::SourceWorktreeSettlement => Some("source_worktree_settlement"),
        // The stall classifier model is edited through the Model Roles
        // dropdown (`agent_actors_dropdown_config` idx 5), never through the
        // generic daemon-feature row path, even though its value is cached
        // in `app.daemon_features` for `DaemonFeatureEntry::display_value`.
        SettingId::ClassifierModel => None,
        _ => SETTINGS.iter().find(|spec| spec.id == id).and_then(|spec| {
            if let SettingOwner::Daemon(field) = spec.owner {
                Some(field)
            } else {
                None
            }
        }),
    }
}

/// The `MemoryDreaming` section's daemon-owned row count.
#[must_use]
pub fn memory_dreaming_row_count() -> usize {
    SETTINGS
        .iter()
        .filter(|spec| spec.section == SettingsSection::MemoryDreaming)
        .count()
}

/// Every registry row for `section` that is backed by a flat
/// `app.daemon_features` entry, paired with that entry's index, in registry
/// (`SettingId::index()`) order.
#[must_use]
pub fn daemon_feature_rows_for_section(
    app: &App,
    section: SettingsSection,
) -> Vec<(&'static SettingSpec, usize)> {
    let mut rows: Vec<(&'static SettingSpec, usize)> = SETTINGS
        .iter()
        .filter(|spec| spec.section == section)
        .filter_map(|spec| {
            let field = daemon_feature_field_for(spec.id)?;
            let vec_idx = app.daemon_features.iter().position(|e| e.field == field)?;
            Some((spec, vec_idx))
        })
        .collect();
    rows.sort_by_key(|(spec, _)| spec.id.index());
    rows
}

/// Resolve a within-section row index to its `app.daemon_features` index.
///
/// Covers every section whose rows are daemon-features-backed.
#[must_use]
pub fn daemon_feature_vec_index(app: &App, section: SettingsSection, idx: usize) -> Option<usize> {
    daemon_feature_rows_for_section(app, section)
        .get(idx)
        .map(|(_, vec_idx)| *vec_idx)
}

/// Match count for `/` search within one section (Epic M design D.3).
///
/// The number of the section's rows whose label, summary, detail or
/// keywords match every whitespace-separated term of `query`
/// (case-insensitive AND, mirroring `overlay/keybindings_help.rs`'s
/// help-search semantics).
#[must_use]
pub fn search_match_count_in_section(section: SettingsSection, query: &str) -> usize {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(str::to_lowercase)
        .filter(|term| !term.is_empty())
        .collect();
    if terms.is_empty() {
        return 0;
    }
    SETTINGS
        .iter()
        .filter(|spec| spec.section == section)
        .filter(|spec| spec_matches_terms(spec, &terms))
        .count()
}

/// Whether a rendered row belongs to a matching registry setting. Expanded
/// rows share their parent setting's search terms.
pub fn search_row_matches(section: SettingsSection, ui_row: usize, query: &str) -> bool {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(str::to_lowercase)
        .filter(|term| !term.is_empty())
        .collect();
    if terms.is_empty() {
        return false;
    }
    SETTINGS
        .iter()
        .filter(|spec| spec.section == section)
        .nth(spec_position_for_ui_row(section, ui_row))
        .is_some_and(|spec| spec_matches_terms(spec, &terms))
}

fn spec_matches_terms(spec: &SettingSpec, terms: &[String]) -> bool {
    let mut haystack = format!(
        "{} {} {}",
        spec.label,
        spec.section.label(),
        spec.section.group().label()
    )
    .to_lowercase();
    haystack.push(' ');
    haystack.push_str(&spec.summary.to_lowercase());
    if let Some(detail) = spec.detail {
        haystack.push(' ');
        haystack.push_str(&detail.to_lowercase());
    }
    for keyword in spec.keywords {
        haystack.push(' ');
        haystack.push_str(&keyword.to_lowercase());
    }
    for field in spec.owner.daemon_fields() {
        haystack.push(' ');
        haystack.push_str(&field.to_lowercase());
    }
    terms.iter().all(|term| haystack.contains(term.as_str()))
}

/// Dynamic-list settings can occupy more than one physical UI row. Search
/// and highlighting map every expanded row back to its registry setting.
///
/// Every other section's registry-spec position already equals its UI row
/// index. Translate a UI row index into the (possibly collapsed)
/// registry-spec position search operates over.
const fn spec_position_for_ui_row(section: SettingsSection, ui_row: usize) -> usize {
    match section {
        SettingsSection::ThemeColors => match ui_row {
            0 => 0,
            1..=17 => 1,
            18 => 2,
            _ => 3,
        },
        SettingsSection::SessionList => {
            let optional_columns = crate::types::NavigatorOptionalColumn::ALL.len();
            if ui_row == 0 {
                0
            } else if ui_row <= optional_columns {
                1
            } else {
                2
            }
        }
        SettingsSection::ApiProviders
        | SettingsSection::ClaudeHooks
        | SettingsSection::ClaudeSkills => 0,
        _ => ui_row,
    }
}

/// The inverse of `spec_position_for_ui_row`: the first UI row a matched
/// registry spec occupies, so a search jump lands on a real, selectable row
/// even for a spec whose section expands it into several UI rows.
const fn ui_row_for_spec_position(section: SettingsSection, spec_position: usize) -> usize {
    match section {
        SettingsSection::ThemeColors => match spec_position {
            0 => 0,
            1 => 1,
            2 => 18,
            _ => 19,
        },
        SettingsSection::SessionList => match spec_position {
            0 => 0,
            1 => 1,
            _ => 1 + crate::types::NavigatorOptionalColumn::ALL.len(),
        },
        _ => spec_position,
    }
}

fn search_candidates(query: &str) -> Vec<(usize, usize, SettingsSection)> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(str::to_lowercase)
        .filter(|term| !term.is_empty())
        .collect();
    if terms.is_empty() {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    for (section_pos, section) in SettingsSection::ALL.iter().enumerate() {
        for (row_idx, spec) in SETTINGS
            .iter()
            .filter(|spec| spec.section == *section)
            .enumerate()
        {
            if spec_matches_terms(spec, &terms) {
                candidates.push((section_pos, row_idx, *section));
            }
        }
    }
    candidates
}

/// The first match at or after `(from_section, from_index)` in page order.
///
/// Wraps to the first match overall when nothing qualifies from the cursor
/// to the end. `inclusive` controls whether the row at
/// `(from_section, from_index)` itself counts as a match: `Enter` (jump to
/// the query) wants it included; `n` (next distinct match) wants it
/// excluded, so a repeated `n` on a still-selected multi-row match (e.g. a
/// `ThemeColors` role row) advances instead of re-selecting the same spec.
#[must_use]
pub fn find_next_search_match(
    query: &str,
    from_section: SettingsSection,
    from_index: usize,
    inclusive: bool,
) -> Option<(SettingsSection, usize)> {
    let candidates = search_candidates(query);
    if candidates.is_empty() {
        return None;
    }
    let from_section_pos = SettingsSection::ALL
        .iter()
        .position(|section| *section == from_section)
        .unwrap_or(0);
    let from_spec_position = spec_position_for_ui_row(from_section, from_index);
    let from = (from_section_pos, from_spec_position);
    candidates
        .iter()
        .find(|(section_pos, row_idx, _)| {
            let candidate = (*section_pos, *row_idx);
            if inclusive {
                candidate >= from
            } else {
                candidate > from
            }
        })
        .or_else(|| candidates.first())
        .map(|(_, row_idx, section)| (*section, ui_row_for_spec_position(*section, *row_idx)))
}

/// The previous match before `(from_section, from_index)`, wrapping to the
/// last match when nothing precedes the cursor. `inclusive` mirrors
/// `find_next_search_match`'s parameter (Epic M design D.3's `N`).
#[must_use]
pub fn find_prev_search_match(
    query: &str,
    from_section: SettingsSection,
    from_index: usize,
    inclusive: bool,
) -> Option<(SettingsSection, usize)> {
    let candidates = search_candidates(query);
    if candidates.is_empty() {
        return None;
    }
    let from_section_pos = SettingsSection::ALL
        .iter()
        .position(|section| *section == from_section)
        .unwrap_or(0);
    let from_spec_position = spec_position_for_ui_row(from_section, from_index);
    let from = (from_section_pos, from_spec_position);
    candidates
        .iter()
        .rev()
        .find(|(section_pos, row_idx, _)| {
            let candidate = (*section_pos, *row_idx);
            if inclusive {
                candidate <= from
            } else {
                candidate < from
            }
        })
        .or_else(|| candidates.last())
        .map(|(_, row_idx, section)| (*section, ui_row_for_spec_position(*section, *row_idx)))
}

// --- Hooks helpers ---------------------------------------------------------

/// Open the hook form populated for editing the row at the current selection.
/// If the selection is past the end (empty list), behave like 'a' and open a
/// blank form so the user has an immediate affordance.
fn open_hook_edit_for_selected_index(app: &mut App) {
    let idx = app.settings_state.selected_index;
    let target = {
        let rows = app.cached_claude_settings_rows();
        rows.get(idx).map(|row| HookFormEditingTarget {
            event: row.event.clone(),
            entry_index: row.entry_index,
            hook_index: row.hook_index,
        })
    };
    open_hook_form(app, target);
}

/// Delete the hook row at the current selection, write the file, refresh cache.
fn delete_hook_at_selected_index(app: &mut App) {
    let idx = app.settings_state.selected_index;
    let target = {
        let rows = app.cached_claude_settings_rows();
        rows.get(idx)
            .map(|row| (row.event.clone(), row.entry_index, row.hook_index))
    };
    let (event, entry_idx, hook_idx) = match target {
        Some(t) => t,
        None => return,
    };

    let mut settings = match claude_config::load_user_settings() {
        Ok(s) => s.data,
        Err(e) => {
            app.notify_error(format!("Failed to read ~/.claude/settings.json: {}", e));
            return;
        }
    };

    // Walk down the (event, entry, hook) coordinate, peeling empty parents.
    if let Some(entries) = settings.hooks.get_mut(&event) {
        if let Some(entry) = entries.get_mut(entry_idx) {
            if hook_idx < entry.hooks.len() {
                entry.hooks.remove(hook_idx);
            }
            if entry.hooks.is_empty() {
                entries.remove(entry_idx);
            }
        }
        if entries.is_empty() {
            settings.hooks.remove(&event);
        }
    }

    if let Err(e) = claude_config::save_user_settings(&settings) {
        app.notify_error(format!("Failed to save ~/.claude/settings.json: {}", e));
        return;
    }
    app.invalidate_claude_settings_cache();

    // Clamp selection so the next render lands on a valid row.
    let new_max = app.cached_claude_settings_rows().len().saturating_sub(1);
    if app.settings_state.selected_index > new_max {
        app.settings_state.selected_index = new_max;
    }
    app.notify_success("Deleted hook. Applies to new Claude sessions.");
}

// --- Budgets helpers --------------------------------------------------------

/// Handle Enter/Space in the Budgets category: edit the selected policy, or
/// open a blank add form if the selection is on the synthetic empty-state
/// row (mirrors `open_hook_edit_for_selected_index`'s "behave like 'a' when
/// empty" logic).
fn handle_budgets_enter(app: &mut App) {
    let idx = app.settings_state.selected_index;
    let policy = app
        .cached_model_control_status
        .as_ref()
        .and_then(|status| status.policies.get(idx).cloned());
    match policy {
        Some(p) => open_budget_policy_form(app, Some((idx, &p))),
        None => open_budget_policy_form(app, None),
    }
}

/// Delete the budget policy row at the current selection. Enqueues an
/// `LcAction` (RPC round-trip required) instead of mutating locally — unlike
/// `ApiModels`' 'd' arm, which mutates `app.settings.custom_providers`
/// synchronously because custom providers are LOCAL-only settings. No-op if
/// the selection is on the synthetic "(no budget policies)" row.
fn delete_selected_budget_policy(app: &mut App) {
    let idx = app.settings_state.selected_index;
    let has_real_policy = app
        .cached_model_control_status
        .as_ref()
        .is_some_and(|status| idx < status.policies.len());
    if has_real_policy {
        app.pending_lc_actions
            .push(LcAction::DeleteBudgetPolicy(idx));
    }
}

// --- Skills helpers --------------------------------------------------------

/// Open the read-only SKILL.md viewer for the currently selected skill row.
fn open_skill_preview_for_selected_index(app: &mut App) {
    let idx = app.settings_state.selected_index;
    let name = {
        let skills = app.cached_user_skills();
        skills.get(idx).map(|s| s.name.clone())
    };
    if let Some(name) = name {
        open_skill_preview(app, &name);
    }
}

/// Toggle enabled/disabled state of the selected skill via directory rename.
fn toggle_skill_at_selected_index(app: &mut App) {
    let idx = app.settings_state.selected_index;
    let name = {
        let skills = app.cached_user_skills();
        skills.get(idx).map(|s| s.name.clone())
    };
    let name = match name {
        Some(n) => n,
        None => return,
    };
    match claude_config::toggle_skill(&name) {
        Ok(now_enabled) => {
            app.invalidate_claude_skills_cache();
            let verb = if now_enabled { "Enabled" } else { "Disabled" };
            app.notify_success(format!("{} skill: {}", verb, name));
        }
        Err(e) => {
            app.notify_error(format!("Toggle failed: {}", e));
        }
    }
}

/// Delete the selected skill directory immediately (no confirm — matches
/// ApiModels precedent at the existing `'d'` arm above).
fn delete_skill_at_selected_index(app: &mut App) {
    let idx = app.settings_state.selected_index;
    let name = {
        let skills = app.cached_user_skills();
        skills.get(idx).map(|s| s.name.clone())
    };
    let name = match name {
        Some(n) => n,
        None => return,
    };
    match claude_config::delete_skill(&name) {
        Ok(()) => {
            app.invalidate_claude_skills_cache();
            let new_max = app.cached_user_skills().len().saturating_sub(1);
            if app.settings_state.selected_index > new_max {
                app.settings_state.selected_index = new_max;
            }
            app.notify_success(format!("Deleted skill: {}", name));
        }
        Err(e) => {
            app.notify_error(format!("Delete failed: {}", e));
        }
    }
}

/// Handle a key event while a Settings pane is focused.
/// Returns true if the key was consumed.
pub async fn handle_settings_key_event(app: &mut App, key: KeyEvent) -> bool {
    // An open dropdown owns its local navigation and text-like selection keys.
    // Preserve that precedence before adapting Settings keys through the registry.
    if (app.settings_state.model_dropdown.open || app.settings_state.query_active)
        && handle_settings_key(app, key)
    {
        return true;
    }

    let context = crate::action_registry::ActionContext::from_app(app);
    if crate::action_registry::has_binding_for_key(&context, key) {
        match crate::action_registry::request_for_key(&context, key) {
            crate::action_registry::ActionAvailability::Available(request) => {
                crate::action_handler::dispatch_registered_action(app, request).await;
            }
            crate::action_registry::ActionAvailability::Unavailable { reason } => {
                app.notify(reason);
            }
        }
        return true;
    }

    handle_settings_key(app, key)
}

/// Execute an already-authorized Settings key through the legacy domain handler.
/// Registered raw keys enter through `handle_settings_key_event`; this function is
/// also the ungated executor used by `dispatch_registered_action`.
pub fn handle_settings_key(app: &mut App, key: KeyEvent) -> bool {
    // Model dropdown intercept: when the settings dropdown is open, route keys to it
    if app.settings_state.model_dropdown.open {
        use crate::widget::model_dropdown::{ModelDropdownAction, handle_model_dropdown_key};
        let action = handle_model_dropdown_key(
            &mut app.settings_state.model_dropdown,
            &key,
            &app.settings.custom_providers,
        );
        let consumed = !matches!(action, ModelDropdownAction::Ignored);
        match action {
            ModelDropdownAction::Selected(model_id) => {
                if handle_agent_actors_selection(app, model_id) {
                    app.settings_state.model_dropdown.close();
                    app.settings_state.active_dropdown_item = None;
                }
            }
            ModelDropdownAction::ProviderCycled => {
                if app.settings_state.active_dropdown_item == Some(0) {
                    app.selected_provider = app.settings_state.model_dropdown.provider;
                    app.custom_provider_index =
                        app.settings_state.model_dropdown.custom_provider_index;
                    app.available_models = app.settings_state.model_dropdown.models.clone();
                    app.selected_model = app.available_models.first().map(|(id, _)| id.clone());
                    app.model_discovery_rx = None;
                    app.model_refresh_provider = Some(app.selected_provider);
                    app.needs_model_refresh = true;
                } else {
                    queue_agent_actor_dropdown_refresh(app);
                }
            }
            ModelDropdownAction::Dismissed => {
                app.settings_state.model_dropdown.close();
                app.settings_state.active_dropdown_item = None;
            }
            ModelDropdownAction::Consumed | ModelDropdownAction::Ignored => {}
        }
        if consumed {
            return true;
        }
    }

    // Search-query intercept: while composing a `/` query, every key feeds
    // the query line instead of the section's normal bindings (Epic M
    // design D.3). This must run before the main match below so that a
    // query character that happens to collide with a bound key (`a`, `d`,
    // `R`, Enter, ...) is never stolen by that binding.
    if app.settings_state.query_active {
        return match key.code {
            KeyCode::Char(c) => {
                app.settings_state.query.push(c);
                true
            }
            KeyCode::Backspace => {
                app.settings_state.query.pop();
                true
            }
            KeyCode::Esc => {
                app.settings_state.query.clear();
                app.settings_state.query_active = false;
                true
            }
            KeyCode::Enter => {
                app.settings_state.query_active = false;
                if let Some((section, idx)) = find_next_search_match(
                    &app.settings_state.query,
                    app.settings_state.section,
                    app.settings_state.selected_index,
                    true,
                ) {
                    enter_settings_section(app, section, idx);
                } else if !app.settings_state.query.is_empty() {
                    app.notify(format!(
                        "No settings match \"{}\"",
                        app.settings_state.query
                    ));
                }
                true
            }
            _ => true,
        };
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            close_settings(app);
            true
        }

        KeyCode::Char('h') | KeyCode::Left => {
            app.settings_state.focus = SettingsFocus::Categories;
            true
        }

        KeyCode::Char('l') | KeyCode::Right
            if app.settings_state.focus == SettingsFocus::Categories =>
        {
            app.settings_state.focus = SettingsFocus::Items;
            app.settings_state.selected_index = 0;
            refresh_entered_settings_section(app);
            true
        }
        KeyCode::Enter if app.settings_state.focus == SettingsFocus::Categories => {
            app.settings_state.focus = SettingsFocus::Items;
            app.settings_state.selected_index = 0;
            refresh_entered_settings_section(app);
            true
        }

        KeyCode::Enter | KeyCode::Char(' ') if app.settings_state.focus == SettingsFocus::Items => {
            let section = app.settings_state.section;
            let idx = app.settings_state.selected_index;
            if section == SettingsSection::ThemeColors {
                activate_theme_color_row(app);
            } else if section == SettingsSection::ApiProviders {
                handle_api_models_enter(app);
            } else if section == SettingsSection::MessageBridges {
                handle_message_bridges_enter(app);
            } else if section == SettingsSection::ClaudeHooks {
                open_hook_edit_for_selected_index(app);
            } else if section == SettingsSection::ClaudeSkills {
                open_skill_preview_for_selected_index(app);
            } else if DAEMON_FEATURE_SECTIONS.contains(&section) {
                if let Some(vec_idx) = daemon_feature_vec_index(app, section, idx) {
                    let settlement_row = app
                        .daemon_features
                        .get(vec_idx)
                        .is_some_and(|entry| entry.field == "source_worktree_settlement");
                    app.pending_lc_actions.push(if settlement_row {
                        LcAction::OpenSourceWorktreeSettlement
                    } else {
                        LcAction::ToggleDaemonFeature(vec_idx)
                    });
                }
            } else if section == SettingsSection::Usage {
                if let Some(action) = stats_row_action(app, idx) {
                    app.pending_lc_actions.push(action);
                }
            } else if section == SettingsSection::Budgets {
                handle_budgets_enter(app);
            } else if section == SettingsSection::ModelRoles {
                // All Model Roles items use the model dropdown.
                if app.settings_state.model_dropdown.open {
                    app.settings_state.model_dropdown.close();
                    app.settings_state.active_dropdown_item = None;
                } else {
                    let (provider, custom_provider_index, models, current) =
                        agent_actors_dropdown_config(app, idx);
                    app.settings_state.model_dropdown =
                        crate::types::ModelDropdownState::new(provider, models, current.as_deref());
                    app.settings_state.model_dropdown.custom_provider_index = custom_provider_index;
                    app.settings_state.active_dropdown_item = Some(idx);
                    match idx {
                        0 => app.needs_model_refresh = true,
                        1..=5 => queue_agent_actor_dropdown_refresh(app),
                        _ => {}
                    }
                }
            } else if section == SettingsSection::Screen && idx == 1 {
                // Text-area background hex editor — open overlay instead of toggling.
                open_text_area_bg_editor(app);
            } else {
                let state_snapshot = app.settings_state.clone();
                toggle_setting_on_app(app, &state_snapshot);
            }
            true
        }

        KeyCode::Delete
            if app.settings_state.section == SettingsSection::ThemeColors
                && app.settings_state.focus == SettingsFocus::Items =>
        {
            if let Some(role) = theme_role_for_settings_index(app.settings_state.selected_index) {
                crate::ui::theme::set_theme_role_override(role, None);
                app.notify_success(format!("{} reset to built-in", role.label()));
                app.mark_dirty();
            }
            true
        }

        // ApiModels-specific: 'a' to add new provider
        KeyCode::Char('a')
            if app.settings_state.section == SettingsSection::ApiProviders
                && app.settings_state.focus == SettingsFocus::Items =>
        {
            open_provider_form(app, None);
            true
        }

        // ApiModels-specific: 'd' to delete selected provider
        KeyCode::Char('d')
            if app.settings_state.section == SettingsSection::ApiProviders
                && app.settings_state.focus == SettingsFocus::Items =>
        {
            let idx = app.settings_state.selected_index;
            if idx < app.settings.custom_providers.len() {
                app.settings.custom_providers.remove(idx);
                // Clamp selection
                let new_max = app.settings.custom_providers.len().saturating_sub(1);
                if app.settings_state.selected_index > new_max {
                    app.settings_state.selected_index = new_max;
                }
            }
            true
        }

        // Hooks-specific: 'a' to add a new hook entry
        KeyCode::Char('a')
            if app.settings_state.section == SettingsSection::ClaudeHooks
                && app.settings_state.focus == SettingsFocus::Items =>
        {
            open_hook_form(app, None);
            true
        }

        // Hooks-specific: 'd' to delete selected hook entry
        KeyCode::Char('d')
            if app.settings_state.section == SettingsSection::ClaudeHooks
                && app.settings_state.focus == SettingsFocus::Items =>
        {
            delete_hook_at_selected_index(app);
            true
        }

        // Skills-specific: 'e' to enable/disable selected skill
        KeyCode::Char('e')
            if app.settings_state.section == SettingsSection::ClaudeSkills
                && app.settings_state.focus == SettingsFocus::Items =>
        {
            toggle_skill_at_selected_index(app);
            true
        }

        // Skills-specific: 'd' to delete selected skill (matches ApiModels precedent — no confirm)
        KeyCode::Char('d')
            if app.settings_state.section == SettingsSection::ClaudeSkills
                && app.settings_state.focus == SettingsFocus::Items =>
        {
            delete_skill_at_selected_index(app);
            true
        }

        // Budgets-specific: 'a' to open a blank add-policy form
        KeyCode::Char('a')
            if app.settings_state.section == SettingsSection::Budgets
                && app.settings_state.focus == SettingsFocus::Items =>
        {
            open_budget_policy_form(app, None);
            true
        }

        // Budgets-specific: 'd' to delete the selected policy (RPC round-trip;
        // pushes an LcAction rather than mutating locally — see
        // `delete_selected_budget_policy`)
        KeyCode::Char('d')
            if app.settings_state.section == SettingsSection::Budgets
                && app.settings_state.focus == SettingsFocus::Items =>
        {
            delete_selected_budget_policy(app);
            true
        }

        // Daemon-backed sections: R to refresh daemon-backed operator state
        KeyCode::Char('R')
            if (DAEMON_FEATURE_SECTIONS.contains(&app.settings_state.section)
                || matches!(
                    app.settings_state.section,
                    SettingsSection::Usage
                        | SettingsSection::Budgets
                        | SettingsSection::MemoryDreaming
                ))
                && app.settings_state.focus == SettingsFocus::Items =>
        {
            app.pending_lc_actions.push(LcAction::RefreshDaemonFeatures);
            app.pending_lc_actions.push(LcAction::RefreshUsageStats);
            true
        }

        // `/` opens the incremental settings search query line (Epic M design D.3).
        KeyCode::Char('/')
            if app.settings_state.focus == SettingsFocus::Items
                && !app.settings_state.query_active =>
        {
            app.settings_state.query_active = true;
            app.settings_state.query.clear();
            true
        }

        // `n`/`N`: cycle to the next/previous search match, wrapping with a notice.
        KeyCode::Char('n')
            if app.settings_state.focus == SettingsFocus::Items
                && !app.settings_state.query.is_empty() =>
        {
            match find_next_search_match(
                &app.settings_state.query,
                app.settings_state.section,
                app.settings_state.selected_index,
                false,
            ) {
                Some((section, idx)) => {
                    let wrapped = SettingsSection::ALL
                        .iter()
                        .position(|s| *s == section)
                        .unwrap_or(0)
                        < SettingsSection::ALL
                            .iter()
                            .position(|s| *s == app.settings_state.section)
                            .unwrap_or(0)
                        || (section == app.settings_state.section
                            && idx <= app.settings_state.selected_index);
                    enter_settings_section(app, section, idx);
                    if wrapped {
                        app.notify("Search wrapped to the first match");
                    }
                }
                None => app.notify(format!(
                    "No settings match \"{}\"",
                    app.settings_state.query
                )),
            }
            true
        }
        KeyCode::Char('N')
            if app.settings_state.focus == SettingsFocus::Items
                && !app.settings_state.query.is_empty() =>
        {
            match find_prev_search_match(
                &app.settings_state.query,
                app.settings_state.section,
                app.settings_state.selected_index,
                false,
            ) {
                Some((section, idx)) => {
                    let wrapped = SettingsSection::ALL
                        .iter()
                        .position(|s| *s == section)
                        .unwrap_or(0)
                        > SettingsSection::ALL
                            .iter()
                            .position(|s| *s == app.settings_state.section)
                            .unwrap_or(0)
                        || (section == app.settings_state.section
                            && idx >= app.settings_state.selected_index);
                    enter_settings_section(app, section, idx);
                    if wrapped {
                        app.notify("Search wrapped to the last match");
                    }
                }
                None => app.notify(format!(
                    "No settings match \"{}\"",
                    app.settings_state.query
                )),
            }
            true
        }

        // Consume l/Right in items panel (already in right panel)
        KeyCode::Char('l') | KeyCode::Right => true,

        _ => false,
    }
}

/// Navigate down within settings pane (called from App::nav_down).
pub fn nav_down(state: &mut SettingsState, settings: &UserSettings) {
    match state.focus {
        SettingsFocus::Categories => {
            let max = SettingsSection::ALL.len().saturating_sub(1);
            let idx = section_index(state.section);
            if idx < max {
                state.section = SettingsSection::ALL[idx + 1];
                state.selected_index = 0;
            }
        }
        SettingsFocus::Items => {
            let max = item_count(state.section, settings);
            if state.selected_index + 1 < max {
                state.selected_index += 1;
            }
        }
    }
}

/// Navigate up within settings pane (called from App::nav_up).
pub fn nav_up(state: &mut SettingsState, _settings: &UserSettings) {
    match state.focus {
        SettingsFocus::Categories => {
            let idx = section_index(state.section);
            if idx > 0 {
                state.section = SettingsSection::ALL[idx - 1];
                state.selected_index = 0;
            }
        }
        SettingsFocus::Items => {
            state.selected_index = state.selected_index.saturating_sub(1);
        }
    }
}

/// Close settings pane and restore previous pane content.
pub fn close_settings(app: &mut App) {
    let tab = &mut app.tabs[app.active_tab];
    let focused = tab.focused_pane;
    if let Some(pane) = tab.layout.find_pane_mut(focused) {
        if !matches!(pane, Pane::Settings) {
            return;
        }
        if let Some(prev) = app.pre_settings_pane.take() {
            *pane = prev;
        } else {
            *pane = Pane::SessionList {
                selected_index: 0,
                selected_session: None,
                scroll_offset: 0,
                active_zone: Default::default(),
                taskrabbit_selected_index: 0,
                archive_selected_index: 0,
                jobs_selected_index: 0,
            };
        }
    }
}

/// Floor row count for a daemon-features-backed section, computed against
/// the static `DaemonFeatureEntry::defaults()` list rather than live
/// `app.daemon_features` (this function has no `App` access — it backs
/// `nav_down`/`nav_up`, which only see `&UserSettings`).
fn daemon_feature_floor_count(section: SettingsSection) -> usize {
    let defaults = DaemonFeatureEntry::defaults();
    SETTINGS
        .iter()
        .filter(|spec| spec.section == section)
        .filter(|spec| {
            daemon_feature_field_for(spec.id)
                .is_some_and(|field| defaults.iter().any(|entry| entry.field == field))
        })
        .count()
}

/// Number of items in a settings section.
///
/// Static (no I/O) sections pull from `UserSettings`. Hooks / Skills are
/// I/O-backed; the renderer routes through `App::cached_*` getters that load
/// + cache lazily. The value returned here is a floor (the placeholder row)
/// for Hooks; the renderer detects the real row count from the cache and
/// re-flows on each frame via the same getter.
pub fn item_count(section: SettingsSection, settings: &UserSettings) -> usize {
    if DAEMON_FEATURE_SECTIONS.contains(&section) {
        return daemon_feature_floor_count(section);
    }
    match section {
        SettingsSection::ThemeColors => 20,
        SettingsSection::Screen => 4,
        SettingsSection::TranscriptDefaults => 3,
        SettingsSection::InputPrompts => 3,
        // Each provider is one row; always show at least 1 row (the "[+ add]" hint).
        SettingsSection::ApiProviders => settings.custom_providers.len().max(1),
        SettingsSection::SessionList => {
            crate::settings::NAVIGATOR_SETTINGS_ROW_COUNT + settings.card_fields.len()
        }
        SettingsSection::ModelRoles => 6,
        // Hooks/Skills are I/O-backed; resolved via `App::cached_*_count` for
        // navigation purposes (separate code path because UserSettings doesn't
        // own the data). Returning 1 here keeps `selected_index` valid until
        // the cache resolves on first render.
        SettingsSection::ClaudeHooks => 1,
        SettingsSection::ClaudeSkills => 1,
        SettingsSection::MessageBridges => 2,
        SettingsSection::SystemPrompt => 1,
        // Baseline row floor; live rendering/navigation expands this through
        // `model_control_stats::stats_row_count`.
        SettingsSection::Usage => 16,
        // Baseline row floor (always >= 1 via the synthetic empty-state row);
        // live rendering/navigation expands this through
        // `model_control_budgets::budget_row_count`.
        SettingsSection::Budgets => 1,
        SettingsSection::ModelControl
        | SettingsSection::RetriesRecovery
        | SettingsSection::StallDetection
        | SettingsSection::MemoryDreaming
        | SettingsSection::Orchestration
        | SettingsSection::CodeIntelligence
        | SettingsSection::ProviderIsolation
        | SettingsSection::SandboxStorage => {
            unreachable!("handled by DAEMON_FEATURE_SECTIONS above")
        }
    }
}

/// I/O-backed item count for the Hooks section. Forces a cache load.
pub fn hook_row_count(app: &mut App) -> usize {
    app.cached_claude_settings_rows().len().max(1)
}

/// I/O-backed item count for the Skills section. Forces a cache load.
pub fn skill_row_count(app: &mut App) -> usize {
    app.cached_user_skills().len().max(1)
}

fn section_index(section: SettingsSection) -> usize {
    SettingsSection::ALL
        .iter()
        .position(|&c| c == section)
        .unwrap_or(0)
}

pub fn theme_role_for_settings_index(index: usize) -> Option<crate::ui::theme_roles::ThemeRole> {
    index
        .checked_sub(1)
        .and_then(|index| crate::ui::theme_roles::ThemeRole::ALL.get(index).copied())
}

fn activate_theme_color_row(app: &mut App) {
    match app.settings_state.selected_index {
        0 => crate::overlay::open_theme_picker(app),
        1..=17 => {
            if let Some(role) = theme_role_for_settings_index(app.settings_state.selected_index) {
                crate::overlay::open_theme_role_editor(app, role);
            }
        }
        18 => crate::overlay::open_color_customizer(app),
        19 => {
            crate::ui::theme::clear_theme_role_overrides();
            app.notify_success("Active theme semantic overrides reset");
            app.mark_dirty();
        }
        _ => {}
    }
}

/// Handle Enter key in the ApiModels category: edit the selected provider, or open add form.
fn handle_api_models_enter(app: &mut App) {
    let idx = app.settings_state.selected_index;
    let entry = app.settings.custom_providers.get(idx).cloned();
    open_provider_form(app, entry.as_ref());
}

fn handle_message_bridges_enter(app: &mut App) {
    let bridge = match app.settings_state.selected_index {
        0 => MessageBridgeKind::Signal,
        1 => MessageBridgeKind::Imessage,
        _ => return,
    };
    open_message_bridge_form(app, bridge);
}

fn toggle_setting_on_app(app: &mut App, state: &SettingsState) {
    // RSI-026: SystemPrompt cycling is now daemon-owned. Skip the in-memory
    // mutation in `toggle_setting` (it would race the daemon's authoritative
    // value) and instead emit an action; the async dispatcher dispatches the
    // UpdateDaemonConfig RPC and refreshes the local cache on success.
    if state.section == SettingsSection::SystemPrompt && state.selected_index == 0 {
        app.pending_lc_actions
            .push(LcAction::CycleSystemPromptPreset);
        return;
    }
    toggle_setting(&mut app.settings, state);
    // Rebuild prompt_processor guard when the Prompt compiler toggle changes.
    if state.section == SettingsSection::InputPrompts && state.selected_index == 2 {
        app.prompt_processor =
            crate::prompt_processor::build_processor(&app.settings.prompt_processor);
    }
    // When the global "hide tool results" default changes, update all existing sessions.
    if state.section == SettingsSection::TranscriptDefaults && state.selected_index == 2 {
        let hide = app.settings.default_hide_tool_results;
        for session_state in app.sessions.values_mut() {
            session_state.show_tool_results = !hide;
            crate::ui::height::invalidate_heights(session_state);
        }
    }
    // Card field changes affect has_pills → must invalidate card height cache.
    if state.section == SettingsSection::SessionList {
        app.invalidate_card_cache();
    }
}

/// If the current section is daemon-features-backed, queue a refresh action
/// so the list is populated from the daemon on every entry to the panel.
fn maybe_queue_daemon_features_refresh(app: &mut App) {
    if DAEMON_FEATURE_SECTIONS.contains(&app.settings_state.section) {
        app.pending_lc_actions.push(LcAction::RefreshDaemonFeatures);
    }
}

fn refresh_entered_settings_section(app: &mut App) {
    maybe_queue_daemon_features_refresh(app);
    maybe_queue_usage_stats_refresh(app);
    ensure_claude_caches_for_category(app);
}

fn enter_settings_section(app: &mut App, section: SettingsSection, selected_index: usize) {
    app.settings_state.section = section;
    app.settings_state.selected_index = selected_index;
    refresh_entered_settings_section(app);
}

/// If the current section is Usage, Budgets, or daemon-features-backed,
/// queue a refresh action so usage/model-control aggregates are recomputed
/// from the daemon on every entry to the panel.
fn maybe_queue_usage_stats_refresh(app: &mut App) {
    if DAEMON_FEATURE_SECTIONS.contains(&app.settings_state.section)
        || matches!(
            app.settings_state.section,
            SettingsSection::Usage | SettingsSection::Budgets
        )
    {
        app.pending_lc_actions.push(LcAction::RefreshUsageStats);
    }
}

/// Lazy-load `~/.claude/` caches when entering a Hooks/Skills section.
/// Cheap (sub-millisecond) for files this small; safe to call repeatedly.
pub fn ensure_claude_caches_for_category(app: &mut App) {
    match app.settings_state.section {
        SettingsSection::ClaudeHooks => {
            // Populate by calling the getter (which lazy-loads).
            let _ = app.cached_claude_settings_rows();
        }
        SettingsSection::ClaudeSkills => {
            let _ = app.cached_user_skills();
        }
        _ => {}
    }
}

/// Build (provider, models, current_model) for an Agent Actors dropdown item.
fn agent_actors_dropdown_config(
    app: &App,
    idx: usize,
) -> (
    SessionProvider,
    Option<usize>,
    Vec<(String, String)>,
    Option<String>,
) {
    match idx {
        0 => {
            // Default Model (provider-discovered)
            (
                app.selected_provider,
                app.custom_provider_index,
                app.available_models.clone(),
                app.selected_model.clone(),
            )
        }
        1 => {
            let provider = app.settings.title_model_provider;
            let custom_idx =
                custom_provider_index(app, app.settings.title_model_custom_provider_id);
            let models = actor_models_for_provider(app, provider, custom_idx);
            (
                provider,
                custom_idx,
                models,
                Some(app.settings.title_model_local.clone()),
            )
        }
        2 => {
            let provider = app.settings.prompt_processor.provider;
            let custom_idx =
                custom_provider_index(app, app.settings.prompt_processor.custom_provider_id);
            let models = actor_models_for_provider(app, provider, custom_idx);
            (
                provider,
                custom_idx,
                models,
                Some(app.settings.prompt_processor.model.clone()),
            )
        }
        3 => {
            let provider = app.settings.memory_model_fallback_provider;
            let custom_idx =
                custom_provider_index(app, app.settings.memory_model_fallback_custom_provider_id);
            let models = actor_models_for_provider(app, provider, custom_idx);
            (
                provider,
                custom_idx,
                models,
                Some(app.settings.memory_model_fallback.clone()),
            )
        }
        4 => {
            let provider = app.settings.dream_model_provider;
            let custom_idx =
                custom_provider_index(app, app.settings.dream_model_custom_provider_id);
            let models = actor_models_for_provider(app, provider, custom_idx);
            (
                provider,
                custom_idx,
                models,
                Some(app.settings.dream_model.clone()),
            )
        }
        5 => {
            // Classifier model — daemon-owned, no provider concept of its
            // own (it always calls the OpenAI-compatible endpoint at
            // `stall_classifier_api_url`, default a local Ollama server), so
            // the picker is pinned to the Local provider's discovered model
            // list rather than a provider-cycled dropdown like idx 0/1/2/4.
            let current =
                DaemonFeatureEntry::display_value(&app.daemon_features, "stall_classifier_model")
                    .map(str::to_string);
            (
                SessionProvider::Local,
                None,
                app.local_models.clone(),
                current,
            )
        }
        _ => (SessionProvider::Claude, None, vec![], None),
    }
}

fn custom_provider_index(app: &App, id: Option<uuid::Uuid>) -> Option<usize> {
    id.and_then(|id| {
        app.settings
            .custom_providers
            .iter()
            .position(|entry| entry.id == id)
    })
}

fn actor_models_for_provider(
    app: &App,
    provider: SessionProvider,
    custom_idx: Option<usize>,
) -> Vec<(String, String)> {
    if let Some(idx) = custom_idx
        && let Some(entry) = app.settings.custom_providers.get(idx)
    {
        let model_id = if entry.default_model.is_empty() {
            entry.name.clone()
        } else {
            entry.default_model.clone()
        };
        return vec![(model_id, entry.name.clone())];
    }
    if provider == SessionProvider::Local {
        app.local_models.clone()
    } else {
        crate::app::models_for_provider(provider)
    }
}

fn queue_agent_actor_dropdown_refresh(app: &mut App) {
    let item_idx = match app.settings_state.active_dropdown_item {
        Some(idx @ 1..=5) => idx,
        _ => return,
    };
    let provider = app.settings_state.model_dropdown.provider;
    let custom_provider_index = app.settings_state.model_dropdown.custom_provider_index;
    if custom_provider_index.is_some() {
        return;
    }
    if provider == SessionProvider::Local {
        app.needs_local_model_refresh = true;
        return;
    }
    let socket_path = app.client.socket_path().to_path_buf();
    let fallback = crate::app::models_for_provider(provider);
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.settings_model_discovery_rx = Some(rx);
    tokio::spawn(async move {
        let mut client = crate::client::DaemonClient::new(socket_path);
        let models = if client.connect().await.is_ok() {
            client.discover_models(provider).await.unwrap_or(fallback)
        } else {
            fallback
        };
        let _ = tx.send(crate::app::SettingsModelDiscoveryResult {
            item_idx,
            provider,
            custom_provider_index,
            models,
        });
    });
}

/// Apply a model selection from the Agent Actors dropdown to the correct field.
fn handle_agent_actors_selection(app: &mut App, model_id: String) -> bool {
    if app.settings_state.active_dropdown_item == Some(5) && !app.authoritative_config_ready() {
        // This row mirrors daemon-owned state, unlike the other five local
        // Agent Actors preferences. Refuse before changing either its cached
        // value or the active dropdown so the user keeps the visible state.
        app.notify_error(app.daemon_config_unavailable_reason());
        return false;
    }
    match app.settings_state.active_dropdown_item {
        Some(0) => {
            // Default Model
            app.selected_model = Some(model_id);
            app.selected_provider = app.settings_state.model_dropdown.provider;
            app.custom_provider_index = app.settings_state.model_dropdown.custom_provider_index;
            app.available_models = app.settings_state.model_dropdown.models.clone();
            app.model_discovery_rx = None;
        }
        Some(1) => {
            app.settings.title_model_provider = app.settings_state.model_dropdown.provider;
            app.settings.title_model_custom_provider_id = app
                .settings_state
                .model_dropdown
                .custom_provider_index
                .and_then(|idx| app.settings.custom_providers.get(idx).map(|entry| entry.id));
            app.settings.title_model_local = model_id;
            app.pending_lc_actions.push(LcAction::SyncTitleModelConfig);
        }
        Some(2) => {
            app.settings.prompt_processor.provider = app.settings_state.model_dropdown.provider;
            let custom_entry = app
                .settings_state
                .model_dropdown
                .custom_provider_index
                .and_then(|idx| app.settings.custom_providers.get(idx));
            app.settings.prompt_processor.custom_provider_id = custom_entry.map(|entry| entry.id);
            app.settings.prompt_processor.custom_base_url =
                custom_entry.map(|entry| entry.base_url.clone());
            app.settings.prompt_processor.custom_api_key =
                custom_entry.map(|entry| entry.api_key.clone());
            app.settings.prompt_processor.model = model_id;
            app.prompt_processor =
                crate::prompt_processor::build_processor(&app.settings.prompt_processor);
            app.pending_lc_actions
                .push(LcAction::SyncPromptProcessorConfig);
        }
        Some(3) => {
            app.settings.memory_model_fallback_provider =
                app.settings_state.model_dropdown.provider;
            app.settings.memory_model_fallback_custom_provider_id = app
                .settings_state
                .model_dropdown
                .custom_provider_index
                .and_then(|idx| app.settings.custom_providers.get(idx).map(|entry| entry.id));
            app.settings.memory_model_fallback = model_id;
            app.pending_lc_actions.push(LcAction::SyncMemoryModelConfig);
        }
        Some(4) => {
            app.settings.dream_model_provider = app.settings_state.model_dropdown.provider;
            app.settings.dream_model_custom_provider_id = app
                .settings_state
                .model_dropdown
                .custom_provider_index
                .and_then(|idx| app.settings.custom_providers.get(idx).map(|entry| entry.id));
            app.settings.dream_model = model_id;
            app.pending_lc_actions.push(LcAction::SyncMemoryModelConfig);
        }
        Some(5) => {
            // Classifier model has no `UserSettings` counterpart — it's
            // daemon-owned. The response task keeps this dropdown and the
            // prior daemon mirror intact until semantic acceptance.
            crate::action_handler::daemon_config::sync_classifier_model_config(app, model_id);
            return false;
        }
        _ => {}
    }
    true
}

fn toggle_setting(settings: &mut UserSettings, state: &SettingsState) {
    let idx = state.selected_index;
    match state.section {
        SettingsSection::Screen => match idx {
            0 => {
                settings.text_area_backfill_enabled = !settings.text_area_backfill_enabled;
            }
            // Index 1 (text_area_backfill_hex) opens an editor overlay — handled
            // in `handle_settings_key`, not via toggle.
            2 => {
                settings.formulation_anim_ms =
                    cycle_u64(settings.formulation_anim_ms, FORMULATION_ANIM_DURATIONS);
            }
            3 => {
                settings.activity_indicator_style = settings.activity_indicator_style.next();
            }
            _ => {}
        },
        SettingsSection::InputPrompts => match idx {
            0 => {
                settings.submit_on_enter = !settings.submit_on_enter;
            }
            1 => {
                settings.auto_open_question_modal = !settings.auto_open_question_modal;
            }
            2 => settings.prompt_processor.enabled = !settings.prompt_processor.enabled,
            _ => {}
        },
        SettingsSection::ThemeColors => {}
        SettingsSection::TranscriptDefaults => match idx {
            0 => settings.default_show_system_events = !settings.default_show_system_events,
            1 => settings.default_show_thinking_events = !settings.default_show_thinking_events,
            2 => settings.default_hide_tool_results = !settings.default_hide_tool_results,
            _ => {}
        },
        SettingsSection::ApiProviders => {} // Handled separately via handle_api_models_enter.
        SettingsSection::SessionList => {
            if idx == 0 {
                settings.navigator_preset = settings.navigator_preset.next();
                // A preset selection is an explicit reset to its declared
                // enabled subsequence. Advanced toggles remain immediate, but
                // must not mask future preset changes.
                settings.navigator_optional_columns = None;
            } else if let Some(column) = crate::types::NavigatorOptionalColumn::ALL
                .get(idx.saturating_sub(1))
                .copied()
            {
                settings.toggle_navigator_optional_column(column);
            } else if let Some(entry) = settings
                .card_fields
                .get_mut(idx.saturating_sub(crate::settings::NAVIGATOR_SETTINGS_ROW_COUNT))
            {
                entry.enabled = !entry.enabled;
            }
        }
        // Daemon-features-backed sections and the daemon-owned MemoryDreaming
        // rows are toggled via RPC (`LcAction::ToggleDaemonFeature`); UserSettings
        // is not mutated for them here.
        SettingsSection::ModelControl
        | SettingsSection::RetriesRecovery
        | SettingsSection::StallDetection
        | SettingsSection::Orchestration
        | SettingsSection::CodeIntelligence
        | SettingsSection::ProviderIsolation
        | SettingsSection::SandboxStorage => {}
        // Model Roles items are handled via model dropdown, not toggle_setting.
        SettingsSection::ModelRoles => {}
        // Message bridge rows open an editor form.
        SettingsSection::MessageBridges => {}
        // Hooks/Skills mutate ~/.claude/ on disk via dedicated handlers; toggle is a no-op here.
        SettingsSection::ClaudeHooks => {}
        SettingsSection::ClaudeSkills => {}
        SettingsSection::MemoryDreaming => {}
        // RSI-026: SystemPrompt cycling is daemon-owned and dispatched via
        // `LcAction::CycleSystemPromptPreset` from `toggle_setting_on_app`;
        // toggle_setting itself is a no-op for this section so direct
        // in-memory mutation (which would race the daemon) is impossible.
        SettingsSection::SystemPrompt => {}
        // Usage is a read-only daemon aggregate; nothing to toggle.
        SettingsSection::Usage => {}
        // Budgets rows are add/edit/delete via dedicated 'a'/'d'/Enter
        // handlers (RPC round-trip); toggle is a no-op here.
        SettingsSection::Budgets => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    /// (c) acceptance: `settings_search_commits_and_jumps_across_sections`
    /// (Epic M design D.3). `/` opens the query line, typed characters feed
    /// it (not the section's own bindings — proven by typing `d`, which
    /// would otherwise be a no-op key but must not be swallowed by any
    /// stray binding), and `Enter` commits and jumps across sections to the
    /// first match at/after the cursor.
    #[test]
    fn settings_search_commits_and_jumps_across_sections() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.settings_state.focus = SettingsFocus::Items;
        app.settings_state.section = SettingsSection::ThemeColors;
        app.settings_state.selected_index = 0;

        assert!(handle_settings_key(&mut app, key(KeyCode::Char('/'))));
        assert!(app.settings_state.query_active);
        for ch in "dialectic".chars() {
            assert!(handle_settings_key(&mut app, key(KeyCode::Char(ch))));
        }
        assert_eq!(app.settings_state.query, "dialectic");
        assert!(handle_settings_key(&mut app, key(KeyCode::Enter)));

        assert!(!app.settings_state.query_active);
        assert_eq!(app.settings_state.section, SettingsSection::MemoryDreaming);
        let specs: Vec<&SettingSpec> = SETTINGS
            .iter()
            .filter(|spec| spec.section == SettingsSection::MemoryDreaming)
            .collect();
        assert_eq!(
            specs[app.settings_state.selected_index].id,
            SettingId::DialecticEngine
        );
    }

    #[tokio::test]
    async fn settings_query_keys_bypass_unavailable_item_actions() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        let focused = app.active_tab().focused_pane;
        let Some(pane) = app.active_tab_mut().layout.find_pane_mut(focused) else {
            panic!("focused pane is available");
        };
        *pane = Pane::Settings;
        app.settings_state.focus = SettingsFocus::Items;
        app.settings_state.section = SettingsSection::Usage;
        app.settings_state.selected_index = 0;
        app.settings_state.query_active = true;

        for ch in "dialectic".chars() {
            assert!(handle_settings_key_event(&mut app, key(KeyCode::Char(ch))).await);
        }
        assert_eq!(app.settings_state.query, "dialectic");
        assert!(handle_settings_key_event(&mut app, key(KeyCode::Enter)).await);
        assert!(!app.settings_state.query_active);
        assert_eq!(app.settings_state.section, SettingsSection::MemoryDreaming);
    }

    #[test]
    fn settings_search_jumps_run_section_entry_loaders() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.settings_state.focus = SettingsFocus::Items;
        app.settings_state.section = SettingsSection::ThemeColors;
        app.settings_state.query = "skills".to_string();
        app.settings_state.query_active = true;
        assert!(handle_settings_key(&mut app, key(KeyCode::Enter)));
        assert_eq!(app.settings_state.section, SettingsSection::ClaudeSkills);
        assert!(
            app.cached_user_skills.is_some(),
            "skills cache loaded on jump"
        );

        app.pending_lc_actions.clear();
        app.settings_state.query = "retry max backoff".to_string();
        assert!(handle_settings_key(&mut app, key(KeyCode::Char('n'))));
        assert_eq!(app.settings_state.section, SettingsSection::RetriesRecovery);
        assert!(
            app.pending_lc_actions
                .contains(&LcAction::RefreshDaemonFeatures)
        );
        assert!(
            app.pending_lc_actions
                .contains(&LcAction::RefreshUsageStats)
        );

        app.pending_lc_actions.clear();
        app.settings_state.section = SettingsSection::MessageBridges;
        assert!(handle_settings_key(&mut app, key(KeyCode::Char('N'))));
        assert_eq!(app.settings_state.section, SettingsSection::RetriesRecovery);
        assert!(
            app.pending_lc_actions
                .contains(&LcAction::RefreshDaemonFeatures)
        );
    }

    #[test]
    fn settings_search_highlights_expanded_dynamic_rows() {
        assert!(search_row_matches(
            SettingsSection::ClaudeSkills,
            3,
            "skill"
        ));
        assert!(search_row_matches(SettingsSection::ClaudeHooks, 3, "hook"));
        assert!(search_row_matches(
            SettingsSection::ApiProviders,
            3,
            "provider"
        ));
    }

    /// (c) acceptance: `settings_n_and_N_cycle_matches_with_wrap`. `hex`
    /// matches exactly two rows in two different sections (`Theme roles` in
    /// ThemeColors, via its role-editor detail text; `Background color` in
    /// Screen, via its keywords), with ThemeColors preceding Screen in page
    /// order. `n` cycles forward and wraps to the first match with a
    /// notice; `N` cycles backward and wraps to the last match.
    #[test]
    fn settings_n_and_n_cycle_matches_with_wrap() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.settings_state.focus = SettingsFocus::Items;
        app.settings_state.section = SettingsSection::MessageBridges; // last section
        app.settings_state.selected_index = 1;
        app.settings_state.query = "hex".to_string();

        assert!(handle_settings_key(&mut app, key(KeyCode::Char('n'))));
        assert_eq!(
            app.settings_state.section,
            SettingsSection::ThemeColors,
            "wraps forward to the first match"
        );
        assert_eq!(app.settings_state.selected_index, 1); // first Theme roles row
        assert!(
            app.notifications
                .back()
                .is_some_and(|n| n.message.contains("wrapped")),
            "wrap notice shown: {:?}",
            app.notifications.back()
        );

        app.notifications.clear();
        assert!(handle_settings_key(&mut app, key(KeyCode::Char('n'))));
        assert_eq!(app.settings_state.section, SettingsSection::Screen);
        assert_eq!(app.settings_state.selected_index, 1); // Background color
        assert!(app.notifications.is_empty(), "no wrap notice mid-list");

        assert!(handle_settings_key(&mut app, key(KeyCode::Char('N'))));
        assert_eq!(app.settings_state.section, SettingsSection::ThemeColors);
        assert_eq!(app.settings_state.selected_index, 1);

        assert!(handle_settings_key(&mut app, key(KeyCode::Char('N'))));
        assert_eq!(
            app.settings_state.section,
            SettingsSection::Screen,
            "N wraps back to the last match"
        );
        assert!(
            app.notifications
                .back()
                .is_some_and(|n| n.message.contains("wrapped")),
            "wrap notice shown: {:?}",
            app.notifications.back()
        );
    }

    #[test]
    fn test_settings_category_all_count() {
        // Epic M design D.1: 21 sections under 7 rail groups.
        assert_eq!(SettingsSection::ALL.len(), 21);
    }

    #[test]
    fn test_item_count_per_category() {
        let settings = UserSettings::default();
        assert_eq!(item_count(SettingsSection::ThemeColors, &settings), 20);
        assert_eq!(item_count(SettingsSection::Screen, &settings), 4);
        assert_eq!(
            item_count(SettingsSection::TranscriptDefaults, &settings),
            3
        );
        assert_eq!(
            item_count(SettingsSection::RetriesRecovery, &settings),
            daemon_feature_floor_count(SettingsSection::RetriesRecovery)
        );
        assert_eq!(item_count(SettingsSection::Usage, &settings), 16);
        assert_eq!(item_count(SettingsSection::Budgets, &settings), 1);
    }

    #[test]
    fn test_nav_down_categories() {
        let settings = UserSettings::default();
        let mut state = SettingsState::default();
        assert_eq!(state.section, SettingsSection::ThemeColors);
        for expected in [
            SettingsSection::Screen,
            SettingsSection::SessionList,
            SettingsSection::TranscriptDefaults,
            SettingsSection::InputPrompts,
            SettingsSection::ModelRoles,
            SettingsSection::ApiProviders,
            SettingsSection::SystemPrompt,
            SettingsSection::ModelControl,
            SettingsSection::Budgets,
            SettingsSection::Usage,
            SettingsSection::RetriesRecovery,
            SettingsSection::StallDetection,
            SettingsSection::MemoryDreaming,
            SettingsSection::Orchestration,
            SettingsSection::CodeIntelligence,
            SettingsSection::ProviderIsolation,
            SettingsSection::SandboxStorage,
            SettingsSection::ClaudeHooks,
            SettingsSection::ClaudeSkills,
            SettingsSection::MessageBridges,
        ] {
            nav_down(&mut state, &settings);
            assert_eq!(state.section, expected);
        }
        nav_down(&mut state, &settings);
        assert_eq!(state.section, SettingsSection::MessageBridges); // clamped at last
    }

    #[test]
    fn test_nav_up_categories() {
        let settings = UserSettings::default();
        let mut state = SettingsState {
            section: SettingsSection::TranscriptDefaults,
            ..Default::default()
        };
        nav_up(&mut state, &settings);
        assert_eq!(state.section, SettingsSection::SessionList);
        nav_up(&mut state, &settings);
        assert_eq!(state.section, SettingsSection::Screen);
        nav_up(&mut state, &settings);
        assert_eq!(state.section, SettingsSection::ThemeColors);
        nav_up(&mut state, &settings);
        assert_eq!(state.section, SettingsSection::ThemeColors);
    }

    #[test]
    fn test_nav_down_items() {
        let settings = UserSettings::default();
        let mut state = SettingsState {
            focus: SettingsFocus::Items,
            selected_index: 0,
            section: SettingsSection::ThemeColors,
            ..Default::default()
        };
        for expected in 1..=19 {
            nav_down(&mut state, &settings);
            assert_eq!(state.selected_index, expected);
        }
        nav_down(&mut state, &settings);
        assert_eq!(state.selected_index, 19); // clamped at max (20 rows)
    }

    #[test]
    fn test_toggle_auto_open_question_modal() {
        let mut settings = UserSettings::default();
        let state = SettingsState {
            section: SettingsSection::InputPrompts,
            selected_index: 1,
            focus: SettingsFocus::Items,
            ..Default::default()
        };
        assert!(!settings.auto_open_question_modal);
        toggle_setting(&mut settings, &state);
        assert!(settings.auto_open_question_modal);
        toggle_setting(&mut settings, &state);
        assert!(!settings.auto_open_question_modal);
    }

    #[test]
    fn test_cycle_activity_indicator_style() {
        let mut settings = UserSettings::default();
        let state = SettingsState {
            section: SettingsSection::Screen,
            selected_index: 3,
            focus: SettingsFocus::Items,
            ..Default::default()
        };

        assert_eq!(
            settings.activity_indicator_style,
            crate::settings::ActivityIndicatorStyle::Semantic
        );
        toggle_setting(&mut settings, &state);
        assert_eq!(
            settings.activity_indicator_style,
            crate::settings::ActivityIndicatorStyle::RainbowClassic
        );
        toggle_setting(&mut settings, &state);
        assert_eq!(
            settings.activity_indicator_style,
            crate::settings::ActivityIndicatorStyle::RainbowCompact
        );
        toggle_setting(&mut settings, &state);
        assert_eq!(
            settings.activity_indicator_style,
            crate::settings::ActivityIndicatorStyle::Semantic
        );
    }

    #[test]
    fn test_item_count_card_fields() {
        let settings = UserSettings::default();
        assert_eq!(
            item_count(SettingsSection::SessionList, &settings),
            crate::settings::NAVIGATOR_SETTINGS_ROW_COUNT + settings.card_fields.len()
        );
        assert_eq!(
            settings.card_fields.len(),
            crate::types::CardField::ALL.len()
        );
    }

    #[test]
    fn test_toggle_card_field() {
        let mut settings = UserSettings::default();
        let state = SettingsState {
            section: SettingsSection::SessionList,
            selected_index: crate::settings::NAVIGATOR_SETTINGS_ROW_COUNT, // ContextBar
            focus: SettingsFocus::Items,
            ..Default::default()
        };
        assert!(settings.card_fields[0].enabled);
        toggle_setting(&mut settings, &state);
        assert!(!settings.card_fields[0].enabled);
        toggle_setting(&mut settings, &state);
        assert!(settings.card_fields[0].enabled);
    }

    #[test]
    fn navigator_advanced_toggle_changes_only_selected_optional_column_immediately() {
        let mut settings = UserSettings::default();
        let state = SettingsState {
            section: SettingsSection::SessionList,
            selected_index: 3, // Retry after preset, age, model/effort.
            focus: SettingsFocus::Items,
            ..Default::default()
        };
        toggle_setting(&mut settings, &state);
        let enabled = settings.navigator_optional_columns.as_ref().unwrap();
        assert!(enabled.contains(&crate::types::NavigatorOptionalColumn::Retry));
        assert!(enabled.contains(&crate::types::NavigatorOptionalColumn::Age));
        assert!(enabled.contains(&crate::types::NavigatorOptionalColumn::ModelEffort));
        assert!(!enabled.contains(&crate::types::NavigatorOptionalColumn::Cost));
    }

    #[test]
    fn t37_preset_cycle_clears_advanced_override_and_changes_effective_columns() {
        let mut settings = UserSettings::default();
        settings.toggle_navigator_optional_column(crate::types::NavigatorOptionalColumn::Retry);
        assert!(settings.navigator_optional_columns.is_some());
        let state = SettingsState {
            section: SettingsSection::SessionList,
            selected_index: 0,
            focus: SettingsFocus::Items,
            ..Default::default()
        };
        toggle_setting(&mut settings, &state);
        assert_eq!(
            settings.navigator_preset,
            crate::types::NavigatorPreset::Operations
        );
        assert!(settings.navigator_optional_columns.is_none());
        assert_eq!(
            crate::ui::navigator_layout::preset_columns(settings.navigator_preset),
            vec![
                crate::types::NavigatorOptionalColumn::Age,
                crate::types::NavigatorOptionalColumn::ModelEffort,
                crate::types::NavigatorOptionalColumn::Retry,
                crate::types::NavigatorOptionalColumn::Work,
                crate::types::NavigatorOptionalColumn::Project,
            ]
        );
    }

    #[test]
    fn test_toggle_system_prompt_preset_is_noop_in_direct_helper() {
        // RSI-026: SystemPrompt cycling is daemon-owned. `toggle_setting`
        // itself is a no-op for the SystemPrompt section — the
        // `toggle_setting_on_app` wrapper intercepts the section and emits
        // `LcAction::CycleSystemPromptPreset` instead. Confirm the in-memory
        // mutation path is dead so unit-level callers can't race the daemon.
        use crate::settings::SystemPromptPreset;
        let mut settings = UserSettings::default();
        assert_eq!(settings.system_prompt_preset, SystemPromptPreset::Default);
        let state = SettingsState {
            section: SettingsSection::SystemPrompt,
            selected_index: 0,
            focus: SettingsFocus::Items,
            ..Default::default()
        };
        toggle_setting(&mut settings, &state);
        // The cache must NOT advance — daemon owns the value.
        assert_eq!(settings.system_prompt_preset, SystemPromptPreset::Default);
        toggle_setting(&mut settings, &state);
        assert_eq!(settings.system_prompt_preset, SystemPromptPreset::Default);
    }

    #[test]
    fn test_item_count_tui_configuration() {
        let settings = UserSettings::default();
        assert_eq!(item_count(SettingsSection::InputPrompts, &settings), 3);
    }

    #[test]
    fn test_toggle_prompt_processor_enabled() {
        let mut settings = UserSettings::default();
        assert!(settings.prompt_processor.enabled);
        let state = SettingsState {
            section: SettingsSection::InputPrompts,
            selected_index: 2,
            focus: SettingsFocus::Items,
            ..Default::default()
        };
        toggle_setting(&mut settings, &state);
        assert!(!settings.prompt_processor.enabled);
        toggle_setting(&mut settings, &state);
        assert!(settings.prompt_processor.enabled);
    }

    #[test]
    fn test_item_count_agent_actors() {
        let settings = UserSettings::default();
        assert_eq!(item_count(SettingsSection::ModelRoles, &settings), 6);
    }

    #[test]
    fn test_agent_actors_toggle_setting_is_noop() {
        // Model Roles items are handled via dropdown, not toggle_setting.
        let mut settings = UserSettings::default();
        let original_title = settings.title_model_local.clone();
        let state = SettingsState {
            section: SettingsSection::ModelRoles,
            selected_index: 1,
            focus: SettingsFocus::Items,
            ..Default::default()
        };
        toggle_setting(&mut settings, &state);
        assert_eq!(settings.title_model_local, original_title);
    }

    #[test]
    fn test_item_count_memory_configuration() {
        let settings = UserSettings::default();
        assert_eq!(item_count(SettingsSection::MemoryDreaming, &settings), 6);
    }

    #[test]
    fn memory_rows_write_daemon_fields_only() {
        let specs: Vec<_> = SETTINGS
            .iter()
            .filter(|spec| spec.section == SettingsSection::MemoryDreaming)
            .collect();
        assert_eq!(specs.len(), 6);
        for spec in specs {
            assert!(matches!(spec.owner, SettingOwner::Daemon(_)));
        }
        let app = crate::app::app_test_helpers::with_session_list(0);
        for (idx, spec) in SETTINGS
            .iter()
            .filter(|spec| spec.section == SettingsSection::MemoryDreaming)
            .enumerate()
        {
            if let SettingOwner::Daemon(field) = spec.owner {
                let Some(vec_idx) =
                    daemon_feature_vec_index(&app, SettingsSection::MemoryDreaming, idx)
                else {
                    panic!("daemon row {field} has a feature index");
                };
                assert_eq!(app.daemon_features[vec_idx].field, field);
            }
        }
    }

    #[test]
    fn test_cycle_u64_wraps() {
        assert_eq!(cycle_u64(500, &OBSERVATION_THRESHOLDS), 10); // last -> first
        assert_eq!(cycle_u64(999, &OBSERVATION_THRESHOLDS), 10); // not-in-list -> first
    }

    #[test]
    fn test_user_settings_serde_defaults() {
        // Old state.json without memory fields (or since-removed fields, e.g.
        // `show_audio_waveform`, S-001) should deserialize with defaults.
        let json = r#"{"show_audio_waveform":false}"#;
        let restored: UserSettings = serde_json::from_str(json).unwrap();
        assert!(!restored.memory_owner_migrated);
        assert_eq!(restored.memory_model_local, "gemma4:e4b");
        assert_eq!(restored.memory_model_fallback, "gemma4:e4b");
        assert_eq!(restored.dream_model, "claude-sonnet-5");
        assert!(restored.memory_enabled);
        assert!(!restored.dream_enabled);
        assert_eq!(restored.observation_threshold, 50);
        assert_eq!(restored.dream_cooldown_secs, 28800);
    }

    /// (c) acceptance: `legacy_state_json_with_removed_fields_loads`. Every
    /// field E.1 marks "remove" for slice (c) — S-001 `show_audio_waveform`,
    /// S-002 `audio_viz_mode`, S-027 `prompt_processor.auto_compile`, S-028
    /// `prompt_processor.temperature` — is absent from `UserSettings` /
    /// `PromptProcessorConfig` today; a `state.json` that still carries them
    /// (an old build's output) must load without error, ignoring them.
    #[test]
    fn legacy_state_json_with_removed_fields_loads() {
        let json = r#"{
            "show_audio_waveform": true,
            "audio_viz_mode": "Waveform",
            "prompt_processor": {
                "enabled": true,
                "model": "gemma4:e4b",
                "auto_compile": true,
                "temperature": 0.42
            }
        }"#;
        let restored: UserSettings =
            serde_json::from_str(json).expect("legacy fields are ignored, not rejected");
        assert!(restored.prompt_processor.enabled);
        assert_eq!(restored.prompt_processor.model, "gemma4:e4b");
    }

    #[test]
    fn test_stats_enter_queues_cancel_action_for_running_row() {
        use crate::client::DaemonClient;
        use crossterm::event::{KeyCode, KeyEvent};
        use rsi_common::model_control::{
            AdmissionStatus, BudgetScopeKind, InvocationForeground, InvocationOwner,
            ModelBudgetHeadroom, ModelControlMode, ModelControlStatusReport, ModelInvocationKind,
            ModelInvocationPurpose, ModelInvocationRecord, ModelInvocationStatus,
            ModelInvocationUsage, ModelInvocationView, ModelTier, ModelUsageConfidence, PaidRisk,
        };
        use std::path::PathBuf;

        crate::state::DevState::clear();
        crate::state::PersistedState::default().save();
        let mut app = App::new(DaemonClient::new(PathBuf::from(
            "/tmp/test-settings-stats.sock",
        )));
        app.settings_state.focus = SettingsFocus::Items;
        app.settings_state.section = SettingsSection::Usage;
        app.cached_model_control_status = Some(ModelControlStatusReport {
            mode: ModelControlMode::Normal,
            mode_updated_at: Some("2026-07-15T00:00:00Z".to_string()),
            restart_required_fields: Vec::new(),
            circuit_state: "closed".to_string(),
            circuit_reason: "budget-governed".to_string(),
            circuits: Vec::new(),
            policies: Vec::new(),
            active_invocations: vec![ModelInvocationView {
                record: ModelInvocationRecord {
                    id: uuid::Uuid::from_u128(9),
                    purpose: ModelInvocationPurpose::SessionLaunchFresh,
                    kind: ModelInvocationKind::SessionLifecycle,
                    foreground: InvocationForeground::Foreground,
                    paid_risk: PaidRisk::PaidCapable,
                    status: ModelInvocationStatus::Running,
                    provider: Some("Claude".to_string()),
                    model: Some("claude-sonnet-5".to_string()),
                    backend: Some("Claude".to_string()),
                    model_tier: Some(ModelTier::Premium),
                    effort: Some("medium".to_string()),
                    trigger: "launch_session".to_string(),
                    owner: InvocationOwner {
                        session_id: Some(uuid::Uuid::from_u128(1)),
                        ..Default::default()
                    },
                    owner_scopes: Vec::new(),
                    dedup_key: None,
                    request_fingerprint: None,
                    parent_invocation_id: None,
                    retry_of_invocation_id: None,
                    raw_admission_status: "admitted".to_string(),
                    raw_status: "running".to_string(),
                    admission_status: AdmissionStatus::Admitted,
                    usage: ModelInvocationUsage {
                        input_tokens: Some(10),
                        output_tokens: Some(5),
                        cache_creation_tokens: Some(0),
                        cache_read_tokens: Some(0),
                        reasoning_tokens: Some(0),
                        embedding_input_count: Some(0),
                        wall_time_ms: Some(1_000),
                        estimated_cost_usd: Some(0.01),
                        confidence: ModelUsageConfidence::Measured,
                    },
                    baseline_usage: ModelInvocationUsage::default(),
                    error_class: None,
                    cancellation_requested_at: None,
                    cancellation_reason: None,
                    cancellation_mechanism: None,
                    authorization_reason: Some("policy_session".to_string()),
                    policy_authorized: true,
                    escalation_source: None,
                    escalation_reason: None,
                    policy_snapshot: None,
                    policy_snapshot_status: "valid".to_string(),
                    policy_snapshot_error: None,
                    created_at: "2026-07-15T00:00:00Z".to_string(),
                    started_at: Some("2026-07-15T00:00:01Z".to_string()),
                    completed_at: None,
                },
                owner_summary: "session 00000001".to_string(),
                lineage_summary: "root".to_string(),
                scope_summary: "session:1".to_string(),
                denial_reason: None,
                stop_mechanism: "interrupt_session".to_string(),
                stop_target: Some(uuid::Uuid::from_u128(1).to_string()),
                cancellation_reason: None,
                budget: vec![ModelBudgetHeadroom {
                    scope_kind: BudgetScopeKind::Session,
                    scope_id: Some(uuid::Uuid::from_u128(1).to_string()),
                    purpose: Some(ModelInvocationPurpose::SessionLaunchFresh),
                    model_tier: Some(ModelTier::Premium),
                    effort: Some("medium".to_string()),
                    source: "policy:session/test".to_string(),
                    authorized: true,
                    policy_status: "configured".to_string(),
                    remaining_calls: Some(1),
                    remaining_active: Some(0),
                    remaining_total_tokens: Some(100),
                    remaining_input_tokens: Some(100),
                    remaining_output_tokens: Some(100),
                    remaining_embedding_inputs: None,
                    remaining_wall_time_ms: Some(1_000),
                }],
            }],
            recent_invocations: Vec::new(),
            recent_denials: Vec::new(),
            recent_budget_alerts: Vec::new(),
        });
        app.settings_state.selected_index = crate::model_control_stats::stats_row_count(&app) - 1;

        assert!(handle_settings_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE)
        ));
        assert_eq!(
            app.pending_lc_actions.last(),
            Some(&LcAction::CancelModelInvocation(uuid::Uuid::from_u128(9)))
        );
    }

    fn test_app_for_classifier_row_at(socket_path: std::path::PathBuf) -> App {
        use crate::client::DaemonClient;

        crate::state::DevState::clear();
        crate::state::PersistedState::default().save();
        let mut app = App::new(DaemonClient::new(socket_path));
        app.daemon_features = crate::settings::DaemonFeatureEntry::defaults();
        for entry in app.daemon_features.iter_mut() {
            if entry.field == "stall_classifier_model" {
                entry.value = DaemonFeatureValue::Display("qwen2.5:7b".to_string());
            }
        }
        app.local_models = vec![
            ("qwen3:14b".to_string(), "qwen3:14b".to_string()),
            ("gemma4:26b".to_string(), "gemma4:26b".to_string()),
        ];
        app
    }

    fn test_app_for_classifier_row() -> App {
        test_app_for_classifier_row_at(std::path::PathBuf::from(
            "/tmp/test-settings-classifier-actor.sock",
        ))
    }

    async fn run_classifier_response_test(response: Option<serde_json::Value>) -> App {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let temp_dir = tempfile::tempdir().expect("temporary classifier socket directory");
        let socket_path = temp_dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("classifier listener");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("classifier client");
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let line = lines
                .next_line()
                .await
                .expect("classifier request read")
                .expect("classifier request line");
            let request: serde_json::Value =
                serde_json::from_str(&line).expect("classifier request JSON");
            assert_eq!(request["method"], "UpdateDaemonConfig");
            assert_eq!(request["params"]["field"], "stall_classifier_model");
            assert_eq!(request["params"]["value"], "qwen3:14b");
            if let Some(response) = response {
                let response = if response.get("jsonrpc").is_some() {
                    response
                } else {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"].clone(),
                        "result": response,
                    })
                };
                writer
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .expect("classifier response write");
            }
        });

        let mut app = test_app_for_classifier_row_at(socket_path);
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        open_classifier_dropdown(&mut app);
        assert!(
            handle_settings_key_event(
                &mut app,
                KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE)
            )
            .await
        );
        let entry = app
            .daemon_features
            .iter()
            .find(|entry| entry.field == "stall_classifier_model")
            .expect("classifier mirror while pending");
        assert!(matches!(
            &entry.value,
            DaemonFeatureValue::Display(value) if value == "qwen2.5:7b"
        ));
        assert!(app.settings_state.model_dropdown.open);
        assert!(app.classifier_config_pending.is_some());

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.classifier_config_rx.recv(),
        )
        .await
        .expect("classifier result deadline")
        .expect("classifier result");
        assert!(app.apply_classifier_config_result(result));
        server.await.expect("classifier server");
        app
    }

    fn open_classifier_dropdown(app: &mut App) {
        app.settings_state.active_dropdown_item = Some(5);
        app.settings_state.model_dropdown = crate::types::ModelDropdownState::new(
            SessionProvider::Local,
            app.local_models.clone(),
            Some("qwen2.5:7b"),
        );
    }

    #[test]
    fn test_agent_actors_dropdown_config_classifier_row_is_local_provider() {
        let app = test_app_for_classifier_row();
        let (provider, custom_idx, models, current) = agent_actors_dropdown_config(&app, 5);
        assert_eq!(provider, SessionProvider::Local);
        assert_eq!(custom_idx, None);
        assert_eq!(models, app.local_models);
        assert_eq!(current, Some("qwen2.5:7b".to_string()));
    }

    #[tokio::test]
    async fn test_classifier_selection_ready_path_accepts_and_closes_dropdown() {
        let app = run_classifier_response_test(Some(serde_json::Value::Null)).await;

        let entry = app
            .daemon_features
            .iter()
            .find(|e| e.field == "stall_classifier_model")
            .expect("stall_classifier_model entry present");
        match &entry.value {
            DaemonFeatureValue::Display(s) => assert_eq!(s, "qwen3:14b"),
            other => panic!("expected Display, got {:?}", other),
        }
        assert!(!app.settings_state.model_dropdown.open);
        assert_eq!(app.settings_state.active_dropdown_item, None);
    }

    #[tokio::test]
    async fn classifier_rpc_error_and_transport_drop_restore_prior_choice() {
        for response in [
            Some(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": {"code": -32055, "message": "classifier rejected exactly"},
            })),
            None,
        ] {
            let app = run_classifier_response_test(response).await;
            let entry = app
                .daemon_features
                .iter()
                .find(|entry| entry.field == "stall_classifier_model")
                .expect("classifier mirror after rejection");
            assert!(matches!(
                &entry.value,
                DaemonFeatureValue::Display(value) if value == "qwen2.5:7b"
            ));
            assert_eq!(app.settings_state.active_dropdown_item, Some(5));
            assert!(app.settings_state.model_dropdown.open);
            assert_eq!(
                app.settings_state.model_dropdown.models
                    [app.settings_state.model_dropdown.selected_index]
                    .0,
                "qwen3:14b",
                "the rejected selection remains highlighted for exact retry"
            );
            assert!(app.notifications.iter().any(|notification| {
                notification.message.contains("Classifier update failed:")
            }));
        }
    }

    #[tokio::test]
    async fn test_classifier_selection_ready_to_pending_preserves_open_dropdown() {
        let mut app = test_app_for_classifier_row();
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        open_classifier_dropdown(&mut app);
        app.poll.authoritative_config_ready = false;
        assert!(
            handle_settings_key_event(
                &mut app,
                KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE)
            )
            .await
        );

        let entry = app
            .daemon_features
            .iter()
            .find(|entry| entry.field == "stall_classifier_model")
            .expect("classifier entry");
        assert!(matches!(
            &entry.value,
            DaemonFeatureValue::Display(value) if value == "qwen2.5:7b"
        ));
        assert_eq!(app.settings_state.active_dropdown_item, Some(5));
        assert!(app.settings_state.model_dropdown.open);
        assert!(app.pending_lc_actions.is_empty());
    }

    #[tokio::test]
    async fn test_classifier_selection_ready_to_error_preserves_open_dropdown() {
        let mut app = test_app_for_classifier_row();
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        open_classifier_dropdown(&mut app);
        app.mark_daemon_config_error("temporary daemon error".to_string());
        assert!(
            handle_settings_key_event(
                &mut app,
                KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE)
            )
            .await
        );

        let entry = app
            .daemon_features
            .iter()
            .find(|entry| entry.field == "stall_classifier_model")
            .expect("classifier entry");
        assert!(matches!(
            &entry.value,
            DaemonFeatureValue::Display(value) if value == "qwen2.5:7b"
        ));
        assert_eq!(app.settings_state.active_dropdown_item, Some(5));
        assert!(app.settings_state.model_dropdown.open);
        assert!(app.pending_lc_actions.is_empty());
    }

    #[test]
    fn settlement_settings_row_queues_the_dedicated_overlay_action() {
        let mut app = test_app_for_classifier_row();
        app.settings_state.focus = SettingsFocus::Items;
        app.settings_state.section = SettingsSection::SandboxStorage;
        let rows = daemon_feature_rows_for_section(&app, SettingsSection::SandboxStorage);
        app.settings_state.selected_index = rows
            .iter()
            .position(|(spec, _)| {
                daemon_feature_field_for(spec.id) == Some("source_worktree_settlement")
            })
            .expect("settlement Settings row");

        assert!(handle_settings_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE)
        ));
        assert_eq!(
            app.pending_lc_actions.last(),
            Some(&LcAction::OpenSourceWorktreeSettlement)
        );
    }
}
