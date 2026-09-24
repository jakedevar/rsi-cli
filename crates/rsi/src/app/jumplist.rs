//! Jumplist (Ctrl+O / Ctrl+I) operations for App.

use super::App;
use crate::types::PaneId;
use uuid::Uuid;

impl App {
    /// Maximum jumplist size (matches vim's default).
    const JUMPLIST_MAX: usize = 100;

    /// Push a session onto the jumplist. Called at every organic navigation site.
    /// Skipped during jumplist navigation (navigating_jumplist flag) and
    /// when the new entry matches the current jumplist position (dedup consecutive).
    pub fn push_jumplist(&mut self, session_id: Uuid) {
        if self.navigating_jumplist {
            return;
        }
        // Deduplicate consecutive: if cursor points at the same session, skip
        if !self.session_jumplist.is_empty()
            && self.jumplist_cursor < self.session_jumplist.len()
            && self.session_jumplist[self.jumplist_cursor] == session_id
        {
            return;
        }
        // Truncate forward history
        if !self.session_jumplist.is_empty() {
            self.session_jumplist.truncate(self.jumplist_cursor + 1);
        }
        self.session_jumplist.push(session_id);
        // Cap at max size (drop oldest entries)
        if self.session_jumplist.len() > Self::JUMPLIST_MAX {
            let overflow = self.session_jumplist.len() - Self::JUMPLIST_MAX;
            self.session_jumplist.drain(..overflow);
        }
        self.jumplist_cursor = self.session_jumplist.len() - 1;
    }

    /// Jump backward in session history (Ctrl+O).
    pub fn jump_back(&mut self) {
        if self.session_jumplist.is_empty() {
            return;
        }

        // When focused on a session list (no session detail open), the jumplist
        // cursor still points at the session the user just left. The first Ctrl+O
        // should restore that session — not skip past it to the one before. Only
        // once the user is inside a session detail does Ctrl+O step backward.
        let in_list = matches!(
            self.focused_pane(),
            Some(crate::types::Pane::SessionList { .. })
        );
        if in_list {
            let Some(&session_id) = self.session_jumplist.get(self.jumplist_cursor) else {
                return;
            };
            if self.sessions.contains_key(&session_id) {
                self.navigating_jumplist = true;
                self.open_session_in_current_pane(session_id);
                self.navigating_jumplist = false;
            } else {
                self.session_jumplist.remove(self.jumplist_cursor);
                if self.jumplist_cursor > 0 {
                    self.jumplist_cursor -= 1;
                }
                self.jump_back();
            }
            return;
        }

        if self.jumplist_cursor == 0 {
            return;
        }
        self.navigating_jumplist = true;
        self.jumplist_cursor -= 1;
        let session_id = self.session_jumplist[self.jumplist_cursor];
        // Only navigate if session still exists
        if self.sessions.contains_key(&session_id) {
            self.open_session_in_current_pane(session_id);
        } else {
            // Session was deleted — remove and retry
            self.session_jumplist.remove(self.jumplist_cursor);
            self.navigating_jumplist = false;
            self.jump_back();
            return;
        }
        self.navigating_jumplist = false;
    }

    /// Jump forward in session history (Ctrl+I).
    pub fn jump_forward(&mut self) {
        if self.session_jumplist.is_empty()
            || self.jumplist_cursor >= self.session_jumplist.len() - 1
        {
            return;
        }
        self.navigating_jumplist = true;
        self.jumplist_cursor += 1;
        let session_id = self.session_jumplist[self.jumplist_cursor];
        if self.sessions.contains_key(&session_id) {
            self.open_session_in_current_pane(session_id);
        } else {
            self.session_jumplist.remove(self.jumplist_cursor);
            if self.jumplist_cursor >= self.session_jumplist.len() {
                self.jumplist_cursor = self.session_jumplist.len().saturating_sub(1);
            }
            self.navigating_jumplist = false;
            self.jump_forward();
            return;
        }
        self.navigating_jumplist = false;
    }

    /// Remove a session from the jumplist (called on delete/archive).
    pub(crate) fn clean_jumplist(&mut self, session_id: Uuid) {
        let cursor_session = self.session_jumplist.get(self.jumplist_cursor).copied();
        self.session_jumplist.retain(|id| *id != session_id);
        if self.session_jumplist.is_empty() {
            self.jumplist_cursor = 0;
        } else if let Some(prev) = cursor_session {
            // Try to keep cursor at the same session it was pointing to
            self.jumplist_cursor = self
                .session_jumplist
                .iter()
                .position(|id| *id == prev)
                .unwrap_or(self.session_jumplist.len().saturating_sub(1));
        }
    }

    /// Reset selection in all SessionList panes to first filtered session.
    pub(crate) fn reset_selection_to_first(&mut self) {
        let first = self.session_id_at(0);
        for tab in &mut self.tabs {
            let ids: Vec<PaneId> = tab.layout.leaf_ids();
            for leaf_id in ids {
                if let Some(crate::types::Pane::SessionList {
                    selected_index,
                    selected_session,
                    ..
                }) = tab.layout.find_pane_mut(leaf_id)
                {
                    *selected_index = 0;
                    *selected_session = first;
                }
            }
        }
    }

    /// Reset selection in the active tab's SessionList panes to the first filtered session.
    /// Used after project switching — only resets the newly-active tab so other tabs
    /// preserve their own per-project cursor positions.
    pub(crate) fn reset_active_tab_selection(&mut self) {
        let first = self.session_id_at(0);
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            // Reset the live layout panes.
            let ids: Vec<PaneId> = tab.layout.leaf_ids();
            for leaf_id in ids {
                if let Some(crate::types::Pane::SessionList {
                    selected_index,
                    selected_session,
                    scroll_offset,
                    ..
                }) = tab.layout.find_pane_mut(leaf_id)
                {
                    *selected_index = 0;
                    *selected_session = first;
                    *scroll_offset = 0;
                }
            }
            // Reset the shadow copy used when the detail view is active.
            if let crate::types::Pane::SessionList {
                selected_index,
                selected_session,
                scroll_offset,
                ..
            } = &mut tab.session_list_state
            {
                *selected_index = 0;
                *selected_session = first;
                *scroll_offset = 0;
            }
        }
    }
}
