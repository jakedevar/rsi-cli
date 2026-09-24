//! Trash browser overlay — browse, restore, and permanently purge soft-deleted sessions.

use crate::app::App;
use crate::types::{ArchiveListItem, OverlayState};
use crossterm::event::{KeyCode, KeyEvent};

/// Open the trash browser overlay.
/// Fetches deleted sessions from daemon and groups by time period.
pub async fn open_trash_browser(app: &mut App) {
    let project_id = app.current_project_id;
    let sessions = match app.client.list_deleted_sessions(project_id).await {
        Ok(s) => s,
        Err(e) => {
            app.notify_error(format!("Failed to load trash: {}", e));
            return;
        }
    };

    if sessions.is_empty() {
        app.notify("Trash is empty");
        return;
    }

    let items = build_trash_list_items(&sessions);
    let selected_index = items
        .iter()
        .position(|item| matches!(item, ArchiveListItem::Session(_)))
        .unwrap_or(0);

    app.overlay = OverlayState::TrashBrowser {
        sessions,
        items,
        selected_index,
        scroll_offset: 0,
    };
}

/// Build a flattened display list with time-period section headers.
/// Sessions must already be sorted by updated_at DESC.
pub(super) fn build_trash_list_items(
    sessions: &[rsi_common::types::Session],
) -> Vec<ArchiveListItem> {
    use chrono::Utc;

    let now = Utc::now();
    let one_day_ago = now - chrono::Duration::days(1);
    let two_days_ago = now - chrono::Duration::days(2);
    let three_days_ago = now - chrono::Duration::days(3);
    let one_week_ago = now - chrono::Duration::days(7);
    let one_month_ago = now - chrono::Duration::days(30);

    let mut items = Vec::new();
    let mut current_group: Option<&str> = None;

    for (idx, session) in sessions.iter().enumerate() {
        let group = if session.updated_at >= one_day_ago {
            "Past Day"
        } else if session.updated_at >= two_days_ago {
            "Past 2 Days"
        } else if session.updated_at >= three_days_ago {
            "Past 3 Days"
        } else if session.updated_at >= one_week_ago {
            "Past Week"
        } else if session.updated_at >= one_month_ago {
            "Past Month"
        } else {
            "Older"
        };

        if current_group != Some(group) {
            items.push(ArchiveListItem::Header(group.to_string()));
            current_group = Some(group);
        }
        items.push(ArchiveListItem::Session(idx));
    }

    items
}

/// Handle keys in the trash browser overlay.
#[allow(clippy::needless_range_loop)]
pub(super) async fn handle_trash_browser_key(app: &mut App, key: KeyEvent) {
    let (items_len, _sessions_len) = match &app.overlay {
        OverlayState::TrashBrowser {
            items, sessions, ..
        } => (items.len(), sessions.len()),
        _ => return,
    };

    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            if let OverlayState::TrashBrowser {
                items,
                selected_index,
                ..
            } = &mut app.overlay
            {
                for i in (*selected_index + 1)..items_len {
                    if matches!(items[i], ArchiveListItem::Session(_)) {
                        *selected_index = i;
                        break;
                    }
                }
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let OverlayState::TrashBrowser {
                items,
                selected_index,
                ..
            } = &mut app.overlay
            {
                for i in (0..*selected_index).rev() {
                    if matches!(items[i], ArchiveListItem::Session(_)) {
                        *selected_index = i;
                        break;
                    }
                }
            }
        }
        KeyCode::Char('G') => {
            if let OverlayState::TrashBrowser {
                items,
                selected_index,
                ..
            } = &mut app.overlay
            {
                for i in (0..items_len).rev() {
                    if matches!(items[i], ArchiveListItem::Session(_)) {
                        *selected_index = i;
                        break;
                    }
                }
            }
        }
        KeyCode::Char('U') => {
            // Restore session to Completed
            let session_info = if let OverlayState::TrashBrowser {
                items,
                sessions,
                selected_index,
                ..
            } = &app.overlay
            {
                if let Some(ArchiveListItem::Session(sess_idx)) = items.get(*selected_index) {
                    Some((sessions[*sess_idx].id, *sess_idx))
                } else {
                    None
                }
            } else {
                None
            };

            if let Some((session_id, sess_idx)) = session_info {
                match app.client.undelete_session(session_id).await {
                    Ok(session) => {
                        app.sessions
                            .insert(session_id, crate::types::SessionState::new(session.clone()));
                        if !app.session_order.contains(&session_id) {
                            app.session_order.push(session_id);
                        }
                        app.recalculate_filtered_order();

                        if let OverlayState::TrashBrowser {
                            sessions,
                            items,
                            selected_index,
                            ..
                        } = &mut app.overlay
                        {
                            sessions.remove(sess_idx);
                            *items = build_trash_list_items(sessions);

                            if sessions.is_empty() {
                                app.overlay = OverlayState::None;
                                app.notify_success("Restored (trash is now empty)");
                                return;
                            }

                            let target = (*selected_index).min(items.len().saturating_sub(1));
                            *selected_index = (target..items.len())
                                .chain((0..target).rev())
                                .find(|&i| matches!(items[i], ArchiveListItem::Session(_)))
                                .unwrap_or(0);
                        }

                        let query_preview = if session.query.len() > 30 {
                            format!("{}...", &session.query[..30])
                        } else {
                            session.query.clone()
                        };
                        app.notify_success(format!("Restored: {}", query_preview));
                    }
                    Err(e) => {
                        app.notify_error(format!("Restore failed: {}", e));
                    }
                }
            }
        }
        KeyCode::Char('D') => {
            // Permanently purge session
            let session_info = if let OverlayState::TrashBrowser {
                items,
                sessions,
                selected_index,
                ..
            } = &app.overlay
            {
                if let Some(ArchiveListItem::Session(sess_idx)) = items.get(*selected_index) {
                    Some((
                        sessions[*sess_idx].id,
                        sessions[*sess_idx].query.clone(),
                        *sess_idx,
                    ))
                } else {
                    None
                }
            } else {
                None
            };

            if let Some((session_id, query, sess_idx)) = session_info {
                match app.client.purge_session(session_id).await {
                    Ok(()) => {
                        if let OverlayState::TrashBrowser {
                            sessions,
                            items,
                            selected_index,
                            ..
                        } = &mut app.overlay
                        {
                            sessions.remove(sess_idx);
                            *items = build_trash_list_items(sessions);

                            if sessions.is_empty() {
                                app.overlay = OverlayState::None;
                                app.notify_success("Purged (trash is now empty)");
                                return;
                            }

                            let target = (*selected_index).min(items.len().saturating_sub(1));
                            *selected_index = (target..items.len())
                                .chain((0..target).rev())
                                .find(|&i| matches!(items[i], ArchiveListItem::Session(_)))
                                .unwrap_or(0);
                        }

                        let query_preview = if query.len() > 30 {
                            format!("{}...", &query[..30])
                        } else {
                            query
                        };
                        app.notify_success(format!("Purged: {}", query_preview));
                    }
                    Err(e) => {
                        app.notify_error(format!("Purge failed: {}", e));
                    }
                }
            }
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            app.overlay = OverlayState::None;
        }
        _ => {} // Swallow
    }
}
