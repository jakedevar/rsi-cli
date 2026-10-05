use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use parking_lot::RwLock;
use rsi_common::sandbox_storage::{
    SANDBOX_BUILD_CACHE_RECLAIM_HIGH_WATERMARK_PCT_MAX,
    SANDBOX_BUILD_CACHE_RECLAIM_HIGH_WATERMARK_PCT_MIN,
    SANDBOX_BUILD_CACHE_RECLAIM_INTERVAL_SECS_MAX, SANDBOX_BUILD_CACHE_RECLAIM_INTERVAL_SECS_MIN,
    SANDBOX_BUILD_CACHE_RECLAIM_LOW_WATERMARK_PCT_MAX,
    SANDBOX_BUILD_CACHE_RECLAIM_LOW_WATERMARK_PCT_MIN,
    SANDBOX_BUILD_CACHE_RECLAIM_MAX_CANDIDATES_MAX, SANDBOX_BUILD_CACHE_RECLAIM_MAX_CANDIDATES_MIN,
    SANDBOX_BUILD_CACHE_RECLAIM_TTL_SECS_MAX, SANDBOX_BUILD_CACHE_RECLAIM_TTL_SECS_MIN,
    SandboxBuildCacheReclaimConfig,
};
use rsi_common::types::SessionProvider;

use crate::store_support::config_types::MemoryConfig;

/// Read `RSI_<suffix>` first, falling back to `MOTHERSHIP_<suffix>` then
/// `FLYWHEEL_<suffix>`. Returns the same `Result` shape as `std::env::var`
/// so call sites can keep their existing `.ok()` / `.unwrap_or_else(|_| ...)`
/// chains unchanged.
macro_rules! env_var_legacy {
    ($suffix:literal) => {
        rsi_common::identity::env_with_legacy(
            concat!("RSI_", $suffix),
            &[
                concat!("MOTHERSHIP_", $suffix),
                concat!("FLYWHEEL_", $suffix),
            ],
        )
    };
}

/// Canonical kebab-case slugs for the four-variant `system_prompt_preset`
/// (RSI-026). Mirrors the TUI's `SystemPromptPreset` enum.
pub const SYSTEM_PROMPT_PRESETS: &[&str] = &["default", "concise", "code-only", "caveman"];

pub const SANDBOX_BUILD_CACHE_RECLAIM_TTL_SECS_DEFAULT: u64 = 6 * 60 * 60;
pub const SANDBOX_BUILD_CACHE_RECLAIM_INTERVAL_SECS_DEFAULT: u64 = 60 * 60;
pub const SANDBOX_BUILD_CACHE_RECLAIM_HIGH_WATERMARK_PCT_DEFAULT: u8 = 85;
pub const SANDBOX_BUILD_CACHE_RECLAIM_LOW_WATERMARK_PCT_DEFAULT: u8 = 75;
pub const SANDBOX_BUILD_CACHE_RECLAIM_MAX_CANDIDATES_DEFAULT: u32 = 64;

/// Default systemd user-scope limits for rsid. Memory values use MiB on the
/// operator-facing RPC and TUI surfaces.
pub const RSID_SCOPE_MEMORY_HIGH_MIB_DEFAULT: u64 = 6 * 1024;
pub const RSID_SCOPE_MEMORY_MAX_MIB_DEFAULT: u64 = 8 * 1024;
pub const RSID_SCOPE_MEMORY_SWAP_MAX_MIB_DEFAULT: u64 = 0;
pub const RSID_SCOPE_CPU_WEIGHT_DEFAULT: u32 = 20;
pub const RSID_SCOPE_MEMORY_LIMIT_MIB_MIN: u64 = 256;
pub const RSID_SCOPE_MEMORY_LIMIT_MIB_MAX: u64 = 1024 * 1024;
pub const RSID_SCOPE_CPU_WEIGHT_MIN: u32 = 1;
pub const RSID_SCOPE_CPU_WEIGHT_MAX: u32 = 10_000;

/// Limits on the common worker slice, separate from the daemon scope.
pub const WORKER_SCOPE_MEMORY_SWAP_MAX_MIB_DEFAULT: u64 = 0;
/// Maximum accounted bytes retained for completed transcript reads.
pub const COMPLETED_TRANSCRIPT_CACHE_MAX_BYTES_DEFAULT: u64 = 64 * 1024 * 1024;
pub const SESSION_RETENTION_WINDOW_HOURS_DEFAULT: u64 = 24;
pub const WORKER_SCOPE_CPU_WEIGHT_DEFAULT: u32 = 20;
/// #966: live-context budget at which an OpenRouter session compacts. An
/// agent step costs roughly its live context, so a small budget keeps cached
/// re-reads cheap. `0` turns the absolute budget off.
pub const OPENROUTER_CONTEXT_BUDGET_TOKENS_DEFAULT: u64 = 128_000;
pub const OPENROUTER_CONTEXT_BUDGET_TOKENS_MIN: u64 = 32_000;
pub const OPENROUTER_CONTEXT_BUDGET_TOKENS_MAX: u64 = 2_000_000;
/// #1005: live-context tokens at which a coordinating seat (manager seat,
/// area manager, Epic lead, global manager) is rotated to a fresh successor
/// with a daemon-written handoff. `0` turns the cap off. A per-provider or
/// per-model override is the dynamic field `coordinator_context_cap.<key>`.
/// Off by default until #1156 (a crash after a sandboxed seat's successor was
/// bound to the seat's custody leaves the rotation open for the operator to
/// finish by hand) is fully recovered; the operator can turn it on.
pub const COORDINATOR_CONTEXT_CAP_TOKENS_DEFAULT: u64 = 0;
pub const COORDINATOR_CONTEXT_CAP_TOKENS_MIN: u64 = 32_000;
pub const COORDINATOR_CONTEXT_CAP_TOKENS_MAX: u64 = 2_000_000;
/// Prefix of a per-provider (`<Provider>`) or per-model
/// (`<Provider>/<model>`) cap override field.
pub const COORDINATOR_CONTEXT_CAP_OVERRIDE_PREFIX: &str = "coordinator_context_cap.";
/// #1050: agent-loop iterations one Harness turn may use before the #1039
/// wrap-up. A normal Issue (read, edit, build, test, commit, report) takes
/// 50-150 tool calls, so the default lets it finish in one turn; context and
/// cost stay bounded by compaction and the spend controls.
pub const HARNESS_MAX_ITERATIONS_PER_TURN_DEFAULT: u32 = 150;
pub const HARNESS_MAX_ITERATIONS_PER_TURN_MIN: u32 = 10;
pub const HARNESS_MAX_ITERATIONS_PER_TURN_MAX: u32 = 1000;
pub const MCP_DEFERRED_TOOL_THRESHOLD_DEFAULT: usize = 32;

/// Runtime config fields whose user-initiated `UpdateDaemonConfig` mutations
/// are durable daemon settings. Values are stored in SQLite's existing
/// `daemon_settings` key-value table and re-applied during daemon boot before
/// startup services are initialized.
pub const PERSISTED_RUNTIME_CONFIG_FIELDS: &[&str] = &[
    "session_retention_enabled",
    "session_retention_window_hours",
    "completed_transcript_cache_max_bytes",
    "retry_enabled",
    "satellite_polling_enabled",
    "retry_max_default",
    "retry_on_stall",
    "retry_max_backoff_ms",
    "reconciliation_enabled",
    "stall_detection_enabled",
    "context_rotation_enabled",
    "context_rotation_global_pct",
    "context_rotation_claude_pct",
    "context_rotation_codex_pct",
    "memory_enabled",
    "codegraph_indexing_enabled",
    "queue_enabled",
    "dream_enabled",
    "dialectic_enabled",
    "title_model_local",
    "title_model_provider",
    "title_model_base_url",
    "title_model_fallback",
    "memory_model_local",
    "memory_model_fallback",
    "memory_model_fallback_provider",
    "memory_model_fallback_base_url",
    "dream_model",
    "dream_model_provider",
    "dream_model_base_url",
    "dream_observation_threshold",
    "dream_idle_secs",
    "dream_cooldown_secs",
    "prompt_compile_model_local",
    "prompt_compile_model_provider",
    "prompt_compile_model_base_url",
    "codex_sandbox_mode",
    "claude_config_isolation",
    "system_prompt_preset",
    "stall_classifier_enabled",
    "stall_classifier_model",
    "stall_classifier_idle_secs",
    "stall_classifier_idle_secs_codex",
    "stall_classifier_cooldown_secs",
    "stall_classifier_max_per_session",
    "stall_classifier_confidence_floor",
    "recursive_dag_recovery_controls_enabled",
    "recursive_dag_scheduler_controls_enabled",
    "recursive_dag_cancellation_controls_enabled",
    "recursive_dag_live_scheduler_control_enabled",
    "recursive_dag_run_lease_ttl_ms",
    "recursive_dag_max_concurrent_graphs",
    "gv_render_recursive_origin",
    "gv_info_dashboard",
    // #634 kill switch for the durable topology executor.
    "topology_executor_enabled",
    "topology_max_concurrent_build_nodes",
    // #633: bulk fan-out rule for agent-authored topologies (plan §5.3).
    "topology_bulk_fanout_min_openrouter",
    // Issue #35: the operator surface for issue #34's orchestration child-effort
    // ceiling. The field name is deliberately identical to
    // `store::model_control::KEY_ORCHESTRATION_MAX_CHILD_EFFORT`, because the
    // generic write-through path persists a field under its own name and the
    // admission read path looks the key up by that exact string.
    // `orchestration_max_child_effort_field_name_matches_store_key` locks the two together.
    "orchestration_max_child_effort",
    // Issue #692: operator launch-model allowlist, enforced at the launch funnel.
    "launch_model_allowlist",
    "sandbox_build_cache_reclaim_enabled",
    "sandbox_build_cache_reclaim_ttl_secs",
    "sandbox_build_cache_reclaim_interval_secs",
    "sandbox_build_cache_reclaim_high_watermark_pct",
    "sandbox_build_cache_reclaim_low_watermark_pct",
    "sandbox_build_cache_reclaim_max_candidates",
    "sandbox_max_source_roots",
    "sandbox_min_free_gib",
    "archived_sandbox_purge_enabled",
    // Issue #1007: the daemon-owned rolling merge queue.
    "rolling_queue_enabled",
    // Issue #1073: hold new work while a deploy waits for its quiet point.
    "deploy_drain_enabled",
    "rolling_queue_batch_size",
    "rolling_queue_speculation_depth",
    // Issue #794 S3: child-aware continuation policy.
    "program_hold_while_children_run",
    "child_keepalive_enabled",
    "child_keepalive_window_secs",
    // Issue #1014: resource governor policy (defaults equal the retired cargo-slot).
    "governor_build_slots",
    "governor_lander_slots",
    "governor_max_load",
    "governor_min_free_disk_gb",
    "governor_min_avail_mem_gb",
    "governor_max_workers_slice_gb",
    // Issue #792: daemon-wide Harness tool policy defaults. A session's own
    // policy fills what it leaves unset from these.
    "harness_web_access",
    // Issue #774: `deny_private | offline`.
    "harness_egress_mode",
    // Issue #1111: default for Harness context editing (on); a session's own
    // `tool_policy.context_editing` overrides it.
    "harness_context_editing",
    "harness_max_search_calls",
    "harness_max_fetch_calls",
    "harness_max_result_bytes",
    "harness_max_web_cost_usd_micros",
    "completion_gates_enabled",
    "mcp.deferred_tool_threshold",
    "cloud_spend_stop_line_usd",
    "cloud_spend_daily_cap_usd",
    "agent_build_jobs",
    "agent_build_line_tables_only",
    "agent_build_sccache_enabled",
    "agent_build_sccache_cache_gib",
    "agent_build_slots",
    "rsid_scope_memory_high_mib",
    "rsid_scope_memory_max_mib",
    "rsid_scope_memory_swap_max_mib",
    "rsid_scope_cpu_weight",
    "worker_scope_memory_high_mib",
    "worker_scope_memory_max_mib",
    "worker_scope_memory_swap_max_mib",
    "worker_scope_cpu_weight",
    "vault.env_compat",
    "vault.check_ttl_secs",
    "api_route.openrouter",
    "api_route.bedrock",
    "api_route.fallback",
    "openrouter_context_budget_tokens",
    "harness_max_iterations_per_turn",
    "coordinator_context_cap_tokens",
];

pub fn is_persisted_runtime_config_field(field: &str) -> bool {
    PERSISTED_RUNTIME_CONFIG_FIELDS.contains(&field)
        || openrouter_model_route_key(field).is_some()
        || bedrock_model_route_key(field).is_some()
        || coordinator_context_cap_override_key(field).is_some()
}

/// #1005: the override key of a `coordinator_context_cap.<key>` field. The
/// key is `<Provider>` or `<Provider>/<model>`, the provider spelled as its
/// serde name (`Claude`, `Codex`, ...).
pub fn coordinator_context_cap_override_key(field: &str) -> Option<&str> {
    let key = field.strip_prefix(COORDINATOR_CONTEXT_CAP_OVERRIDE_PREFIX)?;
    let provider = key.split_once('/').map_or(key, |(provider, _)| provider);
    let model_ok = key
        .split_once('/')
        .is_none_or(|(_, model)| !model.is_empty());
    (model_ok
        && !key.chars().any(char::is_whitespace)
        && serde_json::from_value::<SessionProvider>(serde_json::Value::String(provider.into()))
            .is_ok())
    .then_some(key)
}

fn provider_key(provider: SessionProvider) -> String {
    serde_json::to_value(provider)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{provider:?}"))
}

fn bounded_cap_tokens(value: &serde_json::Value) -> Result<u64, String> {
    let range = COORDINATOR_CONTEXT_CAP_TOKENS_MIN..=COORDINATOR_CONTEXT_CAP_TOKENS_MAX;
    value
        .as_u64()
        .filter(|tokens| *tokens == 0 || range.contains(tokens))
        .ok_or_else(|| {
            format!(
                "expected 0 (off) or an integer in {}..={}",
                range.start(),
                range.end()
            )
        })
}

fn openrouter_model_route_key(field: &str) -> Option<&str> {
    let model = field.strip_prefix("api_route.openrouter.")?;
    (!model.is_empty() && !model.chars().any(char::is_whitespace)).then_some(model)
}

fn bedrock_model_route_key(field: &str) -> Option<&str> {
    let model = field.strip_prefix("api_route.bedrock.")?;
    (!model.is_empty() && !model.chars().any(char::is_whitespace)).then_some(model)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenRouterRoute {
    CodexCli,
    Harness,
}

impl OpenRouterRoute {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CodexCli => "codex_cli",
            Self::Harness => "harness",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "codex_cli" => Some(Self::CodexCli),
            "harness" => Some(Self::Harness),
            _ => None,
        }
    }
}

/// Normalize a `system_prompt_preset` value: accepts canonical slugs,
/// case-insensitive label aliases (`"Default"`, `"Code Only"`, `"CodeOnly"`,
/// ...), and underscore aliases (`"code_only"`). Returns the canonical
/// kebab-case slug or `None` if unrecognized.
///
/// Used by `RuntimeConfig::update_field` for input validation on the
/// `UpdateDaemonConfig` RPC and by
/// `crate::store::daemon_settings::maybe_import_legacy_system_prompt_preset`
/// for legacy `state.json` import.
pub fn normalize_system_prompt_preset(raw: &str) -> Option<&'static str> {
    let lower = raw.to_ascii_lowercase();
    // Normalize underscores AND spaces to hyphens so both "code_only" and
    // "code only" map to "code-only". Also handle the camelCase legacy
    // label "codeonly" (collapsed lowercase of TUI enum variant "CodeOnly").
    let normalized = lower.replace('_', "-").replace(' ', "-");
    match normalized.as_str() {
        "default" => Some("default"),
        "concise" => Some("concise"),
        "code-only" | "codeonly" => Some("code-only"),
        "caveman" => Some("caveman"),
        _ => None,
    }
}

fn optional_string(value: &serde_json::Value) -> Result<Option<String>, String> {
    if value.is_null() {
        Ok(None)
    } else {
        Ok(Some(
            value.as_str().ok_or("expected string or null")?.to_string(),
        ))
    }
}

fn bounded_u64(value: &serde_json::Value, min: u64, max: u64) -> Result<u64, String> {
    let value = value.as_u64().ok_or("expected unsigned integer")?;
    if !(min..=max).contains(&value) {
        return Err(format!("expected integer in {min}..={max}"));
    }
    Ok(value)
}

fn update_rotation_pct(field: &AtomicU8, value: &serde_json::Value) -> Result<(), String> {
    let pct = if value.is_null() {
        0
    } else {
        bounded_u64(value, 1, 99)? as u8
    };
    field.store(pct, Ordering::Relaxed);
    Ok(())
}

fn override_json(field: &AtomicU8) -> serde_json::Value {
    match field.load(Ordering::Relaxed) {
        0 => serde_json::Value::Null,
        pct => serde_json::json!(pct),
    }
}

fn parse_recursive_dag_bool_env(var_name: &str, raw: String) -> bool {
    parse_bool_env(var_name, raw)
}

fn parse_bool_env(var_name: &str, raw: String) -> bool {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => panic!("{var_name} must be a boolean (true/false/1/0/yes/no/on/off), got {raw:?}"),
    }
}

fn parse_positive_recursive_dag_u64_env(var_name: &str, raw: String) -> u64 {
    let value = raw
        .trim()
        .parse::<u64>()
        .unwrap_or_else(|_| panic!("{var_name} must be an unsigned integer, got {raw:?}"));
    if value == 0 {
        panic!("{var_name} must be positive");
    }
    value
}

fn parse_positive_recursive_dag_u32_env(var_name: &str, raw: String) -> u32 {
    let value = raw
        .trim()
        .parse::<u32>()
        .unwrap_or_else(|_| panic!("{var_name} must be an unsigned integer, got {raw:?}"));
    if value == 0 {
        panic!("{var_name} must be positive");
    }
    value
}

#[derive(Debug, Clone)]
pub struct Config {
    pub socket_path: PathBuf,
    pub event_buffer_size: usize,
    pub context_rotation_enabled: bool,
    pub memory_enabled: bool,
    pub memory_dir: PathBuf,
    pub memory_embedding_model: String,
    pub memory_embedding_url: Option<String>,
    pub memory_embedding_api_key: Option<String>,
    /// Stall detection threshold for Running sessions (seconds). Default: 1800 (30 min).
    pub stall_timeout_running_secs: u64,
    /// Stall detection threshold for WaitingApproval sessions (seconds). Default: 3600 (60 min).
    pub stall_timeout_waiting_secs: u64,
    /// Whether stall detection is enabled. Default: true.
    pub stall_detection_enabled: bool,
    /// Model used for workflow generation (build phase). None = provider default.
    pub build_model: Option<String>,
    /// Model used for workflow execution (invoke phase). None = provider default.
    pub invoke_model: Option<String>,
    /// Whether the background queue is enabled. Default: true.
    pub queue_enabled: bool,
    /// Queue polling interval in seconds. Default: 30.
    pub queue_poll_interval_secs: u64,
    /// Token threshold for batch processing. Default: 1024.
    pub queue_token_threshold: i64,
    /// Optional workspace root boundaries. When non-empty, all session working_dir values
    /// must be descendants of at least one root. Parsed from RSI_WORKSPACE_ROOTS
    /// (comma-separated paths). Each root is canonicalized at daemon startup.
    /// Example: RSI_WORKSPACE_ROOTS=/home/user/work,/home/user/personal
    pub workspace_roots: Vec<PathBuf>,
    /// Whether the reconciliation loop is enabled. Default: true.
    pub reconciliation_enabled: bool,
    /// Reconciliation liveness check interval (seconds). Default: 120.
    pub reconciliation_liveness_interval_secs: u64,
    /// Reconciliation SQLite consistency check interval (seconds). Default: 600.
    pub reconciliation_consistency_interval_secs: u64,
    /// Stall action for standard sessions ("notify" | "interrupt" | "interrupt_and_retry").
    pub reconciliation_stall_action_standard: String,
    /// Stall action for unattended sessions ("notify" | "interrupt" | "interrupt_and_retry").
    pub reconciliation_stall_action_unattended: String,
    /// Global default max retries for new sessions. Default: 3.
    /// Overridden by per-session LaunchSessionParams.max_retries.
    pub retry_max_default: u8,
    /// Maximum backoff delay in milliseconds. Default: 120_000 (2 minutes).
    pub retry_max_backoff_ms: u64,
    /// Whether stall detection triggers automatic retry. Default: true.
    pub retry_on_stall: bool,
    /// Boot-only smoke guard: suppress retry relaunch requeue during copied-DB smoke checks.
    /// Default false so production restart retry behavior is unchanged.
    pub smoke_suppress_retry_restore: bool,
    /// Whether the dream consolidation system is enabled. Default: false.
    pub dream_enabled: bool,
    /// Observation count threshold to trigger a dream cycle. Default: 50.
    pub dream_observation_threshold: u64,
    /// Idle time (seconds) before dream can trigger. Default: 3600 (60 min).
    pub dream_idle_secs: u64,
    /// Cooldown between dream cycles (seconds). Default: 28800 (8 hours).
    pub dream_cooldown_secs: u64,
    /// Model for dreamer LLM calls. Default: "claude-sonnet-5".
    pub dream_model: Option<String>,
    /// API URL for dreamer LLM calls. Default: Anthropic API.
    pub dream_api_url: Option<String>,
    /// API key for dreamer LLM calls. Default: ANTHROPIC_API_KEY env var.
    pub dream_api_key: Option<String>,
    /// Max sessions to process per dream cycle. Default: 20.
    pub dream_batch_size: usize,
    /// Whether the dialectic query engine is enabled. Default: true.
    pub dialectic_enabled: bool,
    /// OpenAI-compatible API URL for the dialectic agent LLM.
    /// Default: http://localhost:11434/v1 (Ollama).
    pub dialectic_api_url: String,
    /// API key for the dialectic agent (optional for local models).
    pub dialectic_api_key: Option<String>,
    /// Model name for the dialectic agent. Default: qwen2.5:14b.
    pub dialectic_model: String,
    /// Maximum tool iterations per dialectic query. Default: 8.
    pub dialectic_max_iterations: u32,
    /// Whether the scheduled jobs scheduler is enabled. Default: true.
    pub scheduler_enabled: bool,
    /// Scheduler poll interval in seconds. Default: 60.
    pub scheduler_poll_interval_secs: u64,
    /// Root directory for per-session sandbox allocations. Default:
    /// `~/.rsi/sandboxes`. Parsed from `RSI_SANDBOX_BASE`.
    pub sandbox_base: PathBuf,
    /// Codex `--sandbox <mode>` policy for fresh `exec` launches.
    /// Stored as the validated cli-arg string ("read-only" | "workspace-write" |
    /// "danger-full-access"). Parsed via `CodexSandboxMode::parse()` at startup.
    /// Runtime-mutable via `UpdateDaemonConfig` RPC.
    pub codex_sandbox_mode: String,
    /// Claude `-p` untrusted-config isolation policy (string form:
    /// "off" | "settings" | "strict"). Validated on write via
    /// `ClaudeConfigIsolation::parse()`. Defaults to "off", which reproduces
    /// the pre-feature launch argv exactly.
    pub claude_config_isolation: String,
    /// Stall classifier (RSI-0XX). Opt-in. When `false`, the daemon behaves
    /// exactly as before the classifier shipped; the static `stall_detector`
    /// action selector is the sole path. Runtime-mutable.
    pub stall_classifier_enabled: bool,
    /// Startup recursive DAG recovery graph-count budget. Phase 5A.4 keeps
    /// restart recovery bounded and explicitly continuable.
    pub recursive_dag_startup_recovery_max_graphs: u32,
    /// Startup recursive DAG recovery wall-clock budget in milliseconds.
    pub recursive_dag_startup_recovery_time_budget_ms: u64,
    /// Enable explicit recursive DAG recovery control RPCs. Default: false.
    pub recursive_dag_recovery_controls_enabled: bool,
    /// Enable explicit fake-only recursive DAG scheduler control RPCs. Default: false.
    pub recursive_dag_scheduler_controls_enabled: bool,
    /// Enable explicit recursive DAG cancellation control RPCs. Default: false.
    pub recursive_dag_cancellation_controls_enabled: bool,
    /// Enable explicit recursive DAG live scheduler control RPC. Default: false.
    pub recursive_dag_live_scheduler_control_enabled: bool,
    /// Enable gated read-only rendering of a recursive task graph (bridged to a
    /// workflow) inside the `gv` graph overlay. Default: false.
    pub gv_render_recursive_origin: bool,
    /// Enable gated rendering of the info dashboard column on the right side of the
    /// Sugiyama canvas in `gv` overlay. Default: false.
    pub gv_info_dashboard: bool,
    /// Durable topology executor kill switch (#634). Default: true. When off,
    /// no durable execution advances or recovers and live topology runs use
    /// the legacy in-memory runner.
    pub topology_executor_enabled: bool,
    /// Global cap for concurrently running topology command nodes.
    pub topology_max_concurrent_build_nodes: u32,
    /// Agent-authored topologies (#633): a layer with at least this many
    /// same-kind session nodes must run on `OpenRouter`. `0` disables the rule.
    pub topology_bulk_fanout_min_openrouter: u32,
    /// Recursive DAG scheduler lease TTL for explicit fake scheduler RPCs.
    pub recursive_dag_run_lease_ttl_ms: u64,
    /// Global active recursive DAG scheduler run cap for explicit fake scheduler RPCs.
    pub recursive_dag_max_concurrent_graphs: u32,
    /// OpenAI-compatible model name for the stall classifier. Default
    /// `qwen2.5:7b` (Ollama). Runtime-mutable.
    pub stall_classifier_model: String,
    /// OpenAI-compatible API URL for the stall classifier. Default points at
    /// a local Ollama instance.
    pub stall_classifier_api_url: String,
    /// Optional API key for the stall classifier endpoint. `None` falls back
    /// to `ANTHROPIC_API_KEY` for Anthropic-routed deployments.
    pub stall_classifier_api_key: Option<String>,
    /// Per-provider idle threshold (seconds) before the classifier is
    /// signaled for Claude / Local / Antigravity sessions. Default 600.
    /// Runtime-mutable. MUST stay shorter than `stall_timeout_running_secs`
    /// so the classifier fires before the static stall path.
    pub stall_classifier_idle_secs: u64,
    /// Codex-specific idle threshold (seconds). Default 1800 (3x Claude).
    /// Codex `item.started` → `item.completed` can span minutes during a
    /// long shell command, so the threshold must be larger to avoid
    /// false-positives on healthy sessions. Runtime-mutable.
    pub stall_classifier_idle_secs_codex: u64,
    /// Minimum gap between two classifications of the same session
    /// (seconds). Default 1800 (30 min). Runtime-mutable.
    pub stall_classifier_cooldown_secs: u64,
    /// Lifetime cap on the number of classifier verdicts per session.
    /// Default 3. Runtime-mutable.
    pub stall_classifier_max_per_session: u32,
    /// Confidence floor (0.0..1.0). `StalledContinue` / `StalledCheckTeam`
    /// verdicts below this floor are downgraded to telemetry-only.
    /// Default 0.7. Runtime-mutable.
    pub stall_classifier_confidence_floor: f64,
    /// HTTP timeout for a single classifier LLM call (seconds). Default 30.
    /// Boot-time only — applied when constructing the LLM client.
    pub stall_classifier_timeout_secs: u64,
}

