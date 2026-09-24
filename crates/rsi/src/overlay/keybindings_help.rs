//! Keybindings help overlay input handling.

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpView {
    Contextual,
    All,
}

/// Open the keybindings help overlay.
pub fn open_keybindings_help(app: &mut App) {
    open_contextual_help(app);
}

/// Open help for the exact current target, suspending target overlays.
pub fn open_contextual_help(app: &mut App) {
    open_help(app, HelpView::Contextual);
}

pub fn open_all_commands(app: &mut App) {
    open_help(app, HelpView::All);
}

fn open_help(app: &mut App, view: HelpView) {
    let origin = crate::action_registry::ActionContext::from_app(app).origin;
    // Suspend any active overlay intact. Its editor draft, selection, scroll,
    // and transient confirmation state remain owned by the stack until help
    // closes and restores it.
    if matches!(app.overlay, OverlayState::None) {
        // Keep a stack frame even when no regular overlay is visible, so
        // closing help cannot accidentally pop an older nested overlay.
        app.overlay_stack.push(OverlayState::None);
    } else {
        app.push_current_overlay();
    }
    app.overlay = OverlayState::KeybindingsHelp {
        view,
        scroll_offset: 0,
        filter: String::new(),
        search_active: false,
        origin,
    };
    app.mark_dirty();
}

/// Close the keybindings help overlay.
pub fn close_keybindings_help(app: &mut App) {
    if app.previous_overlay().is_some() {
        app.restore_previous_overlay();
    } else {
        app.overlay = OverlayState::None;
    }
    app.mark_dirty();
}

