//! Navigation-scoped hierarchy data effects.
//!
//! This is intentionally shaped like a small `useEffect` dependency array:
//! derive the active navigation node, compare it to the previous key, and
//! dispatch exactly the fetch owned by that key.

use super::{App, HierarchyFetchResult, NavigationEffectNode};
use crate::types::Pane;
use rsi_common::types::Session;
use std::collections::HashSet;
use uuid::Uuid;

impl App {
    /// Run the fetch effect for the current active navigation node if and
    /// only if the dependency key changed.
    pub(crate) fn run_navigation_effect_if_changed(&mut self) {
        // Check if navigation dependencies changed (useEffect-like)
        let focused_pane = self.focused_pane();
        let selected_session = self.selected_session_id();
        let descent_path = &self.tabs[self.active_tab].descent_path;
        let active_tab = self.active_tab;

        let new_deps = crate::app::cache::NavigationDependencies::from_app_state(
            focused_pane,
            selected_session,
            descent_path,
            active_tab,
        );

        if !self.navigation_cache.dependencies_changed(new_deps) {
            return; // Dependencies unchanged, no effect needed
        }

        let node = self.active_navigation_effect_node();
        self.navigation_effect_node = Some(node.clone());

        match node {
            NavigationEffectNode::Root => {
                // Check cache first for immediate rendering
                match self.navigation_cache.get_hierarchy(None) {
                    None => {
                        self.trigger_hierarchy_fetch_if_needed(None, true);
                    }
                    Some(entry) => {
                        if entry.freshness() == crate::app::cache::CacheFreshness::Cached {
                            self.trigger_hierarchy_fetch_if_needed(None, true);
                        }
                    }
                }
            }
            NavigationEffectNode::Container(id) => {
                // Check cache first for immediate rendering
                match self.navigation_cache.get_hierarchy(Some(id)) {
                    None => {
                        self.trigger_hierarchy_fetch_if_needed(Some(id), true);
                    }
                    Some(entry) => {
                        if entry.freshness() == crate::app::cache::CacheFreshness::Cached {
                            self.trigger_hierarchy_fetch_if_needed(Some(id), true);
                        }
                    }
                }
            }
            NavigationEffectNode::Leaf(id) => {
                // Check cache first for immediate rendering
                match self.navigation_cache.get_conversation(id) {
                    None => {
                        self.trigger_focus_fetch_if_needed(id);
                    }
                    Some(entry) => {
                        if entry.freshness() == crate::app::cache::CacheFreshness::Cached {
                            self.trigger_focus_fetch_if_needed(id);
                        }
                    }
                }
                // Trigger model segments fetch for SessionDetail panes
                self.trigger_model_segments_fetch_if_needed(id);
            }
        }
    }

    /// Mark a parent node stale. If it is currently active, schedule the
    /// targeted child refresh immediately; otherwise the next navigation into
    /// that node will fetch it.
    pub(crate) fn invalidate_hierarchy_node(&mut self, parent_id: Option<Uuid>) {
        self.hierarchy_stale_nodes.insert(parent_id);

        // Invalidate cache entry
        self.navigation_cache.invalidate_hierarchy(parent_id);

        match self.active_navigation_effect_node() {
            NavigationEffectNode::Root if parent_id.is_none() => {
                self.trigger_hierarchy_fetch_if_needed(parent_id, false);
            }
            NavigationEffectNode::Container(id) if parent_id == Some(id) => {
                self.trigger_hierarchy_fetch_if_needed(parent_id, false);
            }
            _ => {}
        }
    }

    /// A full `ListSessions` response is an authoritative hierarchy snapshot.
    pub(crate) fn mark_hierarchy_snapshot_loaded(&mut self) {
        self.hierarchy_loaded_nodes.clear();
        self.hierarchy_loaded_nodes.insert(None);

        for (id, state) in &self.sessions {
            self.hierarchy_loaded_nodes.insert(state.session.parent_id);
            if rsi_common::is_container_kind(state.session.session_kind) {
                self.hierarchy_loaded_nodes.insert(Some(*id));
            }
        }
        self.hierarchy_stale_nodes.clear();
    }

