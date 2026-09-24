//! Navigation data cache for eliminating 3-second RPC timeouts during navigation transitions.
//!
//! This module implements a React useEffect-like dependency tracking system for navigation
//! data, providing immediate cache rendering with background updates and push notification-driven
//! cache invalidation.

use rsi_common::types::{ConversationEvent, Session};
use std::collections::{HashMap, HashSet};
use std::time::Instant;
use uuid::Uuid;

/// Core navigation data cache supporting immediate rendering with background updates
#[derive(Debug, Default)]
pub struct NavigationDataCache {
    /// Hierarchy cache: parent_id -> cached children data
    pub hierarchy: HierarchyCache,
    /// Conversation cache: session_id -> cached events
    pub conversations: ConversationCache,
    /// Dependency tracker for React useEffect-like behavior
    pub dependencies: DependencyTracker,
    /// Push notification invalidation tracker
    pub invalidations: InvalidationTracker,
}

/// Cached hierarchy data indexed by parent_id
#[derive(Debug, Default)]
pub struct HierarchyCache {
    /// Cached children data: parent_id -> (sessions, timestamp, freshness)
    /// Key `None` = root sessions, Key `Some(uuid)` = children of that container
    pub entries: HashMap<Option<Uuid>, HierarchyEntry>,
    /// Background fetch tracking
    pub inflight: HashSet<Option<Uuid>>,
}

/// Single hierarchy cache entry
#[derive(Debug, Clone)]
pub struct HierarchyEntry {
    /// Cached session list for this parent
    pub sessions: Vec<Session>,
    /// When this data was last updated
    pub timestamp: Instant,
    /// Data freshness level
    pub freshness: CacheFreshness,
    /// Last known sequence number for incremental updates
    pub last_sequence: Option<i32>,
}

/// Cached conversation data indexed by session_id
#[derive(Debug, Default)]
pub struct ConversationCache {
    /// Cached conversation data: session_id -> (events, timestamp, freshness)
    pub entries: HashMap<Uuid, ConversationEntry>,
    /// Background fetch tracking
    pub inflight: HashSet<Uuid>,
}

/// Single conversation cache entry
#[derive(Debug, Clone)]
pub struct ConversationEntry {
    /// Cached events for this session
    pub events: Vec<ConversationEvent>,
    /// When this data was last updated
    pub timestamp: Instant,
    /// Data freshness level
    pub freshness: CacheFreshness,
    /// Last sequence number for incremental updates
    pub last_sequence: Option<i32>,
}

/// Cache freshness levels for rendering prioritization
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CacheFreshness {
    /// Stale data, showing placeholder/loading state
    Stale = 0,
    /// Cached data from previous session, may be outdated
    Cached = 1,
    /// Fresh data from recent RPC, fully up-to-date
    Fresh = 2,
}

impl HierarchyEntry {
    /// Dynamically evaluate freshness based on elapsed time since timestamp
    pub fn freshness(&self) -> CacheFreshness {
        if self.freshness == CacheFreshness::Stale {
            return CacheFreshness::Stale;
        }
        let elapsed = self.timestamp.elapsed();
        if elapsed > std::time::Duration::from_secs(30) {
            CacheFreshness::Stale
        } else if elapsed > std::time::Duration::from_secs(5) {
            CacheFreshness::Cached
        } else {
            self.freshness
        }
    }
}

impl ConversationEntry {
    /// Dynamically evaluate freshness based on elapsed time since timestamp
    pub fn freshness(&self) -> CacheFreshness {
        if self.freshness == CacheFreshness::Stale {
            return CacheFreshness::Stale;
        }
        let elapsed = self.timestamp.elapsed();
        if elapsed > std::time::Duration::from_secs(15) {
            CacheFreshness::Stale
        } else if elapsed > std::time::Duration::from_secs(2) {
            CacheFreshness::Cached
        } else {
            self.freshness
        }
    }
}

/// Dependency tracking for React useEffect-like behavior
#[derive(Debug, Default)]
pub struct DependencyTracker {
    /// Last navigation node that triggered effects
    pub last_navigation_node: Option<NavigationNode>,
    /// Current dependency array for comparison
    pub current_deps: NavigationDependencies,
}

