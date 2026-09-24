use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{DaemonError, Result};

/// Origin of indexed content: memory files on disk or session transcripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MemorySource {
    /// Markdown files in ~/.flywheel/memory/
    Memory,
    /// Extracted text from session conversation events
    Sessions,
}

impl MemorySource {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemorySource::Memory => "memory",
            MemorySource::Sessions => "sessions",
        }
    }
}

impl std::fmt::Display for MemorySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parse from SQLite TEXT column.
pub fn str_to_memory_source(s: &str) -> Result<MemorySource> {
    match s {
        "memory" => Ok(MemorySource::Memory),
        "sessions" => Ok(MemorySource::Sessions),
        other => Err(DaemonError::Store(format!(
            "Unknown memory source: {}",
            other
        ))),
    }
}

/// A single search hit returned from the memory index.
/// Sent over RPC to the TUI, so it must be Serialize + Deserialize.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchResult {
    /// Relative path to the source file (e.g., "memory/2026-02-28.md")
    pub path: String,
    /// Start line in the source file (1-based)
    pub start_line: u32,
    /// End line in the source file (1-based, inclusive)
    pub end_line: u32,
    /// Relevance score (0.0 to 1.0, higher is more relevant)
    pub score: f64,
    /// Text snippet of the matching chunk
    pub snippet: String,
    /// Whether this came from a memory file or a session transcript
    pub source: MemorySource,
}

/// Metadata for a tracked file in the memory index.
/// Represents a row in the `files` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryFileEntry {
    /// Relative path from the memory root (e.g., "MEMORY.md", "memory/2026-02-28.md")
    pub path: String,
    /// Absolute path on disk
    pub abs_path: PathBuf,
    /// File modification time as Unix timestamp in milliseconds
    pub mtime_ms: i64,
    /// File size in bytes
    pub size: i64,
    /// SHA-256 hex digest of file content
    pub hash: String,
    /// Source classification
    pub source: MemorySource,
    /// Project scope (denormalized): `Some(pid)` for session transcripts
    /// belonging to project `pid`; `None` for global memory files or
    /// for sessions not attached to a project.
    #[serde(default)]
    pub project_id: Option<uuid::Uuid>,
    /// Cheap change-detection summary for session transcripts, as produced by
    /// `Store::event_watermark`. `None` for on-disk memory files (which use
    /// mtime/size) and for pre-V4 session rows that have not been reindexed
    /// since the column was added.
    #[serde(default)]
    pub watermark: Option<String>,
}

/// A text chunk produced by the Markdown chunker.
/// Before persistence, has no `id` — the ID is generated as a composite key
/// from (path, source, start_line, hash).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryChunk {
    /// Start line in the source file (1-based)
    pub start_line: u32,
    /// End line in the source file (1-based, inclusive)
    pub end_line: u32,
    /// The chunk text content
    pub text: String,
    /// SHA-256 hex digest of the text content
    pub hash: String,
}

/// A chunk as stored in the SQLite `chunks` table.
/// Extends MemoryChunk with persistence metadata.
#[derive(Debug, Clone)]
pub struct StoredChunk {
    /// Composite primary key: "{path}:{source}:{start_line}:{hash}"
    pub id: String,
    /// Relative path of the source file
    pub path: String,
    /// Source classification
    pub source: MemorySource,
    /// Start line in the source file (1-based)
    pub start_line: u32,
    /// End line in the source file (1-based, inclusive)
    pub end_line: u32,
    /// SHA-256 hex digest of the text content
    pub hash: String,
    /// Embedding model used when this chunk was indexed
    pub model: String,
    /// The chunk text content
    pub text: String,
    /// JSON-serialized f32 array of the embedding vector, or empty string if no embedding
    pub embedding: String,
    /// Unix timestamp (seconds) of last update
    pub updated_at: i64,
    /// Project scope (denormalized): mirrors the indexed file's `project_id`.
    /// `None` means global / unscoped (memory files or pre-V3 chunks).
    pub project_id: Option<String>,
}

impl StoredChunk {
    /// Generate the composite ID for a chunk.
    pub fn make_id(path: &str, source: MemorySource, start_line: u32, hash: &str) -> String {
        format!("{}:{}:{}:{}", path, source.as_str(), start_line, hash)
    }
}

