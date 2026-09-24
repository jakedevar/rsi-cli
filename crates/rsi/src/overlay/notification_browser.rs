//! Notification browser overlay input handling.

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent};

use super::list;

pub(super) fn handle_notification_browser_key(app: &mut App, key: KeyEvent) {
    // Build a combined list: active notifications + history (newest first)
    let active_len = app.notifications.len();
    let history_len = app.notification_history.len();
    let total = active_len + history_len;

    if total == 0 {
        // Empty state — any key closes
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter) {
            app.overlay = OverlayState::None;
        }
        return;
    }

    if let OverlayState::NotificationBrowser { selected_index, .. } = &mut app.overlay {
        if list::handle_list_nav_key(selected_index, total, &key) {
            return;
        }
    }

    match key.code {
        KeyCode::Char('x') => {
            // Dismiss the selected notification
            if let OverlayState::NotificationBrowser { selected_index, .. } = &mut app.overlay {
                let idx = *selected_index;
                if idx < active_len {
                    // Dismiss active notification -> move to history
                    if let Some(mut n) = app.notifications.remove(idx) {
                        n.dismissed = true;
                        app.notification_history.push(n);
                    }
                    // Adjust selection
                    let new_total = app.notifications.len() + app.notification_history.len();
                    if new_total == 0 {
                        app.overlay = OverlayState::None;
                        return;
                    }
                    *selected_index = idx.min(new_total.saturating_sub(1));
                }
                // History items can't be dismissed further
            }
        }
        KeyCode::Char('N') => {
            // Dismiss all active notifications
            let dismissed: Vec<_> = app.notifications.drain(..).collect();
            for mut n in dismissed {
                n.dismissed = true;
                app.notification_history.push(n);
            }
        }
        KeyCode::Enter => {
            // Navigate to source session if available
            let session_id =
                if let OverlayState::NotificationBrowser { selected_index, .. } = &app.overlay {
                    let idx = *selected_index;
                    if idx < active_len {
                        app.notifications.get(idx).and_then(|n| n.session_id)
                    } else {
                        let hist_idx = idx - active_len;
                        // History is ordered oldest-first; we display newest-first
                        let rev_idx = history_len.saturating_sub(1).saturating_sub(hist_idx);
                        app.notification_history
                            .get(rev_idx)
                            .and_then(|n| n.session_id)
                    }
                } else {
                    None
                };

            if let Some(sid) = session_id {
                app.overlay = OverlayState::None;
                app.open_session_in_current_pane(sid);
            }
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            app.overlay = OverlayState::None;
        }
        _ => {}
    }
}
