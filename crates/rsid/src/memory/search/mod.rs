pub(crate) mod hybrid;
pub(crate) mod keyword;
pub(crate) mod mmr;
pub(crate) mod query;
pub(crate) mod temporal_decay;
pub(crate) mod vector;

use std::collections::HashMap;

use crate::error::Result;
use crate::memory::embedding::batch::EmbeddingControl;
use crate::memory::store::MemoryStore;
use crate::memory::types::{EmbeddingProvider, MemoryConfig, MemorySearchResult, MemorySource};

/// Maximum characters for a search result snippet.
const SNIPPET_MAX_CHARS: usize = 700;

/// Internal intermediate type carrying a chunk through the search pipeline.
/// Each pipeline stage reads and/or modifies the `score` field while preserving
/// the rest of the metadata unchanged.
#[derive(Debug, Clone)]
pub(crate) struct ScoredChunk {
    /// Composite chunk ID: "{path}:{source}:{start_line}:{hash}"
    pub id: String,
    /// Relative path to the source file
    pub path: String,
    /// Source classification (Memory or Sessions)
    pub source: MemorySource,
    /// Start line in the source file (1-based)
    pub start_line: u32,
    /// End line in the source file (1-based, inclusive)
    pub end_line: u32,
    /// The chunk text content (used for snippet and MMR Jaccard similarity)
    pub text: String,
    /// Combined relevance score. Mutated by each pipeline stage.
    pub score: f64,
    /// Vector similarity score (set by vector search, 0.0 if keyword-only)
    pub vector_score: f64,
    /// BM25 text score (set by keyword search, 0.0 if vector-only)
    pub text_score: f64,
}

impl ScoredChunk {
    /// Convert to the public API result type, truncating the snippet.
    fn into_search_result(self, snippet_max_chars: usize) -> MemorySearchResult {
        let snippet = if self.text.len() <= snippet_max_chars {
            self.text
        } else {
            let end = self.text.floor_char_boundary(snippet_max_chars);
            self.text[..end].to_string()
        };
        MemorySearchResult {
            path: self.path,
            start_line: self.start_line,
            end_line: self.end_line,
            score: self.score,
            snippet,
            source: self.source,
        }
    }
}

/// Search mode, determined by whether an embedding provider is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchMode {
    /// No embedding provider. Keyword search only via FTS5.
    FtsOnly,
    /// Embedding provider available. Vector search + optional FTS5 hybrid.
    Hybrid,
}