    /// Consume a targeted child-list fetch result.
    pub(crate) fn apply_hierarchy_fetch_result(&mut self, result: HierarchyFetchResult) -> bool {
        self.hierarchy_fetch_inflight.remove(&result.parent_id);
        let sessions = match result.sessions {
            Ok(sessions) => sessions,
            Err(err) => {
                tracing::debug!(
                    parent_id = ?result.parent_id,
                    error = %err,
                    "hierarchy_fetch error"
                );
                return false;
            }
        };

        // Cache the fetched hierarchy data
        self.navigation_cache
            .cache_hierarchy(result.parent_id, sessions.clone());

        let dirty = self.upsert_hierarchy_sessions(sessions);
        self.hierarchy_loaded_nodes.insert(result.parent_id);
        self.hierarchy_stale_nodes.remove(&result.parent_id);
        dirty
    }

    pub(crate) fn active_navigation_effect_node(&self) -> NavigationEffectNode {
        if let Some(Pane::SessionDetail { session_id }) = self.focused_pane() {
            return NavigationEffectNode::Leaf(*session_id);
        }

        self.tabs
            .get(self.active_tab)
            .and_then(|tab| tab.descent_path.last().copied())
            .map(NavigationEffectNode::Container)
            .unwrap_or(NavigationEffectNode::Root)
    }

    /// Immediately trigger a hierarchy fetch for immediate navigation response.
    /// Always dispatches regardless of loaded/stale state to eliminate lag on nav changes.
    ///
    /// This is the immediate-response counterpart to the periodic effect system.
    /// Navigation actions call this to fetch hierarchy data instantly instead of
    /// waiting for the next poll cycle, eliminating the "epic appears but not selectable"
    /// delay and the main session list delay on back-out navigation.
    pub(crate) fn trigger_hierarchy_fetch_immediate(&mut self, parent_id: Option<Uuid>) {
        if !self.poll.connected {
            return;
        }
        if self.hierarchy_fetch_inflight.contains(&parent_id) {
            return;
        }
        if self.navigation_cache.is_hierarchy_fetch_inflight(parent_id) {
            return;
        }

        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };

