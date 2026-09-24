//! User settings — centralized configuration for display, status bar, and defaults.

use crate::app::SortOrder;
use crate::types::{CardField, NavigatorOptionalColumn, NavigatorPreset};
use rsi_common::model_control::{ModelControlMode, ModelControlStatusReport};
use rsi_common::sandbox_storage::SandboxBuildCacheReclaimReport;
use rsi_common::types::SessionProvider;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

pub const NAVIGATOR_SETTINGS_ROW_COUNT: usize = 1 + NavigatorOptionalColumn::ALL.len();

/// Configuration for the prompt compiler (Ctrl+Y) and grammar correction (Ctrl+Shift+G).
/// The new-session modal may override this target with its selected provider/model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptProcessorConfig {
    pub enabled: bool,
    pub model: String,
    #[serde(default = "default_prompt_processor_provider")]
    pub provider: SessionProvider,
    #[serde(default)]
    pub custom_provider_id: Option<Uuid>,
    #[serde(default)]
    pub custom_base_url: Option<String>,
    #[serde(default)]
    pub custom_api_key: Option<String>,
}

fn default_prompt_processor_provider() -> SessionProvider {
    SessionProvider::Local
}

impl Default for PromptProcessorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: "gemma4:e4b".to_string(),
            provider: default_prompt_processor_provider(),
            custom_provider_id: None,
            custom_base_url: None,
            custom_api_key: None,
        }
    }
}

/// A user-defined OpenAI-compatible provider (stored in tui-state.json).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustomProviderEntry {
    /// Locally generated UUID — stable identifier for this config.
    pub id: Uuid,
    /// Human-readable display name (e.g. "Groq", "My Ollama").
    pub name: String,
    /// API base URL (e.g. "https://api.groq.com/openai/v1").
    pub base_url: String,
    /// API key (plaintext, stored in ~/.flywheel/tui-state.json).
    pub api_key: String,
    /// Default model ID for this provider (e.g. "llama-3.3-70b-versatile").
    pub default_model: String,
}

/// Message bridge managed from the Settings pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageBridgeKind {
    Signal,
    Imessage,
}

impl MessageBridgeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Signal => "Signal",
            Self::Imessage => "iMessage",
        }
    }

    pub fn config_filename(self) -> &'static str {
        match self {
            Self::Signal => "signal.toml",
            Self::Imessage => "imessage.toml",
        }
    }
}

/// Connection settings shared by phone-message bridge configs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageBridgeSettings {
    #[serde(default)]
    pub enabled: bool,
    /// Signal account number. Ignored by iMessage.
    #[serde(default)]
    pub account: String,
    /// Comma-separated allowlist of phone numbers or iMessage handles.
    #[serde(default)]
    pub allow_from: String,
    /// Default working directory for sessions launched from the bridge.
    #[serde(default = "default_bridge_working_dir")]
    pub working_dir: String,
}

impl Default for MessageBridgeSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            account: String::new(),
            allow_from: String::new(),
            working_dir: default_bridge_working_dir(),
        }
    }
}

/// Persisted user-facing bridge connection settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageBridgeConnections {
    #[serde(default)]
    pub signal: MessageBridgeSettings,
    #[serde(default)]
    pub imessage: MessageBridgeSettings,
}

impl Default for MessageBridgeConnections {
    fn default() -> Self {
        Self {
            signal: MessageBridgeSettings::default(),
            imessage: MessageBridgeSettings::default(),
        }
    }
}

impl MessageBridgeConnections {
    pub fn get(&self, kind: MessageBridgeKind) -> &MessageBridgeSettings {
        match kind {
            MessageBridgeKind::Signal => &self.signal,
            MessageBridgeKind::Imessage => &self.imessage,
        }
    }

    pub fn get_mut(&mut self, kind: MessageBridgeKind) -> &mut MessageBridgeSettings {
        match kind {
            MessageBridgeKind::Signal => &mut self.signal,
            MessageBridgeKind::Imessage => &mut self.imessage,
        }
    }
}

/// A session list card field entry with enabled state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardFieldEntry {
    pub field: CardField,
    pub enabled: bool,
}

/// A daemon feature toggle entry. Values are fetched live from the daemon; the
/// daemon persists user changes in SQLite.
#[derive(Debug, Clone)]
pub struct DaemonFeatureEntry {
    /// JSON field name used in RPC (e.g. "retry_enabled").
    pub field: String,
    /// Human-readable display label.
    pub label: String,
    /// Current value: either a bool toggle or a numeric/string display.
    pub value: DaemonFeatureValue,
}

/// The value of a daemon feature — either a bool toggle, a display-only
/// string, or a cyclable set of options (Enter cycles to the next option).
#[derive(Debug, Clone)]
pub enum DaemonFeatureValue {
    Bool(bool),
    Display(String),
    /// Multi-value cycler. `options` is the canonical list (e.g.
    /// `["read-only", "workspace-write", "danger-full-access"]`). `current`
    /// is the index of the active option. Enter on this row advances
    /// `(current + 1) % options.len()` and dispatches `UpdateDaemonConfig`.
    Cycle {
        options: Vec<String>,
        current: usize,
    },
}