/// Navigation node types for dependency tracking
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NavigationNode {
    /// Root session list
    Root,
    /// Container (Group/Epic) with ID
    Container(Uuid),
    /// Leaf session with ID
    Leaf(Uuid),
}

/// Dependency array for useEffect-like comparison
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NavigationDependencies {
    /// Current focused pane type and ID
    pub focused_pane: Option<(String, Option<Uuid>)>, // ("SessionList" | "SessionDetail", session_id)
    /// Current selected session in lists
    pub selected_session: Option<Uuid>,
    /// Current descent path (for hierarchy navigation)
    pub descent_path: Vec<Uuid>,
    /// Active tab index
    pub active_tab: usize,
}

/// Push notification invalidation tracking
#[derive(Debug, Default)]
pub struct InvalidationTracker {
    /// Sessions needing hierarchy refresh (parent_id invalidated)
    pub stale_hierarchy_nodes: HashSet<Option<Uuid>>,
    /// Sessions needing conversation refresh
    pub stale_conversations: HashSet<Uuid>,
    /// Batch invalidation requests from push events
    pub pending_invalidations: Vec<InvalidationRequest>,
}

/// Single invalidation request from push notifications
#[derive(Debug, Clone)]
pub struct InvalidationRequest {
    /// Type of invalidation
    pub kind: InvalidationKind,
    /// When the invalidation was received
    pub timestamp: Instant,
}

/// Types of cache invalidations
#[derive(Debug, Clone)]
pub enum InvalidationKind {
    /// Conversation events changed for session
    ConversationChanged(Uuid),
    /// Session metadata changed (affects hierarchy)
    SessionMetadataChanged(Uuid),
    /// Session was deleted/archived
    SessionDeleted(Uuid),
    /// Child session spawned under parent
    ChildSpawned { parent_id: Uuid, child_id: Uuid },
    /// Full hierarchy refresh needed
    HierarchyRefresh,
}

impl NavigationDataCache {
    /// Create a new navigation data cache
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if navigation dependencies changed (useEffect-like)
    pub fn dependencies_changed(&mut self, new_deps: NavigationDependencies) -> bool {
        let changed = self.dependencies.current_deps != new_deps;
        if changed {
            self.dependencies.current_deps = new_deps;
        }
        changed
    }

    /// Get cached hierarchy data or None if not available/stale
    pub fn get_hierarchy(&self, parent_id: Option<Uuid>) -> Option<&HierarchyEntry> {
        self.hierarchy
            .entries
            .get(&parent_id)
            .filter(|entry| entry.freshness() >= CacheFreshness::Cached)
    }

    /// Get cached conversation data or None if not available/stale
    pub fn get_conversation(&self, session_id: Uuid) -> Option<&ConversationEntry> {
        self.conversations
            .entries
            .get(&session_id)
            .filter(|entry| entry.freshness() >= CacheFreshness::Cached)
    }

    /// Cache hierarchy data
    pub fn cache_hierarchy(&mut self, parent_id: Option<Uuid>, sessions: Vec<Session>) {
        let entry = HierarchyEntry {
            sessions,
            timestamp: Instant::now(),
            freshness: CacheFreshness::Fresh,
            last_sequence: None, // Hierarchy doesn't use sequence numbers
        };
        self.hierarchy.entries.insert(parent_id, entry);

        // Clear inflight tracking
        self.hierarchy.inflight.remove(&parent_id);
    }

    /// Cache conversation data
    pub fn cache_conversation(
        &mut self,
        session_id: Uuid,
        events: Vec<ConversationEvent>,
        last_sequence: Option<i32>,
    ) {
        let entry = ConversationEntry {
            events,
            timestamp: Instant::now(),
            freshness: CacheFreshness::Fresh,
            last_sequence,
        };
        self.conversations.entries.insert(session_id, entry);

        // Clear inflight tracking
        self.conversations.inflight.remove(&session_id);
    }

    /// Mark hierarchy data as stale for invalidation
    pub fn invalidate_hierarchy(&mut self, parent_id: Option<Uuid>) {
        if let Some(entry) = self.hierarchy.entries.get_mut(&parent_id) {
            entry.freshness = CacheFreshness::Stale;
        }
        self.invalidations.stale_hierarchy_nodes.insert(parent_id);
    }

