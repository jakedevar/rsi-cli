//! The operator to-do pop-up (#2950): the selected item in full, with vim keys.
//!
//! Opened by `<Space>dv` (view) and, on its y/n prompt, by `<Space>dd` (done)
//! and `<Space>dx` (dismiss): a close never happens without the prompt, and a
//! close stays undoable (`<Space>du` or `u` here) for the undo window.
//!
//! Keys (item view): `y` yank the `how`, `d` done?, `x` dismiss?, `o` open the
//! link, `u` undo the last close, `n`/`p` next/previous item, `j`/`k` scroll,
//! `Esc`/`q` close. Prompt: `y`/`Enter` confirm, `n`/`Esc` back.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rsi_common::operator_todos::OperatorTodoStatusV1;

use crate::app::App;
use crate::todo_panel::TodoPopupState;
use crate::types::OverlayState;

/// Open the pop-up over the panel's selected item, on the y/n prompt for
/// `confirm` when given.
pub fn open_todo_popup(app: &mut App, confirm: Option<OperatorTodoStatusV1>) {
    app.overlay = OverlayState::TodoPopup(TodoPopupState { scroll: 0, confirm });
    app.mark_dirty();
}

fn state_mut(app: &mut App) -> Option<&mut TodoPopupState> {
    match &mut app.overlay {
        OverlayState::TodoPopup(state) => Some(state),
        _ => None,
    }
}

/// Handle a key inside the to-do pop-up.
pub(super) async fn handle_todo_popup_key(app: &mut App, key: KeyEvent) {
    let Some(confirm) = state_mut(app).map(|state| state.confirm) else {
        return;
    };
    if let Some(status) = confirm {
        handle_confirm_key(app, key, status);
        return;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('y') if !ctrl => app.todo_copy_selected(),
        KeyCode::Char('d') if !ctrl => ask(app, OperatorTodoStatusV1::Done),
        KeyCode::Char('x') if !ctrl => ask(app, OperatorTodoStatusV1::Dismissed),
        KeyCode::Char('u') if !ctrl => app.todo_undo(),
        KeyCode::Char('o') if !ctrl => {
            app.overlay = OverlayState::None;
            app.todo_open_link().await;
        }
        KeyCode::Char('n') if !ctrl => step(app, 1),
        KeyCode::Char('p') if !ctrl => step(app, -1),
        KeyCode::Char('j') | KeyCode::Down => scroll(app, 1),
        KeyCode::Char('k') | KeyCode::Up => scroll(app, -1),
        KeyCode::Char('d') | KeyCode::PageDown => scroll(app, 10),
        KeyCode::Char('u') | KeyCode::PageUp => scroll(app, -10),
        KeyCode::Char('g') | KeyCode::Home => {
            if let Some(state) = state_mut(app) {
                state.scroll = 0;
            }
        }
        KeyCode::Char('q') | KeyCode::Esc => app.overlay = OverlayState::None,
        _ => {}
    }
    app.mark_dirty();
}

/// Ask before closing the selected item.
fn ask(app: &mut App, status: OperatorTodoStatusV1) {
    if app.todo_panel.selected().is_none() {
        app.notify("No to-do item selected");
        return;
    }
    if let Some(state) = state_mut(app) {
        state.confirm = Some(status);
    }
}

fn handle_confirm_key(app: &mut App, key: KeyEvent, status: OperatorTodoStatusV1) {
    match key.code {
        KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
            app.overlay = OverlayState::None;
            app.todo_close_selected(status);
        }
        KeyCode::Char('n' | 'N' | 'q') | KeyCode::Esc => {
            if let Some(state) = state_mut(app) {
                state.confirm = None;
            }
        }
        _ => {}
    }
    app.mark_dirty();
}

/// Show the neighbouring item.
fn step(app: &mut App, delta: isize) {
    if delta >= 0 {
        app.todo_panel.select_next();
    } else {
        app.todo_panel.select_prev();
    }
    if let Some(state) = state_mut(app) {
        state.scroll = 0;
    }
}