impl DaemonFeatureEntry {
    /// Build the default feature list with unknown/pending state (before RPC fetch).
    pub fn defaults() -> Vec<Self> {
        vec![
            Self {
                field: "model_control_mode".to_string(),
                label: "Model control mode".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: vec![
                        "normal".to_string(),
                        "pause-background".to_string(),
                        "deny-paid".to_string(),
                        "local-only".to_string(),
                        "stop-all".to_string(),
                    ],
                    current: 0,
                },
            },
            Self {
                field: "model_control_stop_all".to_string(),
                label: "Emergency stop".to_string(),
                value: DaemonFeatureValue::Display("Press Enter".to_string()),
            },
            Self {
                field: "retry_enabled".to_string(),
                label: "Retry on failure".to_string(),
                value: DaemonFeatureValue::Bool(false),
            },
            Self {
                field: "retry_max_default".to_string(),
                label: "Max retries".to_string(),
                value: DaemonFeatureValue::Display("?".to_string()),
            },
            Self {
                field: "retry_on_stall".to_string(),
                label: "Retry on stall".to_string(),
                value: DaemonFeatureValue::Bool(false),
            },
            Self {
                field: "retry_max_backoff_ms".to_string(),
                label: "Retry max backoff".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: ["5000", "15000", "30000", "60000", "120000", "300000"]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    current: 3,
                },
            },
            Self {
                field: "reconciliation_enabled".to_string(),
                label: "Reconciliation loop".to_string(),
                value: DaemonFeatureValue::Bool(false),
            },
            Self {
                field: "stall_detection_enabled".to_string(),
                label: "Stall detection".to_string(),
                value: DaemonFeatureValue::Bool(false),
            },
            Self {
                field: "context_rotation_enabled".to_string(),
                label: "Context rotation".to_string(),
                value: DaemonFeatureValue::Bool(false),
            },
            Self {
                field: "memory_enabled".to_string(),
                label: "Memory system".to_string(),
                value: DaemonFeatureValue::Bool(false),
            },
            Self {
                field: "codegraph_indexing_enabled".to_string(),
                label: "Codegraph indexing".to_string(),
                value: DaemonFeatureValue::Bool(false),
            },
            Self {
                field: "queue_enabled".to_string(),
                label: "Background queue".to_string(),
                value: DaemonFeatureValue::Bool(false),
            },
            Self {
                field: "recursive_dag_recovery_controls_enabled".to_string(),
                label: "Recursive DAG recovery controls".to_string(),
                value: DaemonFeatureValue::Bool(false),
            },
            Self {
                field: "recursive_dag_scheduler_controls_enabled".to_string(),
                label: "Recursive DAG scheduler controls".to_string(),
                value: DaemonFeatureValue::Bool(false),
            },
            Self {
                field: "recursive_dag_cancellation_controls_enabled".to_string(),
                label: "Recursive DAG cancellation controls".to_string(),
                value: DaemonFeatureValue::Bool(false),
            },
            Self {
                field: "recursive_dag_live_scheduler_control_enabled".to_string(),
                label: "Recursive DAG live scheduler".to_string(),
                value: DaemonFeatureValue::Bool(false),
            },
            Self {
                field: "recursive_dag_run_lease_ttl_ms".to_string(),
                label: "Recursive DAG run lease TTL".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: ["30000", "60000", "300000", "600000", "1800000"]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    current: 2,
                },
            },
            Self {
                field: "recursive_dag_max_concurrent_graphs".to_string(),
                label: "Recursive DAG max concurrent graphs".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: ["1", "2", "4", "8", "16"]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    current: 1,
                },
            },
            Self {
                field: "dream_enabled".to_string(),
                label: "Dream consolidation".to_string(),
                value: DaemonFeatureValue::Bool(false),
            },
            Self {
                field: "dialectic_enabled".to_string(),
                label: "Dialectic engine".to_string(),
                value: DaemonFeatureValue::Bool(false),
            },
            Self {
                field: "dream_observation_threshold".to_string(),
                label: "Observation threshold".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: ["10", "25", "50", "100", "250"]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    current: 2,
                },
            },
            Self {
                field: "dream_cooldown_secs".to_string(),
                label: "Dream cooldown".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: ["1800", "3600", "7200", "14400", "28800", "86400"]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    current: 4,
                },
            },
            Self {
                field: "dream_idle_secs".to_string(),
                label: "Dream idle wait".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: ["60", "300", "900", "1800", "3600"]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    current: 1,
                },
            },
            Self {
                field: "codex_sandbox_mode".to_string(),
                label: "Codex sandbox".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: vec![
                        "read-only".to_string(),
                        "workspace-write".to_string(),
                        "danger-full-access".to_string(),
                    ],
                    // danger-full-access is the canonical default — overwritten by
                    // update_from_json after the first GetDaemonConfig response.
                    current: 2,
                },
            },
            Self {
                field: "claude_config_isolation".to_string(),
                label: "Claude project config isolation".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: vec![
                        "off".to_string(),
                        "settings".to_string(),
                        "strict".to_string(),
                    ],
                    // "off" is the canonical default (preserves the historical
                    // launch argv) — overwritten by update_from_json after the
                    // first GetDaemonConfig response.
                    current: 0,
                },
            },
            Self {
                field: "system_prompt_preset".to_string(),
                label: "System prompt preset".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: vec![
                        "default".to_string(),
                        "concise".to_string(),
                        "code-only".to_string(),
                        "caveman".to_string(),
                    ],
                    // "default" is the canonical default — overwritten by
                    // update_from_json after the first GetDaemonConfig response.
                    current: 0,
                },
            },
            Self {
                field: "orchestration_max_child_effort".to_string(),
                label: "Orchestration max child effort".to_string(),
                value: DaemonFeatureValue::Cycle {
                    // Sourced from rsi-common so this row cannot drift from the
                    // daemon's UpdateDaemonConfig validator or its
                    // admission-time read path (issues #34/#35).
                    options: rsi_common::model_control::ORCHESTRATION_MAX_CHILD_EFFORT_CHOICES
                        .iter()
                        .map(|choice| (*choice).to_string())
                        .collect(),
                    // "unset" is the canonical default — overwritten by
                    // update_from_json after the first GetDaemonConfig response.
                    current: 0,
                },
            },
            Self {
                field: "sandbox_build_cache_reclaim_enabled".to_string(),
                label: "Sandbox cache reclaim".to_string(),
                value: DaemonFeatureValue::Bool(true),
            },
            Self {
                field: "source_worktree_settlement".to_string(),
                label: "⚠ Source worktree settlement".to_string(),
                value: DaemonFeatureValue::Display("Open destructive audit…".to_string()),
            },
            Self {
                field: "sandbox_build_cache_reclaim_ttl_secs".to_string(),
                label: "Cache reclaim TTL".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: ["900", "3600", "21600", "43200", "86400"]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    current: 2,
                },
            },
            Self {
                field: "sandbox_build_cache_reclaim_interval_secs".to_string(),
                label: "Cache reclaim interval".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: ["60", "300", "900", "3600", "21600"]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    current: 3,
                },
            },
            Self {
                field: "sandbox_build_cache_reclaim_high_watermark_pct".to_string(),
                label: "Cache pressure high".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: ["80", "85", "90", "95"]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    current: 1,
                },
            },
            Self {
                field: "sandbox_build_cache_reclaim_low_watermark_pct".to_string(),
                label: "Cache pressure low".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: ["60", "65", "70", "75"]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    current: 3,
                },
            },
            Self {
                field: "sandbox_build_cache_reclaim_max_candidates".to_string(),
                label: "Cache reclaim pass limit".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: ["8", "16", "32", "64", "128", "256"]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    current: 3,
                },
            },
            Self {
                field: "sandbox_storage_status".to_string(),
                label: "Sandbox storage".to_string(),
                value: DaemonFeatureValue::Display("R to refresh".to_string()),
            },
            Self {
                field: "sandbox_build_cache_dry_run".to_string(),
                label: "Preview cache reclaim".to_string(),
                value: DaemonFeatureValue::Display("Press Enter".to_string()),
            },
            Self {
                field: "sandbox_build_cache_reclaim_now".to_string(),
                label: "Reclaim sandbox caches now".to_string(),
                value: DaemonFeatureValue::Display("Press Enter".to_string()),
            },
            Self {
                field: "rsid_scope_memory_high_mib".to_string(),
                label: "rsid MemoryHigh (MiB)".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: ["2048", "4096", "6144", "8192", "12288", "16384"]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    current: 2,
                },
            },
            Self {
                field: "rsid_scope_memory_max_mib".to_string(),
                label: "rsid MemoryMax (MiB)".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: ["4096", "6144", "8192", "12288", "16384", "32768"]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    current: 2,
                },
            },
            Self {
                field: "rsid_scope_memory_swap_max_mib".to_string(),
                label: "rsid MemorySwapMax (MiB)".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: ["0", "1024", "2048", "4096", "8192", "16384"]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    current: 0,
                },
            },
            Self {
                field: "rsid_scope_cpu_weight".to_string(),
                label: "rsid CPUWeight".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: ["10", "20", "50", "100", "200", "500"]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    current: 1,
                },
            },
            Self {
                field: "stall_classifier_enabled".to_string(),
                label: "Stall classifier".to_string(),
                value: DaemonFeatureValue::Bool(false),
            },
            Self {
                field: "stall_classifier_model".to_string(),
                label: "Classifier model".to_string(),
                value: DaemonFeatureValue::Display("?".to_string()),
            },
            Self {
                field: "stall_classifier_idle_secs".to_string(),
                label: "Classifier idle threshold (Claude)".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: vec![
                        "300".to_string(),
                        "600".to_string(),
                        "900".to_string(),
                        "1200".to_string(),
                    ],
                    current: 1,
                },
            },
            Self {
                field: "stall_classifier_idle_secs_codex".to_string(),
                label: "Classifier idle threshold (Codex)".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: vec![
                        "900".to_string(),
                        "1200".to_string(),
                        "1800".to_string(),
                        "2700".to_string(),
                        "3600".to_string(),
                    ],
                    current: 2,
                },
            },
            Self {
                field: "stall_classifier_cooldown_secs".to_string(),
                label: "Classifier cooldown".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: vec![
                        "300".to_string(),
                        "600".to_string(),
                        "1200".to_string(),
                        "1800".to_string(),
                        "3600".to_string(),
                    ],
                    current: 3,
                },
            },
            Self {
                field: "stall_classifier_max_per_session".to_string(),
                label: "Classifier max per session".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: vec![
                        "1".to_string(),
                        "2".to_string(),
                        "3".to_string(),
                        "5".to_string(),
                        "10".to_string(),
                    ],
                    current: 2,
                },
            },
            Self {
                field: "stall_classifier_confidence_floor".to_string(),
                label: "Classifier confidence floor".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: vec![
                        "0.5".to_string(),
                        "0.6".to_string(),
                        "0.7".to_string(),
                        "0.8".to_string(),
                        "0.9".to_string(),
                    ],
                    current: 2,
                },
            },
            Self {
                field: "gv_render_recursive_origin".to_string(),
                label: "Graph overlay: render recursive origin".to_string(),
                value: DaemonFeatureValue::Bool(false),
            },
            Self {
                field: "gv_info_dashboard".to_string(),
                label: "Graph overlay: info dashboard".to_string(),
                value: DaemonFeatureValue::Bool(false),
            },
            Self {
                field: "topology_executor_enabled".to_string(),
                label: "Durable topology executor".to_string(),
                value: DaemonFeatureValue::Bool(true),
            },
            Self {
                field: "topology_max_concurrent_build_nodes".to_string(),
                label: "Topology build nodes".to_string(),
                value: DaemonFeatureValue::Cycle {
                    options: vec![
                        "1".to_string(),
                        "2".to_string(),
                        "3".to_string(),
                        "4".to_string(),
                    ],
                    current: 1,
                },
            },
        ]
    }

    /// Update entries from a JSON daemon config response. Preserves the
    /// existing variant discriminant — `Bool` stays `Bool`, `Cycle` stays
    /// `Cycle` (with its canonical options list reused), so the panel UI
    /// never mutates between bool/cycle just because the daemon's response
    /// shape changed.
    pub fn update_from_json(entries: &mut Vec<Self>, json: &serde_json::Value) {
        for entry in entries.iter_mut() {
            if let Some(val) = json.get(&entry.field) {
                entry.value = match &entry.value {
                    DaemonFeatureValue::Bool(_) => {
                        DaemonFeatureValue::Bool(val.as_bool().unwrap_or(false))
                    }
                    DaemonFeatureValue::Display(_) => DaemonFeatureValue::Display(match val {
                        serde_json::Value::Number(n) => n.to_string(),
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Bool(b) => b.to_string(),
                        _ => val.to_string(),
                    }),
                    DaemonFeatureValue::Cycle {
                        options,
                        current: _,
                    } => {
                        let new_str = match val {
                            serde_json::Value::String(s) => s.clone(),
                            serde_json::Value::Number(n) => n.to_string(),
                            serde_json::Value::Bool(b) => b.to_string(),
                            _ => String::new(),
                        };
                        let mut options = options.clone();
                        if options.iter().all(|option| option.parse::<u64>().is_ok())
                            && new_str.parse::<u64>().is_ok()
                        {
                            options.push(new_str.clone());
                            options.sort_by_key(|option| option.parse::<u64>().unwrap_or(u64::MAX));
                            options.dedup();
                        }
                        let current = options.iter().position(|o| o == &new_str).unwrap_or(0);
                        DaemonFeatureValue::Cycle { options, current }
                    }
                };
            }
        }
    }

    pub fn update_sandbox_storage_report(
        entries: &mut [Self],
        report: &SandboxBuildCacheReclaimReport,
    ) {
        let summary = sandbox_storage_report_summary(report);
        if let Some(entry) = entries
            .iter_mut()
            .find(|entry| entry.field == "sandbox_storage_status")
        {
            entry.value = DaemonFeatureValue::Display(summary);
        }
    }

    pub(crate) fn update_sandbox_storage_status(
        entries: &mut [Self],
        status: &crate::app::bootstrap::SandboxStorageStatus,
    ) {
        use crate::app::bootstrap::SandboxStorageStatus;

        let summary = match status {
            SandboxStorageStatus::Unknown => "Unknown · R to refresh".to_string(),
            SandboxStorageStatus::RefreshingUnknown => "Refreshing · no preview yet".to_string(),
            SandboxStorageStatus::Fresh {
                report,
                observed_at,
            } => format!(
                "Fresh {} · {}",
                observed_at.format("%H:%M:%S UTC"),
                sandbox_storage_report_summary(report)
            ),
            SandboxStorageStatus::RefreshingStale {
                report,
                observed_at,
            } => format!(
                "Refreshing · stale since {} · {}",
                observed_at.format("%H:%M:%S UTC"),
                sandbox_storage_report_summary(report)
            ),
            SandboxStorageStatus::Error {
                message,
                last_success_at,
            } => match last_success_at {
                Some(observed_at) => format!(
                    "Error: {message} · last success {}",
                    observed_at.format("%H:%M:%S UTC")
                ),
                None => format!("Error: {message} · no successful preview"),
            },
        };
        if let Some(entry) = entries
            .iter_mut()
            .find(|entry| entry.field == "sandbox_storage_status")
        {
            entry.value = DaemonFeatureValue::Display(summary);
        }
    }

    pub fn update_sandbox_storage_action(
        entries: &mut [Self],
        report: &SandboxBuildCacheReclaimReport,
        dry_run: bool,
    ) {
        let (field, summary) = if dry_run {
            (
                "sandbox_build_cache_dry_run",
                format!(
                    "{} eligible / {} checked · capacity estimate unavailable",
                    report.eligible_candidates, report.candidates_considered
                ),
            )
        } else {
            (
                "sandbox_build_cache_reclaim_now",
                format!(
                    "{} rm · {} pend",
                    report.fully_removed_count, report.pending_count
                ),
            )
        };
        if let Some(entry) = entries.iter_mut().find(|entry| entry.field == field) {
            entry.value = DaemonFeatureValue::Display(summary);
        }
    }

    pub fn set_sandbox_storage_action_failed(entries: &mut [Self], dry_run: bool) {
        let field = if dry_run {
            "sandbox_build_cache_dry_run"
        } else {
            "sandbox_build_cache_reclaim_now"
        };
        if let Some(entry) = entries.iter_mut().find(|entry| entry.field == field) {
            entry.value = DaemonFeatureValue::Display("FAILED".to_string());
        }
    }

    pub fn set_sandbox_storage_refresh_failed(entries: &mut [Self], error: &str) {
        if let Some(entry) = entries
            .iter_mut()
            .find(|entry| entry.field == "sandbox_storage_status")
        {
            entry.value = DaemonFeatureValue::Display(format!("Refresh failed: {error}"));
        }
    }

    pub fn update_model_control(entries: &mut [Self], status: &ModelControlStatusReport) {
        for entry in entries.iter_mut() {
            match entry.field.as_str() {
                "model_control_mode" => {
                    if let DaemonFeatureValue::Cycle { options, current } = &mut entry.value {
                        let slug = model_control_mode_slug(status.mode);
                        *current = options
                            .iter()
                            .position(|option| option == slug)
                            .unwrap_or(0);
                    }
                }
                "model_control_stop_all" => {
                    entry.value = DaemonFeatureValue::Display(format!(
                        "Enter to stop all ({})",
                        status.circuit_state
                    ));
                }
                _ => {}
            }
        }
    }

    /// Current value of a `Display`-kind daemon feature entry, or `None` if
    /// it hasn't loaded yet (still the `"?"` placeholder), isn't found, or
    /// isn't `Display`-kind. Used to bridge a daemon-owned field (e.g.
    /// `stall_classifier_model`) into an Agent Actors row, which otherwise
    /// reads its "current" value from `UserSettings`.
    pub fn display_value<'a>(entries: &'a [Self], field: &str) -> Option<&'a str> {
        entries
            .iter()
            .find(|e| e.field == field)
            .and_then(|e| match &e.value {
                DaemonFeatureValue::Display(s) if s != "?" => Some(s.as_str()),
                _ => None,
            })
    }
}

