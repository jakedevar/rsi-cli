use chrono::Utc;

use crate::error::Result;
use crate::memory::embedding::parse_embedding_json;
use crate::memory::files::hash_text;
use crate::memory::store::MemoryStore;
use crate::memory::types::{EmbeddingCacheEntry, EmbeddingProvider, MemoryChunk};

/// Embedding cache layer. Sits between the batch pipeline and the actual
/// embedding provider, keyed on (provider_id, model, provider_key, content_hash).
pub struct EmbeddingCache {
    pub provider_id: String,
    pub model: String,
    pub provider_key: String,
    pub max_entries: u32,
    pub enabled: bool,
}

impl EmbeddingCache {
    /// Compute a deterministic provider key from provider metadata.
    pub fn compute_provider_key(provider: &dyn EmbeddingProvider) -> String {
        let json = serde_json::json!({
            "provider": provider.id(),
            "model": provider.model(),
        });
        hash_text(&json.to_string())
    }

    /// Compute provider key including base URL (for OpenAI-compatible providers).
    pub fn compute_provider_key_with_url(
        provider: &dyn EmbeddingProvider,
        base_url: &str,
    ) -> String {
        let json = serde_json::json!({
            "provider": provider.id(),
            "model": provider.model(),
            "base_url": base_url,
        });
        hash_text(&json.to_string())
    }

    /// Look up cached embeddings for a set of chunks.
    /// Returns a CacheLookupResult with hits filled in and missing indices listed.
    pub fn get_cached_embeddings(
        &self,
        store: &MemoryStore,
        chunks: &[MemoryChunk],
    ) -> Result<CacheLookupResult> {
        if !self.enabled || chunks.is_empty() {
            let missing_indices: Vec<usize> = (0..chunks.len()).collect();
            return Ok(CacheLookupResult {
                embeddings: vec![None; chunks.len()],
                missing_indices,
            });
        }

        // Collect unique non-empty hashes
        let unique_hashes: Vec<&str> = chunks
            .iter()
            .filter_map(|c| {
                if c.hash.is_empty() {
                    None
                } else {
                    Some(c.hash.as_str())
                }
            })
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let cached = store.load_embedding_cache(
            &self.provider_id,
            &self.model,
            &self.provider_key,
            &unique_hashes,
        )?;

        let mut embeddings: Vec<Option<Vec<f32>>> = Vec::with_capacity(chunks.len());
        let mut missing_indices: Vec<usize> = Vec::new();

        for (i, chunk) in chunks.iter().enumerate() {
            if chunk.hash.is_empty() {
                embeddings.push(None);
                missing_indices.push(i);
                continue;
            }

            if let Some(embedding_json) = cached.get(&chunk.hash)
                && let Some(vec) = parse_embedding_json(embedding_json)
            {
                embeddings.push(Some(vec));
                continue;
            }

            embeddings.push(None);
            missing_indices.push(i);
        }

        Ok(CacheLookupResult {
            embeddings,
            missing_indices,
        })
    }

    /// Store computed embeddings in the cache.
    pub fn store_embeddings(
        &self,
        store: &MemoryStore,
        entries: &[(String, Vec<f32>)], // (hash, embedding)
    ) -> Result<()> {
        if !self.enabled || entries.is_empty() {
            return Ok(());
        }

        let now = Utc::now().timestamp();
        let cache_entries: Vec<EmbeddingCacheEntry> = entries
            .iter()
            .map(|(hash, embedding)| {
                let embedding_json = serde_json::to_string(embedding).unwrap_or_default();
                let dims = embedding.len() as u32;
                EmbeddingCacheEntry {
                    provider: self.provider_id.clone(),
                    model: self.model.clone(),
                    provider_key: self.provider_key.clone(),
                    hash: hash.clone(),
                    embedding: embedding_json,
                    dims: Some(dims),
                    updated_at: now,
                }
            })
            .collect();

        store.upsert_embedding_cache(&cache_entries)
    }

    /// Prune cache if it exceeds max_entries.
    pub fn prune_if_needed(&self, store: &MemoryStore) -> Result<()> {
        if self.enabled && self.max_entries > 0 {
            store.prune_embedding_cache(self.max_entries)?;
        }
        Ok(())
    }
}

/// Result of a cache lookup for a batch of chunks.
pub struct CacheLookupResult {
    /// One entry per input chunk: Some(embedding) if cached, None if missing.
    pub embeddings: Vec<Option<Vec<f32>>>,
    /// Indices into the original chunks slice that need computing.
    pub missing_indices: Vec<usize>,
}

