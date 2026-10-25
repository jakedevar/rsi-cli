//! Memory search overlay input handling.

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent};

/// Open the memory search overlay.
pub fn open_memory_search(app: &mut App) {
    app.overlay = OverlayState::MemorySearch {
        query: String::new(),
        results: Vec::new(),
        selected_index: 0,
        loading: false,
    };
}

/// Handle keys in the memory search overlay.
pub(super) async fn handle_memory_search_key(app: &mut App, key: KeyEvent) {
    // Standard editing: every printable key types, with a real cursor and
    // selection in the query (Vim keeps `q`, `j` and `k` as commands).
    match app.edit_field(key, |overlay| match overlay {
        OverlayState::MemorySearch { query, .. } => Some(query),
        _ => None,
    }) {
        crate::field_edit::FieldKey::Edited => {
            let query = match &app.overlay {
                OverlayState::MemorySearch { query, .. } => query.clone(),
                _ => return,
            };
            let results = if query.is_empty() {
                Vec::new()
            } else {
                app.client
                    .memory_search(&query, Some(10), None)
                    .await
                    .unwrap_or_default()
            };
            app.overlay = OverlayState::MemorySearch {
                query,
                results,
                selected_index: 0,
                loading: false,
            };
            return;
        }
        crate::field_edit::FieldKey::Moved => return,
        crate::field_edit::FieldKey::Ignored => {}
    }
    // The query is a type-to-filter field: every printable key (q, j, k, g, G
    // included) types into it in both editing modes. Navigation uses only
    // non-printable keys, and Esc closes.
    if let OverlayState::MemorySearch {
        results,
        selected_index,
        ..
    } = &mut app.overlay
        && super::list::handle_filter_list_nav_key(selected_index, results.len(), &key)
    {
        return;
    }
    match key.code {
        KeyCode::Esc => {
            app.overlay = OverlayState::None;
        }
        KeyCode::Backspace => {
            let new_query = if let OverlayState::MemorySearch { query, .. } = &app.overlay {
                let mut q = query.clone();
                q.pop();
                q
            } else {
                return;
            };
            let results = if new_query.is_empty() {
                Vec::new()
            } else {
                app.client
                    .memory_search(&new_query, Some(10), None)
                    .await
                    .unwrap_or_default()
            };
            app.overlay = OverlayState::MemorySearch {
                query: new_query,
                results,
                selected_index: 0,
                loading: false,
            };
        }
        KeyCode::Char(c) => {
            let new_query = if let OverlayState::MemorySearch { query, .. } = &app.overlay {
                let mut q = query.clone();
                q.push(c);
                q
            } else {
                return;
            };
            let results = app
                .client
                .memory_search(&new_query, Some(10), None)
                .await
                .unwrap_or_default();
            app.overlay = OverlayState::MemorySearch {
                selected_index: 0,
                loading: false,
                query: new_query,
                results,
            };
        }
        _ => {}
    }
}