/// Top-level search entry point.
///
/// Orchestrates the full search pipeline: query preprocessing, parallel
/// keyword + vector retrieval, score merging, temporal decay, MMR diversity
/// re-ranking, and final truncation.
///
/// When `project_id` is `Some`, every retrieval mode (FTS-only, hybrid,
/// vector, cosine fallback) restricts results to chunks whose denormalized
/// `project_id` matches. When `None`, the search is unscoped (preserves the
/// pre-V3 global behavior for manual TUI search and debug surfaces).
pub async fn search(
    store: &MemoryStore,
    provider: Option<&dyn EmbeddingProvider>,
    query: &str,
    config: &MemoryConfig,
    project_id: Option<&str>,
    control: Option<&EmbeddingControl>,
) -> Result<Vec<MemorySearchResult>> {
    let cleaned = query.trim();
    if cleaned.is_empty() {
        return Ok(vec![]);
    }

    let max_results = config.max_results as usize;
    let min_score = config.min_score;
    let candidates = (config.max_results)
        .saturating_mul(config.candidate_multiplier)
        .clamp(1, 200) as usize;

    let mode = if provider.is_some() {
        SearchMode::Hybrid
    } else {
        SearchMode::FtsOnly
    };

    let mut results = match mode {
        SearchMode::FtsOnly => search_fts_only(store, cleaned, candidates, project_id)?,
        SearchMode::Hybrid => {
            let provider = provider.unwrap();
            search_hybrid(
                store, provider, cleaned, candidates, config, project_id, control,
            )
            .await?
        }
    };

    // Apply temporal decay (for both modes, if enabled)
    if config.temporal_decay_enabled {
        let now_ms = chrono::Utc::now().timestamp_millis();
        temporal_decay::apply_temporal_decay(
            &mut results,
            config.temporal_decay_half_life_days as f64,
            now_ms,
            store,
        );
        // Re-sort after decay modifies scores
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    // Apply MMR re-ranking (for both modes, if enabled)
    if config.mmr_enabled {
        results = mmr::mmr_rerank(results, config.mmr_lambda);
    }

    // Filter by min_score and truncate
    let final_results: Vec<MemorySearchResult> = results
        .into_iter()
        .filter(|r| r.score >= min_score)
        .take(max_results)
        .map(|r| r.into_search_result(SNIPPET_MAX_CHARS))
        .collect();

    Ok(final_results)
}

/// FTS-only search: extract keywords, search each, merge and deduplicate.
fn search_fts_only(
    store: &MemoryStore,
    query_str: &str,
    candidates: usize,
    project_id: Option<&str>,
) -> Result<Vec<ScoredChunk>> {
    let keywords = query::extract_keywords(query_str);
    let search_terms = if keywords.is_empty() {
        vec![query_str.to_string()]
    } else {
        keywords
    };

    let mut by_id: HashMap<String, ScoredChunk> = HashMap::new();

    for term in &search_terms {
        let results = match keyword::search_keyword(store, term, candidates, None, project_id) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Keyword search failed for term '{}': {}", term, e);
                vec![]
            }
        };
        for r in results {
            by_id
                .entry(r.id.clone())
                .and_modify(|existing| {
                    if r.score > existing.score {
                        *existing = r.clone();
                    }
                })
                .or_insert(r);
        }
    }

    let mut merged: Vec<ScoredChunk> = by_id.into_values().collect();
    merged.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(merged)
}

