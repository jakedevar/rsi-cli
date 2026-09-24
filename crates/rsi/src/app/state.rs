//! State restoration operations for App.

use super::App;
use crate::state::PersistedState;
use crate::types::{Pane, PaneId};
use uuid::Uuid;

impl App {
    /// Apply stashed dev-state session views to loaded sessions.
    /// Called once after the first poll_sessions() so sessions exist in the map.
    pub fn apply_pending_dev_state(&mut self) {
        let views = match self.pending_dev_views.take() {
            Some(v) => v,
            None => return,
        };

        for (uuid_str, view) in views {
            let uuid = match uuid_str.parse::<Uuid>() {
                Ok(u) => u,
                Err(_) => continue,
            };
            let state = match self.sessions.get_mut(&uuid) {
                Some(s) => s,
                None => continue,
            };

            state.scroll_offset = view.scroll_offset;
            state.collapsed_events = view.collapsed_events;
            state.expanded_events = view.expanded_events;
            state.show_system_events = view.show_system_events;
            state.show_tool_results = view.show_tool_results;
            state.follow_tail = view.follow_tail;
            state.center_content = view.center_content;
            state.list_card_expanded = view.list_card_expanded;
            // Don't restore last_sequence — events are never persisted across
            // restarts, so restoring the cursor would cause the fetch logic to
            // skip all events up to that sequence (Append mode with empty vec).
            // Leaving it None forces a full Replace fetch on the first poll.

            // Reconstruct TextArea from saved lines
            let has_content = !(view.input_bar_lines.is_empty()
                || view.input_bar_lines.len() == 1 && view.input_bar_lines[0].is_empty());
            if has_content {
                let mut textarea = tui_textarea::TextArea::new(view.input_bar_lines);
                textarea.set_cursor_line_style(ratatui::style::Style::default());
                textarea.set_block(ratatui::widgets::Block::default());
                *state.input_bar.surface.textarea = textarea;
                state.input_bar.surface.mode = view.input_bar_mode;
            }

            // Restore file viewer if it was open
            if let Some(path_str) = &view.file_viewer_path {
                if let Some(lines) = &view.file_viewer_lines {
                    let path = std::path::PathBuf::from(path_str);
                    let mut viewer = crate::types::FileViewerState::new(path, lines.join("\n"));
                    viewer.dirty = view.file_viewer_dirty.unwrap_or(false);
                    if let Some(mode) = view.file_viewer_mode {
                        viewer.surface.mode = mode;
                    }
                    state.file_viewer = Some(viewer);
                }
            }
        }
    }

    /// Apply stashed fold states from PersistedState to loaded sessions.
    /// Called once after the first poll_sessions() so sessions exist in the map.
    pub fn apply_pending_fold_states(&mut self) {
        let fold_states = match self.pending_fold_states.take() {
            Some(v) => v,
            None => return,
        };

        for (uuid_str, expanded) in fold_states {
            let uuid = match uuid_str.parse::<Uuid>() {
                Ok(u) => u,
                Err(_) => continue,
            };
            if let Some(state) = self.sessions.get_mut(&uuid) {
                state.list_card_expanded = expanded;
            }
        }
        self.invalidate_card_cache();
    }

    /// Reconcile restored pane selections against actual daemon sessions.
    /// Called once after first poll when restoring from PersistedState (not DevState).
    /// For each SessionList pane: if selected_session exists in filtered list,
    /// recalculate selected_index; otherwise fall back to index 0.
    pub fn reconcile_restored_selections(&mut self) {
        self.reconcile_all_session_list_selections(false);

        // Precompute values before taking mutable borrow of tabs
        let fallback_session = if !self.filtered_session_order.is_empty() {
            self.filtered_session_order.first().copied()
        } else {
            self.filtered_taskrabbit_order.first().copied()
        };

        for tab in &mut self.tabs {
            let ids: Vec<PaneId> = tab.layout.leaf_ids();
            for leaf_id in ids {
                // For SessionDetail panes: if the session no longer exists, revert to list
                if let Some(Pane::SessionDetail { session_id }) = tab.layout.find_pane(leaf_id) {
                    let sid = *session_id;
                    if !self.sessions.contains_key(&sid)
                        && let Some(pane) = tab.layout.find_pane_mut(leaf_id)
                    {
                        *pane = Pane::SessionList {
                            selected_index: 0,
                            selected_session: fallback_session,
                            scroll_offset: 0,
                            active_zone: Default::default(),
                            taskrabbit_selected_index: 0,
                            archive_selected_index: 0,
                            jobs_selected_index: 0,
                        };
                    }
                }
            }
        }
    }

    /// Mark the UI as needing a redraw on the next render tick.
    #[inline]
    pub fn mark_dirty(&mut self) {
        self.needs_redraw = true;
    }

    /// Set sort order, re-sort the session list, and persist to disk.
    pub fn set_sort_order(&mut self, order: super::SortOrder) {
        self.settings.sort_order = order;
        self.sort_sessions(false);

        // Persist to disk
        PersistedState::capture(self).save();
    }
}