/// Metadata about the current state of the memory index.
/// Stored as key-value pairs in the `meta` table.
/// Used to detect when a full reindex is needed (model/provider/settings changed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryIndexMeta {
    /// Embedding model name (e.g., "nomic-embed-text")
    pub model: String,
    /// Embedding provider identifier (e.g., "ollama", "openai")
    pub provider: String,
    /// Provider-specific key for cache namespacing (e.g., API key hash)
    pub provider_key: Option<String>,
    /// Which sources are indexed
    pub sources: Vec<MemorySource>,
    /// Chunk size in approximate tokens
    pub chunk_tokens: u32,
    /// Overlap between consecutive chunks in approximate tokens
    pub chunk_overlap: u32,
    /// Dimensionality of stored embedding vectors (None = not yet known)
    pub vector_dims: Option<u32>,
}

/// A cached embedding for a specific text hash + provider + model combination.
/// Avoids re-computing embeddings for unchanged content across reindexes.
#[derive(Debug, Clone)]
pub struct EmbeddingCacheEntry {
    /// Embedding provider name
    pub provider: String,
    /// Embedding model name
    pub model: String,
    /// Provider-specific key (e.g., API key hash) for cache isolation
    pub provider_key: String,
    /// SHA-256 hex digest of the source text
    pub hash: String,
    /// JSON-serialized f32 array of the embedding vector
    pub embedding: String,
    /// Dimensionality of the embedding
    pub dims: Option<u32>,
    /// Unix timestamp (seconds) of last access/update
    pub updated_at: i64,
}

/// Status report for the memory subsystem. Sent to TUI via RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryProviderStatus {
    /// Always "builtin" for Flywheel
    pub backend: String,
    /// Embedding provider name (e.g., "ollama", "openai", "none")
    pub provider: String,
    /// Embedding model name
    pub model: Option<String>,
    /// Number of indexed files
    pub file_count: u32,
    /// Number of indexed chunks
    pub chunk_count: u32,
    /// Whether the index has unsynced changes
    pub dirty: bool,
    /// Path to the memory SQLite database
    pub db_path: String,
    /// FTS5 availability
    pub fts_available: bool,
    /// Vector search availability (sqlite-vec loaded + dims known)
    pub vector_available: bool,
    /// Vector dimensionality (None if not yet determined)
    pub vector_dims: Option<u32>,
    /// Embedding cache stats
    pub cache_entries: u32,
    /// Number of stored observations
    pub observation_count: u32,
}

/// Configuration for the memory subsystem.
/// Populated from environment variables and/or config file, with sane defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Whether the memory system is enabled
    pub enabled: bool,
    /// Root directory for memory files (default: ~/.flywheel/memory/)
    pub memory_dir: PathBuf,
    /// Path to the memory SQLite database (default: ~/.flywheel/memory.sqlite)
    pub db_path: PathBuf,
    /// Which sources to index
    pub sources: Vec<MemorySource>,
    /// Embedding provider ("ollama", "openai", "auto", "none")
    pub embedding_provider: String,
    /// Embedding model name (provider-specific)
    pub embedding_model: String,
    /// Base URL for embedding API (Ollama: http://localhost:11434)
    pub embedding_url: Option<String>,
    /// API key for remote embedding providers
    pub embedding_api_key: Option<String>,
    /// Chunking settings
    pub chunk_tokens: u32,
    pub chunk_overlap: u32,
    /// Search settings
    pub max_results: u32,
    pub min_score: f64,
    /// Hybrid search weights
    pub vector_weight: f64,
    pub text_weight: f64,
    /// Candidate multiplier for hybrid search
    pub candidate_multiplier: u32,
    /// MMR re-ranking
    pub mmr_enabled: bool,
    pub mmr_lambda: f64,
    /// Temporal decay
    pub temporal_decay_enabled: bool,
    pub temporal_decay_half_life_days: u32,
    /// Sync settings
    pub sync_on_session_start: bool,
    pub sync_on_search: bool,
    pub watch_enabled: bool,
    pub watch_debounce_ms: u64,
    /// Embedding cache
    pub cache_enabled: bool,
    pub cache_max_entries: u32,
    /// Whether observation extraction is enabled on session completion.
    pub observation_extraction_enabled: bool,
    /// Maximum session text characters to send to the extraction LLM.
    pub observation_max_input_chars: usize,
    /// Minimum number of conversation events before extraction triggers.
    pub observation_min_events: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        use rsi_common::identity;
        let base = dirs::home_dir()
            .map(|h| h.join(identity::PROJECT_DIR_NAME))
            .unwrap_or_else(|| PathBuf::from(".").join(identity::PROJECT_DIR_NAME));
        Self {
            enabled: true,
            memory_dir: base.join("memory"),
            db_path: base.join("memory.sqlite"),
            sources: vec![MemorySource::Memory],
            embedding_provider: "auto".to_string(),
            embedding_model: "qwen3-embedding:0.6b".to_string(),
            embedding_url: None,
            embedding_api_key: None,
            chunk_tokens: 400,
            chunk_overlap: 80,
            max_results: 6,
            min_score: 0.35,
            vector_weight: 0.7,
            text_weight: 0.3,
            candidate_multiplier: 4,
            mmr_enabled: false,
            mmr_lambda: 0.7,
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: 30,
            sync_on_session_start: true,
            sync_on_search: true,
            watch_enabled: true,
            watch_debounce_ms: 1500,
            cache_enabled: true,
            cache_max_entries: 10_000,
            observation_extraction_enabled: true,
            observation_max_input_chars: 16_000,
            observation_min_events: 4,
        }
    }
}