fn format_binary_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn sandbox_storage_report_summary(report: &SandboxBuildCacheReclaimReport) -> String {
    format!(
        "{} checked · {} eligible · {}% used · {} free · capacity estimate unavailable",
        report.candidates_considered,
        report.eligible_candidates,
        report.filesystem_before.used_percent,
        format_binary_bytes(report.filesystem_before.available_bytes)
    )
}

pub fn model_control_mode_slug(mode: ModelControlMode) -> &'static str {
    match mode {
        ModelControlMode::Normal => "normal",
        ModelControlMode::PauseBackground => "pause-background",
        ModelControlMode::DenyPaid => "deny-paid",
        ModelControlMode::LocalOnly => "local-only",
        ModelControlMode::StopAll => "stop-all",
    }
}

/// System prompt preset for AI session behavior.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SystemPromptPreset {
    /// No custom system prompt — use daemon defaults only.
    #[default]
    Default,
    /// Reduced verbosity — keep explanations but cut filler.
    Concise,
    /// Minimal explanations — code output prioritized.
    CodeOnly,
    /// Full caveman-style compression — max token efficiency.
    Caveman,
}

impl SystemPromptPreset {
    pub const ALL: &[SystemPromptPreset] =
        &[Self::Default, Self::Concise, Self::CodeOnly, Self::Caveman];

    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Concise => "Concise",
            Self::CodeOnly => "Code Only",
            Self::Caveman => "Caveman",
        }
    }

    /// Returns the system prompt text for this preset, or None for Default.
    pub fn content(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            Self::Concise => Some(PRESET_CONCISE),
            Self::CodeOnly => Some(PRESET_CODE_ONLY),
            Self::Caveman => Some(PRESET_CAVEMAN),
        }
    }

    /// Canonical kebab-case slug for this preset (RSI-026). Matches the
    /// daemon's `daemon_settings.system_prompt_preset` value format and
    /// the `UpdateDaemonConfig` wire format.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Concise => "concise",
            Self::CodeOnly => "code-only",
            Self::Caveman => "caveman",
        }
    }

    /// Inverse of `slug()`: map a daemon-supplied canonical slug back to
    /// the enum. Unknown slugs fall back to `Default` to keep the cache safe
    /// even when the daemon ships a variant the TUI doesn't know about.
    pub fn from_slug(slug: &str) -> Self {
        match slug {
            "concise" => Self::Concise,
            "code-only" => Self::CodeOnly,
            "caveman" => Self::Caveman,
            _ => Self::Default,
        }
    }
}

const PRESET_CONCISE: &str = "\
Respond concisely. Drop filler words (just/really/basically/actually/simply), \
pleasantries (sure/certainly/of course/happy to), and hedging. \
Keep full technical accuracy and complete explanations, but be tight and direct. \
Pattern: [thing] [action] [reason]. [next step].";

const PRESET_CODE_ONLY: &str = "\
Respond with code. Minimize prose explanations — only explain when the code alone \
is insufficient or when asked. No preamble, no summary. If a question requires \
explanation, be brief and direct. Code blocks unchanged, errors quoted exact.";

const PRESET_CAVEMAN: &str = "\
Respond terse like smart caveman. All technical substance stay. Only fluff die.\n\
Drop: articles (a/an/the), filler (just/really/basically/actually/simply), \
pleasantries (sure/certainly/of course/happy to), hedging. \
Fragments OK. Short synonyms. Technical terms exact. Code blocks unchanged. \
Errors quoted exact.\n\
Pattern: [thing] [action] [reason]. [next step].\n\
Drop caveman for: security warnings, irreversible action confirmations, \
multi-step sequences where fragment order risks misread. \
Resume caveman after clear part done.";

/// Cycle to the next system prompt preset, wrapping around.
pub fn cycle_preset(current: SystemPromptPreset) -> SystemPromptPreset {
    let idx = SystemPromptPreset::ALL
        .iter()
        .position(|&p| p == current)
        .unwrap_or(0);
    SystemPromptPreset::ALL[(idx + 1) % SystemPromptPreset::ALL.len()]
}

/// Activity indicator shown while a provider is working.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityIndicatorStyle {
    /// A single quiet status line. This is the default for new and existing users.
    #[default]
    Semantic,
    /// The original animated half-block rainbow, preserved unchanged.
    RainbowClassic,
    /// A calmer solid-cell rainbow animation.
    RainbowCompact,
}

impl ActivityIndicatorStyle {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Semantic => "Semantic",
            Self::RainbowClassic => "Rainbow Classic",
            Self::RainbowCompact => "Rainbow Compact",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Semantic => Self::RainbowClassic,
            Self::RainbowClassic => Self::RainbowCompact,
            Self::RainbowCompact => Self::Semantic,
        }
    }
}

/// Centralized user settings persisted across restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSettings {
    /// Active-session indicator style in the session detail pane.
    #[serde(default)]
    pub activity_indicator_style: ActivityIndicatorStyle,

    /// Default: show system events in new sessions.
    #[serde(default)]
    pub default_show_system_events: bool,

    /// Default: show thinking events in new sessions.
    #[serde(default)]
    pub default_show_thinking_events: bool,

    /// Default: hide tool result events in new sessions.
    #[serde(default)]
    pub default_hide_tool_results: bool,

    /// Default session list sort order.
    #[serde(default)]
    pub sort_order: SortOrder,

    /// User-defined OpenAI-compatible provider configs.
    #[serde(default)]
    pub custom_providers: Vec<CustomProviderEntry>,

    /// Signal/iMessage bridge connection settings.
    #[serde(default)]
    pub message_bridges: MessageBridgeConnections,

    /// Local-model prompt corrector configuration.
    #[serde(default)]
    pub prompt_processor: PromptProcessorConfig,

    /// Session list card fields with enabled flags.
    #[serde(default = "default_card_fields")]
    pub card_fields: Vec<CardFieldEntry>,

    /// Optional navigator-column policy. Missing values in older state files
    /// intentionally resolve to the dense preset.
    #[serde(default)]
    pub navigator_preset: NavigatorPreset,

    /// `None` follows the active preset. A present vector is the exact typed
    /// advanced override, containing only optional navigator columns.
    #[serde(default)]
    pub navigator_optional_columns: Option<Vec<NavigatorOptionalColumn>>,

    /// Local Ollama model for title/description generation.
    #[serde(default = "default_title_model_local")]
    pub title_model_local: String,

    /// Provider for title/description generation.
    #[serde(default = "default_title_model_provider")]
    pub title_model_provider: SessionProvider,

    /// Custom provider backing title/description generation, if selected.
    #[serde(default)]
    pub title_model_custom_provider_id: Option<Uuid>,

    /// Fallback CLI model for title/description generation.
    #[serde(default = "default_title_model_fallback")]
    pub title_model_fallback: String,

    /// Selected system prompt preset (RSI-026).
    ///
    /// This is a CACHE field. The authoritative value lives in the daemon's
    /// SQLite `daemon_settings.system_prompt_preset` row. The cache is
    /// hydrated at TUI startup via `GetDaemonConfig` (see
    /// `action_handler::daemon_config::refresh_daemon_features`) and on
    /// every Settings cycle via write-through (see
    /// `action_handler::daemon_config::cycle_system_prompt_preset`).
    ///
    /// `skip_serializing` keeps the field out of `state.json` so the TUI
    /// no longer round-trips ownership of this setting through disk. The
    /// `default` deserializer keeps backward compatibility for first-boot
    /// reads of legacy state.json files (the daemon does its own one-time
    /// import in `maybe_import_legacy_system_prompt_preset`).
    #[serde(default, skip_serializing)]
    pub system_prompt_preset: SystemPromptPreset,

    /// Local Ollama model for memory (observation extraction + summarization).
    #[serde(default = "default_memory_model_local")]
    pub memory_model_local: String,

    /// Fallback model for memory (observation extraction + summarization).
    #[serde(default = "default_memory_model_fallback")]
    pub memory_model_fallback: String,

    /// Provider for memory fallback model.
    #[serde(default = "default_memory_model_fallback_provider")]
    pub memory_model_fallback_provider: SessionProvider,

    /// Custom provider backing memory fallback, if selected.
    #[serde(default)]
    pub memory_model_fallback_custom_provider_id: Option<Uuid>,

    /// Model for dream consolidation (deduction + induction).
    #[serde(default = "default_dream_model")]
    pub dream_model: String,

    /// Provider for dream consolidation.
    #[serde(default = "default_dream_model_provider")]
    pub dream_model_provider: SessionProvider,

    /// Custom provider backing dream consolidation, if selected.
    #[serde(default)]
    pub dream_model_custom_provider_id: Option<Uuid>,

    /// Whether the memory system is enabled.
    #[serde(default = "default_true")]
    pub memory_enabled: bool,

    /// When true (default), plain Enter submits in multi-line text inputs and
    /// Shift+Enter inserts a newline. When false, restore the legacy bindings
    /// (Ctrl+Enter submits; plain Enter inserts a newline). Escape hatch for
    /// terminals that cannot deliver Shift+Enter as a distinct event.
    #[serde(default = "default_true")]
    pub submit_on_enter: bool,

    /// Automatically open the question panel when a session asks for input and
    /// no other overlay is active.
    #[serde(default)]
    pub auto_open_question_modal: bool,

    /// Whether dream consolidation is enabled.
    #[serde(default = "default_false")]
    pub dream_enabled: bool,

    /// Number of observations before dreaming triggers.
    #[serde(default = "default_observation_threshold")]
    pub observation_threshold: u64,

    /// Seconds between dream cycles.
    #[serde(default = "default_dream_cooldown")]
    pub dream_cooldown_secs: u64,

    /// One-shot notice that memory and dream settings now live in the daemon.
    #[serde(default)]
    pub memory_owner_migrated: bool,

    /// Whether to fill the background of message bubbles + input bar with a
    /// solid color. When `false` (default) text areas render transparent
    /// (terminal background bleeds through). When `true`, the fill color is
    /// either `text_area_backfill_hex` (if set) or the per-role theme defaults.
    #[serde(default)]
    pub text_area_backfill_enabled: bool,

    /// Hex color (e.g. `#1E1E2E`) used for the text-area background when
    /// `text_area_backfill_enabled` is true. Empty string ⇒ fall back to the
    /// per-role theme defaults (assistant/user/tool/system bubble + input bar).
    #[serde(default)]
    pub text_area_backfill_hex: String,

    /// Duration in milliseconds of the message-formulation grow animation
    /// (the rainbow-text bubble that appears while a new message is rendering).
    /// Cycles through `FORMULATION_ANIM_DURATIONS` via Enter in Settings > Display.
    #[serde(default = "default_formulation_anim_ms")]
    pub formulation_anim_ms: u64,
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            activity_indicator_style: ActivityIndicatorStyle::default(),
            default_show_system_events: false,
            default_show_thinking_events: false,
            default_hide_tool_results: false,
            sort_order: SortOrder::default(),
            custom_providers: Vec::new(),
            message_bridges: MessageBridgeConnections::default(),
            prompt_processor: PromptProcessorConfig::default(),
            card_fields: default_card_fields(),
            navigator_preset: NavigatorPreset::Dense,
            navigator_optional_columns: None,
            title_model_local: default_title_model_local(),
            title_model_provider: default_title_model_provider(),
            title_model_custom_provider_id: None,
            title_model_fallback: default_title_model_fallback(),
            system_prompt_preset: SystemPromptPreset::default(),
            memory_model_local: default_memory_model_local(),
            memory_model_fallback: default_memory_model_fallback(),
            memory_model_fallback_provider: default_memory_model_fallback_provider(),
            memory_model_fallback_custom_provider_id: None,
            dream_model: default_dream_model(),
            dream_model_provider: default_dream_model_provider(),
            dream_model_custom_provider_id: None,
            memory_enabled: default_true(),
            submit_on_enter: default_true(),
            auto_open_question_modal: false,
            dream_enabled: false,
            observation_threshold: default_observation_threshold(),
            dream_cooldown_secs: default_dream_cooldown(),
            memory_owner_migrated: false,
            text_area_backfill_enabled: false,
            text_area_backfill_hex: String::new(),
            formulation_anim_ms: default_formulation_anim_ms(),
        }
    }
}