fn scroll(app: &mut App, delta: isize) {
    if let Some(state) = state_mut(app) {
        state.scroll = state.scroll.saturating_add_signed(delta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::app_test_helpers::with_session_detail;
    use chrono::Utc;
    use rsi_common::operator_todos::{OperatorTodoListResultV1, OperatorTodoV1};
    use uuid::Uuid;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn item(title: &str, how: &str) -> OperatorTodoV1 {
        OperatorTodoV1 {
            id: Uuid::new_v4(),
            scope: "portfolio:global".into(),
            project_id: None,
            todo_key: title.into(),
            title: title.into(),
            how: how.into(),
            link: None,
            priority: 2,
            added_by: "session:abc".into(),
            status: OperatorTodoStatusV1::Open,
            evidence: None,
            evidence_observed: false,
            closed_by: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            closed_at: None,
        }
    }

    fn app_with(items: Vec<OperatorTodoV1>) -> App {
        let (mut app, _) = with_session_detail();
        app.enter_session();
        app.todo_panel.open = true;
        app.todo_panel.apply(OperatorTodoListResultV1 {
            open: items,
            ..OperatorTodoListResultV1::default()
        });
        app
    }

    fn popup(app: &App) -> TodoPopupState {
        match &app.overlay {
            OverlayState::TodoPopup(state) => state.clone(),
            _ => panic!("the to-do pop-up is not open"),
        }
    }

    #[tokio::test]
    async fn the_popup_opens_on_the_selected_item_and_closes_with_escape() {
        let mut app = app_with(vec![item("a", "do a")]);
        app.todo_open_popup();
        assert_eq!(popup(&app), TodoPopupState::default());
        handle_todo_popup_key(&mut app, KeyEvent::from(KeyCode::Esc)).await;
        assert!(matches!(app.overlay, OverlayState::None));
        assert_eq!(app.todo_panel.open_count(), 1);
    }

    #[tokio::test]
    async fn the_popup_needs_a_selected_item() {
        let mut app = app_with(vec![]);
        app.todo_open_popup();
        assert!(matches!(app.overlay, OverlayState::None));
        assert!(
            app.notifications
                .iter()
                .any(|n| n.message.contains("No to-do item selected"))
        );
    }

    #[tokio::test]
    async fn y_in_the_popup_yanks_the_how_and_closes_nothing() {
        let mut app = app_with(vec![item("restore", "rsi grant restore --caps  v8")]);
        app.todo_open_popup();
        handle_todo_popup_key(&mut app, key('y')).await;
        assert!(app.notifications.iter().any(|n| {
            n.message
                .contains("Copied to-do command: rsi grant restore --caps  v8")
        }));
        assert_eq!(app.todo_panel.open_count(), 1);
        assert!(app.todo_close_handles.is_empty());
        assert!(matches!(app.overlay, OverlayState::TodoPopup(_)));
    }

    #[tokio::test]
    async fn done_asks_first_and_only_a_y_closes() {
        let mut app = app_with(vec![item("a", "do a"), item("b", "do b")]);
        app.todo_open_popup();
        handle_todo_popup_key(&mut app, key('d')).await;
        assert_eq!(popup(&app).confirm, Some(OperatorTodoStatusV1::Done));
        // Nothing but the prompt keys acts.
        for stray in ['x', 'j', 'o', 'u', 'z'] {
            handle_todo_popup_key(&mut app, key(stray)).await;
        }
        assert_eq!(popup(&app).confirm, Some(OperatorTodoStatusV1::Done));
        assert_eq!(app.todo_panel.open_count(), 2);
        assert!(app.todo_close_handles.is_empty());

        handle_todo_popup_key(&mut app, key('y')).await;
        assert!(matches!(app.overlay, OverlayState::None));
        assert_eq!(app.todo_panel.open_count(), 1);
        assert_eq!(app.todo_close_handles.len(), 1);
        assert!(
            app.todo_panel
                .undo_available(chrono::Utc::now())
                .is_some_and(|entry| entry.status == OperatorTodoStatusV1::Done)
        );
    }

    #[tokio::test]
    async fn declining_the_prompt_keeps_the_item_open() {
        let mut app = app_with(vec![item("a", "do a")]);
        app.todo_open_popup();
        handle_todo_popup_key(&mut app, key('x')).await;
        assert_eq!(popup(&app).confirm, Some(OperatorTodoStatusV1::Dismissed));
        handle_todo_popup_key(&mut app, key('n')).await;
        assert_eq!(popup(&app).confirm, None);
        handle_todo_popup_key(&mut app, key('x')).await;
        handle_todo_popup_key(&mut app, KeyEvent::from(KeyCode::Esc)).await;
        assert_eq!(popup(&app).confirm, None);
        assert_eq!(app.todo_panel.open_count(), 1);
        assert!(app.todo_close_handles.is_empty());
        assert!(app.todo_panel.undo_available(chrono::Utc::now()).is_none());
    }

    #[tokio::test]
    async fn n_and_p_walk_the_items_and_j_k_scroll() {
        let mut app = app_with(vec![item("a", "do a"), item("b", "do b")]);
        let first = app.todo_panel.selected().map(|i| i.id);
        app.todo_open_popup();
        handle_todo_popup_key(&mut app, key('j')).await;
        handle_todo_popup_key(&mut app, key('j')).await;
        assert_eq!(popup(&app).scroll, 2);
        handle_todo_popup_key(&mut app, key('k')).await;
        assert_eq!(popup(&app).scroll, 1);
        handle_todo_popup_key(&mut app, key('n')).await;
        assert_ne!(app.todo_panel.selected().map(|i| i.id), first);
        assert_eq!(popup(&app).scroll, 0);
        handle_todo_popup_key(&mut app, key('p')).await;
        assert_eq!(app.todo_panel.selected().map(|i| i.id), first);
    }

    #[tokio::test]
    async fn u_in_the_popup_undoes_the_last_close() {
        let mut app = app_with(vec![item("a", "do a"), item("b", "do b")]);
        app.todo_open_popup();
        handle_todo_popup_key(&mut app, key('d')).await;
        handle_todo_popup_key(&mut app, KeyEvent::from(KeyCode::Enter)).await;
        assert_eq!(app.todo_panel.open_count(), 1);
        app.todo_open_popup();
        handle_todo_popup_key(&mut app, key('u')).await;
        assert_eq!(app.todo_reopen_handles.len(), 1);
        assert!(app.todo_panel.undo_available(chrono::Utc::now()).is_none());
    }
}