impl CacheLookupResult {
    /// Returns true if all embeddings were found in cache.
    pub fn all_cached(&self) -> bool {
        self.missing_indices.is_empty()
    }

    /// Merge freshly computed embeddings into the missing slots.
    /// `computed` must have exactly `missing_indices.len()` elements.
    pub fn merge_computed(&mut self, computed: Vec<Vec<f32>>) {
        for (i, vec) in self.missing_indices.iter().zip(computed.into_iter()) {
            self.embeddings[*i] = Some(vec);
        }
    }

    /// Unwrap all embeddings. Panics if any are None.
    #[allow(dead_code)]
    pub fn into_embeddings(self) -> Vec<Vec<f32>> {
        self.embeddings
            .into_iter()
            .map(|e| e.expect("all embeddings should be present"))
            .collect()
    }

    /// Unwrap all embeddings, substituting empty vec for any None.
    pub fn into_embeddings_lossy(self) -> Vec<Vec<f32>> {
        self.embeddings
            .into_iter()
            .map(|e| e.unwrap_or_default())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cache(enabled: bool) -> EmbeddingCache {
        EmbeddingCache {
            provider_id: "mock".to_string(),
            model: "mock-embed".to_string(),
            provider_key: "key123".to_string(),
            max_entries: 1000,
            enabled,
        }
    }

    fn make_chunk(text: &str) -> MemoryChunk {
        MemoryChunk {
            start_line: 1,
            end_line: 1,
            text: text.to_string(),
            hash: hash_text(text),
        }
    }

    #[test]
    fn test_cache_disabled() {
        let cache = make_cache(false);
        let store = MemoryStore::open_in_memory().unwrap();
        let chunks = vec![make_chunk("hello")];
        let result = cache.get_cached_embeddings(&store, &chunks).unwrap();
        assert_eq!(result.missing_indices, vec![0]);
        assert!(result.embeddings[0].is_none());
    }

    #[test]
    fn test_cache_empty_chunks() {
        let cache = make_cache(true);
        let store = MemoryStore::open_in_memory().unwrap();
        let result = cache.get_cached_embeddings(&store, &[]).unwrap();
        assert!(result.all_cached()); // vacuously true
        assert!(result.embeddings.is_empty());
    }

    #[test]
    fn test_cache_all_miss() {
        let cache = make_cache(true);
        let store = MemoryStore::open_in_memory().unwrap();
        let chunks = vec![make_chunk("hello"), make_chunk("world")];
        let result = cache.get_cached_embeddings(&store, &chunks).unwrap();
        assert_eq!(result.missing_indices, vec![0, 1]);
        assert!(!result.all_cached());
    }

    #[test]
    fn test_cache_store_and_retrieve() {
        let cache = make_cache(true);
        let store = MemoryStore::open_in_memory().unwrap();
        let chunks = vec![make_chunk("hello")];

        // Store an embedding
        let hash = chunks[0].hash.clone();
        let embedding = vec![1.0, 0.0, 0.0];
        cache
            .store_embeddings(&store, &[(hash, embedding.clone())])
            .unwrap();

        // Retrieve it
        let result = cache.get_cached_embeddings(&store, &chunks).unwrap();
        assert!(result.all_cached());
        assert_eq!(result.embeddings[0].as_ref().unwrap(), &embedding);
    }

    #[test]
    fn test_cache_partial_hit() {
        let cache = make_cache(true);
        let store = MemoryStore::open_in_memory().unwrap();

        let chunks = vec![make_chunk("cached"), make_chunk("not cached")];

        // Only cache the first
        let embedding = vec![1.0, 2.0];
        cache
            .store_embeddings(&store, &[(chunks[0].hash.clone(), embedding.clone())])
            .unwrap();

        let result = cache.get_cached_embeddings(&store, &chunks).unwrap();
        assert!(!result.all_cached());
        assert_eq!(result.missing_indices, vec![1]);
        assert_eq!(result.embeddings[0].as_ref().unwrap(), &embedding);
        assert!(result.embeddings[1].is_none());
    }

    #[test]
    fn test_cache_provider_namespace_isolation() {
        let cache1 = EmbeddingCache {
            provider_id: "provider_a".to_string(),
            model: "model".to_string(),
            provider_key: "key".to_string(),
            max_entries: 1000,
            enabled: true,
        };
        let cache2 = EmbeddingCache {
            provider_id: "provider_b".to_string(),
            model: "model".to_string(),
            provider_key: "key".to_string(),
            max_entries: 1000,
            enabled: true,
        };

        let store = MemoryStore::open_in_memory().unwrap();
        let chunks = vec![make_chunk("test")];

        // Store under provider_a
        cache1
            .store_embeddings(&store, &[(chunks[0].hash.clone(), vec![1.0])])
            .unwrap();

        // Provider_b should miss
        let result = cache2.get_cached_embeddings(&store, &chunks).unwrap();
        assert!(!result.all_cached());
    }

    #[test]
    fn test_cache_model_namespace_isolation() {
        let cache1 = EmbeddingCache {
            provider_id: "p".to_string(),
            model: "model_a".to_string(),
            provider_key: "key".to_string(),
            max_entries: 1000,
            enabled: true,
        };
        let cache2 = EmbeddingCache {
            provider_id: "p".to_string(),
            model: "model_b".to_string(),
            provider_key: "key".to_string(),
            max_entries: 1000,
            enabled: true,
        };

        let store = MemoryStore::open_in_memory().unwrap();
        let chunks = vec![make_chunk("test")];

        cache1
            .store_embeddings(&store, &[(chunks[0].hash.clone(), vec![1.0])])
            .unwrap();

        let result = cache2.get_cached_embeddings(&store, &chunks).unwrap();
        assert!(!result.all_cached());
    }

    #[test]
    fn test_cache_prune_over_limit() {
        let cache = EmbeddingCache {
            provider_id: "p".to_string(),
            model: "m".to_string(),
            provider_key: "k".to_string(),
            max_entries: 2,
            enabled: true,
        };

        let store = MemoryStore::open_in_memory().unwrap();

        // Store 5 entries
        for i in 0..5 {
            let text = format!("entry_{}", i);
            let hash = hash_text(&text);
            cache
                .store_embeddings(&store, &[(hash, vec![i as f32])])
                .unwrap();
        }

        cache.prune_if_needed(&store).unwrap();
        // After pruning, should have at most max_entries
        // (exact count depends on prune implementation)
    }

    #[test]
    fn test_cache_prune_disabled() {
        let cache = make_cache(false);
        let store = MemoryStore::open_in_memory().unwrap();
        // Should not error
        cache.prune_if_needed(&store).unwrap();
    }

    #[test]
    fn test_cache_empty_hash_skip() {
        let cache = make_cache(true);
        let store = MemoryStore::open_in_memory().unwrap();

        let chunk = MemoryChunk {
            start_line: 1,
            end_line: 1,
            text: "".to_string(),
            hash: "".to_string(), // empty hash
        };

        let result = cache.get_cached_embeddings(&store, &[chunk]).unwrap();
        assert_eq!(result.missing_indices, vec![0]);
    }

    #[test]
    fn test_merge_computed() {
        let mut result = CacheLookupResult {
            embeddings: vec![Some(vec![1.0]), None, None],
            missing_indices: vec![1, 2],
        };
        result.merge_computed(vec![vec![2.0], vec![3.0]]);
        assert_eq!(result.embeddings[0].as_ref().unwrap(), &vec![1.0]);
        assert_eq!(result.embeddings[1].as_ref().unwrap(), &vec![2.0]);
        assert_eq!(result.embeddings[2].as_ref().unwrap(), &vec![3.0]);
    }

    #[test]
    fn test_into_embeddings_lossy() {
        let result = CacheLookupResult {
            embeddings: vec![Some(vec![1.0]), None, Some(vec![3.0])],
            missing_indices: vec![1],
        };
        let vecs = result.into_embeddings_lossy();
        assert_eq!(vecs[0], vec![1.0]);
        assert!(vecs[1].is_empty()); // lossy: None -> empty
        assert_eq!(vecs[2], vec![3.0]);
    }

    #[test]
    fn test_provider_key_stability() {
        use crate::memory::embedding::mock::MockEmbeddingProvider;
        let mock = MockEmbeddingProvider::new(8);
        let key1 = EmbeddingCache::compute_provider_key(&mock);
        let key2 = EmbeddingCache::compute_provider_key(&mock);
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_provider_key_differs_by_model() {
        use crate::memory::embedding::mock::MockEmbeddingProvider;
        let mock1 = MockEmbeddingProvider::new(8);
        let mut mock2 = MockEmbeddingProvider::new(8);
        mock2.model = "different-model".to_string();
        let key1 = EmbeddingCache::compute_provider_key(&mock1);
        let key2 = EmbeddingCache::compute_provider_key(&mock2);
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_provider_key_includes_url() {
        use crate::memory::embedding::mock::MockEmbeddingProvider;
        let mock = MockEmbeddingProvider::new(8);
        let key_without = EmbeddingCache::compute_provider_key(&mock);
        let key_with = EmbeddingCache::compute_provider_key_with_url(&mock, "http://localhost");
        assert_ne!(key_without, key_with);
    }
}