/// Runtime-mutable daemon configuration. Uses atomic types for lock-free reads.
/// Initialized from Config at startup, can be updated via UpdateDaemonConfig RPC.
#[derive(Debug)]
pub struct RuntimeConfig {
    pub session_retention_enabled: AtomicBool,
    pub session_retention_window_hours: AtomicU64,
    pub completed_transcript_cache_max_bytes: AtomicU64,
    pub retry_enabled: AtomicBool,
    /// Global kill switch for hub satellite polling. Default on.
    pub satellite_polling_enabled: AtomicBool,
    pub retry_max_default: AtomicU8,
    pub retry_on_stall: AtomicBool,
    pub smoke_suppress_retry_restore: AtomicBool,
    pub retry_max_backoff_ms: AtomicU64,
    pub reconciliation_enabled: AtomicBool,
    pub stall_detection_enabled: AtomicBool,
    pub context_rotation_enabled: AtomicBool,
    /// Zero means no override; valid configured percentages are 1..=99.
    pub context_rotation_global_pct: AtomicU8,
    pub context_rotation_claude_pct: AtomicU8,
    pub context_rotation_codex_pct: AtomicU8,
    pub memory_enabled: AtomicBool,
    /// Operator-owned Codegraph S3 indexing gate. The index manager clones this
    /// flag and observes live RPC updates through the same atomic value.
    pub codegraph_indexing_enabled: Arc<AtomicBool>,
    pub queue_enabled: AtomicBool,
    pub dream_enabled: AtomicBool,
    pub dialectic_enabled: AtomicBool,
    /// Local Ollama model for title/description generation.
    pub title_model_local: RwLock<String>,
    pub title_model_provider: RwLock<SessionProvider>,
    pub title_model_base_url: RwLock<Option<String>>,
    pub title_model_api_key: RwLock<Option<String>>,
    /// Fallback CLI model for title/description generation.
    pub title_model_fallback: RwLock<String>,
    /// Local Ollama model for memory (observation extraction + summarization).
    pub memory_model_local: RwLock<String>,
    /// Fallback CLI model for memory (observation extraction + summarization).
    pub memory_model_fallback: RwLock<String>,
    pub memory_model_fallback_provider: RwLock<SessionProvider>,
    pub memory_model_fallback_base_url: RwLock<Option<String>>,
    pub memory_model_fallback_api_key: RwLock<Option<String>>,
    /// Model for dream consolidation (deduction + induction).
    pub dream_model: RwLock<String>,
    pub dream_model_provider: RwLock<SessionProvider>,
    pub dream_model_base_url: RwLock<Option<String>>,
    pub dream_model_api_key: RwLock<Option<String>>,
    /// Observation count threshold to trigger dreaming.
    pub dream_observation_threshold: AtomicU64,
    /// Required daemon idle duration before Dream may auto-run.
    pub dream_idle_secs: AtomicU64,
    /// Cooldown between dream cycles (seconds).
    pub dream_cooldown_secs: AtomicU64,
    /// Local Ollama model for Ctrl+P prompt compilation.
    pub prompt_compile_model_local: RwLock<String>,
    pub prompt_compile_model_provider: RwLock<SessionProvider>,
    pub prompt_compile_model_base_url: RwLock<Option<String>>,
    pub prompt_compile_model_api_key: RwLock<Option<String>>,
    /// Codex `--sandbox` policy for fresh `exec` launches (string form:
    /// "read-only" | "workspace-write" | "danger-full-access"). Validated on
    /// write via `CodexSandboxMode::parse()`. Read on every fresh Codex spawn
    /// so RPC mutations take effect without daemon restart.
    pub codex_sandbox_mode: RwLock<String>,
    /// SECURITY (operator-only). Claude `-p` untrusted-config isolation
    /// policy: `"off"` (default) | `"settings"` | `"strict"`. Validated on
    /// write via `ClaudeConfigIsolation::parse()`. Read on every Claude spawn
    /// (`ClaudeClient::config_isolation`) so an RPC mutation takes effect
    /// without a daemon restart.
    ///
    /// `"off"` emits no CLI flags, so the default launch argv is unchanged.
    ///
    /// Operator-only by construction: reachable only through
    /// `GetDaemonConfig`/`UpdateDaemonConfig`, which are absent from
    /// `AGENT_VERBS`/`READ_VERBS`, so a session-attributed caller is refused
    /// by the pre-dispatch gate and cannot lower its own isolation.
    pub claude_config_isolation: RwLock<String>,
    /// System-prompt preset value (RSI-026). Stored as the canonical
    /// kebab-case slug: `"default" | "concise" | "code-only" | "caveman"`.
    /// Authoritative source. Validated on write via
    /// `normalize_system_prompt_preset`. Read by `GetDaemonConfig`; the TUI
    /// caches the value and resolves slug → string content per launch.
    pub system_prompt_preset: RwLock<String>,
    /// Operator ceiling on the effort an orchestration child may request
    /// (issues #34/#35). Stored as a canonical value from
    /// `ORCHESTRATION_MAX_CHILD_EFFORT_CHOICES`: `"unset"` (the default — fall
    /// back to the tree root's own effort) or one of
    /// `low | medium | high | xhigh | max | ultra`. Validated on write via
    /// `normalize_orchestration_max_child_effort`.
    ///
    /// This lock is the OPERATOR VIEW, not the authority. The authority is the
    /// `daemon_settings` row, which `enforce_orchestration_tier_escalation`
    /// reads inside the admission transaction so a change takes effect without
    /// a daemon restart. The two are kept in step by the generic write-through
    /// on update and by `apply_persisted_runtime_config` at boot.
    ///
    /// Operator-only by construction: reachable only through
    /// `GetDaemonConfig`/`UpdateDaemonConfig`, which are absent from
    /// `AGENT_VERBS`/`READ_VERBS`, so a session-attributed caller is refused by
    /// the pre-dispatch gate and cannot raise its own ceiling.
    pub orchestration_max_child_effort: RwLock<String>,
    /// Issue #692: operator launch-model allowlist (model ids). Empty means
    /// unrestricted. Every launch's EFFECTIVE model (request, else project
    /// default, else provider default) is canonicalised (the Claude `[1m]`
    /// variant tag and `openrouter/`/`bedrock/` routing prefix dropped) and must equal
    /// an entry exactly, case-insensitively; a launch with no determinable
    /// model is refused while the list is set. Checked before any side effect
    /// at the launch chokepoint, preflighted for continuations and rotations,
    /// and pre-checked on `AgentSpawnChild`; a change applies to the next
    /// launch without a restart. Validated on write via
    /// `rsi_common::launch_allowlist::normalize_launch_model_allowlist`.
    /// Operator-only: reachable only through `GetDaemonConfig`/`UpdateDaemonConfig`.
    pub launch_model_allowlist: RwLock<Vec<String>>,
    /// #694 K1: operator-only key-vault settings (`vault.env_compat`,
    /// `vault.check_ttl_secs`), shared with the daemon's `VaultHandle` so an
    /// `UpdateDaemonConfig` change applies to the next resolution/check.
    pub vault_settings: Arc<crate::vault::VaultSettings>,
    pub openrouter_route: RwLock<OpenRouterRoute>,
    pub openrouter_model_routes: RwLock<std::collections::HashMap<String, OpenRouterRoute>>,
    pub bedrock_route: RwLock<OpenRouterRoute>,
    pub bedrock_model_routes: RwLock<std::collections::HashMap<String, OpenRouterRoute>>,
    pub api_route_fallback: AtomicBool,
    /// #966: absolute live-context budget for OpenRouter sessions (0 = off).
    /// Read at each launch and each Harness compaction check.
    pub openrouter_context_budget_tokens: AtomicU64,
    /// #1005: hard live-context cap for coordinating seats (0 = off).
    pub coordinator_context_cap_tokens: AtomicU64,
    /// #1005: per-provider/per-model cap overrides (0 = off for that key).
    pub coordinator_context_cap_overrides: RwLock<std::collections::HashMap<String, u64>>,
    /// #1050: operator cap on agent-loop iterations per Harness turn. Read
    /// when a turn starts, so a change applies to the next turn.
    pub harness_max_iterations_per_turn: AtomicU32,
    /// Issue #69: operator-owned target-cache lifecycle controls. These
    /// settings never grant worktree or branch deletion authority.
    sandbox_build_cache_reclaim: RwLock<SandboxBuildCacheReclaimConfig>,
    /// Admission limits for new Git worktree roots. Only operator config RPC
    /// may change these; allocation reads them while holding its permit.
    pub sandbox_max_source_roots: AtomicU32,
    pub sandbox_min_free_gib: AtomicU64,
    pub archived_sandbox_purge_enabled: AtomicBool,
    /// Issue #1007: rolling merge queue switch (default off), batch size
    /// (1..=8; slice S1 always runs one source per gate) and speculation
    /// depth (0..=2). Operator-only: reachable only through
    /// `GetDaemonConfig`/`UpdateDaemonConfig`.
    pub rolling_queue_enabled: AtomicBool,
    /// Issue #1073: while an agent-requested deploy waits for its quiet point
    /// (default on), the daemon holds new child launches, child continuations,
    /// scheduled child wakes and new agent jobs so a busy hub can go quiet.
    /// Operator-only via `GetDaemonConfig`/`UpdateDaemonConfig`.
    pub deploy_drain_enabled: AtomicBool,
    pub rolling_queue_batch_size: AtomicU32,
    /// Issue #1014: resource governor policy. `governor_max_load` 0 means 1.25 x cores.
    pub governor_build_slots: AtomicU32,
    pub governor_lander_slots: AtomicU32,
    pub governor_max_load: AtomicU32,
    pub governor_min_free_disk_gb: AtomicU64,
    pub governor_min_avail_mem_gb: AtomicU64,
    pub governor_max_workers_slice_gb: AtomicU64,
    pub rolling_queue_speculation_depth: AtomicU32,
    /// Issue #794 S3: hold a program-mode master's due Resume wake while its
    /// children run (default on) and, when `child_keepalive_enabled` (default
    /// off), give an idle parent one bounded keep-alive Resume per
    /// `child_keepalive_window_secs` window. Operator-only.
    pub program_hold_while_children_run: AtomicBool,
    pub child_keepalive_enabled: AtomicBool,
    pub child_keepalive_window_secs: AtomicU64,
    /// Issue #792: daemon-wide Harness tool policy defaults. `harness_web_access`
    /// is `enabled | hosted_only | disabled`; each cap is 0 for unlimited. A
    /// per-session `tool_policy` overrides a default it sets. Operator-only:
    /// reachable through `GetDaemonConfig`/`UpdateDaemonConfig`.
    pub harness_web_access: RwLock<String>,
    /// Issue #774: default network egress mode for Harness network tools and
    /// the shell: `deny_private` (public addresses only) or `offline`.
    pub harness_egress_mode: RwLock<String>,
    /// Issue #1111: daemon default for Harness context editing.
    pub harness_context_editing: AtomicBool,
    pub harness_max_search_calls: AtomicU64,
    pub harness_max_fetch_calls: AtomicU64,
    pub harness_max_result_bytes: AtomicU64,
    pub harness_max_web_cost_usd_micros: AtomicU64,
    /// #794: operator kill switch for session completion gates. A configured
    /// gate still emits a visible disabled event when a Harness session ends.
    pub completion_gates_enabled: AtomicBool,
    /// Above this many permitted MCP tools, only `tool_search` is advertised.
    /// Zero defers every tool; 256 is the current session tool cap.
    pub mcp_deferred_tool_threshold: AtomicUsize,
    /// #1036: remote-gate spend caps in whole USD. The daemon mirrors them
    /// into `spend-caps.json` beside the ledger for `scripts/cloud-spend.py`.
    pub cloud_spend_stop_line_usd: AtomicU64,
    pub cloud_spend_daily_cap_usd: AtomicU64,
    /// Launch defaults for sandboxed agent builds. Read at each process spawn.
    pub agent_build_jobs: AtomicU32,
    pub agent_build_line_tables_only: AtomicBool,
    pub agent_build_sccache_enabled: AtomicBool,
    pub agent_build_sccache_cache_gib: AtomicU32,
    pub agent_build_slots: AtomicU32,
    /// Configured limits for the systemd user scope that launches rsid.
    pub rsid_scope_memory_high_mib: AtomicU64,
    pub rsid_scope_memory_max_mib: AtomicU64,
    pub rsid_scope_memory_swap_max_mib: AtomicU64,
    pub rsid_scope_cpu_weight: AtomicU32,
    /// Aggregate limits for the worker slice; never substituted for rsid_scope_*.
    pub worker_scope_memory_high_mib: AtomicU64,
    pub worker_scope_memory_max_mib: AtomicU64,
    pub worker_scope_memory_swap_max_mib: AtomicU64,
    pub worker_scope_cpu_weight: AtomicU32,
    /// Transient high/low hysteresis state. This is telemetry, not a persisted
    /// operator setting, and resets conservatively on daemon restart.
    pub sandbox_build_cache_pressure_active: AtomicBool,
    /// Stall classifier opt-in toggle. Runtime-mutable via
    /// `UpdateDaemonConfig`. When false, the classifier task drops every
    /// signal it receives without invoking the LLM.
    pub stall_classifier_enabled: AtomicBool,
    /// Model name for the stall classifier LLM. The classifier client is built
    /// at daemon boot, so a changed model takes effect after a daemon restart.
    pub stall_classifier_model: RwLock<String>,
    /// Per-provider idle threshold (seconds) for non-Codex providers.
    pub stall_classifier_idle_secs: AtomicU64,
    /// Per-provider idle threshold (seconds) for Codex sessions.
    pub stall_classifier_idle_secs_codex: AtomicU64,
    /// Minimum gap between two classifications of the same session.
    pub stall_classifier_cooldown_secs: AtomicU64,
    /// Lifetime cap on classifier verdicts per session.
    pub stall_classifier_max_per_session: AtomicU32,
    /// Confidence floor (0.0..1.0) below which Stalled* verdicts downgrade
    /// to telemetry-only. RwLock instead of an atomic because `f64` has no
    /// `AtomicF64` in stable std; reads are cheap (uncontended).
    pub stall_classifier_confidence_floor: RwLock<f64>,
    /// Recovery control RPC gate.
    pub recursive_dag_recovery_controls_enabled: AtomicBool,
    /// Fake scheduler control RPC gate.
    pub recursive_dag_scheduler_controls_enabled: AtomicBool,
    /// Cancellation control RPC gate.
    pub recursive_dag_cancellation_controls_enabled: AtomicBool,
    /// Live scheduler control RPC gate. Default false; when true it enables
    /// only the explicit ordinary recursive DAG live scheduler RPC path.
    pub recursive_dag_live_scheduler_control_enabled: AtomicBool,
    /// Gated read-only recursive-graph rendering in `gv`. Default false; when
    /// true the `GetRecursiveGraphAsWorkflow` RPC and the gv "Recursive graphs"
    /// picker source become reachable.
    pub gv_render_recursive_origin: AtomicBool,
    /// Gated rendering of the info dashboard column on the right side of the
    /// Sugiyama canvas in `gv` overlay. Default: false.
    pub gv_info_dashboard: AtomicBool,
    /// Durable topology executor kill switch (#634). Default: true.
    pub topology_executor_enabled: AtomicBool,
    pub topology_max_concurrent_build_nodes: AtomicU32,
    /// #633 bulk fan-out rule threshold (0 disables).
    pub topology_bulk_fanout_min_openrouter: AtomicU32,
    /// Recursive DAG scheduler lease TTL for explicit fake scheduler RPCs.
    pub recursive_dag_run_lease_ttl_ms: AtomicU64,
    /// Global active recursive DAG scheduler run cap for explicit fake scheduler RPCs.
    pub recursive_dag_max_concurrent_graphs: AtomicU32,
}

impl RuntimeConfig {
    /// CLI-backed rotation policy. Other provider families retain 65%.
    pub fn context_rotation_threshold_pct(&self, provider: SessionProvider) -> f64 {
        let family = match provider {
            SessionProvider::Claude => Some(&self.context_rotation_claude_pct),
            SessionProvider::Codex | SessionProvider::Pioneer | SessionProvider::CodexAppServer => {
                Some(&self.context_rotation_codex_pct)
            }
            _ => None,
        };
        family
            .map(|specific| {
                let global = self.context_rotation_global_pct.load(Ordering::Relaxed);
                (global != 0)
                    .then_some(global)
                    .or_else(|| {
                        let pct = specific.load(Ordering::Relaxed);
                        (pct != 0).then_some(pct)
                    })
                    .unwrap_or(65) as f64
            })
            .unwrap_or(65.0)
    }
    pub fn from_config(config: &Config) -> Arc<Self> {
        Self::from_config_with_system_prompt_preset(config, "default".to_string())
    }

    /// Variant of `from_config` that accepts a pre-resolved seed for
    /// `system_prompt_preset` — used by the daemon boot path so the
    /// SQLite-backed value (loaded via
    /// `crate::store::daemon_settings::maybe_import_legacy_system_prompt_preset`)
    /// is the canonical initial value (RSI-026).
    ///
    /// Test code and other call sites that don't care about the preset can
    /// continue calling `from_config(&config)` which seeds `"default"`.
    pub fn from_config_with_system_prompt_preset(
        config: &Config,
        system_prompt_preset_seed: String,
    ) -> Arc<Self> {
        let (worker_memory_high_mib, worker_memory_max_mib) =
            rsi_common::worker_memory::default_limits_mib();
        Arc::new(Self {
            session_retention_enabled: AtomicBool::new(true),
            session_retention_window_hours: AtomicU64::new(SESSION_RETENTION_WINDOW_HOURS_DEFAULT),
            completed_transcript_cache_max_bytes: AtomicU64::new(
                COMPLETED_TRANSCRIPT_CACHE_MAX_BYTES_DEFAULT,
            ),
            retry_enabled: AtomicBool::new(config.retry_max_default > 0),
            satellite_polling_enabled: AtomicBool::new(true),
            retry_max_default: AtomicU8::new(config.retry_max_default),
            retry_on_stall: AtomicBool::new(config.retry_on_stall),
            smoke_suppress_retry_restore: AtomicBool::new(config.smoke_suppress_retry_restore),
            retry_max_backoff_ms: AtomicU64::new(config.retry_max_backoff_ms),
            reconciliation_enabled: AtomicBool::new(config.reconciliation_enabled),
            stall_detection_enabled: AtomicBool::new(config.stall_detection_enabled),
            context_rotation_enabled: AtomicBool::new(config.context_rotation_enabled),
            context_rotation_global_pct: AtomicU8::new(0),
            context_rotation_claude_pct: AtomicU8::new(0),
            context_rotation_codex_pct: AtomicU8::new(0),
            memory_enabled: AtomicBool::new(config.memory_enabled),
            codegraph_indexing_enabled: Arc::new(AtomicBool::new(false)),
            queue_enabled: AtomicBool::new(config.queue_enabled),
            dream_enabled: AtomicBool::new(config.dream_enabled),
            dialectic_enabled: AtomicBool::new(config.dialectic_enabled),
            title_model_local: RwLock::new("gemma4:e4b".to_string()),
            title_model_provider: RwLock::new(SessionProvider::Local),
            title_model_base_url: RwLock::new(None),
            title_model_api_key: RwLock::new(None),
            title_model_fallback: RwLock::new("haiku".to_string()),
            memory_model_local: RwLock::new("gemma4:e4b".to_string()),
            memory_model_fallback: RwLock::new("gemma4:e4b".to_string()),
            memory_model_fallback_provider: RwLock::new(SessionProvider::Local),
            memory_model_fallback_base_url: RwLock::new(None),
            memory_model_fallback_api_key: RwLock::new(None),
            dream_model: RwLock::new(
                config
                    .dream_model
                    .clone()
                    .unwrap_or_else(|| "claude-sonnet-5".to_string()),
            ),
            dream_model_provider: RwLock::new(SessionProvider::Claude),
            dream_model_base_url: RwLock::new(config.dream_api_url.clone()),
            dream_model_api_key: RwLock::new(config.dream_api_key.clone()),
            dream_observation_threshold: AtomicU64::new(config.dream_observation_threshold),
            dream_idle_secs: AtomicU64::new(config.dream_idle_secs),
            dream_cooldown_secs: AtomicU64::new(config.dream_cooldown_secs),
            prompt_compile_model_local: RwLock::new("gemma4:e4b".to_string()),
            prompt_compile_model_provider: RwLock::new(SessionProvider::Local),
            prompt_compile_model_base_url: RwLock::new(None),
            prompt_compile_model_api_key: RwLock::new(None),
            codex_sandbox_mode: RwLock::new(config.codex_sandbox_mode.clone()),
            claude_config_isolation: RwLock::new(config.claude_config_isolation.clone()),
            system_prompt_preset: RwLock::new(system_prompt_preset_seed),
            // Default to the sentinel, never to an effort name: an unset
            // ceiling is the pre-#34 behaviour (fall back to the tree root's
            // own effort). `apply_persisted_runtime_config` overlays the stored
            // value at boot if the operator has set one.
            orchestration_max_child_effort: RwLock::new(
                rsi_common::model_control::ORCHESTRATION_MAX_CHILD_EFFORT_UNSET.to_string(),
            ),
            launch_model_allowlist: RwLock::new(Vec::new()),
            vault_settings: Arc::new(crate::vault::VaultSettings::default()),
            openrouter_route: RwLock::new(OpenRouterRoute::CodexCli),
            openrouter_model_routes: RwLock::new(std::collections::HashMap::new()),
            bedrock_route: RwLock::new(OpenRouterRoute::CodexCli),
            bedrock_model_routes: RwLock::new(std::collections::HashMap::new()),
            api_route_fallback: AtomicBool::new(true),
            openrouter_context_budget_tokens: AtomicU64::new(
                OPENROUTER_CONTEXT_BUDGET_TOKENS_DEFAULT,
            ),
            coordinator_context_cap_tokens: AtomicU64::new(COORDINATOR_CONTEXT_CAP_TOKENS_DEFAULT),
            coordinator_context_cap_overrides: RwLock::new(std::collections::HashMap::new()),
            harness_max_iterations_per_turn: AtomicU32::new(
                HARNESS_MAX_ITERATIONS_PER_TURN_DEFAULT,
            ),
            sandbox_build_cache_reclaim: RwLock::new(SandboxBuildCacheReclaimConfig {
                enabled: true,
                ttl_secs: SANDBOX_BUILD_CACHE_RECLAIM_TTL_SECS_DEFAULT,
                interval_secs: SANDBOX_BUILD_CACHE_RECLAIM_INTERVAL_SECS_DEFAULT,
                high_watermark_pct: SANDBOX_BUILD_CACHE_RECLAIM_HIGH_WATERMARK_PCT_DEFAULT,
                low_watermark_pct: SANDBOX_BUILD_CACHE_RECLAIM_LOW_WATERMARK_PCT_DEFAULT,
                max_candidates: SANDBOX_BUILD_CACHE_RECLAIM_MAX_CANDIDATES_DEFAULT,
            }),
            sandbox_max_source_roots: AtomicU32::new(4096),
            // Match the interim cargo-slot host floor until the operator
            // chooses a different allocation threshold.
            sandbox_min_free_gib: AtomicU64::new(30),
            archived_sandbox_purge_enabled: AtomicBool::new(false),
            program_hold_while_children_run: AtomicBool::new(
                rsi_common::child_autonomy::PROGRAM_HOLD_DEFAULT,
            ),
            child_keepalive_enabled: AtomicBool::new(
                rsi_common::child_autonomy::KEEPALIVE_ENABLED_DEFAULT,
            ),
            child_keepalive_window_secs: AtomicU64::new(
                rsi_common::child_autonomy::KEEPALIVE_WINDOW_DEFAULT_SECS,
            ),
            harness_web_access: RwLock::new("enabled".to_string()),
            harness_egress_mode: RwLock::new("deny_private".to_string()),
            harness_context_editing: AtomicBool::new(true),
            harness_max_search_calls: AtomicU64::new(0),
            harness_max_fetch_calls: AtomicU64::new(0),
            harness_max_result_bytes: AtomicU64::new(0),
            harness_max_web_cost_usd_micros: AtomicU64::new(0),
            completion_gates_enabled: AtomicBool::new(true),
            mcp_deferred_tool_threshold: AtomicUsize::new(MCP_DEFERRED_TOOL_THRESHOLD_DEFAULT),
            cloud_spend_stop_line_usd: AtomicU64::new(
                rsi_common::cloud_spend::DEFAULT_STOP_LINE_USD,
            ),
            cloud_spend_daily_cap_usd: AtomicU64::new(
                rsi_common::cloud_spend::DEFAULT_DAILY_CAP_USD,
            ),
            rolling_queue_enabled: AtomicBool::new(false),
            deploy_drain_enabled: AtomicBool::new(true),
            rolling_queue_batch_size: AtomicU32::new(
                rsi_common::rolling_queue::ROLLING_QUEUE_DEFAULT_BATCH_SIZE,
            ),
            rolling_queue_speculation_depth: AtomicU32::new(
                rsi_common::rolling_queue::ROLLING_QUEUE_DEFAULT_SPECULATION_DEPTH,
            ),
            governor_build_slots: AtomicU32::new(4),
            governor_lander_slots: AtomicU32::new(5),
            governor_max_load: AtomicU32::new(0),
            governor_min_free_disk_gb: AtomicU64::new(30),
            governor_min_avail_mem_gb: AtomicU64::new(16),
            governor_max_workers_slice_gb: AtomicU64::new(30),
            agent_build_jobs: AtomicU32::new(4),
            agent_build_line_tables_only: AtomicBool::new(true),
            agent_build_sccache_enabled: AtomicBool::new(true),
            agent_build_sccache_cache_gib: AtomicU32::new(10),
            agent_build_slots: AtomicU32::new(16),
            rsid_scope_memory_high_mib: AtomicU64::new(RSID_SCOPE_MEMORY_HIGH_MIB_DEFAULT),
            rsid_scope_memory_max_mib: AtomicU64::new(RSID_SCOPE_MEMORY_MAX_MIB_DEFAULT),
            rsid_scope_memory_swap_max_mib: AtomicU64::new(RSID_SCOPE_MEMORY_SWAP_MAX_MIB_DEFAULT),
            rsid_scope_cpu_weight: AtomicU32::new(RSID_SCOPE_CPU_WEIGHT_DEFAULT),
            worker_scope_memory_high_mib: AtomicU64::new(worker_memory_high_mib),
            worker_scope_memory_max_mib: AtomicU64::new(worker_memory_max_mib),
            worker_scope_memory_swap_max_mib: AtomicU64::new(
                WORKER_SCOPE_MEMORY_SWAP_MAX_MIB_DEFAULT,
            ),
            worker_scope_cpu_weight: AtomicU32::new(WORKER_SCOPE_CPU_WEIGHT_DEFAULT),
            sandbox_build_cache_pressure_active: AtomicBool::new(false),
            stall_classifier_enabled: AtomicBool::new(config.stall_classifier_enabled),
            stall_classifier_model: RwLock::new(config.stall_classifier_model.clone()),
            stall_classifier_idle_secs: AtomicU64::new(config.stall_classifier_idle_secs),
            stall_classifier_idle_secs_codex: AtomicU64::new(
                config.stall_classifier_idle_secs_codex,
            ),
            stall_classifier_cooldown_secs: AtomicU64::new(config.stall_classifier_cooldown_secs),
            stall_classifier_max_per_session: AtomicU32::new(
                config.stall_classifier_max_per_session,
            ),
            stall_classifier_confidence_floor: RwLock::new(
                config.stall_classifier_confidence_floor,
            ),
            recursive_dag_recovery_controls_enabled: AtomicBool::new(
                config.recursive_dag_recovery_controls_enabled,
            ),
            recursive_dag_scheduler_controls_enabled: AtomicBool::new(
                config.recursive_dag_scheduler_controls_enabled,
            ),
            recursive_dag_cancellation_controls_enabled: AtomicBool::new(
                config.recursive_dag_cancellation_controls_enabled,
            ),
            recursive_dag_live_scheduler_control_enabled: AtomicBool::new(
                config.recursive_dag_live_scheduler_control_enabled,
            ),
            gv_render_recursive_origin: AtomicBool::new(config.gv_render_recursive_origin),
            gv_info_dashboard: AtomicBool::new(config.gv_info_dashboard),
            topology_executor_enabled: AtomicBool::new(config.topology_executor_enabled),
            topology_max_concurrent_build_nodes: AtomicU32::new(
                config.topology_max_concurrent_build_nodes,
            ),
            topology_bulk_fanout_min_openrouter: AtomicU32::new(
                config.topology_bulk_fanout_min_openrouter,
            ),
            recursive_dag_run_lease_ttl_ms: AtomicU64::new(config.recursive_dag_run_lease_ttl_ms),
            recursive_dag_max_concurrent_graphs: AtomicU32::new(
                config.recursive_dag_max_concurrent_graphs,
            ),
        })
    }

