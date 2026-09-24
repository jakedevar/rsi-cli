//! Cache key helpers and artifact metadata for generation artifacts.
//!
//! Pure logic layer -- no persistence. The daemon handles SQLite storage
//! separately in `rsid::store::graph_cache`.

use serde::{Deserialize, Serialize};

use crate::format::WorkflowDefinition;

/// Cache key for a generation artifact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct CacheKey {
    /// Hash of the intent text.
    pub intent_hash: String,
    /// Generation parameters that affect output.
    pub params_hash: String,
    /// Version of topology library (for invalidation).
    pub topology_version: String,
    /// Hash of the RESOLVED prompt/intent CONTENT (D6 content-staleness).
    ///
    /// Distinct from `intent_hash`: this hashes the fully-resolved input
    /// content that actually feeds generation, so an edited input invalidates
    /// the cache (a distinct `to_key_string`) even when the raw intent text and
    /// topology_version are unchanged.
    pub content_hash: String,
}

impl CacheKey {
    /// Create a cache key from intent, resolved content, and params.
    ///
    /// `content` is the RESOLVED prompt/intent content (not merely the raw
    /// intent text); its hash drives content-staleness invalidation.
    pub fn new(intent: &str, content: &str, model: Option<&str>, topology_version: &str) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        intent.hash(&mut hasher);
        let intent_hash = format!("{:x}", hasher.finish());

        let mut hasher = DefaultHasher::new();
        model.hash(&mut hasher);
        let params_hash = format!("{:x}", hasher.finish());

        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        let content_hash = format!("{:x}", hasher.finish());

        Self {
            intent_hash,
            params_hash,
            topology_version: topology_version.to_string(),
            content_hash,
        }
    }

    /// Create a composite key string for storage.
    pub fn to_key_string(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.intent_hash, self.params_hash, self.topology_version, self.content_hash
        )
    }
}

/// A cached generation artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    pub key: CacheKey,
    pub workflow: WorkflowDefinition,
    pub reasoning: String,
    pub created_at: String,
    pub hit_count: u64,
}

/// In-memory cache for generation artifacts.
/// Daemon persistence is handled separately in flywheeld.
#[derive(Debug, Default)]
pub struct GenerationCache {
    entries: std::collections::HashMap<String, CacheEntry>,
}

impl GenerationCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&mut self, key: &CacheKey) -> Option<&CacheEntry> {
        let key_str = key.to_key_string();
        if let Some(entry) = self.entries.get_mut(&key_str) {
            entry.hit_count += 1;
            Some(entry)
        } else {
            None
        }
    }

    pub fn insert(&mut self, entry: CacheEntry) {
        self.entries.insert(entry.key.to_key_string(), entry);
    }

    pub fn invalidate(&mut self, key: &CacheKey) -> bool {
        self.entries.remove(&key.to_key_string()).is_some()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Invalidate all entries with a different topology version.
    pub fn invalidate_stale(&mut self, current_version: &str) {
        self.entries
            .retain(|_, entry| entry.key.topology_version == current_version);
    }
}

/// Current topology library version for cache invalidation.
pub const TOPOLOGY_VERSION: &str = "1.0.0";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::WorkflowDefinition;

    fn make_entry(intent: &str, model: Option<&str>) -> CacheEntry {
        let key = CacheKey::new(intent, intent, model, TOPOLOGY_VERSION);
        CacheEntry {
            key,
            workflow: WorkflowDefinition::new("test-workflow"),
            reasoning: "test reasoning".to_string(),
            created_at: "2026-03-21T00:00:00Z".to_string(),
            hit_count: 0,
        }
    }

    #[test]
    fn cache_key_creation_and_to_key_string() {
        let key = CacheKey::new(
            "build a pipeline",
            "build a pipeline",
            Some("opus-4"),
            TOPOLOGY_VERSION,
        );
        assert!(!key.intent_hash.is_empty());
        assert!(!key.params_hash.is_empty());
        assert!(!key.content_hash.is_empty());
        assert_eq!(key.topology_version, TOPOLOGY_VERSION);

        // key_string is now a 4-tuple: intent:params:topology:content.
        let key_str = key.to_key_string();
        assert!(key_str.contains(':'));
        assert_eq!(key_str.matches(':').count(), 3);
        // content_hash round-trips through the key string (trailing segment).
        assert!(key_str.ends_with(&key.content_hash));
    }

    #[test]
    fn keys_differing_only_in_content_hash_are_distinct() {
        // Same intent, params, and topology_version, but resolved CONTENT differs
        // => distinct cache keys (content-staleness cache miss / invalidation).
        let a = CacheKey::new(
            "intent",
            "resolved content A",
            Some("opus-4"),
            TOPOLOGY_VERSION,
        );
        let b = CacheKey::new(
            "intent",
            "resolved content B",
            Some("opus-4"),
            TOPOLOGY_VERSION,
        );

        assert_eq!(a.intent_hash, b.intent_hash);
        assert_eq!(a.params_hash, b.params_hash);
        assert_eq!(a.topology_version, b.topology_version);
        assert_ne!(a.content_hash, b.content_hash);
        assert_ne!(a.to_key_string(), b.to_key_string());
        assert_ne!(a, b);

        // A GenerationCache keyed on the changed-content key is a miss.
        let mut cache = GenerationCache::new();
        cache.insert(CacheEntry {
            key: a.clone(),
            workflow: WorkflowDefinition::new("test-workflow"),
            reasoning: "r".to_string(),
            created_at: "2026-03-21T00:00:00Z".to_string(),
            hit_count: 0,
        });
        assert!(cache.get(&a).is_some());
        assert!(cache.get(&b).is_none());
    }

    #[test]
    fn cache_insert_and_get_increments_hit_count() {
        let mut cache = GenerationCache::new();
        let entry = make_entry("test intent", None);
        let key = entry.key.clone();

        cache.insert(entry);
        assert_eq!(cache.len(), 1);
        assert!(!cache.is_empty());

        let hit = cache.get(&key).unwrap();
        assert_eq!(hit.hit_count, 1);

        let hit2 = cache.get(&key).unwrap();
        assert_eq!(hit2.hit_count, 2);
    }

    #[test]
    fn cache_invalidation_by_key() {
        let mut cache = GenerationCache::new();
        let entry = make_entry("test intent", None);
        let key = entry.key.clone();

        cache.insert(entry);
        assert!(cache.invalidate(&key));
        assert!(cache.is_empty());
        assert!(!cache.invalidate(&key));
    }

    #[test]
    fn cache_clear() {
        let mut cache = GenerationCache::new();
        cache.insert(make_entry("a", None));
        cache.insert(make_entry("b", None));
        assert_eq!(cache.len(), 2);

        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn invalidate_stale_entries() {
        let mut cache = GenerationCache::new();

        // Insert entry with current version
        cache.insert(make_entry("current", None));

        // Insert entry with old version
        let mut old_entry = make_entry("old", None);
        old_entry.key.topology_version = "0.9.0".to_string();
        let old_key_str = old_entry.key.to_key_string();
        cache.entries.insert(old_key_str, old_entry);

        assert_eq!(cache.len(), 2);

        cache.invalidate_stale(TOPOLOGY_VERSION);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cache_miss_returns_none() {
        let mut cache = GenerationCache::new();
        let key = CacheKey::new("nonexistent", "nonexistent", None, TOPOLOGY_VERSION);
        assert!(cache.get(&key).is_none());
    }
}