        self.hierarchy_fetch_inflight.insert(parent_id);
        self.navigation_cache
            .mark_hierarchy_fetch_inflight(parent_id);
        let socket_path = self.client.socket_path().to_path_buf();
        let tx = self.hierarchy_fetch_tx.clone();
        tracing::trace!(
            target = "rsi::profile",
            parent_id = ?parent_id,
            "hierarchy_fetch_immediate"
        );
        runtime.spawn(async move {
            let sessions = match tokio::time::timeout(std::time::Duration::from_secs(3), async {
                let mut client = crate::client::DaemonClient::new(socket_path);
                match client.connect().await {
                    Ok(()) => client
                        .list_session_children(parent_id)
                        .await
                        .map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                }
            })
            .await
            {
                Ok(result) => result,
                Err(_) => Err("hierarchy fetch timed out".to_string()),
            };
            let _ = tx.send(HierarchyFetchResult {
                parent_id,
                sessions,
            });
        });
    }

    fn trigger_hierarchy_fetch_if_needed(&mut self, parent_id: Option<Uuid>, force: bool) {
        if !self.poll.connected {
            return;
        }
        if self.hierarchy_fetch_inflight.contains(&parent_id) {
            return;
        }
        if self.navigation_cache.is_hierarchy_fetch_inflight(parent_id) {
            return;
        }
        let loaded = self.hierarchy_loaded_nodes.contains(&parent_id);
        let stale = self.hierarchy_stale_nodes.contains(&parent_id);
        if !force && loaded && !stale {
            return;
        }

        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };

        self.hierarchy_fetch_inflight.insert(parent_id);
        self.navigation_cache
            .mark_hierarchy_fetch_inflight(parent_id);
        let socket_path = self.client.socket_path().to_path_buf();
        let tx = self.hierarchy_fetch_tx.clone();
        tracing::trace!(
            target = "rsi::profile",
            parent_id = ?parent_id,
            "hierarchy_fetch_dispatch"
        );
        runtime.spawn(async move {
            let sessions = match tokio::time::timeout(std::time::Duration::from_secs(3), async {
                let mut client = crate::client::DaemonClient::new(socket_path);
                match client.connect().await {
                    Ok(()) => client
                        .list_session_children(parent_id)
                        .await
                        .map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                }
            })
            .await
            {
                Ok(result) => result,
                Err(_) => Err("hierarchy fetch timed out".to_string()),
            };
            let _ = tx.send(HierarchyFetchResult {
                parent_id,
                sessions,
            });
        });
    }

    fn upsert_hierarchy_sessions(&mut self, sessions: Vec<Session>) -> bool {
        let mut changed = false;

        for session in sessions {
            let id = session.id;
            if let Some(state) = self.sessions.get_mut(&id) {
                let old_parent_id = state.session.parent_id;
                let old_updated_at = state.session.updated_at;
                let old_status = state.session.status;
                let old_lead = state.session.lead_session_id;
                let old_project = state.session.project_id;
                let prev_total_input = state.session.total_input_tokens;

                changed |= old_parent_id != session.parent_id
                    || old_updated_at != session.updated_at
                    || old_status != session.status
                    || old_lead != session.lead_session_id
                    || old_project != session.project_id;

                state.session = session;
                if let Some(prev) = prev_total_input {
                    let new_val = state.session.total_input_tokens.unwrap_or(0);
                    if prev > new_val {
                        state.session.total_input_tokens = Some(prev);
                    }
                }
            } else {
                self.session_order.push(id);
                let mut new_state = crate::types::SessionState::new(session);
                new_state.show_system_events = self.settings.default_show_system_events;
                new_state.show_thinking_events = self.settings.default_show_thinking_events;
                new_state.show_tool_results = !self.settings.default_hide_tool_results;
                self.sessions.insert(id, new_state);
                changed = true;
            }
        }

        if changed {
            self.sort_sessions(true);
            self.invalidate_card_cache();
        }
        changed
    }

    /// Invalidate all cached navigation data and mark for refresh.
    /// Used by manual refresh actions to force immediate updates.
    pub(crate) fn invalidate_all_cache(&mut self) {
        // Clear navigation cache
        self.navigation_cache.invalidate_all_hierarchy();
        self.navigation_cache.invalidate_all_conversations();

        // Clear loaded state to force refresh
        self.hierarchy_loaded_nodes.clear();
        self.hierarchy_stale_nodes.insert(None);

        // Mark all nodes as stale for immediate refresh
        for (_, state) in &self.sessions {
            self.hierarchy_stale_nodes.insert(state.session.parent_id);
        }
    }

    /// Trigger comprehensive navigation refresh including sessions, projects, and labels.
    /// This replaces the periodic poll cycle for user-controlled updates.
    pub(crate) async fn trigger_full_navigation_refresh(&mut self) {
        if !self.poll.connected {
            return;
        }

        // Trigger immediate fetches for all navigation data
        self.trigger_hierarchy_fetch_immediate(None);
        self.trigger_projects_fetch_immediate().await;
        self.trigger_labels_fetch_immediate().await;

        // Run navigation effect to update current view
        self.run_navigation_effect_if_changed();
    }

    /// Trigger immediate projects fetch for manual refresh.
    pub(crate) async fn trigger_projects_fetch_immediate(&mut self) -> bool {
        if !self.poll.connected {
            return false;
        }

        match self.client.list_projects().await {
            Ok(projects) => {
                let changed = self.update_projects(projects);
                if changed {
                    self.mark_dirty();
                }
                changed
            }
            Err(err) => {
                self.push_notification(
                    crate::types::NotificationKind::OperationFailed,
                    crate::types::NotificationPriority::High,
                    format!("Failed to refresh projects: {}", err),
                    None,
                );
                false
            }
        }
    }

    /// Trigger immediate labels fetch for manual refresh.
    pub(crate) async fn trigger_labels_fetch_immediate(&mut self) -> bool {
        if !self.poll.connected {
            return false;
        }

        match self.client.list_labels().await {
            Ok(labels) => {
                self.update_labels(labels);
                self.mark_dirty();
                true
            }
            Err(err) => {
                self.push_notification(
                    crate::types::NotificationKind::OperationFailed,
                    crate::types::NotificationPriority::High,
                    format!("Failed to refresh labels: {}", err),
                    None,
                );
                false
            }
        }
    }

    /// Conversation polling should follow actual detail visibility, not the
    /// selected row in a session list. This prevents list navigation from
    /// becoming a periodic event-fetch trigger.
    pub(crate) fn visible_detail_session_ids(&self) -> HashSet<Uuid> {
        let mut ids = HashSet::new();
        for tab in &self.tabs {
            for pane_id in tab.layout.leaf_ids() {
                if let Some(Pane::SessionDetail { session_id }) = tab.layout.find_pane(pane_id) {
                    ids.insert(*session_id);
                }
            }
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DaemonClient;
    use std::path::PathBuf;

    fn test_app() -> App {
        crate::state::DevState::clear();
        crate::state::PersistedState::default().save();
        App::new(DaemonClient::new(PathBuf::from(
            "/tmp/test-hierarchy-effect.sock",
        )))
    }

    #[test]
    fn hierarchy_seeding_honours_default_show_system_events() {
        for enabled in [false, true] {
            let mut app = test_app();
            app.settings.default_show_system_events = enabled;
            let id = Uuid::new_v4();
            let session = crate::app::app_test_helpers::baseline_session(
                id,
                rsi_common::types::SessionKind::Standard,
            );
            app.upsert_hierarchy_sessions(vec![session]);
            assert_eq!(app.sessions[&id].show_system_events, enabled);
        }
    }

    #[test]
    fn hierarchy_seeding_honours_default_show_thinking_events() {
        for enabled in [false, true] {
            let mut app = test_app();
            app.settings.default_show_thinking_events = enabled;
            let id = Uuid::new_v4();
            let session = crate::app::app_test_helpers::baseline_session(
                id,
                rsi_common::types::SessionKind::Standard,
            );
            app.upsert_hierarchy_sessions(vec![session]);
            assert_eq!(app.sessions[&id].show_thinking_events, enabled);
        }
    }

    #[tokio::test]
    async fn navigation_switch_fetches_container_even_when_snapshot_loaded() {
        let mut app = test_app();
        let container_id = Uuid::new_v4();
        app.poll.connected = true;
        app.hierarchy_loaded_nodes.insert(Some(container_id));
        app.tabs[app.active_tab].descent_path = vec![container_id];

        app.run_navigation_effect_if_changed();

        assert_eq!(
            app.navigation_effect_node,
            Some(NavigationEffectNode::Container(container_id))
        );
        assert!(
            app.hierarchy_fetch_inflight.contains(&Some(container_id)),
            "view-switch fetch must not be suppressed by loaded snapshot state"
        );
    }

    #[tokio::test]
    async fn unchanged_navigation_node_does_not_refetch_container() {
        let mut app = test_app();
        let container_id = Uuid::new_v4();
        app.poll.connected = true;
        app.tabs[app.active_tab].descent_path = vec![container_id];

        app.run_navigation_effect_if_changed();
        app.hierarchy_fetch_inflight.clear();
        app.run_navigation_effect_if_changed();

        assert!(
            app.hierarchy_fetch_inflight.is_empty(),
            "dependency key equality should suppress duplicate fetches"
        );
    }

    #[tokio::test]
    async fn immediate_hierarchy_fetch_triggers_without_dependency_checks() {
        let mut app = test_app();
        let container_id = Uuid::new_v4();
        app.poll.connected = true;
        // Pre-mark the container as loaded to test that immediate fetch bypasses this check
        app.hierarchy_loaded_nodes.insert(Some(container_id));

        app.trigger_hierarchy_fetch_immediate(Some(container_id));

        assert!(
            app.hierarchy_fetch_inflight.contains(&Some(container_id)),
            "immediate fetch must trigger regardless of loaded state"
        );
    }

    #[tokio::test]
    async fn immediate_hierarchy_fetch_respects_inflight_gate() {
        let mut app = test_app();
        let container_id = Uuid::new_v4();
        app.poll.connected = true;
        app.hierarchy_fetch_inflight.insert(Some(container_id));

        app.trigger_hierarchy_fetch_immediate(Some(container_id));

        assert_eq!(
            app.hierarchy_fetch_inflight.len(),
            1,
            "immediate fetch must not start duplicate requests"
        );
    }
}