    pub fn to_json(&self) -> serde_json::Value {
        let reclaim = self.sandbox_build_cache_reclaim_snapshot();
        let mut value = serde_json::json!({
            "retry_enabled": self.retry_enabled.load(Ordering::Relaxed),
            "satellite_polling_enabled": self.satellite_polling_enabled.load(Ordering::Relaxed),
            "retry_max_default": self.retry_max_default.load(Ordering::Relaxed),
            "retry_on_stall": self.retry_on_stall.load(Ordering::Relaxed),
            "retry_max_backoff_ms": self.retry_max_backoff_ms.load(Ordering::Relaxed),
            "reconciliation_enabled": self.reconciliation_enabled.load(Ordering::Relaxed),
            "stall_detection_enabled": self.stall_detection_enabled.load(Ordering::Relaxed),
            "context_rotation_enabled": self.context_rotation_enabled.load(Ordering::Relaxed),
            "memory_enabled": self.memory_enabled.load(Ordering::Relaxed),
            "codegraph_indexing_enabled": self.codegraph_indexing_enabled.load(Ordering::Relaxed),
            "queue_enabled": self.queue_enabled.load(Ordering::Relaxed),
            "dream_enabled": self.dream_enabled.load(Ordering::Relaxed),
            "dialectic_enabled": self.dialectic_enabled.load(Ordering::Relaxed),
            "title_model_local": *self.title_model_local.read(),
            "title_model_provider": *self.title_model_provider.read(),
            "title_model_base_url": self.title_model_base_url.read().clone(),
            "title_model_fallback": *self.title_model_fallback.read(),
            "memory_model_local": *self.memory_model_local.read(),
            "memory_model_fallback": *self.memory_model_fallback.read(),
            "memory_model_fallback_provider": *self.memory_model_fallback_provider.read(),
            "memory_model_fallback_base_url": self.memory_model_fallback_base_url.read().clone(),
            "dream_model": *self.dream_model.read(),
            "dream_model_provider": *self.dream_model_provider.read(),
            "dream_model_base_url": self.dream_model_base_url.read().clone(),
            "dream_observation_threshold": self.dream_observation_threshold.load(Ordering::Relaxed),
            "dream_idle_secs": self.dream_idle_secs.load(Ordering::Relaxed),
            "dream_cooldown_secs": self.dream_cooldown_secs.load(Ordering::Relaxed),
            "prompt_compile_model_local": *self.prompt_compile_model_local.read(),
            "prompt_compile_model_provider": *self.prompt_compile_model_provider.read(),
            "prompt_compile_model_base_url": self.prompt_compile_model_base_url.read().clone(),
            "codex_sandbox_mode": *self.codex_sandbox_mode.read(),
            "claude_config_isolation": *self.claude_config_isolation.read(),
            "system_prompt_preset": *self.system_prompt_preset.read(),
            "orchestration_max_child_effort": *self.orchestration_max_child_effort.read(),
            "stall_classifier_enabled": self.stall_classifier_enabled.load(Ordering::Relaxed),
            "stall_classifier_model": *self.stall_classifier_model.read(),
            "stall_classifier_idle_secs": self.stall_classifier_idle_secs.load(Ordering::Relaxed),
            "stall_classifier_idle_secs_codex": self.stall_classifier_idle_secs_codex.load(Ordering::Relaxed),
            "stall_classifier_cooldown_secs": self.stall_classifier_cooldown_secs.load(Ordering::Relaxed),
            "stall_classifier_max_per_session": self.stall_classifier_max_per_session.load(Ordering::Relaxed),
            "stall_classifier_confidence_floor": *self.stall_classifier_confidence_floor.read(),
        });
        if let serde_json::Value::Object(ref mut map) = value {
            map.insert(
                rsi_common::launch_allowlist::LAUNCH_MODEL_ALLOWLIST_FIELD.to_string(),
                self.launch_model_allowlist.read().clone().into(),
            );
            map.insert(
                "session_retention_enabled".to_string(),
                self.session_retention_enabled
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "session_retention_window_hours".to_string(),
                self.session_retention_window_hours
                    .load(Ordering::Relaxed)
                    .into(),
            );
            for (name, field) in [
                (
                    "context_rotation_global_pct",
                    &self.context_rotation_global_pct,
                ),
                (
                    "context_rotation_claude_pct",
                    &self.context_rotation_claude_pct,
                ),
                (
                    "context_rotation_codex_pct",
                    &self.context_rotation_codex_pct,
                ),
            ] {
                map.insert(name.to_string(), override_json(field));
            }
            map.insert(
                "completed_transcript_cache_max_bytes".to_string(),
                self.completed_transcript_cache_max_bytes
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "rsid_scope_memory_high_mib".to_string(),
                self.rsid_scope_memory_high_mib
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "agent_build_jobs".to_string(),
                self.agent_build_jobs.load(Ordering::Relaxed).into(),
            );
            map.insert(
                "agent_build_line_tables_only".to_string(),
                self.agent_build_line_tables_only
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "agent_build_sccache_enabled".to_string(),
                self.agent_build_sccache_enabled
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "agent_build_sccache_cache_gib".to_string(),
                self.agent_build_sccache_cache_gib
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "agent_build_slots".to_string(),
                self.agent_build_slots.load(Ordering::Relaxed).into(),
            );
            map.insert(
                "rsid_scope_memory_max_mib".to_string(),
                self.rsid_scope_memory_max_mib
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "rsid_scope_memory_swap_max_mib".to_string(),
                self.rsid_scope_memory_swap_max_mib
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "rsid_scope_cpu_weight".to_string(),
                self.rsid_scope_cpu_weight.load(Ordering::Relaxed).into(),
            );
            map.insert(
                "worker_scope_memory_high_mib".to_string(),
                self.worker_scope_memory_high_mib
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "worker_scope_memory_max_mib".to_string(),
                self.worker_scope_memory_max_mib
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "worker_scope_memory_swap_max_mib".to_string(),
                self.worker_scope_memory_swap_max_mib
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "worker_scope_cpu_weight".to_string(),
                self.worker_scope_cpu_weight.load(Ordering::Relaxed).into(),
            );
            map.insert(
                "sandbox_build_cache_reclaim_enabled".to_string(),
                reclaim.enabled.into(),
            );
            map.insert(
                "sandbox_build_cache_reclaim_ttl_secs".to_string(),
                reclaim.ttl_secs.into(),
            );
            map.insert(
                "sandbox_build_cache_reclaim_interval_secs".to_string(),
                reclaim.interval_secs.into(),
            );
            map.insert(
                "sandbox_build_cache_reclaim_high_watermark_pct".to_string(),
                reclaim.high_watermark_pct.into(),
            );
            map.insert(
                "sandbox_build_cache_reclaim_low_watermark_pct".to_string(),
                reclaim.low_watermark_pct.into(),
            );
            map.insert(
                "sandbox_build_cache_reclaim_max_candidates".to_string(),
                reclaim.max_candidates.into(),
            );
            map.insert(
                "sandbox_max_source_roots".to_string(),
                self.sandbox_max_source_roots.load(Ordering::Relaxed).into(),
            );
            map.insert(
                "sandbox_min_free_gib".to_string(),
                self.sandbox_min_free_gib.load(Ordering::Relaxed).into(),
            );
            map.insert(
                "archived_sandbox_purge_enabled".to_string(),
                self.archived_sandbox_purge_enabled
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "program_hold_while_children_run".to_string(),
                self.program_hold_while_children_run
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "child_keepalive_enabled".to_string(),
                self.child_keepalive_enabled.load(Ordering::Relaxed).into(),
            );
            map.insert(
                "child_keepalive_window_secs".to_string(),
                self.child_keepalive_window_secs
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "governor_build_slots".to_string(),
                self.governor_build_slots.load(Ordering::Relaxed).into(),
            );
            map.insert(
                "governor_lander_slots".to_string(),
                self.governor_lander_slots.load(Ordering::Relaxed).into(),
            );
            map.insert(
                "governor_max_load".to_string(),
                self.governor_max_load.load(Ordering::Relaxed).into(),
            );
            map.insert(
                "governor_min_free_disk_gb".to_string(),
                self.governor_min_free_disk_gb
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "governor_min_avail_mem_gb".to_string(),
                self.governor_min_avail_mem_gb
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "governor_max_workers_slice_gb".to_string(),
                self.governor_max_workers_slice_gb
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "harness_web_access".to_string(),
                self.harness_web_access.read().clone().into(),
            );
            map.insert(
                "harness_egress_mode".to_string(),
                self.harness_egress_mode.read().clone().into(),
            );
            map.insert(
                "harness_context_editing".to_string(),
                self.harness_context_editing.load(Ordering::Relaxed).into(),
            );
            map.insert(
                "mcp.deferred_tool_threshold".to_string(),
                self.mcp_deferred_tool_threshold
                    .load(Ordering::Relaxed)
                    .into(),
            );
            for (field, atomic) in [
                ("cloud_spend_stop_line_usd", &self.cloud_spend_stop_line_usd),
                ("cloud_spend_daily_cap_usd", &self.cloud_spend_daily_cap_usd),
                ("harness_max_search_calls", &self.harness_max_search_calls),
                ("harness_max_fetch_calls", &self.harness_max_fetch_calls),
                ("harness_max_result_bytes", &self.harness_max_result_bytes),
                (
                    "harness_max_web_cost_usd_micros",
                    &self.harness_max_web_cost_usd_micros,
                ),
            ] {
                map.insert(field.to_string(), atomic.load(Ordering::Relaxed).into());
            }
            map.insert(
                "completion_gates_enabled".to_string(),
                self.completion_gates_enabled.load(Ordering::Relaxed).into(),
            );
            map.insert(
                "rolling_queue_enabled".to_string(),
                self.rolling_queue_enabled.load(Ordering::Relaxed).into(),
            );
            map.insert(
                "deploy_drain_enabled".to_string(),
                self.deploy_drain_enabled.load(Ordering::Relaxed).into(),
            );
            map.insert(
                "rolling_queue_batch_size".to_string(),
                self.rolling_queue_batch_size.load(Ordering::Relaxed).into(),
            );
            map.insert(
                "rolling_queue_speculation_depth".to_string(),
                self.rolling_queue_speculation_depth
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                rsi_common::provider_credentials::SETTING_VAULT_ENV_COMPAT.to_string(),
                self.vault_settings.env_compat().into(),
            );
            map.insert(
                rsi_common::provider_credentials::SETTING_VAULT_CHECK_TTL_SECS.to_string(),
                self.vault_settings.check_ttl_secs().into(),
            );
            map.insert(
                "api_route.openrouter".to_string(),
                self.openrouter_route.read().as_str().into(),
            );
            map.insert(
                "api_route.bedrock".to_string(),
                self.bedrock_route.read().as_str().into(),
            );
            map.insert(
                "api_route.fallback".to_string(),
                self.api_route_fallback.load(Ordering::Relaxed).into(),
            );
            map.insert(
                "openrouter_context_budget_tokens".to_string(),
                self.openrouter_context_budget_tokens
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "harness_max_iterations_per_turn".to_string(),
                self.harness_max_iterations_per_turn
                    .load(Ordering::Relaxed)
                    .into(),
            );
            for (model, route) in self.openrouter_model_routes.read().iter() {
                map.insert(
                    format!("api_route.openrouter.{model}"),
                    route.as_str().into(),
                );
            }
            for (model, route) in self.bedrock_model_routes.read().iter() {
                map.insert(format!("api_route.bedrock.{model}"), route.as_str().into());
            }
            map.insert(
                "coordinator_context_cap_tokens".to_string(),
                self.coordinator_context_cap_tokens
                    .load(Ordering::Relaxed)
                    .into(),
            );
            for (key, tokens) in self.coordinator_context_cap_overrides.read().iter() {
                map.insert(
                    format!("{COORDINATOR_CONTEXT_CAP_OVERRIDE_PREFIX}{key}"),
                    (*tokens).into(),
                );
            }
            map.insert("recursive_dag_inspection".to_string(), true.into());
            map.insert("recursive_dag_run_inspection".to_string(), true.into());
            map.insert("recursive_dag_recovery_status".to_string(), true.into());
            map.insert(
                "recursive_dag_live_status_inspection".to_string(),
                true.into(),
            );
            map.insert(
                "recursive_dag_live_validation_inspection".to_string(),
                true.into(),
            );
            map.insert(
                "recursive_dag_recovery_controls_enabled".to_string(),
                self.recursive_dag_recovery_controls_enabled
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "recursive_dag_scheduler_controls_enabled".to_string(),
                self.recursive_dag_scheduler_controls_enabled
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "recursive_dag_cancellation_controls_enabled".to_string(),
                self.recursive_dag_cancellation_controls_enabled
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "recursive_dag_live_scheduler_control_enabled".to_string(),
                self.recursive_dag_live_scheduler_control_enabled
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "gv_render_recursive_origin".to_string(),
                self.gv_render_recursive_origin
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "gv_info_dashboard".to_string(),
                self.gv_info_dashboard.load(Ordering::Relaxed).into(),
            );
            map.insert(
                "topology_executor_enabled".to_string(),
                self.topology_executor_enabled
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "topology_max_concurrent_build_nodes".to_string(),
                self.topology_max_concurrent_build_nodes
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "topology_bulk_fanout_min_openrouter".to_string(),
                self.topology_bulk_fanout_min_openrouter
                    .load(Ordering::Relaxed)
                    .into(),
            );
            // Fixed Phase 5A.5 constants, not runtime state and not the live
            // capability flags (those are `DaemonCapabilities.recursive_dag_*`,
            // fed by `recursive_dag_live_scheduler_control_enabled`). The
            // `_fixed_` infix keeps them from being mistaken for those flags.
            map.insert(
                "recursive_dag_fixed_fake_executor_only".to_string(),
                true.into(),
            );
            map.insert(
                "recursive_dag_fixed_live_executor_enabled".to_string(),
                false.into(),
            );
            map.insert(
                "recursive_dag_fixed_background_loop_enabled".to_string(),
                false.into(),
            );
            map.insert(
                "recursive_dag_run_lease_ttl_ms".to_string(),
                self.recursive_dag_run_lease_ttl_ms
                    .load(Ordering::Relaxed)
                    .into(),
            );
            map.insert(
                "recursive_dag_max_concurrent_graphs".to_string(),
                self.recursive_dag_max_concurrent_graphs
                    .load(Ordering::Relaxed)
                    .into(),
            );
        }
        value
    }

    /// Current resource governor policy (#1014), read from the live atomics.
    #[must_use]
    pub fn governor_policy(&self) -> crate::store_support::config_types::GovernorPolicy {
        crate::store_support::config_types::GovernorPolicy {
            build_slots: self.governor_build_slots.load(Ordering::Relaxed),
            lander_slots: self.governor_lander_slots.load(Ordering::Relaxed),
            max_load: self.governor_max_load.load(Ordering::Relaxed),
            min_free_disk_gb: self.governor_min_free_disk_gb.load(Ordering::Relaxed),
            min_avail_mem_gb: self.governor_min_avail_mem_gb.load(Ordering::Relaxed),
            max_workers_slice_gb: self.governor_max_workers_slice_gb.load(Ordering::Relaxed),
        }
    }

    /// The daemon-wide Harness tool policy defaults (#792). A cap of 0 is
    /// unlimited; an all-default result restricts nothing.
    pub fn harness_tool_policy_defaults(
        &self,
    ) -> rsi_common::harness_tool_policy::HarnessToolPolicy {
        use rsi_common::harness_tool_policy::{HarnessToolPolicy, ToolBudgets, WebAccessMode};
        let cap = |atomic: &AtomicU64| Some(atomic.load(Ordering::Relaxed)).filter(|&n| n > 0);
        let web = WebAccessMode::parse(&self.harness_web_access.read())
            .filter(|mode| *mode != WebAccessMode::Enabled);
        HarnessToolPolicy {
            enabled_tools: None,
            denied_tools: Vec::new(),
            web_access: web,
            egress: rsi_common::egress_policy::EgressMode::parse(&self.harness_egress_mode.read())
                .filter(|mode| *mode != rsi_common::egress_policy::EgressMode::DenyPrivate),
            // Only an explicit off is a default worth layering: unset already
            // means on, so an all-default result still restricts nothing.
            context_editing: (!self.harness_context_editing.load(Ordering::Relaxed))
                .then_some(false),
            budgets: ToolBudgets {
                max_search_calls: cap(&self.harness_max_search_calls)
                    .and_then(|n| u32::try_from(n).ok()),
                max_fetch_calls: cap(&self.harness_max_fetch_calls)
                    .and_then(|n| u32::try_from(n).ok()),
                max_result_bytes: cap(&self.harness_max_result_bytes),
                max_web_cost_usd_micros: cap(&self.harness_max_web_cost_usd_micros),
            },
        }
    }

    /// The ONE predicate for "this session is under a Harness tool policy":
    /// it has a stored per-session row, or it runs a Harness-loop provider that
    /// the daemon-wide defaults (never stored) restrict. Child inheritance and
    /// the provider-route guard both use it, so a default-only restriction
    /// cannot be escaped through a CLI child or a Harness -> Codex CLI fallback.
    pub fn session_is_under_tool_policy(
        &self,
        provider: rsi_common::types::SessionProvider,
        stored: Option<&rsi_common::harness_tool_policy::HarnessToolPolicy>,
    ) -> bool {
        stored.is_some()
            || (rsi_common::harness_tool_policy::provider_runs_harness_loop(provider)
                && !self.harness_tool_policy_defaults().is_default())
    }

    /// #1036: the operator's remote-gate spend caps.
    pub fn cloud_spend_caps(&self) -> rsi_common::cloud_spend::CloudSpendCaps {
        rsi_common::cloud_spend::CloudSpendCaps {
            stop_line_usd: self.cloud_spend_stop_line_usd.load(Ordering::Relaxed),
            daily_cap_usd: self.cloud_spend_daily_cap_usd.load(Ordering::Relaxed),
        }
    }

    pub fn mcp_deferred_tool_threshold(&self) -> usize {
        self.mcp_deferred_tool_threshold
            .load(Ordering::Relaxed)
            .clamp(0, 256)
    }