    /// Mark conversation data as stale for invalidation
    pub fn invalidate_conversation(&mut self, session_id: Uuid) {
        if let Some(entry) = self.conversations.entries.get_mut(&session_id) {
            entry.freshness = CacheFreshness::Stale;
        }
        self.invalidations.stale_conversations.insert(session_id);
    }

    /// Invalidate all hierarchy cache entries for manual refresh
    pub fn invalidate_all_hierarchy(&mut self) {
        for entry in self.hierarchy.entries.values_mut() {
            entry.freshness = CacheFreshness::Stale;
        }
        // Mark all parent IDs as stale
        let parent_ids: Vec<Option<Uuid>> = self.hierarchy.entries.keys().cloned().collect();
        for parent_id in parent_ids {
            self.invalidations.stale_hierarchy_nodes.insert(parent_id);
        }
    }

    /// Invalidate all conversation cache entries for manual refresh
    pub fn invalidate_all_conversations(&mut self) {
        for entry in self.conversations.entries.values_mut() {
            entry.freshness = CacheFreshness::Stale;
        }
        // Mark all session IDs as stale
        let session_ids: Vec<Uuid> = self.conversations.entries.keys().cloned().collect();
        for session_id in session_ids {
            self.invalidations.stale_conversations.insert(session_id);
        }
    }

    /// Process push notification for cache invalidation
    pub fn process_push_invalidation(&mut self, event_type: &str, session_id: Option<Uuid>) {
        let request = match event_type {
            "conversation_event" => {
                if let Some(id) = session_id {
                    InvalidationRequest {
                        kind: InvalidationKind::ConversationChanged(id),
                        timestamp: Instant::now(),
                    }
                } else {
                    return;
                }
            }
            "session_status_changed" | "session_metadata_changed" => {
                if let Some(id) = session_id {
                    InvalidationRequest {
                        kind: InvalidationKind::SessionMetadataChanged(id),
                        timestamp: Instant::now(),
                    }
                } else {
                    return;
                }
            }
            "session_deleted" | "session_archived" => {
                if let Some(id) = session_id {
                    InvalidationRequest {
                        kind: InvalidationKind::SessionDeleted(id),
                        timestamp: Instant::now(),
                    }
                } else {
                    return;
                }
            }
            "child_spawned" => {
                // This would need additional data from the push event
                InvalidationRequest {
                    kind: InvalidationKind::HierarchyRefresh,
                    timestamp: Instant::now(),
                }
            }
            "subscription_reset" => InvalidationRequest {
                kind: InvalidationKind::HierarchyRefresh,
                timestamp: Instant::now(),
            },
            _ => return, // Unknown event type
        };

        self.invalidations.pending_invalidations.push(request);
    }

    /// Apply pending invalidations from push notifications
    pub fn apply_pending_invalidations(&mut self) {
        let requests = self
            .invalidations
            .pending_invalidations
            .drain(..)
            .collect::<Vec<_>>();
        for request in requests {
            match request.kind {
                InvalidationKind::ConversationChanged(session_id) => {
                    self.invalidate_conversation(session_id);
                }
                InvalidationKind::SessionMetadataChanged(session_id) => {
                    // Need to invalidate both conversation and any hierarchy containing this session
                    self.invalidate_conversation(session_id);
                    // TODO: Find parent and invalidate hierarchy
                }
                InvalidationKind::SessionDeleted(session_id) => {
                    // Remove from both caches
                    self.conversations.entries.remove(&session_id);
                    // TODO: Remove from hierarchy cache and rebuild children index
                }
                InvalidationKind::ChildSpawned {
                    parent_id,
                    child_id: _,
                } => {
                    self.invalidate_hierarchy(Some(parent_id));
                }
                InvalidationKind::HierarchyRefresh => {
                    // Invalidate all hierarchy data
                    for entry in self.hierarchy.entries.values_mut() {
                        entry.freshness = CacheFreshness::Stale;
                    }
                    self.invalidations.stale_hierarchy_nodes.clear();
                    self.invalidations.stale_hierarchy_nodes.insert(None); // Mark root as stale
                }
            }
        }
    }

    /// Check if a hierarchy fetch is already in flight
    pub fn is_hierarchy_fetch_inflight(&self, parent_id: Option<Uuid>) -> bool {
        self.hierarchy.inflight.contains(&parent_id)
    }

