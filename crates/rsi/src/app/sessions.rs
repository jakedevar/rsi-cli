//! Session list management for App.

use super::App;
use crate::types::{Pane, PaneId, SessionState};
use crate::ui::session::resolve_session_display;
use rsi_common::types::Session;
use std::collections::HashSet;
use uuid::Uuid;

fn is_visible_in_session_list(status: rsi_common::types::SessionStatus) -> bool {
    !matches!(
        status,
        rsi_common::types::SessionStatus::Archived | rsi_common::types::SessionStatus::Deleted
    )
}

fn is_visible_session_state(state: &SessionState) -> bool {
    is_visible_in_session_list(state.session.status)
}

fn is_jobs_zone_session(session: &Session) -> bool {
    // Fresh scheduled launches are root-level sessions created by the
    // scheduler. Resume/watch deliveries wake an existing session and should
    // keep that session in its hierarchy, even if an older daemon stamped the
    // wake job id onto the row.
    session.scheduled_job_id.is_some() && session.parent_id.is_none()
}

impl App {
    fn upsert_visible_session(&mut self, session: Session) -> bool {
        if !is_visible_in_session_list(session.status) {
            return false;
        }

        let id = session.id;
        if let Some(state) = self.sessions.get_mut(&id) {
            let old_status = state.session.status;
            let new_status = session.status;
            let session_kind = session.session_kind;
            let mut changed = false;
            if state.session.updated_at != session.updated_at
                || old_status != new_status
                || state.session.project_id != session.project_id
                || state.session.pinned_at != session.pinned_at
                || state.session.parent_id != session.parent_id
                || state.session.lead_session_id != session.lead_session_id
                || state.session.context_fill_pct != session.context_fill_pct
                || state.session.context_usage_confidence != session.context_usage_confidence
                || state.session.context_window != session.context_window
                || state.session.input_tokens != session.input_tokens
                || state.session.resolved_context_budget != session.resolved_context_budget
            {
                changed = true;
            }
            // Preserve high-watermark for total_input_tokens across poll overwrite.
            // Push events may set a daemon-estimated value (API baseline + daemon delta)
            // that's higher than the daemon's API-only tracked.session.total_input_tokens.
            let prev_total_input = state.session.total_input_tokens;
            state.session = session;
            if let Some(prev) = prev_total_input {
                let new_val = state.session.total_input_tokens.unwrap_or(0);
                if prev > new_val {
                    state.session.total_input_tokens = Some(prev);
                }
            }

            // Display-only: seed the live counter from the daemon-stamped
            // `context_fill_pct` (idle/historical sessions get a bar without the
            // TUI recomputing). Live bus events refresh `live_context_pct` for
            // active sessions; this only fills the gap before the first event.
            if state.live_context_pct.is_none() {
                state.live_context_pct = state.session.context_fill_pct;
            }

            // Detect TaskRabbit/Bug completion/failure for notification
            if old_status != new_status {
                match session_kind {
                    rsi_common::types::SessionKind::TaskRabbit => match new_status {
                        rsi_common::types::SessionStatus::Completed => {
                            self.push_notification(
                                crate::types::NotificationKind::TaskRabbitComplete,
                                crate::types::NotificationPriority::Medium,
                                "TaskRabbit: done ✓".to_string(),
                                Some(id),
                            );
                        }
                        rsi_common::types::SessionStatus::Failed => {
                            self.push_notification(
                                crate::types::NotificationKind::TaskRabbitFailed,
                                crate::types::NotificationPriority::High,
                                "TaskRabbit: failed ✗".to_string(),
                                Some(id),
                            );
                        }
                        _ => {}
                    },
                    rsi_common::types::SessionKind::Bug => match new_status {
                        rsi_common::types::SessionStatus::Completed => {
                            self.push_notification(
                                crate::types::NotificationKind::BugComplete,
                                crate::types::NotificationPriority::Medium,
                                "Bug: done ✓".to_string(),
                                Some(id),
                            );
                        }
                        rsi_common::types::SessionStatus::Failed => {
                            self.push_notification(
                                crate::types::NotificationKind::BugFailed,
                                crate::types::NotificationPriority::High,
                                "Bug: failed ✗".to_string(),
                                Some(id),
                            );
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }

            changed
        } else {
            self.session_order.push(id);
            let mut new_state = SessionState::new(session);
            new_state.show_system_events = self.settings.default_show_system_events;
            new_state.show_thinking_events = self.settings.default_show_thinking_events;
            new_state.show_tool_results = !self.settings.default_hide_tool_results;
            self.sessions.insert(id, new_state);
            if let Some(placement) = self.accepted_launch_placements.remove(&id) {
                self.apply_launch_placement(id, placement);
            }
            true
        }
    }

    pub(crate) fn place_accepted_launch(
        &mut self,
        session_id: Uuid,
        placement: super::LaunchPlacement,
    ) {
        if placement == super::LaunchPlacement::CurrentPane {
            return;
        }
        if self.sessions.contains_key(&session_id) {
            self.apply_launch_placement(session_id, placement);
        } else {
            self.accepted_launch_placements
                .insert(session_id, placement);
        }
    }

    fn apply_launch_placement(&mut self, session_id: Uuid, placement: super::LaunchPlacement) {
        match placement {
            super::LaunchPlacement::CurrentPane => {}
            super::LaunchPlacement::NewTab => self.open_session_in_new_tab(session_id),
            super::LaunchPlacement::NewSplit => self.open_session_in_new_split(session_id),
        }
    }

    fn finalize_session_update(
        &mut self,
        changed: bool,
        preserve_visual_position: bool,
        authoritative_snapshot: bool,
    ) -> bool {
        // Sort by staleness: oldest updated_at first (longest without attention at top)
        self.sort_sessions(preserve_visual_position);
        if authoritative_snapshot {
            self.mark_hierarchy_snapshot_loaded();
        }

        // Invalidate card height cache when session data changes
        if changed {
            self.invalidate_card_cache();
        }

        // Sync panes that have no selection yet (e.g. startup before first nav_down)
        for tab in &mut self.tabs {
            let ids: Vec<PaneId> = tab.layout.leaf_ids();
            for leaf_id in ids {
                if let Some(Pane::SessionList {
                    selected_index,
                    selected_session,
                    ..
                }) = tab.layout.find_pane_mut(leaf_id)
                    && selected_session.is_none()
                {
                    *selected_session = self.filtered_session_order.get(*selected_index).copied();
                }
            }
        }

        changed
    }

    // --- Children Index (RSI hierarchy nav latency refactor: Option B) ---

    /// Recompute `children_by_parent` from current `session_order` + `sessions`.
    ///
    /// O(N) in session count. Called from `sort_sessions` (after the sort) and
    /// from the top of `recalculate_filtered_order` so any push-handler path
    /// that bypasses sort (e.g. `session_deleted`) still sees a consistent
    /// index when the filter pipeline reads it.
    ///
    /// Bucket ordering follows `session_order` (which the sort step has just
    /// rewritten), so downstream consumers do not need to re-sort.
    pub(crate) fn rebuild_children_index(&mut self) {
        self.children_by_parent.clear();
        for id in &self.session_order {
            if let Some(state) = self.sessions.get(id) {
                self.children_by_parent
                    .entry(state.session.parent_id)
                    .or_default()
                    .push(*id);
            }
        }
    }

    /// Debug-only invariant check: every entry in `session_order` MUST be
    /// in exactly one bucket of `children_by_parent`, keyed by its current
    /// `parent_id`. Total bucket size equals `session_order.len()`.
    ///
    /// `session_order` is the source-of-truth ordering and is what the
    /// rebuild iterates -- the invariant follows the rebuild semantics
    /// rather than `sessions.len()` so transient states where a session
    /// exists in `self.sessions` but not yet in `session_order` (or
    /// vice-versa, post test-fixture inserts) don't trip the assert.
    #[cfg(debug_assertions)]
    pub(crate) fn assert_children_index_invariant(&self) {
        let total: usize = self.children_by_parent.values().map(Vec::len).sum();
        debug_assert_eq!(
            total,
            self.session_order.len(),
            "children_by_parent bucket sum ({}) != session_order.len() ({})",
            total,
            self.session_order.len()
        );
        for id in &self.session_order {
            let Some(state) = self.sessions.get(id) else {
                // Stale session_order entry pointing at a removed session.
                // The next sort_sessions call cleans it up; not a bucket bug.
                continue;
            };
            let key = state.session.parent_id;
            let bucket = self
                .children_by_parent
                .get(&key)
                .expect("children_by_parent missing bucket for known parent_id");
            debug_assert!(
                bucket.contains(id),
                "session {} not in its parent_id={:?} bucket",
                id,
                key
            );
        }
    }

    // --- Session List Navigation ---

    /// Total number of navigable items in the session list (main + taskrabbit, excluding separators).
    pub fn session_list_len(&self) -> usize {
        self.filtered_session_order.len() + self.filtered_taskrabbit_order.len()
    }

    /// Get session ID by combined index (skipping separator).
    /// Indices 0..main.len() -> main sessions
    /// Indices main.len()..total -> taskrabbit sessions
    pub fn session_id_at(&self, index: usize) -> Option<Uuid> {
        let main_len = self.filtered_session_order.len();
        if index < main_len {
            self.filtered_session_order.get(index).copied()
        } else {
            self.filtered_taskrabbit_order
                .get(index - main_len)
                .copied()
        }
    }

    /// Find combined index for a session ID.
    pub fn session_index_of(&self, id: &Uuid) -> Option<usize> {
        let main_len = self.filtered_session_order.len();
        if let Some(pos) = self.filtered_session_order.iter().position(|x| x == id) {
            Some(pos)
        } else {
            self.filtered_taskrabbit_order
                .iter()
                .position(|x| x == id)
                .map(|pos| main_len + pos)
        }
    }

    /// Update session state from daemon response.
    pub(crate) fn update_sessions(&mut self, sessions: Vec<Session>) -> bool {
        let mut changed = false;
        let sessions: Vec<Session> = sessions
            .into_iter()
            .filter(|session| is_visible_in_session_list(session.status))
            .collect();

        // Remove sessions no longer returned by the daemon (e.g. archived by rotation),
        // but never evict sessions currently being viewed in a SessionDetail pane — an
        // archived session opened via the archive browser lives in app.sessions but is
        // intentionally absent from ListSessions (which excludes archived ones).
        let daemon_ids: HashSet<Uuid> = sessions.iter().map(|s| s.id).collect();
        let pinned_in_panes: HashSet<Uuid> = self
            .tabs
            .iter()
            .flat_map(|tab| {
                tab.layout
                    .leaf_ids()
                    .into_iter()
                    .filter_map(|leaf_id| match tab.layout.find_pane(leaf_id) {
                        Some(Pane::SessionDetail { session_id }) => Some(*session_id),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let removed_ids: Vec<Uuid> = self
            .session_order
            .iter()
            .filter(|id| !daemon_ids.contains(id) && !pinned_in_panes.contains(id))
            .copied()
            .collect();
        if removed_ids
            .iter()
            .any(|id| self.manager_roster.contains(*id))
        {
            self.manager_roster.request_refresh();
        }
        for id in &removed_ids {
            self.sessions.remove(id);
            changed = true;
        }
        if !removed_ids.is_empty() {
            self.session_order.retain(|id| !removed_ids.contains(id));
        }

        for session in sessions {
            self.note_manager_turnover(&session);
            changed |= self.upsert_visible_session(session);
        }

        self.finalize_session_update(changed, true, true)
    }

    pub(crate) fn upsert_session(&mut self, session: Session) -> bool {
        self.note_manager_turnover(&session);
        let changed = self.upsert_visible_session(session);
        self.finalize_session_update(changed, true, false)
    }

    /// Sort `session_order` according to the current `sort_order`.
    /// Recalculates filtered order and fixes up all SessionList pane selections.
    ///
    /// When `preserve_visual_position` is `true` (background polls), the cursor
    /// stays on the same row and the UUID updates to whatever session now occupies
    /// that index. When `false` (user-initiated sorts), the cursor follows the
    /// previously-selected session UUID to its new position.
    pub(crate) fn sort_sessions(&mut self, preserve_visual_position: bool) {
        let sort_order = self.settings.sort_order;
        self.session_order.sort_by(|a, b| {
            let a_sess = self.sessions.get(a);
            let b_sess = self.sessions.get(b);
            let a_pinned = a_sess.and_then(|s| s.session.pinned_at);
            let b_pinned = b_sess.and_then(|s| s.session.pinned_at);

            // Pin position depends on sort order
            match (a_pinned, b_pinned) {
                (Some(a_pin), Some(b_pin)) => {
                    // Among pinned sessions, most recently pinned comes first
                    b_pin.cmp(&a_pin)
                }
                (Some(_), None) => {
                    // Pin position depends on sort order:
                    // Newest-first sorts → pinned at top (Less)
                    // Oldest-first sorts → pinned at bottom (Greater)
                    match sort_order {
                        super::SortOrder::FreshestFirst
                        | super::SortOrder::NewestCreated
                        | super::SortOrder::ByLabel => std::cmp::Ordering::Less,
                        super::SortOrder::StalestFirst | super::SortOrder::OldestCreated => {
                            std::cmp::Ordering::Greater
                        }
                    }
                }
                (None, Some(_)) => match sort_order {
                    super::SortOrder::FreshestFirst
                    | super::SortOrder::NewestCreated
                    | super::SortOrder::ByLabel => std::cmp::Ordering::Greater,
                    super::SortOrder::StalestFirst | super::SortOrder::OldestCreated => {
                        std::cmp::Ordering::Less
                    }
                },
                (None, None) => match sort_order {
                    super::SortOrder::StalestFirst => {
                        let a_time = a_sess.map(|s| s.session.updated_at);
                        let b_time = b_sess.map(|s| s.session.updated_at);
                        a_time.cmp(&b_time)
                    }
                    super::SortOrder::FreshestFirst => {
                        let a_time = a_sess.map(|s| s.session.updated_at);
                        let b_time = b_sess.map(|s| s.session.updated_at);
                        b_time.cmp(&a_time)
                    }
                    super::SortOrder::OldestCreated => {
                        let a_time = a_sess.map(|s| s.session.created_at);
                        let b_time = b_sess.map(|s| s.session.created_at);
                        a_time.cmp(&b_time)
                    }
                    super::SortOrder::NewestCreated => {
                        let a_time = a_sess.map(|s| s.session.created_at);
                        let b_time = b_sess.map(|s| s.session.created_at);
                        b_time.cmp(&a_time)
                    }
                    super::SortOrder::ByLabel => {
                        // Primary: group_id (None sorts last)
                        // Secondary: within same group, sort by updated_at (stalest first)
                        let a_group = a_sess.and_then(|s| s.session.group_id);
                        let b_group = b_sess.and_then(|s| s.session.group_id);
                        match (a_group, b_group) {
                            (Some(ag), Some(bg)) if ag == bg => {
                                // Same group: sort by updated_at ascending (stalest first)
                                let a_time = a_sess.map(|s| s.session.updated_at);
                                let b_time = b_sess.map(|s| s.session.updated_at);
                                a_time.cmp(&b_time)
                            }
                            (Some(_), None) => std::cmp::Ordering::Less, // grouped before ungrouped
                            (None, Some(_)) => std::cmp::Ordering::Greater,
                            (None, None) => {
                                // Both ungrouped: stalest first
                                let a_time = a_sess.map(|s| s.session.updated_at);
                                let b_time = b_sess.map(|s| s.session.updated_at);
                                a_time.cmp(&b_time)
                            }
                            (Some(ag), Some(bg)) => {
                                // Different groups: sort by group UUID for stable ordering
                                ag.cmp(&bg)
                            }
                        }
                    }
                },
            }
        });

        // Children index must follow the new session_order so bucket
        // contents render in the freshly-sorted order. Rebuilt BEFORE
        // recalculate_filtered_order which will consume the index.
        self.rebuild_children_index();
        #[cfg(debug_assertions)]
        self.assert_children_index_invariant();

        self.recalculate_filtered_order();

        self.reconcile_all_session_list_selections(preserve_visual_position);
    }

    fn matches_main_list_scope(&self, id: Uuid, search_active: bool, query_lower: &str) -> bool {
        let Some(state) = self.sessions.get(&id) else {
            return false;
        };
        if !is_visible_session_state(state) {
            return false;
        }
        if self
            .current_project_id
            .is_some_and(|project_id| state.session.project_id != Some(project_id))
        {
            return false;
        }
        if state.session.session_kind == rsi_common::types::SessionKind::TaskRabbit
            || is_jobs_zone_session(&state.session)
        {
            return false;
        }
        !search_active || {
            let (root_query, _) = resolve_session_display(&state.session, &self.sessions);
            root_query.to_lowercase().contains(query_lower)
        }
    }

    fn append_expanded_descendants(
        &self,
        parent_id: Uuid,
        search_active: bool,
        query_lower: &str,
        seen: &mut HashSet<Uuid>,
        visible: &mut Vec<Uuid>,
    ) {
        let parent_is_expanded_container = self.sessions.get(&parent_id).is_some_and(|state| {
            state.list_card_expanded
                && rsi_common::types::is_container_kind(state.session.session_kind)
        });
        if !parent_is_expanded_container {
            return;
        }

        let Some(children) = self.children_by_parent.get(&Some(parent_id)) else {
            return;
        };
        for child_id in children {
            if !seen.insert(*child_id)
                || !self.matches_main_list_scope(*child_id, search_active, query_lower)
            {
                continue;
            }
            visible.push(*child_id);
            self.append_expanded_descendants(*child_id, search_active, query_lower, seen, visible);
        }
    }

    /// Recalculate filtered_session_order based on current_project_id, session kind, and search.
    pub(crate) fn recalculate_filtered_order(&mut self) {
        // Activity is projected from the canonical visible main order. Search,
        // project scope, descent, and sorting can all replace that order
        // without advancing card_generation, so force the next ultra-wide
        // refresh to derive from the newly calculated scope.
        self.session_list_render.activity_generation = u64::MAX;

        let search_active = !self.search_query.is_empty()
            && self.search_target == crate::types::SearchTarget::SessionList;
        let query_lower = self.search_query.to_lowercase();

        // --- Stale ancestor cleanup for the active tab's descent path ---
        // Pop any trailing UUIDs that no longer exist in self.sessions.
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            let before_len = tab.descent_path.len();
            tab.descent_path
                .retain(|uuid| self.sessions.contains_key(uuid));
            if tab.descent_path.len() < before_len {
                tracing::warn!(
                    popped = before_len - tab.descent_path.len(),
                    "descent_path: pruned stale ancestor(s) no longer in session map"
                );
            }
        }

        // Children index may be stale if a push-event handler mutated
        // session_order / parent_id without routing through sort_sessions
        // (e.g. session_deleted). Cheap rebuild guards every downstream
        // lookup in this method and in mini_dag.
        self.rebuild_children_index();
        #[cfg(debug_assertions)]
        self.assert_children_index_invariant();

        // Active tab descent head: None = roots, Some(id) = children of that container.
        let descent_head: Option<Uuid> = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.descent_path.last().copied());

        // Candidates are the direct children of the current descent head.
        // The bucket is already ordered to match `session_order` (rebuilt
        // immediately above), so we drop the per-element parent_id check
        // that the linear scan used to perform.
        let candidate_ids: Vec<Uuid> = self
            .children_by_parent
            .get(&descent_head)
            .cloned()
            .unwrap_or_default();
        let all_main: Vec<Uuid> = candidate_ids
            .into_iter()
            .filter(|id| self.matches_main_list_scope(*id, search_active, &query_lower))
            .collect();

        // The full-list navigator is organized by operational urgency while
        // preserving the selected sort order inside each section. Reorder the
        // actual UUID list here (rather than only repainting it in the
        // renderer) so keyboard navigation, mouse hit-testing, and rows all
        // consume one canonical order. Label mode retains its established
        // grouping; active search renders one flat RESULTS section.
        self.refresh_session_focus_index();
        let all_main = if !search_active && self.settings.sort_order != super::SortOrder::ByLabel {
            let manager_ids = self.manager_roster.member_ids();
            let mut managers = Vec::new();
            let mut needs_you = Vec::new();
            let mut in_flight = Vec::new();
            let mut recent = Vec::new();
            let mut quiet = Vec::new();
            for id in all_main {
                if descent_head.is_none() && manager_ids.contains(&id) {
                    managers.push(id);
                    continue;
                }
                match self
                    .session_list_render
                    .focus_index
                    .get(&id)
                    .map_or(crate::types::SessionFocusGroup::Quiet, |entry| entry.group)
                {
                    crate::types::SessionFocusGroup::NeedsYou => needs_you.push(id),
                    crate::types::SessionFocusGroup::InFlight => in_flight.push(id),
                    crate::types::SessionFocusGroup::Recent => recent.push(id),
                    crate::types::SessionFocusGroup::Quiet => quiet.push(id),
                }
            }
            managers
                .into_iter()
                .chain(needs_you)
                .chain(in_flight)
                .chain(recent)
                .chain(quiet)
                .collect()
        } else {
            all_main
        };

        // Explicitly expanded containers recursively splice their descendants
        // into the canonical list in depth-first preorder. The current descent
        // head is pre-seeded as seen so malformed parent cycles cannot project
        // the active container back into its own child list.
        let mut expanded_main: Vec<Uuid> = Vec::with_capacity(all_main.len());
        let mut seen: HashSet<Uuid> = descent_head.into_iter().collect();
        for id in &all_main {
            if !seen.insert(*id) {
                continue;
            }
            expanded_main.push(*id);
            self.append_expanded_descendants(
                *id,
                search_active,
                &query_lower,
                &mut seen,
                &mut expanded_main,
            );
        }

        // All main sessions stay in the main zone.
        self.filtered_session_order = expanded_main;

        // Build TaskRabbit filtered list

        let taskrabbit_candidates: Vec<Uuid> = self
            .session_order
            .iter()
            .filter(|id| {
                let Some(state) = self.sessions.get(id) else {
                    return false;
                };
                if !is_visible_session_state(state) {
                    return false;
                }

                // Must match project filter
                let project_match = match self.current_project_id {
                    None => true,
                    Some(project_id) => state.session.project_id == Some(project_id),
                };
                // Must be TaskRabbit
                let kind_match = project_match
                    && state.session.session_kind == rsi_common::types::SessionKind::TaskRabbit;
                if !kind_match {
                    return false;
                }

                // Apply search filter
                if !search_active {
                    return true;
                }
                self.sessions.get(id).map_or(false, |s| {
                    let (root_query, _) = resolve_session_display(&s.session, &self.sessions);
                    root_query.to_lowercase().contains(&query_lower)
                })
            })
            .copied()
            .collect();

        // Separate pinned (always shown) from unpinned
        let (pinned, unpinned): (Vec<_>, Vec<_>) =
            taskrabbit_candidates.into_iter().partition(|id| {
                self.sessions
                    .get(id)
                    .map(|s| s.session.pinned_at.is_some())
                    .unwrap_or(false)
            });

        // Always show the 10 most recent unpinned TaskRabbit sessions
        let mut by_recency = unpinned.clone();
        by_recency.sort_by(|a, b| {
            let a_time = self.sessions.get(a).map(|s| s.session.updated_at);
            let b_time = self.sessions.get(b).map(|s| s.session.updated_at);
            b_time.cmp(&a_time)
        });
        let top_10: HashSet<Uuid> = by_recency.into_iter().take(10).collect();
        let visible_unpinned: Vec<Uuid> = unpinned
            .into_iter()
            .filter(|id| top_10.contains(id))
            .collect();

        // Merge pinned + visible unpinned, preserving session_order ordering
        let visible_set: HashSet<Uuid> = pinned
            .iter()
            .chain(visible_unpinned.iter())
            .copied()
            .collect();
        self.filtered_taskrabbit_order = self
            .session_order
            .iter()
            .filter(|id| visible_set.contains(id))
            .copied()
            .collect();

        // Build Jobs filtered list (sessions with a scheduled_job_id)
        let mut jobs_candidates: Vec<Uuid> = self
            .session_order
            .iter()
            .filter(|id| {
                let Some(state) = self.sessions.get(id) else {
                    return false;
                };
                if !is_visible_session_state(state) {
                    return false;
                }

                // Must match project filter
                let project_match = match self.current_project_id {
                    None => true,
                    Some(project_id) => state.session.project_id == Some(project_id),
                };
                if !project_match {
                    return false;
                }

                // Must be a root-level scheduled job launch.
                let is_job = is_jobs_zone_session(&state.session);
                if !is_job {
                    return false;
                }

                // Apply search filter
                if !search_active {
                    return true;
                }
                self.sessions.get(id).map_or(false, |s| {
                    let (root_query, _) = resolve_session_display(&s.session, &self.sessions);
                    root_query.to_lowercase().contains(&query_lower)
                })
            })
            .copied()
            .collect();

        // Sort by most recent updated_at first
        jobs_candidates.sort_by(|a, b| {
            let a_time = self.sessions.get(a).map(|s| s.session.updated_at);
            let b_time = self.sessions.get(b).map(|s| s.session.updated_at);
            b_time.cmp(&a_time)
        });

        self.filtered_jobs_order = jobs_candidates;
    }

    /// Refresh the cached hierarchy rollup when session cards change or when
    /// wall-clock aging can move a session across the Recent/Quiet boundary.
    pub(crate) fn refresh_session_focus_index(&mut self) {
        let now = chrono::Utc::now();
        let time_bucket = now.timestamp().div_euclid(60 * 60);
        if self.session_list_render.focus_generation == self.card_generation
            && self.session_list_render.focus_index.len() == self.sessions.len()
            && self.session_list_render.focus_time_bucket == time_bucket
        {
            return;
        }

        self.session_list_render.focus_index =
            crate::types::compute_session_focus_index(&self.sessions, now);
        self.session_list_render.focus_generation = self.card_generation;
        self.session_list_render.focus_time_bucket = time_bucket;
    }

    /// Invalidate TUI-local session-browser projections after an in-place push
    /// mutation that does not advance `card_generation`.
    pub(crate) fn invalidate_session_browser_cache(&mut self) {
        self.session_list_render.focus_generation = u64::MAX;
        self.session_list_render.activity_generation = u64::MAX;
    }

    /// Rebuild the canonical operational order after a push mutation changes
    /// focus-group membership, preserving each list pane's selected UUID.
    pub(crate) fn refresh_session_browser_order_after_focus_change(&mut self) {
        self.invalidate_session_browser_cache();
        self.recalculate_filtered_order();
        self.reconcile_all_session_list_selections(false);
    }

    /// Lazily refresh the bounded ultra-wide activity projection.
    ///
    /// Selection is intentionally absent from the cache key; the renderer
    /// excludes the selected row while taking its small display slices.
    pub(crate) fn refresh_session_activity(&mut self) {
        self.refresh_session_focus_index();
        let time_bucket = chrono::Utc::now().timestamp().div_euclid(60);
        if self.session_list_render.activity_generation == self.card_generation
            && self.session_list_render.activity_time_bucket == time_bucket
            && self.session_list_render.activity.is_some()
        {
            return;
        }

        self.session_list_render.activity = Some(crate::types::compute_session_activity(
            &self.sessions,
            &self.filtered_session_order,
            &self.session_list_render.focus_index,
        ));
        self.session_list_render.activity_generation = self.card_generation;
        self.session_list_render.activity_time_bucket = time_bucket;
    }

    pub(crate) fn reconcile_all_session_list_selections(&mut self, preserve_visual_position: bool) {
        let filtered_session_order = self.filtered_session_order.clone();
        let filtered_taskrabbit_order = self.filtered_taskrabbit_order.clone();
        let filtered_archived_order = self.filtered_archived_order.clone();
        let filtered_jobs_order = self.filtered_jobs_order.clone();

        for tab in &mut self.tabs {
            // Reconcile the shadow/fallback copy
            if let Pane::SessionList {
                selected_index,
                selected_session,
                active_zone,
                taskrabbit_selected_index,
                archive_selected_index,
                jobs_selected_index,
                ..
            } = &mut tab.session_list_state
            {
                Self::reconcile_single_pane_selection(
                    &filtered_session_order,
                    &filtered_taskrabbit_order,
                    &filtered_archived_order,
                    &filtered_jobs_order,
                    active_zone,
                    selected_index,
                    selected_session,
                    taskrabbit_selected_index,
                    archive_selected_index,
                    jobs_selected_index,
                    preserve_visual_position,
                );
            }

            // Reconcile all live panes in active layout
            let ids: Vec<PaneId> = tab.layout.leaf_ids();
            for leaf_id in ids {
                if let Some(Pane::SessionList {
                    selected_index,
                    selected_session,
                    active_zone,
                    taskrabbit_selected_index,
                    archive_selected_index,
                    jobs_selected_index,
                    ..
                }) = tab.layout.find_pane_mut(leaf_id)
                {
                    Self::reconcile_single_pane_selection(
                        &filtered_session_order,
                        &filtered_taskrabbit_order,
                        &filtered_archived_order,
                        &filtered_jobs_order,
                        active_zone,
                        selected_index,
                        selected_session,
                        taskrabbit_selected_index,
                        archive_selected_index,
                        jobs_selected_index,
                        preserve_visual_position,
                    );
                }
            }
        }
    }

    fn reconcile_single_pane_selection(
        filtered_session_order: &[Uuid],
        filtered_taskrabbit_order: &[Uuid],
        filtered_archived_order: &[Uuid],
        filtered_jobs_order: &[Uuid],
        active_zone: &crate::types::SessionListZone,
        selected_index: &mut usize,
        selected_session: &mut Option<Uuid>,
        taskrabbit_selected_index: &mut usize,
        archive_selected_index: &mut usize,
        jobs_selected_index: &mut usize,
        preserve_visual_position: bool,
    ) {
        let main_len = filtered_session_order.len();
        let tr_len = filtered_taskrabbit_order.len();
        let arc_len = filtered_archived_order.len();
        let jobs_len = filtered_jobs_order.len();

        // 1. Clamp indices
        if main_len == 0 {
            *selected_index = 0;
        } else if *selected_index >= main_len {
            *selected_index = main_len - 1;
        }

        if tr_len == 0 {
            *taskrabbit_selected_index = 0;
        } else if *taskrabbit_selected_index >= tr_len {
            *taskrabbit_selected_index = tr_len - 1;
        }

        if arc_len == 0 {
            *archive_selected_index = 0;
        } else if *archive_selected_index >= arc_len {
            *archive_selected_index = arc_len - 1;
        }

        if jobs_len == 0 {
            *jobs_selected_index = 0;
        } else if *jobs_selected_index >= jobs_len {
            *jobs_selected_index = jobs_len - 1;
        }

        // 2. Resolve active list and active index ref
        let list_to_check = match active_zone {
            crate::types::SessionListZone::Main => filtered_session_order,
            crate::types::SessionListZone::TaskRabbit => filtered_taskrabbit_order,
            crate::types::SessionListZone::Archive => filtered_archived_order,
            crate::types::SessionListZone::Jobs => filtered_jobs_order,
        };

        let active_index_ref = match active_zone {
            crate::types::SessionListZone::Main => selected_index,
            crate::types::SessionListZone::TaskRabbit => taskrabbit_selected_index,
            crate::types::SessionListZone::Archive => archive_selected_index,
            crate::types::SessionListZone::Jobs => jobs_selected_index,
        };

        // 3. Update selected_session
        if preserve_visual_position {
            *selected_session = list_to_check.get(*active_index_ref).copied();
        } else if let Some(prev_uuid) = *selected_session {
            if let Some(new_pos) = list_to_check.iter().position(|&uuid| uuid == prev_uuid) {
                *active_index_ref = new_pos;
            } else {
                *selected_session = list_to_check.get(*active_index_ref).copied();
            }
        } else {
            *selected_session = list_to_check.get(*active_index_ref).copied();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DaemonClient;
    use crate::types::{Pane, PaneId, SplitNode, Tab};
    use rsi_common::types::{
        ContextUsageConfidence, Session, SessionKind, SessionProvider, SessionStatus,
    };
    use std::path::PathBuf;

    fn make_session(id: Uuid, parent_id: Option<Uuid>, kind: SessionKind) -> Session {
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
            context_usage_confidence: ContextUsageConfidence::default(),
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
            parent_id,
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

    fn make_test_app() -> App {
        let client = DaemonClient::new(PathBuf::from("/tmp/test.sock"));
        let mut app = App::new(client);
        let initial_pane_id = PaneId(0);
        app.current_project_id = None;
        app.tabs = vec![Tab {
            name: "[1]".to_string(),
            session_list_state: Tab::default_session_list_state(),
            layout: SplitNode::Leaf {
                pane: Pane::SessionList {
                    selected_index: 0,
                    selected_session: None,
                    scroll_offset: 0,
                    active_zone: Default::default(),
                    taskrabbit_selected_index: 0,
                    archive_selected_index: 0,
                    jobs_selected_index: 0,
                },
                id: initial_pane_id,
            },
            focused_pane: initial_pane_id,
            project_id: None,
            detail_split_offset: 0,
            layout_x_offset_adj: 0,
            session_list_width_pct: 0,
            descent_path: Vec::new(),
            bottom_focus_target: crate::types::BottomZone::Off,
            mini_dag_focus: None,
        }];
        app.active_tab = 0;
        app.next_pane_id = 1;
        app
    }

    #[test]
    fn session_seeding_honours_default_show_system_events() {
        for enabled in [false, true] {
            let mut app = make_test_app();
            app.settings.default_show_system_events = enabled;
            let id = Uuid::new_v4();
            app.upsert_visible_session(make_session(id, None, SessionKind::Standard));
            assert_eq!(app.sessions[&id].show_system_events, enabled);
        }
    }

    #[test]
    fn session_seeding_honours_default_show_thinking_events() {
        for enabled in [false, true] {
            let mut app = make_test_app();
            app.settings.default_show_thinking_events = enabled;
            let id = Uuid::new_v4();
            app.upsert_visible_session(make_session(id, None, SessionKind::Standard));
            assert_eq!(app.sessions[&id].show_thinking_events, enabled);
        }
    }

    #[test]
    fn poll_refresh_consumes_resolved_context_budget_verbatim() {
        use rsi_common::provider_capabilities::{
            CapabilityConfidence, CapabilityEvidence, CapabilitySource, ContextCapacity,
            ResolvedContextBudget,
        };

        let mut app = make_test_app();
        let session_id = Uuid::new_v4();
        let mut session = make_session(session_id, None, SessionKind::Standard);
        session.resolved_context_budget = Some(ResolvedContextBudget {
            active_tokens: 258_400,
            capacity: ContextCapacity {
                provider_default_tokens: Some(272_000),
                provider_max_tokens: Some(872_000),
                effective_percent: Some(95),
                ..ContextCapacity::default()
            },
            evidence: CapabilityEvidence {
                source: CapabilitySource::ProviderCatalog,
                source_version: Some("codex-cli 0.155.1".to_string()),
                source_digest: None,
                observed_at: None,
                confidence: CapabilityConfidence::Verified,
            },
        });
        assert!(app.update_sessions(vec![session.clone()]));

        let runtime_budget = ResolvedContextBudget {
            active_tokens: 300_123,
            capacity: ContextCapacity {
                runtime_effective_tokens: Some(300_123),
                ..ContextCapacity::default()
            },
            evidence: CapabilityEvidence {
                source: CapabilitySource::RuntimeTelemetry,
                source_version: None,
                source_digest: None,
                observed_at: None,
                confidence: CapabilityConfidence::Authoritative,
            },
        };
        session.resolved_context_budget = Some(runtime_budget.clone());

        assert!(
            app.update_sessions(vec![session]),
            "budget-only polling refresh must request a redraw"
        );
        assert_eq!(
            app.sessions[&session_id].session.resolved_context_budget,
            Some(runtime_budget)
        );
    }

    /// Root view shows only sessions without a parent.
    #[test]
    fn descent_filter_root_view() {
        let mut app = make_test_app();
        let root_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();

        app.update_sessions(vec![
            make_session(root_id, None, SessionKind::Group),
            make_session(child_id, Some(root_id), SessionKind::Standard),
        ]);
        app.recalculate_filtered_order();

        // With empty descent_path, only root-level sessions (parent_id == None) should appear
        assert!(
            app.filtered_session_order.contains(&root_id),
            "root session should be visible"
        );
        assert!(
            !app.filtered_session_order.contains(&child_id),
            "child session should NOT be visible at root"
        );
    }

    /// Descended view shows children of the container we're in.
    #[test]
    fn descent_filter_descended_view() {
        let mut app = make_test_app();
        let group_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let other_root_id = Uuid::new_v4();

        app.update_sessions(vec![
            make_session(group_id, None, SessionKind::Group),
            make_session(child_id, Some(group_id), SessionKind::Standard),
            make_session(other_root_id, None, SessionKind::Standard),
        ]);

        // Descend into the group
        app.tabs[0].descent_path = vec![group_id];
        app.recalculate_filtered_order();

        assert!(
            app.filtered_session_order.contains(&child_id),
            "child should be visible when descended into parent"
        );
        assert!(
            !app.filtered_session_order.contains(&group_id),
            "group itself should NOT appear as its own child"
        );
        assert!(
            !app.filtered_session_order.contains(&other_root_id),
            "sibling root should NOT appear when descended"
        );
    }

    #[test]
    fn explicit_nested_accordions_project_group_epic_and_sessions() {
        let mut app = make_test_app();
        let group_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let first_session_id = Uuid::new_v4();
        let second_session_id = Uuid::new_v4();

        app.update_sessions(vec![
            make_session(group_id, None, SessionKind::Group),
            make_session(epic_id, Some(group_id), SessionKind::Epic),
            make_session(first_session_id, Some(epic_id), SessionKind::Story),
            make_session(second_session_id, Some(epic_id), SessionKind::Task),
        ]);

        assert_eq!(app.filtered_session_order, vec![group_id]);

        app.sessions.get_mut(&group_id).unwrap().list_card_expanded = true;
        app.recalculate_filtered_order();
        assert_eq!(app.filtered_session_order, vec![group_id, epic_id]);

        app.sessions.get_mut(&epic_id).unwrap().list_card_expanded = true;
        app.recalculate_filtered_order();
        assert_eq!(&app.filtered_session_order[..2], &[group_id, epic_id]);
        assert_eq!(app.filtered_session_order.len(), 4);
        assert!(app.filtered_session_order[2..].contains(&first_session_id));
        assert!(app.filtered_session_order[2..].contains(&second_session_id));

        app.sessions.get_mut(&group_id).unwrap().list_card_expanded = false;
        app.recalculate_filtered_order();
        assert_eq!(app.filtered_session_order, vec![group_id]);
    }

    #[test]
    fn recursive_accordion_projection_stops_at_malformed_cycle() {
        let mut app = make_test_app();
        let group_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let mut group = make_session(group_id, Some(epic_id), SessionKind::Group);
        let mut epic = make_session(epic_id, Some(group_id), SessionKind::Epic);
        group.title = Some("cycle group".to_string());
        epic.title = Some("cycle epic".to_string());

        app.update_sessions(vec![group, epic]);
        app.sessions.get_mut(&group_id).unwrap().list_card_expanded = true;
        app.sessions.get_mut(&epic_id).unwrap().list_card_expanded = true;
        app.tabs[0].descent_path = vec![group_id];
        app.recalculate_filtered_order();

        assert_eq!(app.filtered_session_order, vec![epic_id]);
    }

    #[test]
    fn removed_manager_snapshot_requests_roster_refresh() {
        let mut app = make_test_app();
        let manager_id = Uuid::new_v4();
        app.update_sessions(vec![make_session(manager_id, None, SessionKind::Standard)]);
        app.manager_roster.by_project.insert(
            Uuid::new_v4(),
            super::super::manager_roster::ManagerRosterEntry {
                session_id: manager_id,
                tier: super::super::manager_roster::ManagerTier::Project,
            },
        );
        app.update_sessions(Vec::new());
        assert!(app.manager_roster.take_refresh());
    }

    #[test]
    fn manager_band_navigation_and_refresh_preserve_selected_uuid() {
        let mut app = make_test_app();
        let manager_id = Uuid::new_v4();
        let worker_id = Uuid::new_v4();
        let mut worker = make_session(worker_id, None, SessionKind::Standard);
        worker.status = SessionStatus::WaitingApproval;
        app.update_sessions(vec![
            worker,
            make_session(manager_id, None, SessionKind::Standard),
        ]);
        let project_id = Uuid::new_v4();
        app.manager_roster.by_project.insert(
            project_id,
            super::super::manager_roster::ManagerRosterEntry {
                session_id: manager_id,
                tier: super::super::manager_roster::ManagerTier::Project,
            },
        );
        app.sort_sessions(false);
        assert_eq!(app.filtered_session_order, vec![manager_id, worker_id]);
        app.reset_selection_to_first();
        assert_eq!(app.selected_session_id(), Some(manager_id));
        app.nav_down();
        assert_eq!(app.selected_session_id(), Some(worker_id));
        app.nav_up();
        assert_eq!(app.selected_session_id(), Some(manager_id));

        let mut successor = make_session(Uuid::new_v4(), None, SessionKind::Standard);
        successor.continued_from = Some(manager_id);
        app.upsert_session(successor);
        assert!(app.manager_roster.take_refresh());
        assert!(!app.manager_roster.take_refresh());

        app.manager_roster.by_project.clear();
        app.sort_sessions(false);
        assert_eq!(app.selected_session_id(), Some(manager_id));
        assert_eq!(app.filtered_session_order[0], worker_id);
    }

    #[test]
    fn main_order_prioritizes_operational_focus_groups() {
        let mut app = make_test_app();
        app.settings.sort_order = crate::app::SortOrder::FreshestFirst;
        let waiting_id = Uuid::new_v4();
        let running_id = Uuid::new_v4();
        let recent_id = Uuid::new_v4();
        let quiet_id = Uuid::new_v4();

        let mut waiting = make_session(waiting_id, None, SessionKind::Standard);
        waiting.status = SessionStatus::WaitingApproval;
        let running = make_session(running_id, None, SessionKind::Standard);
        let mut recent = make_session(recent_id, None, SessionKind::Standard);
        recent.status = SessionStatus::Completed;
        recent.updated_at = chrono::Utc::now() - chrono::Duration::days(2);
        let mut quiet = make_session(quiet_id, None, SessionKind::Standard);
        quiet.status = SessionStatus::Completed;
        quiet.updated_at = chrono::Utc::now() - chrono::Duration::days(12);

        app.update_sessions(vec![quiet, recent, running, waiting]);
        app.recalculate_filtered_order();

        assert_eq!(
            app.filtered_session_order,
            vec![waiting_id, running_id, recent_id, quiet_id],
            "focus sections must lead with operator attention, then live work, recency, and quiet history"
        );
    }

    /// A scheduled wake/resume target can carry a historical scheduled_job_id,
    /// but it still belongs in its parent hierarchy.
    #[test]
    fn descent_filter_parented_scheduled_wake_target_stays_in_hierarchy() {
        let mut app = make_test_app();
        let epic_id = Uuid::new_v4();
        let master_id = Uuid::new_v4();
        let job_id = Uuid::new_v4();
        let mut master = make_session(master_id, Some(epic_id), SessionKind::Story);
        master.status = SessionStatus::Completed;
        master.scheduled_job_id = Some(job_id);

        app.update_sessions(vec![make_session(epic_id, None, SessionKind::Epic), master]);

        app.tabs[0].descent_path = vec![epic_id];
        app.recalculate_filtered_order();

        assert!(
            app.filtered_session_order.contains(&master_id),
            "parented scheduled wake target should remain visible in the Epic"
        );
        assert!(
            !app.filtered_jobs_order.contains(&master_id),
            "parented scheduled wake target should not be reclassified into Jobs"
        );
    }

    /// Fresh scheduled launches are root-level job sessions and stay out of the
    /// normal main hierarchy.
    #[test]
    fn root_scheduled_launch_stays_in_jobs_zone() {
        let mut app = make_test_app();
        let job_session_id = Uuid::new_v4();
        let mut job_session = make_session(job_session_id, None, SessionKind::Standard);
        job_session.scheduled_job_id = Some(Uuid::new_v4());

        app.update_sessions(vec![job_session]);
        app.recalculate_filtered_order();

        assert!(
            !app.filtered_session_order.contains(&job_session_id),
            "root scheduled launch should not appear in the main hierarchy"
        );
        assert!(
            app.filtered_jobs_order.contains(&job_session_id),
            "root scheduled launch should remain visible in Jobs"
        );
    }

    /// Stale ancestors are pruned.
    #[test]
    fn descent_filter_stale_ancestor_pop() {
        let mut app = make_test_app();
        let stale_id = Uuid::new_v4(); // not in sessions map
        let valid_id = Uuid::new_v4();

        app.update_sessions(vec![make_session(valid_id, None, SessionKind::Standard)]);

        // Descent path contains a stale UUID
        app.tabs[0].descent_path = vec![stale_id];
        app.recalculate_filtered_order();

        // Stale ancestor should have been pruned
        assert!(
            app.tabs[0].descent_path.is_empty(),
            "stale ancestor should be pruned from descent_path"
        );
        // With empty path, should show roots
        assert!(app.filtered_session_order.contains(&valid_id));
    }

    // --- Phase 1: children_by_parent index invariants ---

    /// Root bucket (None key) contains exactly the parentless sessions in
    /// `session_order` order. Sum-of-buckets == `session_order.len()`.
    #[test]
    fn children_index_root_bucket_matches_session_order() {
        let mut app = make_test_app();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        app.update_sessions(vec![
            make_session(a, None, SessionKind::Standard),
            make_session(b, None, SessionKind::Standard),
            make_session(c, None, SessionKind::Standard),
        ]);

        let bucket = app.children_by_parent.get(&None).unwrap();
        // Three root sessions -> three entries.
        assert_eq!(bucket.len(), 3);
        // Bucket ordering matches session_order (post-sort).
        let bucket_set: HashSet<Uuid> = bucket.iter().copied().collect();
        let order_set: HashSet<Uuid> = app.session_order.iter().copied().collect();
        assert_eq!(bucket_set, order_set);
        // Invariant holds.
        app.assert_children_index_invariant();
    }

    /// Container bucket lists direct children in `session_order` order;
    /// children are NOT in the root bucket.
    #[test]
    fn children_index_container_bucket_groups_children() {
        let mut app = make_test_app();
        let group_id = Uuid::new_v4();
        let child_a = Uuid::new_v4();
        let child_b = Uuid::new_v4();
        app.update_sessions(vec![
            make_session(group_id, None, SessionKind::Group),
            make_session(child_a, Some(group_id), SessionKind::Standard),
            make_session(child_b, Some(group_id), SessionKind::Standard),
        ]);

        let container_bucket = app.children_by_parent.get(&Some(group_id)).unwrap();
        assert_eq!(container_bucket.len(), 2);
        assert!(container_bucket.contains(&child_a));
        assert!(container_bucket.contains(&child_b));

        let root_bucket = app.children_by_parent.get(&None).unwrap();
        assert!(root_bucket.contains(&group_id));
        assert!(!root_bucket.contains(&child_a));
        assert!(!root_bucket.contains(&child_b));

        app.assert_children_index_invariant();
    }

    /// Re-parenting a session via metadata change moves it between buckets
    /// and preserves the sum-of-buckets invariant.
    #[test]
    fn children_index_survives_reparent() {
        let mut app = make_test_app();
        let g1 = Uuid::new_v4();
        let g2 = Uuid::new_v4();
        let kid = Uuid::new_v4();
        app.update_sessions(vec![
            make_session(g1, None, SessionKind::Group),
            make_session(g2, None, SessionKind::Group),
            make_session(kid, Some(g1), SessionKind::Standard),
        ]);

        // Pre-reparent: child is under g1, not g2.
        assert!(app.children_by_parent[&Some(g1)].contains(&kid));
        assert!(
            !app.children_by_parent
                .get(&Some(g2))
                .is_some_and(|v| v.contains(&kid))
        );

        // Re-parent: emulate the session_metadata_changed push handler --
        // mutate parent_id, then sort_sessions(true) (which rebuilds index).
        app.sessions.get_mut(&kid).unwrap().session.parent_id = Some(g2);
        app.sort_sessions(true);

        assert!(
            !app.children_by_parent
                .get(&Some(g1))
                .is_some_and(|v| v.contains(&kid))
        );
        assert!(app.children_by_parent[&Some(g2)].contains(&kid));
        app.assert_children_index_invariant();
    }

    /// Deleting a session (the session_deleted push handler path) removes
    /// it from its bucket and preserves the sum invariant.
    #[test]
    fn children_index_survives_delete() {
        let mut app = make_test_app();
        let group_id = Uuid::new_v4();
        let kid = Uuid::new_v4();
        app.update_sessions(vec![
            make_session(group_id, None, SessionKind::Group),
            make_session(kid, Some(group_id), SessionKind::Standard),
        ]);

        assert!(app.children_by_parent[&Some(group_id)].contains(&kid));

        // Emulate `apply_push_event("session_deleted")` post-handler state:
        // remove from sessions + session_order, then recalculate_filtered_order
        // (which rebuilds the index at the top of its body).
        app.sessions.remove(&kid);
        app.session_order.retain(|id| *id != kid);
        app.recalculate_filtered_order();

        let group_bucket = app
            .children_by_parent
            .get(&Some(group_id))
            .cloned()
            .unwrap_or_default();
        assert!(!group_bucket.contains(&kid));
        app.assert_children_index_invariant();
    }

    /// Sort-order changes re-order bucket contents to match the new
    /// `session_order`.
    #[test]
    fn children_index_survives_sort_change() {
        let mut app = make_test_app();
        // Pin the initial sort order so the test is independent of whatever
        // PersistedState default the test app picked up.
        app.settings.sort_order = crate::app::SortOrder::StalestFirst;

        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        // Stagger updated_at so sort order is observable.
        let mut sa = make_session(a, None, SessionKind::Standard);
        let mut sb = make_session(b, None, SessionKind::Standard);
        sa.updated_at = chrono::Utc::now() - chrono::Duration::seconds(60);
        sb.updated_at = chrono::Utc::now();
        app.update_sessions(vec![sa, sb]);

        // StalestFirst: older `a` should precede fresher `b`.
        let initial = app.children_by_parent[&None].clone();
        assert_eq!(initial.len(), 2);
        assert_eq!(initial[0], a, "stalest (a) should come first");
        assert_eq!(initial[1], b);

        // Flip to freshest-first; sort_sessions(false) re-sorts and rebuilds.
        app.settings.sort_order = crate::app::SortOrder::FreshestFirst;
        app.sort_sessions(false);

        let after = app.children_by_parent[&None].clone();
        assert_eq!(after.len(), 2);
        // The order should be the reverse of the initial order.
        assert_eq!(after[0], b, "freshest (b) should now come first");
        assert_eq!(after[1], a);
        app.assert_children_index_invariant();
    }

    #[test]
    fn activity_cache_is_stable_across_selection_only_changes() {
        let mut app = make_test_app();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        app.update_sessions(vec![
            make_session(first, None, SessionKind::Standard),
            make_session(second, None, SessionKind::Standard),
        ]);
        app.refresh_session_activity();
        let before = app
            .session_list_render
            .activity
            .as_ref()
            .unwrap()
            .recent_changes
            .as_ptr();

        if let crate::types::Pane::SessionList { selected_index, .. } =
            &mut app.tabs[0].session_list_state
        {
            *selected_index = 1;
        }
        app.refresh_session_activity();
        let after = app
            .session_list_render
            .activity
            .as_ref()
            .unwrap()
            .recent_changes
            .as_ptr();

        assert_eq!(before, after, "selection must not rebuild activity data");
    }

    #[test]
    fn activity_cache_rebuilds_after_visible_main_order_recalculation() {
        let mut app = make_test_app();
        let visible = Uuid::new_v4();
        let hidden = Uuid::new_v4();
        let mut visible_session = make_session(visible, None, SessionKind::Standard);
        visible_session.title = Some("Keep visible".to_string());
        visible_session.status = SessionStatus::WaitingApproval;
        let mut hidden_session = make_session(hidden, None, SessionKind::Standard);
        hidden_session.title = Some("Drop hidden".to_string());

        app.update_sessions(vec![visible_session, hidden_session]);
        app.refresh_session_activity();
        let card_generation = app.card_generation;
        let primed = app.session_list_render.activity.as_ref().unwrap();
        assert_eq!(primed.flow.needs_you, 1);
        assert_eq!(primed.flow.in_flight, 1);
        assert_eq!(primed.recent_changes.len(), 2);

        app.search_target = crate::types::SearchTarget::SessionList;
        app.search_query = "keep visible".to_string();
        app.recalculate_filtered_order();
        assert_eq!(
            app.card_generation, card_generation,
            "filter-only recalculation must not masquerade as a card mutation"
        );
        assert_eq!(app.filtered_session_order, vec![visible]);

        app.refresh_session_activity();
        let refreshed = app.session_list_render.activity.as_ref().unwrap();
        assert_eq!(
            refreshed
                .operator_queue
                .iter()
                .map(|item| item.session_id)
                .collect::<Vec<_>>(),
            vec![visible]
        );
        assert_eq!(
            refreshed
                .recent_changes
                .iter()
                .map(|item| item.session_id)
                .collect::<Vec<_>>(),
            vec![visible]
        );
        assert_eq!(refreshed.flow.needs_you, 1);
        assert_eq!(refreshed.flow.in_flight, 0);
        assert_eq!(refreshed.flow.recent, 0);
        assert_eq!(refreshed.flow.quiet, 0);
        assert_eq!(refreshed.flow.failed, 0);
    }
}
