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
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.overlay = OverlayState::None;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if let OverlayState::MemorySearch {
                results,
                selected_index,
                ..
            } = &mut app.overlay
            {
                if *selected_index + 1 < results.len() {
                    *selected_index += 1;
                }
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let OverlayState::MemorySearch { selected_index, .. } = &mut app.overlay {
                *selected_index = selected_index.saturating_sub(1);
            }
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
