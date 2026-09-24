//! Search operations for App.

use super::App;
use crate::types::{InputMode, Pane, SearchTarget};

impl App {
    /// Cancel any active search (if in search mode or confirmed search is active).
    pub fn cancel_search_if_active(&mut self) {
        if self.search_query.is_empty() && self.search_matches.is_empty() {
            return;
        }
        self.search_query.clear();
        self.search_matches.clear();
        self.search_match_cursor = 0;
        if self.input_mode == InputMode::Search {
            self.input_mode = InputMode::Normal;
        }
        self.recalculate_filtered_order();
    }

    /// Apply current search query — recalculate filters and clamp selection.
    pub fn apply_search(&mut self) {
        match self.search_target {
            SearchTarget::SessionList => {
                self.recalculate_filtered_order();
                self.reconcile_all_session_list_selections(false);
            }
            SearchTarget::SessionDetail => {
                self.search_session_events();
            }
        }
    }

    /// Clear search filter and restore full session list.
    pub fn clear_search_filter(&mut self) {
        if self.search_target == SearchTarget::SessionList {
            self.recalculate_filtered_order();
            self.reconcile_all_session_list_selections(false);
        }
        self.search_matches.clear();
        self.search_match_cursor = 0;
    }

    /// Scan current session's events for search query matches.
    fn search_session_events(&mut self) {
        self.search_matches.clear();
        self.search_match_cursor = 0;

        let session_id = match self.focused_pane().cloned() {
            Some(Pane::SessionDetail { session_id }) => session_id,
            _ => return,
        };
        let Some(state) = self.sessions.get(&session_id) else {
            return;
        };
        if self.search_query.is_empty() {
            return;
        }

        let query_lower = self.search_query.to_lowercase();
        for (idx, event) in state.events.iter().enumerate() {
            if event.content.to_lowercase().contains(&query_lower) {
                self.search_matches.push(idx);
            }
        }

        // Jump to first match
        if !self.search_matches.is_empty() {
            self.jump_to_search_match(0);
        }
    }

    /// Jump to the Nth search match (by index into search_matches).
    fn jump_to_search_match(&mut self, match_index: usize) {
        let Some(&event_idx) = self.search_matches.get(match_index) else {
            return;
        };
        let session_id = match self.focused_pane().cloned() {
            Some(Pane::SessionDetail { session_id }) => session_id,
            _ => return,
        };
        let Some(state) = self.sessions.get_mut(&session_id) else {
            return;
        };
        if let Some(&offset) = state.event_offsets.get(event_idx) {
            state.scroll_offset = offset;
            state.current_event_index = Some(event_idx);
            state.follow_tail = false;
        }
        self.search_match_cursor = match_index;
    }

    /// Jump to next search match (wraps around).
    pub fn next_search_match(&mut self) {
        if self.search_matches.is_empty() {
            return;
        }
        let next = (self.search_match_cursor + 1) % self.search_matches.len();
        self.jump_to_search_match(next);
    }

    /// Jump to previous search match (wraps around).
    pub fn prev_search_match(&mut self) {
        if self.search_matches.is_empty() {
            return;
        }
        let prev = if self.search_match_cursor == 0 {
            self.search_matches.len() - 1
        } else {
            self.search_match_cursor - 1
        };
        self.jump_to_search_match(prev);
    }
}
