//! TUI application state.

pub(crate) mod attention;
pub(crate) mod bootstrap;
mod cache;
mod graph;
mod handoff;
mod hierarchy_effect;
pub(crate) mod issues;
mod jumplist;
mod layout;
pub(crate) mod manager_roster;
mod models;
mod navigation;
mod notifications;
mod polling;
mod projects;
mod search;
mod session_actions;
mod sessions;
mod state;

pub(crate) use handoff::{DocregOperationContext, DocregOperationResult, PendingDocregOperation};
pub(crate) use session_actions::{
    InteractiveLaunchOrigin, InteractiveLaunchResult, LaunchOptions, LaunchPlacement,
    PendingInteractiveLaunch,
};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod navigation_delay_test;

#[cfg(test)]
pub(crate) mod app_test_helpers;

use crate::client::DaemonClient;
use crate::keybindings::{LcKeyManager, build_key_manager};
use crate::profiling::CacheCounters;
use crate::settings::UserSettings;
use crate::state::{DevState, PersistedState};
use crate::types::{
    GraphDraft, InputMode, InputPurpose, OverlayState, Pane, PaneId, SearchTarget, SessionState,
    SplitNode, Tab,
};
use crate::ui::theme;
use rsi_common::rpc::ProviderRateLimitSnapshot;
use rsi_common::types::{Session, SessionProvider};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use uuid::Uuid;

/// Maximum walk depth for `App::effective_topology` traversal.
/// Mirrors `rsid::session::hierarchy_ops::MAX_HIERARCHY_DEPTH = 16`
/// without cross-crate import (TUI cannot import from `rsid`).
/// Distinct from `MAX_HIER_DEPTH` (mini-DAG render cap) and
/// `MAX_SPAWN_DEPTH` (daemon spawn policy).
pub(crate) const MAX_HIERARCHY_DEPTH: u32 = 16;

/// Session list sort order.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SortOrder {
    /// Oldest updated_at first (longest without attention at top).
    #[default]
    StalestFirst,
    /// Newest updated_at first (most recently active at top).
    #[serde(alias = "NewestFirst")]
    FreshestFirst,
    /// Oldest created_at first (earliest created sessions at top).
    OldestCreated,
    /// Newest created_at first (most recently created sessions at top).
    NewestCreated,
    /// Group sessions by their label, labels sorted by most-recent member.
    ByLabel,
}

impl SortOrder {
    pub const ALL: [SortOrder; 5] = [
        SortOrder::StalestFirst,
        SortOrder::FreshestFirst,
        SortOrder::OldestCreated,
        SortOrder::NewestCreated,
        SortOrder::ByLabel,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SortOrder::StalestFirst => "Stalest first",
            SortOrder::FreshestFirst => "Freshest first",
            SortOrder::OldestCreated => "Oldest created",
            SortOrder::NewestCreated => "Newest created",
            SortOrder::ByLabel => "By label",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            SortOrder::StalestFirst => "Sessions neglected longest at top",
            SortOrder::FreshestFirst => "Most recently active sessions at top",
            SortOrder::OldestCreated => "Earliest created sessions at top",
            SortOrder::NewestCreated => "Most recently created sessions at top",
            SortOrder::ByLabel => "Sessions grouped by their assigned label",
        }
    }
}

/// Available Claude models: (model_id, display_name).
///
/// This is the canonical catalog in `rsi_common::claude_catalog`, which the
/// daemon's `ClaudeClient::discover_models` also projects — the two lists had
/// drifted apart (the picker offered `claude-opus-4-5`, the daemon catalog did
/// not) and nothing pinned them together (V-002/F-124/F-158). Add or remove a
/// model there, once, and both surfaces move together; the catalog also
/// carries each model's context window, so the daemon can no longer advertise
/// "Fable 5 (1M)" here while computing fill against 128k.
///
/// Still used as the static fallback for the daemon-unreachable case: live
/// discovery (`DiscoverModels` RPC) returns the same list.
pub const CLAUDE_MODELS: &[(&str, &str)] = rsi_common::claude_catalog::CLAUDE_MODEL_MENU;

/// Available Codex models: (model_id, display_name).
///
/// Static fallback only. The daemon normally asks the installed Codex CLI for
/// its bundled model catalog with `codex debug models --bundled`.
pub const CODEX_MODELS: &[(&str, &str)] = &[
    ("gpt-6-sol", "GPT-6-Sol"),
    ("gpt-6-luna", "GPT-6-Luna"),
    ("gpt-6-astra", "GPT-6-Astra"),
    ("gpt-5.5", "GPT-5.5"),
    ("gpt-5.2", "GPT-5.2"),
];

/// Minimal offline fallback for Pioneer. Explicit discovery always refreshes
/// the live account catalog; no vendor model list is baked into the TUI.
pub const PIONEER_MODELS: &[(&str, &str)] = &[("claude-sonnet-5", "Claude Sonnet 5")];

/// Minimal offline fallback for OpenRouter. Model IDs use OpenRouter's
/// provider/model namespace and live discovery can replace this list.
pub const BEDROCK_MODELS: &[(&str, &str)] =
    &[("global.openai.gpt-5.6-sol", "GPT-5.6 Sol (Global)")];

pub const OPENROUTER_MODELS: &[(&str, &str)] = &[("openai/gpt-5.2", "OpenAI GPT-5.2")];

const PIONEER_RETIRED_AUTO_MODEL: &str = "pioneer/auto";

/// Available local models: (model_id, display_name).
/// Defaults shown here; daemon also discovers models via GET /v1/models.
/// Ordered by throughput measured on this box (RTX 5080 16GB, dictate-agent
/// resident, ollama 0.32.7, `think:false`, 150-tok budget) — NOT by parameter
/// count:
///   e4b                     157.2 tok/s  <-- utility tier
///   12b     (multimodal)     84.5 tok/s  <-- text+vision+audio, 7.6GB
///   35b-a3b (MoE, 3B active) 58.9 tok/s  <-- strongest coding model that is fast
///   27b     (dense)           9.8 tok/s  <-- 17GB spills 16GB VRAM, CPU-bound
///
/// Two counter-intuitive results worth preserving:
///  1. The 35B MoE beats the smaller 27B dense by ~6x, because only 3B params
///     are active per token while the dense 27B does not fit in VRAM.
///  2. These numbers are ollama-version-sensitive: on ollama 0.20.5 the same
///     MoE measured 20.4 tok/s (2.9x slower). Re-measure after ollama upgrades
///     rather than trusting the figures above.
pub const LOCAL_MODELS: &[(&str, &str)] = &[
    ("qwen3.6:35b-a3b", "Qwen3.6 35B-A3B (MoE)"),
    ("gemma4:12b", "Gemma 4 12B (multimodal)"),
    ("gemma4:e4b", "Gemma 4 E4B (fast)"),
    ("qwen3.6:27b", "Qwen3.6 27B (slow: spills VRAM)"),
];

/// Available Antigravity models: (model_id, display_name).
///
/// Antigravity is a Gemini-only provider — do not add Claude/Anthropic model
/// IDs here even if the upstream `agy` CLI exposes them.
pub const ANTIGRAVITY_MODELS: &[(&str, &str)] = &[
    ("gemini-3-flash", "Gemini 3.0 Flash"),
    ("gemini-3-pro-high", "Gemini 3.0 Pro High"),
    ("gemini-3-pro-low", "Gemini 3.0 Pro Low"),
    ("gemini-3.5-flash", "Gemini 3.5 Flash"),
    ("gemini-3.5-flash-low", "Gemini 3.5 Flash (Low)"),
    ("gemini-3.5-flash-medium", "Gemini 3.5 Flash (Medium)"),
    ("gemini-3.5-flash-high", "Gemini 3.5 Flash (High)"),
    ("gemini-3.5-pro-high", "Gemini 3.5 Pro (High)"),
    ("gemini-3.5-pro-medium", "Gemini 3.5 Pro (Medium)"),
    ("gemini-3.5-pro-low", "Gemini 3.5 Pro (Low)"),
    ("gpt-oss-120b-medium", "GPT OSS 120B Medium"),
];

pub(crate) fn models_for_provider(provider: SessionProvider) -> Vec<(String, String)> {
    let src = match provider {
        SessionProvider::Claude => CLAUDE_MODELS,
        SessionProvider::Codex | SessionProvider::CodexAppServer => CODEX_MODELS,
        SessionProvider::Pioneer => PIONEER_MODELS,
        SessionProvider::OpenRouter => OPENROUTER_MODELS,
        SessionProvider::Bedrock => BEDROCK_MODELS,
        SessionProvider::Local => LOCAL_MODELS,
        SessionProvider::Antigravity => ANTIGRAVITY_MODELS,
        SessionProvider::Harness => {
            // Harness models are dynamic (depend on API keys), return empty static list.
            // Real discovery uses DiscoverModels RPC which calls harness_models().
            return Vec::new();
        }
        _ => return Vec::new(),
    };
    src.iter()
        .map(|(id, name)| (id.to_string(), name.to_string()))
        .collect()
}

/// System prompt for TaskRabbit one-shot task executor.
pub(crate) const TASKRABBIT_SYSTEM_PROMPT: &str = "You are a one-shot task executor inside rsi, a TUI for managing Claude sessions. Complete the following task concisely and completely. Do not ask follow-up questions \u{2014} work with what you have. If you encounter a blocking error that prevents completion, output exactly `[TASKRABBIT_ESCALATE]` as the very last line of your response so the system can escalate to an interactive session.";