/// Handle keys in the keybindings help overlay.
pub(super) fn handle_keybindings_help_key(app: &mut App, key: KeyEvent) {
    let (search_active, has_filter) = match &app.overlay {
        OverlayState::KeybindingsHelp {
            search_active,
            filter,
            ..
        } => (*search_active, !filter.is_empty()),
        _ => return,
    };

    if search_active {
        // --- Search mode: text input for filter ---
        match key.code {
            KeyCode::Esc => {
                // Clear filter and exit search mode
                if let OverlayState::KeybindingsHelp {
                    filter,
                    search_active,
                    scroll_offset,
                    ..
                } = &mut app.overlay
                {
                    filter.clear();
                    *search_active = false;
                    *scroll_offset = 0;
                }
            }
            KeyCode::Enter => {
                // Accept filter and return to scroll mode
                if let OverlayState::KeybindingsHelp {
                    search_active,
                    scroll_offset,
                    ..
                } = &mut app.overlay
                {
                    *search_active = false;
                    *scroll_offset = 0;
                }
            }
            KeyCode::Backspace => {
                if let OverlayState::KeybindingsHelp {
                    filter,
                    scroll_offset,
                    ..
                } = &mut app.overlay
                {
                    filter.pop();
                    *scroll_offset = 0;
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let OverlayState::KeybindingsHelp {
                    filter,
                    scroll_offset,
                    ..
                } = &mut app.overlay
                {
                    filter.push(c);
                    *scroll_offset = 0;
                }
            }
            _ => {} // Ignore other keys in search mode
        }
    } else {
        // --- Scroll mode: vim navigation ---
        match key.code {
            KeyCode::Tab => {
                if let OverlayState::KeybindingsHelp {
                    view,
                    scroll_offset,
                    ..
                } = &mut app.overlay
                {
                    *view = match view {
                        HelpView::Contextual => HelpView::All,
                        HelpView::All => HelpView::Contextual,
                    };
                    *scroll_offset = 0;
                }
            }
            KeyCode::Char('m' | 'M')
                if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT =>
            {
                let mode = if key.code == KeyCode::Char('M') {
                    crate::manual::open::ManualMode::Pager
                } else {
                    crate::manual::open::ManualMode::Browser
                };
                app.pending_manual = Some(mode);
            }
            // Enter search mode
            KeyCode::Char('/') => {
                if let OverlayState::KeybindingsHelp {
                    search_active,
                    filter,
                    scroll_offset,
                    ..
                } = &mut app.overlay
                {
                    *search_active = true;
                    filter.clear();
                    *scroll_offset = 0;
                }
            }
            // Scroll down
            KeyCode::Char('j') | KeyCode::Down => {
                if let OverlayState::KeybindingsHelp { scroll_offset, .. } = &mut app.overlay {
                    *scroll_offset = scroll_offset.saturating_add(1);
                }
            }
            // Scroll up
            KeyCode::Char('k') | KeyCode::Up => {
                if let OverlayState::KeybindingsHelp { scroll_offset, .. } = &mut app.overlay {
                    *scroll_offset = scroll_offset.saturating_sub(1);
                }
            }
            // Page down (Ctrl+d / Ctrl+f)
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let OverlayState::KeybindingsHelp { scroll_offset, .. } = &mut app.overlay {
                    *scroll_offset = scroll_offset.saturating_add(15);
                }
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let OverlayState::KeybindingsHelp { scroll_offset, .. } = &mut app.overlay {
                    *scroll_offset = scroll_offset.saturating_add(30);
                }
            }
            // Page up (Ctrl+u / Ctrl+b)
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let OverlayState::KeybindingsHelp { scroll_offset, .. } = &mut app.overlay {
                    *scroll_offset = scroll_offset.saturating_sub(15);
                }
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let OverlayState::KeybindingsHelp { scroll_offset, .. } = &mut app.overlay {
                    *scroll_offset = scroll_offset.saturating_sub(30);
                }
            }
            // Jump to top (g or Home)
            KeyCode::Char('g') | KeyCode::Home => {
                if let OverlayState::KeybindingsHelp { scroll_offset, .. } = &mut app.overlay {
                    *scroll_offset = 0;
                }
            }
            // Jump to bottom (G or End)
            KeyCode::Char('G') | KeyCode::End => {
                if let OverlayState::KeybindingsHelp { scroll_offset, .. } = &mut app.overlay {
                    *scroll_offset = 9999;
                }
            }
            // Close overlay — if filter active, clear it first
            KeyCode::Esc => {
                if has_filter {
                    if let OverlayState::KeybindingsHelp {
                        filter,
                        scroll_offset,
                        ..
                    } = &mut app.overlay
                    {
                        filter.clear();
                        *scroll_offset = 0;
                    }
                } else {
                    close_keybindings_help(app);
                }
            }
            KeyCode::Char('q') | KeyCode::Char('?') => {
                close_keybindings_help(app);
            }
            _ => {} // Ignore other keys
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::app_test_helpers::with_session_list;

    #[test]
    fn tab_toggles_help_view() {
        let mut app = with_session_list(0);
        open_contextual_help(&mut app);
        handle_keybindings_help_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(
            app.overlay,
            OverlayState::KeybindingsHelp {
                view: HelpView::All,
                scroll_offset: 0,
                ..
            }
        ));
        handle_keybindings_help_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(
            app.overlay,
            OverlayState::KeybindingsHelp {
                view: HelpView::Contextual,
                ..
            }
        ));
    }

    #[test]
    #[allow(non_snake_case)]
    fn help_m_requests_manual_browser_and_M_requests_pager() {
        let mut app = with_session_list(0);
        open_contextual_help(&mut app);
        handle_keybindings_help_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
        );
        assert_eq!(
            app.pending_manual,
            Some(crate::manual::open::ManualMode::Browser)
        );
        app.pending_manual = None;
        handle_keybindings_help_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT),
        );
        assert_eq!(
            app.pending_manual,
            Some(crate::manual::open::ManualMode::Pager)
        );
    }

    #[test]
    fn contextual_help_preserves_exact_issue_pane_state() {
        let mut app = with_session_list(0);
        let pane_id = app.active_tab().focused_pane;
        let mut state = crate::types::IssueWorkspaceState::new(Some(uuid::Uuid::new_v4()));
        state.active_tab = crate::types::IssueWorkspaceTab::Dispatched;
        state.dispatched.selected_row = 3;
        state.dispatched.scroll_offset = 7;
        state.transient.pending_yank = true;
        state.transient.pending_jump = true;
        *app.active_tab_mut()
            .layout
            .find_pane_mut(pane_id)
            .expect("focused pane") = crate::types::Pane::Issues(state);
        open_contextual_help(&mut app);
        assert!(matches!(app.overlay, OverlayState::KeybindingsHelp { .. }));
        close_keybindings_help(&mut app);
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(crate::types::Pane::Issues(state))
                if state.active_tab == crate::types::IssueWorkspaceTab::Dispatched
                    && state.dispatched.selected_row == 3
                    && state.dispatched.scroll_offset == 7
                    && state.transient.pending_yank
                    && state.transient.pending_jump
        ));
    }

    #[test]
    fn contextual_help_suspends_and_restores_an_unsaved_overlay_editor() {
        let mut app = with_session_list(0);
        app.overlay = OverlayState::ThemeRoleEditor {
            role: crate::ui::theme_roles::ThemeRole::Accent,
            input: "#123456".to_string(),
            opening_overrides: Vec::new(),
            assessment: None,
            committed: false,
            pending_acknowledgement: None,
        };
        open_contextual_help(&mut app);
        assert!(matches!(
            app.overlay,
            OverlayState::KeybindingsHelp {
                origin: crate::action_registry::HelpOrigin::ThemeRoleEditor,
                ..
            }
        ));
        close_keybindings_help(&mut app);
        assert!(matches!(
            &app.overlay,
            OverlayState::ThemeRoleEditor {
                input,
                committed: false,
                ..
            } if input == "#123456"
        ));
    }

    #[test]
    fn contextual_help_preserves_an_existing_nested_overlay_stack() {
        let mut app = with_session_list(0);
        app.overlay_stack
            .push(OverlayState::SortPicker { selected_index: 5 });
        open_contextual_help(&mut app);
        close_keybindings_help(&mut app);
        assert!(matches!(app.overlay, OverlayState::None));
        assert!(matches!(
            app.overlay_stack.as_slice(),
            [OverlayState::SortPicker { selected_index: 5 }]
        ));
    }

    #[test]
    fn question_mark_remains_filter_text_during_help_search() {
        let mut app = with_session_list(0);
        open_contextual_help(&mut app);
        if let OverlayState::KeybindingsHelp { search_active, .. } = &mut app.overlay {
            *search_active = true;
        }
        handle_keybindings_help_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
        );
        assert!(matches!(
            app.overlay,
            OverlayState::KeybindingsHelp { ref filter, .. } if filter == "?"
        ));
    }

    #[test]
    fn escape_clears_accepted_filter_before_closing_help() {
        let mut app = with_session_list(0);
        open_contextual_help(&mut app);
        if let OverlayState::KeybindingsHelp { filter, .. } = &mut app.overlay {
            *filter = "refresh".to_string();
        }
        handle_keybindings_help_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(
            app.overlay,
            OverlayState::KeybindingsHelp { ref filter, .. } if filter.is_empty()
        ));
        handle_keybindings_help_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.overlay, OverlayState::None));
    }
}
