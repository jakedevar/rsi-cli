//! Navigation operations for App (inline pane lookup for disjoint field borrows).
//!
//! NOTE: These methods inline `self.tabs[self.active_tab].layout.find_pane_mut(...)`
//! instead of calling `self.focused_pane_mut()`. This is required because
//! `focused_pane_mut()` borrows ALL of `self`, but inlining the field access
//! only borrows `self.tabs`, leaving `self.session_order` and `self.sessions`
//! available for simultaneous access (Rust's disjoint field borrow rules).

use super::{App, EventApplyMode, FocusFetchResult, ModelSegmentsFetchResult};
use crate::types::{Pane, PaneId};
use uuid::Uuid;

impl App {
    /// Dispatch an on-demand `GetConversation` for a leaf detail view. This
    /// is called by the navigation-node effect when the active view changes
    /// to a leaf session.
    ///
    /// - Never-loaded sessions: `since_sequence = None` -> full replace.
    /// - Loaded sessions: `since_sequence = Some(last_sequence)` -> targeted
    ///   incremental refresh on the view switch.
    /// - Container kinds (Group/Epic) are skipped — they have no events.
    /// - Inflight dedup via `focus_fetch_inflight` prevents simultaneous
    ///   re-dispatch for the same session on rapid re-focus.
    ///
    /// Spawns a tokio task that owns its own `DaemonClient` (cloned socket
    /// path) and sends the result through `focus_fetch_tx`. The result is
    /// consumed by a `select!` arm in `event::run_event_loop` via
    /// `focus_fetch_rx`.
    pub(crate) fn trigger_focus_fetch_if_needed(&mut self, session_id: Uuid) {
        if self.focus_fetch_inflight.contains(&session_id) {
            return;
        }
        if self
            .navigation_cache
            .is_conversation_fetch_inflight(session_id)
        {
            return;
        }
        let Some(state) = self.sessions.get_mut(&session_id) else {
            return;
        };
        if !rsi_common::is_leaf_kind(state.session.session_kind) {
            return;
        }
        // Record navigation timing for poll suppression
        state.last_navigation_time = Some(std::time::Instant::now());
        let since_sequence = state.last_sequence;

        // Defensive: skip if no tokio runtime is currently registered.
        // Production always runs under run_event_loop's runtime, but
        // synchronous unit tests (e.g. `nav_down` smoke tests in
        // `app::tests`) call into this code without one. Without this
        // guard `tokio::spawn` panics.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };

        self.focus_fetch_inflight.insert(session_id);
        self.navigation_cache
            .mark_conversation_fetch_inflight(session_id);
        let socket_path = self.client.socket_path().to_path_buf();
        let tx = self.focus_fetch_tx.clone();
        tracing::trace!(
            target = "rsi::profile",
            session = %session_id,
            since = since_sequence.unwrap_or(-1),
            "focus_fetch_dispatch"
        );
        runtime.spawn(async move {
            let events = match tokio::time::timeout(std::time::Duration::from_secs(3), async {
                let mut client = crate::client::DaemonClient::new(socket_path);
                match client.connect().await {
                    Ok(()) => client
                        .get_conversation(session_id, since_sequence)
                        .await
                        .map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                }
            })
            .await
            {
                Ok(result) => result,
                Err(_) => Err("focus fetch timed out".to_string()),
            };
            let _ = tx.send(FocusFetchResult {
                session_id,
                since_sequence,
                events,
            });
        });
    }

    /// Consume a focus-fetch result delivered via `focus_fetch_rx`.
    /// Mirrors the post-processing path in `App::fetch_conversations` so
    /// behavior is identical to a periodic poll completing for the same
    /// cursor. Returns `true` when the TUI should redraw.
    pub(crate) fn apply_focus_fetch_result(&mut self, result: FocusFetchResult) -> bool {
        self.focus_fetch_inflight.remove(&result.session_id);
        let events = match result.events {
            Ok(ev) => ev,
            Err(err) => {
                tracing::debug!(
                    session = %result.session_id,
                    error = %err,
                    "focus_fetch error (non-fatal; next poll cycle will retry)"
                );
                return false;
            }
        };

        // Cache the conversation data
        let last_sequence = if let Some(state) = self.sessions.get(&result.session_id) {
            state.last_sequence
        } else {
            result.since_sequence
        };
        self.navigation_cache
            .cache_conversation(result.session_id, events.clone(), last_sequence);

        let event_count = events.len();
        let topology = self.effective_topology(result.session_id);
        // D2 (stamp-while-watching): resolve focus BEFORE the mutable borrow
        // below — `apply_session_events` takes `&mut SessionState` only, so
        // the caller (here) is responsible for the extra re-stamp when this
        // session is the currently-focused detail pane.
        let is_focused_detail = matches!(
            self.focused_pane(),
            Some(Pane::SessionDetail { session_id: sid }) if *sid == result.session_id
        );
        let workflows = &self.workflows;
        let dirty = if let Some(state) = self.sessions.get_mut(&result.session_id) {
            let mode = if result.since_sequence.is_some() {
                EventApplyMode::Append
            } else {
                EventApplyMode::Replace
            };
            let changed = Self::apply_session_events(state, events, mode, workflows, topology);
            if changed && is_focused_detail {
                state.last_seen_events_generation = state.events_generation;
            }
            changed
        } else {
            false
        };
        tracing::trace!(
            target = "rsi::profile",
            session = %result.session_id,
            events = event_count,
            dirty,
            "focus_fetch_apply"
        );
        dirty
    }