/// Maximum number of conversations fetched per RPC batch.
pub(crate) const CONVERSATION_BATCH_SIZE: usize = 4;

/// Describes how fetched events should be applied to session state.
#[derive(Debug, Clone, Copy)]
pub(super) enum EventApplyMode {
    Replace,
    Append,
}

/// Result envelope for an on-demand conversation fetch dispatched on
/// focus change (RSI hierarchy nav latency refactor: Option A).
///
/// The `since_sequence` captures `state.last_sequence` at the moment of
/// dispatch so the receiver picks the matching `EventApplyMode` (Replace
/// if None, Append if Some) without re-reading state, which may have
/// raced between dispatch and result delivery.
///
/// `events` carries either the fetched payload or a stringified error.
/// On error the inflight set is cleared but no state mutation happens --
/// the next periodic poll cycle retries.
#[derive(Debug)]
pub struct FocusFetchResult {
    pub session_id: Uuid,
    pub since_sequence: Option<i32>,
    pub events: Result<Vec<rsi_common::types::ConversationEvent>, String>,
}

/// One bounded fallback-poll result. The task owns its short-lived client and
/// returns only data/error classification; the event-loop task mutates App.
#[derive(Debug)]
pub struct ConversationPollResult {
    /// Bootstrap/connection attempt that owned this request. Results from an
    /// earlier transport generation must not replace a newer snapshot.
    pub attempt: u64,
    /// Monotonic owner generation for the exact task that produced this
    /// result. A queued older completion cannot settle a newer task.
    pub generation: u64,
    pub batches:
        Result<Vec<(Uuid, Option<i32>, Vec<rsi_common::types::ConversationEvent>)>, String>,
    pub batch_unsupported: bool,
    pub connection_lost: bool,
}

/// Completion from an owned, bounded `/resume_handoff` launch dispatch.
/// The event-loop task owns deduplication; the task only performs socket I/O.
pub struct AutoResumeHandoffResult {
    pub generation: u64,
    pub source_session_id: Uuid,
    pub accepted: Result<Uuid, String>,
    pub model_warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DocregCommandOccurrence {
    pub command: String,
    pub ordinal: usize,
}

#[derive(Debug, Default)]
struct DocregLaunchProgress {
    accepted: HashSet<DocregCommandOccurrence>,
    restore_required: bool,
}

fn conversation_docregblock_contents(
    events: &[rsi_common::types::ConversationEvent],
) -> Vec<String> {
    const OPEN: &str = "<docregblock>";
    const CLOSE: &str = "</docregblock>";

    let mut commands = Vec::new();
    for event in events {
        let mut remaining = event.content.as_str();
        while let Some(open) = remaining.find(OPEN) {
            remaining = &remaining[open + OPEN.len()..];
            let Some(close) = remaining.find(CLOSE) else {
                break;
            };
            let command = remaining[..close].trim();
            if !command.is_empty() {
                commands.push(command.to_string());
            }
            remaining = &remaining[close + CLOSE.len()..];
        }
    }
    commands
}

/// Result envelope for a model segments fetch triggered by navigation to a SessionDetail.
/// Contains the fetched segments or an error, keyed by session_id.
#[derive(Debug)]
pub struct ModelSegmentsFetchResult {
    pub session_id: Uuid,
    pub segments: Result<Vec<rsi_common::types::ModelSegment>, String>,
}

/// Dependency key for the navigation-scoped fetch effect (the React
/// `useEffect(deps=[active_node])` analogue used by
/// `App::run_navigation_effect_if_changed`).
///
/// - `Root`         → no descent (the top-level Group/Standard list)
/// - `Container(id)` → descended into the Group/Epic identified by `id`
/// - `Leaf(id)`      → a leaf-detail pane is focused (Story/Standard/...)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum NavigationEffectNode {
    Root,
    Container(Uuid),
    Leaf(Uuid),
}

/// Result envelope for a targeted `ListSessionChildren` dispatched by the
/// navigation hierarchy-fetch effect. Mirrors `FocusFetchResult` semantics:
/// `parent_id` keys the inflight set and `Result` carries either the
/// fetched sessions or a stringified error.
#[derive(Debug)]
pub struct HierarchyFetchResult {
    pub parent_id: Option<Uuid>,
    pub sessions: Result<Vec<Session>, String>,
}

#[derive(Debug)]
pub struct SettingsModelDiscoveryResult {
    pub item_idx: usize,
    pub provider: SessionProvider,
    pub custom_provider_index: Option<usize>,
    pub models: Vec<(String, String)>,
}

pub(crate) struct ClassifierConfigResult {
    pub generation: u64,
    pub model_id: String,
    pub accepted: Result<(), String>,
}

pub(crate) struct PendingClassifierConfig {
    pub generation: u64,
    pub model_id: String,
    pub restore_dropdown: crate::types::ModelDropdownState,
}

#[derive(Debug)]
pub struct ModelDiscoveryResult {
    pub provider: SessionProvider,
    pub models: Vec<(String, String)>,
}

#[derive(Debug, Default, Clone)]
pub struct AppMetrics {
    pub last_render_ms: Option<f64>,
    pub last_poll_ms: Option<f64>,
    pub last_cache_hit_rate: Option<f64>,
}

impl AppMetrics {
    pub fn record_render(&mut self, elapsed: Duration, counters: CacheCounters) {
        self.last_render_ms = Some(elapsed.as_secs_f64() * 1000.0);
        let total = counters.total();
        self.last_cache_hit_rate = if total > 0 {
            Some(counters.hits as f64 / total as f64)
        } else {
            None
        };
    }

    pub fn record_poll(&mut self, elapsed: Duration) {
        self.last_poll_ms = Some(elapsed.as_secs_f64() * 1000.0);
    }
}

/// Navigation direction for focus movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavDirection {
    Left,
    Right,
    Up,
    Down,
}

/// Top-level application state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalRequest {
    Lazygit(std::path::PathBuf),
    Pager(std::path::PathBuf),
}

pub struct App {
    /// Daemon connection
    pub client: DaemonClient,
    /// Connection and capability state (connected flag, batch-fetch support).
    pub poll: crate::poll_controller::PollController,
    /// Typed startup/reconnect coordinator and process-local storage freshness.
    pub(crate) bootstrap: bootstrap::BootstrapCoordinator,
    /// Launch-time terminal color capability; stable for editor assessments.
    pub terminal_color_capability: crate::ui::theme_roles::TerminalColorCapability,

    /// Sessions indexed by ID, with ordered list of IDs
    pub sessions: HashMap<Uuid, SessionState>,
    pub session_order: Vec<Uuid>,

    /// Index from `parent_id` -> ordered list of direct children. Key `None`
    /// means top-level sessions (no parent); key `Some(uuid)` means children
    /// of that container. Order within each bucket matches `session_order`.
    ///
    /// Rebuilt by `rebuild_children_index()` at the end of `sort_sessions`
    /// and at the top of `recalculate_filtered_order`, so any push-handler
    /// path that mutates session_order / parent_id without calling sort
    /// still sees a consistent index when the filter pipeline runs. The
    /// index is a derived view -- never mutated incrementally.
    pub children_by_parent: HashMap<Option<Uuid>, Vec<Uuid>>,

    /// Workflow status cache indexed by workflow ID.
    pub workflows: HashMap<Uuid, rsi_common::types::Workflow>,
    /// Recursive task graph summaries cached at gv picker-open time (ordered so
    /// the "Recursive graphs" picker can index by position). Populated only when
    /// the `gv_render_recursive_origin` cap is observed true.
    pub recursive_graphs: Vec<rsi_common::recursive_dag::RecursiveTaskGraphSummary>,
    /// Graph editor drafts keyed by primary draft identity.
    pub graph_drafts: HashMap<Uuid, GraphDraft>,
    /// Most recently active graph draft. Reopened by `gv` until an explicit open/clone flow exists.
    pub active_graph_draft_id: Option<Uuid>,
    /// Active and recent workflow execution snapshots keyed by execution ID.
    pub graph_executions: HashMap<Uuid, rsi_common::types::WorkflowExecutionSnapshot>,

    /// Tab/split layout
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    pub next_pane_id: u64,

    /// UI state
    pub input_mode: InputMode,
    pub input_purpose: InputPurpose,
    pub quit: bool,
    /// Dirty flag: when true, the next render tick will redraw the terminal.
    /// Set by any state mutation; cleared after each draw.
    pub needs_redraw: bool,
    /// When true, the main loop should kick off background model discovery.
    pub needs_model_refresh: bool,
    /// Optional one-shot provider target for prompt-local model discovery.
    pub model_refresh_provider: Option<SessionProvider>,
    /// Receiver for background model discovery results.
    pub model_discovery_rx: Option<tokio::sync::oneshot::Receiver<ModelDiscoveryResult>>,
    /// Receiver for Agent Actors row-specific provider discovery results.
    pub settings_model_discovery_rx:
        Option<tokio::sync::oneshot::Receiver<SettingsModelDiscoveryResult>>,
    /// When true, the main loop should kick off background local (Ollama) model discovery.
    pub needs_local_model_refresh: bool,
    /// Receiver for background local model discovery results.
    pub local_model_discovery_rx: Option<tokio::sync::oneshot::Receiver<Vec<(String, String)>>>,
    /// Set by the renderer when a loading bar is painted; keeps redraws
    /// flowing at ~30 fps so the animation stays smooth.
    pub has_loading_bar: bool,
    /// Set by the renderer when a formulation animation is active; keeps
    /// redraws flowing at ~30 fps during the ~200 ms grow animation.
    pub has_formulation_animation: bool,
    /// Whether HUD rails are currently rendered (used for status bar fallback).
    pub hud_rails_visible: bool,
    /// When true, all actions (navigation, fold, search, lifecycle, etc.)
    /// operate on the session list clone within the session detail view
    /// instead of the detail content. Toggled by Backspace. Implemented via
    /// `interaction_pane_id()` which transparently proxies `focused_pane()`
    /// to the SessionList pane.
    pub detail_list_focused: bool,