/// Convenience wrapper for renderers — collapses the three-state
/// `resolve_text_area_backfill` result into a single `ratatui::style::Color`.
///
/// - Toggle OFF, Transparent theme ⇒ `Color::Reset` (terminal bg shows
///   through — the Reset leg was designed for transparent aesthetics and is
///   part of the Transparent theme's locked baseline).
/// - Toggle OFF, opaque theme      ⇒ `fallback` (T1.1: opaque themes always
///   paint a full background, so a translucent terminal cannot bleed
///   through surfaces drawn over the opaque root fill).
/// - Toggle ON, hex  ⇒ `Color::Rgb(...)` (any theme).
/// - Toggle ON, none ⇒ `fallback` (the caller's per-area theme default).
pub fn text_area_bg_color(
    settings: &UserSettings,
    fallback: ratatui::style::Color,
) -> ratatui::style::Color {
    match resolve_text_area_backfill(settings) {
        None => {
            if crate::ui::theme::is_transparent_theme() {
                ratatui::style::Color::Reset
            } else {
                fallback
            }
        }
        Some(Some([r, g, b])) => ratatui::style::Color::Rgb(r, g, b),
        Some(None) => fallback,
    }
}

/// Resolve the user's chosen text-area backfill color.
///
/// Returns:
/// - `None` when the toggle is OFF (transparent / Color::Reset behavior).
/// - `Some(None)` when the toggle is ON but no hex is set ⇒ caller should use
///   per-role theme defaults (the legacy backfill behavior).
/// - `Some(Some([r,g,b]))` when the toggle is ON and a valid hex is set ⇒
///   caller should fill with that color uniformly.
pub fn resolve_text_area_backfill(settings: &UserSettings) -> Option<Option<[u8; 3]>> {
    if !settings.text_area_backfill_enabled {
        return None;
    }
    let s = settings.text_area_backfill_hex.trim();
    if s.is_empty() {
        return Some(None);
    }
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 {
        return Some(None);
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok();
    let g = u8::from_str_radix(&hex[2..4], 16).ok();
    let b = u8::from_str_radix(&hex[4..6], 16).ok();
    match (r, g, b) {
        (Some(r), Some(g), Some(b)) => Some(Some([r, g, b])),
        _ => Some(None),
    }
}

impl UserSettings {
    /// Update the cached `system_prompt_preset` from a daemon-supplied
    /// canonical slug (`"default" | "concise" | "code-only" | "caveman"`)
    /// (RSI-026). Unknown slugs fall back to `Default` to keep the cache
    /// safe even when the daemon ships a variant the TUI doesn't yet know
    /// about. The daemon validates input on `UpdateDaemonConfig`, so the
    /// fallback path should be unreachable for daemons of equal or older
    /// vintage than the TUI.
    pub fn set_system_prompt_preset_from_slug(&mut self, slug: &str) {
        self.system_prompt_preset = SystemPromptPreset::from_slug(slug);
    }

    /// Inverse of `set_system_prompt_preset_from_slug`: read the cache as
    /// the canonical kebab-case slug suitable for handing to
    /// `UpdateDaemonConfig`.
    pub fn system_prompt_preset_slug(&self) -> &'static str {
        self.system_prompt_preset.slug()
    }

    /// Returns whether a card field is enabled.
    /// Defaults to `true` if the field is not found (forward-compat with new fields).
    pub fn is_card_field_enabled(&self, field: CardField) -> bool {
        self.card_fields
            .iter()
            .find(|e| e.field == field)
            .is_none_or(|e| e.enabled)
    }

    pub fn toggle_navigator_optional_column(&mut self, column: NavigatorOptionalColumn) {
        let enabled = self.navigator_optional_columns.get_or_insert_with(|| {
            crate::ui::navigator_layout::preset_columns(self.navigator_preset)
        });
        if let Some(index) = enabled.iter().position(|item| *item == column) {
            enabled.remove(index);
        } else {
            enabled.push(column);
        }
    }
}

/// Curated local Ollama model options for title generation.
///
/// Titles are short, high-frequency, and latency-sensitive: the fast tier
/// leads. Legacy entries stay at the end so existing configs keep resolving.
pub const TITLE_MODELS_LOCAL: &[&str] =
    &["gemma4:e4b", "gemma4:12b", "qwen3.6:35b-a3b", "qwen3.6:27b"];

/// Curated CLI model options for title generation fallback.
pub const TITLE_MODELS_FALLBACK: &[&str] = &["haiku", "sonnet", "opus"];

/// Curated local Ollama model options for memory/dream operations.
pub const MEMORY_MODELS_LOCAL: &[&str] =
    &["gemma4:e4b", "gemma4:12b", "qwen3.6:35b-a3b", "qwen3.6:27b"];

/// Curated CLI model options for memory fallback.
pub const MEMORY_MODELS_FALLBACK: &[&str] = &["haiku", "sonnet", "opus"];

/// Curated observation threshold presets.
pub const OBSERVATION_THRESHOLDS: &[u64] = &[10, 25, 50, 100, 200, 500];

/// Curated dream cooldown presets (seconds).
pub const DREAM_COOLDOWNS: &[u64] = &[1800, 3600, 7200, 14400, 28800, 86400];

/// Preset durations (ms) for the formulation grow animation, cycled via Enter
/// in Settings > Display. Range: 100 ms (snappy) to 2 000 ms (dramatic).
pub const FORMULATION_ANIM_DURATIONS: &[u64] = &[100, 200, 300, 500, 750, 1000, 1500, 2000];

/// Curated local model options for prompt compilation / grammar fixing.
/// NOTE: the previous head of this list, `gemma4:27b`, was never a real tag —
/// the Gemma 4 family ships 12b / 26b / 31b (plus e2b / e4b). Likewise there is
/// no `qwen3:4b` in the 3.6 line; the fast tier is `gemma4:e4b`.
pub const PROMPT_PROCESSOR_MODELS: &[&str] =
    &["gemma4:e4b", "gemma4:12b", "qwen3.6:35b-a3b", "qwen3.6:27b"];

fn default_title_model_local() -> String {
    "gemma4:e4b".to_string()
}

fn default_title_model_provider() -> SessionProvider {
    SessionProvider::Local
}

fn default_title_model_fallback() -> String {
    "haiku".to_string()
}

fn default_memory_model_local() -> String {
    "gemma4:e4b".to_string()
}

fn default_memory_model_fallback() -> String {
    default_title_model_local()
}

fn default_memory_model_fallback_provider() -> SessionProvider {
    SessionProvider::Local
}

fn default_dream_model() -> String {
    "claude-sonnet-5".to_string()
}

fn default_dream_model_provider() -> SessionProvider {
    SessionProvider::Claude
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

fn default_bridge_working_dir() -> String {
    std::env::current_dir()
        .ok()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .to_string_lossy()
        .to_string()
}

pub fn message_bridge_config_path(kind: MessageBridgeKind) -> PathBuf {
    rsi_common::identity::data_path(kind.config_filename(), kind.config_filename())
}

pub fn write_message_bridge_config(
    kind: MessageBridgeKind,
    settings: &MessageBridgeSettings,
) -> std::io::Result<PathBuf> {
    let path = message_bridge_config_path(kind);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, render_message_bridge_config(kind, settings))?;
    Ok(path)
}

pub fn load_message_bridge_config(
    kind: MessageBridgeKind,
) -> Result<Option<MessageBridgeSettings>, String> {
    let path = message_bridge_config_path(kind);
    if !path.exists() {
        return Ok(None);
    }

    let contents = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
    let value: toml::Value = toml::from_str(&contents)
        .map_err(|e| format!("failed to parse {}: {}", path.display(), e))?;
    let table = value
        .as_table()
        .ok_or_else(|| format!("{} is not a TOML table", path.display()))?;

    let mut settings = MessageBridgeSettings {
        enabled: true,
        ..MessageBridgeSettings::default()
    };
    if let Some(enabled) = table.get("enabled").and_then(toml::Value::as_bool) {
        settings.enabled = enabled;
    }
    if let Some(account) = table.get("account").and_then(toml_scalar_to_string) {
        settings.account = account;
    }
    if let Some(handles) = table
        .get("allow_from")
        .or_else(|| table.get("allowlist"))
        .and_then(toml_string_array_value)
    {
        settings.allow_from = handles.join(", ");
    }
    if let Some(working_dir) = table
        .get("default_working_dir")
        .or_else(|| table.get("working_dir"))
        .and_then(toml_scalar_to_string)
    {
        settings.working_dir = working_dir;
    }

    Ok(Some(settings))
}

