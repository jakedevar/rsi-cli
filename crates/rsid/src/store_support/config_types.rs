//! Config-owned value types and knob bounds (moved down from `queue::types`,
//! `memory::types`, `topology::agent` and `governor`, which re-export them).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Configuration for the background queue.
#[derive(Debug, Clone)]
pub struct QueueConfig {
    /// Polling interval in seconds. Default: 30.
    pub poll_interval_secs: u64,
    /// Default token threshold for batching. Default: 1024.
    pub default_token_threshold: i64,
    /// Stale claim timeout in seconds. Default: 300 (5 minutes).
    pub stale_claim_timeout_secs: i64,
    /// Max retry attempts per task. Default: 5.
    pub max_attempts: i32,
    /// Retention period for completed items in seconds. Default: 86400 (24 hours).
    pub completed_retention_secs: i64,
    /// Whether the queue is enabled. Default: true.
    pub enabled: bool,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: 30,
            default_token_threshold: 1024,
            stale_claim_timeout_secs: 300,
            max_attempts: 5,
            completed_retention_secs: 86400,
            enabled: true,
        }
    }
}

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

/// Default of the operator knob `topology_bulk_fanout_min_openrouter`: a
/// layer with this many same-kind session nodes must run on `OpenRouter`.
pub(crate) const DEFAULT_BULK_FANOUT_MIN_OPENROUTER: u32 = 4;

/// Knob range; `0` disables the fan-out rule.
pub(crate) const BULK_FANOUT_MIN_OPENROUTER_MAX: u32 = 64;

/// Operator policy. Defaults equal the values cargo-slot shipped with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GovernorPolicy {
    pub build_slots: u32,
    pub lander_slots: u32,
    /// Maximum 1-minute load; `0` means 1.25 x logical cores.
    pub max_load: u32,
    pub min_free_disk_gb: u64,
    pub min_avail_mem_gb: u64,
    pub max_workers_slice_gb: u64,
}

impl Default for GovernorPolicy {
    fn default() -> Self {
        Self {
            build_slots: 4,
            lander_slots: 5,
            max_load: 0,
            min_free_disk_gb: 30,
            min_avail_mem_gb: 16,
            max_workers_slice_gb: 30,
        }
    }
}