    /// When set, the event loop will suspend the TUI and run an external program.
    pub pending_external: Option<ExternalRequest>,
    /// Manual opening is deferred until event dispatch returns.
    pub pending_manual: Option<crate::manual::open::ManualMode>,

    /// Timestamp of last paste operation — used to deduplicate Ctrl+V Press+Release
    /// events on terminals with Kitty keyboard protocol (Ghostty, Alacritty).
    pub last_paste_instant: std::time::Instant,

    /// Active prompt processor, built from `settings.prompt_processor` at startup.
    /// `None` means disabled or misconfigured.
    pub prompt_processor: Option<Box<dyn crate::prompt_processor::PromptProcessor>>,

    /// Oneshot receiver for an in-flight prompt compilation task.
    /// Carries the overlay surface id so results are routed to the right popup.
    pub prompt_compile_rx: Option<(
        uuid::Uuid,
        String,
        tokio::sync::oneshot::Receiver<Result<crate::prompt_processor::CompileResult, String>>,
    )>,

    /// Oneshot receiver for an in-progress input-bar prompt compilation.
    /// Carries the session_id and original input so the result is delivered to the right bar.
    pub input_bar_compile_rx: Option<(
        uuid::Uuid,
        String,
        tokio::sync::oneshot::Receiver<Result<crate::prompt_processor::CompileResult, String>>,
    )>,

    /// Oneshot receiver for an in-flight AI command transformation.
    /// Carries the source provenance so the result lands on the correct InputSurface.
    pub ai_command_rx: Option<(
        crate::types::AiAssistantSource,
        tokio::sync::oneshot::Receiver<Result<String, String>>,
    )>,

    /// Oneshot receiver for an in-flight AI chat response.
    pub ai_chat_rx: Option<tokio::sync::oneshot::Receiver<Result<String, String>>>,

    /// Pending dialectic query response.
    pub dialectic_rx: Option<
        tokio::sync::oneshot::Receiver<Result<rsi_common::rpc::QueryMemoryResponse, String>>,
    >,

    /// Pending recursive DAG browser readback.
    pub recursive_dag_rx: Option<
        tokio::sync::oneshot::Receiver<Result<crate::types::RecursiveDagAsyncResult, String>>,
    >,
    /// Last bounded recursive DAG browser state. This is render-only cache
    /// populated by the explicit `:dag` load path; it is not polled.
    pub recursive_dag_cache: Option<crate::types::RecursiveDagBrowserState>,

    /// Input buffers
    pub input_buffer: String,
    pub command_buffer: String,

    /// Active search query (used in Search input mode)
    pub search_query: String,
    /// What the current search is targeting
    pub search_target: SearchTarget,
    /// For session detail: event indices of matches
    pub search_matches: Vec<usize>,
    /// Current match index for n/N navigation
    pub search_match_cursor: usize,

    /// Last-known render/poll metrics (RSI_PROFILE only).
    pub metrics: AppMetrics,

    // --- Notification System ---
    /// Active notifications (newest at back). TTL-expired entries pruned each render frame.
    pub notifications: std::collections::VecDeque<crate::types::Notification>,
    /// Monotonic ID counter for notifications.
    pub next_notification_id: u64,
    /// Dismissed/expired notifications for history overlay. Capped at 100.
    pub notification_history: Vec<crate::types::Notification>,

    /// modalkit key manager for vim keybinding dispatch
    pub key_manager: LcKeyManager,
    /// True when the vim machine is mid-sequence (e.g., Space pressed, waiting for next key).
    /// When set, the input bar bypasses its own normal-mode handling so leader sequences
    /// (like Space+g) pass through to the vim machine.
    pub vim_machine_pending: bool,

    /// Active overlay (popup, prompt, etc.). Captures all input when not None.
    pub overlay: OverlayState,
    /// Suspended overlays beneath the active one.
    pub overlay_stack: Vec<OverlayState>,
    /// Pending overlay-local leader prefix (`Space`) for text-area overlays.
    pub overlay_leader_pending: bool,
    /// Whether an overlay was visible on the previous frame (for cleanup clears).
    pub overlay_visible_last_frame: bool,

    /// Simultaneously visible input overlays (Blank/TaskRabbit prompts), oldest first.
    /// These render as a vertical stack and are navigable with Ctrl+J/Ctrl+K.
    pub input_overlays: Vec<OverlayState>,
    /// Index of the focused input overlay in `input_overlays` (receives key input).
    pub focused_input_idx: usize,
    /// Set when the focused pane switches type (e.g. SessionList → SessionDetail)
    /// or when the terminal is resized / re-shown (i3 hide/show cycle).
    /// Triggers a full-frame clear on the next render to flush stale terminal cells
    /// that ratatui's diff algorithm can't detect (transparent backgrounds, or
    /// compositor surface resets after window visibility changes).
    pub pane_switch_clear: bool,
    /// Last known terminal dimensions (cols, rows). Used to detect size changes
    /// even when crossterm's `CEvent::Resize` is dropped (known i3/Ghostty issue).
    pub last_terminal_size: (u16, u16),

    /// Selected model for new session launches (None = default).
    pub selected_model: Option<String>,
    /// Selected provider for new session launches.
    pub selected_provider: SessionProvider,
    /// Selected effort level for new reasoning-capable sessions.
    /// None = provider default. Claude uses "max"; Codex supports model-dependent top efforts.
    pub selected_effort: Option<String>,
    /// Index into `settings.custom_providers` when a custom provider is selected.
    /// `None` means a built-in provider (Claude/Codex/Pioneer/Local/etc.) is active.
    pub custom_provider_index: Option<usize>,
    /// Availability map reported by daemon health check.
    pub provider_availability: HashMap<SessionProvider, bool>,
    /// Latest account-level plan-window utilization per provider (V99, P1-B).
    ///
    /// Seeded from `GetHealthStatus` on connect and then kept current by the
    /// `provider_rate_limit_updated` push event — the push path is what makes
    /// this live, since health status is only fetched at connect time.
    pub provider_rate_limits: HashMap<SessionProvider, ProviderRateLimitSnapshot>,

    /// Dynamically discovered models (model_id, display_name) for selected provider.
    /// Falls back to provider-specific static model lists if discovery fails.
    pub available_models: Vec<(String, String)>,
    /// Dynamically discovered local (Ollama) models, independent of selected_provider.
    /// Always reflects what Ollama reports via GET /v1/models.
    pub local_models: Vec<(String, String)>,

    /// State for the global model dropdown (anchored to status bar).
    pub model_dropdown: crate::types::ModelDropdownState,
    /// Cached anchor rect for model segment in status bar (computed during render).
    pub model_segment_rect: Option<ratatui::layout::Rect>,

    /// Discovered slash commands from .claude/commands/ and .claude/skills/
    pub available_commands: Vec<crate::suggestions::CommandSuggestion>,

    // --- Project filtering ---
    /// All projects fetched from daemon.
    pub projects: Vec<rsi_common::types::Project>,
    pub(crate) manager_roster: manager_roster::ManagerRoster,
    manager_roster_refresh: Option<tokio::task::JoinHandle<manager_roster::RefreshResults>>,

    /// All session labels from daemon (refreshed on poll).
    pub labels: Vec<rsi_common::types::SessionLabel>,

    /// Current project filter. None = show all sessions.
    pub current_project_id: Option<Uuid>,

    /// Filtered session order (subset of session_order matching current_project_id).
    /// Recalculated when project filter or sessions change.
    pub filtered_session_order: Vec<Uuid>,

    /// Filtered TaskRabbit sessions: last 2 hours or 10 most recent.
    /// Pinned TaskRabbit sessions always included.
    pub filtered_taskrabbit_order: Vec<Uuid>,

    /// Archived sessions loaded from daemon for the Archive zone.
    pub filtered_archived_order: Vec<Uuid>,

    /// Sessions spawned by scheduled jobs for the Jobs zone.
    pub filtered_jobs_order: Vec<Uuid>,

    /// Last session detail that was focused (for `i` in session list).
    pub last_viewed_session: Option<Uuid>,

    // --- Jumplist (Ctrl+O / Ctrl+I) ---
    /// Session jumplist for Ctrl+O / Ctrl+I navigation.
    /// Ordered list of visited session IDs with a cursor.
    pub session_jumplist: Vec<Uuid>,
    pub jumplist_cursor: usize,
    /// Flag: true during jump_back/jump_forward to suppress recording.
    pub navigating_jumplist: bool,

    /// Stashed session view state from dev-state hot-reload, applied after first poll.
    pub pending_dev_views: Option<HashMap<String, crate::state::SessionViewState>>,

    /// Stashed fold states from PersistedState, applied after first poll.
    pub pending_fold_states: Option<HashMap<String, bool>>,

    /// Per-modal geometry deltas keyed by PromptPurpose variant name.
    pub modal_geometries: HashMap<String, crate::types::ModalGeometry>,