    /// Check if a conversation fetch is already in flight
    pub fn is_conversation_fetch_inflight(&self, session_id: Uuid) -> bool {
        self.conversations.inflight.contains(&session_id)
    }

    /// Mark hierarchy fetch as in flight
    pub fn mark_hierarchy_fetch_inflight(&mut self, parent_id: Option<Uuid>) {
        self.hierarchy.inflight.insert(parent_id);
    }

    /// Mark conversation fetch as in flight
    pub fn mark_conversation_fetch_inflight(&mut self, session_id: Uuid) {
        self.conversations.inflight.insert(session_id);
    }

    /// Get navigation node from current app state
    pub fn current_navigation_node(
        focused_pane: Option<&crate::types::Pane>,
        descent_path: &[Uuid],
    ) -> NavigationNode {
        // If focused on a session detail, it's a leaf node
        if let Some(crate::types::Pane::SessionDetail { session_id }) = focused_pane {
            return NavigationNode::Leaf(*session_id);
        }

        // Otherwise, determine based on descent path
        descent_path
            .last()
            .map(|&id| NavigationNode::Container(id))
            .unwrap_or(NavigationNode::Root)
    }

    /// Get cache statistics for debugging and monitoring
    pub fn get_cache_stats(&self) -> CacheStats {
        CacheStats {
            hierarchy_entries: self.hierarchy.entries.len(),
            hierarchy_inflight: self.hierarchy.inflight.len(),
            conversation_entries: self.conversations.entries.len(),
            conversation_inflight: self.conversations.inflight.len(),
            pending_invalidations: self.invalidations.pending_invalidations.len(),
            stale_hierarchy_nodes: self.invalidations.stale_hierarchy_nodes.len(),
            stale_conversations: self.invalidations.stale_conversations.len(),
        }
    }
}

impl NavigationDependencies {
    /// Create dependencies from current app state
    pub fn from_app_state(
        focused_pane: Option<&crate::types::Pane>,
        selected_session: Option<Uuid>,
        descent_path: &[Uuid],
        active_tab: usize,
    ) -> Self {
        let focused_pane = focused_pane.map(|pane| match pane {
            crate::types::Pane::SessionList { .. } => ("SessionList".to_string(), selected_session),
            crate::types::Pane::SessionDetail { session_id } => {
                ("SessionDetail".to_string(), Some(*session_id))
            }
            crate::types::Pane::Settings => ("Settings".to_string(), None),
            crate::types::Pane::PromptCreator => ("PromptCreator".to_string(), None),
            crate::types::Pane::Issues(_) => ("Issues".to_string(), None),
        });

        Self {
            focused_pane,
            selected_session,
            descent_path: descent_path.to_vec(),
            active_tab,
        }
    }
}

/// Cache statistics for debugging and monitoring
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub hierarchy_entries: usize,
    pub hierarchy_inflight: usize,
    pub conversation_entries: usize,
    pub conversation_inflight: usize,
    pub pending_invalidations: usize,
    pub stale_hierarchy_nodes: usize,
    pub stale_conversations: usize,
}

impl CacheFreshness {
    /// Check if this freshness level allows rendering (not stale)
    pub fn allows_rendering(self) -> bool {
        self >= CacheFreshness::Cached
    }