    /// Trigger model segments fetch for a session detail pane if not already in flight.
    /// This provides event-driven model segments loading when navigating to SessionDetail panes,
    /// replacing the periodic polling approach.
    pub(crate) fn trigger_model_segments_fetch_if_needed(&mut self, session_id: Uuid) {
        if self.model_segments_fetch_inflight.contains(&session_id) {
            return;
        }
        let Some(state) = self.sessions.get(&session_id) else {
            return;
        };
        if !rsi_common::is_leaf_kind(state.session.session_kind) {
            return;
        }

        // Defensive: skip if no tokio runtime is currently registered
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };

        self.model_segments_fetch_inflight.insert(session_id);
        let socket_path = self.client.socket_path().to_path_buf();
        let tx = self.model_segments_fetch_tx.clone();
        tracing::trace!(
            target = "rsi::profile",
            session = %session_id,
            "model_segments_fetch_dispatch"
        );
        runtime.spawn(async move {
            let segments = match tokio::time::timeout(std::time::Duration::from_secs(3), async {
                let mut client = crate::client::DaemonClient::new(socket_path);
                match client.connect().await {
                    Ok(()) => client
                        .get_model_segments(session_id)
                        .await
                        .map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                }
            })
            .await
            {
                Ok(result) => result,
                Err(_) => Err("model segments fetch timed out".to_string()),
            };
            let _ = tx.send(ModelSegmentsFetchResult {
                session_id,
                segments,
            });
        });
    }

    /// Consume a model segments fetch result and update the session state.
    /// Returns `true` when the TUI should redraw.
    pub(crate) fn apply_model_segments_fetch_result(
        &mut self,
        result: ModelSegmentsFetchResult,
    ) -> bool {
        self.model_segments_fetch_inflight
            .remove(&result.session_id);
        let segments = match result.segments {
            Ok(segments) => segments,
            Err(err) => {
                tracing::debug!(
                    session = %result.session_id,
                    error = %err,
                    "model_segments_fetch error (non-fatal)"
                );
                return false;
            }
        };

        if let Some(state) = self.sessions.get_mut(&result.session_id) {
            if state.model_segments != segments {
                state.model_segments = segments;
                state.events_generation += 1; // Invalidate height cache
                tracing::trace!(
                    target = "rsi::profile",
                    session = %result.session_id,
                    "model_segments_updated"
                );
                return true;
            }
        }
        false
    }

    /// Move selection down in a session list pane, or scroll down in detail pane.
    /// Zone-aware: j/k stays within the active zone (Main or TaskRabbit).
    pub fn nav_down(&mut self) {
        let main_len = self.filtered_session_order.len();
        let tr_len = self.filtered_taskrabbit_order.len();
        let arc_len = self.filtered_archived_order.len();
        let jobs_len = self.filtered_jobs_order.len();

        let focused = self.interaction_pane_id();
        let tab = &mut self.tabs[self.active_tab];
        if let Some(pane) = tab.find_pane_mut(focused) {
            match pane {
                Pane::SessionList {
                    selected_index,
                    selected_session,
                    active_zone,
                    taskrabbit_selected_index,
                    archive_selected_index,
                    jobs_selected_index,
                    ..
                } => match active_zone {
                    crate::types::SessionListZone::Main => {
                        if *selected_index + 1 < main_len {
                            *selected_index += 1;
                        }
                        *selected_session =
                            self.filtered_session_order.get(*selected_index).copied();
                    }
                    crate::types::SessionListZone::TaskRabbit => {
                        if *taskrabbit_selected_index + 1 < tr_len {
                            *taskrabbit_selected_index += 1;
                        }
                        *selected_session = self
                            .filtered_taskrabbit_order
                            .get(*taskrabbit_selected_index)
                            .copied();
                    }
                    crate::types::SessionListZone::Archive => {
                        if *archive_selected_index + 1 < arc_len {
                            *archive_selected_index += 1;
                        }
                        *selected_session = self
                            .filtered_archived_order
                            .get(*archive_selected_index)
                            .copied();
                    }
                    crate::types::SessionListZone::Jobs => {
                        if *jobs_selected_index + 1 < jobs_len {
                            *jobs_selected_index += 1;
                        }
                        *selected_session =
                            self.filtered_jobs_order.get(*jobs_selected_index).copied();
                    }
                },
                Pane::SessionDetail { session_id } => {
                    let sid = *session_id;
                    if let Some(state) = self.sessions.get_mut(&sid) {
                        state.scroll_offset = state.scroll_offset.saturating_add(1);
                        state.follow_tail = false;
                        state.follow_tail_hold = false;
                    }
                }
                Pane::Settings => {
                    // Drop the borrow before doing I/O for Hooks/Skills caches.
                }
                Pane::PromptCreator => {}
                Pane::Issues(_) => {}
            }
        }
        // For Settings the j/k handling needs `&mut App` (Hooks/Skills item
        // counts are I/O-backed via cached_*). Resolve after the layout
        // borrow above is released.
        if matches!(self.focused_pane(), Some(crate::types::Pane::Settings)) {
            self.settings_nav_down();
        }
    }

    /// Move selection up in a session list pane, or scroll up in detail pane.
    /// Zone-aware: j/k stays within the active zone (Main or TaskRabbit).
    pub fn nav_up(&mut self) {
        let focused = self.interaction_pane_id();
        let tab = &mut self.tabs[self.active_tab];
        if let Some(pane) = tab.find_pane_mut(focused) {
            match pane {
                Pane::SessionList {
                    selected_index,
                    selected_session,
                    active_zone,
                    taskrabbit_selected_index,
                    archive_selected_index,
                    jobs_selected_index,
                    ..
                } => match active_zone {
                    crate::types::SessionListZone::Main => {
                        if *selected_index > 0 {
                            *selected_index -= 1;
                        }
                        *selected_session =
                            self.filtered_session_order.get(*selected_index).copied();
                    }
                    crate::types::SessionListZone::TaskRabbit => {
                        if *taskrabbit_selected_index > 0 {
                            *taskrabbit_selected_index -= 1;
                        }
                        *selected_session = self
                            .filtered_taskrabbit_order
                            .get(*taskrabbit_selected_index)
                            .copied();
                    }
                    crate::types::SessionListZone::Archive => {
                        if *archive_selected_index > 0 {
                            *archive_selected_index -= 1;
                        }
                        *selected_session = self
                            .filtered_archived_order
                            .get(*archive_selected_index)
                            .copied();
                    }
                    crate::types::SessionListZone::Jobs => {
                        if *jobs_selected_index > 0 {
                            *jobs_selected_index -= 1;
                        }
                        *selected_session =
                            self.filtered_jobs_order.get(*jobs_selected_index).copied();
                    }
                },
                Pane::SessionDetail { session_id } => {
                    let sid = *session_id;
                    if let Some(state) = self.sessions.get_mut(&sid) {
                        state.scroll_offset = state.scroll_offset.saturating_sub(1);
                        state.follow_tail = false;
                        state.follow_tail_hold = false;
                    }
                }
                Pane::Settings => {
                    // Drop the borrow before doing I/O for Hooks/Skills caches.
                }
                Pane::PromptCreator => {}
                Pane::Issues(_) => {}
            }
        }
        if matches!(self.focused_pane(), Some(crate::types::Pane::Settings)) {
            self.settings_nav_up();
        }
    }

    /// Settings-pane j navigation. For Hooks / Skills, the item count is
    /// I/O-backed so we resolve it through the App-level cache rather than
    /// `UserSettings`.
    fn settings_nav_down(&mut self) {
        use crate::settings_registry::SettingsSection;
        match self.settings_state.focus {
            crate::types::SettingsFocus::Categories => {
                crate::settings_keys::nav_down(&mut self.settings_state, &self.settings);
            }
            crate::types::SettingsFocus::Items => {
                let max = match self.settings_state.section {
                    SettingsSection::ClaudeHooks => crate::settings_keys::hook_row_count(self),
                    SettingsSection::ClaudeSkills => crate::settings_keys::skill_row_count(self),
                    SettingsSection::Usage => crate::model_control_stats::stats_row_count(self),
                    SettingsSection::Budgets => {
                        crate::model_control_budgets::budget_row_count(self)
                    }
                    section => crate::settings_keys::item_count(section, &self.settings),
                };
                if self.settings_state.selected_index + 1 < max {
                    self.settings_state.selected_index += 1;
                }
            }
        }
    }

    fn settings_nav_up(&mut self) {
        crate::settings_keys::nav_up(&mut self.settings_state, &self.settings);
    }

    /// Enter a selected session's detail view (in the focused pane).
    pub fn enter_session(&mut self) {
        // First pass: extract the session_id (immutable borrow, temporary)
        let session_id = match self.focused_pane() {
            Some(Pane::SessionList {
                selected_session: Some(id),
                ..
            }) => *id,
            Some(Pane::SessionDetail { .. }) => {
                // In unified navigation mode: read from session list state
                match self.selected_session_id_from_list() {
                    Some(id) => id,
                    None => return,
                }
            }
            _ => return,
        };
        // Track as last viewed session (for 'i' in session list)
        self.last_viewed_session = Some(session_id);
        self.push_jumplist(session_id);
        // Auto-scroll to bottom when entering session detail
        if let Some(state) = self.sessions.get_mut(&session_id) {
            state.follow_tail = true;
            // D2: stamp-on-entry — this session's detail view is now seen.
            state.last_seen_events_generation = state.events_generation;
        }
        // Second pass: mutate the pane (previous borrow is dropped).
        // Save the SessionList state to the tab before overwriting, so the
        // detail list clone proxy can use it in single-pane layouts.
        let tab = &mut self.tabs[self.active_tab];
        let focused = tab.focused_pane;
        if let Some(pane) = tab.layout.find_pane_mut(focused) {
            if matches!(pane, Pane::SessionList { .. }) {
                tab.session_list_state = pane.clone();
            }
            *pane = Pane::SessionDetail { session_id };
        }
        self.detail_list_focused = false;
        self.pane_switch_clear = true;
        self.run_navigation_effect_if_changed();
    }

    /// Navigate the session list up (previous item), targeting the session list
    /// state directly — bypasses interaction_pane_id() proxy.
    /// Used by Left arrow key in session detail view.
    pub fn nav_list_up(&mut self) {
        let tab = &mut self.tabs[self.active_tab];

        // Determine which pane holds the session list state:
        // 1. Check the layout tree for a real SessionList pane (split layouts)
        // 2. Fall back to tab.session_list_state (single-pane detail view)
        let pane = {
            let mut found_id = None;
            for pid in tab.layout.leaf_ids() {
                if matches!(tab.layout.find_pane(pid), Some(Pane::SessionList { .. })) {
                    found_id = Some(pid);
                    break;
                }
            }
            if let Some(pid) = found_id {
                tab.layout.find_pane_mut(pid)
            } else {
                Some(&mut tab.session_list_state)
            }
        };

        if let Some(Pane::SessionList {
            selected_index,
            selected_session,
            active_zone,
            taskrabbit_selected_index,
            archive_selected_index,
            jobs_selected_index,
            ..
        }) = pane
        {
            match active_zone {
                crate::types::SessionListZone::Main => {
                    if *selected_index > 0 {
                        *selected_index -= 1;
                    }
                    *selected_session = self.filtered_session_order.get(*selected_index).copied();
                }
                crate::types::SessionListZone::TaskRabbit => {
                    if *taskrabbit_selected_index > 0 {
                        *taskrabbit_selected_index -= 1;
                    }
                    *selected_session = self
                        .filtered_taskrabbit_order
                        .get(*taskrabbit_selected_index)
                        .copied();
                }
                crate::types::SessionListZone::Archive => {
                    if *archive_selected_index > 0 {
                        *archive_selected_index -= 1;
                    }
                    *selected_session = self
                        .filtered_archived_order
                        .get(*archive_selected_index)
                        .copied();
                }
                crate::types::SessionListZone::Jobs => {
                    if *jobs_selected_index > 0 {
                        *jobs_selected_index -= 1;
                    }
                    *selected_session = self.filtered_jobs_order.get(*jobs_selected_index).copied();
                }
            }
        }
    }

    /// Navigate the session list down (next item), targeting the session list
    /// state directly — bypasses interaction_pane_id() proxy.
    /// Used by Right arrow key in session detail view.
    pub fn nav_list_down(&mut self) {
        let main_len = self.filtered_session_order.len();
        let tr_len = self.filtered_taskrabbit_order.len();
        let arc_len = self.filtered_archived_order.len();
        let jobs_len = self.filtered_jobs_order.len();

        let tab = &mut self.tabs[self.active_tab];
        let pane = {
            let mut found_id = None;
            for pid in tab.layout.leaf_ids() {
                if matches!(tab.layout.find_pane(pid), Some(Pane::SessionList { .. })) {
                    found_id = Some(pid);
                    break;
                }
            }
            if let Some(pid) = found_id {
                tab.layout.find_pane_mut(pid)
            } else {
                Some(&mut tab.session_list_state)
            }
        };

        if let Some(Pane::SessionList {
            selected_index,
            selected_session,
            active_zone,
            taskrabbit_selected_index,
            archive_selected_index,
            jobs_selected_index,
            ..
        }) = pane
        {
            match active_zone {
                crate::types::SessionListZone::Main => {
                    if *selected_index + 1 < main_len {
                        *selected_index += 1;
                    }
                    *selected_session = self.filtered_session_order.get(*selected_index).copied();
                }
                crate::types::SessionListZone::TaskRabbit => {
                    if *taskrabbit_selected_index + 1 < tr_len {
                        *taskrabbit_selected_index += 1;
                    }
                    *selected_session = self
                        .filtered_taskrabbit_order
                        .get(*taskrabbit_selected_index)
                        .copied();
                }
                crate::types::SessionListZone::Archive => {
                    if *archive_selected_index + 1 < arc_len {
                        *archive_selected_index += 1;
                    }
                    *selected_session = self
                        .filtered_archived_order
                        .get(*archive_selected_index)
                        .copied();
                }
                crate::types::SessionListZone::Jobs => {
                    if *jobs_selected_index + 1 < jobs_len {
                        *jobs_selected_index += 1;
                    }
                    *selected_session = self.filtered_jobs_order.get(*jobs_selected_index).copied();
                }
            }
        }
    }

    /// Get the selected selected session ID from the session list state (bypasses interaction_pane_id).
    fn selected_session_id_from_list(&self) -> Option<Uuid> {
        let tab = self.active_tab();
        // Check layout tree first
        for pid in tab.layout.leaf_ids() {
            if let Some(Pane::SessionList {
                selected_session, ..
            }) = tab.layout.find_pane(pid)
            {
                return *selected_session;
            }
        }
        // Fall back to tab-stored state
        if let Pane::SessionList {
            selected_session, ..
        } = &tab.session_list_state
        {
            return *selected_session;
        }
        None
    }

    /// Open a specific session in the current pane (without requiring selection).
    /// Used by 'i' keybinding to jump to last viewed session from session list.
    pub fn open_session_in_current_pane(&mut self, session_id: Uuid) {
        self.last_viewed_session = Some(session_id);
        self.push_jumplist(session_id);
        // Auto-scroll to bottom when entering session detail
        if let Some(state) = self.sessions.get_mut(&session_id) {
            state.follow_tail = true;
            // D2: stamp-on-entry — this session's detail view is now seen.
            state.last_seen_events_generation = state.events_generation;
        }
        let tab = &mut self.tabs[self.active_tab];
        let focused = tab.focused_pane;
        if let Some(pane) = tab.layout.find_pane_mut(focused) {
            *pane = Pane::SessionDetail { session_id };
        }
        self.pane_switch_clear = true;
        self.run_navigation_effect_if_changed();
    }

    /// Go back to session list in the focused pane.
    /// Restores cursor to the session that was being viewed.
    pub fn back_to_list(&mut self) {
        let focused = self.tabs[self.active_tab].focused_pane;

        let session_id = match self.tabs[self.active_tab].layout.find_pane(focused) {
            Some(Pane::SessionDetail { session_id }) => *session_id,
            _ => return,
        };

        // Determine which zone and index the session belongs to
        let (zone, main_idx, tr_idx, arc_idx, sel) = if let Some(pos) = self
            .filtered_session_order
            .iter()
            .position(|x| *x == session_id)
        {
            (
                crate::types::SessionListZone::Main,
                pos,
                0,
                0,
                Some(session_id),
            )
        } else if let Some(pos) = self
            .filtered_taskrabbit_order
            .iter()
            .position(|x| *x == session_id)
        {
            (
                crate::types::SessionListZone::TaskRabbit,
                0,
                pos,
                0,
                Some(session_id),
            )
        } else if let Some(pos) = self
            .filtered_archived_order
            .iter()
            .position(|x| *x == session_id)
        {
            (
                crate::types::SessionListZone::Archive,
                0,
                0,
                pos,
                Some(session_id),
            )
        } else {
            let first = self
                .filtered_session_order
                .first()
                .or(self.filtered_taskrabbit_order.first())
                .or(self.filtered_archived_order.first())
                .copied();
            (Default::default(), 0, 0, 0, first)
        };

        if let Some(pane) = self.tabs[self.active_tab].layout.find_pane_mut(focused) {
            *pane = Pane::SessionList {
                selected_index: main_idx,
                selected_session: sel,
                scroll_offset: 0,
                active_zone: zone,
                taskrabbit_selected_index: tr_idx,
                archive_selected_index: arc_idx,
                jobs_selected_index: 0,
            };
        }
        self.detail_list_focused = false;
        self.pane_switch_clear = true;
        self.recalculate_filtered_order();
        self.run_navigation_effect_if_changed();

        let current_parent_id = self
            .tabs
            .get(self.active_tab)
            .and_then(|tab| tab.descent_path.last().copied());
        self.trigger_hierarchy_fetch_immediate(current_parent_id);
    }

    /// Convert all `SessionDetail` panes referencing `session_id` back to `SessionList`.
    /// Used after delete/archive so stale detail views fall back to the list.
    pub(crate) fn revert_detail_panes_for(&mut self, session_id: uuid::Uuid) {
        // Compute fallback session before mutable borrow
        let first_session = self
            .filtered_session_order
            .first()
            .or(self.filtered_taskrabbit_order.first())
            .or(self.filtered_archived_order.first())
            .copied();

        for tab in &mut self.tabs {
            let ids: Vec<PaneId> = tab.layout.leaf_ids();
            for leaf_id in ids {
                if let Some(pane @ Pane::SessionDetail { .. }) = tab.layout.find_pane_mut(leaf_id)
                    && matches!(pane, Pane::SessionDetail { session_id: sid } if *sid == session_id)
                {
                    *pane = Pane::SessionList {
                        selected_index: 0,
                        selected_session: first_session,
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

    /// Jump to first item in session list (zone-aware), or top of conversation in detail.
    pub fn jump_to_top(&mut self) {
        let focused = self.interaction_pane_id();

        // Snapshot the active zone before mutable borrow
        let zone = match self.tabs[self.active_tab].find_pane(focused) {
            Some(Pane::SessionList { active_zone, .. }) => Some(*active_zone),
            _ => None,
        };

        match self.tabs[self.active_tab].find_pane(focused) {
            Some(Pane::SessionList { .. }) => {
                if let Some(pane) = self.tabs[self.active_tab].find_pane_mut(focused) {
                    if let Pane::SessionList {
                        selected_index,
                        selected_session,
                        taskrabbit_selected_index,
                        archive_selected_index,
                        jobs_selected_index,
                        ..
                    } = pane
                    {
                        match zone.unwrap_or_default() {
                            crate::types::SessionListZone::Main => {
                                *selected_index = 0;
                                *selected_session = self.filtered_session_order.first().copied();
                            }
                            crate::types::SessionListZone::TaskRabbit => {
                                *taskrabbit_selected_index = 0;
                                *selected_session = self.filtered_taskrabbit_order.first().copied();
                            }
                            crate::types::SessionListZone::Archive => {
                                *archive_selected_index = 0;
                                *selected_session = self.filtered_archived_order.first().copied();
                            }
                            crate::types::SessionListZone::Jobs => {
                                *jobs_selected_index = 0;
                                *selected_session = self.filtered_jobs_order.first().copied();
                            }
                        }
                    }
                }
            }
            Some(Pane::SessionDetail { session_id }) => {
                let sid = *session_id;
                if let Some(state) = self.sessions.get_mut(&sid) {
                    state.scroll_offset = 0;
                    state.follow_tail = false;
                    state.clear_next_render = true;
                    if !state.events.is_empty() {
                        state.current_event_index = Some(0);
                    }
                }
            }
            _ => {}
        }
    }

    /// Jump to last item in session list (zone-aware).
    pub fn jump_to_bottom(&mut self) {
        let main_len = self.filtered_session_order.len();
        let tr_len = self.filtered_taskrabbit_order.len();
        let arc_len = self.filtered_archived_order.len();
        let jobs_len = self.filtered_jobs_order.len();

        let focused = self.interaction_pane_id();

        // Snapshot zone
        let zone = match self.tabs[self.active_tab].find_pane(focused) {
            Some(Pane::SessionList { active_zone, .. }) => Some(*active_zone),
            _ => None,
        };

        match self.tabs[self.active_tab].find_pane(focused) {
            Some(Pane::SessionList { .. }) => {
                if let Some(pane) = self.tabs[self.active_tab].find_pane_mut(focused) {
                    if let Pane::SessionList {
                        selected_index,
                        selected_session,
                        taskrabbit_selected_index,
                        archive_selected_index,
                        jobs_selected_index,
                        ..
                    } = pane
                    {
                        match zone.unwrap_or_default() {
                            crate::types::SessionListZone::Main => {
                                if main_len > 0 {
                                    *selected_index = main_len - 1;
                                    *selected_session = self.filtered_session_order.last().copied();
                                }
                            }
                            crate::types::SessionListZone::TaskRabbit => {
                                if tr_len > 0 {
                                    *taskrabbit_selected_index = tr_len - 1;
                                    *selected_session =
                                        self.filtered_taskrabbit_order.last().copied();
                                }
                            }
                            crate::types::SessionListZone::Archive => {
                                if arc_len > 0 {
                                    *archive_selected_index = arc_len - 1;
                                    *selected_session =
                                        self.filtered_archived_order.last().copied();
                                }
                            }
                            crate::types::SessionListZone::Jobs => {
                                if jobs_len > 0 {
                                    *jobs_selected_index = jobs_len - 1;
                                    *selected_session = self.filtered_jobs_order.last().copied();
                                }
                            }
                        }
                    }
                }
            }
            Some(Pane::SessionDetail { session_id }) => {
                let sid = *session_id;
                if let Some(state) = self.sessions.get_mut(&sid) {
                    if let Some(idx) = (0..state.events.len())
                        .rev()
                        .find(|&i| state.event_heights.get(i).copied().unwrap_or(0) > 0)
                        && !state.event_offsets.is_empty()
                    {
                        state.scroll_offset = state.event_offsets[idx];
                        state.current_event_index = Some(idx);
                        state.follow_tail = false;
                    } else {
                        state.follow_tail = true;
                    }
                    state.clear_next_render = true;
                }
            }
            Some(Pane::Settings) | Some(Pane::PromptCreator) | Some(Pane::Issues(_)) | None => {}
        }
    }
}

#[cfg(test)]
mod focus_fetch_tests {
    //! Phase 2 unit tests: focus-change on-demand fetch (Option A).
    //!
    //! Tests that exercise `trigger_focus_fetch_if_needed` use
    //! `#[tokio::test]` because the dispatch path calls `tokio::spawn`,
    //! which panics outside a tokio runtime. The spawned task tries to
    //! connect to a fake socket path and fails harmlessly; we only assert
    //! the synchronous side effects (inflight set membership, no spawn at
    //! all when gated).

    use super::*;
    use crate::client::DaemonClient;
    use crate::types::SessionState;
    use rsi_common::types::{
        ConversationEvent, EventType, Session, SessionKind, SessionProvider, SessionStatus,
    };
    use std::path::PathBuf;

    fn test_app() -> App {
        crate::state::DevState::clear();
        crate::state::PersistedState::default().save();
        App::new(DaemonClient::new(PathBuf::from("/tmp/test.sock")))
    }

    fn baseline_session(id: Uuid, kind: SessionKind) -> Session {
        Session {
            context_fill_pct: None,
            id,
            provider: SessionProvider::Claude,
            claude_session_id: None,
            query: "test".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            pending_question: None,
            pending_archive: false,
            working_dir: PathBuf::from("/tmp"),
            git_branch: None,
            status: SessionStatus::Running,
            project_id: None,
            session_kind: kind,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            context_usage_confidence: rsi_common::types::ContextUsageConfidence::default(),
            continued_from: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            scheduled_job_id: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: None,
            work_time_ms: None,
            approval_started_at: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: None,
            lead_session_id: None,
            is_eval: false,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        }
    }

    #[test]
    fn list_navigation_does_not_change_explicit_fold_state() {
        let mut app = test_app();
        let first_id = Uuid::new_v4();
        let second_id = Uuid::new_v4();
        app.filtered_session_order = vec![first_id, second_id];
        app.sessions.insert(
            first_id,
            SessionState::new(baseline_session(first_id, SessionKind::Group)),
        );
        app.sessions.insert(
            second_id,
            SessionState::new(baseline_session(second_id, SessionKind::Epic)),
        );
        if let Some(Pane::SessionList {
            selected_index,
            selected_session,
            ..
        }) = app.focused_pane_mut()
        {
            *selected_index = 0;
            *selected_session = Some(first_id);
        }

        app.nav_down();

        assert_eq!(app.selected_session_id(), Some(second_id));
        assert!(!app.sessions[&first_id].list_card_expanded);
        assert!(!app.sessions[&second_id].list_card_expanded);
    }

    fn mk_event(session_id: Uuid, sequence: i32) -> ConversationEvent {
        ConversationEvent {
            id: sequence as i64,
            session_id,
            sequence,
            event_type: EventType::Message,
            role: Some(rsi_common::types::Role::Assistant),
            created_at: chrono::Utc::now(),
            content: format!("event {}", sequence),
            tool_name: None,
            tool_input: None,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    #[tokio::test]
    async fn focus_fetch_skips_containers() {
        let mut app = test_app();
        let group_id = Uuid::new_v4();
        app.sessions.insert(
            group_id,
            SessionState::new(baseline_session(group_id, SessionKind::Group)),
        );

        app.trigger_focus_fetch_if_needed(group_id);
        assert!(
            app.focus_fetch_inflight.is_empty(),
            "container kind must not enqueue a fetch"
        );
    }

    #[tokio::test]
    async fn focus_fetch_incremental_for_loaded_sessions() {
        let mut app = test_app();
        let session_id = Uuid::new_v4();
        let mut state = SessionState::new(baseline_session(session_id, SessionKind::Standard));
        // Mark as already loaded; a detail-view switch should issue an
        // incremental fetch from the last known sequence.
        state.events.push(mk_event(session_id, 1));
        state.last_sequence = Some(1);
        app.sessions.insert(session_id, state);

        app.trigger_focus_fetch_if_needed(session_id);
        assert!(
            app.focus_fetch_inflight.contains(&session_id),
            "loaded leaf detail view should enqueue an incremental fetch"
        );
    }

    #[tokio::test]
    async fn enter_session_runs_leaf_navigation_effect() {
        let mut app = test_app();
        let session_id = Uuid::new_v4();
        app.sessions.insert(
            session_id,
            SessionState::new(baseline_session(session_id, SessionKind::Standard)),
        );
        if let Some(Pane::SessionList {
            selected_session, ..
        }) = app.focused_pane_mut()
        {
            *selected_session = Some(session_id);
        }

        app.enter_session();

        assert_eq!(
            app.navigation_effect_node,
            Some(crate::app::NavigationEffectNode::Leaf(session_id))
        );
        assert!(
            app.focus_fetch_inflight.contains(&session_id),
            "detail view switch must dispatch the leaf fetch immediately"
        );
    }

    #[tokio::test]
    async fn open_session_in_current_pane_runs_leaf_navigation_effect() {
        let mut app = test_app();
        let session_id = Uuid::new_v4();
        app.sessions.insert(
            session_id,
            SessionState::new(baseline_session(session_id, SessionKind::Standard)),
        );

        app.open_session_in_current_pane(session_id);

        assert_eq!(
            app.navigation_effect_node,
            Some(crate::app::NavigationEffectNode::Leaf(session_id))
        );
        assert!(
            app.focus_fetch_inflight.contains(&session_id),
            "programmatic detail jumps must not wait for the fallback poll"
        );
    }

    #[tokio::test]
    async fn focus_fetch_skips_inflight() {
        let mut app = test_app();
        let session_id = Uuid::new_v4();
        app.sessions.insert(
            session_id,
            SessionState::new(baseline_session(session_id, SessionKind::Standard)),
        );
        // Pre-mark as inflight (e.g. the previous focus-change already
        // dispatched and the result hasn't landed yet).
        app.focus_fetch_inflight.insert(session_id);

        app.trigger_focus_fetch_if_needed(session_id);
        assert_eq!(
            app.focus_fetch_inflight.len(),
            1,
            "duplicate dispatch must not double-insert into inflight set"
        );
    }

    #[test]
    fn focus_fetch_clears_inflight_on_result() {
        let mut app = test_app();
        let session_id = Uuid::new_v4();
        app.sessions.insert(
            session_id,
            SessionState::new(baseline_session(session_id, SessionKind::Standard)),
        );
        app.focus_fetch_inflight.insert(session_id);

        let result = FocusFetchResult {
            session_id,
            since_sequence: None,
            events: Ok(vec![]),
        };
        let _ = app.apply_focus_fetch_result(result);
        assert!(
            !app.focus_fetch_inflight.contains(&session_id),
            "apply_focus_fetch_result must clear inflight regardless of result"
        );
    }

    #[test]
    fn focus_fetch_clears_inflight_on_error() {
        let mut app = test_app();
        let session_id = Uuid::new_v4();
        app.sessions.insert(
            session_id,
            SessionState::new(baseline_session(session_id, SessionKind::Standard)),
        );
        app.focus_fetch_inflight.insert(session_id);

        let result = FocusFetchResult {
            session_id,
            since_sequence: None,
            events: Err("simulated daemon disconnect".to_string()),
        };
        let dirty = app.apply_focus_fetch_result(result);
        assert!(
            !dirty,
            "error path must not mark the UI dirty (nothing changed)"
        );
        assert!(
            !app.focus_fetch_inflight.contains(&session_id),
            "error path must still clear inflight so retry can happen"
        );
    }

    #[test]
    fn apply_focus_fetch_result_replace_when_unloaded() {
        let mut app = test_app();
        let session_id = Uuid::new_v4();
        let state = SessionState::new(baseline_session(session_id, SessionKind::Standard));
        // Confirm unloaded: empty events, no last_sequence.
        assert!(state.events.is_empty());
        assert!(state.last_sequence.is_none());
        app.sessions.insert(session_id, state);

        let events = vec![mk_event(session_id, 1), mk_event(session_id, 2)];
        let result = FocusFetchResult {
            session_id,
            since_sequence: None,
            events: Ok(events),
        };
        app.apply_focus_fetch_result(result);

        let after = &app.sessions[&session_id];
        assert_eq!(after.events.len(), 2, "Replace mode should populate events");
        assert_eq!(after.last_sequence, Some(2), "last_sequence should track");
    }

    #[test]
    fn apply_focus_fetch_result_append_when_loaded() {
        let mut app = test_app();
        let session_id = Uuid::new_v4();
        let mut state = SessionState::new(baseline_session(session_id, SessionKind::Standard));
        state.events.push(mk_event(session_id, 1));
        state.last_sequence = Some(1);
        app.sessions.insert(session_id, state);

        let new_events = vec![mk_event(session_id, 2), mk_event(session_id, 3)];
        let result = FocusFetchResult {
            session_id,
            since_sequence: Some(1),
            events: Ok(new_events),
        };
        app.apply_focus_fetch_result(result);

        let after = &app.sessions[&session_id];
        assert_eq!(
            after.events.len(),
            3,
            "Append mode should add to existing events"
        );
        assert_eq!(after.last_sequence, Some(3));
    }
}