    /// Last-used modal dropdown values (P2.4). Restored from `PersistedState`
    /// at startup; rewritten on every successful modal submit.
    pub modal_defaults: crate::state::ModalDefaults,

    /// Draft text preserved across TaskRabbit popup open/close cycles.
    pub taskrabbit_draft: Vec<String>,

    /// Draft text preserved across Blank popup open/close cycles.
    pub blank_draft: Vec<String>,

    /// Draft state for the unified entity-creation modal preserved across
    /// Esc + re-open cycles (locked decision §B7). Cleared on `<C-d>` and
    /// on successful submit. Mirrors the `blank_draft` shape.
    pub create_entity_draft: Option<crate::types::CreateEntityDraft>,

    /// Snapshot of the in-flight `CreateEntityForm` overlay state when a
    /// sub-overlay (parent picker, etc.) is open on top. Set by
    /// `overlay::create_entity_form::push_to_sub_overlay`, consumed by the
    /// sub-overlay's close path which writes back the chosen value and
    /// restores the form to `app.overlay`. NEVER persisted.
    pub create_entity_form_pending: Option<Box<crate::types::OverlayState>>,

    /// One response-bearing interactive launch. UI destruction and placement
    /// are committed only after the matching generation returns a session ID.
    pub(crate) interactive_launch_generation: u64,
    pub(crate) interactive_launch_pending: Option<PendingInteractiveLaunch>,
    pub(crate) interactive_launch_handle: Option<tokio::task::JoinHandle<()>>,
    pub(crate) interactive_launch_tx: tokio::sync::mpsc::Sender<InteractiveLaunchResult>,
    pub(crate) interactive_launch_rx: tokio::sync::mpsc::Receiver<InteractiveLaunchResult>,
    /// Placement intent keyed by the exact daemon-returned identity. A push
    /// for an unrelated new session can never consume this map entry.
    pub(crate) accepted_launch_placements: HashMap<Uuid, LaunchPlacement>,

    /// Accepted replacement launches retained while their terminal docreg
    /// source remains retryable. Stable command-occurrence identities survive
    /// append/reorder edits, so a retry cannot duplicate an accepted launch.
    /// A raced archive also leaves an explicit response-backed restore bit.
    docreg_launch_progress: HashMap<Uuid, DocregLaunchProgress>,
    pub(crate) docreg_operation_generation: u64,
    pub(crate) docreg_operation_pending: Option<PendingDocregOperation>,
    docreg_operation_context: Option<DocregOperationContext>,
    pub(crate) docreg_operation_handle: Option<tokio::task::JoinHandle<()>>,
    pub(crate) docreg_operation_tx: tokio::sync::mpsc::Sender<DocregOperationResult>,
    pub(crate) docreg_operation_rx: tokio::sync::mpsc::Receiver<DocregOperationResult>,

    /// Sessions already auto-launched via `/resume_handoff` docregblock.
    /// Prevents re-triggering on subsequent poll cycles.
    pub(crate) auto_launched_handoff_sessions: HashSet<Uuid>,

    /// Centralized user settings (sort order, center layout, status bar, etc.).
    pub settings: UserSettings,

    /// Settings pane navigation state (category, selected item, focus panel).
    pub settings_state: crate::types::SettingsState,

    /// Stashed pane content to restore when settings is closed.
    pub pre_settings_pane: Option<Pane>,

    /// Prompt creator/editor navigation state.
    pub prompt_creator_state: crate::types::PromptCreatorState,
    /// File viewer for the prompt creator (independent of session file viewers).
    pub prompt_creator_viewer: Option<crate::types::FileViewerState>,
    /// Stashed pane content to restore when prompt creator is closed.
    pub pre_prompt_creator_pane: Option<Pane>,

    /// Pane content replaced by an Issues workspace, keyed by physical leaf.
    pub pre_issues_panes: HashMap<PaneId, Pane>,

    /// Cached daemon feature toggles for the DaemonFeatures settings category.
    /// Populated on first entry to the category; refreshed on each entry.
    pub daemon_features: Vec<crate::settings::DaemonFeatureEntry>,

    /// One response-bearing classifier mutation. The daemon-owned mirror and
    /// dropdown lifecycle are committed only by its generation-matched result.
    pub(crate) classifier_config_generation: u64,
    pub(crate) classifier_config_pending: Option<PendingClassifierConfig>,
    pub(crate) classifier_config_handle: Option<tokio::task::JoinHandle<()>>,
    pub(crate) classifier_config_tx: tokio::sync::mpsc::Sender<ClassifierConfigResult>,
    pub(crate) classifier_config_rx: tokio::sync::mpsc::Receiver<ClassifierConfigResult>,

    /// Deferred LcActions enqueued by synchronous key handlers (e.g. settings_keys)
    /// that need async dispatch. Drained by the event loop after the key handler returns.
    pub pending_lc_actions: Vec<crate::modalkit_types::LcAction>,

    /// Push notification stream from daemon (second socket connection).
    /// `Some` when daemon supports push and connection is active.
    pub notification_stream: Option<crate::notification_stream::NotificationStream>,

    /// Sessions currently being fetched on-demand via the focus-change
    /// effect (RSI hierarchy nav latency refactor: Option A). Guards
    /// against duplicate dispatches when the user rapidly re-focuses the
    /// same session. Cleared when the result arrives (success OR error).
    pub focus_fetch_inflight: HashSet<Uuid>,

    /// Sender half of the focus-fetch result channel. Cloned into spawned
    /// fetch tasks; held by `App` so it never closes.
    pub focus_fetch_tx: tokio::sync::mpsc::UnboundedSender<FocusFetchResult>,

    /// Receiver half of the focus-fetch result channel. Polled by a
    /// `select!` arm in `event::run_event_loop`.
    pub focus_fetch_rx: tokio::sync::mpsc::UnboundedReceiver<FocusFetchResult>,

    /// The periodic fallback poll is one owned, cancellable task rather than
    /// an awaited RPC in the event loop.
    pub conversation_poll_handle: Option<tokio::task::JoinHandle<()>>,
    pub(crate) conversation_poll_generation: u64,
    pub(crate) conversation_poll_active_generation: Option<u64>,
    pub(crate) conversation_poll_active_cursors: Vec<rsi_common::rpc::ConversationFetchCursor>,
    pub conversation_poll_tx: tokio::sync::mpsc::Sender<ConversationPollResult>,
    pub conversation_poll_rx: tokio::sync::mpsc::Receiver<ConversationPollResult>,
    /// One retained cycle continuation makes multi-batch fallback polling
    /// event-driven without a timer retry loop while work is in flight.
    pub conversation_poll_pending_phase: Option<crate::poll_controller::PollPhase>,
    /// A selected/active initial hydration batch that arrived behind a live
    /// poll. It is bounded to one normal conversation batch.
    pub conversation_poll_hydration_pending: Option<Vec<rsi_common::rpc::ConversationFetchCursor>>,

    /// A single owned launch task keeps auto-resume RPC writes off the event
    /// loop. Sessions are marked deduplicated only after its accepted result.
    pub auto_resume_handoff_handle: Option<tokio::task::JoinHandle<()>>,
    pub(crate) auto_resume_handoff_generation: u64,
    pub(crate) auto_resume_handoff_active_generation: Option<u64>,
    pub auto_resume_handoff_inflight: HashSet<Uuid>,
    /// Failed docreg sources are skipped until a demonstrable external
    /// progress epoch (conversation/status/metadata/config recovery).
    pub auto_resume_handoff_failed: HashMap<Uuid, u64>,
    pub(crate) auto_resume_progress_epoch: u64,
    pub auto_resume_handoff_tx: tokio::sync::mpsc::Sender<AutoResumeHandoffResult>,
    pub auto_resume_handoff_rx: tokio::sync::mpsc::Receiver<AutoResumeHandoffResult>,

    /// Sessions currently having model segments fetched on-demand via the
    /// SessionDetail focus effect. Guards against duplicate dispatches when
    /// rapidly switching between the same session detail panes.
    pub model_segments_fetch_inflight: HashSet<Uuid>,

    /// Sender half of the model segments fetch result channel.
    pub model_segments_fetch_tx: tokio::sync::mpsc::UnboundedSender<ModelSegmentsFetchResult>,

    /// Receiver half of the model segments fetch result channel.
    pub model_segments_fetch_rx: tokio::sync::mpsc::UnboundedReceiver<ModelSegmentsFetchResult>,

    /// Last `NavigationEffectNode` that ran the hierarchy/detail fetch
    /// effect. Mirrors a React `useEffect([deps])` guard: when the active
    /// navigation node (Root / Container / Leaf) is unchanged, no fetch
    /// fires — eliminating redundant network requests on idle re-renders.
    pub(crate) navigation_effect_node: Option<NavigationEffectNode>,

    /// Parent nodes currently being refreshed via on-demand
    /// `ListSessionChildren` after a hierarchy view switch. Mirrors
    /// `focus_fetch_inflight` for the container-children-fetch path.
    pub hierarchy_fetch_inflight: HashSet<Option<Uuid>>,

    /// Parent nodes whose children list is known to match the daemon
    /// snapshot. Full `ListSessions` marks all nodes loaded; a targeted
    /// `ListSessionChildren` marks just one node. Membership is the
    /// "data is fresh, skip refetch" gate.
    pub hierarchy_loaded_nodes: HashSet<Option<Uuid>>,

