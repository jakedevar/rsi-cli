//! Notification browser overlay input handling.

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent};

use super::list;

pub(super) async fn handle_notification_browser_key(app: &mut App, key: KeyEvent) {
    // Both sections display newest first. Keep command indexing in that order.
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
        *selected_index = (*selected_index).min(total - 1);
        if list::handle_list_nav_key(selected_index, total, &key) {
            return;
        }
    }

    match key.code {
        KeyCode::Char('x') => {
            // Dismiss the selected notification
            if let OverlayState::NotificationBrowser { selected_index, .. } = &mut app.overlay {
                let display_idx = *selected_index;
                if display_idx < active_len {
                    // Dismiss active notification -> move to history
                    let queue_idx = active_len - 1 - display_idx;
                    if let Some(mut n) = app.notifications.remove(queue_idx) {
                        n.dismissed = true;
                        app.notification_history.push(n);
                    }
                    *selected_index = display_idx
                        .min(app.notifications.len() + app.notification_history.len() - 1);
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
            if let OverlayState::NotificationBrowser { selected_index, .. } = &mut app.overlay {
                *selected_index = 0;
            }
        }
        KeyCode::Enter => {
            // Navigate to source session if available
            let session_id =
                if let OverlayState::NotificationBrowser { selected_index, .. } = &app.overlay {
                    app.notifications
                        .iter()
                        .rev()
                        .chain(app.notification_history.iter().rev())
                        .nth(*selected_index)
                        .and_then(|n| n.session_id)
                } else {
                    None
                };

            if let Some(sid) = session_id {
                if !app.sessions.contains_key(&sid) {
                    match app.client.get_session(sid).await {
                        Ok(session) => {
                            app.upsert_session(session.clone());
                            // Archived sessions are excluded from the visible roster, but
                            // their notification links still open a detail pane.
                            if !app.sessions.contains_key(&sid) {
                                app.sessions
                                    .insert(sid, crate::types::SessionState::new(session));
                            }
                        }
                        Err(error) => {
                            app.notify_error(format!("Session unavailable: {error}"));
                            return;
                        }
                    }
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DaemonClient;
    use crate::types::{NotificationKind, NotificationPriority, Pane};
    use crossterm::event::KeyModifiers;
    use rsi_common::types::{SessionKind, SessionStatus};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn fixture() -> (App, uuid::Uuid, uuid::Uuid) {
        let mut app = crate::app::app_test_helpers::with_session_list(2);
        let ids: Vec<_> = app.sessions.keys().copied().collect();
        app.push_notification(
            NotificationKind::Info,
            NotificationPriority::Medium,
            "older".into(),
            Some(ids[0]),
        );
        app.push_notification(
            NotificationKind::Info,
            NotificationPriority::Medium,
            "newer".into(),
            Some(ids[1]),
        );
        app.overlay = OverlayState::NotificationBrowser {
            selected_index: 0,
            scroll_offset: 0,
        };
        (app, ids[0], ids[1])
    }

    #[tokio::test]
    async fn enter_opens_newest_active_notice_session() {
        let (mut app, _, newer) = fixture();
        handle_notification_browser_key(&mut app, key(KeyCode::Enter)).await;
        assert!(matches!(app.overlay, OverlayState::None));
        assert!(
            matches!(app.focused_pane(), Some(Pane::SessionDetail { session_id }) if *session_id == newer)
        );
    }

    #[tokio::test]
    async fn dismiss_and_enter_use_same_display_order_for_history() {
        let (mut app, older, newer) = fixture();
        handle_notification_browser_key(&mut app, key(KeyCode::Char('x'))).await;
        assert_eq!(app.notifications.len(), 1);
        assert_eq!(app.notifications[0].session_id, Some(older));
        assert_eq!(app.notification_history[0].session_id, Some(newer));
        handle_notification_browser_key(&mut app, key(KeyCode::Char('j'))).await;
        handle_notification_browser_key(&mut app, key(KeyCode::Enter)).await;
        assert!(
            matches!(app.focused_pane(), Some(Pane::SessionDetail { session_id }) if *session_id == newer)
        );
    }

    #[tokio::test]
    async fn dismiss_all_keeps_newest_history_notice_selected() {
        let (mut app, _, newer) = fixture();
        handle_notification_browser_key(&mut app, key(KeyCode::Char('N'))).await;
        assert!(app.notifications.is_empty());
        assert!(matches!(
            app.overlay,
            OverlayState::NotificationBrowser {
                selected_index: 0,
                ..
            }
        ));
        handle_notification_browser_key(&mut app, key(KeyCode::Enter)).await;
        assert!(
            matches!(app.focused_pane(), Some(Pane::SessionDetail { session_id }) if *session_id == newer)
        );
    }

    #[tokio::test]
    async fn enter_fetches_linked_session_missing_from_local_cache() {
        let temp_dir = tempfile::tempdir().unwrap();
        let socket_path = temp_dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let id = uuid::Uuid::new_v4();
        let mut session = crate::app::app_test_helpers::baseline_session(id, SessionKind::Standard);
        session.status = SessionStatus::Archived;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let line = lines.next_line().await.unwrap().unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "GetSession");
            assert_eq!(request["params"]["session_id"], id.to_string());
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"].clone(),
                "result": session,
            });
            writer
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        });

        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.client = DaemonClient::new(socket_path);
        app.client.connect().await.unwrap();
        app.push_notification(
            NotificationKind::Info,
            NotificationPriority::Medium,
            "archived".into(),
            Some(id),
        );
        app.overlay = OverlayState::NotificationBrowser {
            selected_index: 0,
            scroll_offset: 0,
        };
        handle_notification_browser_key(&mut app, key(KeyCode::Enter)).await;
        server.await.unwrap();

        assert!(app.sessions.contains_key(&id));
        assert!(
            matches!(app.focused_pane(), Some(Pane::SessionDetail { session_id }) if *session_id == id)
        );
    }
}
