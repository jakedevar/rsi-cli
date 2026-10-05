//! Window / layout action handlers.
//!
//! Covers: pane splits, tab navigation, input bar focus, settings,
//! session-in-new-tab, navigate-right, and quit.

use crate::app::App;
use crate::modalkit_types::LcAction;
use crate::types::Pane;

pub(super) fn dispatch(app: &mut App, action: LcAction) {
    match action {
        LcAction::OpenSessionInNewTab => {
            if let Some(session_id) = app.selected_session_id() {
                app.open_session_in_new_tab(session_id);
            }
        }

        LcAction::NavigateRight => {
            if let Some(Pane::SessionList {
                selected_session: Some(_),
                ..
            }) = app.focused_pane().cloned()
            {
                app.enter_session();
            }
        }

        LcAction::NextTab => {
            app.next_tab();
        }

        LcAction::PrevTab => {
            app.prev_tab();
        }

        LcAction::EnterInputBarInsert(style) => match app.focused_pane().cloned() {
            Some(Pane::SessionDetail { session_id }) => {
                crate::input_bar::enter_insert_mode(app, session_id, style);
            }
            Some(Pane::SessionList {
                selected_session, ..
            }) => {
                // Prefer the selected session in the list; fall back to last viewed.
                let target = selected_session.or(app.last_viewed_session);
                if let Some(session_id) = target {
                    app.open_session_in_current_pane(session_id);
                    crate::input_bar::enter_insert_mode(app, session_id, style);
                } else {
                    app.notify("No session selected (use j/k to select)");
                }
            }
            Some(Pane::Settings) | Some(Pane::PromptCreator) | Some(Pane::Issues(_)) | None => {}
        },

        LcAction::EnterSessionNormalMode => match app.focused_pane().cloned() {
            Some(Pane::SessionDetail { .. }) => {
                // Already in session detail, stay in normal mode
            }
            Some(Pane::SessionList {
                selected_session: Some(_),
                ..
            }) => {
                app.enter_session();
            }
            Some(Pane::SessionList {
                selected_session: None,
                ..
            }) => {
                app.notify("No session selected (use j/k to select)");
            }
            Some(Pane::Settings) | Some(Pane::PromptCreator) | Some(Pane::Issues(_)) | None => {}
        },

        LcAction::OpenSettings => {
            let tab = &mut app.tabs[app.active_tab];
            let focused = tab.focused_pane;
            if let Some(pane) = tab.layout.find_pane_mut(focused) {
                if matches!(pane, Pane::Settings) {
                    crate::settings_keys::close_settings(app);
                } else {
                    app.pre_settings_pane = Some(pane.clone());
                    *pane = Pane::Settings;
                    crate::settings_keys::reopen_settings(app);
                }
            }
        }

        LcAction::OpenSettingsAt(section) => {
            let tab = &mut app.tabs[app.active_tab];
            let focused = tab.focused_pane;
            if let Some(pane) = tab.layout.find_pane_mut(focused) {
                if !matches!(pane, Pane::Settings) {
                    app.pre_settings_pane = Some(pane.clone());
                    *pane = Pane::Settings;
                }
            }
            // Land in Items focus on the requested section so the user can
            // immediately use `a / Enter / d / e` chords.
            app.settings_state.clear_transient();
            crate::settings_keys::enter_remembered_settings_section(app, section);
        }

        LcAction::OpenPromptCreator => {
            let tab = &mut app.tabs[app.active_tab];
            let focused = tab.focused_pane;
            if let Some(pane) = tab.layout.find_pane_mut(focused) {
                if matches!(pane, Pane::PromptCreator) {
                    crate::prompt_creator_keys::close_prompt_creator(app);
                } else {
                    app.pre_prompt_creator_pane = Some(pane.clone());
                    *pane = Pane::PromptCreator;
                    app.refresh_prompts();
                    app.prompt_creator_state.editing = false;
                    app.prompt_creator_viewer = None;
                    // Initialize model dropdown
                    let models = crate::app::models_for_provider(app.selected_provider.clone());
                    app.prompt_creator_state.model_dropdown =
                        Some(crate::types::ModelDropdownState::closed(
                            app.selected_provider.clone(),
                            models,
                            app.selected_model.as_deref(),
                        ));
                }
            }
        }

        LcAction::CloseFocusedPane => {
            app.close_focused_pane();
        }

        LcAction::Quit => {
            app.quit = true;
        }

        LcAction::GrowSidebar => {
            let term_width = crate::ui::terminal_width_for_layout(app);
            let tab = &mut app.tabs[app.active_tab];
            if tab.session_list_width_pct == 0 {
                tab.session_list_width_pct = legacy_sidebar_pct(tab, term_width);
            }
            // Cap at the render clamp's ceiling (not a higher off-screen value)
            // so a reversed drag moves the divider on the very next keypress.
            tab.session_list_width_pct =
                (tab.session_list_width_pct + 3).min(crate::ui::SIDEBAR_MAX_PCT);
            app.pane_switch_clear = true;
        }

        LcAction::ShrinkSidebar => {
            let term_width = crate::ui::terminal_width_for_layout(app);
            let tab = &mut app.tabs[app.active_tab];

            if tab.session_list_width_pct == 0 {
                tab.session_list_width_pct = legacy_sidebar_pct(tab, term_width);
            }

            // Reduce by 3 percentage points, floored at the render clamp's lower
            // bound — mirrors the grow ceiling above so the stored width always
            // equals what's on screen and the divider tracks every keypress.
            tab.session_list_width_pct = tab
                .session_list_width_pct
                .saturating_sub(3)
                .max(crate::ui::SIDEBAR_MIN_PCT);
            app.pane_switch_clear = true;
        }

        _ => unreachable!("window::dispatch called with non-window action"),
    }
}