    /// Parent nodes dirtied by push events (`session_metadata_changed`,
    /// `child_spawned`, etc.). Re-entering an otherwise-loaded node
    /// fetches only when stale, so push handles incrementals and the
    /// nav effect only does targeted refreshes.
    pub hierarchy_stale_nodes: HashSet<Option<Uuid>>,

    /// Sender half for targeted hierarchy children fetch results.
    /// Cloned into spawned `ListSessionChildren` tasks.
    pub hierarchy_fetch_tx: tokio::sync::mpsc::UnboundedSender<HierarchyFetchResult>,

    /// Receiver half polled by `event::run_event_loop`'s `select!` arm.
    pub hierarchy_fetch_rx: tokio::sync::mpsc::UnboundedReceiver<HierarchyFetchResult>,

    /// Generation-fenced Issue workspace results from dedicated daemon clients.
    pub issues_tx: tokio::sync::mpsc::UnboundedSender<issues::IssueWorkspaceAsyncEvent>,
    pub issues_rx: tokio::sync::mpsc::UnboundedReceiver<issues::IssueWorkspaceAsyncEvent>,

    /// Navigation data cache for immediate rendering with background updates.
    /// Provides React useEffect-like dependency tracking and push notification-driven invalidation.
    pub navigation_cache: cache::NavigationDataCache,

    /// Cached memory provider status, fetched once at connect time.
    /// `None` when memory is disabled or daemon doesn't support memory search.
    pub memory_status: Option<rsi_common::rpc::MemoryProviderStatus>,

    /// Directory for storing pasted clipboard images (~/.rsi/paste/).
    pub paste_dir: std::path::PathBuf,

    /// Cached render state for the session list card layout.
    pub session_list_render: crate::types::SessionListRenderState,

    /// Bumped on any card-affecting state change (fold toggle, session data update).
    /// Consumed by ZoneRenderState cache invalidation.
    pub card_generation: u64,

    /// Cached FLYWHEEL.md workflow status per project, fetched lazily when project form is opened.
    /// Value is a raw JSON response from GetProjectWorkflow: `{ exists, healthy, last_error, loaded_at }`.
    pub workflow_statuses: HashMap<Uuid, serde_json::Value>,

    /// Embedded terminal emulator (lazy-spawned on first toggle).
    pub terminal: Option<crate::terminal::EmbeddedTerminal>,
    /// Receiver for PTY output bytes from the reader thread.
    pub terminal_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,

    /// Cached `~/.claude/settings.json` for the Settings -> Hooks category.
    /// Populated on first read; invalidated after a successful save / external reload.
    pub cached_claude_settings: Option<crate::claude_config::LoadedSettings>,
    /// Flattened hook rows derived from `cached_claude_settings`. Recomputed
    /// on every `cached_claude_settings_rows()` call after the cache is
    /// (re)populated; `&[HookRow]` is returned to the renderer.
    pub cached_hook_rows: Vec<crate::claude_config::HookRow>,
    /// Cached `~/.claude/skills/` listing for the Settings -> Skills category.
    /// Same lifecycle as `cached_claude_settings`.
    pub cached_user_skills: Option<Vec<crate::claude_config::SkillEntry>>,

    /// Cached lifetime usage aggregate for the Settings -> Stats category
    /// (T8). Populated on first entry to the category; refreshed on each
    /// entry (mirrors `daemon_features`/`RefreshDaemonFeatures`).
    pub cached_usage_stats: Option<rsi_common::types::UsageStats>,

    /// Cached model-control/operator status for Settings -> Stats and
    /// Settings -> Daemon Features live controls.
    pub cached_model_control_status: Option<rsi_common::model_control::ModelControlStatusReport>,
}

/// Replace any `Pane::Settings` leaves with `SessionList` so settings panes
/// are never persisted across restarts or hot-reloads.
fn sanitize_tabs(mut tabs: Vec<Tab>) -> Vec<Tab> {
    for tab in &mut tabs {
        sanitize_node(&mut tab.layout);
    }
    tabs
}

fn sanitize_node(node: &mut SplitNode) {
    match node {
        SplitNode::Leaf { pane, .. } => {
            if matches!(pane, Pane::Settings | Pane::PromptCreator) {
                *pane = Pane::SessionList {
                    selected_index: 0,
                    selected_session: None,
                    scroll_offset: 0,
                    active_zone: Default::default(),
                    taskrabbit_selected_index: 0,
                    archive_selected_index: 0,
                    jobs_selected_index: 0,
                };
            } else if let Pane::Issues(state) = pane {
                state.normalize_after_restore();
            }
        }
        SplitNode::Split { first, second, .. } => {
            sanitize_node(first);
            sanitize_node(second);
        }
    }
}

/// Replace a node in the split tree by PaneId, applying a transformation.
pub(crate) fn replace_node(
    node: SplitNode,
    target: PaneId,
    replacement: impl FnOnce(SplitNode) -> SplitNode,
) -> SplitNode {
    match node {
        SplitNode::Leaf { id, .. } if id == target => replacement(node),
        SplitNode::Split {
            direction,
            first,
            second,
            id,
        } => {
            if first.find_pane(target).is_some() || first.id() == target {
                SplitNode::Split {
                    direction,
                    first: Box::new(replace_node(*first, target, replacement)),
                    second,
                    id,
                }
            } else {
                SplitNode::Split {
                    direction,
                    first,
                    second: Box::new(replace_node(*second, target, replacement)),
                    id,
                }
            }
        }
        other => other,
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(handle) = self.conversation_poll_handle.take() {
            handle.abort();
        }
        self.conversation_poll_active_generation = None;
        self.conversation_poll_active_cursors.clear();
        if let Some(handle) = self.auto_resume_handoff_handle.take() {
            handle.abort();
        }
        self.auto_resume_handoff_active_generation = None;
        if let Some(handle) = self.interactive_launch_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.docreg_operation_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.classifier_config_handle.take() {
            handle.abort();
        }
    }
}

