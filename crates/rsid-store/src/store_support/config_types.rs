//! Config-owned value types and knob bounds (moved down from `queue::types`,
//! `memory::types`, `topology::agent` and `governor`, which re-export them).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

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
pub const DEFAULT_BULK_FANOUT_MIN_OPENROUTER: u32 = 4;

/// Knob range; `0` disables the fan-out rule.
pub const BULK_FANOUT_MIN_OPENROUTER_MAX: u32 = 64;

/// Default of the operator knob `governor_check_slots` (#2430).
pub const DEFAULT_GOVERNOR_CHECK_SLOTS: u32 = 2;

fn default_check_slots() -> u32 {
    DEFAULT_GOVERNOR_CHECK_SLOTS
}

/// Operator policy. Defaults equal the values cargo-slot shipped with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GovernorPolicy {
    pub build_slots: u32,
    pub lander_slots: u32,
    /// Slots of the fast `cargo check` lane (#2430), a pool of its own.
    #[serde(default = "default_check_slots")]
    pub check_slots: u32,
    /// Maximum 1-minute load; `0` means 1.25 x logical cores.
    pub max_load: u32,
    pub min_free_disk_gb: u64,
    pub min_avail_mem_gb: u64,
    pub max_workers_slice_gb: u64,
}

impl GovernorPolicy {
    /// The values cargo-slot shipped with: a 32-core Linux hub.
    pub const LINUX_DEFAULT: Self = Self {
        build_slots: 4,
        lander_slots: 5,
        check_slots: DEFAULT_GOVERNOR_CHECK_SLOTS,
        max_load: 0,
        min_free_disk_gb: 30,
        min_avail_mem_gb: 16,
        max_workers_slice_gb: 30,
    };

    /// A 16 GB, 12-core Mac Mini satellite (#2937). One rsid cargo unit
    /// (two rustc workers plus a link of the 120 MB binary) peaks around
    /// 4-6 GB, so two concurrent units already use most of the machine and a
    /// third swaps: 2 build slots, 1 lander and 1 check slot. Memory is
    /// "available" as vm_stat reports it (free + inactive + speculative
    /// pages, no purgeable double count), and a new unit needs 4 GB of it
    /// (the Linux 16 GB floor would hold every start on this machine). The
    /// load cap stays 1.25 x cores; there is no workers cgroup slice here.
    pub const MACOS_16GB_DEFAULT: Self = Self {
        build_slots: 2,
        lander_slots: 1,
        check_slots: 1,
        max_load: 0,
        min_free_disk_gb: 30,
        min_avail_mem_gb: 4,
        max_workers_slice_gb: 30,
    };

    /// The default for the host this daemon runs on.
    #[must_use]
    pub const fn platform_default() -> Self {
        if cfg!(target_os = "macos") {
            Self::MACOS_16GB_DEFAULT
        } else {
            Self::LINUX_DEFAULT
        }
    }
}

impl Default for GovernorPolicy {
    fn default() -> Self {
        Self::platform_default()
    }
}

// The live `governor_build_slots` / `governor_min_avail_mem_gb` settings,
// mirrored for the daemon's own macOS job capacity gate (#2951), which has no
// RuntimeConfig at its call site (`agent_jobs::submit_with_hold`,
// `start_queued_jobs`). Same custody as `thermal_guard::set_ceiling_c`: the
// store publishes on every accepted write and on the startup apply.
static LIVE_BUILD_SLOTS: AtomicU32 = AtomicU32::new(GovernorPolicy::platform_default().build_slots);
static LIVE_MIN_AVAIL_MEM_GB: AtomicU64 =
    AtomicU64::new(GovernorPolicy::platform_default().min_avail_mem_gb);

impl GovernorPolicy {
    /// Publish the operator's `build_slots` and `min_avail_mem_gb` for
    /// [`Self::live_job_gate`].
    pub fn publish_live_job_gate(&self) {
        LIVE_BUILD_SLOTS.store(self.build_slots, Ordering::Relaxed);
        LIVE_MIN_AVAIL_MEM_GB.store(self.min_avail_mem_gb, Ordering::Relaxed);
    }

    /// The platform default with the two live settings the job capacity gate
    /// reads (`build_slots`, `min_avail_mem_gb`) applied; the daemon-side gate
    /// uses no other field.
    #[must_use]
    pub fn live_job_gate() -> Self {
        Self {
            build_slots: LIVE_BUILD_SLOTS.load(Ordering::Relaxed),
            min_avail_mem_gb: LIVE_MIN_AVAIL_MEM_GB.load(Ordering::Relaxed),
            ..Self::platform_default()
        }
    }
}
