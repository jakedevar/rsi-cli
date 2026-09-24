//! Session rating overlay input handling — 1–10 digit selection.

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent};

/// Open the rating overlay for the currently focused/selected session,
/// pre-seeding the picker with the session's existing rating (if any).
pub fn open_rating_overlay(app: &mut App) {
    let session_id = match app.selected_session_id() {
        Some(id) => id,
        None => {
            app.notify("No session selected");
            return;
        }
    };
    let selected_rating = app
        .sessions
        .get(&session_id)
        .and_then(|s| s.session.rating)
        .map(|r| r.clamp(1, 10) as u32);
    app.overlay = OverlayState::RatingOverlay {
        session_id,
        selected_rating,
    };
    app.mark_dirty();
}

/// Handle keys in the rating overlay.
///
/// Digits 1–9 select that rating; `0` selects 10. `h`/Left and `l`/Right
/// nudge the selection (clamped 1–10). Enter confirms and submits the
/// rating via RPC. Esc cancels without persisting.
pub(super) async fn handle_rating_overlay_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char(c @ '1'..='9') => {
            if let OverlayState::RatingOverlay {
                selected_rating, ..
            } = &mut app.overlay
            {
                *selected_rating = Some((c as u32) - ('0' as u32));
                app.mark_dirty();
            }
        }
        KeyCode::Char('0') => {
            if let OverlayState::RatingOverlay {
                selected_rating, ..
            } = &mut app.overlay
            {
                *selected_rating = Some(10);
                app.mark_dirty();
            }
        }
        KeyCode::Char('h') | KeyCode::Left => {
            if let OverlayState::RatingOverlay {
                selected_rating, ..
            } = &mut app.overlay
            {
                *selected_rating = Some(match *selected_rating {
                    Some(n) if n > 1 => n - 1,
                    Some(_) => 1,
                    None => 1,
                });
                app.mark_dirty();
            }
        }
        KeyCode::Char('l') | KeyCode::Right => {
            if let OverlayState::RatingOverlay {
                selected_rating, ..
            } = &mut app.overlay
            {
                *selected_rating = Some(match *selected_rating {
                    Some(n) if n < 10 => n + 1,
                    Some(_) => 10,
                    None => 1,
                });
                app.mark_dirty();
            }
        }
        KeyCode::Enter => {
            let (session_id, rating) = match &app.overlay {
                OverlayState::RatingOverlay {
                    session_id,
                    selected_rating: Some(r),
                } => (*session_id, *r),
                _ => return,
            };
            app.overlay = OverlayState::None;
            if let Err(e) = app
                .client
                .update_session_rating(session_id, Some(rating as i16))
                .await
            {
                app.notify_error(format!("Rating failed: {}", e));
            } else {
                if let Some(state) = app.sessions.get_mut(&session_id) {
                    state.session.rating = Some(rating as i16);
                }
                app.notify(format!("Rated {}/10", rating));
            }
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            app.overlay = OverlayState::None;
            app.mark_dirty();
        }
        _ => {}
    }
}