/// Hybrid search: keyword + vector search, merge results.
async fn search_hybrid(
    store: &MemoryStore,
    provider: &dyn EmbeddingProvider,
    query_str: &str,
    candidates: usize,
    config: &MemoryConfig,
    project_id: Option<&str>,
    control: Option<&EmbeddingControl>,
) -> Result<Vec<ScoredChunk>> {
    // Keyword search (non-fatal on error)
    let keyword_results = match keyword::search_keyword(
        store,
        query_str,
        candidates,
        Some(provider.model()),
        project_id,
    ) {
        Ok(results) => results,
        Err(e) => {
            tracing::warn!("Keyword search failed: {}", e);
            vec![]
        }
    };

    // Embed the query
    let query_vec =
        crate::memory::embedding::batch::embed_query_with_timeout(provider, query_str, control)
            .await?;

    // If embedding is all zeros, return keyword results only
    if query_vec.iter().all(|&v| v == 0.0) {
        return Ok(keyword_results);
    }

    // Vector search (non-fatal on error)
    let vector_results =
        match vector::search_vector(store, &query_vec, candidates, provider.model(), project_id) {
            Ok(results) => results,
            Err(e) => {
                tracing::warn!("Vector search failed: {}", e);
                vec![]
            }
        };

    // Merge
    Ok(hybrid::merge_hybrid_results(
        vector_results,
        keyword_results,
        config.vector_weight,
        config.text_weight,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::error::DaemonError;
    use crate::memory::embedding::batch::EmbeddingControl;
    use crate::memory::types::MemoryConfig;
    use crate::store::Store;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    struct MockEmbeddingProvider {
        query_embedding: Vec<f32>,
    }

    #[async_trait::async_trait]
    impl EmbeddingProvider for MockEmbeddingProvider {
        fn id(&self) -> &str {
            "mock"
        }
        fn model(&self) -> &str {
            "mock-model"
        }
        fn max_input_tokens(&self) -> Option<u32> {
            Some(8192)
        }
        async fn embed_query(
            &self,
            _text: &str,
            _execution: crate::model_control::AdmittedEmbeddingExecution,
        ) -> Result<Vec<f32>> {
            Ok(self.query_embedding.clone())
        }
        async fn embed_batch(
            &self,
            texts: &[String],
            _execution: crate::model_control::AdmittedEmbeddingExecution,
        ) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| self.query_embedding.clone()).collect())
        }
    }

    fn default_config() -> MemoryConfig {
        MemoryConfig {
            max_results: 6,
            min_score: 0.0, // Don't filter by score in most tests
            vector_weight: 0.7,
            text_weight: 0.3,
            candidate_multiplier: 4,
            mmr_enabled: false,
            mmr_lambda: 0.7,
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: 30,
            ..MemoryConfig::default()
        }
    }

    fn embedding_control() -> EmbeddingControl {
        EmbeddingControl {
            store: Arc::new(Mutex::new(
                Store::open_in_memory().expect("model-control store"),
            )),
            event_bus: Arc::new(EventBus::new(8)),
            owner: rsi_common::model_control::InvocationOwner::default(),
            provider: "Local".to_string(),
            backend: "ollama".to_string(),
            model: "mock-model".to_string(),
            base_url: Some("http://127.0.0.1:11434".to_string()),
            trigger: "search_test".to_string(),
            dedup_namespace: format!("search-test:{}", uuid::Uuid::new_v4()),
        }
    }

    fn setup_test_store(chunks: &[(&str, &str, &str, u32, u32, &str, &str, &str)]) -> MemoryStore {
        let store = MemoryStore::open_in_memory().unwrap();
        for (id, path, source, start, end, text, model, embedding) in chunks {
            store
                .conn()
                .execute(
                    "INSERT INTO chunks (id, path, source, start_line, end_line, hash, model, text, embedding, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, 'test-hash', ?6, ?7, ?8, 0)",
                    rusqlite::params![id, path, source, start, end, model, text, embedding],
                )
                .unwrap();
            if store.fts_available() {
                store
                    .conn()
                    .execute(
                        "INSERT INTO chunks_fts (id, path, source, start_line, end_line, text, model) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        rusqlite::params![id, path, source, start, end, text, model],
                    )
                    .unwrap();
            }
        }
        store
    }

    #[tokio::test]
    async fn test_search_empty_query() {
        let store = setup_test_store(&[]);
        let config = default_config();
        let results = search(&store, None, "", &config, None, None).await.unwrap();
        assert!(results.is_empty());

        let results = search(&store, None, "   ", &config, None, None)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_fts_only_mode() {
        let store = setup_test_store(&[(
            "c1",
            "memory/test.md",
            "memory",
            1,
            10,
            "rust programming language",
            "nomic",
            "",
        )]);
        let config = default_config();

        if !store.fts_available() {
            return;
        }

        let results = search(&store, None, "rust", &config, None, None)
            .await
            .unwrap();
        assert!(!results.is_empty());
        assert!(results[0].snippet.contains("rust"));
    }

    #[tokio::test]
    async fn test_search_hybrid_mode() {
        let store = setup_test_store(&[(
            "c1",
            "memory/test.md",
            "memory",
            1,
            10,
            "rust programming",
            "mock-model",
            "[1.0, 0.0, 0.0]",
        )]);
        let config = default_config();
        let provider = MockEmbeddingProvider {
            query_embedding: vec![1.0, 0.0, 0.0],
        };

        let control = embedding_control();
        let results = search(
            &store,
            Some(&provider),
            "rust",
            &config,
            None,
            Some(&control),
        )
        .await
        .unwrap();
        assert!(!results.is_empty());
    }

    #[tokio::test]
    async fn test_search_hybrid_fails_closed_without_embedding_control() {
        let store = setup_test_store(&[]);
        let config = default_config();
        let provider = MockEmbeddingProvider {
            query_embedding: vec![1.0, 0.0, 0.0],
        };

        let error = search(&store, Some(&provider), "rust", &config, None, None)
            .await
            .expect_err("hybrid search without model control must fail");
        assert!(matches!(error, DaemonError::PolicyDenied(_)));
    }

    #[tokio::test]
    async fn test_search_min_score_filter() {
        let store = setup_test_store(&[(
            "c1",
            "memory/test.md",
            "memory",
            1,
            10,
            "rust programming",
            "nomic",
            "",
        )]);
        let mut config = default_config();
        config.min_score = 0.99; // Very high threshold

        if !store.fts_available() {
            return;
        }

        let results = search(&store, None, "rust", &config, None, None)
            .await
            .unwrap();
        // Score from FTS is unlikely to be >= 0.99
        // This tests that the filter works (may or may not filter depending on BM25)
        for r in &results {
            assert!(r.score >= 0.99);
        }
    }

    #[tokio::test]
    async fn test_search_max_results_limit() {
        let store = setup_test_store(&[
            ("c1", "a.md", "memory", 1, 5, "rust safety", "nomic", ""),
            ("c2", "b.md", "memory", 1, 5, "rust memory", "nomic", ""),
            (
                "c3",
                "c.md",
                "memory",
                1,
                5,
                "rust performance",
                "nomic",
                "",
            ),
            (
                "c4",
                "d.md",
                "memory",
                1,
                5,
                "rust concurrency",
                "nomic",
                "",
            ),
        ]);
        let mut config = default_config();
        config.max_results = 2;

        if !store.fts_available() {
            return;
        }

        let results = search(&store, None, "rust", &config, None, None)
            .await
            .unwrap();
        assert!(results.len() <= 2);
    }

    #[tokio::test]
    async fn test_search_candidate_multiplier() {
        let config = default_config();
        let candidates = (config.max_results)
            .saturating_mul(config.candidate_multiplier)
            .max(1)
            .min(200) as usize;
        assert_eq!(candidates, 24); // 6 * 4
    }

    #[tokio::test]
    async fn test_search_candidate_multiplier_capped() {
        let mut config = default_config();
        config.max_results = 100;
        config.candidate_multiplier = 4;
        let candidates = (config.max_results)
            .saturating_mul(config.candidate_multiplier)
            .max(1)
            .min(200) as usize;
        assert_eq!(candidates, 200); // Capped at 200
    }

    #[tokio::test]
    async fn test_search_snippet_truncation() {
        let long_text = "x".repeat(1000);
        let store = setup_test_store(&[(
            "c1",
            "memory/test.md",
            "memory",
            1,
            10,
            &long_text,
            "nomic",
            "",
        )]);
        let config = default_config();

        if !store.fts_available() {
            return;
        }

        // Need to match something, inject the keyword
        let text_with_keyword = format!("findme {}", "x".repeat(1000));
        store
            .conn()
            .execute(
                "UPDATE chunks SET text = ?1 WHERE id = 'c1'",
                [&text_with_keyword],
            )
            .unwrap();
        // Update FTS too
        store
            .conn()
            .execute("DELETE FROM chunks_fts WHERE id = 'c1'", [])
            .unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO chunks_fts (id, path, source, start_line, end_line, text, model) \
                 VALUES ('c1', 'memory/test.md', 'memory', 1, 10, ?1, 'nomic')",
                [&text_with_keyword],
            )
            .unwrap();

        let results = search(&store, None, "findme", &config, None, None)
            .await
            .unwrap();
        if !results.is_empty() {
            assert!(results[0].snippet.len() <= SNIPPET_MAX_CHARS);
        }
    }

    #[tokio::test]
    async fn test_search_conversion_to_search_result() {
        let chunk = ScoredChunk {
            id: "test:memory:1:hash".to_string(),
            path: "memory/test.md".to_string(),
            source: MemorySource::Memory,
            start_line: 5,
            end_line: 15,
            text: "hello world".to_string(),
            score: 0.85,
            vector_score: 0.8,
            text_score: 0.3,
        };
        let result = chunk.into_search_result(700);
        assert_eq!(result.path, "memory/test.md");
        assert_eq!(result.start_line, 5);
        assert_eq!(result.end_line, 15);
        assert!((result.score - 0.85).abs() < f64::EPSILON);
        assert_eq!(result.snippet, "hello world");
        assert_eq!(result.source, MemorySource::Memory);
    }

    #[tokio::test]
    async fn test_search_fts_only_extracts_keywords() {
        let store = setup_test_store(&[
            (
                "c1",
                "memory/test.md",
                "memory",
                1,
                10,
                "rust programming language",
                "nomic",
                "",
            ),
            (
                "c2",
                "memory/other.md",
                "memory",
                1,
                5,
                "python scripting",
                "nomic",
                "",
            ),
        ]);
        let config = default_config();

        if !store.fts_available() {
            return;
        }

        // "the rust" -> keyword extraction removes "the", searches for "rust"
        let results = search(&store, None, "the rust", &config, None, None)
            .await
            .unwrap();
        assert!(!results.is_empty());
        assert!(results[0].snippet.contains("rust"));
    }

    #[tokio::test]
    async fn test_search_fts_only_fallback_to_raw_query() {
        let store = setup_test_store(&[(
            "c1",
            "memory/test.md",
            "memory",
            1,
            10,
            "the is a to for",
            "nomic",
            "",
        )]);
        let config = default_config();

        if !store.fts_available() {
            return;
        }

        // All stop words -> falls back to raw query
        let results = search(&store, None, "the is a", &config, None, None)
            .await
            .unwrap();
        // May or may not find results, but shouldn't crash
        let _ = results;
    }

    #[tokio::test]
    async fn test_search_temporal_decay_applied() {
        let store = setup_test_store(&[(
            "c1",
            "memory/2020-01-01.md",
            "memory",
            1,
            10,
            "old rust content",
            "nomic",
            "",
        )]);
        let mut config = default_config();
        config.temporal_decay_enabled = true;
        config.temporal_decay_half_life_days = 30;

        if !store.fts_available() {
            return;
        }

        let results = search(&store, None, "rust", &config, None, None)
            .await
            .unwrap();
        // Score should be decayed because the file is very old
        if !results.is_empty() {
            assert!(results[0].score < 1.0);
        }
    }

    #[tokio::test]
    async fn test_search_mmr_applied() {
        let store = setup_test_store(&[
            (
                "c1",
                "a.md",
                "memory",
                1,
                5,
                "rust memory safety",
                "nomic",
                "",
            ),
            (
                "c2",
                "b.md",
                "memory",
                1,
                5,
                "rust memory allocation",
                "nomic",
                "",
            ),
            (
                "c3",
                "c.md",
                "memory",
                1,
                5,
                "rust concurrency async",
                "nomic",
                "",
            ),
        ]);
        let mut config = default_config();
        config.mmr_enabled = true;
        config.mmr_lambda = 0.7;

        if !store.fts_available() {
            return;
        }

        let results = search(&store, None, "rust", &config, None, None)
            .await
            .unwrap();
        // All items should be present (MMR re-orders, doesn't remove)
        assert!(results.len() <= 3);
    }

    #[tokio::test]
    async fn test_search_both_decay_and_mmr() {
        let store = setup_test_store(&[
            (
                "c1",
                "memory/2026-02-01.md",
                "memory",
                1,
                5,
                "rust programming",
                "nomic",
                "",
            ),
            (
                "c2",
                "memory/2026-02-20.md",
                "memory",
                1,
                5,
                "rust development",
                "nomic",
                "",
            ),
        ]);
        let mut config = default_config();
        config.temporal_decay_enabled = true;
        config.temporal_decay_half_life_days = 30;
        config.mmr_enabled = true;
        config.mmr_lambda = 0.7;

        if !store.fts_available() {
            return;
        }

        let results = search(&store, None, "rust", &config, None, None)
            .await
            .unwrap();
        // Should not crash with both enabled
        let _ = results;
    }
}