    /// Check if this freshness level is considered fresh
    pub fn is_fresh(self) -> bool {
        self == CacheFreshness::Fresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_freshness_ordering() {
        assert!(CacheFreshness::Fresh > CacheFreshness::Cached);
        assert!(CacheFreshness::Cached > CacheFreshness::Stale);
        assert!(CacheFreshness::Fresh.allows_rendering());
        assert!(CacheFreshness::Cached.allows_rendering());
        assert!(!CacheFreshness::Stale.allows_rendering());
    }

    #[test]
    fn dependencies_change_detection() {
        let mut cache = NavigationDataCache::new();

        let deps1 = NavigationDependencies {
            focused_pane: Some(("SessionList".to_string(), None)),
            selected_session: None,
            descent_path: vec![],
            active_tab: 0,
        };

        let deps2 = NavigationDependencies {
            focused_pane: Some(("SessionDetail".to_string(), Some(Uuid::new_v4()))),
            selected_session: Some(Uuid::new_v4()),
            descent_path: vec![],
            active_tab: 0,
        };

        assert!(cache.dependencies_changed(deps1.clone()));
        assert!(!cache.dependencies_changed(deps1.clone())); // Same deps, no change
        assert!(cache.dependencies_changed(deps2)); // Different deps, change detected
    }

    #[test]
    fn cache_invalidation() {
        let mut cache = NavigationDataCache::new();
        let session_id = Uuid::new_v4();

        // Cache some conversation data
        cache.cache_conversation(session_id, vec![], Some(5));
        let entry = cache.get_conversation(session_id).unwrap();
        assert_eq!(entry.freshness, CacheFreshness::Fresh);

        // Invalidate it
        cache.invalidate_conversation(session_id);
        assert!(cache.get_conversation(session_id).is_none()); // Should be filtered out as stale
    }

    #[test]
    fn push_invalidation_processing() {
        let mut cache = NavigationDataCache::new();
        let session_id = Uuid::new_v4();

        cache.process_push_invalidation("conversation_event", Some(session_id));
        assert_eq!(cache.invalidations.pending_invalidations.len(), 1);

        match &cache.invalidations.pending_invalidations[0].kind {
            InvalidationKind::ConversationChanged(id) => assert_eq!(*id, session_id),
            _ => panic!("Wrong invalidation kind"),
        }
    }

    #[test]
    fn cache_hierarchy_data() {
        use rsi_common::types::{SessionKind, SessionProvider, SessionStatus};

        fn mk_test_session(id: Uuid, parent_id: Option<Uuid>) -> Session {
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
                working_dir: std::path::PathBuf::from("/tmp"),
                git_branch: None,
                status: SessionStatus::Running,
                project_id: None,
                session_kind: SessionKind::Standard,
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

        let mut cache = NavigationDataCache::new();
        let parent_id = Some(Uuid::new_v4());
        let child1_id = Uuid::new_v4();
        let child2_id = Uuid::new_v4();

        let sessions = vec![
            mk_test_session(child1_id, parent_id),
            mk_test_session(child2_id, parent_id),
        ];

        cache.cache_hierarchy(parent_id, sessions.clone());
        let cached_entry = cache.get_hierarchy(parent_id).unwrap();
        assert_eq!(cached_entry.sessions.len(), 2);
        assert_eq!(cached_entry.freshness, CacheFreshness::Fresh);
    }

    #[test]
    fn test_dynamic_freshness_degradation() {
        let mut cache = NavigationDataCache::new();
        let session_id = Uuid::new_v4();

        // 1. Cache conversation
        cache.cache_conversation(session_id, vec![], Some(0));

        {
            let entry = cache.get_conversation(session_id).unwrap();
            assert_eq!(entry.freshness(), CacheFreshness::Fresh);
        }

        // Simulate elapsed time by backdating the timestamp in the entry
        if let Some(entry) = cache.conversations.entries.get_mut(&session_id) {
            entry.timestamp = Instant::now() - std::time::Duration::from_secs(3);
        }

        {
            let entry = cache.get_conversation(session_id).unwrap();
            assert_eq!(entry.freshness(), CacheFreshness::Cached);
        }

        if let Some(entry) = cache.conversations.entries.get_mut(&session_id) {
            entry.timestamp = Instant::now() - std::time::Duration::from_secs(16);
        }

        assert!(cache.get_conversation(session_id).is_none()); // Becomes Stale, returns None
    }

    #[test]
    fn test_hierarchy_dynamic_freshness_degradation() {
        let mut cache = NavigationDataCache::new();
        let parent_id = Some(Uuid::new_v4());

        cache.cache_hierarchy(parent_id, vec![]);

        {
            let entry = cache.get_hierarchy(parent_id).unwrap();
            assert_eq!(entry.freshness(), CacheFreshness::Fresh);
        }

        if let Some(entry) = cache.hierarchy.entries.get_mut(&parent_id) {
            entry.timestamp = Instant::now() - std::time::Duration::from_secs(6);
        }

        {
            let entry = cache.get_hierarchy(parent_id).unwrap();
            assert_eq!(entry.freshness(), CacheFreshness::Cached);
        }

        if let Some(entry) = cache.hierarchy.entries.get_mut(&parent_id) {
            entry.timestamp = Instant::now() - std::time::Duration::from_secs(31);
        }

        assert!(cache.get_hierarchy(parent_id).is_none()); // Becomes Stale
    }
}
