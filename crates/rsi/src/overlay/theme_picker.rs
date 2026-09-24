//! Theme picker overlay input handling.

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent};

use super::list;

/// Open the theme picker overlay, pre-selecting the active flavor.
pub fn open_theme_picker(app: &mut App) {
    let current = crate::ui::theme::active_theme_index();
    app.overlay = OverlayState::ThemePicker {
        selected_index: current,
        original_index: current,
    };
}

/// Handle keys in the theme picker overlay.
pub(super) fn handle_theme_picker_key(app: &mut App, key: KeyEvent) {
    let theme_count = crate::ui::theme::theme_count();

    // Shared list navigation (j/k/g/G) with live theme preview
    if let OverlayState::ThemePicker { selected_index, .. } = &mut app.overlay {
        if list::handle_list_nav_key(selected_index, theme_count, &key) {
            apply_theme_selection(*selected_index);
            return;
        }
    }

    match key.code {
        KeyCode::Enter => {
            let index = match &app.overlay {
                OverlayState::ThemePicker { selected_index, .. } => *selected_index,
                _ => return,
            };
            apply_theme_selection(index);
            app.overlay = OverlayState::None;
        }
        KeyCode::Char(c @ '1'..='9') => {
            let index = (c as u8 - b'1') as usize;
            if index < theme_count {
                apply_theme_selection(index);
                app.overlay = OverlayState::None;
            }
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            let original_index = match &app.overlay {
                OverlayState::ThemePicker { original_index, .. } => *original_index,
                _ => return,
            };
            apply_theme_selection(original_index);
            app.overlay = OverlayState::None;
        }
        _ => {}
    }
}

fn apply_theme_selection(index: usize) {
    crate::ui::theme::set_theme_by_index(index);
}
