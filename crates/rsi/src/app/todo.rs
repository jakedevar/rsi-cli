//! Operator to-do panel operations for `App` (#1747 S2): the background
//! `OperatorTodoList` read that feeds the panel and the status-line badge, and
//! the keys that open an item's link, mark it done or dismiss it.
//!
//! Daemon I/O runs in spawned tasks so the input/render task never waits on
//! the socket (the same shape as the worker-pressure Health read).

use super::App;
use crate::todo_panel::TodoLink;
use crate::types::{Pane, SplitNode};
use rsi_common::operator_todos::{OperatorTodoListResultV1, OperatorTodoStatusV1, OperatorTodoV1};
use uuid::Uuid;

const TODO_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
const TODO_RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

async fn fetch_todos(
    socket_path: std::path::PathBuf,
    closed_since: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<OperatorTodoListResultV1, String> {
    tokio::time::timeout(TODO_RPC_TIMEOUT, async {
        let mut client = crate::client::DaemonClient::new(socket_path);
        client.connect().await.map_err(|error| error.to_string())?;
        client
            .list_operator_todos(closed_since)
            .await
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|_| "operator to-do read timed out".to_string())?
}

async fn close_todo(
    socket_path: std::path::PathBuf,
    id: Uuid,
    status: OperatorTodoStatusV1,
) -> Result<OperatorTodoV1, (Uuid, String)> {
    tokio::time::timeout(TODO_RPC_TIMEOUT, async {
        let mut client = crate::client::DaemonClient::new(socket_path);
        client
            .connect()
            .await
            .map_err(|error| (id, error.to_string()))?;
        client
            .close_operator_todo(id, status)
            .await
            .map_err(|error| (id, error.to_string()))
    })
    .await
    .map_err(|_| (id, "operator to-do close timed out".to_string()))?
}

async fn reopen_todo(socket_path: std::path::PathBuf, id: Uuid) -> Result<OperatorTodoV1, String> {
    tokio::time::timeout(TODO_RPC_TIMEOUT, async {
        let mut client = crate::client::DaemonClient::new(socket_path);
        client.connect().await.map_err(|error| error.to_string())?;
        client
            .reopen_operator_todo(id)
            .await
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|_| "operator to-do reopen timed out".to_string())?
}

fn node_shows_detail(node: &SplitNode) -> bool {
    match node {
        SplitNode::Leaf { pane, .. } => matches!(pane, Pane::SessionDetail { .. }),
        SplitNode::Split { first, second, .. } => {
            node_shows_detail(first) || node_shows_detail(second)
        }
    }
}

impl App {
    /// The panel is asked for and a session detail is on screen, so the
    /// operator can see it. Whether it fits is the renderer's call.
    #[must_use]
    pub(crate) fn todo_panel_visible(&self) -> bool {
        self.todo_panel.open
            && self
                .tabs
                .get(self.active_tab)
                .is_some_and(|tab| node_shows_detail(&tab.layout))
    }

    /// Complete finished background calls, remember the visit and start the
    /// next read. Runs every loop turn.
    pub(crate) async fn poll_todo_panel(&mut self) {
        if self
            .todo_refresh_handle
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
            && let Some(handle) = self.todo_refresh_handle.take()
        {
            match handle.await {
                Ok(Ok(list)) => {
                    self.todo_panel.apply(list);
                    self.mark_dirty();
                }
                Ok(Err(error)) => tracing::debug!(%error, "operator to-do read unavailable"),
                Err(error) => tracing::debug!(%error, "operator to-do read task failed"),
            }
        }

        let (finished, pending): (Vec<_>, Vec<_>) = std::mem::take(&mut self.todo_close_handles)
            .into_iter()
            .partition(tokio::task::JoinHandle::is_finished);
        self.todo_close_handles = pending;
        for handle in finished {
            match handle.await {
                Ok(Ok(_)) => {}
                Ok(Err((_, error))) => {
                    self.notify_error(format!("To-do update failed: {error}"));
                    // The optimistic removal was wrong; read the truth.
                    self.todo_next_refresh_at = std::time::Instant::now();
                }
                Err(error) => tracing::debug!(%error, "operator to-do close task failed"),
            }
        }

        let (finished, pending): (Vec<_>, Vec<_>) = std::mem::take(&mut self.todo_reopen_handles)
            .into_iter()
            .partition(tokio::task::JoinHandle::is_finished);
        self.todo_reopen_handles = pending;
        for handle in finished {
            match handle.await {
                Ok(Ok(restored)) => {
                    // A read that started before the reopen would drop the
                    // item again; discard it and read once more.
                    if let Some(stale) = self.todo_refresh_handle.take() {
                        stale.abort();
                    }
                    self.notify(format!("Restored to-do: {}", restored.title));
                    self.todo_panel.restore_open(restored);
                    self.todo_next_refresh_at = std::time::Instant::now();
                    self.mark_dirty();
                }
                Ok(Err(error)) => {
                    self.notify_error(format!("Undo failed: {error}"));
                    self.todo_next_refresh_at = std::time::Instant::now();
                }
                Err(error) => tracing::debug!(%error, "operator to-do reopen task failed"),
            }
        }

        if self.todo_panel.expire_undo(chrono::Utc::now()) {
            self.mark_dirty();
        }

        if self.todo_panel_visible() {
            self.todo_panel.touch_visit(chrono::Utc::now());
        }

        if !self.poll.connected
            || self.todo_refresh_handle.is_some()
            || std::time::Instant::now() < self.todo_next_refresh_at
        {
            return;
        }
        self.todo_next_refresh_at = std::time::Instant::now() + TODO_REFRESH_INTERVAL;
        let socket_path = self.client.socket_path().to_path_buf();
        let closed_since = self.todo_panel.closed_since();
        self.todo_refresh_handle = Some(tokio::spawn(fetch_todos(socket_path, closed_since)));
    }

    /// Show or hide the panel and remember the choice.
    pub(crate) fn toggle_todo_panel(&mut self) {
        self.todo_panel.open = !self.todo_panel.open;
        // The surface beside the panel shifts; repaint every cell.
        self.pane_switch_clear = true;
        self.mark_dirty();
        crate::state::PersistedState::capture(self).save();
    }

    /// Move the panel cursor by `delta` rows. A hidden panel opens first so
    /// the operator never moves a cursor they cannot see.
    pub(crate) fn todo_move(&mut self, delta: isize) {
        if !self.todo_panel.open {
            self.toggle_todo_panel();
            return;
        }
        if delta >= 0 {
            self.todo_panel.select_next();
        } else {
            self.todo_panel.select_prev();
        }
        self.mark_dirty();
    }

    /// Ask before closing the selected item (#2950): open the to-do pop-up on
    /// its y/n prompt. `<Space>dd` and `<Space>dx` end here, never at the close.
    pub(crate) fn todo_confirm_close(&mut self, status: OperatorTodoStatusV1) {
        let Some((_, status)) = crate::todo_panel::close_target(&self.todo_panel, status) else {
            self.notify("No to-do item selected");
            return;
        };
        crate::overlay::todo_popup::open_todo_popup(self, Some(status));
    }

    /// Open the selected item in the to-do pop-up.
    pub(crate) fn todo_open_popup(&mut self) {
        if self.todo_panel.selected().is_none() {
            self.notify("No to-do item selected");
            return;
        }
        crate::overlay::todo_popup::open_todo_popup(self, None);
    }

    /// Reopen the item the operator just closed, while the window is open.
    pub(crate) fn todo_undo(&mut self) {
        let Some(entry) = self.todo_panel.take_undo(chrono::Utc::now()) else {
            self.notify(format!(
                "Nothing to undo (a close can be undone for {} s)",
                rsi_common::operator_todos::OPERATOR_TODO_UNDO_WINDOW_SECS
            ));
            return;
        };
        let socket_path = self.client.socket_path().to_path_buf();
        self.todo_reopen_handles
            .push(tokio::spawn(reopen_todo(socket_path, entry.id)));
        self.notify(format!("Restoring to-do: {}", entry.title));
        self.mark_dirty();
    }

    /// Mark the selected item done or dismissed through `OperatorTodoClose`.
    /// Callers confirm first ([`Self::todo_confirm_close`]); the close stays
    /// undoable for the undo window.
    pub(crate) fn todo_close_selected(&mut self, status: OperatorTodoStatusV1) {
        let Some((id, status)) = crate::todo_panel::close_target(&self.todo_panel, status) else {
            self.notify("No to-do item selected");
            return;
        };
        if let Some(item) = self.todo_panel.selected().cloned() {
            self.todo_panel
                .record_close(&item, status, chrono::Utc::now());
        }
        self.todo_panel.remove_open(id);
        self.mark_dirty();
        let socket_path = self.client.socket_path().to_path_buf();
        self.todo_close_handles
            .push(tokio::spawn(close_todo(socket_path, id, status)));
        // Read back soon so the list matches the daemon.
        self.todo_next_refresh_at = std::time::Instant::now();
        let what = match status {
            OperatorTodoStatusV1::Dismissed => "To-do dismissed",
            _ => "To-do marked done",
        };
        self.notify(format!(
            "{what} · <Space>du undoes it for {} s",
            rsi_common::operator_todos::OPERATOR_TODO_UNDO_WINDOW_SECS
        ));
    }

    /// The exact text `<Space>dc` puts on the clipboard: the selected item's
    /// `how`, byte for byte.
    pub(crate) fn todo_copy_payload(&self) -> Option<String> {
        self.todo_panel.selected().map(|item| item.how.clone())
    }

    /// Copy the selected item's `how` verbatim (no wrapping, `$ ` prefix or
    /// border) to the clipboard via OSC 52 and confirm in the status line.
    pub(crate) fn todo_copy_selected(&mut self) {
        let Some(text) = self.todo_copy_payload() else {
            self.notify("No to-do item selected");
            return;
        };
        crate::clipboard::osc52_copy(&text);
        self.notify(copied_message(&text));
        self.mark_dirty();
    }

    /// Open the selected item's linked Issue, session or URL.
    pub(crate) async fn todo_open_link(&mut self) {
        let Some(item) = self.todo_panel.selected().cloned() else {
            self.notify("No to-do item selected");
            return;
        };
        let Some(link) = item.link.as_deref() else {
            self.notify("This to-do has no link");
            return;
        };
        match TodoLink::parse(link) {
            Some(TodoLink::Issue(number)) => match i64::try_from(number) {
                Ok(number) => self.open_issue_number(number),
                Err(_) => self.notify_error(format!("Not an Issue number: {number}")),
            },
            Some(TodoLink::Session(id)) => {
                if !crate::overlay::global_manager_workspace::jump_to_session(self, id).await {
                    self.notify_error("Linked session is unavailable");
                }
            }
            Some(TodoLink::Url(url)) => {
                if let Err(error) = crate::browser::open_url(&url) {
                    self.notify_error(format!("Could not open {url}: {error}"));
                }
            }
            None => self.notify(format!("Cannot open link: {link}")),
        }
        self.mark_dirty();
    }

    /// The operator has caught up: clear the highlights and re-read.
    pub(crate) fn todo_acknowledge(&mut self) {
        self.todo_panel.acknowledge(chrono::Utc::now());
        self.todo_next_refresh_at = std::time::Instant::now();
        self.mark_dirty();
        crate::state::PersistedState::capture(self).save();
    }
}

/// The status-line confirmation: "copied" plus the first characters.
fn copied_message(text: &str) -> String {
    const PREVIEW_CHARS: usize = 40;
    let mut preview: String = text.chars().take(PREVIEW_CHARS).collect();
    if text.chars().count() > PREVIEW_CHARS {
        preview.push('…');
    }
    format!("Copied to-do command: {preview}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::app_test_helpers::with_session_detail;

    /// A daemon-free App whose focused pane is a session detail.
    fn detail_app() -> App {
        let (mut app, _) = with_session_detail();
        app.enter_session();
        app
    }
    use chrono::Utc;
    use rsi_common::operator_todos::OperatorTodoListResultV1;

    fn item(title: &str, link: Option<&str>) -> OperatorTodoV1 {
        OperatorTodoV1 {
            id: Uuid::new_v4(),
            scope: "portfolio:global".into(),
            project_id: None,
            todo_key: title.into(),
            title: title.into(),
            how: "do it".into(),
            link: link.map(str::to_string),
            priority: 3,
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
        let mut app = detail_app();
        app.todo_panel.open = true;
        app.todo_panel.apply(OperatorTodoListResultV1 {
            open: items,
            ..OperatorTodoListResultV1::default()
        });
        app
    }

    #[test]
    fn the_panel_is_visible_only_when_open_over_a_session_detail() {
        let mut app = app_with(vec![]);
        assert!(app.todo_panel_visible());
        app.todo_panel.open = false;
        assert!(!app.todo_panel_visible());
    }

    #[test]
    fn the_toggle_key_flips_and_remembers_the_choice() {
        let mut app = app_with(vec![]);
        app.toggle_todo_panel();
        assert!(!app.todo_panel.open);
        assert!(crate::state::PersistedState::capture(&app).todo_panel_hidden);
        app.toggle_todo_panel();
        assert!(app.todo_panel.open);
        assert!(!crate::state::PersistedState::capture(&app).todo_panel_hidden);
    }

    #[test]
    fn the_saved_state_restores_the_choice_and_the_visit_marker() {
        let mut app = app_with(vec![]);
        app.todo_panel.touch_visit(Utc::now());
        app.todo_panel.open = false;
        let saved = crate::state::PersistedState::capture(&app);
        let restored =
            crate::todo_panel::TodoPanel::restored(!saved.todo_panel_hidden, saved.todo_last_visit);
        assert!(!restored.open);
        assert_eq!(restored.closed_since(), app.todo_panel.last_visit);
    }

    #[test]
    fn cursor_keys_move_and_a_hidden_panel_opens_first() {
        let a = item("a", None);
        let b = item("b", None);
        let mut app = app_with(vec![a, b]);
        let first = app.todo_panel.selected().map(|i| i.id);
        app.todo_move(1);
        assert_ne!(app.todo_panel.selected().map(|i| i.id), first);
        app.todo_move(-1);
        assert_eq!(app.todo_panel.selected().map(|i| i.id), first);

        app.todo_panel.open = false;
        app.todo_move(1);
        assert!(app.todo_panel.open);
        assert_eq!(app.todo_panel.selected().map(|i| i.id), first);
    }

    #[tokio::test]
    async fn done_removes_the_item_at_once_and_sends_one_close() {
        let a = item("a", None);
        let b = item("b", None);
        let mut app = app_with(vec![a.clone(), b]);
        let selected = app.todo_panel.selected().map(|i| i.id).unwrap();
        app.todo_close_selected(OperatorTodoStatusV1::Done);
        assert_eq!(app.todo_panel.open_count(), 1);
        assert!(app.todo_panel.open_items().iter().all(|i| i.id != selected));
        assert_eq!(app.todo_close_handles.len(), 1);
        assert!(app.todo_panel.selected().is_some());
    }

    #[tokio::test]
    async fn dismiss_with_nothing_selected_tells_the_operator() {
        let mut app = app_with(vec![]);
        app.todo_close_selected(OperatorTodoStatusV1::Dismissed);
        assert!(app.todo_close_handles.is_empty());
        assert!(
            app.notifications
                .iter()
                .any(|n| n.message.contains("No to-do item selected"))
        );
    }

    #[tokio::test]
    async fn open_link_explains_a_missing_or_unopenable_link() {
        let mut app = app_with(vec![item("plain", None)]);
        app.todo_open_link().await;
        assert!(
            app.notifications
                .iter()
                .any(|n| n.message.contains("no link"))
        );

        let mut app = app_with(vec![item("odd", Some("file:///etc/passwd"))]);
        app.todo_open_link().await;
        assert!(
            app.notifications
                .iter()
                .any(|n| n.message.contains("Cannot open link"))
        );
    }

    #[tokio::test]
    async fn an_issue_link_opens_the_issues_workspace() {
        let mut app = app_with(vec![item("fix", Some("issue:#1747"))]);
        app.todo_open_link().await;
        let tab = app.active_tab();
        assert!(
            tab.layout
                .leaf_ids()
                .into_iter()
                .any(|id| matches!(tab.layout.find_pane(id), Some(Pane::Issues(_))))
        );
    }

    #[tokio::test]
    async fn an_issue_link_goes_straight_to_that_issue_number() {
        let mut app = app_with(vec![item("fix", Some("issue:#2377"))]);
        app.todo_open_link().await;
        let tab = app.active_tab();
        let jump = tab
            .layout
            .leaf_ids()
            .into_iter()
            .find_map(|id| match tab.layout.find_pane(id) {
                Some(Pane::Issues(state)) => state.transient.pending_number_jump.clone(),
                _ => None,
            })
            .expect("the link starts a go-to-number request");
        assert_eq!(jump.number, 2377);
    }

    #[test]
    fn acknowledging_clears_the_catch_up_highlights() {
        let mut app = detail_app();
        app.todo_panel = crate::todo_panel::TodoPanel::restored(
            true,
            Some(Utc::now() - chrono::Duration::hours(1)),
        );
        app.todo_panel.apply(OperatorTodoListResultV1 {
            open: vec![item("fresh", None)],
            ..OperatorTodoListResultV1::default()
        });
        assert!(app.todo_panel.has_catch_up());
        app.todo_acknowledge();
        assert!(!app.todo_panel.has_catch_up());
    }

    #[tokio::test]
    async fn the_space_d_chords_reach_the_panel_through_the_action_dispatcher() {
        use crate::modalkit_types::LcAction;
        let mut app = app_with(vec![item("a", None), item("b", None)]);
        let first = app.todo_panel.selected().map(|i| i.id);
        crate::action_handler::dispatch_lc_action(&mut app, LcAction::TodoNext).await;
        assert_ne!(app.todo_panel.selected().map(|i| i.id), first);
        // Done asks first (#2950): the chord opens the y/n prompt and closes nothing.
        crate::action_handler::dispatch_lc_action(&mut app, LcAction::TodoDone).await;
        assert_eq!(app.todo_panel.open_count(), 2);
        assert!(matches!(
            app.overlay,
            crate::types::OverlayState::TodoPopup(ref state)
                if state.confirm == Some(OperatorTodoStatusV1::Done)
        ));
        crate::overlay::handle_overlay_key(
            &mut app,
            crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char('y')),
        )
        .await;
        assert_eq!(app.todo_panel.open_count(), 1);
        crate::action_handler::dispatch_lc_action(&mut app, LcAction::TodoAcknowledge).await;
        crate::action_handler::dispatch_lc_action(&mut app, LcAction::ToggleTodoPanel).await;
        assert!(!app.todo_panel.open);
    }

    #[tokio::test]
    async fn copy_chord_copies_the_how_verbatim_and_confirms() {
        use crate::modalkit_types::LcAction;
        let how = "rsi grant restore --caps  v8   --scope portfolio:global  --reason \"operator restore after outage\" --yes";
        let mut todo = item("restore", None);
        todo.how = how.to_string();
        let mut app = app_with(vec![todo]);
        assert_eq!(app.todo_copy_payload().as_deref(), Some(how));
        assert_eq!(
            crate::clipboard::encode_osc52(&app.todo_copy_payload().unwrap()),
            crate::clipboard::encode_osc52(how)
        );
        crate::action_handler::dispatch_lc_action(&mut app, LcAction::TodoCopy).await;
        assert!(app.notifications.iter().any(|n| {
            n.message
                .contains("Copied to-do command: rsi grant restore --caps  v8")
        }));
        // Yanking leaves the item open and closes nothing.
        assert_eq!(app.todo_panel.open_count(), 1);
        assert!(app.todo_close_handles.is_empty());
    }

    #[tokio::test]
    async fn copy_with_nothing_selected_explains_it() {
        use crate::modalkit_types::LcAction;
        let mut app = app_with(vec![]);
        assert_eq!(app.todo_copy_payload(), None);
        crate::action_handler::dispatch_lc_action(&mut app, LcAction::TodoCopy).await;
        assert!(
            app.notifications
                .iter()
                .any(|n| n.message.contains("No to-do item selected"))
        );
    }

    #[tokio::test]
    async fn dismiss_chord_asks_and_a_declined_prompt_changes_nothing() {
        use crate::modalkit_types::LcAction;
        let mut app = app_with(vec![item("a", None)]);
        crate::action_handler::dispatch_lc_action(&mut app, LcAction::TodoDismiss).await;
        assert!(matches!(
            app.overlay,
            crate::types::OverlayState::TodoPopup(ref state)
                if state.confirm == Some(OperatorTodoStatusV1::Dismissed)
        ));
        crate::overlay::handle_overlay_key(
            &mut app,
            crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char('n')),
        )
        .await;
        assert_eq!(app.todo_panel.open_count(), 1);
        assert!(app.todo_close_handles.is_empty());
    }

    #[tokio::test]
    async fn the_popup_and_confirm_chords_need_an_item() {
        use crate::modalkit_types::LcAction;
        let mut app = app_with(vec![]);
        for action in [
            LcAction::TodoPopup,
            LcAction::TodoDone,
            LcAction::TodoDismiss,
        ] {
            crate::action_handler::dispatch_lc_action(&mut app, action).await;
        }
        assert!(matches!(app.overlay, crate::types::OverlayState::None));
        assert!(
            app.notifications
                .iter()
                .any(|n| n.message.contains("No to-do item selected"))
        );
    }

    #[tokio::test]
    async fn closing_offers_an_undo_that_sends_one_reopen_and_then_nothing() {
        use crate::modalkit_types::LcAction;
        let mut app = app_with(vec![item("a", None), item("b", None)]);
        let closed = app.todo_panel.selected().map(|i| i.id).unwrap();
        app.todo_close_selected(OperatorTodoStatusV1::Done);
        assert!(
            app.notifications
                .iter()
                .any(|n| n.message.contains("<Space>du undoes it"))
        );
        assert_eq!(
            app.todo_panel
                .undo_available(chrono::Utc::now())
                .map(|e| e.id),
            Some(closed)
        );

        crate::action_handler::dispatch_lc_action(&mut app, LcAction::TodoUndo).await;
        assert_eq!(app.todo_reopen_handles.len(), 1);
        assert!(app.todo_panel.undo_available(chrono::Utc::now()).is_none());
        // A second press has nothing left to undo.
        crate::action_handler::dispatch_lc_action(&mut app, LcAction::TodoUndo).await;
        assert_eq!(app.todo_reopen_handles.len(), 1);
        assert!(
            app.notifications
                .iter()
                .any(|n| n.message.contains("Nothing to undo"))
        );
    }

    #[tokio::test]
    async fn an_undo_after_the_window_is_refused_locally() {
        let mut app = app_with(vec![item("a", None)]);
        let selected = app.todo_panel.selected().cloned().unwrap();
        let long_ago = chrono::Utc::now()
            - chrono::Duration::seconds(
                rsi_common::operator_todos::OPERATOR_TODO_UNDO_WINDOW_SECS + 1,
            );
        app.todo_panel
            .record_close(&selected, OperatorTodoStatusV1::Done, long_ago);
        app.todo_undo();
        assert!(app.todo_reopen_handles.is_empty());
        assert!(
            app.notifications
                .iter()
                .any(|n| n.message.contains("Nothing to undo"))
        );
    }
}