    pub fn persisted_field_value(&self, field: &str) -> Option<serde_json::Value> {
        if !is_persisted_runtime_config_field(field) {
            return None;
        }
        Some(
            self.to_json()
                .get(field)
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
    }

    /// #1050: the operator's per-turn iteration cap for Harness sessions.
    pub fn harness_max_iterations(&self) -> u32 {
        self.harness_max_iterations_per_turn.load(Ordering::Relaxed)
    }

    /// Issue #692: the typed refusal for a launch of `model` on `provider`, or
    /// `None` when the operator allowlist admits it (an empty list admits
    /// everything). `model` must be the EFFECTIVE model (project and provider
    /// defaults applied); `None` means no model could be determined. Read live,
    /// so a change applies to the next launch.
    pub fn launch_model_refusal(
        &self,
        provider: SessionProvider,
        model: Option<&str>,
    ) -> Option<String> {
        let allowlist = self.launch_model_allowlist.read();
        if rsi_common::launch_allowlist::launch_model_allowed(&allowlist, Some(provider), model) {
            return None;
        }
        Some(rsi_common::launch_allowlist::launch_model_refusal(
            &allowlist, model,
        ))
    }

    /// #794: whether a Harness session runs its configured completion gates.
    pub fn completion_gates_enabled(&self) -> bool {
        self.completion_gates_enabled.load(Ordering::Relaxed)
    }

    /// #966: the absolute live-context budget for OpenRouter sessions, or
    /// `None` when the operator turned it off.
    pub fn openrouter_context_budget(&self) -> Option<u64> {
        Some(
            self.openrouter_context_budget_tokens
                .load(Ordering::Relaxed),
        )
        .filter(|&n| n > 0)
    }

    /// #1005: the hard context cap for a coordinating seat on `provider` /
    /// `model`, or `None` when it is off. A model override wins over a
    /// provider override, which wins over the global setting.
    pub fn coordinator_context_cap(
        &self,
        provider: SessionProvider,
        model: Option<&str>,
    ) -> Option<u64> {
        let provider = provider_key(provider);
        let overrides = self.coordinator_context_cap_overrides.read();
        let cap = model
            .and_then(|model| overrides.get(&format!("{provider}/{model}")))
            .or_else(|| overrides.get(&provider))
            .copied()
            .unwrap_or_else(|| self.coordinator_context_cap_tokens.load(Ordering::Relaxed));
        Some(cap).filter(|&tokens| tokens > 0)
    }

    pub fn openrouter_route_for(&self, model: &str) -> OpenRouterRoute {
        let normalized = model.strip_prefix("openrouter/").unwrap_or(model);
        self.openrouter_model_routes
            .read()
            .get(normalized)
            .copied()
            .unwrap_or_else(|| *self.openrouter_route.read())
    }

    pub fn bedrock_route_for(&self, model: &str) -> OpenRouterRoute {
        let normalized = model.strip_prefix("bedrock/").unwrap_or(model);
        self.bedrock_model_routes
            .read()
            .get(normalized)
            .copied()
            .unwrap_or_else(|| *self.bedrock_route.read())
    }

    pub fn any_openrouter_harness_route(&self) -> bool {
        *self.openrouter_route.read() == OpenRouterRoute::Harness
            || self
                .openrouter_model_routes
                .read()
                .values()
                .any(|route| *route == OpenRouterRoute::Harness)
    }

    pub fn sandbox_build_cache_reclaim_snapshot(&self) -> SandboxBuildCacheReclaimConfig {
        *self.sandbox_build_cache_reclaim.read()
    }

    /// Validate and canonicalize one 69A setting without publishing it.
    pub fn prepare_sandbox_build_cache_update(
        &self,
        field: &str,
        value: &serde_json::Value,
    ) -> Result<Option<SandboxBuildCacheReclaimConfig>, String> {
        let mut next = self.sandbox_build_cache_reclaim_snapshot();
        match field {
            "sandbox_build_cache_reclaim_enabled" => {
                next.enabled = value.as_bool().ok_or("expected bool")?;
            }
            "sandbox_build_cache_reclaim_ttl_secs" => {
                next.ttl_secs = bounded_u64(
                    value,
                    SANDBOX_BUILD_CACHE_RECLAIM_TTL_SECS_MIN,
                    SANDBOX_BUILD_CACHE_RECLAIM_TTL_SECS_MAX,
                )?;
            }
            "sandbox_build_cache_reclaim_interval_secs" => {
                next.interval_secs = bounded_u64(
                    value,
                    SANDBOX_BUILD_CACHE_RECLAIM_INTERVAL_SECS_MIN,
                    SANDBOX_BUILD_CACHE_RECLAIM_INTERVAL_SECS_MAX,
                )?;
            }
            "sandbox_build_cache_reclaim_high_watermark_pct" => {
                next.high_watermark_pct = bounded_u64(
                    value,
                    u64::from(SANDBOX_BUILD_CACHE_RECLAIM_HIGH_WATERMARK_PCT_MIN),
                    u64::from(SANDBOX_BUILD_CACHE_RECLAIM_HIGH_WATERMARK_PCT_MAX),
                )? as u8;
            }
            "sandbox_build_cache_reclaim_low_watermark_pct" => {
                next.low_watermark_pct = bounded_u64(
                    value,
                    u64::from(SANDBOX_BUILD_CACHE_RECLAIM_LOW_WATERMARK_PCT_MIN),
                    u64::from(SANDBOX_BUILD_CACHE_RECLAIM_LOW_WATERMARK_PCT_MAX),
                )? as u8;
            }
            "sandbox_build_cache_reclaim_max_candidates" => {
                next.max_candidates = bounded_u64(
                    value,
                    u64::from(SANDBOX_BUILD_CACHE_RECLAIM_MAX_CANDIDATES_MIN),
                    u64::from(SANDBOX_BUILD_CACHE_RECLAIM_MAX_CANDIDATES_MAX),
                )? as u32;
            }
            _ => return Ok(None),
        }
        next.validate().map_err(str::to_string).map(Some)
    }

    /// Publish an already validated snapshot. This path performs no I/O and
    /// cannot fail, so RPC callers can safely use it after durable commit.
    pub fn publish_sandbox_build_cache_config(&self, config: SandboxBuildCacheReclaimConfig) {
        *self.sandbox_build_cache_reclaim.write() = config;
    }

    /// Update a single field by name. Returns `Ok(true)` if the field was found and updated.
    pub fn update_field(&self, field: &str, value: &serde_json::Value) -> Result<bool, String> {
        if let Some(model) = bedrock_model_route_key(field) {
            if value.is_null() {
                self.bedrock_model_routes.write().remove(model);
            } else {
                let route = OpenRouterRoute::parse(value.as_str().ok_or("expected string")?)
                    .ok_or("expected harness or codex_cli")?;
                self.bedrock_model_routes
                    .write()
                    .insert(model.to_string(), route);
            }
            return Ok(true);
        }
        if let Some(key) = coordinator_context_cap_override_key(field) {
            if value.is_null() {
                self.coordinator_context_cap_overrides.write().remove(key);
            } else {
                let tokens = bounded_cap_tokens(value)?;
                self.coordinator_context_cap_overrides
                    .write()
                    .insert(key.to_string(), tokens);
            }
            return Ok(true);
        }
        if let Some(model) = openrouter_model_route_key(field) {
            if value.is_null() {
                self.openrouter_model_routes.write().remove(model);
            } else {
                let route = OpenRouterRoute::parse(value.as_str().ok_or("expected string")?)
                    .ok_or("expected harness or codex_cli")?;
                self.openrouter_model_routes
                    .write()
                    .insert(model.to_string(), route);
            }
            return Ok(true);
        }
        match field {
            "sandbox_max_source_roots" => {
                let value = bounded_u64(value, 1, 65_536)?;
                self.sandbox_max_source_roots.store(
                    u32::try_from(value).map_err(|_| "expected value within u32 range")?,
                    Ordering::Relaxed,
                );
                Ok(true)
            }
            "sandbox_min_free_gib" => {
                let value = bounded_u64(value, 0, 1024)?;
                self.sandbox_min_free_gib.store(value, Ordering::Relaxed);
                Ok(true)
            }
            "archived_sandbox_purge_enabled" => {
                self.archived_sandbox_purge_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "program_hold_while_children_run" => {
                self.program_hold_while_children_run
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "child_keepalive_enabled" => {
                self.child_keepalive_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "child_keepalive_window_secs" => {
                let value = bounded_u64(
                    value,
                    rsi_common::child_autonomy::KEEPALIVE_WINDOW_MIN_SECS,
                    rsi_common::child_autonomy::KEEPALIVE_WINDOW_MAX_SECS,
                )?;
                self.child_keepalive_window_secs
                    .store(value, Ordering::Relaxed);
                Ok(true)
            }
            "governor_build_slots" => {
                let value = bounded_u64(value, 1, 16)?;
                self.governor_build_slots.store(
                    u32::try_from(value).map_err(|_| "expected value within u32 range")?,
                    Ordering::Relaxed,
                );
                Ok(true)
            }
            "governor_lander_slots" => {
                let value = bounded_u64(value, 1, 16)?;
                self.governor_lander_slots.store(
                    u32::try_from(value).map_err(|_| "expected value within u32 range")?,
                    Ordering::Relaxed,
                );
                Ok(true)
            }
            "governor_max_load" => {
                let value = bounded_u64(value, 0, 1024)?;
                self.governor_max_load.store(
                    u32::try_from(value).map_err(|_| "expected value within u32 range")?,
                    Ordering::Relaxed,
                );
                Ok(true)
            }
            "governor_min_free_disk_gb" => {
                let value = bounded_u64(value, 0, 4096)?;
                self.governor_min_free_disk_gb
                    .store(value, Ordering::Relaxed);
                Ok(true)
            }
            "governor_min_avail_mem_gb" => {
                let value = bounded_u64(value, 0, 4096)?;
                self.governor_min_avail_mem_gb
                    .store(value, Ordering::Relaxed);
                Ok(true)
            }
            "governor_max_workers_slice_gb" => {
                let value = bounded_u64(value, 1, 4096)?;
                self.governor_max_workers_slice_gb
                    .store(value, Ordering::Relaxed);
                Ok(true)
            }
            "harness_web_access" => {
                let text = value.as_str().ok_or("expected string")?;
                let mode = rsi_common::harness_tool_policy::WebAccessMode::parse(text)
                    .ok_or("expected enabled, hosted_only or disabled")?;
                *self.harness_web_access.write() = mode.as_str().to_string();
                Ok(true)
            }
            "harness_egress_mode" => {
                let text = value.as_str().ok_or("expected string")?;
                let mode = rsi_common::egress_policy::EgressMode::parse(text)
                    .ok_or("expected deny_private or offline")?;
                *self.harness_egress_mode.write() = mode.as_str().to_string();
                Ok(true)
            }
            "harness_context_editing" => {
                self.harness_context_editing
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "harness_max_search_calls" | "harness_max_fetch_calls" => {
                let value = bounded_u64(value, 0, 1_000_000)?;
                let atomic = if field == "harness_max_search_calls" {
                    &self.harness_max_search_calls
                } else {
                    &self.harness_max_fetch_calls
                };
                atomic.store(value, Ordering::Relaxed);
                Ok(true)
            }
            "completion_gates_enabled" => {
                self.completion_gates_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "harness_max_result_bytes" => {
                let value = bounded_u64(value, 0, 1 << 40)?;
                self.harness_max_result_bytes
                    .store(value, Ordering::Relaxed);
                Ok(true)
            }
            "harness_max_web_cost_usd_micros" => {
                let value = bounded_u64(value, 0, 1_000_000_000_000)?;
                self.harness_max_web_cost_usd_micros
                    .store(value, Ordering::Relaxed);
                Ok(true)
            }
            "mcp.deferred_tool_threshold" => {
                let value = bounded_u64(value, 0, 256)?;
                self.mcp_deferred_tool_threshold.store(
                    usize::try_from(value).expect("bounded usize"),
                    Ordering::Relaxed,
                );
                Ok(true)
            }
            "cloud_spend_stop_line_usd" => {
                let value = bounded_u64(value, 0, rsi_common::cloud_spend::MAX_CAP_USD)?;
                self.cloud_spend_stop_line_usd
                    .store(value, Ordering::Relaxed);
                Ok(true)
            }
            "cloud_spend_daily_cap_usd" => {
                let value = bounded_u64(value, 0, rsi_common::cloud_spend::MAX_CAP_USD)?;
                self.cloud_spend_daily_cap_usd
                    .store(value, Ordering::Relaxed);
                Ok(true)
            }
            "rolling_queue_enabled" => {
                self.rolling_queue_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "deploy_drain_enabled" => {
                self.deploy_drain_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "rolling_queue_batch_size" => {
                let value = bounded_u64(
                    value,
                    rsi_common::rolling_queue::ROLLING_QUEUE_MIN_BATCH_SIZE,
                    rsi_common::rolling_queue::ROLLING_QUEUE_MAX_BATCH_SIZE,
                )?;
                self.rolling_queue_batch_size.store(
                    u32::try_from(value).map_err(|_| "expected value within u32 range")?,
                    Ordering::Relaxed,
                );
                Ok(true)
            }
            "rolling_queue_speculation_depth" => {
                let value = bounded_u64(
                    value,
                    0,
                    rsi_common::rolling_queue::ROLLING_QUEUE_MAX_SPECULATION_DEPTH,
                )?;
                self.rolling_queue_speculation_depth.store(
                    u32::try_from(value).map_err(|_| "expected value within u32 range")?,
                    Ordering::Relaxed,
                );
                Ok(true)
            }
            "completed_transcript_cache_max_bytes" => {
                let cap = value.as_u64().ok_or("expected non-negative integer")?;
                self.completed_transcript_cache_max_bytes
                    .store(cap, Ordering::Relaxed);
                Ok(true)
            }
            "api_route.openrouter" => {
                let route = OpenRouterRoute::parse(value.as_str().ok_or("expected string")?)
                    .ok_or("expected harness or codex_cli")?;
                *self.openrouter_route.write() = route;
                Ok(true)
            }
            "api_route.bedrock" => {
                let route = OpenRouterRoute::parse(value.as_str().ok_or("expected string")?)
                    .ok_or("expected harness or codex_cli")?;
                *self.bedrock_route.write() = route;
                Ok(true)
            }
            "api_route.fallback" => {
                self.api_route_fallback
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "coordinator_context_cap_tokens" => {
                self.coordinator_context_cap_tokens
                    .store(bounded_cap_tokens(value)?, Ordering::Relaxed);
                Ok(true)
            }
            "openrouter_context_budget_tokens" => {
                let range =
                    OPENROUTER_CONTEXT_BUDGET_TOKENS_MIN..=OPENROUTER_CONTEXT_BUDGET_TOKENS_MAX;
                let tokens = value
                    .as_u64()
                    .filter(|tokens| *tokens == 0 || range.contains(tokens))
                    .ok_or_else(|| {
                        format!(
                            "expected 0 (off) or an integer in {}..={}",
                            range.start(),
                            range.end()
                        )
                    })?;
                self.openrouter_context_budget_tokens
                    .store(tokens, Ordering::Relaxed);
                Ok(true)
            }
            "harness_max_iterations_per_turn" => {
                let iterations = bounded_u64(
                    value,
                    u64::from(HARNESS_MAX_ITERATIONS_PER_TURN_MIN),
                    u64::from(HARNESS_MAX_ITERATIONS_PER_TURN_MAX),
                )?;
                self.harness_max_iterations_per_turn.store(
                    u32::try_from(iterations).map_err(|_| "expected value within u32 range")?,
                    Ordering::Relaxed,
                );
                Ok(true)
            }
            "retry_enabled" => {
                let v = value.as_bool().ok_or("expected bool")?;
                self.retry_enabled.store(v, Ordering::Relaxed);
                if !v {
                    self.retry_max_default.store(0, Ordering::Relaxed);
                }
                Ok(true)
            }
            "satellite_polling_enabled" => {
                self.satellite_polling_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "retry_max_default" => {
                let v = value.as_u64().ok_or("expected number")? as u8;
                self.retry_max_default.store(v, Ordering::Relaxed);
                self.retry_enabled.store(v > 0, Ordering::Relaxed);
                Ok(true)
            }
            "retry_on_stall" => {
                self.retry_on_stall
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "retry_max_backoff_ms" => {
                let v = value.as_u64().ok_or("expected number")?;
                self.retry_max_backoff_ms.store(v, Ordering::Relaxed);
                Ok(true)
            }
            "reconciliation_enabled" => {
                self.reconciliation_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "stall_detection_enabled" => {
                self.stall_detection_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "context_rotation_enabled" => {
                self.context_rotation_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "context_rotation_global_pct" => {
                update_rotation_pct(&self.context_rotation_global_pct, value)?;
                Ok(true)
            }
            "context_rotation_claude_pct" => {
                update_rotation_pct(&self.context_rotation_claude_pct, value)?;
                Ok(true)
            }
            "context_rotation_codex_pct" => {
                update_rotation_pct(&self.context_rotation_codex_pct, value)?;
                Ok(true)
            }
            "memory_enabled" => {
                self.memory_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "session_retention_enabled" => {
                self.session_retention_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "session_retention_window_hours" => {
                self.session_retention_window_hours
                    .store(bounded_u64(value, 1, 8760)?, Ordering::Relaxed);
                Ok(true)
            }
            "codegraph_indexing_enabled" => {
                self.codegraph_indexing_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "queue_enabled" => {
                self.queue_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "dream_enabled" => {
                self.dream_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "dialectic_enabled" => {
                self.dialectic_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "title_model_local" => {
                let v = value.as_str().ok_or("expected string")?;
                *self.title_model_local.write() = v.to_string();
                Ok(true)
            }
            "title_model_provider" => {
                *self.title_model_provider.write() = serde_json::from_value(value.clone())
                    .map_err(|_| "expected session provider")?;
                Ok(true)
            }
            "title_model_base_url" => {
                *self.title_model_base_url.write() = optional_string(value)?;
                Ok(true)
            }
            "title_model_api_key" => {
                *self.title_model_api_key.write() = optional_string(value)?;
                Ok(true)
            }
            "title_model_fallback" => {
                let v = value.as_str().ok_or("expected string")?;
                *self.title_model_fallback.write() = v.to_string();
                Ok(true)
            }
            "memory_model_local" => {
                let v = value.as_str().ok_or("expected string")?;
                *self.memory_model_local.write() = v.to_string();
                Ok(true)
            }
            "memory_model_fallback" => {
                let v = value.as_str().ok_or("expected string")?;
                *self.memory_model_fallback.write() = v.to_string();
                Ok(true)
            }
            "memory_model_fallback_provider" => {
                *self.memory_model_fallback_provider.write() =
                    serde_json::from_value(value.clone())
                        .map_err(|_| "expected session provider")?;
                Ok(true)
            }
            "memory_model_fallback_base_url" => {
                *self.memory_model_fallback_base_url.write() = optional_string(value)?;
                Ok(true)
            }
            "memory_model_fallback_api_key" => {
                *self.memory_model_fallback_api_key.write() = optional_string(value)?;
                Ok(true)
            }
            "dream_model" => {
                let v = value.as_str().ok_or("expected string")?;
                *self.dream_model.write() = v.to_string();
                Ok(true)
            }
            "dream_model_provider" => {
                *self.dream_model_provider.write() = serde_json::from_value(value.clone())
                    .map_err(|_| "expected session provider")?;
                Ok(true)
            }
            "dream_model_base_url" => {
                *self.dream_model_base_url.write() = optional_string(value)?;
                Ok(true)
            }
            "dream_model_api_key" => {
                *self.dream_model_api_key.write() = optional_string(value)?;
                Ok(true)
            }
            "dream_observation_threshold" => {
                let v = value.as_u64().ok_or("expected number")?;
                self.dream_observation_threshold.store(v, Ordering::Relaxed);
                Ok(true)
            }
            "dream_idle_secs" => {
                let v = value.as_u64().ok_or("expected number")?;
                self.dream_idle_secs.store(v, Ordering::Relaxed);
                Ok(true)
            }
            "dream_cooldown_secs" => {
                let v = value.as_u64().ok_or("expected number")?;
                self.dream_cooldown_secs.store(v, Ordering::Relaxed);
                Ok(true)
            }
            "prompt_compile_model_local" => {
                let v = value.as_str().ok_or("expected string")?;
                *self.prompt_compile_model_local.write() = v.to_string();
                Ok(true)
            }
            "prompt_compile_model_provider" => {
                *self.prompt_compile_model_provider.write() = serde_json::from_value(value.clone())
                    .map_err(|_| "expected session provider")?;
                Ok(true)
            }
            "prompt_compile_model_base_url" => {
                *self.prompt_compile_model_base_url.write() = optional_string(value)?;
                Ok(true)
            }
            "prompt_compile_model_api_key" => {
                *self.prompt_compile_model_api_key.write() = optional_string(value)?;
                Ok(true)
            }
            "codex_sandbox_mode" => {
                let v = value.as_str().ok_or("expected string")?;
                let parsed = crate::store_support::provider_settings::CodexSandboxMode::parse(v)
                    .ok_or("expected one of: read-only, workspace-write, danger-full-access")?;
                *self.codex_sandbox_mode.write() = parsed.cli_arg().to_string();
                Ok(true)
            }
            "claude_config_isolation" => {
                let v = value.as_str().ok_or("expected string")?;
                let parsed =
                    crate::store_support::provider_settings::ClaudeConfigIsolation::parse(v)
                        .ok_or("expected one of: off, settings, strict")?;
                *self.claude_config_isolation.write() = parsed.cli_arg().to_string();
                Ok(true)
            }
            "system_prompt_preset" => {
                // Daemon-owned, persisted via daemon_settings table.
                // The in-memory lock is the source of truth at runtime; the
                // SQLite write-through happens in `RpcServer::handle_update_daemon_config`
                // after this call returns Ok(true). This method intentionally
                // does NOT touch SQLite — it can't (no Store handle in scope).
                let v = value.as_str().ok_or("expected string")?;
                let canonical = normalize_system_prompt_preset(v)
                    .ok_or("expected one of: default, concise, code-only, caveman")?;
                *self.system_prompt_preset.write() = canonical.to_string();
                Ok(true)
            }
            rsi_common::provider_credentials::SETTING_VAULT_ENV_COMPAT => {
                let enabled = value.as_bool().ok_or("expected bool")?;
                self.vault_settings
                    .env_compat
                    .store(enabled, Ordering::Release);
                Ok(true)
            }
            rsi_common::provider_credentials::SETTING_VAULT_CHECK_TTL_SECS => {
                let secs = bounded_u64(
                    value,
                    rsi_common::provider_credentials::MIN_CHECK_TTL_SECS,
                    rsi_common::provider_credentials::MAX_CHECK_TTL_SECS,
                )?;
                self.vault_settings
                    .check_ttl_secs
                    .store(secs, Ordering::Release);
                Ok(true)
            }
            "orchestration_max_child_effort" => {
                // Issue #35 (b). Bounded enum on write. This is the boundary
                // that makes the R8 LOW-2 read-path degrade unreachable for the
                // supported path: a value that never gets stored can never be
                // read back malformed. `null` is accepted as a spelling of
                // "clear the ceiling" so a caller can unset without knowing the
                // sentinel.
                let v = if value.is_null() {
                    rsi_common::model_control::ORCHESTRATION_MAX_CHILD_EFFORT_UNSET
                } else {
                    value.as_str().ok_or("expected string")?
                };
                let canonical =
                    rsi_common::model_control::normalize_orchestration_max_child_effort(v)
                        .ok_or("expected one of: unset, low, medium, high, xhigh, max, ultra")?;
                *self.orchestration_max_child_effort.write() = canonical.to_string();
                Ok(true)
            }
            "launch_model_allowlist" => {
                *self.launch_model_allowlist.write() =
                    rsi_common::launch_allowlist::normalize_launch_model_allowlist(value)?;
                Ok(true)
            }
            "sandbox_build_cache_reclaim_enabled" => {
                let prepared = self
                    .prepare_sandbox_build_cache_update(field, value)?
                    .expect("matched 69A field must prepare");
                self.publish_sandbox_build_cache_config(prepared);
                Ok(true)
            }
            "sandbox_build_cache_reclaim_ttl_secs" => {
                let prepared = self
                    .prepare_sandbox_build_cache_update(field, value)?
                    .expect("matched 69A field must prepare");
                self.publish_sandbox_build_cache_config(prepared);
                Ok(true)
            }
            "sandbox_build_cache_reclaim_interval_secs" => {
                let prepared = self
                    .prepare_sandbox_build_cache_update(field, value)?
                    .expect("matched 69A field must prepare");
                self.publish_sandbox_build_cache_config(prepared);
                Ok(true)
            }
            "sandbox_build_cache_reclaim_high_watermark_pct" => {
                let prepared = self
                    .prepare_sandbox_build_cache_update(field, value)?
                    .expect("matched 69A field must prepare");
                self.publish_sandbox_build_cache_config(prepared);
                Ok(true)
            }
            "sandbox_build_cache_reclaim_low_watermark_pct" => {
                let prepared = self
                    .prepare_sandbox_build_cache_update(field, value)?
                    .expect("matched 69A field must prepare");
                self.publish_sandbox_build_cache_config(prepared);
                Ok(true)
            }
            "sandbox_build_cache_reclaim_max_candidates" => {
                let prepared = self
                    .prepare_sandbox_build_cache_update(field, value)?
                    .expect("matched 69A field must prepare");
                self.publish_sandbox_build_cache_config(prepared);
                Ok(true)
            }
            "stall_classifier_enabled" => {
                self.stall_classifier_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "stall_classifier_model" => {
                let v = value.as_str().ok_or("expected string")?;
                if v.trim().is_empty() {
                    return Err("expected non-empty model name".to_string());
                }
                *self.stall_classifier_model.write() = v.to_string();
                Ok(true)
            }
            "stall_classifier_idle_secs" => {
                let v = value.as_u64().ok_or("expected number")?;
                self.stall_classifier_idle_secs.store(v, Ordering::Relaxed);
                Ok(true)
            }
            "stall_classifier_idle_secs_codex" => {
                let v = value.as_u64().ok_or("expected number")?;
                self.stall_classifier_idle_secs_codex
                    .store(v, Ordering::Relaxed);
                Ok(true)
            }
            "stall_classifier_cooldown_secs" => {
                let v = value.as_u64().ok_or("expected number")?;
                self.stall_classifier_cooldown_secs
                    .store(v, Ordering::Relaxed);
                Ok(true)
            }
            "stall_classifier_max_per_session" => {
                let v = value.as_u64().ok_or("expected number")?;
                let v_u32 = u32::try_from(v).map_err(|_| "value exceeds u32::MAX")?;
                self.stall_classifier_max_per_session
                    .store(v_u32, Ordering::Relaxed);
                Ok(true)
            }
            "stall_classifier_confidence_floor" => {
                let v = value.as_f64().ok_or("expected number")?;
                if !(0.0..=1.0).contains(&v) {
                    return Err("expected 0.0..=1.0".to_string());
                }
                *self.stall_classifier_confidence_floor.write() = v;
                Ok(true)
            }
            "agent_build_jobs" => {
                let jobs = bounded_u64(value, 1, 32)? as u32;
                self.agent_build_jobs.store(jobs, Ordering::Relaxed);
                Ok(true)
            }
            "agent_build_line_tables_only" => {
                self.agent_build_line_tables_only
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "agent_build_sccache_enabled" => {
                self.agent_build_sccache_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "agent_build_sccache_cache_gib" => {
                let gib = bounded_u64(value, 1, 512)? as u32;
                self.agent_build_sccache_cache_gib
                    .store(gib, Ordering::Relaxed);
                Ok(true)
            }
            "agent_build_slots" => {
                let slots = bounded_u64(value, 1, 64)? as u32;
                self.agent_build_slots.store(slots, Ordering::Relaxed);
                Ok(true)
            }
            "rsid_scope_memory_high_mib" => {
                let v = bounded_u64(
                    value,
                    RSID_SCOPE_MEMORY_LIMIT_MIB_MIN,
                    RSID_SCOPE_MEMORY_LIMIT_MIB_MAX,
                )?;
                if v >= self.rsid_scope_memory_max_mib.load(Ordering::Relaxed) {
                    return Err("MemoryHigh must be below MemoryMax".to_string());
                }
                self.rsid_scope_memory_high_mib.store(v, Ordering::Relaxed);
                Ok(true)
            }
            "rsid_scope_memory_max_mib" => {
                let v = bounded_u64(
                    value,
                    RSID_SCOPE_MEMORY_LIMIT_MIB_MIN,
                    RSID_SCOPE_MEMORY_LIMIT_MIB_MAX,
                )?;
                if v <= self.rsid_scope_memory_high_mib.load(Ordering::Relaxed) {
                    return Err("MemoryMax must be above MemoryHigh".to_string());
                }
                self.rsid_scope_memory_max_mib.store(v, Ordering::Relaxed);
                Ok(true)
            }
            "rsid_scope_memory_swap_max_mib" => {
                let v = bounded_u64(value, 0, RSID_SCOPE_MEMORY_LIMIT_MIB_MAX)?;
                self.rsid_scope_memory_swap_max_mib
                    .store(v, Ordering::Relaxed);
                Ok(true)
            }
            "rsid_scope_cpu_weight" => {
                let v = value.as_u64().ok_or("expected unsigned integer")?;
                let v = u32::try_from(v).map_err(|_| "value exceeds u32::MAX")?;
                if !(RSID_SCOPE_CPU_WEIGHT_MIN..=RSID_SCOPE_CPU_WEIGHT_MAX).contains(&v) {
                    return Err(format!(
                        "expected integer in {}..={}",
                        RSID_SCOPE_CPU_WEIGHT_MIN, RSID_SCOPE_CPU_WEIGHT_MAX
                    ));
                }
                self.rsid_scope_cpu_weight.store(v, Ordering::Relaxed);
                Ok(true)
            }
            "worker_scope_memory_high_mib" => {
                let v = bounded_u64(
                    value,
                    RSID_SCOPE_MEMORY_LIMIT_MIB_MIN,
                    RSID_SCOPE_MEMORY_LIMIT_MIB_MAX,
                )?;
                if v >= self.worker_scope_memory_max_mib.load(Ordering::Relaxed) {
                    return Err("MemoryHigh must be below MemoryMax".to_string());
                }
                self.worker_scope_memory_high_mib
                    .store(v, Ordering::Relaxed);
                Ok(true)
            }
            "worker_scope_memory_max_mib" => {
                let v = bounded_u64(
                    value,
                    RSID_SCOPE_MEMORY_LIMIT_MIB_MIN,
                    RSID_SCOPE_MEMORY_LIMIT_MIB_MAX,
                )?;
                if v <= self.worker_scope_memory_high_mib.load(Ordering::Relaxed) {
                    return Err("MemoryMax must be above MemoryHigh".to_string());
                }
                self.worker_scope_memory_max_mib.store(v, Ordering::Relaxed);
                Ok(true)
            }
            "worker_scope_memory_swap_max_mib" => {
                let v = bounded_u64(value, 0, RSID_SCOPE_MEMORY_LIMIT_MIB_MAX)?;
                self.worker_scope_memory_swap_max_mib
                    .store(v, Ordering::Relaxed);
                Ok(true)
            }
            "worker_scope_cpu_weight" => {
                let v = value.as_u64().ok_or("expected unsigned integer")?;
                let v = u32::try_from(v).map_err(|_| "value exceeds u32::MAX")?;
                if !(RSID_SCOPE_CPU_WEIGHT_MIN..=RSID_SCOPE_CPU_WEIGHT_MAX).contains(&v) {
                    return Err(format!(
                        "expected integer in {}..={}",
                        RSID_SCOPE_CPU_WEIGHT_MIN, RSID_SCOPE_CPU_WEIGHT_MAX
                    ));
                }
                self.worker_scope_cpu_weight.store(v, Ordering::Relaxed);
                Ok(true)
            }
            "recursive_dag_recovery_controls_enabled" => {
                self.recursive_dag_recovery_controls_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "recursive_dag_scheduler_controls_enabled" => {
                self.recursive_dag_scheduler_controls_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "recursive_dag_cancellation_controls_enabled" => {
                self.recursive_dag_cancellation_controls_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "recursive_dag_live_scheduler_control_enabled" => {
                self.recursive_dag_live_scheduler_control_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "gv_render_recursive_origin" => {
                self.gv_render_recursive_origin
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "gv_info_dashboard" => {
                self.gv_info_dashboard
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "topology_executor_enabled" => {
                self.topology_executor_enabled
                    .store(value.as_bool().ok_or("expected bool")?, Ordering::Relaxed);
                Ok(true)
            }
            "topology_max_concurrent_build_nodes" => {
                let value = value.as_u64().ok_or("expected number")?;
                if !(1..=16).contains(&value) {
                    return Err("expected build node concurrency in 1..=16".to_string());
                }
                let value = u32::try_from(value).map_err(|error| error.to_string())?;
                self.topology_max_concurrent_build_nodes
                    .store(value, Ordering::Relaxed);
                Ok(true)
            }
            "topology_bulk_fanout_min_openrouter" => {
                let value = value.as_u64().ok_or("expected number")?;
                let max =
                    u64::from(crate::store_support::config_types::BULK_FANOUT_MIN_OPENROUTER_MAX);
                if value == 1 || value > max {
                    return Err(format!("expected 0 (off) or a fan-out width in 2..={max}"));
                }
                let value = u32::try_from(value).map_err(|error| error.to_string())?;
                self.topology_bulk_fanout_min_openrouter
                    .store(value, Ordering::Relaxed);
                Ok(true)
            }
            "recursive_dag_run_lease_ttl_ms" => {
                let v = value.as_u64().ok_or("expected number")?;
                if v == 0 {
                    return Err("expected positive lease ttl milliseconds".to_string());
                }
                self.recursive_dag_run_lease_ttl_ms
                    .store(v, Ordering::Relaxed);
                Ok(true)
            }
            "recursive_dag_max_concurrent_graphs" => {
                let v = value.as_u64().ok_or("expected number")?;
                if v == 0 {
                    return Err("expected positive max concurrent graph count".to_string());
                }
                let v_u32 = u32::try_from(v).map_err(|_| "value exceeds u32::MAX")?;
                self.recursive_dag_max_concurrent_graphs
                    .store(v_u32, Ordering::Relaxed);
                Ok(true)
            }
            "recursive_dag_fixed_fake_executor_only" => {
                let enabled = value.as_bool().ok_or("expected bool")?;
                if enabled {
                    Ok(true)
                } else {
                    Err("recursive DAG execution is fake-only in Phase 5A.5".to_string())
                }
            }
            "recursive_dag_fixed_live_executor_enabled" => {
                let enabled = value.as_bool().ok_or("expected bool")?;
                if !enabled {
                    Ok(true)
                } else {
                    Err("live recursive DAG execution is not available in Phase 5A.5".to_string())
                }
            }
            "recursive_dag_fixed_background_loop_enabled" => {
                let enabled = value.as_bool().ok_or("expected bool")?;
                if !enabled {
                    Ok(true)
                } else {
                    Err(
                        "recursive DAG background scheduling is not available in Phase 5A.5"
                            .to_string(),
                    )
                }
            }
            _ => Ok(false),
        }
    }
}

/// Env fields shared by both the `linear` and `local` issue-tracker `kind`s
/// (see `Config::issue_tracker_common_fields`).
struct IssueTrackerCommonFields {
    project_id: Option<uuid::Uuid>,
    provider: rsi_common::types::SessionProvider,
    model: Option<String>,
    poll_interval_ms: u64,
    active_states: Vec<String>,
    max_concurrent: usize,
    completion_state: Option<String>,
}

impl Config {
    /// Copy persisted runtime settings back into the startup `Config` snapshot.
    ///
    /// Some services are created conditionally at daemon boot from `Config`
    /// rather than from the live `RuntimeConfig`; after SQLite-backed settings
    /// are applied to `RuntimeConfig`, this keeps boot-time gates aligned with
    /// the durable user settings.
    pub fn apply_runtime_config_snapshot(&mut self, runtime_config: &RuntimeConfig) {
        self.retry_max_default = runtime_config.retry_max_default.load(Ordering::Relaxed);
        self.retry_on_stall = runtime_config.retry_on_stall.load(Ordering::Relaxed);
        self.retry_max_backoff_ms = runtime_config.retry_max_backoff_ms.load(Ordering::Relaxed);
        self.reconciliation_enabled = runtime_config
            .reconciliation_enabled
            .load(Ordering::Relaxed);
        self.stall_detection_enabled = runtime_config
            .stall_detection_enabled
            .load(Ordering::Relaxed);
        self.context_rotation_enabled = runtime_config
            .context_rotation_enabled
            .load(Ordering::Relaxed);
        self.memory_enabled = runtime_config.memory_enabled.load(Ordering::Relaxed);
        self.queue_enabled = runtime_config.queue_enabled.load(Ordering::Relaxed);
        self.dream_enabled = runtime_config.dream_enabled.load(Ordering::Relaxed);
        self.dialectic_enabled = runtime_config.dialectic_enabled.load(Ordering::Relaxed);
        self.dream_model = Some(runtime_config.dream_model.read().clone());
        self.dream_api_url = runtime_config.dream_model_base_url.read().clone();
        self.dream_observation_threshold = runtime_config
            .dream_observation_threshold
            .load(Ordering::Relaxed);
        self.dream_idle_secs = runtime_config.dream_idle_secs.load(Ordering::Relaxed);
        self.dream_cooldown_secs = runtime_config.dream_cooldown_secs.load(Ordering::Relaxed);
        self.codex_sandbox_mode = runtime_config.codex_sandbox_mode.read().clone();
        self.claude_config_isolation = runtime_config.claude_config_isolation.read().clone();
        self.stall_classifier_enabled = runtime_config
            .stall_classifier_enabled
            .load(Ordering::Relaxed);
        self.stall_classifier_model = runtime_config.stall_classifier_model.read().clone();
        self.stall_classifier_idle_secs = runtime_config
            .stall_classifier_idle_secs
            .load(Ordering::Relaxed);
        self.stall_classifier_idle_secs_codex = runtime_config
            .stall_classifier_idle_secs_codex
            .load(Ordering::Relaxed);
        self.stall_classifier_cooldown_secs = runtime_config
            .stall_classifier_cooldown_secs
            .load(Ordering::Relaxed);
        self.stall_classifier_max_per_session = runtime_config
            .stall_classifier_max_per_session
            .load(Ordering::Relaxed);
        self.stall_classifier_confidence_floor =
            *runtime_config.stall_classifier_confidence_floor.read();
        self.recursive_dag_recovery_controls_enabled = runtime_config
            .recursive_dag_recovery_controls_enabled
            .load(Ordering::Relaxed);
        self.recursive_dag_scheduler_controls_enabled = runtime_config
            .recursive_dag_scheduler_controls_enabled
            .load(Ordering::Relaxed);
        self.recursive_dag_cancellation_controls_enabled = runtime_config
            .recursive_dag_cancellation_controls_enabled
            .load(Ordering::Relaxed);
        self.recursive_dag_live_scheduler_control_enabled = runtime_config
            .recursive_dag_live_scheduler_control_enabled
            .load(Ordering::Relaxed);
        self.gv_render_recursive_origin = runtime_config
            .gv_render_recursive_origin
            .load(Ordering::Relaxed);
        self.gv_info_dashboard = runtime_config.gv_info_dashboard.load(Ordering::Relaxed);
        self.topology_executor_enabled = runtime_config
            .topology_executor_enabled
            .load(Ordering::Relaxed);
        self.topology_max_concurrent_build_nodes = runtime_config
            .topology_max_concurrent_build_nodes
            .load(Ordering::Relaxed);
        self.topology_bulk_fanout_min_openrouter = runtime_config
            .topology_bulk_fanout_min_openrouter
            .load(Ordering::Relaxed);
        self.recursive_dag_run_lease_ttl_ms = runtime_config
            .recursive_dag_run_lease_ttl_ms
            .load(Ordering::Relaxed);
        self.recursive_dag_max_concurrent_graphs = runtime_config
            .recursive_dag_max_concurrent_graphs
            .load(Ordering::Relaxed);
    }

    /// Create config from environment variables with sensible defaults.
    ///
    /// Environment variables (legacy `MOTHERSHIP_*` and `FLYWHEEL_*` honored as fallbacks):
    /// - `RSI_SOCKET`: Override socket path (useful for testing)
    /// - `RSI_EVENT_BUFFER`: Override event buffer size
    /// - `RSI_CONTEXT_ROTATION_ENABLED`: Enable context rotation (`1`/`true`/`yes`/`on`)
    pub fn from_env() -> Self {
        let socket_path = rsi_common::identity::default_socket_path();

        let event_buffer_size = env_var_legacy!("EVENT_BUFFER")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100);

        let context_rotation_enabled = env_var_legacy!("CONTEXT_ROTATION_ENABLED")
            .ok()
            .map(|s| matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);

        let memory_enabled = env_var_legacy!("MEMORY_ENABLED")
            .ok()
            .map(|s| {
                !matches!(
                    s.to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            })
            .unwrap_or(true); // default: enabled

        let memory_dir = env_var_legacy!("MEMORY_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| rsi_common::identity::data_path("memory", "memory"));

        // qwen3-embedding:0.6b is 1024-dim (nomic-embed-text was 768). Changing
        // this fires ReindexTrigger::ModelChanged, and MemoryStore recreates the
        // vec0 tables at the new width — see MemoryStore::ensure_vector_table.
        let memory_embedding_model = env_var_legacy!("MEMORY_EMBEDDING_MODEL")
            .unwrap_or_else(|_| "qwen3-embedding:0.6b".to_string());

        let memory_embedding_url = env_var_legacy!("MEMORY_EMBEDDING_URL").ok();
        let memory_embedding_api_key = env_var_legacy!("MEMORY_EMBEDDING_API_KEY").ok();

        let stall_timeout_running_secs = env_var_legacy!("STALL_TIMEOUT_RUNNING_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1800);

        let stall_timeout_waiting_secs = env_var_legacy!("STALL_TIMEOUT_WAITING_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3600);

        let stall_detection_enabled = env_var_legacy!("STALL_DETECTION_ENABLED")
            .ok()
            .map(|s| {
                !matches!(
                    s.to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            })
            .unwrap_or(true);

        let build_model = env_var_legacy!("BUILD_MODEL").ok();
        let invoke_model = env_var_legacy!("INVOKE_MODEL").ok();

        let queue_enabled = env_var_legacy!("QUEUE_ENABLED")
            .ok()
            .map(|s| {
                !matches!(
                    s.to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            })
            .unwrap_or(true);

        let queue_poll_interval_secs = env_var_legacy!("QUEUE_POLL_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);

        let queue_token_threshold = env_var_legacy!("QUEUE_TOKEN_THRESHOLD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1024);

        let workspace_roots: Vec<PathBuf> = env_var_legacy!("WORKSPACE_ROOTS")
            .ok()
            .map(|s| {
                s.split(',')
                    .filter(|p| !p.trim().is_empty())
                    .filter_map(|p| {
                        let path = PathBuf::from(p.trim());
                        match path.canonicalize() {
                            Ok(canon) => Some(canon),
                            Err(e) => {
                                eprintln!(
                                    "Warning: workspace root '{}' is not accessible: {}. Skipping.",
                                    p.trim(),
                                    e
                                );
                                None
                            }
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let reconciliation_enabled = env_var_legacy!("RECONCILIATION_ENABLED")
            .ok()
            .map(|s| {
                !matches!(
                    s.to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            })
            .unwrap_or(true);

        let reconciliation_liveness_interval_secs = env_var_legacy!("RECONCILIATION_LIVENESS_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(120);

        let reconciliation_consistency_interval_secs =
            env_var_legacy!("RECONCILIATION_CONSISTENCY_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(600);

        let reconciliation_stall_action_standard =
            env_var_legacy!("RECONCILIATION_STALL_ACTION").unwrap_or_else(|_| "notify".to_string());

        let reconciliation_stall_action_unattended =
            env_var_legacy!("RECONCILIATION_STALL_ACTION_UNATTENDED")
                .unwrap_or_else(|_| "interrupt_and_retry".to_string());

        let retry_max_default = env_var_legacy!("RETRY_MAX_DEFAULT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let retry_max_backoff_ms = env_var_legacy!("RETRY_MAX_BACKOFF_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(120_000);

        let retry_on_stall = env_var_legacy!("RETRY_ON_STALL")
            .ok()
            .map(|s| matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(true);

        let smoke_suppress_retry_restore = env_var_legacy!("SMOKE_SUPPRESS_RETRY_RESTORE")
            .ok()
            .map(|s| parse_bool_env("RSI_SMOKE_SUPPRESS_RETRY_RESTORE", s))
            .unwrap_or(false);

        let dream_enabled = env_var_legacy!("DREAM_ENABLED")
            .ok()
            .map(|s| parse_bool_env("RSI_DREAM_ENABLED", s))
            .unwrap_or(false);

        let dream_observation_threshold = env_var_legacy!("DREAM_OBSERVATION_THRESHOLD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(50);

        let dream_idle_secs = env_var_legacy!("DREAM_IDLE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3600);

        let dream_cooldown_secs = env_var_legacy!("DREAM_COOLDOWN_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(28800);

        let dream_model = env_var_legacy!("DREAM_MODEL").ok();
        let dream_api_url = env_var_legacy!("DREAM_API_URL").ok();
        let dream_api_key = env_var_legacy!("DREAM_API_KEY")
            .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
            .ok();

        let dream_batch_size = env_var_legacy!("DREAM_BATCH_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20);

        let dialectic_enabled = env_var_legacy!("DIALECTIC_ENABLED")
            .ok()
            .map(|s| {
                !matches!(
                    s.to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            })
            .unwrap_or(true);

        let dialectic_api_url = env_var_legacy!("DIALECTIC_URL")
            .unwrap_or_else(|_| "http://localhost:11434/v1".to_string());

        let dialectic_api_key = env_var_legacy!("DIALECTIC_KEY").ok();

        let dialectic_model =
            env_var_legacy!("DIALECTIC_MODEL").unwrap_or_else(|_| "qwen2.5:14b".to_string());

        let dialectic_max_iterations = env_var_legacy!("DIALECTIC_MAX_ITERATIONS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8);

        let scheduler_enabled = env_var_legacy!("SCHEDULER_ENABLED")
            .ok()
            .map(|s| {
                !matches!(
                    s.to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            })
            .unwrap_or(true);

        let scheduler_poll_interval_secs = env_var_legacy!("SCHEDULER_POLL_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60);

        let sandbox_base = env_var_legacy!("SANDBOX_BASE")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|h| h.join(".rsi/sandboxes"))
                    .unwrap_or_else(|| PathBuf::from("/tmp/rsi-sandboxes"))
            });

        let codex_sandbox_mode = env_var_legacy!("CODEX_SANDBOX_MODE")
            .ok()
            .map(|raw| {
                match crate::store_support::provider_settings::CodexSandboxMode::parse(&raw) {
                    Some(mode) => mode.cli_arg().to_string(),
                    None => {
                        tracing::warn!(
                            env_var = "RSI_CODEX_SANDBOX_MODE",
                            value = %raw,
                            "Invalid Codex sandbox mode; defaulting to danger-full-access"
                        );
                        "danger-full-access".to_string()
                    }
                }
            })
            .unwrap_or_else(|| "danger-full-access".to_string());

        // SECURITY: defaults to "off" so an operator who sets nothing gets the
        // exact pre-feature Claude launch argv.
        let claude_config_isolation = env_var_legacy!("CLAUDE_CONFIG_ISOLATION")
            .ok()
            .map(
                |raw| match crate::store_support::provider_settings::ClaudeConfigIsolation::parse(
                    &raw,
                ) {
                    Some(mode) => mode.cli_arg().to_string(),
                    None => {
                        tracing::warn!(
                            env_var = "RSI_CLAUDE_CONFIG_ISOLATION",
                            value = %raw,
                            "Invalid Claude config isolation policy; defaulting to off"
                        );
                        "off".to_string()
                    }
                },
            )
            .unwrap_or_else(|| "off".to_string());

        // Stall classifier knobs (opt-in; default off so daemon behavior is
        // unchanged from pre-RSI-0XX builds).
        let stall_classifier_enabled = env_var_legacy!("STALL_CLASSIFIER_ENABLED")
            .ok()
            .map(|s| matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);

        let recursive_dag_startup_recovery_max_graphs =
            env_var_legacy!("RECURSIVE_DAG_STARTUP_RECOVERY_MAX_GRAPHS")
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(
                    crate::store::recursive_dag::DEFAULT_RECURSIVE_STARTUP_RECOVERY_MAX_GRAPHS,
                );

        let recursive_dag_startup_recovery_time_budget_ms =
            env_var_legacy!("RECURSIVE_DAG_STARTUP_RECOVERY_TIME_BUDGET_MS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(
                    crate::store::recursive_dag::DEFAULT_RECURSIVE_STARTUP_RECOVERY_TIME_BUDGET_MS,
                );

        let recursive_dag_all_controls_enabled = env_var_legacy!("RECURSIVE_DAG_CONTROLS_ENABLED")
            .ok()
            .map(|s| parse_recursive_dag_bool_env("RSI_RECURSIVE_DAG_CONTROLS_ENABLED", s))
            .unwrap_or(false);

        let recursive_dag_recovery_controls_enabled =
            env_var_legacy!("RECURSIVE_DAG_RECOVERY_CONTROLS_ENABLED")
                .ok()
                .map(|s| {
                    parse_recursive_dag_bool_env("RSI_RECURSIVE_DAG_RECOVERY_CONTROLS_ENABLED", s)
                })
                .unwrap_or(recursive_dag_all_controls_enabled);

        let recursive_dag_scheduler_controls_enabled =
            env_var_legacy!("RECURSIVE_DAG_SCHEDULER_CONTROLS_ENABLED")
                .ok()
                .map(|s| {
                    parse_recursive_dag_bool_env("RSI_RECURSIVE_DAG_SCHEDULER_CONTROLS_ENABLED", s)
                })
                .unwrap_or(recursive_dag_all_controls_enabled);

        let recursive_dag_cancellation_controls_enabled =
            env_var_legacy!("RECURSIVE_DAG_CANCELLATION_CONTROLS_ENABLED")
                .ok()
                .map(|s| {
                    parse_recursive_dag_bool_env(
                        "RSI_RECURSIVE_DAG_CANCELLATION_CONTROLS_ENABLED",
                        s,
                    )
                })
                .unwrap_or(recursive_dag_all_controls_enabled);

        let recursive_dag_live_scheduler_control_enabled =
            env_var_legacy!("RECURSIVE_DAG_LIVE_SCHEDULER_CONTROL_ENABLED")
                .ok()
                .map(|value| {
                    parse_recursive_dag_bool_env(
                        "RSI_RECURSIVE_DAG_LIVE_SCHEDULER_CONTROL_ENABLED",
                        value,
                    )
                })
                .unwrap_or(false);

        let gv_render_recursive_origin = env_var_legacy!("GV_RENDER_RECURSIVE_ORIGIN")
            .ok()
            .map(|value| parse_recursive_dag_bool_env("RSI_GV_RENDER_RECURSIVE_ORIGIN", value))
            .unwrap_or(false);

        let gv_info_dashboard = env_var_legacy!("GV_INFO_DASHBOARD")
            .ok()
            .map(|value| parse_recursive_dag_bool_env("RSI_GV_INFO_DASHBOARD", value))
            .unwrap_or(false);

        let topology_executor_enabled = env_var_legacy!("TOPOLOGY_EXECUTOR_ENABLED")
            .ok()
            .is_none_or(|value| {
                parse_recursive_dag_bool_env("RSI_TOPOLOGY_EXECUTOR_ENABLED", value)
            });
        let topology_max_concurrent_build_nodes = 2;
        let topology_bulk_fanout_min_openrouter =
            crate::store_support::config_types::DEFAULT_BULK_FANOUT_MIN_OPENROUTER;

        let recursive_dag_run_lease_ttl_ms = env_var_legacy!("RECURSIVE_DAG_RUN_LEASE_TTL_MS")
            .ok()
            .map(|s| parse_positive_recursive_dag_u64_env("RSI_RECURSIVE_DAG_RUN_LEASE_TTL_MS", s))
            .unwrap_or(
                (crate::store::recursive_dag::DEFAULT_RECURSIVE_SCHEDULER_LEASE_TTL_SECONDS as u64)
                    * 1000,
            );

        let recursive_dag_max_concurrent_graphs =
            env_var_legacy!("RECURSIVE_DAG_MAX_CONCURRENT_GRAPHS")
                .ok()
                .map(|s| {
                    parse_positive_recursive_dag_u32_env(
                        "RSI_RECURSIVE_DAG_MAX_CONCURRENT_GRAPHS",
                        s,
                    )
                })
                .unwrap_or(
                    crate::store::recursive_dag::DEFAULT_RECURSIVE_SCHEDULER_MAX_ACTIVE_RUNS,
                );

        if let Ok(value) = env_var_legacy!("RECURSIVE_DAG_FAKE_EXECUTOR_ONLY") {
            let enabled =
                parse_recursive_dag_bool_env("RSI_RECURSIVE_DAG_FAKE_EXECUTOR_ONLY", value);
            if !enabled {
                panic!("recursive DAG execution is fake-only in Phase 5A.5");
            }
        }
        if let Ok(value) = env_var_legacy!("RECURSIVE_DAG_LIVE_EXECUTOR_ENABLED") {
            let enabled =
                parse_recursive_dag_bool_env("RSI_RECURSIVE_DAG_LIVE_EXECUTOR_ENABLED", value);
            if enabled {
                panic!(
                    "standalone recursive DAG live executor is not available; use RSI_RECURSIVE_DAG_LIVE_SCHEDULER_CONTROL_ENABLED for explicit ordinary live scheduler reachability"
                );
            }
        }
        if let Ok(value) = env_var_legacy!("RECURSIVE_DAG_BACKGROUND_LOOP_ENABLED") {
            let enabled =
                parse_recursive_dag_bool_env("RSI_RECURSIVE_DAG_BACKGROUND_LOOP_ENABLED", value);
            if enabled {
                panic!("recursive DAG background scheduling is not available in Phase 5A.5");
            }
        }

        let stall_classifier_model =
            env_var_legacy!("STALL_CLASSIFIER_MODEL").unwrap_or_else(|_| "qwen2.5:7b".to_string());

        let stall_classifier_api_url = env_var_legacy!("STALL_CLASSIFIER_API_URL")
            .unwrap_or_else(|_| "http://localhost:11434/v1/chat/completions".to_string());

        let stall_classifier_api_key = env_var_legacy!("STALL_CLASSIFIER_API_KEY")
            .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
            .ok();

        let stall_classifier_idle_secs = env_var_legacy!("STALL_CLASSIFIER_IDLE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(600);

        let stall_classifier_idle_secs_codex = env_var_legacy!("STALL_CLASSIFIER_IDLE_SECS_CODEX")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1800);

        let stall_classifier_cooldown_secs = env_var_legacy!("STALL_CLASSIFIER_COOLDOWN_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1800);

        let stall_classifier_max_per_session = env_var_legacy!("STALL_CLASSIFIER_MAX_PER_SESSION")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3u32);

        let stall_classifier_confidence_floor =
            env_var_legacy!("STALL_CLASSIFIER_CONFIDENCE_FLOOR")
                .ok()
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|v| (0.0..=1.0).contains(v))
                .unwrap_or(0.7);

        let stall_classifier_timeout_secs = env_var_legacy!("STALL_CLASSIFIER_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);

        Self {
            socket_path,
            event_buffer_size,
            context_rotation_enabled,
            memory_enabled,
            memory_dir,
            memory_embedding_model,
            memory_embedding_url,
            memory_embedding_api_key,
            stall_timeout_running_secs,
            stall_timeout_waiting_secs,
            stall_detection_enabled,
            build_model,
            invoke_model,
            queue_enabled,
            queue_poll_interval_secs,
            queue_token_threshold,
            workspace_roots,
            reconciliation_enabled,
            reconciliation_liveness_interval_secs,
            reconciliation_consistency_interval_secs,
            reconciliation_stall_action_standard,
            reconciliation_stall_action_unattended,
            retry_max_default,
            retry_max_backoff_ms,
            retry_on_stall,
            smoke_suppress_retry_restore,
            dream_enabled,
            dream_observation_threshold,
            dream_idle_secs,
            dream_cooldown_secs,
            dream_model,
            dream_api_url,
            dream_api_key,
            dream_batch_size,
            dialectic_enabled,
            dialectic_api_url,
            dialectic_api_key,
            dialectic_model,
            dialectic_max_iterations,
            scheduler_enabled,
            scheduler_poll_interval_secs,
            sandbox_base,
            codex_sandbox_mode,
            claude_config_isolation,
            stall_classifier_enabled,
            recursive_dag_startup_recovery_max_graphs,
            recursive_dag_startup_recovery_time_budget_ms,
            recursive_dag_recovery_controls_enabled,
            recursive_dag_scheduler_controls_enabled,
            recursive_dag_cancellation_controls_enabled,
            recursive_dag_live_scheduler_control_enabled,
            gv_render_recursive_origin,
            gv_info_dashboard,
            topology_executor_enabled,
            topology_max_concurrent_build_nodes,
            topology_bulk_fanout_min_openrouter,
            recursive_dag_run_lease_ttl_ms,
            recursive_dag_max_concurrent_graphs,
            stall_classifier_model,
            stall_classifier_api_url,
            stall_classifier_api_key,
            stall_classifier_idle_secs,
            stall_classifier_idle_secs_codex,
            stall_classifier_cooldown_secs,
            stall_classifier_max_per_session,
            stall_classifier_confidence_floor,
            stall_classifier_timeout_secs,
        }
    }

    /// Build a MemoryConfig from this daemon config. Returns None if memory is disabled.
    pub fn memory_config(&self) -> Option<MemoryConfig> {
        if !self.memory_enabled {
            return None;
        }

        let db_path = self
            .memory_dir
            .parent()
            .map(|p| p.join("memory.sqlite"))
            .unwrap_or_else(|| self.memory_dir.join("memory.sqlite"));

        Some(MemoryConfig {
            enabled: true,
            memory_dir: self.memory_dir.clone(),
            db_path,
            embedding_model: self.memory_embedding_model.clone(),
            embedding_url: self.memory_embedding_url.clone(),
            embedding_api_key: self.memory_embedding_api_key.clone(),
            ..Default::default()
        })
    }

    /// Build an IssueTrackerConfig from environment variables.
    ///
    /// The `kind` selector (`RSI_ISSUE_TRACKER_KIND`, unset = `"linear"`)
    /// picks the backend: `"linear"` (or unset) needs `LINEAR_API_KEY` +
    /// `LINEAR_TEAM_ID` + `ISSUE_TRACKER_WORKING_DIR` (today's gate,
    /// unchanged); `"local"` also requires a valid project UUID; any
    /// other value disables the tracker (`None`). Selection is exclusive —
    /// the manager holds a single `Box<dyn Tracker>`.
    pub fn issue_tracker_config(
        &self,
    ) -> Option<crate::store_support::issue_tracker::IssueTrackerConfig> {
        let kind = env_var_legacy!("ISSUE_TRACKER_KIND")
            .ok()
            .unwrap_or_else(|| "linear".to_string());

        match kind.as_str() {
            "local" => Self::issue_tracker_config_local(),
            "linear" => Self::issue_tracker_config_linear(),
            other => {
                tracing::warn!(
                    kind = %other,
                    "Unknown RSI_ISSUE_TRACKER_KIND value; issue tracker disabled"
                );
                None
            }
        }
    }

    /// Env fields shared by every `kind` (generic, non-Linear-specific).
    fn issue_tracker_common_fields() -> IssueTrackerCommonFields {
        let project_id = env_var_legacy!("ISSUE_TRACKER_PROJECT_ID")
            .ok()
            .and_then(|s| uuid::Uuid::parse_str(&s).ok());

        let provider = env_var_legacy!("ISSUE_TRACKER_PROVIDER")
            .ok()
            .and_then(|s| match s.to_ascii_lowercase().as_str() {
                "claude" => Some(rsi_common::types::SessionProvider::Claude),
                "codex" => Some(rsi_common::types::SessionProvider::Codex),
                "pioneer" => Some(rsi_common::types::SessionProvider::Pioneer),
                "openrouter" => Some(rsi_common::types::SessionProvider::OpenRouter),
                "bedrock" => Some(rsi_common::types::SessionProvider::Bedrock),
                "local" => Some(rsi_common::types::SessionProvider::Local),
                "gemini" | "antigravity" | "agy" => {
                    Some(rsi_common::types::SessionProvider::Antigravity)
                }
                _ => None,
            })
            .unwrap_or(rsi_common::types::SessionProvider::Claude);

        let model = env_var_legacy!("ISSUE_TRACKER_MODEL").ok();

        let poll_interval_ms = env_var_legacy!("ISSUE_TRACKER_POLL_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30_000);

        let active_states = env_var_legacy!("ISSUE_TRACKER_ACTIVE_STATES")
            .ok()
            .map(|s| s.split(',').map(|v| v.trim().to_string()).collect())
            .unwrap_or_else(|| vec!["started".to_string(), "unstarted".to_string()]);

        let max_concurrent = env_var_legacy!("ISSUE_TRACKER_MAX_CONCURRENT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);

        let completion_state = env_var_legacy!("ISSUE_TRACKER_COMPLETION_STATE").ok();

        IssueTrackerCommonFields {
            project_id,
            provider,
            model,
            poll_interval_ms,
            active_states,
            max_concurrent,
            completion_state,
        }
    }

    /// Today's Linear gate, byte-identical to the pre-C2 behavior: `Some`
    /// only when both `LINEAR_API_KEY` and `LINEAR_TEAM_ID` are set along
    /// with a working directory for dispatched sessions.
    fn issue_tracker_config_linear()
    -> Option<crate::store_support::issue_tracker::IssueTrackerConfig> {
        let api_key = env_var_legacy!("LINEAR_API_KEY").ok()?;
        let team_id = env_var_legacy!("LINEAR_TEAM_ID").ok()?;
        let working_dir_str = env_var_legacy!("ISSUE_TRACKER_WORKING_DIR").ok()?;
        let working_dir = std::path::PathBuf::from(working_dir_str);

        if api_key.is_empty() || team_id.is_empty() {
            return None;
        }

        let assignee = env_var_legacy!("LINEAR_ASSIGNEE").ok();
        let common = Self::issue_tracker_common_fields();

        Some(crate::store_support::issue_tracker::IssueTrackerConfig {
            enabled: true,
            kind: "linear".to_string(),
            api_key,
            team_id,
            assignee,
            working_dir,
            project_id: common.project_id,
            provider: common.provider,
            model: common.model,
            poll_interval_ms: common.poll_interval_ms,
            active_states: common.active_states,
            max_concurrent: common.max_concurrent,
            max_retries: 3,
            stall_timeout_ms: 300_000,
            max_turns: None,
            completion_state: common.completion_state,
        })
    }

    /// `kind=local`: only `ISSUE_TRACKER_WORKING_DIR` is required;
    /// `api_key`/`team_id` are empty strings (no Linear creds needed).
    /// `assignee` is always `None` — `LocalTracker` ignores it (D-C2-3,
    /// single-user store), so there is no point parsing `LINEAR_ASSIGNEE`
    /// for a local-only setup.
    fn issue_tracker_config_local()
    -> Option<crate::store_support::issue_tracker::IssueTrackerConfig> {
        let working_dir_str = env_var_legacy!("ISSUE_TRACKER_WORKING_DIR").ok()?;
        let working_dir = std::path::PathBuf::from(working_dir_str);

        let common = Self::issue_tracker_common_fields();
        let project_id = match common.project_id {
            Some(project_id) => project_id,
            None => {
                tracing::warn!(
                    "Local issue tracker disabled: RSI_ISSUE_TRACKER_PROJECT_ID is missing or not a UUID"
                );
                return None;
            }
        };

        Some(crate::store_support::issue_tracker::IssueTrackerConfig {
            enabled: true,
            kind: "local".to_string(),
            api_key: String::new(),
            team_id: String::new(),
            assignee: None,
            working_dir,
            project_id: Some(project_id),
            provider: common.provider,
            model: common.model,
            poll_interval_ms: common.poll_interval_ms,
            active_states: common.active_states,
            max_concurrent: common.max_concurrent,
            max_retries: 3,
            stall_timeout_ms: 300_000,
            max_turns: None,
            completion_state: common.completion_state,
        })
    }

    /// Build a QueueConfig from this daemon config. Returns None if queue is disabled.
    pub fn queue_config(&self) -> Option<crate::store_support::config_types::QueueConfig> {
        if !self.queue_enabled {
            return None;
        }
        Some(crate::store_support::config_types::QueueConfig {
            poll_interval_secs: self.queue_poll_interval_secs,
            default_token_threshold: self.queue_token_threshold,
            ..Default::default()
        })
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::from_env()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn agent_build_settings_are_bounded_and_visible_to_operator() {
        let runtime = RuntimeConfig::from_config(&Config::default());
        assert_eq!(runtime.to_json()["agent_build_jobs"], 4);
        assert_eq!(runtime.to_json()["agent_build_line_tables_only"], true);
        assert_eq!(runtime.to_json()["agent_build_sccache_enabled"], true);
        assert_eq!(runtime.to_json()["agent_build_sccache_cache_gib"], 10);
        assert_eq!(runtime.to_json()["agent_build_slots"], 16);
        runtime
            .update_field("agent_build_jobs", &serde_json::json!(8))
            .unwrap();
        runtime
            .update_field("agent_build_sccache_cache_gib", &serde_json::json!(20))
            .unwrap();
        runtime
            .update_field("agent_build_slots", &serde_json::json!(24))
            .unwrap();
        assert_eq!(runtime.to_json()["agent_build_jobs"], 8);
        assert_eq!(runtime.to_json()["agent_build_sccache_cache_gib"], 20);
        assert_eq!(runtime.to_json()["agent_build_slots"], 24);
        assert!(
            runtime
                .update_field("agent_build_jobs", &serde_json::json!(0))
                .is_err()
        );
        assert!(
            runtime
                .update_field("agent_build_sccache_cache_gib", &serde_json::json!(513))
                .is_err()
        );
        assert!(
            runtime
                .update_field("agent_build_slots", &serde_json::json!(0))
                .is_err()
        );
        assert!(
            runtime
                .update_field("agent_build_slots", &serde_json::json!(65))
                .is_err()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn rotation_threshold_precedence_and_validation() {
        let config = RuntimeConfig::from_config(&Config::from_env());
        assert_eq!(
            config.context_rotation_threshold_pct(SessionProvider::Claude),
            65.0
        );
        config
            .update_field("context_rotation_claude_pct", &serde_json::json!(72))
            .unwrap();
        config
            .update_field("context_rotation_codex_pct", &serde_json::json!(80))
            .unwrap();
        assert_eq!(
            config.context_rotation_threshold_pct(SessionProvider::Claude),
            72.0
        );
        for provider in [
            SessionProvider::Codex,
            SessionProvider::Pioneer,
            SessionProvider::CodexAppServer,
        ] {
            assert_eq!(config.context_rotation_threshold_pct(provider), 80.0);
        }
        config
            .update_field("context_rotation_global_pct", &serde_json::json!(90))
            .unwrap();
        assert_eq!(
            config.context_rotation_threshold_pct(SessionProvider::Claude),
            90.0
        );
        assert_eq!(
            config.context_rotation_threshold_pct(SessionProvider::Codex),
            90.0
        );
        assert_eq!(
            config.context_rotation_threshold_pct(SessionProvider::Local),
            65.0
        );
        for invalid in [
            serde_json::json!(0),
            serde_json::json!(100),
            serde_json::json!(65.5),
            serde_json::json!("65"),
        ] {
            assert!(
                config
                    .update_field("context_rotation_global_pct", &invalid)
                    .is_err()
            );
        }
        assert_eq!(
            config.context_rotation_threshold_pct(SessionProvider::Claude),
            90.0
        );
        config
            .update_field("context_rotation_global_pct", &serde_json::Value::Null)
            .unwrap();
        assert!(config.to_json()["context_rotation_global_pct"].is_null());
        assert_eq!(
            config.context_rotation_threshold_pct(SessionProvider::Claude),
            72.0
        );
    }

    /// T10 (rsid side): every persisted daemon field has exactly one entry in
    /// the shared `rsi_common::daemon_config_catalog`, and the catalog names
    /// no other field (positive set equality).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn persisted_fields_are_catalogued() {
        use std::collections::BTreeSet;
        let persisted: BTreeSet<&str> = PERSISTED_RUNTIME_CONFIG_FIELDS.iter().copied().collect();
        assert_eq!(persisted.len(), PERSISTED_RUNTIME_CONFIG_FIELDS.len());
        let catalogued: BTreeSet<&str> = rsi_common::daemon_config_catalog::DAEMON_CONFIG_FIELDS
            .iter()
            .map(|spec| spec.field)
            .collect();
        assert_eq!(
            catalogued.len(),
            rsi_common::daemon_config_catalog::DAEMON_CONFIG_FIELDS.len()
        );
        assert_eq!(catalogued, persisted);
    }

    /// #925: the satellite polling kill switch is a persisted daemon setting.
    /// It defaults on and round-trips false -> true through the JSON view.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn satellite_polling_enabled_is_persisted_and_round_trips() {
        assert!(PERSISTED_RUNTIME_CONFIG_FIELDS.contains(&"satellite_polling_enabled"));
        let config = RuntimeConfig::from_config(&Config::default());
        assert_eq!(config.to_json()["satellite_polling_enabled"], true);
        assert!(
            config
                .update_field("satellite_polling_enabled", &serde_json::json!(false))
                .is_ok()
        );
        assert_eq!(config.to_json()["satellite_polling_enabled"], false);
        assert_eq!(
            config.persisted_field_value("satellite_polling_enabled"),
            Some(serde_json::json!(false))
        );
        assert!(
            config
                .update_field("satellite_polling_enabled", &serde_json::json!(true))
                .is_ok()
        );
        assert_eq!(config.to_json()["satellite_polling_enabled"], true);
        assert!(
            config
                .update_field("satellite_polling_enabled", &serde_json::json!("on"))
                .is_err()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn governor_settings_default_to_cargo_slot_and_validate() {
        let config = RuntimeConfig::from_config(&Config::default());
        assert_eq!(
            config.governor_policy(),
            crate::store_support::config_types::GovernorPolicy::default()
        );
        for (field, good, bad) in [
            ("governor_build_slots", 6, 0),
            ("governor_lander_slots", 3, 17),
            ("governor_max_load", 48, 5000),
            ("governor_min_free_disk_gb", 50, 9000),
            ("governor_min_avail_mem_gb", 24, 9000),
            ("governor_max_workers_slice_gb", 36, 0),
        ] {
            assert!(PERSISTED_RUNTIME_CONFIG_FIELDS.contains(&field), "{field}");
            assert!(config.update_field(field, &serde_json::json!(good)).is_ok());
            assert_eq!(
                config.persisted_field_value(field),
                Some(serde_json::json!(good))
            );
            assert!(config.update_field(field, &serde_json::json!(bad)).is_err());
            assert_eq!(
                config.to_json()[field],
                good,
                "{field} keeps accepted value"
            );
        }
        assert_eq!(config.governor_policy().build_slots, 6);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn deploy_drain_setting_defaults_on_persists_and_validates() {
        assert!(PERSISTED_RUNTIME_CONFIG_FIELDS.contains(&"deploy_drain_enabled"));
        let config = RuntimeConfig::from_config(&Config::default());
        assert_eq!(config.to_json()["deploy_drain_enabled"], true);
        assert!(
            config
                .update_field("deploy_drain_enabled", &serde_json::json!(false))
                .is_ok()
        );
        assert_eq!(
            config.persisted_field_value("deploy_drain_enabled"),
            Some(serde_json::json!(false))
        );
        assert!(!config.deploy_drain_enabled.load(Ordering::Relaxed));
        assert!(
            config
                .update_field("deploy_drain_enabled", &serde_json::json!(1))
                .is_err()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn rolling_queue_settings_default_persist_and_validate() {
        for field in [
            "rolling_queue_enabled",
            "rolling_queue_batch_size",
            "rolling_queue_speculation_depth",
        ] {
            assert!(PERSISTED_RUNTIME_CONFIG_FIELDS.contains(&field), "{field}");
        }
        let config = RuntimeConfig::from_config(&Config::default());
        let json = config.to_json();
        assert_eq!(json["rolling_queue_enabled"], false);
        assert_eq!(json["rolling_queue_batch_size"], 4);
        assert_eq!(json["rolling_queue_speculation_depth"], 1);
        for (field, value) in [
            ("rolling_queue_enabled", serde_json::json!(true)),
            ("rolling_queue_batch_size", serde_json::json!(8)),
            ("rolling_queue_speculation_depth", serde_json::json!(0)),
        ] {
            assert!(config.update_field(field, &value).is_ok(), "{field}");
            assert_eq!(config.persisted_field_value(field), Some(value));
        }
        for (field, value) in [
            ("rolling_queue_enabled", serde_json::json!(1)),
            ("rolling_queue_batch_size", serde_json::json!(0)),
            ("rolling_queue_batch_size", serde_json::json!(9)),
            ("rolling_queue_speculation_depth", serde_json::json!(3)),
        ] {
            assert!(
                config.update_field(field, &value).is_err(),
                "{field} {value}"
            );
        }
        // A rejected write leaves the accepted value in place.
        assert_eq!(config.to_json()["rolling_queue_batch_size"], 8);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn child_autonomy_settings_default_persist_and_validate() {
        for field in [
            "program_hold_while_children_run",
            "child_keepalive_enabled",
            "child_keepalive_window_secs",
        ] {
            assert!(PERSISTED_RUNTIME_CONFIG_FIELDS.contains(&field), "{field}");
        }
        let config = RuntimeConfig::from_config(&Config::default());
        let json = config.to_json();
        assert_eq!(json["program_hold_while_children_run"], true);
        assert_eq!(json["child_keepalive_enabled"], false);
        assert_eq!(json["child_keepalive_window_secs"], 1500);
        for (field, value) in [
            ("program_hold_while_children_run", serde_json::json!(false)),
            ("child_keepalive_enabled", serde_json::json!(true)),
            ("child_keepalive_window_secs", serde_json::json!(300)),
            ("child_keepalive_window_secs", serde_json::json!(21_600)),
        ] {
            assert!(config.update_field(field, &value).is_ok(), "{field}");
            assert_eq!(config.persisted_field_value(field), Some(value));
        }
        for (field, value) in [
            ("program_hold_while_children_run", serde_json::json!("off")),
            ("child_keepalive_enabled", serde_json::json!(1)),
            ("child_keepalive_window_secs", serde_json::json!(299)),
            ("child_keepalive_window_secs", serde_json::json!(21_601)),
            ("child_keepalive_window_secs", serde_json::json!(-5)),
        ] {
            assert!(
                config.update_field(field, &value).is_err(),
                "{field} {value}"
            );
        }
        // A rejected write leaves the accepted value.
        assert_eq!(config.to_json()["child_keepalive_window_secs"], 21_600);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn harness_egress_mode_persists_validates_and_layers_into_the_policy() {
        use rsi_common::egress_policy::EgressMode;
        assert!(PERSISTED_RUNTIME_CONFIG_FIELDS.contains(&"harness_egress_mode"));
        let config = RuntimeConfig::from_config(&Config::default());
        assert_eq!(config.to_json()["harness_egress_mode"], "deny_private");
        assert_eq!(
            config.harness_tool_policy_defaults().egress_mode(),
            EgressMode::DenyPrivate
        );
        let value = serde_json::json!("offline");
        assert!(config.update_field("harness_egress_mode", &value).is_ok());
        assert_eq!(
            config.persisted_field_value("harness_egress_mode"),
            Some(value)
        );
        let defaults = config.harness_tool_policy_defaults();
        assert_eq!(defaults.egress, Some(EgressMode::Offline));
        // A session that sets nothing inherits the daemon default.
        let effective =
            rsi_common::harness_tool_policy::HarnessToolPolicy::default().or_defaults(&defaults);
        assert_eq!(effective.egress_mode(), EgressMode::Offline);
        for value in [serde_json::json!("allow"), serde_json::json!(true)] {
            assert!(
                config.update_field("harness_egress_mode", &value).is_err(),
                "{value}"
            );
        }
        assert_eq!(config.to_json()["harness_egress_mode"], "offline");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn harness_context_editing_default_persists_validates_and_layers() {
        use rsi_common::harness_tool_policy::HarnessToolPolicy;
        assert!(PERSISTED_RUNTIME_CONFIG_FIELDS.contains(&"harness_context_editing"));
        let config = RuntimeConfig::from_config(&Config::default());
        // Default on: nothing to layer, and a policy-less session is on.
        assert_eq!(config.to_json()["harness_context_editing"], true);
        let on = config.harness_tool_policy_defaults();
        assert_eq!(on.context_editing, None);
        assert!(
            HarnessToolPolicy::default()
                .or_defaults(&on)
                .context_editing_enabled()
        );
        // Off for new sessions.
        let off = serde_json::json!(false);
        assert!(config.update_field("harness_context_editing", &off).is_ok());
        assert_eq!(
            config.persisted_field_value("harness_context_editing"),
            Some(off)
        );
        let defaults = config.harness_tool_policy_defaults();
        assert_eq!(defaults.context_editing, Some(false));
        assert!(
            !HarnessToolPolicy::default()
                .or_defaults(&defaults)
                .context_editing_enabled()
        );
        // An explicit session policy wins over the daemon default, either way.
        let explicit_on = HarnessToolPolicy {
            context_editing: Some(true),
            ..HarnessToolPolicy::default()
        };
        assert!(explicit_on.or_defaults(&defaults).context_editing_enabled());
        // Only a bool is accepted, and a rejected write keeps the value.
        for value in [serde_json::json!("off"), serde_json::json!(0)] {
            assert!(
                config
                    .update_field("harness_context_editing", &value)
                    .is_err(),
                "{value}"
            );
        }
        assert_eq!(config.to_json()["harness_context_editing"], false);
        // Back on clears the default again.
        let back = serde_json::json!(true);
        assert!(
            config
                .update_field("harness_context_editing", &back)
                .is_ok()
        );
        assert_eq!(config.harness_tool_policy_defaults().context_editing, None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn harness_tool_policy_defaults_persist_validate_and_layer() {
        use rsi_common::harness_tool_policy::{HarnessToolPolicy, ToolBudgets, WebAccessMode};
        let fields = [
            "harness_web_access",
            "harness_max_search_calls",
            "harness_max_fetch_calls",
            "harness_max_result_bytes",
            "harness_max_web_cost_usd_micros",
        ];
        for field in fields {
            assert!(PERSISTED_RUNTIME_CONFIG_FIELDS.contains(&field), "{field}");
        }
        let config = RuntimeConfig::from_config(&Config::default());
        assert_eq!(config.to_json()["harness_web_access"], "enabled");
        assert!(config.harness_tool_policy_defaults().is_default());
        for (field, value) in [
            ("harness_web_access", serde_json::json!("disabled")),
            ("harness_max_search_calls", serde_json::json!(3)),
            ("harness_max_fetch_calls", serde_json::json!(2)),
            ("harness_max_result_bytes", serde_json::json!(4096)),
            ("harness_max_web_cost_usd_micros", serde_json::json!(50_000)),
        ] {
            assert!(config.update_field(field, &value).is_ok(), "{field}");
            assert_eq!(config.persisted_field_value(field), Some(value));
        }
        let defaults = config.harness_tool_policy_defaults();
        assert_eq!(defaults.web_access, Some(WebAccessMode::Disabled));
        assert_eq!(
            defaults.budgets,
            ToolBudgets {
                max_search_calls: Some(3),
                max_fetch_calls: Some(2),
                max_result_bytes: Some(4096),
                max_web_cost_usd_micros: Some(50_000),
            }
        );
        // A session policy overrides only what it sets.
        let session = HarnessToolPolicy {
            web_access: Some(WebAccessMode::HostedOnly),
            budgets: ToolBudgets {
                max_search_calls: Some(1),
                ..ToolBudgets::default()
            },
            ..HarnessToolPolicy::default()
        };
        let effective = session.or_defaults(&defaults);
        assert_eq!(effective.web_access, Some(WebAccessMode::HostedOnly));
        assert_eq!(effective.budgets.max_search_calls, Some(1));
        assert_eq!(effective.budgets.max_fetch_calls, Some(2));
        for (field, value) in [
            ("harness_web_access", serde_json::json!("always")),
            ("harness_web_access", serde_json::json!(1)),
            ("harness_max_search_calls", serde_json::json!(-1)),
            ("harness_max_search_calls", serde_json::json!(1_000_001)),
            ("harness_max_result_bytes", serde_json::json!("big")),
        ] {
            assert!(
                config.update_field(field, &value).is_err(),
                "{field} {value}"
            );
        }
        assert_eq!(config.to_json()["harness_max_search_calls"], 3);
        // Zero returns a cap to unlimited.
        assert!(
            config
                .update_field("harness_max_search_calls", &serde_json::json!(0))
                .is_ok()
        );
        assert_eq!(
            config
                .harness_tool_policy_defaults()
                .budgets
                .max_search_calls,
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn cloud_spend_caps_default_validate_and_persist() {
        let config = RuntimeConfig::from_config(&Config::from_env());
        assert_eq!(config.to_json()["cloud_spend_stop_line_usd"], 90);
        assert_eq!(config.to_json()["cloud_spend_daily_cap_usd"], 15);
        for field in ["cloud_spend_stop_line_usd", "cloud_spend_daily_cap_usd"] {
            assert!(is_persisted_runtime_config_field(field), "{field}");
            for bad in [
                serde_json::json!(-1),
                serde_json::json!(1.5),
                serde_json::json!("9"),
                serde_json::json!(rsi_common::cloud_spend::MAX_CAP_USD + 1),
            ] {
                assert!(config.update_field(field, &bad).is_err(), "{field} {bad}");
            }
            assert_eq!(config.update_field(field, &serde_json::json!(7)), Ok(true));
        }
        let caps = config.cloud_spend_caps();
        assert_eq!((caps.stop_line_usd, caps.daily_cap_usd), (7, 7));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn mcp_deferred_tool_threshold_defaults_validate_and_round_trips() {
        let field = "mcp.deferred_tool_threshold";
        assert!(PERSISTED_RUNTIME_CONFIG_FIELDS.contains(&field));
        let config = RuntimeConfig::from_config(&Config::default());
        assert_eq!(config.to_json()[field], MCP_DEFERRED_TOOL_THRESHOLD_DEFAULT);
        assert_eq!(config.mcp_deferred_tool_threshold(), 32);
        for value in [0, 256] {
            assert!(
                config
                    .update_field(field, &serde_json::json!(value))
                    .is_ok()
            );
            assert_eq!(
                config.persisted_field_value(field),
                Some(serde_json::json!(value))
            );
            assert_eq!(config.to_json()[field], value);
            assert_eq!(config.mcp_deferred_tool_threshold(), value);
        }
        for invalid in [
            serde_json::json!(257),
            serde_json::json!(-1),
            serde_json::json!("32"),
            serde_json::Value::Null,
        ] {
            assert!(config.update_field(field, &invalid).is_err(), "{invalid}");
        }
        assert_eq!(config.to_json()[field], 256);
        assert_eq!(config.mcp_deferred_tool_threshold(), 256);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn session_retention_settings_validate_and_round_trip() {
        let config = RuntimeConfig::from_config(&Config::default());
        assert_eq!(config.to_json()["session_retention_enabled"], true);
        assert_eq!(config.to_json()["session_retention_window_hours"], 24);
        assert!(
            config
                .update_field("session_retention_enabled", &serde_json::json!(false))
                .is_ok()
        );
        assert!(
            config
                .update_field("session_retention_window_hours", &serde_json::json!(72))
                .is_ok()
        );
        assert_eq!(
            config.persisted_field_value("session_retention_window_hours"),
            Some(serde_json::json!(72))
        );
        for invalid in [
            serde_json::json!(0),
            serde_json::json!(8761),
            serde_json::json!("24"),
        ] {
            assert!(
                config
                    .update_field("session_retention_window_hours", &invalid)
                    .is_err()
            );
        }
        assert_eq!(config.to_json()["session_retention_window_hours"], 72);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn completed_transcript_cache_cap_accepts_zero_and_survives_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rsi.db");
        let store = crate::store::Store::open(&path).unwrap();
        let runtime = RuntimeConfig::from_config(&Config::default());
        let field = "completed_transcript_cache_max_bytes";
        assert_eq!(
            runtime.to_json()[field],
            COMPLETED_TRANSCRIPT_CACHE_MAX_BYTES_DEFAULT
        );
        assert!(runtime.update_field(field, &serde_json::json!(-1)).is_err());
        assert!(runtime.update_field(field, &serde_json::json!(0)).unwrap());
        crate::store::daemon_settings::persist_runtime_config_field(&store, &runtime, field)
            .unwrap();
        drop(store);
        let reopened = crate::store::Store::open(&path).unwrap();
        let restarted = RuntimeConfig::from_config(&Config::default());
        crate::store::daemon_settings::apply_persisted_runtime_config(&reopened, &restarted)
            .unwrap();
        assert_eq!(restarted.to_json()[field], 0);
    }

    /// Issue #692: the launch-model allowlist defaults to empty (unrestricted),
    /// validates on write, appears in `GetDaemonConfig`, and survives a daemon
    /// restart through the durable `daemon_settings` row.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn launch_model_allowlist_validates_round_trips_and_survives_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rsi.db");
        let store = crate::store::Store::open(&path).unwrap();
        let runtime = RuntimeConfig::from_config(&Config::default());
        let field = "launch_model_allowlist";
        assert_eq!(runtime.to_json()[field], serde_json::json!([]));

        for bad in [
            serde_json::json!(7),
            serde_json::json!([1]),
            serde_json::json!([""]),
            serde_json::json!({"model": "x"}),
        ] {
            runtime
                .update_field(field, &bad)
                .expect_err("a malformed allowlist must be refused");
            assert!(runtime.launch_model_allowlist.read().is_empty());
        }

        assert!(
            runtime
                .update_field(field, &serde_json::json!("gpt-6-sol, z-ai/glm-5.3-flashx"))
                .unwrap()
        );
        assert_eq!(
            runtime.to_json()[field],
            serde_json::json!(["gpt-6-sol", "z-ai/glm-5.3-flashx"])
        );
        crate::store::daemon_settings::persist_runtime_config_field(&store, &runtime, field)
            .unwrap();
        drop(store);

        let reopened = crate::store::Store::open(&path).unwrap();
        let restarted = RuntimeConfig::from_config(&Config::default());
        crate::store::daemon_settings::apply_persisted_runtime_config(&reopened, &restarted)
            .unwrap();
        assert_eq!(
            restarted.to_json()[field],
            serde_json::json!(["gpt-6-sol", "z-ai/glm-5.3-flashx"])
        );

        assert!(
            runtime
                .update_field(field, &serde_json::Value::Null)
                .unwrap()
        );
        assert_eq!(runtime.to_json()[field], serde_json::json!([]));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn codegraph_indexing_persists_through_daemon_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rsi.db");
        let store = crate::store::Store::open(&path).unwrap();
        let runtime = RuntimeConfig::from_config(&Config::default());
        runtime
            .update_field("codegraph_indexing_enabled", &serde_json::json!(true))
            .unwrap();
        assert!(
            crate::store::daemon_settings::persist_runtime_config_field(
                &store,
                &runtime,
                "codegraph_indexing_enabled",
            )
            .unwrap()
        );
        assert_eq!(
            store
                .get_daemon_setting("codegraph_indexing_enabled")
                .unwrap()
                .as_deref(),
            Some("true")
        );
        drop(store);

        let reopened = crate::store::Store::open(&path).unwrap();
        let restarted = RuntimeConfig::from_config(&Config::default());
        let shared = Arc::clone(&restarted.codegraph_indexing_enabled);
        assert_eq!(restarted.to_json()["codegraph_indexing_enabled"], false);
        crate::store::daemon_settings::apply_persisted_runtime_config(&reopened, &restarted)
            .unwrap();
        assert_eq!(restarted.to_json()["codegraph_indexing_enabled"], true);
        assert!(shared.load(Ordering::Relaxed));

        restarted
            .update_field("codegraph_indexing_enabled", &serde_json::json!(false))
            .unwrap();
        assert!(!shared.load(Ordering::Relaxed));
        crate::store::daemon_settings::persist_runtime_config_field(
            &reopened,
            &restarted,
            "codegraph_indexing_enabled",
        )
        .unwrap();
        drop(reopened);

        let reopened = crate::store::Store::open(&path).unwrap();
        let restarted = RuntimeConfig::from_config(&Config::default());
        crate::store::daemon_settings::apply_persisted_runtime_config(&reopened, &restarted)
            .unwrap();
        assert_eq!(restarted.to_json()["codegraph_indexing_enabled"], false);
    }

    // env-guard (RSI-020): every (new_name, [legacy_names]) triple this test
    // module must scrub before reading config. Names mirror the
    // `env_var_legacy!()` invocations at the top of this file (`config.rs:13-20`)
    // — each `RSI_*` name reachable via that macro has parallel `MOTHERSHIP_*`
    // and `FLYWHEEL_*` fallbacks resolved by `rsi_common::identity::env_with_legacy()`.
    // Without scrubbing the legacy parallels, parent-shell vars leak through and
    // break the tests' pristine-env assumption.
    //
    // Maintenance: when a new `env_var_legacy!("NEW_SUFFIX")` site lands, append
    // `("RSI_NEW_SUFFIX", &["MOTHERSHIP_NEW_SUFFIX", "FLYWHEEL_NEW_SUFFIX"])`
    // here. `ANTHROPIC_API_KEY` carries no legacy parallels by design.
    const ENV_TRIPLES: &[(&str, &[&str])] = &[
        ("RSI_DAEMON_SOCKET_PATH", &[]),
        ("RSI_SOCKET", &["MOTHERSHIP_SOCKET", "FLYWHEEL_SOCKET"]),
        (
            "RSI_EVENT_BUFFER",
            &["MOTHERSHIP_EVENT_BUFFER", "FLYWHEEL_EVENT_BUFFER"],
        ),
        (
            "RSI_CONTEXT_ROTATION_ENABLED",
            &[
                "MOTHERSHIP_CONTEXT_ROTATION_ENABLED",
                "FLYWHEEL_CONTEXT_ROTATION_ENABLED",
            ],
        ),
        (
            "RSI_MEMORY_ENABLED",
            &["MOTHERSHIP_MEMORY_ENABLED", "FLYWHEEL_MEMORY_ENABLED"],
        ),
        (
            "RSI_MEMORY_DIR",
            &["MOTHERSHIP_MEMORY_DIR", "FLYWHEEL_MEMORY_DIR"],
        ),
        (
            "RSI_MEMORY_EMBEDDING_MODEL",
            &[
                "MOTHERSHIP_MEMORY_EMBEDDING_MODEL",
                "FLYWHEEL_MEMORY_EMBEDDING_MODEL",
            ],
        ),
        (
            "RSI_MEMORY_EMBEDDING_URL",
            &[
                "MOTHERSHIP_MEMORY_EMBEDDING_URL",
                "FLYWHEEL_MEMORY_EMBEDDING_URL",
            ],
        ),
        (
            "RSI_MEMORY_EMBEDDING_API_KEY",
            &[
                "MOTHERSHIP_MEMORY_EMBEDDING_API_KEY",
                "FLYWHEEL_MEMORY_EMBEDDING_API_KEY",
            ],
        ),
        (
            "RSI_STALL_TIMEOUT_RUNNING_SECS",
            &[
                "MOTHERSHIP_STALL_TIMEOUT_RUNNING_SECS",
                "FLYWHEEL_STALL_TIMEOUT_RUNNING_SECS",
            ],
        ),
        (
            "RSI_STALL_TIMEOUT_WAITING_SECS",
            &[
                "MOTHERSHIP_STALL_TIMEOUT_WAITING_SECS",
                "FLYWHEEL_STALL_TIMEOUT_WAITING_SECS",
            ],
        ),
        (
            "RSI_STALL_DETECTION_ENABLED",
            &[
                "MOTHERSHIP_STALL_DETECTION_ENABLED",
                "FLYWHEEL_STALL_DETECTION_ENABLED",
            ],
        ),
        (
            "RSI_BUILD_MODEL",
            &["MOTHERSHIP_BUILD_MODEL", "FLYWHEEL_BUILD_MODEL"],
        ),
        (
            "RSI_INVOKE_MODEL",
            &["MOTHERSHIP_INVOKE_MODEL", "FLYWHEEL_INVOKE_MODEL"],
        ),
        (
            "RSI_QUEUE_ENABLED",
            &["MOTHERSHIP_QUEUE_ENABLED", "FLYWHEEL_QUEUE_ENABLED"],
        ),
        (
            "RSI_QUEUE_POLL_INTERVAL_SECS",
            &[
                "MOTHERSHIP_QUEUE_POLL_INTERVAL_SECS",
                "FLYWHEEL_QUEUE_POLL_INTERVAL_SECS",
            ],
        ),
        (
            "RSI_QUEUE_TOKEN_THRESHOLD",
            &[
                "MOTHERSHIP_QUEUE_TOKEN_THRESHOLD",
                "FLYWHEEL_QUEUE_TOKEN_THRESHOLD",
            ],
        ),
        (
            "RSI_WORKSPACE_ROOTS",
            &["MOTHERSHIP_WORKSPACE_ROOTS", "FLYWHEEL_WORKSPACE_ROOTS"],
        ),
        (
            "RSI_RECONCILIATION_ENABLED",
            &[
                "MOTHERSHIP_RECONCILIATION_ENABLED",
                "FLYWHEEL_RECONCILIATION_ENABLED",
            ],
        ),
        (
            "RSI_RECONCILIATION_LIVENESS_SECS",
            &[
                "MOTHERSHIP_RECONCILIATION_LIVENESS_SECS",
                "FLYWHEEL_RECONCILIATION_LIVENESS_SECS",
            ],
        ),
        (
            "RSI_RECONCILIATION_CONSISTENCY_SECS",
            &[
                "MOTHERSHIP_RECONCILIATION_CONSISTENCY_SECS",
                "FLYWHEEL_RECONCILIATION_CONSISTENCY_SECS",
            ],
        ),
        (
            "RSI_RECONCILIATION_STALL_ACTION",
            &[
                "MOTHERSHIP_RECONCILIATION_STALL_ACTION",
                "FLYWHEEL_RECONCILIATION_STALL_ACTION",
            ],
        ),
        (
            "RSI_RECONCILIATION_STALL_ACTION_UNATTENDED",
            &[
                "MOTHERSHIP_RECONCILIATION_STALL_ACTION_UNATTENDED",
                "FLYWHEEL_RECONCILIATION_STALL_ACTION_UNATTENDED",
            ],
        ),
        (
            "RSI_RETRY_MAX_DEFAULT",
            &["MOTHERSHIP_RETRY_MAX_DEFAULT", "FLYWHEEL_RETRY_MAX_DEFAULT"],
        ),
        (
            "RSI_RETRY_MAX_BACKOFF_MS",
            &[
                "MOTHERSHIP_RETRY_MAX_BACKOFF_MS",
                "FLYWHEEL_RETRY_MAX_BACKOFF_MS",
            ],
        ),
        (
            "RSI_RETRY_ON_STALL",
            &["MOTHERSHIP_RETRY_ON_STALL", "FLYWHEEL_RETRY_ON_STALL"],
        ),
        (
            "RSI_SMOKE_SUPPRESS_RETRY_RESTORE",
            &[
                "MOTHERSHIP_SMOKE_SUPPRESS_RETRY_RESTORE",
                "FLYWHEEL_SMOKE_SUPPRESS_RETRY_RESTORE",
            ],
        ),
        (
            "RSI_DREAM_ENABLED",
            &["MOTHERSHIP_DREAM_ENABLED", "FLYWHEEL_DREAM_ENABLED"],
        ),
        (
            "RSI_DREAM_OBSERVATION_THRESHOLD",
            &[
                "MOTHERSHIP_DREAM_OBSERVATION_THRESHOLD",
                "FLYWHEEL_DREAM_OBSERVATION_THRESHOLD",
            ],
        ),
        (
            "RSI_DREAM_IDLE_SECS",
            &["MOTHERSHIP_DREAM_IDLE_SECS", "FLYWHEEL_DREAM_IDLE_SECS"],
        ),
        (
            "RSI_DREAM_COOLDOWN_SECS",
            &[
                "MOTHERSHIP_DREAM_COOLDOWN_SECS",
                "FLYWHEEL_DREAM_COOLDOWN_SECS",
            ],
        ),
        (
            "RSI_DREAM_MODEL",
            &["MOTHERSHIP_DREAM_MODEL", "FLYWHEEL_DREAM_MODEL"],
        ),
        (
            "RSI_DREAM_API_URL",
            &["MOTHERSHIP_DREAM_API_URL", "FLYWHEEL_DREAM_API_URL"],
        ),
        (
            "RSI_DREAM_API_KEY",
            &["MOTHERSHIP_DREAM_API_KEY", "FLYWHEEL_DREAM_API_KEY"],
        ),
        (
            "RSI_DREAM_BATCH_SIZE",
            &["MOTHERSHIP_DREAM_BATCH_SIZE", "FLYWHEEL_DREAM_BATCH_SIZE"],
        ),
        ("ANTHROPIC_API_KEY", &[]),
        (
            "RSI_DIALECTIC_ENABLED",
            &["MOTHERSHIP_DIALECTIC_ENABLED", "FLYWHEEL_DIALECTIC_ENABLED"],
        ),
        (
            "RSI_DIALECTIC_URL",
            &["MOTHERSHIP_DIALECTIC_URL", "FLYWHEEL_DIALECTIC_URL"],
        ),
        (
            "RSI_DIALECTIC_KEY",
            &["MOTHERSHIP_DIALECTIC_KEY", "FLYWHEEL_DIALECTIC_KEY"],
        ),
        (
            "RSI_DIALECTIC_MODEL",
            &["MOTHERSHIP_DIALECTIC_MODEL", "FLYWHEEL_DIALECTIC_MODEL"],
        ),
        (
            "RSI_DIALECTIC_MAX_ITERATIONS",
            &[
                "MOTHERSHIP_DIALECTIC_MAX_ITERATIONS",
                "FLYWHEEL_DIALECTIC_MAX_ITERATIONS",
            ],
        ),
        (
            "RSI_CODEX_SANDBOX_MODE",
            &[
                "MOTHERSHIP_CODEX_SANDBOX_MODE",
                "FLYWHEEL_CODEX_SANDBOX_MODE",
            ],
        ),
        (
            "RSI_RECURSIVE_DAG_STARTUP_RECOVERY_MAX_GRAPHS",
            &[
                "MOTHERSHIP_RECURSIVE_DAG_STARTUP_RECOVERY_MAX_GRAPHS",
                "FLYWHEEL_RECURSIVE_DAG_STARTUP_RECOVERY_MAX_GRAPHS",
            ],
        ),
        (
            "RSI_RECURSIVE_DAG_STARTUP_RECOVERY_TIME_BUDGET_MS",
            &[
                "MOTHERSHIP_RECURSIVE_DAG_STARTUP_RECOVERY_TIME_BUDGET_MS",
                "FLYWHEEL_RECURSIVE_DAG_STARTUP_RECOVERY_TIME_BUDGET_MS",
            ],
        ),
        (
            "RSI_RECURSIVE_DAG_CONTROLS_ENABLED",
            &[
                "MOTHERSHIP_RECURSIVE_DAG_CONTROLS_ENABLED",
                "FLYWHEEL_RECURSIVE_DAG_CONTROLS_ENABLED",
            ],
        ),
        (
            "RSI_RECURSIVE_DAG_RECOVERY_CONTROLS_ENABLED",
            &[
                "MOTHERSHIP_RECURSIVE_DAG_RECOVERY_CONTROLS_ENABLED",
                "FLYWHEEL_RECURSIVE_DAG_RECOVERY_CONTROLS_ENABLED",
            ],
        ),
        (
            "RSI_RECURSIVE_DAG_SCHEDULER_CONTROLS_ENABLED",
            &[
                "MOTHERSHIP_RECURSIVE_DAG_SCHEDULER_CONTROLS_ENABLED",
                "FLYWHEEL_RECURSIVE_DAG_SCHEDULER_CONTROLS_ENABLED",
            ],
        ),
        (
            "RSI_RECURSIVE_DAG_CANCELLATION_CONTROLS_ENABLED",
            &[
                "MOTHERSHIP_RECURSIVE_DAG_CANCELLATION_CONTROLS_ENABLED",
                "FLYWHEEL_RECURSIVE_DAG_CANCELLATION_CONTROLS_ENABLED",
            ],
        ),
        (
            "RSI_RECURSIVE_DAG_LIVE_SCHEDULER_CONTROL_ENABLED",
            &[
                "MOTHERSHIP_RECURSIVE_DAG_LIVE_SCHEDULER_CONTROL_ENABLED",
                "FLYWHEEL_RECURSIVE_DAG_LIVE_SCHEDULER_CONTROL_ENABLED",
            ],
        ),
        (
            "RSI_RECURSIVE_DAG_RUN_LEASE_TTL_MS",
            &[
                "MOTHERSHIP_RECURSIVE_DAG_RUN_LEASE_TTL_MS",
                "FLYWHEEL_RECURSIVE_DAG_RUN_LEASE_TTL_MS",
            ],
        ),
        (
            "RSI_RECURSIVE_DAG_MAX_CONCURRENT_GRAPHS",
            &[
                "MOTHERSHIP_RECURSIVE_DAG_MAX_CONCURRENT_GRAPHS",
                "FLYWHEEL_RECURSIVE_DAG_MAX_CONCURRENT_GRAPHS",
            ],
        ),
        (
            "RSI_RECURSIVE_DAG_FAKE_EXECUTOR_ONLY",
            &[
                "MOTHERSHIP_RECURSIVE_DAG_FAKE_EXECUTOR_ONLY",
                "FLYWHEEL_RECURSIVE_DAG_FAKE_EXECUTOR_ONLY",
            ],
        ),
        (
            "RSI_RECURSIVE_DAG_LIVE_EXECUTOR_ENABLED",
            &[
                "MOTHERSHIP_RECURSIVE_DAG_LIVE_EXECUTOR_ENABLED",
                "FLYWHEEL_RECURSIVE_DAG_LIVE_EXECUTOR_ENABLED",
            ],
        ),
        (
            "RSI_RECURSIVE_DAG_BACKGROUND_LOOP_ENABLED",
            &[
                "MOTHERSHIP_RECURSIVE_DAG_BACKGROUND_LOOP_ENABLED",
                "FLYWHEEL_RECURSIVE_DAG_BACKGROUND_LOOP_ENABLED",
            ],
        ),
        // Stall classifier (RSI-0XX) — see `config.rs` `from_env` block.
        (
            "RSI_STALL_CLASSIFIER_ENABLED",
            &[
                "MOTHERSHIP_STALL_CLASSIFIER_ENABLED",
                "FLYWHEEL_STALL_CLASSIFIER_ENABLED",
            ],
        ),
        (
            "RSI_RECURSIVE_DAG_STARTUP_RECOVERY_MAX_GRAPHS",
            &[
                "MOTHERSHIP_RECURSIVE_DAG_STARTUP_RECOVERY_MAX_GRAPHS",
                "FLYWHEEL_RECURSIVE_DAG_STARTUP_RECOVERY_MAX_GRAPHS",
            ],
        ),
        (
            "RSI_RECURSIVE_DAG_STARTUP_RECOVERY_TIME_BUDGET_MS",
            &[
                "MOTHERSHIP_RECURSIVE_DAG_STARTUP_RECOVERY_TIME_BUDGET_MS",
                "FLYWHEEL_RECURSIVE_DAG_STARTUP_RECOVERY_TIME_BUDGET_MS",
            ],
        ),
        (
            "RSI_STALL_CLASSIFIER_MODEL",
            &[
                "MOTHERSHIP_STALL_CLASSIFIER_MODEL",
                "FLYWHEEL_STALL_CLASSIFIER_MODEL",
            ],
        ),
        (
            "RSI_STALL_CLASSIFIER_API_URL",
            &[
                "MOTHERSHIP_STALL_CLASSIFIER_API_URL",
                "FLYWHEEL_STALL_CLASSIFIER_API_URL",
            ],
        ),
        (
            "RSI_STALL_CLASSIFIER_API_KEY",
            &[
                "MOTHERSHIP_STALL_CLASSIFIER_API_KEY",
                "FLYWHEEL_STALL_CLASSIFIER_API_KEY",
            ],
        ),
        (
            "RSI_STALL_CLASSIFIER_IDLE_SECS",
            &[
                "MOTHERSHIP_STALL_CLASSIFIER_IDLE_SECS",
                "FLYWHEEL_STALL_CLASSIFIER_IDLE_SECS",
            ],
        ),
        (
            "RSI_STALL_CLASSIFIER_IDLE_SECS_CODEX",
            &[
                "MOTHERSHIP_STALL_CLASSIFIER_IDLE_SECS_CODEX",
                "FLYWHEEL_STALL_CLASSIFIER_IDLE_SECS_CODEX",
            ],
        ),
        (
            "RSI_STALL_CLASSIFIER_COOLDOWN_SECS",
            &[
                "MOTHERSHIP_STALL_CLASSIFIER_COOLDOWN_SECS",
                "FLYWHEEL_STALL_CLASSIFIER_COOLDOWN_SECS",
            ],
        ),
        (
            "RSI_STALL_CLASSIFIER_MAX_PER_SESSION",
            &[
                "MOTHERSHIP_STALL_CLASSIFIER_MAX_PER_SESSION",
                "FLYWHEEL_STALL_CLASSIFIER_MAX_PER_SESSION",
            ],
        ),
        (
            "RSI_STALL_CLASSIFIER_CONFIDENCE_FLOOR",
            &[
                "MOTHERSHIP_STALL_CLASSIFIER_CONFIDENCE_FLOOR",
                "FLYWHEEL_STALL_CLASSIFIER_CONFIDENCE_FLOOR",
            ],
        ),
        (
            "RSI_STALL_CLASSIFIER_TIMEOUT_SECS",
            &[
                "MOTHERSHIP_STALL_CLASSIFIER_TIMEOUT_SECS",
                "FLYWHEEL_STALL_CLASSIFIER_TIMEOUT_SECS",
            ],
        ),
        // C2 local-tracker: issue_tracker_config() env surface (config.rs
        // `env_var_legacy!` sites above `issue_tracker_config`/
        // `issue_tracker_config_linear`/`issue_tracker_config_local`).
        (
            "RSI_ISSUE_TRACKER_KIND",
            &[
                "MOTHERSHIP_ISSUE_TRACKER_KIND",
                "FLYWHEEL_ISSUE_TRACKER_KIND",
            ],
        ),
        (
            "RSI_LINEAR_API_KEY",
            &["MOTHERSHIP_LINEAR_API_KEY", "FLYWHEEL_LINEAR_API_KEY"],
        ),
        (
            "RSI_LINEAR_TEAM_ID",
            &["MOTHERSHIP_LINEAR_TEAM_ID", "FLYWHEEL_LINEAR_TEAM_ID"],
        ),
        (
            "RSI_LINEAR_ASSIGNEE",
            &["MOTHERSHIP_LINEAR_ASSIGNEE", "FLYWHEEL_LINEAR_ASSIGNEE"],
        ),
        (
            "RSI_ISSUE_TRACKER_WORKING_DIR",
            &[
                "MOTHERSHIP_ISSUE_TRACKER_WORKING_DIR",
                "FLYWHEEL_ISSUE_TRACKER_WORKING_DIR",
            ],
        ),
        (
            "RSI_ISSUE_TRACKER_PROJECT_ID",
            &[
                "MOTHERSHIP_ISSUE_TRACKER_PROJECT_ID",
                "FLYWHEEL_ISSUE_TRACKER_PROJECT_ID",
            ],
        ),
        (
            "RSI_ISSUE_TRACKER_PROVIDER",
            &[
                "MOTHERSHIP_ISSUE_TRACKER_PROVIDER",
                "FLYWHEEL_ISSUE_TRACKER_PROVIDER",
            ],
        ),
        (
            "RSI_ISSUE_TRACKER_MODEL",
            &[
                "MOTHERSHIP_ISSUE_TRACKER_MODEL",
                "FLYWHEEL_ISSUE_TRACKER_MODEL",
            ],
        ),
        (
            "RSI_ISSUE_TRACKER_POLL_INTERVAL_MS",
            &[
                "MOTHERSHIP_ISSUE_TRACKER_POLL_INTERVAL_MS",
                "FLYWHEEL_ISSUE_TRACKER_POLL_INTERVAL_MS",
            ],
        ),
        (
            "RSI_ISSUE_TRACKER_ACTIVE_STATES",
            &[
                "MOTHERSHIP_ISSUE_TRACKER_ACTIVE_STATES",
                "FLYWHEEL_ISSUE_TRACKER_ACTIVE_STATES",
            ],
        ),
        (
            "RSI_ISSUE_TRACKER_MAX_CONCURRENT",
            &[
                "MOTHERSHIP_ISSUE_TRACKER_MAX_CONCURRENT",
                "FLYWHEEL_ISSUE_TRACKER_MAX_CONCURRENT",
            ],
        ),
        (
            "RSI_ISSUE_TRACKER_COMPLETION_STATE",
            &[
                "MOTHERSHIP_ISSUE_TRACKER_COMPLETION_STATE",
                "FLYWHEEL_ISSUE_TRACKER_COMPLETION_STATE",
            ],
        ),
    ];

    /// Run `f` with every `RSI_*` and parallel `MOTHERSHIP_*` / `FLYWHEEL_*`
    /// var unset for the duration of the closure. `temp_env::with_vars`
    /// snapshots and restores prior values around the call, including on
    /// panic, and serializes via its own internal process-global mutex (so no
    /// extra test-side mutex is needed here).
    fn with_clean_env<F, R>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let mut vars: Vec<(&str, Option<&str>)> = Vec::with_capacity(ENV_TRIPLES.len() * 3);
        for (new_name, legacy) in ENV_TRIPLES {
            vars.push((*new_name, None));
            for l in *legacy {
                vars.push((*l, None));
            }
        }
        temp_env::with_vars(vars, f)
    }

    /// Run `f` with the clean-env scrub *plus* the supplied overrides applied
    /// on top. Used by tests that exercise `RSI_*` override behavior.
    fn with_clean_env_and<F, R>(overrides: &[(&str, &str)], f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let mut vars: Vec<(&str, Option<&str>)> =
            Vec::with_capacity(ENV_TRIPLES.len() * 3 + overrides.len());
        for (new_name, legacy) in ENV_TRIPLES {
            vars.push((*new_name, None));
            for l in *legacy {
                vars.push((*l, None));
            }
        }
        for (k, v) in overrides {
            vars.push((*k, Some(*v)));
        }
        temp_env::with_vars(vars, f)
    }

    fn config_from_env_panic_message(overrides: &[(&str, &str)]) -> String {
        let panic = std::panic::catch_unwind(|| {
            with_clean_env_and(overrides, || {
                let _ = Config::from_env();
            });
        })
        .expect_err("Config::from_env should reject invalid recursive DAG env");
        if let Some(message) = panic.downcast_ref::<String>() {
            message.clone()
        } else if let Some(message) = panic.downcast_ref::<&'static str>() {
            (*message).to_string()
        } else {
            "non-string panic".to_string()
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_default_config() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env(|| {
            let config = Config::default();
            assert!(config.socket_path.to_string_lossy().contains("rsi"));
            assert_eq!(config.event_buffer_size, 100);
            assert!(!config.context_rotation_enabled);
            assert!(config.memory_enabled); // default: true
            assert!(config.memory_dir.to_string_lossy().contains("memory"));
            assert_eq!(config.memory_embedding_model, "qwen3-embedding:0.6b");
            assert!(config.memory_embedding_url.is_none());
            assert!(config.memory_embedding_api_key.is_none());
            assert_eq!(config.stall_timeout_running_secs, 1800);
            assert_eq!(config.stall_timeout_waiting_secs, 3600);
            assert!(config.stall_detection_enabled);
            assert!(config.build_model.is_none());
            assert!(config.invoke_model.is_none());
            assert!(config.queue_enabled);
            assert_eq!(config.queue_poll_interval_secs, 30);
            assert_eq!(config.queue_token_threshold, 1024);
            assert!(config.queue_config().is_some());
            assert_eq!(config.retry_max_default, 0);
            assert_eq!(config.retry_max_backoff_ms, 120_000);
            assert!(config.retry_on_stall);
            assert!(!config.smoke_suppress_retry_restore);
            assert!(!config.dream_enabled);
            assert_eq!(config.dream_observation_threshold, 50);
            assert_eq!(config.dream_idle_secs, 3600);
            assert_eq!(config.dream_cooldown_secs, 28800);
            assert!(config.dream_model.is_none());
            assert!(config.dream_api_url.is_none());
            // dream_api_key may or may not be set depending on ANTHROPIC_API_KEY in env
            assert_eq!(config.dream_batch_size, 20);
            assert!(config.dialectic_enabled);
            assert_eq!(config.dialectic_api_url, "http://localhost:11434/v1");
            assert!(config.dialectic_api_key.is_none());
            assert_eq!(config.dialectic_model, "qwen2.5:14b");
            assert_eq!(config.dialectic_max_iterations, 8);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_env_override() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(
            &[
                ("RSI_SOCKET", "/tmp/test.sock"),
                ("RSI_EVENT_BUFFER", "50"),
                ("RSI_CONTEXT_ROTATION_ENABLED", "true"),
            ],
            || {
                let config = Config::from_env();
                assert_eq!(config.socket_path, PathBuf::from("/tmp/test.sock"));
                assert_eq!(config.event_buffer_size, 50);
                assert!(config.context_rotation_enabled);
            },
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_memory_disabled_by_env() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(&[("RSI_MEMORY_ENABLED", "false")], || {
            let config = Config::from_env();
            assert!(!config.memory_enabled);
            assert!(config.memory_config().is_none());
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_memory_disabled_by_zero() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(&[("RSI_MEMORY_ENABLED", "0")], || {
            let config = Config::from_env();
            assert!(!config.memory_enabled);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn recursive_dag_smoke_suppress_retry_restore_env_is_default_off() {
        with_clean_env(|| {
            let config = Config::from_env();
            assert!(!config.smoke_suppress_retry_restore);
        });

        for truthy in ["1", "true", "yes", "on", " TRUE "] {
            with_clean_env_and(&[("RSI_SMOKE_SUPPRESS_RETRY_RESTORE", truthy)], || {
                let config = Config::from_env();
                assert!(config.smoke_suppress_retry_restore);
            });
        }

        for falsey in ["0", "false", "no", "off", " FALSE "] {
            with_clean_env_and(&[("RSI_SMOKE_SUPPRESS_RETRY_RESTORE", falsey)], || {
                let config = Config::from_env();
                assert!(!config.smoke_suppress_retry_restore);
            });
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn recursive_dag_smoke_suppress_retry_restore_invalid_env_fails_visibly() {
        let message =
            config_from_env_panic_message(&[("RSI_SMOKE_SUPPRESS_RETRY_RESTORE", "maybe")]);
        assert!(message.contains("RSI_SMOKE_SUPPRESS_RETRY_RESTORE"));
        assert!(message.contains("must be a boolean"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn recursive_dag_smoke_suppress_retry_restore_legacy_env_is_supported() {
        with_clean_env_and(&[("RSI_SMOKE_SUPPRESS_RETRY_RESTORE", "true")], || {
            let config = Config::from_env();
            assert!(config.smoke_suppress_retry_restore);
        });

        with_clean_env_and(
            &[("MOTHERSHIP_SMOKE_SUPPRESS_RETRY_RESTORE", "true")],
            || {
                let config = Config::from_env();
                assert!(config.smoke_suppress_retry_restore);
            },
        );

        with_clean_env_and(
            &[("FLYWHEEL_SMOKE_SUPPRESS_RETRY_RESTORE", "false")],
            || {
                let config = Config::from_env();
                assert!(!config.smoke_suppress_retry_restore);
            },
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_memory_disabled_by_off() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(&[("RSI_MEMORY_ENABLED", "off")], || {
            let config = Config::from_env();
            assert!(!config.memory_enabled);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_memory_enabled_by_default() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env(|| {
            let config = Config::from_env();
            assert!(config.memory_enabled);
            assert!(config.memory_config().is_some());
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_memory_dir_override() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(&[("RSI_MEMORY_DIR", "/custom/memory")], || {
            let config = Config::from_env();
            assert_eq!(config.memory_dir, PathBuf::from("/custom/memory"));
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_memory_embedding_model_override() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(
            &[("RSI_MEMORY_EMBEDDING_MODEL", "text-embedding-3-small")],
            || {
                let config = Config::from_env();
                assert_eq!(config.memory_embedding_model, "text-embedding-3-small");
            },
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_memory_embedding_url_and_key() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(
            &[
                ("RSI_MEMORY_EMBEDDING_URL", "http://localhost:8080"),
                ("RSI_MEMORY_EMBEDDING_API_KEY", "sk-test"),
            ],
            || {
                let config = Config::from_env();
                assert_eq!(
                    config.memory_embedding_url.as_deref(),
                    Some("http://localhost:8080")
                );
                assert_eq!(config.memory_embedding_api_key.as_deref(), Some("sk-test"));
            },
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_memory_config_db_path_suffix() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(&[("RSI_MEMORY_DIR", "/home/user/.rsi/memory")], || {
            let config = Config::from_env();
            let mc = config.memory_config().unwrap();
            assert!(mc.db_path.to_string_lossy().ends_with("memory.sqlite"));
            // db_path should be sibling to memory_dir, not inside it
            assert_eq!(mc.db_path, PathBuf::from("/home/user/.rsi/memory.sqlite"));
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_memory_config_inherits_defaults() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env(|| {
            let config = Config::from_env();
            let mc = config.memory_config().unwrap();
            assert_eq!(mc.chunk_tokens, 400);
            assert_eq!(mc.chunk_overlap, 80);
            assert_eq!(mc.max_results, 6);
            assert!((mc.min_score - 0.35).abs() < f64::EPSILON);
            assert!((mc.vector_weight - 0.7).abs() < f64::EPSILON);
            assert!((mc.text_weight - 0.3).abs() < f64::EPSILON);
            assert_eq!(mc.candidate_multiplier, 4);
            assert_eq!(mc.watch_debounce_ms, 1500);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_memory_enabled_explicit_true() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(&[("RSI_MEMORY_ENABLED", "true")], || {
            let config = Config::from_env();
            assert!(config.memory_enabled);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_stall_timeout_override() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(
            &[
                ("RSI_STALL_TIMEOUT_RUNNING_SECS", "60"),
                ("RSI_STALL_TIMEOUT_WAITING_SECS", "120"),
            ],
            || {
                let config = Config::from_env();
                assert_eq!(config.stall_timeout_running_secs, 60);
                assert_eq!(config.stall_timeout_waiting_secs, 120);
            },
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_stall_detection_disabled() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(&[("RSI_STALL_DETECTION_ENABLED", "false")], || {
            let config = Config::from_env();
            assert!(!config.stall_detection_enabled);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_stall_detection_disabled_by_zero() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(&[("RSI_STALL_DETECTION_ENABLED", "0")], || {
            let config = Config::from_env();
            assert!(!config.stall_detection_enabled);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_stall_detection_enabled_by_default() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env(|| {
            let config = Config::from_env();
            assert!(config.stall_detection_enabled);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_workspace_roots_default_empty() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env(|| {
            let config = Config::from_env();
            assert!(config.workspace_roots.is_empty());
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_workspace_roots_with_tmp() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        // /tmp always exists, so canonicalize should succeed
        with_clean_env_and(&[("RSI_WORKSPACE_ROOTS", "/tmp")], || {
            let config = Config::from_env();
            assert_eq!(config.workspace_roots.len(), 1);
            assert!(config.workspace_roots[0].is_absolute());
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_workspace_roots_nonexistent_skipped() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(
            &[(
                "RSI_WORKSPACE_ROOTS",
                "/tmp,/nonexistent/path/that/does/not/exist",
            )],
            || {
                let config = Config::from_env();
                // Only /tmp is valid; nonexistent path should be skipped
                assert_eq!(config.workspace_roots.len(), 1);
            },
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_reconciliation_defaults() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env(|| {
            let config = Config::from_env();
            assert!(config.reconciliation_enabled);
            assert_eq!(config.reconciliation_liveness_interval_secs, 120);
            assert_eq!(config.reconciliation_consistency_interval_secs, 600);
            assert_eq!(config.reconciliation_stall_action_standard, "notify");
            assert_eq!(
                config.reconciliation_stall_action_unattended,
                "interrupt_and_retry"
            );
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_reconciliation_disabled_by_env() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(&[("RSI_RECONCILIATION_ENABLED", "false")], || {
            let config = Config::from_env();
            assert!(!config.reconciliation_enabled);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_reconciliation_interval_override() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(
            &[
                ("RSI_RECONCILIATION_LIVENESS_SECS", "30"),
                ("RSI_RECONCILIATION_CONSISTENCY_SECS", "300"),
            ],
            || {
                let config = Config::from_env();
                assert_eq!(config.reconciliation_liveness_interval_secs, 30);
                assert_eq!(config.reconciliation_consistency_interval_secs, 300);
            },
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_reconciliation_stall_action_override() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(
            &[
                ("RSI_RECONCILIATION_STALL_ACTION", "interrupt"),
                ("RSI_RECONCILIATION_STALL_ACTION_UNATTENDED", "interrupt"),
            ],
            || {
                let config = Config::from_env();
                assert_eq!(config.reconciliation_stall_action_standard, "interrupt");
                assert_eq!(config.reconciliation_stall_action_unattended, "interrupt");
            },
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_retry_max_default_override() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(&[("RSI_RETRY_MAX_DEFAULT", "3")], || {
            let config = Config::from_env();
            assert_eq!(config.retry_max_default, 3);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_retry_max_backoff_ms_override() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(&[("RSI_RETRY_MAX_BACKOFF_MS", "60000")], || {
            let config = Config::from_env();
            assert_eq!(config.retry_max_backoff_ms, 60_000);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_retry_on_stall_enabled() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(&[("RSI_RETRY_ON_STALL", "true")], || {
            let config = Config::from_env();
            assert!(config.retry_on_stall);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_retry_on_stall_enabled_by_default() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env(|| {
            let config = Config::from_env();
            assert!(config.retry_on_stall);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_dream_disabled_by_env() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(&[("RSI_DREAM_ENABLED", "false")], || {
            let config = Config::from_env();
            assert!(!config.dream_enabled);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_dream_threshold_override() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(&[("RSI_DREAM_OBSERVATION_THRESHOLD", "100")], || {
            let config = Config::from_env();
            assert_eq!(config.dream_observation_threshold, 100);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_dream_model_override() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(&[("RSI_DREAM_MODEL", "gpt-4")], || {
            let config = Config::from_env();
            assert_eq!(config.dream_model.as_deref(), Some("gpt-4"));
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_dream_batch_size_override() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(&[("RSI_DREAM_BATCH_SIZE", "5")], || {
            let config = Config::from_env();
            assert_eq!(config.dream_batch_size, 5);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn codex_sandbox_mode_defaults_to_danger_full_access() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env(|| {
            let config = Config::from_env();
            assert_eq!(config.codex_sandbox_mode, "danger-full-access");
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn codex_sandbox_mode_parses_env() {
        // env-guard (RSI-020): scrubs FLYWHEEL_* / MOTHERSHIP_* legacy fallbacks before reading config.
        with_clean_env_and(&[("RSI_CODEX_SANDBOX_MODE", "danger-full-access")], || {
            let config = Config::from_env();
            assert_eq!(config.codex_sandbox_mode, "danger-full-access");
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn codex_sandbox_mode_parses_underscore_alias() {
        // CodexSandboxMode::parse normalizes underscores to hyphens (see codex.rs).
        with_clean_env_and(&[("RSI_CODEX_SANDBOX_MODE", "danger_full_access")], || {
            let config = Config::from_env();
            assert_eq!(config.codex_sandbox_mode, "danger-full-access");
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn codex_sandbox_mode_invalid_env_falls_back() {
        with_clean_env_and(&[("RSI_CODEX_SANDBOX_MODE", "garbage")], || {
            let config = Config::from_env();
            // Invalid input warns and falls back to the default.
            assert_eq!(config.codex_sandbox_mode, "danger-full-access");
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn recursive_dag_startup_recovery_defaults_are_bounded() {
        with_clean_env(|| {
            let config = Config::from_env();
            assert_eq!(
                config.recursive_dag_startup_recovery_max_graphs,
                crate::store::recursive_dag::DEFAULT_RECURSIVE_STARTUP_RECOVERY_MAX_GRAPHS
            );
            assert_eq!(
                config.recursive_dag_startup_recovery_time_budget_ms,
                crate::store::recursive_dag::DEFAULT_RECURSIVE_STARTUP_RECOVERY_TIME_BUDGET_MS
            );
            assert!(!config.recursive_dag_recovery_controls_enabled);
            assert!(!config.recursive_dag_scheduler_controls_enabled);
            assert!(!config.recursive_dag_cancellation_controls_enabled);
            assert!(!config.recursive_dag_live_scheduler_control_enabled);
            assert!(!config.gv_render_recursive_origin);
            assert!(!config.gv_info_dashboard);
            assert_eq!(
                config.recursive_dag_run_lease_ttl_ms,
                (crate::store::recursive_dag::DEFAULT_RECURSIVE_SCHEDULER_LEASE_TTL_SECONDS as u64)
                    * 1000
            );
            assert_eq!(
                config.recursive_dag_max_concurrent_graphs,
                crate::store::recursive_dag::DEFAULT_RECURSIVE_SCHEDULER_MAX_ACTIVE_RUNS
            );
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn recursive_dag_startup_recovery_env_override() {
        with_clean_env_and(
            &[
                ("RSI_RECURSIVE_DAG_STARTUP_RECOVERY_MAX_GRAPHS", "7"),
                ("RSI_RECURSIVE_DAG_STARTUP_RECOVERY_TIME_BUDGET_MS", "23"),
                ("RSI_RECURSIVE_DAG_CONTROLS_ENABLED", "true"),
                ("RSI_RECURSIVE_DAG_RUN_LEASE_TTL_MS", "1234"),
                ("RSI_RECURSIVE_DAG_MAX_CONCURRENT_GRAPHS", "2"),
            ],
            || {
                let config = Config::from_env();
                assert_eq!(config.recursive_dag_startup_recovery_max_graphs, 7);
                assert_eq!(config.recursive_dag_startup_recovery_time_budget_ms, 23);
                assert!(config.recursive_dag_recovery_controls_enabled);
                assert!(config.recursive_dag_scheduler_controls_enabled);
                assert!(config.recursive_dag_cancellation_controls_enabled);
                assert!(!config.recursive_dag_live_scheduler_control_enabled);
                assert_eq!(config.recursive_dag_run_lease_ttl_ms, 1234);
                assert_eq!(config.recursive_dag_max_concurrent_graphs, 2);
            },
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn recursive_dag_control_env_overrides_are_granular() {
        with_clean_env_and(
            &[
                ("RSI_RECURSIVE_DAG_CONTROLS_ENABLED", "true"),
                ("RSI_RECURSIVE_DAG_SCHEDULER_CONTROLS_ENABLED", "false"),
            ],
            || {
                let config = Config::from_env();
                assert!(config.recursive_dag_recovery_controls_enabled);
                assert!(!config.recursive_dag_scheduler_controls_enabled);
                assert!(config.recursive_dag_cancellation_controls_enabled);
                assert!(!config.recursive_dag_live_scheduler_control_enabled);
            },
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn recursive_dag_control_env_invalid_values_fail_visibly() {
        let message =
            config_from_env_panic_message(&[("RSI_RECURSIVE_DAG_CONTROLS_ENABLED", "maybe")]);
        assert!(message.contains("RSI_RECURSIVE_DAG_CONTROLS_ENABLED"));
        assert!(message.contains("must be a boolean"));

        let message = config_from_env_panic_message(&[("RSI_RECURSIVE_DAG_RUN_LEASE_TTL_MS", "0")]);
        assert!(message.contains("RSI_RECURSIVE_DAG_RUN_LEASE_TTL_MS"));
        assert!(message.contains("must be positive"));

        let message =
            config_from_env_panic_message(&[("RSI_RECURSIVE_DAG_MAX_CONCURRENT_GRAPHS", "many")]);
        assert!(message.contains("RSI_RECURSIVE_DAG_MAX_CONCURRENT_GRAPHS"));
        assert!(message.contains("must be an unsigned integer"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn recursive_dag_live_scheduler_env_flag_is_explicit_default_false() {
        with_clean_env_and(
            &[("RSI_RECURSIVE_DAG_LIVE_SCHEDULER_CONTROL_ENABLED", "false")],
            || {
                let config = Config::from_env();
                assert!(!config.recursive_dag_live_scheduler_control_enabled);
            },
        );
        with_clean_env_and(
            &[("RSI_RECURSIVE_DAG_LIVE_SCHEDULER_CONTROL_ENABLED", "true")],
            || {
                let config = Config::from_env();
                assert!(config.recursive_dag_live_scheduler_control_enabled);
            },
        );

        let message =
            config_from_env_panic_message(&[("RSI_RECURSIVE_DAG_LIVE_EXECUTOR_ENABLED", "true")]);
        assert!(message.contains("standalone recursive DAG live executor is not available"));

        let message =
            config_from_env_panic_message(&[("RSI_RECURSIVE_DAG_BACKGROUND_LOOP_ENABLED", "true")]);
        assert!(message.contains("recursive DAG background scheduling is not available"));

        let message =
            config_from_env_panic_message(&[("RSI_RECURSIVE_DAG_FAKE_EXECUTOR_ONLY", "false")]);
        assert!(message.contains("recursive DAG execution is fake-only"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn recursive_dag_startup_recovery_zero_max_graphs_falls_back() {
        with_clean_env_and(
            &[
                ("RSI_RECURSIVE_DAG_STARTUP_RECOVERY_MAX_GRAPHS", "0"),
                ("RSI_RECURSIVE_DAG_STARTUP_RECOVERY_TIME_BUDGET_MS", "0"),
            ],
            || {
                let config = Config::from_env();
                assert_eq!(
                    config.recursive_dag_startup_recovery_max_graphs,
                    crate::store::recursive_dag::DEFAULT_RECURSIVE_STARTUP_RECOVERY_MAX_GRAPHS
                );
                assert_eq!(config.recursive_dag_startup_recovery_time_budget_ms, 0);
            },
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn sandbox_build_cache_runtime_config_defaults_and_json_are_complete() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config(&config);
            let json = rc.to_json();

            let reclaim = rc.sandbox_build_cache_reclaim_snapshot();
            assert!(reclaim.enabled);
            assert_eq!(
                reclaim.ttl_secs,
                SANDBOX_BUILD_CACHE_RECLAIM_TTL_SECS_DEFAULT
            );
            assert_eq!(
                reclaim.interval_secs,
                SANDBOX_BUILD_CACHE_RECLAIM_INTERVAL_SECS_DEFAULT
            );
            assert_eq!(
                json["sandbox_build_cache_reclaim_high_watermark_pct"],
                SANDBOX_BUILD_CACHE_RECLAIM_HIGH_WATERMARK_PCT_DEFAULT
            );
            assert_eq!(
                json["sandbox_build_cache_reclaim_low_watermark_pct"],
                SANDBOX_BUILD_CACHE_RECLAIM_LOW_WATERMARK_PCT_DEFAULT
            );
            assert_eq!(
                json["sandbox_build_cache_reclaim_max_candidates"],
                SANDBOX_BUILD_CACHE_RECLAIM_MAX_CANDIDATES_DEFAULT
            );
            for field in [
                "sandbox_build_cache_reclaim_enabled",
                "sandbox_build_cache_reclaim_ttl_secs",
                "sandbox_build_cache_reclaim_interval_secs",
                "sandbox_build_cache_reclaim_high_watermark_pct",
                "sandbox_build_cache_reclaim_low_watermark_pct",
                "sandbox_build_cache_reclaim_max_candidates",
            ] {
                assert!(is_persisted_runtime_config_field(field), "{field}");
                assert!(rc.persisted_field_value(field).is_some(), "{field}");
            }
            assert!(
                !is_persisted_runtime_config_field("sandbox_build_cache_pressure_active"),
                "transient hysteresis state must not be persisted"
            );
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn sandbox_allocation_limits_default_validate_and_persist_as_runtime_fields() {
        with_clean_env(|| {
            let config = Config::from_env();
            let runtime = RuntimeConfig::from_config(&config);
            assert_eq!(
                runtime.sandbox_max_source_roots.load(Ordering::Relaxed),
                4096
            );
            assert_eq!(runtime.sandbox_min_free_gib.load(Ordering::Relaxed), 30);

            assert_eq!(runtime.to_json()["sandbox_max_source_roots"], 4096);
            assert_eq!(runtime.to_json()["sandbox_min_free_gib"], 30);
            assert!(is_persisted_runtime_config_field(
                "sandbox_max_source_roots"
            ));
            assert!(is_persisted_runtime_config_field("sandbox_min_free_gib"));
            assert_eq!(
                runtime.persisted_field_value("sandbox_max_source_roots"),
                Some(serde_json::json!(4096))
            );
            assert_eq!(
                runtime.persisted_field_value("sandbox_min_free_gib"),
                Some(serde_json::json!(30))
            );

            assert!(
                runtime
                    .update_field("sandbox_max_source_roots", &serde_json::json!(1))
                    .unwrap()
            );
            assert!(
                runtime
                    .update_field("sandbox_min_free_gib", &serde_json::json!(0))
                    .unwrap()
            );
            assert_eq!(
                runtime.persisted_field_value("sandbox_max_source_roots"),
                Some(serde_json::json!(1))
            );
            assert_eq!(
                runtime.persisted_field_value("sandbox_min_free_gib"),
                Some(serde_json::json!(0))
            );

            let before = runtime.to_json();
            for (field, invalid) in [
                ("sandbox_max_source_roots", serde_json::json!(0)),
                ("sandbox_max_source_roots", serde_json::json!(65_537)),
                ("sandbox_min_free_gib", serde_json::json!(1025)),
                ("sandbox_min_free_gib", serde_json::json!(-1)),
            ] {
                assert!(
                    runtime.update_field(field, &invalid).is_err(),
                    "{field}={invalid}"
                );
                assert_eq!(runtime.to_json()[field], before[field], "{field}");
            }
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn sandbox_build_cache_runtime_config_validates_bounds_atomically() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config(&config);

            for (field, value) in [
                ("sandbox_build_cache_reclaim_ttl_secs", 900_u64),
                ("sandbox_build_cache_reclaim_interval_secs", 300),
                ("sandbox_build_cache_reclaim_high_watermark_pct", 90),
                ("sandbox_build_cache_reclaim_low_watermark_pct", 80),
                ("sandbox_build_cache_reclaim_max_candidates", 128),
            ] {
                assert!(
                    rc.update_field(field, &serde_json::json!(value))
                        .expect("bounded setting must update"),
                    "{field}"
                );
                assert_eq!(rc.to_json()[field], value, "{field}");
            }

            let before = rc.to_json();
            for (field, value) in [
                ("sandbox_build_cache_reclaim_ttl_secs", 0_u64),
                ("sandbox_build_cache_reclaim_interval_secs", 59),
                ("sandbox_build_cache_reclaim_high_watermark_pct", 80),
                ("sandbox_build_cache_reclaim_low_watermark_pct", 90),
                ("sandbox_build_cache_reclaim_max_candidates", 1025),
            ] {
                assert!(
                    rc.update_field(field, &serde_json::json!(value)).is_err(),
                    "{field}={value} must fail"
                );
                assert_eq!(rc.to_json()[field], before[field], "{field}");
            }
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn sandbox_build_cache_runtime_config_concurrent_watermarks_never_publish_invalid_pair() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config(&config);
            std::thread::scope(|scope| {
                let high = Arc::clone(&rc);
                scope.spawn(move || {
                    let _ = high.update_field(
                        "sandbox_build_cache_reclaim_high_watermark_pct",
                        &serde_json::json!(80),
                    );
                });
                let low = Arc::clone(&rc);
                scope.spawn(move || {
                    let _ = low.update_field(
                        "sandbox_build_cache_reclaim_low_watermark_pct",
                        &serde_json::json!(82),
                    );
                });
            });
            let snapshot = rc.sandbox_build_cache_reclaim_snapshot();
            assert!(snapshot.low_watermark_pct < snapshot.high_watermark_pct);
            assert!(matches!(
                (snapshot.high_watermark_pct, snapshot.low_watermark_pct),
                (80, 75) | (85, 82)
            ));
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_update_codex_sandbox_mode_valid() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config(&config);
            // Seed value matches Config.
            assert_eq!(*rc.codex_sandbox_mode.read(), "danger-full-access");
            // Accept all three canonical values.
            for v in ["read-only", "workspace-write", "danger-full-access"] {
                let ok = rc
                    .update_field("codex_sandbox_mode", &serde_json::json!(v))
                    .expect("update_field should succeed");
                assert!(ok);
                assert_eq!(*rc.codex_sandbox_mode.read(), v);
            }
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_update_codex_sandbox_mode_accepts_underscore_alias() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config(&config);
            let ok = rc
                .update_field(
                    "codex_sandbox_mode",
                    &serde_json::json!("danger_full_access"),
                )
                .expect("update_field should accept underscore alias");
            assert!(ok);
            // Stored as canonical hyphenated form.
            assert_eq!(*rc.codex_sandbox_mode.read(), "danger-full-access");
        });
    }

    /// Issue #35 (b). Every legal ceiling round-trips through the operator
    /// write path and lands in `GetDaemonConfig`'s payload.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_update_orchestration_max_child_effort_accepts_every_choice() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config(&config);
            // Default is the sentinel: an unset ceiling is the pre-#34 rule.
            assert_eq!(*rc.orchestration_max_child_effort.read(), "unset");
            for choice in rsi_common::model_control::ORCHESTRATION_MAX_CHILD_EFFORT_CHOICES {
                assert!(
                    rc.update_field("orchestration_max_child_effort", &serde_json::json!(choice))
                        .expect("legal ceiling must be accepted"),
                    "{choice} must be a known field"
                );
                assert_eq!(*rc.orchestration_max_child_effort.read(), *choice);
                assert_eq!(
                    rc.to_json()
                        .get("orchestration_max_child_effort")
                        .and_then(|v| v.as_str()),
                    Some(*choice),
                    "GetDaemonConfig payload must expose the ceiling"
                );
            }
        });
    }

    /// Issue #35 (b). Whitespace/case are normalized, and both `""` and JSON
    /// `null` mean "clear the ceiling".
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_update_orchestration_max_child_effort_normalizes_and_clears() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config(&config);
            for raw in ["  XHigh ", "XHIGH", "xhigh"] {
                rc.update_field("orchestration_max_child_effort", &serde_json::json!(raw))
                    .expect("normalizable value must be accepted");
                assert_eq!(*rc.orchestration_max_child_effort.read(), "xhigh");
            }
            for clear in [serde_json::json!(""), serde_json::json!(null)] {
                rc.update_field("orchestration_max_child_effort", &clear)
                    .expect("clearing must be accepted");
                assert_eq!(*rc.orchestration_max_child_effort.read(), "unset");
                // Re-arm so the next iteration proves a real transition.
                rc.update_field(
                    "orchestration_max_child_effort",
                    &serde_json::json!("xhigh"),
                )
                .expect("re-arm must be accepted");
            }
        });
    }

    /// Issue #35 (b). The bound is real: out-of-enum values are refused and the
    /// lock is left untouched, so a bad write cannot install a ceiling.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_update_orchestration_max_child_effort_rejects_out_of_enum() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config(&config);
            rc.update_field(
                "orchestration_max_child_effort",
                &serde_json::json!("xhigh"),
            )
            .expect("baseline set must be accepted");
            // "ultra2"/"extreme" are the dangerous shapes: near-miss spellings
            // that must not silently rank as 0 and deny every child.
            for bad in [
                serde_json::json!("XHIGH!!"),
                serde_json::json!("extreme"),
                serde_json::json!("ultra2"),
                serde_json::json!("xhigh xhigh"),
                serde_json::json!(42),
                serde_json::json!(true),
                serde_json::json!(["xhigh"]),
            ] {
                let before = rc.orchestration_max_child_effort.read().clone();
                let err = rc
                    .update_field("orchestration_max_child_effort", &bad)
                    .expect_err(&format!("{bad} must be rejected"));
                assert!(
                    err.contains("xhigh") || err.contains("expected string"),
                    "error must name the legal set or the type: {err}"
                );
                assert_eq!(
                    *rc.orchestration_max_child_effort.read(),
                    before,
                    "rejected write must leave the ceiling untouched"
                );
            }
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_update_codex_sandbox_mode_invalid() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config(&config);
            let before = rc.codex_sandbox_mode.read().clone();
            let err = rc
                .update_field("codex_sandbox_mode", &serde_json::json!("garbage"))
                .expect_err("garbage value should be rejected");
            assert!(
                err.contains("read-only"),
                "err must mention valid options: {err}"
            );
            // Lock unchanged on rejection.
            assert_eq!(*rc.codex_sandbox_mode.read(), before);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_gates_recursive_dag_live_scheduler_and_background_controls() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config(&config);

            assert!(
                rc.update_field(
                    "recursive_dag_fixed_fake_executor_only",
                    &serde_json::json!(true)
                )
                .unwrap()
            );
            let err = rc
                .update_field(
                    "recursive_dag_fixed_fake_executor_only",
                    &serde_json::json!(false),
                )
                .expect_err("fake-only=false should be rejected");
            assert!(err.contains("fake-only"));

            assert!(
                rc.update_field(
                    "recursive_dag_fixed_live_executor_enabled",
                    &serde_json::json!(false),
                )
                .unwrap()
            );
            let err = rc
                .update_field(
                    "recursive_dag_fixed_live_executor_enabled",
                    &serde_json::json!(true),
                )
                .expect_err("live execution should be rejected");
            assert!(err.contains("not available"));

            assert!(
                rc.update_field(
                    "recursive_dag_live_scheduler_control_enabled",
                    &serde_json::json!(false),
                )
                .unwrap()
            );
            assert!(
                rc.update_field(
                    "recursive_dag_live_scheduler_control_enabled",
                    &serde_json::json!(true),
                )
                .unwrap()
            );
            assert!(
                rc.recursive_dag_live_scheduler_control_enabled
                    .load(Ordering::Relaxed)
            );

            let err = rc
                .update_field(
                    "recursive_dag_live_scheduler_control_enabled",
                    &serde_json::json!("false"),
                )
                .expect_err("non-bool live scheduler config should be rejected");
            assert!(err.contains("expected bool"));

            assert!(
                rc.update_field(
                    "recursive_dag_fixed_background_loop_enabled",
                    &serde_json::json!(false),
                )
                .unwrap()
            );
            let err = rc
                .update_field(
                    "recursive_dag_fixed_background_loop_enabled",
                    &serde_json::json!(true),
                )
                .expect_err("background loop should be rejected");
            assert!(err.contains("not available"));

            let err = rc
                .update_field(
                    "recursive_dag_fixed_live_executor_enabled",
                    &serde_json::json!("false"),
                )
                .expect_err("non-bool live config should be rejected");
            assert!(err.contains("expected bool"));
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_recursive_dag_live_and_background_flags_remain_false() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config(&config);
            let json = rc.to_json();

            assert_eq!(json["recursive_dag_live_status_inspection"], true);
            assert_eq!(json["recursive_dag_live_validation_inspection"], true);
            assert_eq!(json["recursive_dag_live_scheduler_control_enabled"], false);
            assert_eq!(json["gv_render_recursive_origin"], false);
            assert_eq!(json["gv_info_dashboard"], false);
            assert_eq!(json["recursive_dag_fixed_fake_executor_only"], true);
            assert_eq!(json["recursive_dag_fixed_live_executor_enabled"], false);
            assert_eq!(json["recursive_dag_fixed_background_loop_enabled"], false);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn gv_render_recursive_origin_is_persisted_and_default_false() {
        with_clean_env(|| {
            // Default false (struct + env).
            let config = Config::from_env();
            assert!(!config.gv_render_recursive_origin);
            let rc = RuntimeConfig::from_config(&config);
            assert_eq!(rc.to_json()["gv_render_recursive_origin"], false);

            // Allowlist membership ⇒ durable across restart.
            assert!(is_persisted_runtime_config_field(
                "gv_render_recursive_origin"
            ));

            // update_field round-trip persistence: toggle true then read back.
            assert!(
                rc.update_field("gv_render_recursive_origin", &serde_json::json!(true))
                    .unwrap()
            );
            assert!(rc.gv_render_recursive_origin.load(Ordering::Relaxed));
            assert_eq!(rc.to_json()["gv_render_recursive_origin"], true);

            // Non-bool rejected.
            let err = rc
                .update_field("gv_render_recursive_origin", &serde_json::json!("true"))
                .expect_err("non-bool gv_render_recursive_origin should be rejected");
            assert!(err.contains("expected bool"));
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn topology_executor_kill_switch_is_persisted_and_default_true() {
        with_clean_env(|| {
            let config = Config::from_env();
            assert!(config.topology_executor_enabled);
            let rc = RuntimeConfig::from_config(&config);
            assert_eq!(rc.to_json()["topology_executor_enabled"], true);
            assert!(is_persisted_runtime_config_field(
                "topology_executor_enabled"
            ));
            assert!(
                rc.update_field("topology_executor_enabled", &serde_json::json!(false))
                    .unwrap()
            );
            assert!(!rc.topology_executor_enabled.load(Ordering::Relaxed));
            assert_eq!(rc.to_json()["topology_executor_enabled"], false);
            let err = rc
                .update_field("topology_executor_enabled", &serde_json::json!("off"))
                .expect_err("non-bool kill switch must be rejected");
            assert!(err.contains("expected bool"));
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn topology_build_node_concurrency_is_operator_editable() {
        with_clean_env(|| {
            let config = Config::from_env();
            assert_eq!(config.topology_max_concurrent_build_nodes, 2);
            let rc = RuntimeConfig::from_config(&config);
            assert_eq!(rc.to_json()["topology_max_concurrent_build_nodes"], 2);
            assert!(is_persisted_runtime_config_field(
                "topology_max_concurrent_build_nodes"
            ));
            assert!(
                rc.update_field("topology_max_concurrent_build_nodes", &serde_json::json!(3))
                    .unwrap()
            );
            assert_eq!(
                rc.topology_max_concurrent_build_nodes
                    .load(Ordering::Relaxed),
                3
            );
            assert!(
                rc.update_field("topology_max_concurrent_build_nodes", &serde_json::json!(0))
                    .is_err()
            );
            assert!(
                rc.update_field(
                    "topology_max_concurrent_build_nodes",
                    &serde_json::json!(17)
                )
                .is_err()
            );
        });
    }

    /// #633: the bulk fan-out threshold is operator-editable through
    /// `UpdateDaemonConfig`, persisted, defaults to 4 and accepts 0 (off).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn topology_bulk_fanout_min_openrouter_is_operator_editable() {
        with_clean_env(|| {
            let config = Config::from_env();
            assert_eq!(config.topology_bulk_fanout_min_openrouter, 4);
            let rc = RuntimeConfig::from_config(&config);
            assert_eq!(rc.to_json()["topology_bulk_fanout_min_openrouter"], 4);
            assert!(is_persisted_runtime_config_field(
                "topology_bulk_fanout_min_openrouter"
            ));
            for accepted in [0_u32, 2, 6, 64] {
                assert!(
                    rc.update_field(
                        "topology_bulk_fanout_min_openrouter",
                        &serde_json::json!(accepted)
                    )
                    .unwrap()
                );
                assert_eq!(
                    rc.topology_bulk_fanout_min_openrouter
                        .load(Ordering::Relaxed),
                    accepted
                );
            }
            for refused in [
                serde_json::json!(1),
                serde_json::json!(65),
                serde_json::json!("4"),
            ] {
                assert!(
                    rc.update_field("topology_bulk_fanout_min_openrouter", &refused)
                        .is_err()
                );
            }
            assert_eq!(
                rc.topology_bulk_fanout_min_openrouter
                    .load(Ordering::Relaxed),
                64
            );
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn gv_info_dashboard_is_persisted_and_default_false() {
        with_clean_env(|| {
            // Default false (struct + env).
            let config = Config::from_env();
            assert!(!config.gv_info_dashboard);
            let rc = RuntimeConfig::from_config(&config);
            assert_eq!(rc.to_json()["gv_info_dashboard"], false);

            // Allowlist membership ⇒ durable across restart.
            assert!(is_persisted_runtime_config_field("gv_info_dashboard"));

            // update_field round-trip persistence: toggle true then read back.
            assert!(
                rc.update_field("gv_info_dashboard", &serde_json::json!(true))
                    .unwrap()
            );
            assert!(rc.gv_info_dashboard.load(Ordering::Relaxed));
            assert_eq!(rc.to_json()["gv_info_dashboard"], true);

            // Non-bool rejected.
            let err = rc
                .update_field("gv_info_dashboard", &serde_json::json!("true"))
                .expect_err("non-bool gv_info_dashboard should be rejected");
            assert!(err.contains("expected bool"));
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_update_codex_sandbox_mode_rejects_non_string() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config(&config);
            let err = rc
                .update_field("codex_sandbox_mode", &serde_json::json!(42))
                .expect_err("non-string value should be rejected");
            assert!(err.contains("string"), "err must mention string: {err}");
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_codex_sandbox_mode_serializes_in_to_json() {
        with_clean_env_and(&[("RSI_CODEX_SANDBOX_MODE", "read-only")], || {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config(&config);
            let json = rc.to_json();
            assert_eq!(
                json.get("codex_sandbox_mode").and_then(|v| v.as_str()),
                Some("read-only")
            );
        });
    }

    // --- RSI-026: system_prompt_preset tests ---

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn system_prompt_preset_defaults_to_default() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config(&config);
            // Seed default ("default" — set by from_config helper).
            assert_eq!(*rc.system_prompt_preset.read(), "default");
            let json = rc.to_json();
            assert_eq!(
                json.get("system_prompt_preset").and_then(|v| v.as_str()),
                Some("default")
            );
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn from_config_with_system_prompt_preset_uses_seed() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config_with_system_prompt_preset(
                &config,
                "caveman".to_string(),
            );
            assert_eq!(*rc.system_prompt_preset.read(), "caveman");
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_update_system_prompt_preset_valid() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config(&config);
            for v in ["default", "concise", "code-only", "caveman"] {
                let ok = rc
                    .update_field("system_prompt_preset", &serde_json::json!(v))
                    .expect("update_field should succeed");
                assert!(ok);
                assert_eq!(*rc.system_prompt_preset.read(), v);
            }
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_update_system_prompt_preset_accepts_label_alias() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config(&config);

            // Title-cased labels (TUI Display strings) normalize to canonical slugs.
            let ok = rc
                .update_field("system_prompt_preset", &serde_json::json!("Code Only"))
                .expect("accepts 'Code Only'");
            assert!(ok);
            assert_eq!(*rc.system_prompt_preset.read(), "code-only");

            let ok = rc
                .update_field("system_prompt_preset", &serde_json::json!("CodeOnly"))
                .expect("accepts 'CodeOnly'");
            assert!(ok);
            assert_eq!(*rc.system_prompt_preset.read(), "code-only");

            let ok = rc
                .update_field("system_prompt_preset", &serde_json::json!("Caveman"))
                .expect("accepts 'Caveman'");
            assert!(ok);
            assert_eq!(*rc.system_prompt_preset.read(), "caveman");

            let ok = rc
                .update_field("system_prompt_preset", &serde_json::json!("DEFAULT"))
                .expect("accepts 'DEFAULT'");
            assert!(ok);
            assert_eq!(*rc.system_prompt_preset.read(), "default");
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_update_system_prompt_preset_accepts_underscore_alias() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config(&config);
            let ok = rc
                .update_field("system_prompt_preset", &serde_json::json!("code_only"))
                .expect("accepts 'code_only'");
            assert!(ok);
            assert_eq!(*rc.system_prompt_preset.read(), "code-only");
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_update_system_prompt_preset_invalid() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config(&config);
            let before = rc.system_prompt_preset.read().clone();
            let err = rc
                .update_field("system_prompt_preset", &serde_json::json!("garbage"))
                .expect_err("garbage value should be rejected");
            assert!(
                err.contains("default") || err.contains("concise"),
                "err must mention valid options: {err}"
            );
            // Lock unchanged on rejection.
            assert_eq!(*rc.system_prompt_preset.read(), before);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_update_system_prompt_preset_rejects_non_string() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config(&config);
            let err = rc
                .update_field("system_prompt_preset", &serde_json::json!(42))
                .expect_err("non-string value should be rejected");
            assert!(err.contains("string"), "err must mention string: {err}");
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_system_prompt_preset_serializes_in_to_json() {
        with_clean_env(|| {
            let config = Config::from_env();
            let rc = RuntimeConfig::from_config_with_system_prompt_preset(
                &config,
                "caveman".to_string(),
            );
            let json = rc.to_json();
            assert_eq!(
                json.get("system_prompt_preset").and_then(|v| v.as_str()),
                Some("caveman")
            );
        });
    }

    // --- Stall classifier (RSI-0XX) ---

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn stall_classifier_env_defaults() {
        with_clean_env(|| {
            let c = Config::from_env();
            assert!(!c.stall_classifier_enabled);
            assert_eq!(c.stall_classifier_model, "qwen2.5:7b");
            assert_eq!(
                c.stall_classifier_api_url,
                "http://localhost:11434/v1/chat/completions"
            );
            assert_eq!(c.stall_classifier_idle_secs, 600);
            assert_eq!(c.stall_classifier_idle_secs_codex, 1800);
            assert_eq!(c.stall_classifier_cooldown_secs, 1800);
            assert_eq!(c.stall_classifier_max_per_session, 3);
            assert!((c.stall_classifier_confidence_floor - 0.7).abs() < 1e-9);
            assert_eq!(c.stall_classifier_timeout_secs, 30);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn stall_classifier_env_overrides() {
        with_clean_env_and(
            &[
                ("RSI_STALL_CLASSIFIER_ENABLED", "true"),
                ("RSI_STALL_CLASSIFIER_MODEL", "qwen3:14b"),
                ("RSI_STALL_CLASSIFIER_API_URL", "http://api.example/v1"),
                ("RSI_STALL_CLASSIFIER_API_KEY", "k-test"),
                ("RSI_STALL_CLASSIFIER_IDLE_SECS", "300"),
                ("RSI_STALL_CLASSIFIER_IDLE_SECS_CODEX", "900"),
                ("RSI_STALL_CLASSIFIER_COOLDOWN_SECS", "60"),
                ("RSI_STALL_CLASSIFIER_MAX_PER_SESSION", "9"),
                ("RSI_STALL_CLASSIFIER_CONFIDENCE_FLOOR", "0.5"),
                ("RSI_STALL_CLASSIFIER_TIMEOUT_SECS", "15"),
            ],
            || {
                let c = Config::from_env();
                assert!(c.stall_classifier_enabled);
                assert_eq!(c.stall_classifier_model, "qwen3:14b");
                assert_eq!(c.stall_classifier_api_url, "http://api.example/v1");
                assert_eq!(c.stall_classifier_api_key.as_deref(), Some("k-test"));
                assert_eq!(c.stall_classifier_idle_secs, 300);
                assert_eq!(c.stall_classifier_idle_secs_codex, 900);
                assert_eq!(c.stall_classifier_cooldown_secs, 60);
                assert_eq!(c.stall_classifier_max_per_session, 9);
                assert!((c.stall_classifier_confidence_floor - 0.5).abs() < 1e-9);
                assert_eq!(c.stall_classifier_timeout_secs, 15);
            },
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn stall_classifier_env_confidence_floor_rejects_out_of_range() {
        with_clean_env_and(&[("RSI_STALL_CLASSIFIER_CONFIDENCE_FLOOR", "1.5")], || {
            let c = Config::from_env();
            // Out-of-range parses → ignored → default 0.7.
            assert!((c.stall_classifier_confidence_floor - 0.7).abs() < 1e-9);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_update_stall_classifier_enabled_round_trip() {
        with_clean_env(|| {
            let c = Config::from_env();
            let rc = RuntimeConfig::from_config(&c);
            assert!(
                !rc.stall_classifier_enabled
                    .load(std::sync::atomic::Ordering::Relaxed)
            );

            rc.update_field("stall_classifier_enabled", &serde_json::json!(true))
                .expect("update accepts bool");
            assert!(
                rc.stall_classifier_enabled
                    .load(std::sync::atomic::Ordering::Relaxed)
            );

            rc.update_field("stall_classifier_enabled", &serde_json::json!("nope"))
                .expect_err("rejects non-bool");
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_update_stall_classifier_model_validation() {
        with_clean_env(|| {
            let c = Config::from_env();
            let rc = RuntimeConfig::from_config(&c);
            rc.update_field("stall_classifier_model", &serde_json::json!("qwen3:14b"))
                .expect("string accepted");
            assert_eq!(*rc.stall_classifier_model.read(), "qwen3:14b");

            rc.update_field("stall_classifier_model", &serde_json::json!("   "))
                .expect_err("rejects whitespace-only");
            rc.update_field("stall_classifier_model", &serde_json::json!(42))
                .expect_err("rejects non-string");
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_update_stall_classifier_numeric_fields() {
        with_clean_env(|| {
            let c = Config::from_env();
            let rc = RuntimeConfig::from_config(&c);

            rc.update_field("stall_classifier_idle_secs", &serde_json::json!(120))
                .expect("u64");
            rc.update_field("stall_classifier_idle_secs_codex", &serde_json::json!(900))
                .expect("u64");
            rc.update_field("stall_classifier_cooldown_secs", &serde_json::json!(60))
                .expect("u64");
            rc.update_field(
                "stall_classifier_max_per_session",
                &serde_json::json!(11u64),
            )
            .expect("u32-coercible");

            use std::sync::atomic::Ordering;
            assert_eq!(rc.stall_classifier_idle_secs.load(Ordering::Relaxed), 120);
            assert_eq!(
                rc.stall_classifier_idle_secs_codex.load(Ordering::Relaxed),
                900
            );
            assert_eq!(
                rc.stall_classifier_cooldown_secs.load(Ordering::Relaxed),
                60
            );
            assert_eq!(
                rc.stall_classifier_max_per_session.load(Ordering::Relaxed),
                11
            );

            rc.update_field("stall_classifier_idle_secs", &serde_json::json!("oops"))
                .expect_err("rejects non-number");
            rc.update_field(
                "stall_classifier_max_per_session",
                &serde_json::json!(u64::MAX),
            )
            .expect_err("rejects u32 overflow");
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_update_confidence_floor_bounds() {
        with_clean_env(|| {
            let c = Config::from_env();
            let rc = RuntimeConfig::from_config(&c);

            rc.update_field(
                "stall_classifier_confidence_floor",
                &serde_json::json!(0.42),
            )
            .expect("0.42 in range");
            assert!((*rc.stall_classifier_confidence_floor.read() - 0.42).abs() < 1e-9);

            rc.update_field("stall_classifier_confidence_floor", &serde_json::json!(0.0))
                .expect("0.0 in range");
            rc.update_field("stall_classifier_confidence_floor", &serde_json::json!(1.0))
                .expect("1.0 in range");

            rc.update_field(
                "stall_classifier_confidence_floor",
                &serde_json::json!(-0.1),
            )
            .expect_err("rejects < 0");
            rc.update_field("stall_classifier_confidence_floor", &serde_json::json!(1.5))
                .expect_err("rejects > 1");
            rc.update_field(
                "stall_classifier_confidence_floor",
                &serde_json::json!("0.8"),
            )
            .expect_err("rejects string");
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn runtime_config_to_json_includes_stall_classifier_keys() {
        with_clean_env(|| {
            let c = Config::from_env();
            let rc = RuntimeConfig::from_config(&c);
            let j = rc.to_json();
            for k in [
                "stall_classifier_enabled",
                "stall_classifier_model",
                "stall_classifier_idle_secs",
                "stall_classifier_idle_secs_codex",
                "stall_classifier_cooldown_secs",
                "stall_classifier_max_per_session",
                "stall_classifier_confidence_floor",
            ] {
                assert!(j.get(k).is_some(), "expected key {} in to_json", k);
            }
        });
    }

    // --- end stall classifier ---

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn normalize_system_prompt_preset_accepts_all_canonical() {
        for s in ["default", "concise", "code-only", "caveman"] {
            assert_eq!(normalize_system_prompt_preset(s), Some(s));
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn normalize_system_prompt_preset_rejects_unknown() {
        assert_eq!(normalize_system_prompt_preset("garbage"), None);
        assert_eq!(normalize_system_prompt_preset(""), None);
        assert_eq!(normalize_system_prompt_preset("future-variant"), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn normalize_system_prompt_preset_normalizes_label_and_underscore_aliases() {
        assert_eq!(normalize_system_prompt_preset("Default"), Some("default"));
        assert_eq!(normalize_system_prompt_preset("DEFAULT"), Some("default"));
        assert_eq!(normalize_system_prompt_preset("Concise"), Some("concise"));
        assert_eq!(
            normalize_system_prompt_preset("Code Only"),
            Some("code-only")
        );
        assert_eq!(
            normalize_system_prompt_preset("CodeOnly"),
            Some("code-only")
        );
        assert_eq!(
            normalize_system_prompt_preset("code_only"),
            Some("code-only")
        );
        assert_eq!(normalize_system_prompt_preset("CAVEMAN"), Some("caveman"));
    }

    // --- C2 local-tracker: issue_tracker_config() kind selector ---

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn issue_tracker_config_nothing_set_is_none() {
        // env-guard (RSI-020): scrubs FLYWHEEL_*/MOTHERSHIP_* legacy fallbacks
        // (and the RSI_* primaries, via ENV_TRIPLES) before reading config.
        with_clean_env(|| {
            let config = Config::default();
            assert!(config.issue_tracker_config().is_none());
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn issue_tracker_config_unset_kind_with_linear_trio_is_linear() {
        with_clean_env_and(
            &[
                ("RSI_LINEAR_API_KEY", "key-123"),
                ("RSI_LINEAR_TEAM_ID", "team-123"),
                ("RSI_ISSUE_TRACKER_WORKING_DIR", "/tmp/linear-work"),
            ],
            || {
                let config = Config::default();
                let it = config
                    .issue_tracker_config()
                    .expect("unset kind + full Linear trio must be Some");
                assert_eq!(it.kind, "linear");
                assert_eq!(it.api_key, "key-123");
                assert_eq!(it.team_id, "team-123");
                assert_eq!(it.working_dir, std::path::PathBuf::from("/tmp/linear-work"));
                assert!(it.enabled);
            },
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn issue_tracker_config_explicit_linear_kind_matches_unset_behavior() {
        with_clean_env_and(
            &[
                ("RSI_ISSUE_TRACKER_KIND", "linear"),
                ("RSI_LINEAR_API_KEY", "key-123"),
                ("RSI_LINEAR_TEAM_ID", "team-123"),
                ("RSI_ISSUE_TRACKER_WORKING_DIR", "/tmp/linear-work"),
            ],
            || {
                let config = Config::default();
                let it = config
                    .issue_tracker_config()
                    .expect("explicit kind=linear + full trio must be Some");
                assert_eq!(it.kind, "linear");
                assert_eq!(it.api_key, "key-123");
                assert_eq!(it.team_id, "team-123");
            },
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn issue_tracker_config_kind_local_needs_working_dir_and_project_id() {
        with_clean_env_and(
            &[
                ("RSI_ISSUE_TRACKER_KIND", "local"),
                ("RSI_ISSUE_TRACKER_WORKING_DIR", "/tmp/local-work"),
                (
                    "RSI_ISSUE_TRACKER_PROJECT_ID",
                    "11111111-1111-4111-8111-111111111111",
                ),
            ],
            || {
                let config = Config::default();
                let it = config
                    .issue_tracker_config()
                    .expect("kind=local + working_dir + project id must be Some");
                assert_eq!(it.kind, "local");
                assert_eq!(it.api_key, "");
                assert_eq!(it.team_id, "");
                assert_eq!(it.working_dir, std::path::PathBuf::from("/tmp/local-work"));
                assert_eq!(
                    it.project_id,
                    Some(
                        uuid::Uuid::parse_str("11111111-1111-4111-8111-111111111111")
                            .expect("valid fixture UUID")
                    )
                );
                assert!(it.enabled);
            },
        );
    }

    /// Review F5b: P-001 precedence — an explicit `kind=local` wins even when
    /// the full Linear credential trio is also present.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn issue_tracker_config_kind_local_wins_over_full_linear_creds() {
        with_clean_env_and(
            &[
                ("RSI_ISSUE_TRACKER_KIND", "local"),
                ("RSI_ISSUE_TRACKER_WORKING_DIR", "/tmp/local-work"),
                (
                    "RSI_ISSUE_TRACKER_PROJECT_ID",
                    "11111111-1111-4111-8111-111111111111",
                ),
                ("RSI_LINEAR_API_KEY", "lin_key"),
                ("RSI_LINEAR_TEAM_ID", "lin_team"),
            ],
            || {
                let config = Config::default();
                let it = config
                    .issue_tracker_config()
                    .expect("kind=local with Linear creds present must still be Some(local)");
                assert_eq!(it.kind, "local", "local must win over Linear creds");
                assert_eq!(
                    it.api_key, "",
                    "Linear creds must be ignored on the local arm"
                );
                assert_eq!(it.team_id, "");
            },
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn issue_tracker_config_kind_local_without_working_dir_is_none() {
        with_clean_env_and(&[("RSI_ISSUE_TRACKER_KIND", "local")], || {
            let config = Config::default();
            assert!(config.issue_tracker_config().is_none());
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn issue_tracker_config_unknown_kind_is_none() {
        with_clean_env_and(
            &[
                ("RSI_ISSUE_TRACKER_KIND", "bogus"),
                ("RSI_ISSUE_TRACKER_WORKING_DIR", "/tmp/local-work"),
                ("RSI_LINEAR_API_KEY", "key-123"),
                ("RSI_LINEAR_TEAM_ID", "team-123"),
            ],
            || {
                let config = Config::default();
                assert!(config.issue_tracker_config().is_none());
            },
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn rsid_scope_limits_are_persisted_and_validated() {
        let runtime = RuntimeConfig::from_config(&Config::default());
        let initial = runtime.to_json();
        assert_eq!(
            initial["rsid_scope_memory_high_mib"],
            RSID_SCOPE_MEMORY_HIGH_MIB_DEFAULT
        );
        assert_eq!(
            initial["rsid_scope_memory_max_mib"],
            RSID_SCOPE_MEMORY_MAX_MIB_DEFAULT
        );
        assert_eq!(
            initial["rsid_scope_memory_swap_max_mib"],
            RSID_SCOPE_MEMORY_SWAP_MAX_MIB_DEFAULT
        );
        assert_eq!(
            initial["rsid_scope_cpu_weight"],
            RSID_SCOPE_CPU_WEIGHT_DEFAULT
        );

        for field in [
            "rsid_scope_memory_high_mib",
            "rsid_scope_memory_max_mib",
            "rsid_scope_memory_swap_max_mib",
            "rsid_scope_cpu_weight",
        ] {
            assert!(is_persisted_runtime_config_field(field), "{field}");
        }

        runtime
            .update_field("rsid_scope_memory_high_mib", &serde_json::json!(7168))
            .expect("valid MemoryHigh value");
        runtime
            .update_field("rsid_scope_memory_swap_max_mib", &serde_json::json!(0))
            .expect("zero swap limit disables daemon swap");
        runtime
            .update_field("rsid_scope_cpu_weight", &serde_json::json!(35))
            .expect("valid CPUWeight value");
        assert!(
            runtime
                .update_field("rsid_scope_memory_high_mib", &serde_json::json!(255))
                .is_err()
        );
        assert!(
            runtime
                .update_field("rsid_scope_memory_high_mib", &serde_json::json!(8192))
                .is_err()
        );
        assert!(
            runtime
                .update_field("rsid_scope_memory_max_mib", &serde_json::json!(7168))
                .is_err()
        );
        assert!(
            runtime
                .update_field("rsid_scope_memory_max_mib", &serde_json::json!("8192"))
                .is_err()
        );
        runtime
            .update_field("rsid_scope_memory_max_mib", &serde_json::json!(12_288))
            .expect("raise MemoryMax before MemoryHigh");
        runtime
            .update_field("rsid_scope_memory_high_mib", &serde_json::json!(8192))
            .expect("MemoryHigh remains below MemoryMax");
        assert!(
            runtime
                .update_field("rsid_scope_cpu_weight", &serde_json::json!(10_001))
                .is_err()
        );
        assert_eq!(runtime.to_json()["rsid_scope_cpu_weight"], 35);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn worker_scope_limits_are_independent_and_validated() {
        let runtime = RuntimeConfig::from_config(&Config::default());
        let initial = runtime.to_json();
        let (default_high, default_max) = rsi_common::worker_memory::default_limits_mib();
        assert_eq!(initial["worker_scope_memory_high_mib"], default_high);
        assert_eq!(initial["worker_scope_memory_max_mib"], default_max);
        assert_eq!(initial["worker_scope_memory_swap_max_mib"], 0);
        assert_eq!(initial["worker_scope_cpu_weight"], 20);
        for field in [
            "worker_scope_memory_high_mib",
            "worker_scope_memory_max_mib",
            "worker_scope_memory_swap_max_mib",
            "worker_scope_cpu_weight",
        ] {
            assert!(is_persisted_runtime_config_field(field), "{field}");
        }
        runtime
            .update_field("worker_scope_memory_max_mib", &serde_json::json!(1_048_576))
            .unwrap();
        runtime
            .update_field("worker_scope_memory_high_mib", &serde_json::json!(7168))
            .unwrap();
        runtime
            .update_field("worker_scope_memory_max_mib", &serde_json::json!(8192))
            .unwrap();
        assert!(
            runtime
                .update_field("worker_scope_memory_max_mib", &serde_json::json!(7168))
                .is_err()
        );
        assert!(
            runtime
                .update_field("worker_scope_memory_high_mib", &serde_json::json!(8192))
                .is_err()
        );
        assert!(
            runtime
                .update_field(
                    "worker_scope_memory_swap_max_mib",
                    &serde_json::json!(1_048_577)
                )
                .is_err()
        );
        assert!(
            runtime
                .update_field("worker_scope_cpu_weight", &serde_json::json!(10_001))
                .is_err()
        );
        assert_eq!(runtime.to_json()["rsid_scope_memory_high_mib"], 6144);
        assert_eq!(runtime.to_json()["worker_scope_memory_high_mib"], 7168);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn openrouter_route_settings_default_override_and_snapshot() {
        let runtime = RuntimeConfig::from_config(&Config::default());
        assert_eq!(
            runtime.openrouter_route_for("qwen/qwen3-coder-next"),
            OpenRouterRoute::CodexCli
        );
        assert!(runtime.api_route_fallback.load(Ordering::Relaxed));
        runtime
            .update_field("api_route.openrouter", &serde_json::json!("harness"))
            .unwrap();
        runtime
            .update_field(
                "api_route.openrouter.qwen/qwen3-coder-next",
                &serde_json::json!("codex_cli"),
            )
            .unwrap();
        runtime
            .update_field("api_route.fallback", &serde_json::json!(false))
            .unwrap();
        assert_eq!(
            runtime.openrouter_route_for("deepseek/deepseek-v4.1-flash"),
            OpenRouterRoute::Harness
        );
        assert_eq!(
            runtime.openrouter_route_for("openrouter/qwen/qwen3-coder-next"),
            OpenRouterRoute::CodexCli
        );
        assert_eq!(
            runtime.to_json()["api_route.openrouter.qwen/qwen3-coder-next"],
            "codex_cli"
        );
        assert_eq!(runtime.to_json()["api_route.fallback"], false);
        assert!(is_persisted_runtime_config_field(
            "api_route.openrouter.qwen/qwen3-coder-next"
        ));
        runtime
            .update_field(
                "api_route.openrouter.qwen/qwen3-coder-next",
                &serde_json::Value::Null,
            )
            .unwrap();
        assert_eq!(
            runtime.openrouter_route_for("qwen/qwen3-coder-next"),
            OpenRouterRoute::Harness
        );
        assert!(
            runtime
                .update_field("api_route.openrouter", &serde_json::json!("invalid"))
                .is_err()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn openrouter_context_budget_defaults_bounds_and_turns_off_at_zero() {
        let runtime = RuntimeConfig::from_config(&Config::default());
        assert_eq!(runtime.openrouter_context_budget(), Some(128_000));
        assert_eq!(
            runtime.to_json()["openrouter_context_budget_tokens"],
            128_000
        );
        assert!(is_persisted_runtime_config_field(
            "openrouter_context_budget_tokens"
        ));
        for accepted in [32_000, 200_000, 2_000_000] {
            runtime
                .update_field(
                    "openrouter_context_budget_tokens",
                    &serde_json::json!(accepted),
                )
                .unwrap();
            assert_eq!(runtime.openrouter_context_budget(), Some(accepted));
        }
        for refused in [
            serde_json::json!(31_999),
            serde_json::json!(2_000_001),
            serde_json::json!(-1),
            serde_json::json!("128000"),
        ] {
            let error = runtime
                .update_field("openrouter_context_budget_tokens", &refused)
                .unwrap_err();
            assert!(error.contains("32000..=2000000"), "{error}");
        }
        assert_eq!(runtime.openrouter_context_budget(), Some(2_000_000));
        runtime
            .update_field("openrouter_context_budget_tokens", &serde_json::json!(0))
            .unwrap();
        assert_eq!(runtime.openrouter_context_budget(), None);
        assert_eq!(runtime.to_json()["openrouter_context_budget_tokens"], 0);
    }

    /// #1050: the per-turn Harness iteration cap defaults to 150, accepts
    /// 10..=1000 and refuses anything else without changing the value.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn harness_max_iterations_default_bounds_and_persistence() {
        let runtime = RuntimeConfig::from_config(&Config::default());
        assert_eq!(runtime.harness_max_iterations(), 150);
        assert_eq!(runtime.to_json()["harness_max_iterations_per_turn"], 150);
        assert!(is_persisted_runtime_config_field(
            "harness_max_iterations_per_turn"
        ));
        for accepted in [10_u64, 25, 1000] {
            runtime
                .update_field(
                    "harness_max_iterations_per_turn",
                    &serde_json::json!(accepted),
                )
                .unwrap();
            assert_eq!(runtime.harness_max_iterations(), accepted as u32);
        }
        for refused in [
            serde_json::json!(9),
            serde_json::json!(1001),
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!("150"),
        ] {
            assert!(
                runtime
                    .update_field("harness_max_iterations_per_turn", &refused)
                    .is_err(),
                "{refused}"
            );
        }
        assert_eq!(runtime.harness_max_iterations(), 1000);
        assert_eq!(
            runtime.persisted_field_value("harness_max_iterations_per_turn"),
            Some(serde_json::json!(1000))
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn openrouter_route_settings_survive_daemon_restart() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&directory.path().join("settings.db")).unwrap();
        let active = RuntimeConfig::from_config(&Config::default());
        for (field, value) in [
            ("api_route.openrouter", serde_json::json!("harness")),
            (
                "api_route.openrouter.qwen/qwen3-coder-next",
                serde_json::json!("codex_cli"),
            ),
            ("api_route.fallback", serde_json::json!(false)),
        ] {
            active.update_field(field, &value).unwrap();
            crate::store::daemon_settings::persist_runtime_config_field(&store, &active, field)
                .unwrap();
        }
        let restarted = RuntimeConfig::from_config(&Config::default());
        crate::store::daemon_settings::apply_persisted_runtime_config(&store, &restarted).unwrap();
        assert_eq!(
            restarted.openrouter_route_for("z-ai/glm-5.3-flashx"),
            OpenRouterRoute::Harness
        );
        assert_eq!(
            restarted.openrouter_route_for("qwen/qwen3-coder-next"),
            OpenRouterRoute::CodexCli
        );
        assert!(!restarted.api_route_fallback.load(Ordering::Relaxed));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn bedrock_route_settings_round_trip_and_survive_daemon_restart() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&directory.path().join("settings.db")).unwrap();
        let active = RuntimeConfig::from_config(&Config::default());
        assert_eq!(
            active.bedrock_route_for("global.openai.gpt-5.6-sol"),
            OpenRouterRoute::CodexCli
        );
        for (field, value) in [
            ("api_route.bedrock", serde_json::json!("harness")),
            (
                "api_route.bedrock.global.openai.gpt-5.6-sol",
                serde_json::json!("codex_cli"),
            ),
        ] {
            active.update_field(field, &value).unwrap();
            crate::store::daemon_settings::persist_runtime_config_field(&store, &active, field)
                .unwrap();
        }
        assert_eq!(active.to_json()["api_route.bedrock"], "harness");
        assert_eq!(
            active.to_json()["api_route.bedrock.global.openai.gpt-5.6-sol"],
            "codex_cli"
        );
        let restarted = RuntimeConfig::from_config(&Config::default());
        crate::store::daemon_settings::apply_persisted_runtime_config(&store, &restarted).unwrap();
        assert_eq!(
            restarted.bedrock_route_for("other-model"),
            OpenRouterRoute::Harness
        );
        assert_eq!(
            restarted.bedrock_route_for("bedrock/global.openai.gpt-5.6-sol"),
            OpenRouterRoute::CodexCli
        );
        assert!(
            restarted
                .update_field("api_route.bedrock", &serde_json::json!("invalid"))
                .is_err()
        );
        restarted
            .update_field(
                "api_route.bedrock.global.openai.gpt-5.6-sol",
                &serde_json::Value::Null,
            )
            .unwrap();
        assert_eq!(
            restarted.bedrock_route_for("global.openai.gpt-5.6-sol"),
            OpenRouterRoute::Harness
        );
    }
}
