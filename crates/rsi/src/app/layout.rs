//! Tab/split layout operations for App.

use super::{App, replace_node};
use crate::types::{Pane, PaneId, SplitDirection, SplitNode, Tab};
use uuid::Uuid;

impl App {
    /// Generate a new unique PaneId.
    pub fn alloc_pane_id(&mut self) -> PaneId {
        let id = PaneId(self.next_pane_id);
        self.next_pane_id += 1;
        id
    }

    /// Get the active tab.
    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active_tab]
    }

    /// Get the active tab mutably.
    pub fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_tab]
    }

    /// Get the pane ID used for user interactions.
    /// When `detail_list_focused` is true and the real focused pane is a
    /// SessionDetail, this returns the SessionList pane ID instead, so all
    /// existing action handlers (nav, fold, search, lifecycle, etc.)
    /// automatically operate on the session list clone within the detail view.
    ///
    /// If no SessionList pane exists in the layout tree (single-pane detail
    /// view), returns `DETAIL_LIST_SENTINEL` which maps to the tab-stored
    /// `session_list_state` via `Tab::find_pane[_mut]()`.
    pub fn interaction_pane_id(&self) -> PaneId {
        let tab = self.active_tab();
        if self.detail_list_focused {
            if matches!(
                tab.layout.find_pane(tab.focused_pane),
                Some(Pane::SessionDetail { .. })
            ) {
                // First check if there's a real SessionList pane in the tree (split layout)
                for pid in tab.layout.leaf_ids() {
                    if matches!(tab.layout.find_pane(pid), Some(Pane::SessionList { .. })) {
                        return pid;
                    }
                }
                // No SessionList in tree — use the tab-stored session list state
                return crate::types::DETAIL_LIST_SENTINEL;
            }
        }
        tab.focused_pane
    }

    /// Get the focused pane in the active tab.
    /// Proxied through `interaction_pane_id()` so that when the detail list
    /// is focused, this returns the SessionList pane (from tree or tab state).
    pub fn focused_pane(&self) -> Option<&Pane> {
        let id = self.interaction_pane_id();
        self.active_tab().find_pane(id)
    }

    /// Get the focused pane mutably in the active tab.
    /// Proxied through `interaction_pane_id()`.
    pub fn focused_pane_mut(&mut self) -> Option<&mut Pane> {
        let id = self.interaction_pane_id();
        self.active_tab_mut().find_pane_mut(id)
    }

    /// Bump the card generation counter to invalidate session list height caches.
    pub fn invalidate_card_cache(&mut self) {
        self.card_generation = self.card_generation.wrapping_add(1);
    }

    /// Whether the focused pane is a session list.
    pub fn focused_pane_is_session_list(&self) -> bool {
        matches!(self.focused_pane(), Some(Pane::SessionList { .. }))
    }

    /// Get the session list pane mutably from the current tab, regardless of focus.
    /// Checks layout tree first, falls back to tab-stored `session_list_state`.
    pub fn session_list_pane_mut(&mut self) -> &mut crate::types::Pane {
        let tab = &self.tabs[self.active_tab];
        // Check if any leaf in the layout tree is a SessionList
        for pid in tab.layout.leaf_ids() {
            if matches!(
                tab.layout.find_pane(pid),
                Some(crate::types::Pane::SessionList { .. })
            ) {
                return self.tabs[self.active_tab]
                    .layout
                    .find_pane_mut(pid)
                    .unwrap();
            }
        }
        // No SessionList in tree — use the tab-stored session list state
        &mut self.tabs[self.active_tab].session_list_state
    }

    /// Get the currently selected session ID from the focused pane.
    pub fn selected_session_id(&self) -> Option<Uuid> {
        match self.focused_pane()? {
            Pane::SessionList {
                selected_session, ..
            } => *selected_session,
            Pane::SessionDetail { session_id } => Some(*session_id),
            Pane::Settings | Pane::PromptCreator | Pane::Issues(_) => None,
        }
    }

    /// Get the working directory of the focused session (if any).
    pub fn focused_session_working_dir(&self) -> Option<std::path::PathBuf> {
        match self.focused_pane() {
            Some(Pane::SessionDetail { session_id }) => self
                .sessions
                .get(session_id)
                .map(|s| s.session.working_dir.clone()),
            _ => self
                .selected_session_id()
                .and_then(|id| self.sessions.get(&id))
                .map(|s| s.session.working_dir.clone()),
        }
    }

    /// Get the cached viewport height of the currently focused session detail pane.
    /// Returns None if the focused pane is not a session detail or the session doesn't exist.
    pub fn focused_session_viewport(&self) -> Option<usize> {
        match self.focused_pane()? {
            Pane::SessionDetail { session_id } => self
                .sessions
                .get(session_id)
                .map(|s| s.last_viewport_height),
            _ => None,
        }
    }

    /// Get the currently selected session state.
    pub fn selected_session_state(&self) -> Option<&crate::types::SessionState> {
        self.selected_session_id()
            .and_then(|id| self.sessions.get(&id))
    }

    /// Resolve the effective topology for a session by walking its
    /// `parent_id` chain to the first ancestor with `workflow_id.is_some()`.
    /// Honors `workflow_id_override` first (returns it immediately if set).
    /// Mirrors `rsid::session::hierarchy_ops::effective_topology_with_override`.
    ///
    /// Returns `None` if the session is unknown, no ancestor carries a
    /// topology, or the walk hits the depth cap (cycle protection).
    ///
    /// Per P1.3 (topology-on-epic): consumers must derive on read rather
    /// than trust `state.session.workflow_id`, which is `None` on every
    /// child spawned after the kill-the-copy change.
    pub fn effective_topology(&self, session_id: Uuid) -> Option<Uuid> {
        let state = self.sessions.get(&session_id)?;
        if state.session.workflow_id_override.is_some() {
            return state.session.workflow_id_override;
        }
        if state.session.workflow_id.is_some() {
            return state.session.workflow_id;
        }
        let mut cursor: Option<Uuid> = state.session.parent_id;
        let mut depth: u32 = 1;
        while let Some(pid) = cursor {
            if depth >= super::MAX_HIERARCHY_DEPTH {
                tracing::warn!(
                    %session_id,
                    depth,
                    "App::effective_topology depth cap reached — possible parent_id cycle"
                );
                return None;
            }
            let ancestor = self.sessions.get(&pid)?;
            if ancestor.session.workflow_id.is_some() {
                return ancestor.session.workflow_id;
            }
            cursor = ancestor.session.parent_id;
            depth += 1;
        }
        None
    }

    // --- Tab operations ---

    /// Create a new tab with a session list pane.
    pub fn create_tab(&mut self) {
        let pane_id = self.alloc_pane_id();
        let list_pane = Pane::SessionList {
            selected_index: 0,
            selected_session: self.session_id_at(0),
            scroll_offset: 0,
            active_zone: Default::default(),
            taskrabbit_selected_index: 0,
            archive_selected_index: 0,
            jobs_selected_index: 0,
        };
        let tab = Tab {
            name: format!("[{}]", self.tabs.len() + 1),
            session_list_state: list_pane.clone(),
            layout: SplitNode::Leaf {
                pane: list_pane,
                id: pane_id,
            },
            focused_pane: pane_id,
            project_id: self.active_project_id(),
            detail_split_offset: 0,
            layout_x_offset_adj: 0,
            session_list_width_pct: 0,
            descent_path: Vec::new(),
            bottom_focus_target: crate::types::BottomZone::Off,
            mini_dag_focus: None,
        };
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
    }

    /// Open a session directly in a new tab's detail view.
    pub fn open_session_in_new_tab(&mut self, session_id: Uuid) {
        // Auto-scroll to bottom when entering session detail
        if let Some(state) = self.sessions.get_mut(&session_id) {
            state.follow_tail = true;
            // D2: stamp-on-entry — this session's detail view is now seen.
            state.last_seen_events_generation = state.events_generation;
        }
        let pane_id = self.alloc_pane_id();
        let tab = Tab {
            name: String::new(),
            session_list_state: Tab::default_session_list_state(),
            layout: SplitNode::Leaf {
                pane: Pane::SessionDetail { session_id },
                id: pane_id,
            },
            focused_pane: pane_id,
            project_id: self.active_project_id(),
            detail_split_offset: 0,
            layout_x_offset_adj: 0,
            session_list_width_pct: 0,
            descent_path: Vec::new(),
            bottom_focus_target: crate::types::BottomZone::Off,
            mini_dag_focus: None,
        };
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
        self.sync_project_filter();
        self.last_viewed_session = Some(session_id);
        self.push_jumplist(session_id);
        self.run_navigation_effect_if_changed();
    }

    /// Open a session in a new vertical split pane.
    pub fn open_session_in_new_split(&mut self, session_id: Uuid) {
        if let Some(state) = self.sessions.get_mut(&session_id) {
            state.follow_tail = true;
            // D2: stamp-on-entry — this session's detail view is now seen.
            state.last_seen_events_generation = state.events_generation;
        }

        let new_pane_id = self.alloc_pane_id();
        let split_id = self.alloc_pane_id();

        let tab = &mut self.tabs[self.active_tab];
        let focused = tab.focused_pane;

        tab.layout = replace_node(tab.layout.clone(), focused, |original| SplitNode::Split {
            direction: SplitDirection::Vertical,
            first: Box::new(original),
            second: Box::new(SplitNode::Leaf {
                pane: Pane::SessionDetail { session_id },
                id: new_pane_id,
            }),
            id: split_id,
        });

        tab.focused_pane = new_pane_id;

        self.last_viewed_session = Some(session_id);
        self.push_jumplist(session_id);
        self.run_navigation_effect_if_changed();
    }

    /// Switch to the next workspace (L / gt).
    pub fn next_tab(&mut self) {
        if self.tabs.len() <= 1 {
            return;
        }
        self.active_tab = (self.active_tab + 1) % self.tabs.len();
        self.detail_list_focused = false;
        self.sync_project_filter();
        // View-switch effect: each tab carries its own descent_path,
        // so changing tabs is a navigation-node change. Fire the
        // useEffect-style dependency check so the new tab's active
        // node gets a targeted refresh.
        self.run_navigation_effect_if_changed();
    }

    /// Switch to the previous workspace (H / gT).
    pub fn prev_tab(&mut self) {
        if self.tabs.len() <= 1 {
            return;
        }
        self.active_tab = if self.active_tab == 0 {
            self.tabs.len() - 1
        } else {
            self.active_tab - 1
        };
        self.detail_list_focused = false;
        self.sync_project_filter();
        // View-switch effect: see `next_tab` for rationale.
        self.run_navigation_effect_if_changed();
    }

    /// Close the active workspace. If it's the last, quit.
    pub fn close_tab(&mut self) {
        if self.tabs.len() <= 1 {
            self.quit = true;
            return;
        }
        self.tabs.remove(self.active_tab);
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        }
        self.sync_project_filter();
        self.run_navigation_effect_if_changed();
    }

    // --- Split operations ---

    /// Split the focused pane. New pane opens a session list.
    pub fn split_focused(&mut self, direction: SplitDirection) {
        // Collect data from self before taking &mut tab (avoids borrow conflicts)
        let new_id = self.alloc_pane_id();
        let split_id = self.alloc_pane_id();
        let first_session = self.session_id_at(0);

        let tab = &mut self.tabs[self.active_tab];
        let focused = tab.focused_pane;

        // Find and replace the focused leaf with a split containing the original + new pane
        tab.layout = replace_node(tab.layout.clone(), focused, |original| SplitNode::Split {
            direction,
            first: Box::new(original),
            second: Box::new(SplitNode::Leaf {
                pane: Pane::SessionList {
                    selected_index: 0,
                    selected_session: first_session,
                    scroll_offset: 0,
                    active_zone: Default::default(),
                    taskrabbit_selected_index: 0,
                    archive_selected_index: 0,
                    jobs_selected_index: 0,
                },
                id: new_id,
            }),
            id: split_id,
        });

        // Keep focus on the original pane
    }

    /// Close the focused pane. If it's the only pane in the tab, close the tab.
    pub fn close_focused_pane(&mut self) {
        let focused = self.tabs[self.active_tab].focused_pane;
        let leaf_ids = self.tabs[self.active_tab].layout.leaf_ids();

        if leaf_ids.len() <= 1 {
            // Only one pane — close the tab
            self.close_tab();
            return;
        }

        // Find the next pane to focus
        let current_idx = leaf_ids.iter().position(|id| *id == focused).unwrap_or(0);
        let next_focus = if current_idx + 1 < leaf_ids.len() {
            leaf_ids[current_idx + 1]
        } else {
            leaf_ids[current_idx.saturating_sub(1)]
        };

        let tab = &mut self.tabs[self.active_tab];
        if let Some(new_layout) = tab.layout.clone().remove_pane(focused) {
            tab.layout = new_layout;
            tab.focused_pane = next_focus;
        }
        self.run_navigation_effect_if_changed();
    }

    /// Close all panes except the focused one (:only).
    pub fn close_other_panes(&mut self) {
        let tab = &self.tabs[self.active_tab];
        let focused = tab.focused_pane;
        // Clone the focused pane's data, then replace layout
        if let Some(pane) = tab.layout.find_pane(focused).cloned() {
            self.tabs[self.active_tab].layout = SplitNode::Leaf { pane, id: focused };
        }
    }

    /// Move focus to the neighboring pane in the given direction.
    pub fn focus_neighbor(&mut self, direction: super::NavDirection) {
        let tab = &mut self.tabs[self.active_tab];
        let leaf_ids = tab.layout.leaf_ids();
        if leaf_ids.len() <= 1 {
            return;
        }

        let current_idx = leaf_ids
            .iter()
            .position(|id| *id == tab.focused_pane)
            .unwrap_or(0);

        // Simplified: Left/Up goes to previous leaf, Right/Down goes to next leaf.
        // Full directional awareness requires position tracking (deferred to modalkit Phase 4C).
        let next_idx = match direction {
            super::NavDirection::Left | super::NavDirection::Up => {
                if current_idx > 0 {
                    current_idx - 1
                } else {
                    leaf_ids.len() - 1
                }
            }
            super::NavDirection::Right | super::NavDirection::Down => {
                (current_idx + 1) % leaf_ids.len()
            }
        };

        tab.focused_pane = leaf_ids[next_idx];
        self.run_navigation_effect_if_changed();
    }
}