/// Trait for embedding providers. Implementations compute vector embeddings
/// from text input. Used by the indexing pipeline and search query path.
///
/// All methods are async because embedding may involve HTTP calls to
/// local (Ollama) or remote (OpenAI) APIs.
#[async_trait::async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Provider identifier (e.g., "ollama", "openai")
    fn id(&self) -> &str;

    /// Model name (e.g., "nomic-embed-text", "text-embedding-3-small")
    fn model(&self) -> &str;

    /// Maximum input tokens per text (for chunk enforcement)
    fn max_input_tokens(&self) -> Option<u32>;

    /// Embed a single query string. Returns the embedding vector.
    async fn embed_query(
        &self,
        text: &str,
        execution: crate::model_control::AdmittedEmbeddingExecution,
    ) -> Result<Vec<f32>>;

    /// Embed a batch of text strings. Returns one embedding vector per input.
    async fn embed_batch(
        &self,
        texts: &[String],
        execution: crate::model_control::AdmittedEmbeddingExecution,
    ) -> Result<Vec<Vec<f32>>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_source_as_str() {
        assert_eq!(MemorySource::Memory.as_str(), "memory");
        assert_eq!(MemorySource::Sessions.as_str(), "sessions");
    }

    #[test]
    fn test_str_to_memory_source_valid() {
        assert_eq!(
            str_to_memory_source("memory").unwrap(),
            MemorySource::Memory
        );
        assert_eq!(
            str_to_memory_source("sessions").unwrap(),
            MemorySource::Sessions
        );
    }

    #[test]
    fn test_str_to_memory_source_invalid() {
        let err = str_to_memory_source("unknown").unwrap_err();
        match err {
            DaemonError::Store(msg) => assert!(msg.contains("Unknown memory source")),
            _ => panic!("Expected DaemonError::Store"),
        }
    }

    #[test]
    fn test_memory_search_result_serde() {
        let result = MemorySearchResult {
            path: "memory/2026-02-28.md".to_string(),
            start_line: 10,
            end_line: 20,
            score: 0.85,
            snippet: "test snippet".to_string(),
            source: MemorySource::Memory,
        };
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: MemorySearchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.path, result.path);
        assert_eq!(deserialized.start_line, result.start_line);
        assert_eq!(deserialized.end_line, result.end_line);
        assert!((deserialized.score - result.score).abs() < f64::EPSILON);
        assert_eq!(deserialized.snippet, result.snippet);
        assert_eq!(deserialized.source, result.source);
    }

    #[test]
    fn test_memory_provider_status_serde() {
        let status = MemoryProviderStatus {
            backend: "builtin".to_string(),
            provider: "ollama".to_string(),
            model: Some("nomic-embed-text".to_string()),
            file_count: 42,
            chunk_count: 1000,
            dirty: false,
            db_path: "/home/user/.flywheel/memory.sqlite".to_string(),
            fts_available: true,
            vector_available: false,
            vector_dims: None,
            cache_entries: 500,
            observation_count: 15,
        };
        let json = serde_json::to_string(&status).unwrap();
        let deserialized: MemoryProviderStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.backend, status.backend);
        assert_eq!(deserialized.provider, status.provider);
        assert_eq!(deserialized.model, status.model);
        assert_eq!(deserialized.file_count, status.file_count);
        assert_eq!(deserialized.chunk_count, status.chunk_count);
        assert_eq!(deserialized.dirty, status.dirty);
        assert_eq!(deserialized.fts_available, status.fts_available);
        assert_eq!(deserialized.vector_available, status.vector_available);
        assert_eq!(deserialized.vector_dims, status.vector_dims);
        assert_eq!(deserialized.cache_entries, status.cache_entries);
    }

    #[test]
    fn test_stored_chunk_make_id() {
        let id = StoredChunk::make_id("memory/test.md", MemorySource::Memory, 5, "abc123");
        assert_eq!(id, "memory/test.md:memory:5:abc123");

        let id2 = StoredChunk::make_id("sessions/x.md", MemorySource::Sessions, 0, "deadbeef");
        assert_eq!(id2, "sessions/x.md:sessions:0:deadbeef");
    }

    #[test]
    fn test_memory_index_meta_serde() {
        let meta = MemoryIndexMeta {
            model: "nomic-embed-text".to_string(),
            provider: "ollama".to_string(),
            provider_key: Some("key123".to_string()),
            sources: vec![MemorySource::Memory, MemorySource::Sessions],
            chunk_tokens: 400,
            chunk_overlap: 80,
            vector_dims: Some(384),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let deserialized: MemoryIndexMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.model, meta.model);
        assert_eq!(deserialized.provider, meta.provider);
        assert_eq!(deserialized.provider_key, meta.provider_key);
        assert_eq!(deserialized.sources, meta.sources);
        assert_eq!(deserialized.chunk_tokens, meta.chunk_tokens);
        assert_eq!(deserialized.chunk_overlap, meta.chunk_overlap);
        assert_eq!(deserialized.vector_dims, meta.vector_dims);

        // Test with None optionals
        let meta_none = MemoryIndexMeta {
            model: "m".to_string(),
            provider: "p".to_string(),
            provider_key: None,
            sources: vec![MemorySource::Memory],
            chunk_tokens: 200,
            chunk_overlap: 40,
            vector_dims: None,
        };
        let json2 = serde_json::to_string(&meta_none).unwrap();
        let deser2: MemoryIndexMeta = serde_json::from_str(&json2).unwrap();
        assert_eq!(deser2.provider_key, None);
        assert_eq!(deser2.vector_dims, None);
    }

    #[test]
    fn test_memory_config_default() {
        let cfg = MemoryConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.chunk_tokens, 400);
        assert_eq!(cfg.chunk_overlap, 80);
        assert_eq!(cfg.max_results, 6);
        assert!((cfg.min_score - 0.35).abs() < f64::EPSILON);
        assert!((cfg.vector_weight - 0.7).abs() < f64::EPSILON);
        assert!((cfg.text_weight - 0.3).abs() < f64::EPSILON);
        assert_eq!(cfg.candidate_multiplier, 4);
        assert!(!cfg.mmr_enabled);
        assert!((cfg.mmr_lambda - 0.7).abs() < f64::EPSILON);
        assert!(!cfg.temporal_decay_enabled);
        assert_eq!(cfg.temporal_decay_half_life_days, 30);
        assert!(cfg.sync_on_session_start);
        assert!(cfg.sync_on_search);
        assert!(cfg.watch_enabled);
        assert_eq!(cfg.watch_debounce_ms, 1500);
        assert!(cfg.cache_enabled);
        assert_eq!(cfg.cache_max_entries, 10_000);
        assert_eq!(cfg.embedding_provider, "auto");
        assert_eq!(cfg.embedding_model, "qwen3-embedding:0.6b");
        assert!(cfg.db_path.to_string_lossy().contains("memory.sqlite"));
        assert!(cfg.memory_dir.to_string_lossy().contains("memory"));
        assert_eq!(cfg.sources, vec![MemorySource::Memory]);
    }
}