fn legacy_sidebar_pct(tab: &crate::types::Tab, term_width: u16) -> u16 {
    let term_width = term_width.max(1);
    let effective_x_off = crate::ui::effective_layout_x_offset(tab);
    let centering = if term_width > 80 {
        (term_width - 80) / 2
    } else {
        0
    };
    let current_sidebar_cols = centering + effective_x_off;
    let raw_pct = ((current_sidebar_cols as u32 * 100) / term_width as u32) as u16;
    // Clamp the seeded width into the same range the renderer honors, so the
    // very first grow/shrink can never start from an off-screen (>ceiling)
    // value that would swallow a reversing keypress.
    raw_pct.clamp(crate::ui::SIDEBAR_MIN_PCT, crate::ui::SIDEBAR_MAX_PCT)
}

/// Check whether the session list sidebar should auto-expand after navigation.
///
/// When the sidebar is in percentage-based mode at ≤20 % of the viewport and the
/// right edge of the sidebar area exceeds the viewport right edge, expand by 10 %
/// of the viewport width (up to a maximum of 50 %).
pub(crate) fn maybe_auto_expand_session_list(app: &mut App) {
    let tab = &app.tabs[app.active_tab];

    // Only active in percentage mode, and only at the minimum threshold.
    if tab.session_list_width_pct == 0 || tab.session_list_width_pct > 20 {
        return;
    }

    let term_width = crate::ui::terminal_width_for_layout(app);
    let sidebar_cols = (term_width as u32 * tab.session_list_width_pct as u32 / 100) as u16;

    // Check if the sidebar's right edge extends beyond the viewport's right edge.
    if sidebar_cols > term_width {
        let tab = &mut app.tabs[app.active_tab];
        tab.session_list_width_pct = (tab.session_list_width_pct + 10).min(50);
        app.pane_switch_clear = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_keys::handle_settings_key;
    use crate::types::{SettingsFocus, SettingsSection};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn settings_reopen_remembers_location_for_q_escape_and_toggle() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.settings_state.select_section(SettingsSection::Screen);
        app.settings_state.selected_index = 1;
        app.settings_state.focus = SettingsFocus::Items;
        dispatch(&mut app, LcAction::OpenSettings);
        for close_key in [Some(KeyCode::Char('q')), Some(KeyCode::Esc), None] {
            if let Some(code) = close_key {
                assert!(handle_settings_key(
                    &mut app,
                    KeyEvent::new(code, KeyModifiers::NONE)
                ));
            } else {
                dispatch(&mut app, LcAction::OpenSettings);
            }
            assert!(matches!(app.focused_pane(), Some(Pane::SessionList { .. })));
            dispatch(&mut app, LcAction::OpenSettings);
            assert!(matches!(app.focused_pane(), Some(Pane::Settings)));
            assert_eq!(app.settings_state.section, SettingsSection::Screen);
            assert_eq!(app.settings_state.selected_index, 1);
            assert_eq!(app.settings_state.focus, SettingsFocus::Items);
        }
    }

    #[test]
    fn settings_direct_link_restores_target_row_and_refreshes_target_data() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.settings_state
            .section_rows
            .insert(SettingsSection::Budgets, 4);
        app.settings_state
            .section_rows
            .insert(SettingsSection::Screen, 1);
        dispatch(&mut app, LcAction::OpenSettingsAt(SettingsSection::Budgets));
        assert_eq!(app.settings_state.section, SettingsSection::Budgets);
        assert_eq!(app.settings_state.selected_index, 4);
        assert_eq!(app.settings_state.focus, SettingsFocus::Items);
        assert!(
            app.pending_lc_actions
                .contains(&LcAction::RefreshUsageStats)
        );
        dispatch(&mut app, LcAction::OpenSettingsAt(SettingsSection::Screen));
        assert_eq!(app.settings_state.selected_index, 1);
    }

    #[test]
    fn settings_reopen_clears_transient_input_and_preserves_category_focus() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.settings_state.section = SettingsSection::ProviderKeys;
        app.settings_state.selected_index = 2;
        app.settings_state.focus = SettingsFocus::Categories;
        app.settings_state.query = "unfinished search".into();
        app.settings_state.query_active = true;
        app.settings_state.model_dropdown.open = true;
        app.settings_state.active_dropdown_item = Some(2);
        app.settings_state.provider_key_clear_confirmation =
            Some(crate::types::ProviderKeyClearConfirmation {
                slot: rsi_common::provider_credentials::ProviderCredentialSlot::ALL[2],
                armed_at_ms: chrono::Utc::now().timestamp_millis(),
            });
        dispatch(&mut app, LcAction::OpenSettings);
        assert_eq!(app.settings_state.section, SettingsSection::ProviderKeys);
        assert_eq!(app.settings_state.selected_index, 2);
        assert_eq!(app.settings_state.focus, SettingsFocus::Categories);
        assert!(app.settings_state.query.is_empty());
        assert!(!app.settings_state.query_active);
        assert!(!app.settings_state.model_dropdown.open);
        assert_eq!(app.settings_state.active_dropdown_item, None);
        assert!(app.settings_state.provider_key_clear_confirmation.is_none());
        assert!(
            app.pending_lc_actions
                .contains(&LcAction::RefreshProviderCredentials)
        );
    }

    #[test]
    fn settings_location_survives_app_restart() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        dispatch(&mut app, LcAction::OpenSettingsAt(SettingsSection::Screen));
        app.settings_state.selected_index = 1;
        dispatch(&mut app, LcAction::OpenSettings);
        let mut restarted = App::new(crate::client::DaemonClient::new(std::path::PathBuf::from(
            "/tmp/settings-restart.sock",
        )));
        dispatch(&mut restarted, LcAction::OpenSettings);
        assert_eq!(restarted.settings_state.section, SettingsSection::Screen);
        assert_eq!(restarted.settings_state.selected_index, 1);
        assert_eq!(restarted.settings_state.focus, SettingsFocus::Items);
    }
}