fn render_message_bridge_config(
    kind: MessageBridgeKind,
    settings: &MessageBridgeSettings,
) -> String {
    let handles = split_bridge_handles(&settings.allow_from);
    let dm_policy = if handles.is_empty() {
        "open"
    } else {
        "allowlist"
    };
    let allow_from = toml_string_array(&handles);
    let working_dir = toml_escape(&settings.working_dir);

    match kind {
        MessageBridgeKind::Signal => format!(
            "enabled = {}\n\
poll_interval_ms = 2000\n\
dm_policy = \"{}\"\n\
allow_from = {}\n\
max_message_length = 4000\n\
echo_cache_ttl_ms = 5000\n\
debounce_ms = 500\n\
default_working_dir = \"{}\"\n\
default_provider = \"Claude\"\n\
account = \"{}\"\n",
            settings.enabled,
            dm_policy,
            allow_from,
            working_dir,
            toml_escape(&settings.account)
        ),
        MessageBridgeKind::Imessage => format!(
            "enabled = {}\n\
poll_interval_ms = 2000\n\
dm_policy = \"{}\"\n\
allow_from = {}\n\
max_message_length = 4000\n\
echo_cache_ttl_ms = 5000\n\
debounce_ms = 500\n\
default_working_dir = \"{}\"\n\
default_provider = \"Claude\"\n",
            settings.enabled, dm_policy, allow_from, working_dir
        ),
    }
}