impl App {
    pub fn new(client: DaemonClient) -> Self {
        let initial_pane_id = PaneId(0);
        let list_pane = Pane::SessionList {
            selected_index: 0,
            selected_session: None,
            scroll_offset: 0,
            active_zone: Default::default(),
            taskrabbit_selected_index: 0,
            archive_selected_index: 0,
            jobs_selected_index: 0,
        };
        let initial_tab = Tab {
            name: "[1]".to_string(),
            session_list_state: list_pane.clone(),
            layout: SplitNode::Leaf {
                pane: list_pane,
                id: initial_pane_id,
            },
            focused_pane: initial_pane_id,
            project_id: None,
            detail_split_offset: 0,
            layout_x_offset_adj: 0,
            session_list_width_pct: 0,
            descent_path: Vec::new(),
            bottom_focus_target: crate::types::BottomZone::Off,
            mini_dag_focus: None,
        };

        // Discover commands at startup
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
        let available_commands = crate::suggestions::discover_commands(&cwd);

        // Load persisted state (project filter, etc.)
        let persisted = PersistedState::load();

        // Try to restore dev state for hot-reload
        let dev_state = DevState::load();

        let (
            tabs,
            active_tab,
            next_pane_id,
            last_viewed_session,
            selected_model,
            selected_provider,
            selected_effort,
            current_project_id,
            pending_dev_views,
            pending_fold_states,
            session_jumplist,
            jumplist_cursor,
            settings,
            theme_name,
            active_graph_draft_id,
            restored_graph_drafts,
            restored_create_entity_draft,
        ) = if let Some(ds) = dev_state {
            DevState::clear();
            let tabs = if ds.tabs.is_empty() {
                vec![initial_tab]
            } else {
                ds.tabs
            };
            let active_tab = ds.active_tab.min(tabs.len().saturating_sub(1));
            let next_pane_id = ds.next_pane_id.max(1);
            let pending = if ds.session_views.is_empty() {
                None
            } else {
                Some(ds.session_views)
            };
            let mut settings = ds.settings.clone();
            settings.sort_order = ds.sort_order;
            (
                tabs,
                active_tab,
                next_pane_id,
                ds.last_viewed_session,
                ds.selected_model,
                ds.selected_provider.unwrap_or(SessionProvider::Claude),
                ds.selected_effort,
                ds.current_project_id.or(persisted.current_project_id),
                pending,
                None, // pending_fold_states (DevState has fold state in session_views)
                ds.session_jumplist,
                ds.jumplist_cursor,
                settings,
                ds.theme_flavor.or(persisted.theme_flavor.clone()),
                ds.active_graph_draft_id,
                ds.graph_drafts
                    .into_iter()
                    .map(|draft| {
                        let draft = draft.normalize_after_restore();
                        (draft.draft_id, draft)
                    })
                    .collect::<HashMap<_, _>>(),
                ds.create_entity_draft,
            )
        } else {
            let restored = crate::state::emergency_drafts::load();
            let tabs = if persisted.tabs.is_empty() {
                vec![initial_tab]
            } else {
                persisted.tabs.clone()
            };
            let active_tab = persisted.active_tab.min(tabs.len().saturating_sub(1));
            let next_pane_id = persisted.next_pane_id.max(1);
            let mut settings = persisted.settings.clone();
            settings.sort_order = persisted.sort_order;
            let fold_states = if persisted.session_fold_states.is_empty() {
                None
            } else {
                Some(persisted.session_fold_states.clone())
            };
            (
                tabs,
                active_tab,
                next_pane_id,
                persisted.last_viewed_session,
                persisted.selected_model.clone(),
                persisted
                    .selected_provider
                    .unwrap_or(SessionProvider::Claude),
                persisted.selected_effort.clone(),
                persisted.current_project_id,
                None, // pending_dev_views (only from DevState)
                fold_states,
                persisted.session_jumplist.clone(),
                persisted
                    .jumplist_cursor
                    .min(persisted.session_jumplist.len().saturating_sub(1)),
                settings,
                persisted.theme_flavor.clone(),
                restored.active_graph_draft_id,
                // Cold start — check for emergency graph drafts from a previous crash/exit
                restored.drafts,
                None,
            )
        };

        let active_graph_draft_id = active_graph_draft_id
            .filter(|draft_id| restored_graph_drafts.contains_key(draft_id))
            .or_else(|| restored_graph_drafts.keys().next().copied());

        // Sanitize: replace any persisted Settings panes with SessionList fallback.
        let tabs = sanitize_tabs(tabs);

        let mut provider_availability = HashMap::new();
        provider_availability.insert(SessionProvider::Claude, true);
        provider_availability.insert(SessionProvider::Codex, true);
        provider_availability.insert(SessionProvider::Pioneer, true);
        provider_availability.insert(SessionProvider::OpenRouter, true);
        provider_availability.insert(SessionProvider::Bedrock, true);
        provider_availability.insert(SessionProvider::Local, true);
        provider_availability.insert(SessionProvider::Antigravity, true);

        let selected_model = if selected_provider == SessionProvider::Pioneer
            && selected_model.as_deref() == Some(PIONEER_RETIRED_AUTO_MODEL)
        {
            Some(PIONEER_MODELS[0].0.to_string())
        } else {
            selected_model
        };

        // Default effort based on model when not persisted
        let selected_effort =
            selected_effort.or_else(|| default_effort_for_model(selected_model.as_deref()));

        // Navigation-scoped fetch channels: conversation fetches for leaf
        // detail views and child-list fetches for root/container views.
        let (focus_fetch_tx, focus_fetch_rx) = tokio::sync::mpsc::unbounded_channel();
        let (conversation_poll_tx, conversation_poll_rx) = tokio::sync::mpsc::channel(1);
        let (auto_resume_handoff_tx, auto_resume_handoff_rx) = tokio::sync::mpsc::channel(1);
        let (interactive_launch_tx, interactive_launch_rx) = tokio::sync::mpsc::channel(1);
        let (docreg_operation_tx, docreg_operation_rx) = tokio::sync::mpsc::channel(1);
        let (classifier_config_tx, classifier_config_rx) = tokio::sync::mpsc::channel(1);
        let (hierarchy_fetch_tx, hierarchy_fetch_rx) = tokio::sync::mpsc::unbounded_channel();

        // Model segments fetch channel for event-driven SessionDetail model segments loading
        let (model_segments_fetch_tx, model_segments_fetch_rx) =
            tokio::sync::mpsc::unbounded_channel();
        let (issues_tx, issues_rx) = tokio::sync::mpsc::unbounded_channel();

        let app = Self {
            client,
            poll: crate::poll_controller::PollController::default(),
            bootstrap: bootstrap::BootstrapCoordinator::default(),
            terminal_color_capability:
                crate::ui::theme_roles::TerminalColorCapability::from_launch_environment(),
            sessions: HashMap::new(),
            session_order: Vec::new(),
            workflows: HashMap::new(),
            recursive_graphs: Vec::new(),
            graph_drafts: restored_graph_drafts,
            active_graph_draft_id,
            graph_executions: HashMap::new(),
            tabs,
            active_tab,
            next_pane_id,
            input_mode: InputMode::Normal,
            input_purpose: InputPurpose::NewSession,
            quit: false,
            needs_redraw: true, // Draw on first frame
            needs_model_refresh: false,
            model_refresh_provider: None,
            model_discovery_rx: None,
            settings_model_discovery_rx: None,
            needs_local_model_refresh: true,
            local_model_discovery_rx: None,
            has_loading_bar: false,
            has_formulation_animation: false,
            hud_rails_visible: false,
            detail_list_focused: false,
            pending_external: None,
            pending_manual: None,
            last_paste_instant: std::time::Instant::now() - std::time::Duration::from_secs(1),
            prompt_processor: crate::prompt_processor::build_processor(&settings.prompt_processor),
            prompt_compile_rx: None,
            input_bar_compile_rx: None,
            ai_command_rx: None,
            ai_chat_rx: None,
            dialectic_rx: None,
            recursive_dag_rx: None,
            recursive_dag_cache: None,
            input_buffer: String::new(),
            command_buffer: String::new(),
            search_query: String::new(),
            search_target: SearchTarget::default(),
            search_matches: Vec::new(),
            search_match_cursor: 0,
            metrics: AppMetrics::default(),
            notifications: std::collections::VecDeque::new(),
            next_notification_id: 0,
            notification_history: Vec::new(),
            key_manager: build_key_manager(),
            vim_machine_pending: false,
            overlay: OverlayState::None,
            overlay_stack: Vec::new(),
            overlay_leader_pending: false,
            overlay_visible_last_frame: false,
            input_overlays: Vec::new(),
            focused_input_idx: 0,
            pane_switch_clear: false,
            last_terminal_size: (0, 0),
            selected_model,
            selected_provider,
            selected_effort,
            custom_provider_index: None,
            provider_availability,
            provider_rate_limits: HashMap::new(),
            available_models: models_for_provider(selected_provider),
            local_models: models_for_provider(SessionProvider::Local),
            model_dropdown: crate::types::ModelDropdownState::default(),
            model_segment_rect: None,
            available_commands,
            projects: Vec::new(),
            manager_roster: manager_roster::ManagerRoster::default(),
            manager_roster_refresh: None,
            labels: Vec::new(),
            current_project_id,
            filtered_session_order: Vec::new(),
            filtered_taskrabbit_order: Vec::new(),
            filtered_archived_order: Vec::new(),
            filtered_jobs_order: Vec::new(),
            last_viewed_session,
            session_jumplist,
            jumplist_cursor,
            navigating_jumplist: false,
            pending_dev_views,
            pending_fold_states,
            modal_geometries: persisted.modal_geometries.clone(),
            modal_defaults: persisted.modal_defaults.clone(),
            taskrabbit_draft: Vec::new(),
            blank_draft: Vec::new(),
            create_entity_draft: restored_create_entity_draft,
            create_entity_form_pending: None,
            interactive_launch_generation: 0,
            interactive_launch_pending: None,
            interactive_launch_handle: None,
            interactive_launch_tx,
            interactive_launch_rx,
            accepted_launch_placements: HashMap::new(),
            docreg_launch_progress: HashMap::new(),
            docreg_operation_generation: 0,
            docreg_operation_pending: None,
            docreg_operation_context: None,
            docreg_operation_handle: None,
            docreg_operation_tx,
            docreg_operation_rx,
            auto_launched_handoff_sessions: HashSet::new(),
            settings,
            settings_state: crate::types::SettingsState::default(),
            pre_settings_pane: None,
            prompt_creator_state: crate::types::PromptCreatorState::default(),
            prompt_creator_viewer: None,
            pre_prompt_creator_pane: None,
            pre_issues_panes: HashMap::new(),
            daemon_features: crate::settings::DaemonFeatureEntry::defaults(),
            classifier_config_generation: 0,
            classifier_config_pending: None,
            classifier_config_handle: None,
            classifier_config_tx,
            classifier_config_rx,
            pending_lc_actions: Vec::new(),
            notification_stream: None,
            memory_status: None,
            paste_dir: {
                let dir = rsi_common::identity::data_path("paste", "paste");
                let _ = std::fs::create_dir_all(&dir);
                dir
            },
            session_list_render: crate::types::SessionListRenderState::default(),
            card_generation: 0,
            workflow_statuses: HashMap::new(),
            terminal: None,
            terminal_rx: None,
            cached_claude_settings: None,
            cached_hook_rows: Vec::new(),
            cached_user_skills: None,
            cached_usage_stats: None,
            cached_model_control_status: None,
            children_by_parent: HashMap::new(),
            focus_fetch_inflight: HashSet::new(),
            focus_fetch_tx,
            focus_fetch_rx,
            conversation_poll_handle: None,
            conversation_poll_generation: 0,
            conversation_poll_active_generation: None,
            conversation_poll_active_cursors: Vec::new(),
            conversation_poll_tx,
            conversation_poll_rx,
            conversation_poll_pending_phase: None,
            conversation_poll_hydration_pending: None,
            auto_resume_handoff_handle: None,
            auto_resume_handoff_generation: 0,
            auto_resume_handoff_active_generation: None,
            auto_resume_handoff_inflight: HashSet::new(),
            auto_resume_handoff_failed: HashMap::new(),
            auto_resume_progress_epoch: 0,
            auto_resume_handoff_tx,
            auto_resume_handoff_rx,
            model_segments_fetch_inflight: HashSet::new(),
            model_segments_fetch_tx,
            model_segments_fetch_rx,
            navigation_effect_node: None,
            hierarchy_fetch_inflight: HashSet::new(),
            hierarchy_loaded_nodes: HashSet::new(),
            hierarchy_stale_nodes: HashSet::new(),
            hierarchy_fetch_tx,
            hierarchy_fetch_rx,
            issues_tx,
            issues_rx,
            navigation_cache: cache::NavigationDataCache::new(),
        };

        theme::apply_startup_theme_state(
            theme_name.as_deref(),
            &persisted.theme_role_overrides.registered(),
            &persisted.border_color_overrides.as_array(),
        );

        app
    }

