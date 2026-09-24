//! Sort picker overlay input handling.

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent};

use super::list;

/// Open the sort order picker overlay, pre-selecting the current sort.
pub fn open_sort_picker(app: &mut App) {
    use crate::app::SortOrder;
    let selected_index = SortOrder::ALL
        .iter()
        .position(|&o| o == app.settings.sort_order)
        .unwrap_or(0);
    app.overlay = OverlayState::SortPicker { selected_index };
}

/// Close the sort picker and apply the selected sort order.
pub fn close_sort_picker(app: &mut App) {
    use crate::app::SortOrder;
    let selected_index = match &app.overlay {
        OverlayState::SortPicker { selected_index } => *selected_index,
        _ => return,
    };
    app.overlay = OverlayState::None;
    if let Some(&order) = SortOrder::ALL.get(selected_index) {
        app.set_sort_order(order);
    }
}

/// Handle keys in the sort picker overlay.
pub(super) fn handle_sort_picker_key(app: &mut App, key: KeyEvent) {
    use crate::app::SortOrder;
    let len = SortOrder::ALL.len();

    if let OverlayState::SortPicker { selected_index } = &mut app.overlay {
        if list::handle_list_nav_key(selected_index, len, &key) {
            return;
        }
    }

    match key.code {
        KeyCode::Enter => {
            close_sort_picker(app);
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            app.overlay = OverlayState::None;
        }
        _ => {}
    }
}