fn split_bridge_handles(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn toml_string_array(items: &[String]) -> String {
    let body = items
        .iter()
        .map(|s| format!("\"{}\"", toml_escape(s)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{}]", body)
}

fn toml_escape(input: &str) -> String {
    input.replace('\\', "\\\\").replace('"', "\\\"")
}

fn toml_scalar_to_string(value: &toml::Value) -> Option<String> {
    match value {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Integer(n) => Some(n.to_string()),
        _ => None,
    }
}

fn toml_string_array_value(value: &toml::Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(toml_scalar_to_string)
        .collect()
}

fn default_observation_threshold() -> u64 {
    50
}

fn default_dream_cooldown() -> u64 {
    28800
}

fn default_formulation_anim_ms() -> u64 {
    500
}

/// Cycle to the next value in the options list, wrapping around.
/// Returns the first option if current is not in the list.
pub fn cycle_u64(current: u64, options: &[u64]) -> u64 {
    let idx = options.iter().position(|&v| v == current);
    match idx {
        Some(i) => options[(i + 1) % options.len()],
        None => options.first().copied().unwrap_or(current),
    }
}

/// Cycle to the next model in the list, wrapping around.
/// Returns the first option if current is not in the list.
pub fn cycle_model(current: &str, options: &[&str]) -> String {
    let idx = options.iter().position(|&m| m == current);
    match idx {
        Some(i) => options[(i + 1) % options.len()].to_string(),
        None => options.first().map(|s| s.to_string()).unwrap_or_default(),
    }
}

/// Default card field layout — all fields enabled.
pub fn default_card_fields() -> Vec<CardFieldEntry> {
    CardField::ALL
        .iter()
        .map(|&field| CardFieldEntry {
            field,
            enabled: true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigator_preset_defaults_to_dense_and_roundtrips() {
        let settings: UserSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(
            settings.navigator_preset,
            crate::types::NavigatorPreset::Dense
        );
        for preset in [
            crate::types::NavigatorPreset::Dense,
            crate::types::NavigatorPreset::Operations,
            crate::types::NavigatorPreset::Cost,
        ] {
            let mut settings = UserSettings::default();
            settings.navigator_preset = preset;
            let restored: UserSettings =
                serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
            assert_eq!(restored.navigator_preset, preset);
        }
    }

    #[test]
    fn navigator_optional_override_is_absent_for_legacy_state_and_toggles_one_column() {
        let mut settings: UserSettings = serde_json::from_str("{}").unwrap();
        assert!(settings.navigator_optional_columns.is_none());
        settings.toggle_navigator_optional_column(NavigatorOptionalColumn::Retry);
        let enabled = settings.navigator_optional_columns.as_ref().unwrap();
        assert!(enabled.contains(&NavigatorOptionalColumn::Retry));
        assert!(enabled.contains(&NavigatorOptionalColumn::Age));
        assert!(enabled.contains(&NavigatorOptionalColumn::ModelEffort));
        settings.toggle_navigator_optional_column(NavigatorOptionalColumn::Retry);
        assert!(
            !settings
                .navigator_optional_columns
                .as_ref()
                .unwrap()
                .contains(&NavigatorOptionalColumn::Retry)
        );
    }

    #[test]
    fn user_settings_default_roundtrip() {
        let settings = UserSettings::default();
        let json = serde_json::to_string(&settings).unwrap();
        let restored: UserSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.sort_order, SortOrder::default());
        assert_eq!(
            restored.activity_indicator_style,
            ActivityIndicatorStyle::Semantic
        );
    }

    #[test]
    fn activity_indicator_style_defaults_for_legacy_settings_and_roundtrips() {
        let legacy: UserSettings = serde_json::from_str(r#"{"show_audio_waveform":false}"#)
            .expect("legacy settings should deserialize");
        assert_eq!(
            legacy.activity_indicator_style,
            ActivityIndicatorStyle::Semantic
        );

        let mut settings = UserSettings::default();
        settings.activity_indicator_style = ActivityIndicatorStyle::RainbowCompact;
        let json = serde_json::to_string(&settings).expect("settings should serialize");
        let restored: UserSettings =
            serde_json::from_str(&json).expect("settings should deserialize");
        assert_eq!(
            restored.activity_indicator_style,
            ActivityIndicatorStyle::RainbowCompact
        );
    }

    #[test]
    fn legacy_status_bar_segments_key_still_deserializes() {
        // T2 removed the `status_bar_segments` field entirely. `UserSettings`
        // has no `#[serde(deny_unknown_fields)]`, so a persisted config file
        // from before this slice that still contains the old key must still
        // load — the key is silently ignored, not a load-compatibility break.
        let json = r#"{"status_bar_segments":[
            {"segment":"Connection","enabled":true},
            {"segment":"Cost","enabled":false}
        ]}"#;
        let settings: Result<UserSettings, _> = serde_json::from_str(json);
        assert!(
            settings.is_ok(),
            "legacy status_bar_segments key must not break deserialization: {:?}",
            settings.err()
        );
    }

    #[test]
    fn card_fields_default_roundtrip() {
        let settings = UserSettings::default();
        let json = serde_json::to_string(&settings).unwrap();
        let restored: UserSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.card_fields.len(), CardField::ALL.len());
        assert!(restored.card_fields.iter().all(|e| e.enabled));
    }

    #[test]
    fn auto_open_question_modal_defaults_false() {
        let settings = UserSettings::default();
        assert!(!settings.auto_open_question_modal);

        let restored: UserSettings = serde_json::from_str(r#"{"show_audio_waveform":false}"#)
            .expect("legacy settings should deserialize");
        assert!(!restored.auto_open_question_modal);
    }

    #[test]
    fn card_fields_backward_compat_missing_field() {
        // Old state.json without card_fields should deserialize to defaults.
        let json = r#"{"show_audio_waveform":false}"#;
        let restored: UserSettings = serde_json::from_str(json).unwrap();
        assert_eq!(restored.card_fields.len(), CardField::ALL.len());
        assert!(restored.card_fields.iter().all(|e| e.enabled));
    }

    #[test]
    fn is_card_field_enabled_returns_true_for_unknown() {
        // Forward-compat: field not in vec defaults to enabled.
        let settings = UserSettings {
            card_fields: vec![],
            ..UserSettings::default()
        };
        assert!(settings.is_card_field_enabled(CardField::ContextBar));
    }

    #[test]
    fn test_cycle_preset() {
        assert_eq!(
            cycle_preset(SystemPromptPreset::Default),
            SystemPromptPreset::Concise
        );
        assert_eq!(
            cycle_preset(SystemPromptPreset::Concise),
            SystemPromptPreset::CodeOnly
        );
        assert_eq!(
            cycle_preset(SystemPromptPreset::CodeOnly),
            SystemPromptPreset::Caveman
        );
        assert_eq!(
            cycle_preset(SystemPromptPreset::Caveman),
            SystemPromptPreset::Default
        );
    }

    #[test]
    fn test_preset_content() {
        assert!(SystemPromptPreset::Default.content().is_none());
        assert!(SystemPromptPreset::Concise.content().is_some());
        assert!(SystemPromptPreset::CodeOnly.content().is_some());
        assert!(SystemPromptPreset::Caveman.content().is_some());
    }

    /// RSI-026: `system_prompt_preset` is now `#[serde(skip_serializing)]`
    /// because the daemon's SQLite row is authoritative. We still accept the
    /// field on read for one-shot legacy compatibility with old `state.json`
    /// files (the daemon's `maybe_import_legacy_system_prompt_preset` does the
    /// same read), but writes intentionally drop it. This test locks BOTH
    /// halves of that contract: (1) write does NOT emit the field; (2) read
    /// DOES accept the field when present.
    #[test]
    fn test_system_prompt_preset_skipped_during_serialization() {
        let mut settings = UserSettings::default();
        settings.system_prompt_preset = SystemPromptPreset::Caveman;
        let json = serde_json::to_string(&settings).unwrap();
        assert!(
            !json.contains("system_prompt_preset"),
            "system_prompt_preset must NOT appear in serialized state.json \
             (RSI-026: daemon-owned via daemon_settings table). Got: {}",
            json
        );
        // Read path: legacy state.json with the field present still deserializes,
        // so the daemon's one-time import path observes the legacy value.
        let legacy = r#"{"show_audio_waveform":false,"system_prompt_preset":"Caveman"}"#;
        let restored: UserSettings = serde_json::from_str(legacy).unwrap();
        assert_eq!(restored.system_prompt_preset, SystemPromptPreset::Caveman);
    }

    #[test]
    fn test_system_prompt_preset_backward_compat() {
        let json = r#"{"show_audio_waveform":false}"#;
        let restored: UserSettings = serde_json::from_str(json).unwrap();
        assert_eq!(restored.system_prompt_preset, SystemPromptPreset::Default);
    }

    #[test]
    fn prompt_processor_serde_roundtrip() {
        let mut settings = UserSettings::default();
        settings.prompt_processor.enabled = false;
        settings.prompt_processor.model = "llama3.1:8b".to_string();
        let json = serde_json::to_string(&settings).unwrap();
        let restored: UserSettings = serde_json::from_str(&json).unwrap();
        assert!(!restored.prompt_processor.enabled);
        assert_eq!(restored.prompt_processor.model, "llama3.1:8b");
    }

    #[test]
    fn text_area_backfill_default_off() {
        let settings = UserSettings::default();
        assert!(!settings.text_area_backfill_enabled);
        assert!(settings.text_area_backfill_hex.is_empty());
        assert!(resolve_text_area_backfill(&settings).is_none());
    }

    #[test]
    fn text_area_backfill_enabled_no_hex_returns_some_none() {
        let mut settings = UserSettings::default();
        settings.text_area_backfill_enabled = true;
        assert_eq!(resolve_text_area_backfill(&settings), Some(None));
    }

    #[test]
    fn text_area_backfill_enabled_with_hex() {
        let mut settings = UserSettings::default();
        settings.text_area_backfill_enabled = true;
        settings.text_area_backfill_hex = "#1E1E2E".to_string();
        assert_eq!(
            resolve_text_area_backfill(&settings),
            Some(Some([0x1E, 0x1E, 0x2E]))
        );
    }

    #[test]
    fn text_area_backfill_enabled_invalid_hex_falls_back() {
        let mut settings = UserSettings::default();
        settings.text_area_backfill_enabled = true;
        settings.text_area_backfill_hex = "not-a-hex".to_string();
        assert_eq!(resolve_text_area_backfill(&settings), Some(None));
    }

    #[test]
    fn text_area_backfill_serde_roundtrip() {
        let mut settings = UserSettings::default();
        settings.text_area_backfill_enabled = true;
        settings.text_area_backfill_hex = "#ABCDEF".to_string();
        let json = serde_json::to_string(&settings).unwrap();
        let restored: UserSettings = serde_json::from_str(&json).unwrap();
        assert!(restored.text_area_backfill_enabled);
        assert_eq!(restored.text_area_backfill_hex, "#ABCDEF");
    }

    #[test]
    fn text_area_backfill_serde_backward_compat() {
        // Old state.json without the new fields ⇒ defaults (off + empty).
        let json = r#"{"show_audio_waveform":false}"#;
        let restored: UserSettings = serde_json::from_str(json).unwrap();
        assert!(!restored.text_area_backfill_enabled);
        assert!(restored.text_area_backfill_hex.is_empty());
    }

    #[test]
    fn prompt_processor_backward_compat() {
        // Old state.json without prompt_processor should deserialize to defaults.
        let json = r#"{"show_audio_waveform":false}"#;
        let restored: UserSettings = serde_json::from_str(json).unwrap();
        assert!(restored.prompt_processor.enabled);
        assert_eq!(restored.prompt_processor.model, "gemma4:e4b");
    }

    // RSI-022: DaemonFeatureEntry tests covering the new Cycle variant
    // and the Codex-sandbox entry plumbing.

    /// SECURITY: the Claude config-isolation control must be operator-editable
    /// from the settings overlay, not SQL-only. It rides the generic
    /// `Cycle` dispatch, so presence in `defaults()` is the whole contract.
    #[test]
    fn daemon_feature_defaults_has_claude_config_isolation_entry() {
        let entries = DaemonFeatureEntry::defaults();
        let entry = entries
            .iter()
            .find(|e| e.field == "claude_config_isolation")
            .expect("claude_config_isolation entry must be present in defaults");
        match &entry.value {
            DaemonFeatureValue::Cycle { options, current } => {
                assert_eq!(
                    options.as_slice(),
                    &[
                        "off".to_string(),
                        "settings".to_string(),
                        "strict".to_string(),
                    ]
                );
                // Seed default: off (index 0) — preserves the historical
                // Claude launch argv until the operator opts in.
                assert_eq!(*current, 0);
            }
            other => panic!("expected Cycle, got {:?}", other),
        }
    }

    /// #634: the durable topology executor kill switch is operator-editable
    /// from the settings pane (AGENTS.md: no SQL-only knobs); it rides the
    /// generic Bool toggle, so presence in `defaults()` is the contract.
    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn daemon_feature_defaults_has_topology_executor_kill_switch() {
        let mut entries = DaemonFeatureEntry::defaults();
        let entry = entries
            .iter()
            .find(|e| e.field == "topology_executor_enabled")
            .expect("topology_executor_enabled entry must be present in defaults");
        assert_eq!(entry.label, "Durable topology executor");
        assert!(matches!(entry.value, DaemonFeatureValue::Bool(true)));
        DaemonFeatureEntry::update_from_json(
            &mut entries,
            &serde_json::json!({ "topology_executor_enabled": false }),
        );
        let entry = entries
            .iter()
            .find(|e| e.field == "topology_executor_enabled")
            .unwrap();
        assert!(matches!(entry.value, DaemonFeatureValue::Bool(false)));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn daemon_feature_defaults_has_topology_build_concurrency() {
        let mut entries = DaemonFeatureEntry::defaults();
        DaemonFeatureEntry::update_from_json(
            &mut entries,
            &serde_json::json!({ "topology_max_concurrent_build_nodes": 3 }),
        );
        let entry = entries
            .iter()
            .find(|e| e.field == "topology_max_concurrent_build_nodes")
            .unwrap();
        assert!(matches!(
            &entry.value,
            DaemonFeatureValue::Cycle { current: 2, .. }
        ));
    }

    #[test]
    fn daemon_feature_defaults_expose_rsid_systemd_scope_limits() {
        let mut entries = DaemonFeatureEntry::defaults();
        for field in [
            "rsid_scope_memory_high_mib",
            "rsid_scope_memory_max_mib",
            "rsid_scope_memory_swap_max_mib",
            "rsid_scope_cpu_weight",
        ] {
            assert!(entries.iter().any(|entry| entry.field == field), "{field}");
        }

        DaemonFeatureEntry::update_from_json(
            &mut entries,
            &serde_json::json!({
                "rsid_scope_memory_high_mib": 7168,
                "rsid_scope_memory_max_mib": 12288,
                "rsid_scope_memory_swap_max_mib": 0,
                "rsid_scope_cpu_weight": 35,
            }),
        );

        for (field, expected) in [
            ("rsid_scope_memory_high_mib", "7168"),
            ("rsid_scope_memory_max_mib", "12288"),
            ("rsid_scope_memory_swap_max_mib", "0"),
            ("rsid_scope_cpu_weight", "35"),
        ] {
            let entry = entries.iter().find(|entry| entry.field == field).unwrap();
            assert!(matches!(
                &entry.value,
                DaemonFeatureValue::Cycle { options, current }
                    if options.get(*current).is_some_and(|value| value == expected)
            ));
        }
    }

    #[test]
    fn daemon_feature_defaults_has_codex_sandbox_entry() {
        let entries = DaemonFeatureEntry::defaults();
        // Item-count contract with settings_keys.rs::item_count.
        // RSI-026: bumped to 12 — system_prompt_preset joined the list.
        // Stall classifier adds 7 daemon-owned knobs.
        // Issue #69 adds six controls, one status row, and two explicit
        // target-cache actions.
        // Claude project-config isolation (SECURITY) adds one control -> 33.
        // #634, #635 and Epic M added fifteen daemon controls -> 48.
        // #647 adds four operator scope controls -> 52.
        assert_eq!(entries.len(), 52);
        let codex = entries
            .iter()
            .find(|e| e.field == "codex_sandbox_mode")
            .expect("codex_sandbox_mode entry must be present in defaults");
        match &codex.value {
            DaemonFeatureValue::Cycle { options, current } => {
                assert_eq!(
                    options.as_slice(),
                    &[
                        "read-only".to_string(),
                        "workspace-write".to_string(),
                        "danger-full-access".to_string(),
                    ]
                );
                // Seed default: danger-full-access (index 2).
                assert_eq!(*current, 2);
            }
            other => panic!("expected Cycle, got {:?}", other),
        }
    }

    /// Issue #35. The orchestration effort ceiling is presented as a cycle row
    /// whose options come from the shared `rsi-common` vocabulary, so the TUI
    /// can never offer a value the daemon's validator would reject.
    #[test]
    fn daemon_feature_defaults_has_orchestration_max_child_effort_entry() {
        let entries = DaemonFeatureEntry::defaults();
        let entry = entries
            .iter()
            .find(|e| e.field == "orchestration_max_child_effort")
            .expect("orchestration_max_child_effort entry must be present in defaults");
        match &entry.value {
            DaemonFeatureValue::Cycle { options, current } => {
                assert_eq!(
                    options.as_slice(),
                    rsi_common::model_control::ORCHESTRATION_MAX_CHILD_EFFORT_CHOICES
                        .iter()
                        .map(|c| (*c).to_string())
                        .collect::<Vec<_>>()
                        .as_slice(),
                    "TUI options must be the shared vocabulary verbatim"
                );
                // Defaults to the sentinel, i.e. no operator ceiling declared.
                assert_eq!(*current, 0);
                assert_eq!(options[0], "unset");
            }
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    #[test]
    fn sandbox_storage_status_states_and_build_cache_entries_round_trip() {
        let mut entries = DaemonFeatureEntry::defaults();
        for field in [
            "sandbox_build_cache_reclaim_enabled",
            "sandbox_build_cache_reclaim_ttl_secs",
            "sandbox_build_cache_reclaim_interval_secs",
            "sandbox_build_cache_reclaim_high_watermark_pct",
            "sandbox_build_cache_reclaim_low_watermark_pct",
            "sandbox_build_cache_reclaim_max_candidates",
            "sandbox_storage_status",
            "sandbox_build_cache_dry_run",
            "sandbox_build_cache_reclaim_now",
        ] {
            assert!(entries.iter().any(|entry| entry.field == field), "{field}");
        }

        DaemonFeatureEntry::update_from_json(
            &mut entries,
            &serde_json::json!({
                "sandbox_build_cache_reclaim_enabled": false,
                "sandbox_build_cache_reclaim_ttl_secs": 3600,
                "sandbox_build_cache_reclaim_interval_secs": 300,
                "sandbox_build_cache_reclaim_high_watermark_pct": 90,
                "sandbox_build_cache_reclaim_low_watermark_pct": 70,
                "sandbox_build_cache_reclaim_max_candidates": 128
            }),
        );
        assert!(matches!(
            entries
                .iter()
                .find(|entry| entry.field == "sandbox_build_cache_reclaim_enabled")
                .map(|entry| &entry.value),
            Some(DaemonFeatureValue::Bool(false))
        ));
        for (field, expected) in [
            ("sandbox_build_cache_reclaim_ttl_secs", "3600"),
            ("sandbox_build_cache_reclaim_interval_secs", "300"),
            ("sandbox_build_cache_reclaim_high_watermark_pct", "90"),
            ("sandbox_build_cache_reclaim_low_watermark_pct", "70"),
            ("sandbox_build_cache_reclaim_max_candidates", "128"),
        ] {
            let Some(entry) = entries.iter().find(|entry| entry.field == field) else {
                panic!("{field} entry exists");
            };
            match &entry.value {
                DaemonFeatureValue::Cycle { options, current } => {
                    assert_eq!(options[*current], expected, "{field}");
                }
                other => panic!("expected Cycle for {field}, got {other:?}"),
            }
        }

        DaemonFeatureEntry::update_sandbox_storage_report(
            &mut entries,
            &rsi_common::sandbox_storage::SandboxBuildCacheReclaimReport {
                version: 1,
                dry_run: true,
                enabled: true,
                config: rsi_common::sandbox_storage::SandboxBuildCacheReclaimConfig {
                    enabled: true,
                    ttl_secs: 21_600,
                    interval_secs: 3_600,
                    high_watermark_pct: 85,
                    low_watermark_pct: 75,
                    max_candidates: 64,
                },
                pressure_active_before: false,
                pressure_active_after: false,
                filesystem_before: rsi_common::sandbox_storage::SandboxFilesystemStats {
                    total_bytes: 831_978_258_432,
                    available_bytes: 351_038_164_992,
                    used_bytes: 480_940_093_440,
                    used_percent: 58,
                },
                filesystem_after: rsi_common::sandbox_storage::SandboxFilesystemStats {
                    total_bytes: 831_978_258_432,
                    available_bytes: 367_144_292_352,
                    used_bytes: 464_833_966_080,
                    used_percent: 55,
                },
                candidates_considered: 12,
                eligible_candidates: 3,
                skip_counts: std::collections::BTreeMap::new(),
                would_reclaim_count: 0,
                would_reclaim_bytes: 0,
                staged_count: 0,
                staged_bytes: 0,
                newly_staged_count: 0,
                newly_staged_bytes: 0,
                recovered_count: 0,
                recovered_bytes: 0,
                pending_count: 0,
                pending_bytes: 0,
                fully_removed_count: 0,
                reclaimed_bytes: 0,
                stopped_at_low_watermark: false,
                candidate_budget_exhausted: false,
                stop_reason:
                    rsi_common::sandbox_storage::SandboxBuildCacheReclaimStopReason::Completed,
            },
        );
        let status = DaemonFeatureEntry::display_value(&entries, "sandbox_storage_status")
            .expect("storage report must populate display row");
        assert!(status.contains("58% used"));
        assert!(status.contains("326.9 GiB free"));
        assert!(status.contains("12 checked"));
        assert!(status.contains("3 eligible"));
        assert!(status.contains("capacity estimate unavailable"));

        let report = rsi_common::sandbox_storage::SandboxBuildCacheReclaimReport {
            would_reclaim_count: 0,
            would_reclaim_bytes: 0,
            fully_removed_count: 2,
            pending_count: 1,
            ..rsi_common::sandbox_storage::SandboxBuildCacheReclaimReport {
                version: 1,
                dry_run: false,
                enabled: true,
                config: rsi_common::sandbox_storage::SandboxBuildCacheReclaimConfig {
                    enabled: true,
                    ttl_secs: 21_600,
                    interval_secs: 3_600,
                    high_watermark_pct: 85,
                    low_watermark_pct: 75,
                    max_candidates: 64,
                },
                pressure_active_before: false,
                pressure_active_after: false,
                filesystem_before: rsi_common::sandbox_storage::SandboxFilesystemStats {
                    total_bytes: 100,
                    available_bytes: 40,
                    used_bytes: 60,
                    used_percent: 60,
                },
                filesystem_after: rsi_common::sandbox_storage::SandboxFilesystemStats {
                    total_bytes: 100,
                    available_bytes: 50,
                    used_bytes: 50,
                    used_percent: 50,
                },
                candidates_considered: 3,
                eligible_candidates: 3,
                skip_counts: std::collections::BTreeMap::new(),
                would_reclaim_count: 0,
                would_reclaim_bytes: 0,
                staged_count: 2,
                staged_bytes: 20,
                newly_staged_count: 2,
                newly_staged_bytes: 20,
                recovered_count: 0,
                recovered_bytes: 0,
                pending_count: 0,
                pending_bytes: 0,
                fully_removed_count: 0,
                reclaimed_bytes: 10,
                stopped_at_low_watermark: false,
                candidate_budget_exhausted: false,
                stop_reason:
                    rsi_common::sandbox_storage::SandboxBuildCacheReclaimStopReason::Completed,
            }
        };
        DaemonFeatureEntry::update_sandbox_storage_action(&mut entries, &report, true);
        assert_eq!(
            DaemonFeatureEntry::display_value(&entries, "sandbox_build_cache_dry_run"),
            Some("3 eligible / 3 checked · capacity estimate unavailable")
        );
        DaemonFeatureEntry::update_sandbox_storage_action(&mut entries, &report, false);
        assert_eq!(
            DaemonFeatureEntry::display_value(&entries, "sandbox_build_cache_reclaim_now"),
            Some("2 rm · 1 pend")
        );

        use crate::app::bootstrap::SandboxStorageStatus;
        let observed_at = chrono::DateTime::parse_from_rfc3339("2026-09-03T12:34:56Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let states = [
            (
                SandboxStorageStatus::Unknown,
                "Unknown · R to refresh".to_string(),
            ),
            (
                SandboxStorageStatus::RefreshingUnknown,
                "Refreshing · no preview yet".to_string(),
            ),
            (
                SandboxStorageStatus::Fresh {
                    report: report.clone(),
                    observed_at,
                },
                "Fresh 12:34:56 UTC · 3 checked · 3 eligible · 60% used · 40 B free · capacity estimate unavailable".to_string(),
            ),
            (
                SandboxStorageStatus::RefreshingStale {
                    report: report.clone(),
                    observed_at,
                },
                "Refreshing · stale since 12:34:56 UTC · 3 checked · 3 eligible · 60% used · 40 B free · capacity estimate unavailable".to_string(),
            ),
            (
                SandboxStorageStatus::Error {
                    message: "preview failed".to_string(),
                    last_success_at: Some(observed_at),
                },
                "Error: preview failed · last success 12:34:56 UTC".to_string(),
            ),
        ];
        for (state, expected) in states {
            DaemonFeatureEntry::update_sandbox_storage_status(&mut entries, &state);
            assert_eq!(
                DaemonFeatureEntry::display_value(&entries, "sandbox_storage_status"),
                Some(expected.as_str())
            );
        }

        let empty = rsi_common::sandbox_storage::SandboxBuildCacheReclaimReport {
            candidates_considered: 0,
            eligible_candidates: 0,
            ..report
        };
        DaemonFeatureEntry::update_sandbox_storage_report(&mut entries, &empty);
        DaemonFeatureEntry::update_sandbox_storage_action(&mut entries, &empty, true);
        let empty_status =
            DaemonFeatureEntry::display_value(&entries, "sandbox_storage_status").unwrap();
        assert!(empty_status.contains("0 checked · 0 eligible"));
        assert!(empty_status.contains("capacity estimate unavailable"));
        assert_eq!(
            DaemonFeatureEntry::display_value(&entries, "sandbox_build_cache_dry_run"),
            Some("0 eligible / 0 checked · capacity estimate unavailable")
        );
    }

    #[test]
    fn source_worktree_settlement_is_an_explicit_destructive_action_row() {
        let entries = DaemonFeatureEntry::defaults();
        let entry = entries
            .iter()
            .find(|entry| entry.field == "source_worktree_settlement")
            .expect("settlement action row");
        assert!(entry.label.starts_with('⚠'));
        assert!(matches!(
            &entry.value,
            DaemonFeatureValue::Display(value) if value == "Open destructive audit…"
        ));
    }

    #[test]
    fn sandbox_build_cache_numeric_cycles_preserve_min_middle_and_maximum_values() {
        for values in [
            (1_u64, 60_u64, 2_u64, 1_u64, 1_u64),
            (12_345, 1_234, 61, 37, 333),
            (2_592_000, 86_400, 99, 98, 1_024),
        ] {
            let mut entries = DaemonFeatureEntry::defaults();
            let expected = [
                ("sandbox_build_cache_reclaim_ttl_secs", values.0),
                ("sandbox_build_cache_reclaim_interval_secs", values.1),
                ("sandbox_build_cache_reclaim_high_watermark_pct", values.2),
                ("sandbox_build_cache_reclaim_low_watermark_pct", values.3),
                ("sandbox_build_cache_reclaim_max_candidates", values.4),
            ];
            DaemonFeatureEntry::update_from_json(
                &mut entries,
                &serde_json::json!({
                    "sandbox_build_cache_reclaim_ttl_secs": values.0,
                    "sandbox_build_cache_reclaim_interval_secs": values.1,
                    "sandbox_build_cache_reclaim_high_watermark_pct": values.2,
                    "sandbox_build_cache_reclaim_low_watermark_pct": values.3,
                    "sandbox_build_cache_reclaim_max_candidates": values.4,
                }),
            );
            for (field, value) in expected {
                let entry = entries.iter().find(|entry| entry.field == field).unwrap();
                let DaemonFeatureValue::Cycle { options, current } = &entry.value else {
                    panic!("{field} must remain a cycle");
                };
                assert_eq!(options[*current], value.to_string(), "{field}");
                assert_eq!(
                    options
                        .iter()
                        .filter(|option| **option == value.to_string())
                        .count(),
                    1,
                    "{field} exact value must be deduplicated"
                );
                assert!(options.windows(2).all(|pair| {
                    pair[0].parse::<u64>().unwrap() < pair[1].parse::<u64>().unwrap()
                }));
            }
        }
    }

    #[test]
    fn sandbox_build_cache_refresh_failure_replaces_stale_status() {
        let mut entries = DaemonFeatureEntry::defaults();
        DaemonFeatureEntry::set_sandbox_storage_refresh_failed(
            &mut entries,
            "strict report rejected",
        );
        assert_eq!(
            DaemonFeatureEntry::display_value(&entries, "sandbox_storage_status"),
            Some("Refresh failed: strict report rejected")
        );
    }

    /// Issue #35. Every value the daemon can report must select the matching
    /// option, so the row never silently falls back to index 0 and shows
    /// "unset" while a real ceiling is in force.
    #[test]
    fn orchestration_max_child_effort_round_trips_through_update_from_json() {
        for (index, choice) in rsi_common::model_control::ORCHESTRATION_MAX_CHILD_EFFORT_CHOICES
            .iter()
            .enumerate()
        {
            let mut entries = DaemonFeatureEntry::defaults();
            let json = serde_json::json!({ "orchestration_max_child_effort": choice });
            DaemonFeatureEntry::update_from_json(&mut entries, &json);
            let entry = entries
                .iter()
                .find(|e| e.field == "orchestration_max_child_effort")
                .expect("entry present");
            match &entry.value {
                DaemonFeatureValue::Cycle { options, current } => {
                    assert_eq!(
                        *current,
                        index,
                        "daemon value {choice} must select its own option, got {:?}",
                        options.get(*current)
                    );
                }
                other => panic!("expected Cycle, got {other:?}"),
            }
        }
    }

    #[test]
    fn daemon_feature_update_from_json_populates_cycle() {
        let mut entries = DaemonFeatureEntry::defaults();
        let json = serde_json::json!({
            "codex_sandbox_mode": "danger-full-access",
        });
        DaemonFeatureEntry::update_from_json(&mut entries, &json);
        let codex = entries
            .iter()
            .find(|e| e.field == "codex_sandbox_mode")
            .unwrap();
        match &codex.value {
            DaemonFeatureValue::Cycle { current, .. } => {
                assert_eq!(*current, 2); // index of danger-full-access
            }
            other => panic!("expected Cycle, got {:?}", other),
        }
    }

    #[test]
    fn daemon_feature_update_from_json_cycle_unknown_falls_back_to_zero() {
        let mut entries = DaemonFeatureEntry::defaults();
        let json = serde_json::json!({
            "codex_sandbox_mode": "this-is-a-future-mode",
        });
        DaemonFeatureEntry::update_from_json(&mut entries, &json);
        let codex = entries
            .iter()
            .find(|e| e.field == "codex_sandbox_mode")
            .unwrap();
        match &codex.value {
            DaemonFeatureValue::Cycle { current, .. } => {
                // Unknown values must NOT panic and must fall back to index 0.
                assert_eq!(*current, 0);
            }
            other => panic!("expected Cycle, got {:?}", other),
        }
    }

    #[test]
    fn numeric_daemon_cycle_preserves_persisted_off_list_value() {
        let mut entries = DaemonFeatureEntry::defaults();
        DaemonFeatureEntry::update_from_json(
            &mut entries,
            &serde_json::json!({
                "retry_max_backoff_ms": 42000,
                "recursive_dag_run_lease_ttl_ms": 42000,
                "recursive_dag_max_concurrent_graphs": 3,
                "dream_observation_threshold": 75,
                "dream_cooldown_secs": 5400,
                "dream_idle_secs": 420,
            }),
        );
        for (field, value) in [
            ("retry_max_backoff_ms", "42000"),
            ("recursive_dag_run_lease_ttl_ms", "42000"),
            ("recursive_dag_max_concurrent_graphs", "3"),
            ("dream_observation_threshold", "75"),
            ("dream_cooldown_secs", "5400"),
            ("dream_idle_secs", "420"),
        ] {
            let entry = entries.iter().find(|entry| entry.field == field).unwrap();
            let DaemonFeatureValue::Cycle { options, current } = &entry.value else {
                panic!("{field} remains a cycle");
            };
            assert_eq!(options[*current], value, "{field}");
        }
        let Some(retry) = entries
            .iter()
            .find(|entry| entry.field == "retry_max_backoff_ms")
        else {
            panic!("retry backoff entry exists");
        };
        let DaemonFeatureValue::Cycle { options, current } = &retry.value else {
            panic!("retry backoff remains a cycle");
        };
        assert_eq!(options[(current + 1) % options.len()], "60000");
    }

    #[test]
    fn stall_classifier_entries_present_in_defaults() {
        let entries = DaemonFeatureEntry::defaults();
        for field in [
            "stall_classifier_enabled",
            "stall_classifier_model",
            "stall_classifier_idle_secs",
            "stall_classifier_idle_secs_codex",
            "stall_classifier_cooldown_secs",
            "stall_classifier_max_per_session",
            "stall_classifier_confidence_floor",
        ] {
            assert!(
                entries.iter().any(|e| e.field == field),
                "missing classifier entry: {field}"
            );
        }
    }

    #[test]
    fn stall_classifier_enabled_is_bool_toggle() {
        let entries = DaemonFeatureEntry::defaults();
        let entry = entries
            .iter()
            .find(|e| e.field == "stall_classifier_enabled")
            .unwrap();
        assert!(matches!(entry.value, DaemonFeatureValue::Bool(false)));
    }

    #[test]
    fn codegraph_indexing_hydrates_as_off_on_bool_toggle() {
        let mut entries = DaemonFeatureEntry::defaults();
        let entry = entries
            .iter()
            .find(|entry| entry.field == "codegraph_indexing_enabled")
            .expect("Codegraph indexing row");
        assert_eq!(entry.label, "Codegraph indexing");
        assert!(matches!(entry.value, DaemonFeatureValue::Bool(false)));

        DaemonFeatureEntry::update_from_json(
            &mut entries,
            &serde_json::json!({"codegraph_indexing_enabled": true}),
        );
        let entry = entries
            .iter()
            .find(|entry| entry.field == "codegraph_indexing_enabled")
            .unwrap();
        assert!(matches!(entry.value, DaemonFeatureValue::Bool(true)));
    }

    #[test]
    fn stall_classifier_numeric_cycles_round_trip_through_update_from_json() {
        let mut entries = DaemonFeatureEntry::defaults();
        let json = serde_json::json!({
            "stall_classifier_idle_secs": 900,
            "stall_classifier_idle_secs_codex": 3600,
            "stall_classifier_cooldown_secs": 600,
            "stall_classifier_max_per_session": 5,
            "stall_classifier_confidence_floor": 0.9,
        });
        DaemonFeatureEntry::update_from_json(&mut entries, &json);

        for (field, expected_current) in [
            ("stall_classifier_idle_secs", 2),
            ("stall_classifier_idle_secs_codex", 4),
            ("stall_classifier_cooldown_secs", 1),
            ("stall_classifier_max_per_session", 3),
            ("stall_classifier_confidence_floor", 4),
        ] {
            let entry = entries.iter().find(|e| e.field == field).unwrap();
            match &entry.value {
                DaemonFeatureValue::Cycle { current, .. } => {
                    assert_eq!(*current, expected_current, "field {field}");
                }
                other => panic!("expected Cycle for {field}, got {other:?}"),
            }
        }
    }

    #[test]
    fn stall_classifier_enabled_round_trips_through_update_from_json() {
        let mut entries = DaemonFeatureEntry::defaults();
        let json = serde_json::json!({ "stall_classifier_enabled": true });
        DaemonFeatureEntry::update_from_json(&mut entries, &json);
        let entry = entries
            .iter()
            .find(|e| e.field == "stall_classifier_enabled")
            .unwrap();
        assert!(matches!(entry.value, DaemonFeatureValue::Bool(true)));
    }

    #[test]
    fn stall_classifier_model_renders_as_display_only() {
        let mut entries = DaemonFeatureEntry::defaults();
        let json = serde_json::json!({ "stall_classifier_model": "qwen2.5:14b" });
        DaemonFeatureEntry::update_from_json(&mut entries, &json);
        let entry = entries
            .iter()
            .find(|e| e.field == "stall_classifier_model")
            .unwrap();
        match &entry.value {
            DaemonFeatureValue::Display(s) => assert_eq!(s, "qwen2.5:14b"),
            other => panic!("expected Display, got {other:?}"),
        }
    }

    #[test]
    fn daemon_feature_update_preserves_variant_for_missing_fields() {
        // If the daemon response omits a known field entirely, the entry
        // value must be left untouched (no surprise variant changes).
        let mut entries = DaemonFeatureEntry::defaults();
        let json = serde_json::json!({});
        DaemonFeatureEntry::update_from_json(&mut entries, &json);
        let codex = entries
            .iter()
            .find(|e| e.field == "codex_sandbox_mode")
            .unwrap();
        // Still a Cycle at the seeded index. The canonical seed is
        // danger-full-access (index 2) — see `defaults()`.
        assert!(matches!(
            codex.value,
            DaemonFeatureValue::Cycle { current: 2, .. }
        ));
    }

    #[test]
    fn model_control_entries_present_in_defaults() {
        let entries = DaemonFeatureEntry::defaults();
        assert!(entries.iter().any(|e| e.field == "model_control_mode"));
        assert!(entries.iter().any(|e| e.field == "model_control_stop_all"));
    }

    #[test]
    fn model_control_update_sets_cycle_and_stop_label() {
        let mut entries = DaemonFeatureEntry::defaults();
        let status = ModelControlStatusReport {
            mode: ModelControlMode::LocalOnly,
            mode_updated_at: Some("2026-07-15T00:00:00Z".to_string()),
            restart_required_fields: Vec::new(),
            circuit_state: "open".to_string(),
            circuit_reason: "local only".to_string(),
            circuits: Vec::new(),
            policies: Vec::new(),
            active_invocations: Vec::new(),
            recent_invocations: Vec::new(),
            recent_denials: Vec::new(),
            recent_budget_alerts: Vec::new(),
        };

        DaemonFeatureEntry::update_model_control(&mut entries, &status);

        let mode = entries
            .iter()
            .find(|e| e.field == "model_control_mode")
            .expect("mode entry");
        match &mode.value {
            DaemonFeatureValue::Cycle { current, .. } => assert_eq!(*current, 3),
            other => panic!("expected cycle entry, got {other:?}"),
        }

        let stop = entries
            .iter()
            .find(|e| e.field == "model_control_stop_all")
            .expect("stop entry");
        match &stop.value {
            DaemonFeatureValue::Display(label) => assert!(label.contains("open")),
            other => panic!("expected display entry, got {other:?}"),
        }
    }

    // RSI-026 — DaemonFeatureEntry + SystemPromptPreset slug coverage.

    #[test]
    fn test_daemon_feature_entry_system_prompt_preset_present() {
        let entries = DaemonFeatureEntry::defaults();
        let entry = entries
            .iter()
            .find(|e| e.field == "system_prompt_preset")
            .expect("system_prompt_preset entry must be present (RSI-026)");
        assert_eq!(entry.label, "System prompt preset");
        match &entry.value {
            DaemonFeatureValue::Cycle { options, current } => {
                assert_eq!(
                    options.as_slice(),
                    &[
                        "default".to_string(),
                        "concise".to_string(),
                        "code-only".to_string(),
                        "caveman".to_string(),
                    ]
                );
                // Seed default is "default" (index 0). The real value gets
                // overwritten by `update_from_json` after the first
                // GetDaemonConfig response — see refresh_daemon_features.
                assert_eq!(*current, 0);
            }
            other => panic!("expected Cycle variant, got {:?}", other),
        }
    }

    #[test]
    fn test_daemon_feature_entry_update_from_json_system_prompt_preset() {
        let mut entries = DaemonFeatureEntry::defaults();
        // Apply each canonical slug; assert the entry's current index advances
        // to match the slug's position in the options list.
        let cases = [
            ("default", 0_usize),
            ("concise", 1),
            ("code-only", 2),
            ("caveman", 3),
        ];
        for (slug, expected_idx) in cases {
            let json = serde_json::json!({ "system_prompt_preset": slug });
            DaemonFeatureEntry::update_from_json(&mut entries, &json);
            let entry = entries
                .iter()
                .find(|e| e.field == "system_prompt_preset")
                .unwrap();
            match &entry.value {
                DaemonFeatureValue::Cycle { current, .. } => {
                    assert_eq!(
                        *current, expected_idx,
                        "slug={} expected current={}",
                        slug, expected_idx
                    );
                }
                other => panic!("expected Cycle variant for {}, got {:?}", slug, other),
            }
        }
    }

    #[test]
    fn test_system_prompt_preset_slug_roundtrip() {
        for variant in SystemPromptPreset::ALL.iter().copied() {
            let slug = variant.slug();
            let roundtripped = SystemPromptPreset::from_slug(slug);
            assert_eq!(
                roundtripped, variant,
                "round-trip failed for {:?} via slug \"{}\"",
                variant, slug
            );
        }
    }

    #[test]
    fn test_system_prompt_preset_from_slug_unknown_falls_back_to_default() {
        // Defensive: the daemon validates input before responding, but if a
        // future daemon ships a slug the TUI doesn't recognize the cache
        // must NOT panic — it falls back to Default.
        assert_eq!(
            SystemPromptPreset::from_slug("future-preset"),
            SystemPromptPreset::Default
        );
        assert_eq!(
            SystemPromptPreset::from_slug(""),
            SystemPromptPreset::Default
        );
    }

    #[test]
    fn test_system_prompt_preset_slug_for_each_variant() {
        assert_eq!(SystemPromptPreset::Default.slug(), "default");
        assert_eq!(SystemPromptPreset::Concise.slug(), "concise");
        assert_eq!(SystemPromptPreset::CodeOnly.slug(), "code-only");
        assert_eq!(SystemPromptPreset::Caveman.slug(), "caveman");
    }

    #[test]
    fn test_user_settings_slug_helpers_roundtrip() {
        // RSI-026: UserSettings helpers slug/from_slug round-trip through
        // the cache field.
        for variant in SystemPromptPreset::ALL.iter().copied() {
            let mut settings = UserSettings::default();
            settings.system_prompt_preset = variant;
            assert_eq!(settings.system_prompt_preset_slug(), variant.slug());

            // Round-trip: clobber, restore from slug.
            settings.system_prompt_preset = SystemPromptPreset::Default;
            settings.set_system_prompt_preset_from_slug(variant.slug());
            assert_eq!(settings.system_prompt_preset, variant);
        }
    }
}