    /// Reload the prompt list from disk and clamp the selected index.
    pub fn refresh_prompts(&mut self) {
        self.prompt_creator_state.prompts = crate::prompt_creator::load_prompt_list();
        if !self.prompt_creator_state.prompts.is_empty() {
            self.prompt_creator_state.selected_index = self
                .prompt_creator_state
                .selected_index
                .min(self.prompt_creator_state.prompts.len() - 1);
        } else {
            self.prompt_creator_state.selected_index = 0;
        }
    }

    /// Lazy-load + cache the `~/.claude/settings.json` flattened hook rows.
    /// Returns an empty slice on I/O / parse failure (the user is notified
    /// via the open-form path; this getter is read-only and used by render).
    pub fn cached_claude_settings_rows(&mut self) -> &[crate::claude_config::HookRow] {
        self.ensure_claude_settings_cache();
        // Stash the rows alongside the loaded settings on demand. We recompute
        // here because BTreeMap mutation through cache invalidation is rare
        // (only after a save) and `flatten_hooks` is O(n) over a dozen rows.
        // Alternatively we could memoize, but the file is ~1 KB and rows ~10.
        match &self.cached_claude_settings {
            Some(loaded) => {
                // SAFETY: we cache the row vec on the App through a thread-local
                // to avoid mutating cached_claude_settings here. Simpler: store
                // rows separately. See `cached_claude_settings_rows_owned`.
                self.cached_hook_rows = crate::claude_config::flatten_hooks(&loaded.data);
                &self.cached_hook_rows
            }
            None => &[],
        }
    }

    /// Lazy-load + cache the `~/.claude/skills/` listing.
    pub fn cached_user_skills(&mut self) -> &[crate::claude_config::SkillEntry] {
        if self.cached_user_skills.is_none() {
            match crate::claude_config::list_user_skills() {
                Ok(list) => self.cached_user_skills = Some(list),
                Err(_) => self.cached_user_skills = Some(Vec::new()),
            }
        }
        self.cached_user_skills.as_deref().unwrap_or(&[])
    }

    /// Invalidate the cached `~/.claude/settings.json` snapshot. Call after a
    /// successful save or when the user requests an explicit reload.
    pub fn invalidate_claude_settings_cache(&mut self) {
        self.cached_claude_settings = None;
        self.cached_hook_rows.clear();
    }

    /// Invalidate the cached skills listing.
    pub fn invalidate_claude_skills_cache(&mut self) {
        self.cached_user_skills = None;
    }

    /// Ensure the cached settings are populated. Silently swallows I/O errors —
    /// the form-open path surfaces them with `notify_error`.
    fn ensure_claude_settings_cache(&mut self) {
        if self.cached_claude_settings.is_none() {
            if let Ok(loaded) = crate::claude_config::load_user_settings() {
                self.cached_claude_settings = Some(loaded);
            }
        }
    }

    pub fn push_current_overlay(&mut self) {
        if !matches!(self.overlay, OverlayState::None) {
            let current = std::mem::replace(&mut self.overlay, OverlayState::None);
            self.overlay_stack.push(current);
        }
        self.overlay_leader_pending = false;
    }

    pub fn restore_previous_overlay(&mut self) {
        self.overlay = self.overlay_stack.pop().unwrap_or(OverlayState::None);
        self.overlay_leader_pending = false;
    }

    pub fn previous_overlay(&self) -> Option<&OverlayState> {
        self.overlay_stack.last()
    }

    pub(crate) fn recursive_dag_browser_state(
        &self,
    ) -> Option<&crate::types::RecursiveDagBrowserState> {
        match &self.overlay {
            OverlayState::RecursiveDagBrowser(state)
                if self.recursive_dag_state_matches_current_project(state) =>
            {
                Some(state)
            }
            OverlayState::RecursiveDagBrowser(_) => None,
            _ => self
                .recursive_dag_cache
                .as_ref()
                .filter(|state| self.recursive_dag_state_matches_current_project(state)),
        }
    }

    pub(crate) fn recursive_dag_state_matches_current_project(
        &self,
        state: &crate::types::RecursiveDagBrowserState,
    ) -> bool {
        state.project_id == self.current_project_id
    }

    /// Returns `true` if any overlay (regular or input stack) is active.
    pub fn any_overlay_active(&self) -> bool {
        !matches!(self.overlay, OverlayState::None) || !self.input_overlays.is_empty()
    }

    /// Get a reference to the focused input overlay (if any).
    pub fn focused_input_overlay(&self) -> Option<&OverlayState> {
        self.input_overlays.get(self.focused_input_idx)
    }

    /// Get a mutable reference to the focused input overlay (if any).
    pub fn focused_input_overlay_mut(&mut self) -> Option<&mut OverlayState> {
        let idx = self.focused_input_idx;
        self.input_overlays.get_mut(idx)
    }

    /// Find an overlay-backed input surface by its stable overlay id.
    pub fn overlay_input_surface_mut(
        &mut self,
        overlay_id: Uuid,
    ) -> Option<&mut crate::input_surface::InputSurface> {
        match &mut self.overlay {
            OverlayState::Prompt {
                overlay_id: id,
                surface,
                ..
            }
            | OverlayState::InputModal {
                overlay_id: id,
                surface,
                ..
            } if *id == overlay_id => return Some(surface),
            _ => {}
        }

        self.input_overlays
            .iter_mut()
            .find_map(|overlay| match overlay {
                OverlayState::Prompt {
                    overlay_id: id,
                    surface,
                    ..
                } if *id == overlay_id => Some(surface),
                _ => None,
            })
    }

    /// Apply a completed prompt compilation result to the original launching surface.
    pub fn apply_prompt_compile_result(
        &mut self,
        overlay_id: Uuid,
        outcome: Result<crate::prompt_processor::CompileResult, String>,
        original_input: Option<String>,
    ) {
        let notification = {
            let Some(surface) = self.overlay_input_surface_mut(overlay_id) else {
                return;
            };

            surface.correction_in_flight = false;

            match outcome {
                Ok(compile_result) => {
                    let mut notification = None;
                    if !compile_result.layer_validation.all_present() {
                        let missing = compile_result.layer_validation.missing().join(", ");
                        notification = Some(format!("Compiler: weak layers [{missing}]"));
                    }

                    match &compile_result.contract {
                        crate::prompt_processor::OutputContract::Complete => {
                            if let Some(ref orig) = original_input {
                                surface.corrected_compile_context =
                                    Some(crate::input_surface::CompileContext {
                                        original_input: orig.clone(),
                                        contract_status: compile_result.contract.to_status_string(),
                                        layer_semantic: compile_result.layer_validation.semantic,
                                        layer_syntactic: compile_result.layer_validation.syntactic,
                                        layer_deictic: compile_result.layer_validation.deictic,
                                        layer_discourse: compile_result.layer_validation.discourse,
                                        layer_pragmatic: compile_result.layer_validation.pragmatic,
                                    });
                            }
                            surface.corrected_preview = Some(compile_result.compiled);
                        }
                        crate::prompt_processor::OutputContract::Incomplete { criterion } => {
                            if notification.is_none() {
                                notification = Some(format!("Compiled (INCOMPLETE: {criterion})"));
                            }
                            if let Some(ref orig) = original_input {
                                surface.corrected_compile_context =
                                    Some(crate::input_surface::CompileContext {
                                        original_input: orig.clone(),
                                        contract_status: compile_result.contract.to_status_string(),
                                        layer_semantic: compile_result.layer_validation.semantic,
                                        layer_syntactic: compile_result.layer_validation.syntactic,
                                        layer_deictic: compile_result.layer_validation.deictic,
                                        layer_discourse: compile_result.layer_validation.discourse,
                                        layer_pragmatic: compile_result.layer_validation.pragmatic,
                                    });
                            }
                            surface.corrected_preview = Some(compile_result.compiled);
                        }
                        crate::prompt_processor::OutputContract::Error { kind, message } => {
                            notification = Some(format!("Compile ERROR [{kind}]: {message}"));
                        }
                        _ => {
                            notification =
                                Some("Compile result: unknown contract variant".to_string());
                        }
                    }

                    notification
                }
                Err(err) => {
                    if let Some(desc) = err.strip_prefix("Ambiguous intent: ") {
                        surface.corrected_preview = Some(format!("CLARIFY:\n- {desc}"));
                        None
                    } else {
                        Some(format!("Prompt compilation failed: {err}"))
                    }
                }
            }
        };

        if let Some(message) = notification {
            self.notify(message);
        }
    }

    /// Apply a completed AI command result to the original launching surface.
    pub fn apply_ai_command_result(
        &mut self,
        source: crate::types::AiAssistantSource,
        outcome: Result<String, String>,
    ) {
        match outcome {
            Ok(transformed) => match source {
                crate::types::AiAssistantSource::InputBar(session_id) => {
                    if let Some(state) = self.sessions.get_mut(&session_id) {
                        state.input_bar.surface.corrected_preview = Some(transformed);
                    }
                }
                crate::types::AiAssistantSource::OverlaySurface(overlay_id) => {
                    if let Some(surface) = self.overlay_input_surface_mut(overlay_id) {
                        surface.corrected_preview = Some(transformed);
                    }
                }
            },
            Err(err) => self.notify(format!("AI command failed: {err}")),
        }
    }

    /// Remove the focused input overlay and adjust the focus index.
    pub fn remove_focused_input_overlay(&mut self) -> Option<OverlayState> {
        if self.focused_input_idx >= self.input_overlays.len() {
            return None;
        }
        let removed = self.input_overlays.remove(self.focused_input_idx);
        if self.input_overlays.is_empty() {
            self.focused_input_idx = 0;
        } else {
            self.focused_input_idx = self.focused_input_idx.min(self.input_overlays.len() - 1);
        }
        Some(removed)
    }

    pub(crate) fn provider_label(provider: SessionProvider) -> &'static str {
        match provider {
            SessionProvider::Claude => "Claude",
            SessionProvider::Codex => "Codex",
            SessionProvider::Pioneer => "Pioneer",
            SessionProvider::OpenRouter => "OpenRouter",
            SessionProvider::Bedrock => "Bedrock",
            SessionProvider::Local => "Local",
            SessionProvider::Antigravity => "Antigravity",
            SessionProvider::CodexAppServer => "Codex(AS)",
            SessionProvider::Harness => "Harness",
            _ => "?",
        }
    }

    pub(crate) fn remaining_docreg_launches(
        &mut self,
        session_id: Uuid,
        commands: &[DocregCommandOccurrence],
    ) -> Vec<DocregCommandOccurrence> {
        let progress = self.docreg_launch_progress.entry(session_id).or_default();
        commands
            .iter()
            .filter(|command| !progress.accepted.contains(*command))
            .cloned()
            .collect()
    }

    pub(crate) fn record_docreg_launch_accepted(
        &mut self,
        session_id: Uuid,
        command: DocregCommandOccurrence,
    ) {
        self.docreg_launch_progress
            .entry(session_id)
            .or_default()
            .accepted
            .insert(command);
    }

    pub(crate) fn docreg_launches_complete(
        &self,
        session_id: Uuid,
        commands: &[DocregCommandOccurrence],
    ) -> bool {
        self.docreg_launch_progress
            .get(&session_id)
            .is_none_or(|progress| {
                commands.is_empty() || commands.iter().all(|c| progress.accepted.contains(c))
            })
    }

    pub(crate) fn docreg_restore_required(&self, session_id: Uuid) -> bool {
        self.docreg_launch_progress
            .get(&session_id)
            .is_some_and(|progress| progress.restore_required)
    }

    pub(crate) fn set_docreg_restore_required(&mut self, session_id: Uuid, required: bool) {
        self.docreg_launch_progress
            .entry(session_id)
            .or_default()
            .restore_required = required;
    }

    pub(crate) fn clear_docreg_launch_progress(&mut self, session_id: Uuid) {
        self.docreg_launch_progress.remove(&session_id);
    }

    pub fn is_provider_available(&self, provider: SessionProvider) -> bool {
        self.provider_availability
            .get(&provider)
            .copied()
            .unwrap_or(true)
    }

    pub(crate) fn apply_session_events(
        state: &mut crate::types::SessionState,
        mut events: Vec<rsi_common::types::ConversationEvent>,
        mode: EventApplyMode,
        workflows: &HashMap<Uuid, rsi_common::types::Workflow>,
        // P1.3: pre-resolved effective topology for `state.session.id`.
        // Caller must invoke `App::effective_topology(session_id)` before
        // taking the `&mut state` borrow that lands here, since the helper
        // needs `&self.sessions` and the mutable session borrow conflicts
        // otherwise. `None` means no Epic ancestor carried a topology
        // (the common case for ad-hoc sessions).
        effective_topology: Option<Uuid>,
    ) -> bool {
        let mut changed = false;
        match mode {
            EventApplyMode::Replace => {
                let needs_update = state.events.len() != events.len()
                    || state.events.last().map(|e| e.id) != events.last().map(|e| e.id);
                if needs_update {
                    state.events = events;
                    changed = true;
                }
            }
            EventApplyMode::Append => {
                let mut appended = false;
                let mut last_sequence = state
                    .last_sequence
                    .or_else(|| state.events.last().map(|e| e.sequence));
                for event in events.drain(..) {
                    if let Some(seq) = last_sequence {
                        if event.sequence <= seq {
                            continue;
                        }
                    }
                    last_sequence = Some(event.sequence);
                    state.events.push(event);
                    appended = true;
                }
                if appended {
                    changed = true;
                }
            }
        }

        if changed {
            state.events_generation += 1;
            state.last_sequence = state.events.last().map(|e| e.sequence);
        }

        // Trigger formulation grow animation when a new renderable event
        // arrives while the session is active (Running/Starting).
        if changed && matches!(mode, EventApplyMode::Append) {
            let is_active = matches!(
                state.session.status,
                rsi_common::types::SessionStatus::Running
                    | rsi_common::types::SessionStatus::Starting,
            );
            if is_active {
                if let Some((idx, event)) = state.events.iter().enumerate().last() {
                    let renderable = matches!(
                        event.event_type,
                        rsi_common::types::EventType::Message
                            | rsi_common::types::EventType::ToolUse
                            | rsi_common::types::EventType::ToolResult,
                    );
                    if renderable {
                        state.formulation = Some(crate::types::FormulationState {
                            event_index: idx,
                            started_at_ms: chrono::Utc::now().timestamp_millis(),
                            target_height: 0,
                        });
                    }
                }
            }
        }

        // Derive pipeline commands from workflow stage.
        // This runs unconditionally because workflow stage can change independently
        // of conversation events (e.g. daemon detects artifact after session completes).
        // P1.3: read effective topology (Epic-rooted derive-on-read) instead of
        // `state.session.workflow_id`, which is `None` on every spawned child
        // after the kill-the-copy change.
        let mut commands = conversation_docregblock_contents(&state.events);
        if let Some(wf_id) = effective_topology {
            if let Some(workflow) = workflows.get(&wf_id) {
                match workflow.stage {
                    rsi_common::types::WorkflowStage::ResearchComplete => {
                        if let Some(ref path) = workflow.artifact_path {
                            let command = format!("/plan @{}", path);
                            if !commands.contains(&command) {
                                commands.push(command);
                            }
                        }
                    }
                    rsi_common::types::WorkflowStage::PlanComplete => {
                        if let Some(ref path) = workflow.artifact_path {
                            let command = format!("/implement @{}", path);
                            if !commands.contains(&command) {
                                commands.push(command);
                            }
                        }
                    }
                    rsi_common::types::WorkflowStage::ImplementComplete => {
                        let command = "/merge_ready".to_string();
                        if !commands.contains(&command) {
                            commands.push(command);
                        }
                    }
                    _ => {}
                }
            }
        }
        if state.docregblock_contents != commands {
            state.docregblock_contents = commands;
            changed = true;
        }

        changed
    }

    // === Modal geometry helpers ===

    /// Returns the persistence key for the active overlay's geometry, if supported.
    pub fn current_overlay_geometry_key(&self) -> Option<String> {
        // Check primary overlay first
        if let OverlayState::Prompt { purpose, .. } = &self.overlay {
            return Some(Self::geometry_key_for_purpose(purpose));
        }
        if matches!(self.overlay, OverlayState::InputModal { .. }) {
            return Some("InputModal".to_string());
        }
        // Then check focused input overlay
        if let Some(OverlayState::Prompt { purpose, .. }) = self.focused_input_overlay() {
            return Some(Self::geometry_key_for_purpose(purpose));
        }
        if matches!(
            self.focused_input_overlay(),
            Some(OverlayState::InputModal { .. })
        ) {
            return Some("InputModal".to_string());
        }
        None
    }

    /// Convert a PromptPurpose to its geometry persistence key.
    pub fn geometry_key_for_purpose(purpose: &crate::types::PromptPurpose) -> String {
        match purpose {
            crate::types::PromptPurpose::Blank => "Blank".to_string(),
            crate::types::PromptPurpose::TaskRabbit => "TaskRabbit".to_string(),
            crate::types::PromptPurpose::ContinueSession(_) => "ContinueSession".to_string(),
            // Phase 4: typed-leaf prompts share a geometry slot per kind so
            // window position is stable across creations of the same kind.
            crate::types::PromptPurpose::CreateTyped { kind, .. } => {
                format!("CreateTyped:{:?}", kind)
            }
        }
    }

    /// Look up the geometry for the active overlay, returning defaults if absent.
    pub fn current_overlay_geometry(&self) -> crate::types::ModalGeometry {
        self.current_overlay_geometry_key()
            .and_then(|key| self.modal_geometries.get(&key).cloned())
            .unwrap_or_default()
    }

    /// Adjust the active overlay's geometry by the given deltas and persist.
    pub fn adjust_overlay_geometry(&mut self, ddx: i16, ddy: i16, ddw: i16, ddh: i16) {
        if let Some(key) = self.current_overlay_geometry_key() {
            let geom = self.modal_geometries.entry(key).or_default();
            geom.dx += ddx;
            geom.dy += ddy;
            geom.dw += ddw;
            geom.dh += ddh;
            crate::state::PersistedState::capture(self).save();
        }
    }

    /// Reset the active overlay's geometry to defaults.
    pub fn reset_overlay_geometry(&mut self) {
        if let Some(key) = self.current_overlay_geometry_key() {
            self.modal_geometries.remove(&key);
            crate::state::PersistedState::capture(self).save();
        }
    }
}

/// Returns the default effort level for a given model, or `None` if effort is not supported.
fn default_effort_for_model(model: Option<&str>) -> Option<String> {
    rsi_common::model_utils::default_effort_level(model?).map(|s| s.to_string())
}
