//! TUI-local types (not shared with daemon).

pub mod context_budget;
pub mod issues;
pub mod recursive_dag;
pub mod row;
pub mod session_activity;
pub mod session_display;
pub mod session_focus;
pub mod session_inspector;

pub use context_budget::{ContextBudgetViewModel, ContextDetailRow, compute_context_budget_view};
pub use issues::*;
pub use recursive_dag::{
    RECURSIVE_DAG_ARTIFACT_LIMIT, RECURSIVE_DAG_ARTIFACT_MAX_LOADED,
    RECURSIVE_DAG_ARTIFACT_PAGE_LIMIT, RECURSIVE_DAG_ARTIFACT_PREVIEW_MAX_BYTES,
    RECURSIVE_DAG_ARTIFACT_PREVIEW_MAX_LINES, RECURSIVE_DAG_ARTIFACT_PREVIEW_RENDER_LINES,
    RECURSIVE_DAG_CONTROL_REASON_LIMIT, RECURSIVE_DAG_CONTROL_REQUESTED_BY,
    RECURSIVE_DAG_EVENT_LIMIT, RECURSIVE_DAG_GRAPH_LIMIT, RECURSIVE_DAG_INSPECTOR_ROW_LIMIT,
    RECURSIVE_DAG_LIVE_LIMIT, RECURSIVE_DAG_METADATA_KEY_LIMIT, RECURSIVE_DAG_RUN_LIMIT,
    RECURSIVE_DAG_TASK_LIMIT, RECURSIVE_DAG_VALIDATION_ISSUE_LIMIT, RecursiveDagArtifactBucket,
    RecursiveDagArtifactDetailState, RecursiveDagArtifactNavDirection,
    RecursiveDagArtifactPageAction, RecursiveDagArtifactPreviewState,
    RecursiveDagArtifactSummaryPageState, RecursiveDagAsyncResult, RecursiveDagBrowserState,
    RecursiveDagControlKind, RecursiveDagControlState, RecursiveDagFakeRunState,
    RecursiveDagInspectorLoadStatus, RecursiveDagInspectorRow, RecursiveDagInspectorRowKind,
    RecursiveDagInspectorSource, RecursiveDagInspectorState, RecursiveDagInspectorView,
    RecursiveDagLiveAttemptArtifactsState, RecursiveDagLiveRunState, RecursiveDagLoadStatus,
    RecursiveDagPanel, RecursiveDagRecoveryInputField, RecursiveDagSelectedGraphData,
    RecursiveDagSessionContext, RecursiveDagStatusSummary, RecursiveDagStatusTone,
    RecursiveDagValidationDetailState, parse_recursive_dag_fake_max_steps,
    parse_recursive_dag_live_max_steps, parse_recursive_dag_recovery_budget,
    recursive_dag_preview_unavailable_message, validate_recursive_dag_cancellation_reason,
    validate_recursive_dag_requested_by,
};
pub use row::{
    ContextCell, CostCell, ModelCell, ProjectCell, SessionRowViewModel, TimeCell,
    compute_session_row,
};
pub use session_activity::{
    OPERATOR_QUEUE_CACHE_LIMIT, RECENT_CHANGES_CACHE_LIMIT, SessionActivityItem,
    SessionActivityViewModel, SessionFlowSummary, compute_session_activity,
};
pub use session_display::{
    RoleTitle, SessionDisplayIdentity, resolve_session_display_identity, split_role_title,
};
pub use session_focus::{SessionFocusEntry, SessionFocusGroup, compute_session_focus_index};
pub use session_inspector::{
    FailureEvidenceSource, InspectorDescendant, InspectorSignal, RetryFacts, SessionInspectorBody,
    SessionInspectorViewModel, SessionRuntimeFacts, WaitingRequirement, compute_session_inspector,
};

use std::collections::{BTreeSet, HashMap, HashSet};

use ratatui::layout::Rect;
use ratatui::text::Line;
use rsi_common::types::{ConversationEvent, Session};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Placeholder tag used by Phase 1 callers that don't yet have a real
/// tag set. Phase 2 (Unified Creation Modal) removes all references to
/// this constant — find them all with `grep PHASE1_PLACEHOLDER_TAG`.
pub const PHASE1_PLACEHOLDER_TAG: &str = "untagged";

/// Unique identifier for a pane within the split tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaneId(pub u64);

/// Which zone of the session list is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SessionListZone {
    TaskRabbit,
    /// Archived sessions zone.
    Archive,
    /// Sessions spawned by scheduled jobs.
    Jobs,
    #[default]
    #[serde(other)]
    Main,
}

/// TUI view modes — what a pane shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pane {
    /// Session list with selection state
    SessionList {
        selected_index: usize,
        selected_session: Option<Uuid>,
        #[serde(default)]
        scroll_offset: usize,
        #[serde(default)]
        active_zone: SessionListZone,
        #[serde(default)]
        taskrabbit_selected_index: usize,
        #[serde(default)]
        archive_selected_index: usize,
        #[serde(default)]
        jobs_selected_index: usize,
    },
    /// Session detail — viewing a specific session's output
    SessionDetail { session_id: Uuid },
    /// Settings page — full-page configuration editor.
    Settings,
    /// Prompt creator/editor — full-screen prompt management view.
    PromptCreator,
    /// Project-scoped local and external Issue workspace.
    Issues(IssueWorkspaceState),
}

/// State for the reusable model dropdown widget.
/// Owned by parent contexts (status bar, input modals, settings).
#[derive(Debug, Clone)]
pub struct ModelDropdownState {
    /// Whether the dropdown is currently visible.
    pub open: bool,
    /// Currently highlighted index in the model list.
    pub selected_index: usize,
    /// Active provider for this dropdown instance.
    pub provider: rsi_common::types::SessionProvider,
    /// Index into custom providers (None = built-in provider).
    pub custom_provider_index: Option<usize>,
    /// Cached model list for the current provider: (model_id, display_name).
    pub models: Vec<(String, String)>,
}

impl ModelDropdownState {
    /// Create a new open dropdown, pre-selecting `current_model` if found.
    pub fn new(
        provider: rsi_common::types::SessionProvider,
        models: Vec<(String, String)>,
        current_model: Option<&str>,
    ) -> Self {
        let selected_index = current_model
            .and_then(|cm| models.iter().position(|(id, _)| id == cm))
            .unwrap_or(0);
        Self {
            open: true,
            selected_index,
            provider,
            custom_provider_index: None,
            models,
        }
    }

    /// Create a closed dropdown (for use as default / initial state).
    pub fn closed(
        provider: rsi_common::types::SessionProvider,
        models: Vec<(String, String)>,
        current_model: Option<&str>,
    ) -> Self {
        let mut s = Self::new(provider, models, current_model);
        s.open = false;
        s
    }

    /// Toggle open/closed state.
    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    /// Close the dropdown.
    pub fn close(&mut self) {
        self.open = false;
    }
}

impl Default for ModelDropdownState {
    fn default() -> Self {
        Self {
            open: false,
            selected_index: 0,
            provider: rsi_common::types::SessionProvider::Claude,
            custom_provider_index: None,
            models: Vec::new(),
        }
    }
}

/// Which settings section is selected in the settings pane. The section is
/// the registry's single source of truth for the page's information
/// architecture (Epic M design D.1); `types/mod.rs` no longer owns a
/// separate `SettingsCategory` enum.
pub use crate::settings_registry::SettingsSection;

/// Settings pane navigation state.
#[derive(Debug, Clone)]
pub struct SettingsState {
    /// Active section in left panel.
    pub section: SettingsSection,
    /// Selected item index in right panel (within current section).
    pub selected_index: usize,
    /// Whether we're in the left (section) or right (settings) panel.
    pub focus: SettingsFocus,
    /// Model dropdown for model selection settings (Model Roles section).
    pub model_dropdown: ModelDropdownState,
    /// Which Model Roles item has the dropdown open (None = no dropdown active).
    pub active_dropdown_item: Option<usize>,
    /// Live incremental search query (Epic M design D.3), entered via `/`.
    pub query: String,
    /// Whether the query line is actively accepting input.
    pub query_active: bool,
}

impl Default for SettingsState {
    fn default() -> Self {
        Self {
            section: SettingsSection::ThemeColors,
            selected_index: 0,
            focus: SettingsFocus::default(),
            model_dropdown: ModelDropdownState::default(),
            active_dropdown_item: None,
            query: String::new(),
            query_active: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SettingsFocus {
    /// Left panel: category selection
    #[default]
    Categories,
    /// Right panel: individual settings
    Items,
}

/// Metadata for a saved prompt file.
#[derive(Debug, Clone)]
pub struct PromptMeta {
    pub filename: String,
    pub first_line: String,
    pub modified_at: std::time::SystemTime,
    pub size_bytes: u64,
    pub path: std::path::PathBuf,
}

/// State for the Prompt Creator/Editor view.
#[derive(Debug, Clone, Default)]
pub struct PromptCreatorState {
    pub selected_index: usize,
    pub prompts: Vec<PromptMeta>,
    pub editing: bool,
    pub selected_model: Option<String>,
    pub scroll_offset: usize,
    pub model_dropdown: Option<ModelDropdownState>,
}

/// A configurable field on session list cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CardField {
    /// 16-position context window fill bar (❯❯❯···)
    ContextBar,
    /// "$X.XX" cost display in card header
    Cost,
    /// "Nt" turn count in card header
    TurnCount,
    /// "retry N/M" retry indicator in card header
    RetryInfo,
    /// ∞ pin indicator at header left column
    PinIndicator,
    /// "↻N" rotation depth suffix appended to title
    RotationSuffix,
    /// Recency-based heat coloring on title text
    HeatColor,
    /// Description text row (visible when card is expanded)
    Description,
    /// Session kind pill badge "TR"/"BUG" (expanded only)
    KindPill,
    /// Docregblock command pill (expanded only)
    DocregblockPill,
    /// TD1 accumulated work-time display in card header
    WorkTime,
    /// Absolute created date+time in card header
    CreatedTimestamp,
}

impl CardField {
    /// All card field variants in default display order.
    pub const ALL: [CardField; 12] = [
        CardField::ContextBar,
        CardField::Cost,
        CardField::TurnCount,
        CardField::RetryInfo,
        CardField::PinIndicator,
        CardField::RotationSuffix,
        CardField::HeatColor,
        CardField::Description,
        CardField::KindPill,
        CardField::DocregblockPill,
        CardField::WorkTime,
        CardField::CreatedTimestamp,
    ];

    /// Human-readable label for the settings UI.
    pub fn label(self) -> &'static str {
        match self {
            CardField::ContextBar => "Context bar",
            CardField::Cost => "Cost",
            CardField::TurnCount => "Turn count",
            CardField::RetryInfo => "Retry info",
            CardField::PinIndicator => "Pin indicator",
            CardField::RotationSuffix => "Rotation depth",
            CardField::HeatColor => "Heat color",
            CardField::Description => "Description",
            CardField::KindPill => "Kind pill (TR/BUG)",
            CardField::DocregblockPill => "Docregblock pill",
            CardField::WorkTime => "Accumulated work time",
            CardField::CreatedTimestamp => "Created date",
        }
    }
}

/// Width policy for the session navigator's optional columns. Required columns
/// are deliberately not represented here and therefore cannot be disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum NavigatorPreset {
    #[default]
    Dense,
    Operations,
    Cost,
}

impl NavigatorPreset {
    pub const fn next(self) -> Self {
        match self {
            Self::Dense => Self::Operations,
            Self::Operations => Self::Cost,
            Self::Cost => Self::Dense,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Dense => "Dense",
            Self::Operations => "Operations",
            Self::Cost => "Cost",
        }
    }
}

/// User-configurable navigator columns. Required navigator columns are
/// deliberately absent, so persisted state can never hide them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NavigatorOptionalColumn {
    Age,
    ModelEffort,
    Retry,
    Cost,
    Work,
    Rotation,
    Project,
    Created,
}

impl NavigatorOptionalColumn {
    pub const ALL: [Self; 8] = [
        Self::Age,
        Self::ModelEffort,
        Self::Retry,
        Self::Cost,
        Self::Work,
        Self::Rotation,
        Self::Project,
        Self::Created,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Age => "Navigator age",
            Self::ModelEffort => "Navigator model / effort",
            Self::Retry => "Navigator retry",
            Self::Cost => "Navigator cost",
            Self::Work => "Navigator work",
            Self::Rotation => "Navigator rotation",
            Self::Project => "Navigator project",
            Self::Created => "Navigator created",
        }
    }
}

/// Direction of a split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitDirection {
    Horizontal, // top/bottom
    Vertical,   // left/right
}

/// Binary tree node for split layouts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SplitNode {
    /// Leaf node containing a single pane.
    Leaf { pane: Pane, id: PaneId },
    /// Internal node splitting into two children.
    Split {
        direction: SplitDirection,
        first: Box<SplitNode>,
        second: Box<SplitNode>,
        id: PaneId,
    },
}

impl SplitNode {
    /// Get the PaneId of this node.
    pub fn id(&self) -> PaneId {
        match self {
            SplitNode::Leaf { id, .. } => *id,
            SplitNode::Split { id, .. } => *id,
        }
    }

    /// Find a leaf node by PaneId and return a reference to its Pane.
    pub fn find_pane(&self, target: PaneId) -> Option<&Pane> {
        match self {
            SplitNode::Leaf { pane, id } if *id == target => Some(pane),
            SplitNode::Split { first, second, .. } => {
                first.find_pane(target).or_else(|| second.find_pane(target))
            }
            _ => None,
        }
    }

    /// Find a leaf node by PaneId and return a mutable reference to its Pane.
    pub fn find_pane_mut(&mut self, target: PaneId) -> Option<&mut Pane> {
        match self {
            SplitNode::Leaf { pane, id } if *id == target => Some(pane),
            SplitNode::Split { first, second, .. } => first
                .find_pane_mut(target)
                .or_else(|| second.find_pane_mut(target)),
            _ => None,
        }
    }

    /// Collect all leaf PaneIds in order (left-to-right, top-to-bottom).
    pub fn leaf_ids(&self) -> Vec<PaneId> {
        match self {
            SplitNode::Leaf { id, .. } => vec![*id],
            SplitNode::Split { first, second, .. } => {
                let mut ids = first.leaf_ids();
                ids.extend(second.leaf_ids());
                ids
            }
        }
    }

    /// Remove a leaf by PaneId. Returns the sibling if this was a split,
    /// or None if this was the root leaf.
    pub fn remove_pane(self, target: PaneId) -> Option<SplitNode> {
        match self {
            SplitNode::Leaf { id, .. } if id == target => None,
            leaf @ SplitNode::Leaf { .. } => Some(leaf),
            SplitNode::Split {
                direction,
                first,
                second,
                id,
            } => {
                if first.id() == target || first.find_pane(target).is_some() {
                    // Target is in the first subtree
                    match first.remove_pane(target) {
                        None => Some(*second), // first was the leaf, promote second
                        Some(new_first) => Some(SplitNode::Split {
                            direction,
                            first: Box::new(new_first),
                            second,
                            id,
                        }),
                    }
                } else {
                    match second.remove_pane(target) {
                        None => Some(*first), // second was the leaf, promote first
                        Some(new_second) => Some(SplitNode::Split {
                            direction,
                            first,
                            second: Box::new(new_second),
                            id,
                        }),
                    }
                }
            }
        }
    }
}

/// Sentinel PaneId for the tab-stored session list state (not in the layout tree).
/// Used by `interaction_pane_id()` when no SessionList pane exists in the tree.
pub const DETAIL_LIST_SENTINEL: PaneId = PaneId(u64::MAX);

/// Legacy persisted bottom-surface focus.
///
/// The always-visible operations deck was retired. The enum remains so older
/// serialized tabs deserialize cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum BottomZone {
    #[default]
    #[serde(alias = "Dag", alias = "Vitals", alias = "Queue")]
    Off,
}

/// A tab containing a layout, focus state, and optional project identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tab {
    pub name: String,
    pub layout: SplitNode,
    pub focused_pane: PaneId,
    /// Project this workspace is bound to. None only during migration from old state.
    #[serde(default)]
    pub project_id: Option<Uuid>,
    /// Persistent session list state for this tab. Used when no SessionList pane
    /// exists in the layout tree (single-pane detail view). Saved when entering a
    /// session from a SessionList pane, and used by `interaction_pane_id()` to
    /// proxy interactions to the detail list clone.
    #[serde(default = "Tab::default_session_list_state")]
    pub session_list_state: Pane,
    /// Offset from default bottom inset for session detail / session list split.
    /// Positive = more rows for bottom container (session list), negative = fewer.
    /// Applied as: effective_inset = (SESSION_DETAIL_BOTTOM_INSET as i16 + offset).clamp(MIN, MAX)
    #[serde(default)]
    pub detail_split_offset: i16,
    /// Offset from default horizontal centering offset (LAYOUT_X_OFFSET).
    /// Positive = shift content right (wider sidebar), negative = shift left (narrower sidebar).
    #[serde(default)]
    pub layout_x_offset_adj: i16,
    /// Session list sidebar width as percentage of viewport width.
    /// 0 = use default layout_x_offset system. When non-zero, overrides the left
    /// sidebar width to be this percentage of the pane width, and applies a 5%
    /// right shift to the detail view content.
    #[serde(default)]
    pub session_list_width_pct: u16,
    /// Hierarchy descent path for this tab.
    /// When non-empty, the session list shows children of the last UUID in the path.
    /// When empty, shows root sessions (parent_id == None).
    #[serde(default)]
    pub descent_path: Vec<Uuid>,
    /// Legacy persisted bottom-surface focus. New interaction keeps this off.
    #[serde(default)]
    pub bottom_focus_target: BottomZone,
    /// Cursor position within the mini-DAG zone (when `bottom_focus_target == Dag`).
    /// `None` defaults to the focused session itself on the next render.
    #[serde(default)]
    pub mini_dag_focus: Option<Uuid>,
}

impl Tab {
    /// Default session list state for serde deserialization and tab construction.
    pub fn default_session_list_state() -> Pane {
        Pane::SessionList {
            selected_index: 0,
            selected_session: None,
            scroll_offset: 0,
            active_zone: SessionListZone::default(),
            taskrabbit_selected_index: 0,
            archive_selected_index: 0,
            jobs_selected_index: 0,
        }
    }

    /// Find a pane by ID, checking both the layout tree and the tab-stored
    /// session list state (for the sentinel ID).
    pub fn find_pane(&self, id: PaneId) -> Option<&Pane> {
        if id == DETAIL_LIST_SENTINEL {
            Some(&self.session_list_state)
        } else {
            self.layout.find_pane(id)
        }
    }

    /// Find a pane by ID mutably, checking both the layout tree and the
    /// tab-stored session list state (for the sentinel ID).
    pub fn find_pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        if id == DETAIL_LIST_SENTINEL {
            Some(&mut self.session_list_state)
        } else {
            self.layout.find_pane_mut(id)
        }
    }
}

/// Input mode for the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// Normal mode — keybindings active
    Normal,
    /// Input mode — typing a query for a new session
    Input,
    /// Command mode — typing a : command
    Command,
    /// Search mode — typing a / search query
    Search,
}

/// What a `/` search is targeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchTarget {
    /// Filter session list by query text
    SessionList,
    /// Search event content in session detail
    SessionDetail,
}

impl Default for SearchTarget {
    fn default() -> Self {
        Self::SessionList
    }
}

/// Why Input mode was entered — determines what happens on Enter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputPurpose {
    /// Typing a query for a new session (from `n` or `:new`)
    NewSession,
    /// Typing a follow-up query for an existing session (from `:continue`)
    ContinueSession(Uuid),
}

/// Cache key for rendered conversation events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RenderCacheKey {
    pub sequence: i32,
    pub width: u16,
    pub is_collapsed: bool,
    pub is_expanded: bool,
    pub is_cursor: bool,
    pub is_last_event: bool,
    pub model_label_hash: u64,
}

#[derive(Debug, Clone)]
pub struct CodeBlockRange {
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct FileLinkTarget {
    pub line: usize,
    pub target: String,
}

/// A web link (markdown `[text](url)` with a non-file target, or a bare
/// `http(s)://` URL) found on a given logical (pre-wrap) line of a rendered
/// event. Mirrors `FileLinkTarget`'s shape; kept as a distinct type so
/// click-time lookup can tell "open in file viewer" apart from "open in
/// browser" without inspecting the string itself.
#[derive(Debug, Clone)]
pub struct WebLinkTarget {
    pub line: usize,
    pub target: String,
}

/// Cached render output for an event at a specific width/state.
#[derive(Debug, Clone)]
pub struct CachedRenderedEvent {
    pub lines: Vec<Line<'static>>,
    pub height: usize,
    pub generation: u64,
    pub code_blocks: Vec<CodeBlockRange>,
    pub file_links: Vec<FileLinkTarget>,
    pub web_links: Vec<WebLinkTarget>,
}

/// State for the "message formulation" grow animation.
/// Active while a new renderable event is growing from the loading container's
/// 3-row footprint to its final height (~200 ms).
#[derive(Debug, Clone, Copy)]
pub struct FormulationState {
    /// Index into `SessionState.events` of the event being formulated.
    pub event_index: usize,
    /// Wall-clock ms when the animation started (chrono::Utc::now().timestamp_millis()).
    pub started_at_ms: i64,
    /// Final rendered height the bubble should grow to. 0 = sentinel (resolved on first render frame).
    pub target_height: u16,
}

/// Per-session state tracked in the TUI (shared across all panes that view the same session).
#[derive(Debug, Clone)]
pub struct SessionState {
    pub session: Session,
    pub events: Vec<ConversationEvent>,
    /// Cached model segments for rendering switch dividers.
    pub model_segments: Vec<rsi_common::types::ModelSegment>,
    /// Scroll offset in post-wrap terminal lines (0 = top of content).
    /// Computed by the height cache in post-wrap space and consumed by Paragraph::scroll().
    pub scroll_offset: usize,
    /// Cached viewport height (content area height minus borders). Updated each render.
    pub last_viewport_height: usize,
    /// Sequence numbers of events that are collapsed (tool use/result pairs).
    pub collapsed_events: HashSet<i32>,
    /// Sequence numbers of events whose content is fully expanded (bypass truncation).
    pub expanded_events: HashSet<i32>,
    /// Cached per-event rendered heights (in terminal lines).
    /// Updated by the renderer each frame. Index corresponds to `events` index.
    pub event_heights: Vec<usize>,
    /// Cached cumulative line offsets (where each event starts).
    /// `event_offsets[i]` = sum of `event_heights[0..i]`.
    pub event_offsets: Vec<usize>,
    /// Total content height in post-wrap terminal lines (events + spinner).
    /// Set by `update_event_heights()`. Includes non-event extras like the loading spinner.
    pub total_content_height: usize,
    /// Terminal width at last height computation (for invalidation on resize).
    pub last_render_width: u16,
    /// Incremented when events are modified (new event, content update, replacement).
    /// Used by `update_event_heights()` to detect stale caches.
    pub events_generation: u64,
    /// D2 (new-message signal): `events_generation` value at the last moment
    /// this session's detail view was known to be seen (stamped on entry, and
    /// continuously while it remains the focused detail pane — see
    /// `app/navigation.rs`, `app/layout.rs`, `app/polling.rs` call sites). Compared
    /// against `events_generation` to derive
    /// `SessionRowViewModel.is_new_message`. Runtime-only — `SessionState` is
    /// never serialized.
    pub last_seen_events_generation: u64,
    /// Generation at last height computation.
    pub last_height_generation: u64,
    /// Whether system events are visible in the detail view. Default: false (hidden).
    pub show_system_events: bool,
    /// Whether thinking events are expanded in the detail view. Default: false (collapsed bubble).
    pub show_thinking_events: bool,
    /// Whether tool result events are visible in the detail view. Default: true (shown).
    pub show_tool_results: bool,
    /// Sequence numbers of tool-group leaders whose groups have been expanded
    /// to show individual collapsed items. When a group-start sequence is in
    /// this set, consecutive collapsed tool events are shown individually
    /// instead of being absorbed into a single "N tool calls" summary line.
    pub expanded_tool_groups: HashSet<i32>,
    /// Cached `ToolUse`/`ToolResult` pairing projection over `events`.
    ///
    /// Structural only — it depends on the event stream, never on fold state,
    /// so it survives every collapse/expand toggle and is refreshed (extended
    /// in place on append, rebuilt only when the projected prefix changed) by
    /// `ui::height::refresh_tool_projection`.
    pub tool_projection: crate::ui::tool_projection::Projection,
    /// `events_generation` at the last projection refresh.
    pub tool_projection_generation: u64,
    /// Fingerprint of the last event covered by `tool_projection`
    /// (`(sequence, event_type, tool_use_id)`). If it still matches, growth of
    /// `events` is a pure append and the projection can be extended instead of
    /// rebuilt.
    pub tool_projection_tail: Option<(i32, rsi_common::types::EventType, Option<String>)>,
    /// Whether the view auto-scrolls to the newest content. Default: true.
    /// Set to false when the user manually scrolls up; re-enabled when they hit bottom.
    pub follow_tail: bool,
    /// One-frame hold-off that prevents `follow_tail` from being re-engaged
    /// by the pre-render scroll-lock check. Set by explicit jump commands
    /// (PrevEvent/NextEvent) whose target scroll offset may land near
    /// `max_scroll`, which would otherwise immediately re-lock the view.
    pub follow_tail_hold: bool,
    /// Persistent input bar state for this session.
    pub input_bar: InputBarState,
    /// Whether to center content on wide terminals. Default: false.
    pub center_content: bool,
    /// Index into `events` of the "current" event under the cursor.
    /// Derived from `scroll_offset` each frame; used by fold commands and visual indicator.
    pub current_event_index: Option<usize>,
    /// Content area rect from the last render (inside borders, excluding input bar).
    /// Used for mouse click → event index mapping.
    pub last_content_area: ratatui::layout::Rect,
    /// Vertical offset from `last_content_area.y` to the first line of actual message
    /// content. Accounts for border, padding, context bar, active task, pending archive,
    /// and issue URL header rows that are rendered above the conversation events.
    /// Computed each frame in `ui/mod.rs::render_pane` to keep click detection in sync
    /// with the actual rendering layout.
    pub last_content_y_offset: u16,
    /// Raw content strings extracted from `<docregblock>` tags across all events.
    /// Populated during event updates; consumed by the `Space+x` keybinding.
    pub docregblock_contents: Vec<String>,
    /// Cached rendered lines for events, keyed by width/fold/cursor state.
    pub render_cache: HashMap<RenderCacheKey, CachedRenderedEvent>,
    /// Widest Markdown-table-driven detail width for `events_generation`.
    /// Kept separately from rendered-event entries because it determines the
    /// width needed to select those entries in the first place.
    pub table_detail_width_cache: Option<(u64, u16)>,
    /// Highest sequence number seen from daemon (for incremental fetch).
    /// None means no events fetched yet — next poll should do a full fetch.
    pub last_sequence: Option<i32>,
    /// When true, clear the entire session detail pane before the next render.
    /// Used for large scroll jumps (gg/G) to avoid stale glyphs.
    pub clear_next_render: bool,
    /// Whether the daemon has flagged this session as stalled (idle beyond threshold).
    /// Set by `session_stalled` push event, cleared when a `conversation_event` arrives.
    pub is_stalled: bool,
    /// Active "message formulation" animation: a new event arrived while the
    /// loading container was visible and we are growing the new bubble from the
    /// container's 3-row footprint to its final height. None when no animation
    /// is in progress.
    pub formulation: Option<FormulationState>,
    /// Active file viewer/editor for this session. When `Some`, renders the file viewer
    /// instead of the conversation content in the session detail pane.
    pub file_viewer: Option<FileViewerState>,
    /// Cached file viewers keyed by path. Preserves undo/redo history across close → reopen.
    pub file_viewer_cache: HashMap<std::path::PathBuf, FileViewerState>,
    /// Whether this session's card is expanded in the session list (shows description, pills).
    /// Default: false (collapsed). Collapsed cards show only header + title.
    pub list_card_expanded: bool,
    /// Cursor index at last height computation — used to invalidate heights when cursor moves,
    /// since the selected event renders with a block border (affecting height by +2).
    pub last_cursor_index: Option<usize>,
    /// Authoritative context-fill percentage emitted by the daemon for this session.
    /// Populated by `app/polling.rs` from `DaemonEvent::ContextUsageUpdated.pct` (the
    /// daemon's provider-aware `live_context_state` computation). `None` before
    /// the first bus event arrives
    /// (cold-start). Consumers MUST prefer this over re-deriving from
    /// `session.total_input_tokens` / `context_window` — the daemon's numerator
    /// selection is the single source of truth.
    pub live_context_pct: Option<f64>,
    /// Memoized recent files (top 5 distinct, newest-first) for the queue zone,
    /// keyed on `events_generation`. Phase 3 populates / consumes this.
    pub recent_files_cache: Option<(u64, Vec<std::path::PathBuf>)>,
    /// Per-session ring buffer of (timestamp, cost_delta) for the cost-burn
    /// sparkline. Capped at 16 entries by the polling loop in Phase 5.
    pub cost_buckets: std::collections::VecDeque<(chrono::DateTime<chrono::Utc>, f64)>,
    /// Cached per-turn metrics for the focused session (drives the cache-hit
    /// sparkline in Phase 5). Fetched lazily via `client.get_turn_metrics`.
    pub turn_metrics: Vec<rsi_common::types::TurnMetric>,
    /// Tracks the highest `Session.num_turns` value we have already fetched
    /// metrics for, so polling can throttle re-fetches in Phase 5.
    pub last_metrics_num_turns: Option<u32>,
    /// When this session was last navigated to (for poll suppression during navigation).
    /// Used to avoid redundant background polling immediately after user navigation.
    pub last_navigation_time: Option<std::time::Instant>,
}

/// Cap on the number of recent-files surfaced in the queue zone.
/// Matches `gf1..gf5` in keybindings (gf6..gf9 are reserved but unbound).
pub const RECENT_FILES_CAP: usize = 5;

/// Pure data computation — walks `events` newest-first and collects the
/// distinct `tool_input.file_path` values from `Edit`/`MultiEdit`/`Write`/
/// `Read` `ToolUse` events. Returns up to `RECENT_FILES_CAP` paths,
/// newest-first. Caller memoizes against `events_generation` via
/// `SessionState::cached_recent_files`.
pub fn compute_recent_files(
    events: &[rsi_common::types::ConversationEvent],
) -> Vec<std::path::PathBuf> {
    use rsi_common::types::EventType;
    use std::collections::HashSet;
    let mut seen: HashSet<std::path::PathBuf> = HashSet::new();
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    for ev in events.iter().rev() {
        if !matches!(ev.event_type, EventType::ToolUse) {
            continue;
        }
        let tn = ev.tool_name.as_deref().unwrap_or("");
        if !matches!(tn, "Edit" | "MultiEdit" | "Write" | "Read") {
            continue;
        }
        let Some(input) = ev.tool_input.as_ref() else {
            continue;
        };
        let Some(fp) = input.get("file_path").and_then(|v| v.as_str()) else {
            continue;
        };
        let path = std::path::PathBuf::from(fp);
        if seen.insert(path.clone()) {
            out.push(path);
            if out.len() >= RECENT_FILES_CAP {
                break;
            }
        }
    }
    out
}

impl SessionState {
    pub fn new(session: Session) -> Self {
        // Display-only: the daemon is the single producer of the context-fill
        // percentage and stamps `context_fill_pct` on every `Session` it returns
        // (live state for active sessions, persisted fields for idle ones). The
        // TUI seeds the live counter from that daemon value and never recomputes
        // pct itself; the `ContextUsageUpdated` bus event refreshes it live.
        let live_context_pct = session.context_fill_pct;
        Self {
            session,
            events: Vec::new(),
            model_segments: Vec::new(),
            scroll_offset: 0,
            last_viewport_height: 0,
            collapsed_events: HashSet::new(),
            expanded_events: HashSet::new(),
            event_heights: Vec::new(),
            event_offsets: Vec::new(),
            total_content_height: 0,
            last_render_width: 0,
            events_generation: 0,
            last_seen_events_generation: 0,
            last_height_generation: 0,
            show_system_events: false,
            show_thinking_events: false,
            show_tool_results: true,
            expanded_tool_groups: HashSet::new(),
            tool_projection: crate::ui::tool_projection::Projection::default(),
            tool_projection_generation: 0,
            tool_projection_tail: None,
            follow_tail: true,
            follow_tail_hold: false,
            input_bar: InputBarState::default(),
            center_content: true,
            current_event_index: None,
            last_content_area: ratatui::layout::Rect::default(),
            last_content_y_offset: 0,
            docregblock_contents: Vec::new(),
            render_cache: HashMap::new(),
            table_detail_width_cache: None,
            last_sequence: None,
            clear_next_render: false,
            is_stalled: false,
            formulation: None,
            file_viewer: None,
            file_viewer_cache: HashMap::new(),
            list_card_expanded: false,
            last_cursor_index: None,
            live_context_pct,
            recent_files_cache: None,
            cost_buckets: std::collections::VecDeque::new(),
            turn_metrics: Vec::new(),
            last_metrics_num_turns: None,
            last_navigation_time: None,
        }
    }

    /// Returns the cached recent-files list (up to `RECENT_FILES_CAP`),
    /// recomputing if `events_generation` has advanced past the cached
    /// generation. Mutates the cache in place. Returns an owned `Vec`
    /// because callers (`queue.rs` rendering, `open_recent_file_n` handler)
    /// need to drop the `&mut self` borrow before continuing.
    pub fn cached_recent_files(&mut self) -> Vec<std::path::PathBuf> {
        let current_gen = self.events_generation;
        if let Some((cached_gen, files)) = &self.recent_files_cache {
            if *cached_gen == current_gen {
                return files.clone();
            }
        }
        let files = compute_recent_files(&self.events);
        self.recent_files_cache = Some((current_gen, files.clone()));
        files
    }
}

/// Detected indentation style for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndentStyle {
    /// True if the file uses hard tabs for indentation.
    pub use_tabs: bool,
    /// Indent width in spaces (or tab-stop width if use_tabs). Default 4.
    pub width: u8,
}

impl Default for IndentStyle {
    fn default() -> Self {
        Self {
            use_tabs: false,
            width: 4,
        }
    }
}

/// Detect indentation style from file lines.
/// Scans the first `scan_limit` non-empty lines.
pub fn detect_indent_style(lines: &[String], scan_limit: usize) -> IndentStyle {
    let mut tab_lines = 0usize;
    let mut space_lines = 0usize;
    let mut indent_deltas: Vec<usize> = Vec::new();
    let mut prev_indent = 0usize;

    for line in lines.iter().take(scan_limit) {
        if line.is_empty() {
            continue;
        }
        // Count leading whitespace type
        let first_char = line.chars().next().unwrap();
        if first_char == '\t' {
            tab_lines += 1;
        } else if first_char == ' ' {
            // Count leading spaces
            let indent = line.len() - line.trim_start_matches(' ').len();
            if indent > 0 {
                space_lines += 1;
                // Track indent deltas to find common indent width
                if indent > prev_indent {
                    indent_deltas.push(indent - prev_indent);
                }
                prev_indent = indent;
            } else {
                prev_indent = 0;
            }
        } else {
            prev_indent = 0;
        }
    }

    // Decide tabs vs spaces
    if tab_lines > space_lines {
        return IndentStyle {
            use_tabs: true,
            width: 4,
        };
    }

    if space_lines == 0 {
        // No indented lines found -- fall back to default
        return IndentStyle::default();
    }

    // Find most common indent delta among {2, 4, 8}
    let counts_2 = indent_deltas.iter().filter(|&&d| d == 2).count();
    let counts_4 = indent_deltas.iter().filter(|&&d| d == 4).count();
    let counts_8 = indent_deltas.iter().filter(|&&d| d == 8).count();

    let width = if counts_2 >= counts_4 && counts_2 >= counts_8 {
        2
    } else if counts_8 > counts_4 {
        8
    } else {
        4
    };

    IndentStyle {
        use_tabs: false,
        width,
    }
}

/// Search state for the file viewer. Lives on FileViewerState.
#[derive(Debug, Clone, Default)]
pub struct FileSearchState {
    /// Current active search pattern (literal string, not regex).
    pub pattern: Option<String>,
    /// All match positions as (line_index, col_start, col_end).
    /// col_end is exclusive (byte offset into the UTF-8 line string).
    pub matches: Vec<(usize, usize, usize)>,
    /// Index of the "current" match (highlighted differently, cursor is here).
    pub current_match: usize,
    /// true = pattern came from `/` (forward), false = came from `?` (backward).
    pub forward: bool,
    /// When true: the `/` or `?` prompt is open, collecting input.
    pub input_active: bool,
    /// Characters typed since search input opened.
    pub input_buffer: String,
}

/// Fold state for the file viewer. Lives on FileViewerState.
#[derive(Debug, Clone, Default)]
pub struct FoldState {
    /// Currently collapsed regions: BTreeSet of (start_line, end_line).
    /// `start_line` is the first line of the foldable block (kept visible).
    /// Lines [start_line+1 ..= end_line] are hidden when collapsed.
    pub folded_ranges: BTreeSet<(usize, usize)>,
    /// All foldable regions discovered from the tree-sitter parse tree.
    /// Cached; recomputed only when content changes (same trigger as ts_tree update).
    /// Sorted by start_line ascending.
    pub foldable_regions: Vec<(usize, usize)>,
}

/// State for the vim-style `:` command line in the file viewer.
#[derive(Debug, Clone, Default)]
pub struct FileCommandState {
    /// Whether the command prompt is currently active.
    pub active: bool,
    /// Characters typed so far (not including the leading `:`).
    pub buffer: String,
    /// Byte offset of the cursor in `buffer`.
    pub cursor: usize,
}

impl FileCommandState {
    /// Insert a character at the cursor position.
    pub fn insert_char(&mut self, c: char) {
        self.buffer.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Delete the character before the cursor (Backspace).
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // Find previous char boundary
        let mut prev = self.cursor - 1;
        while prev > 0 && !self.buffer.is_char_boundary(prev) {
            prev -= 1;
        }
        self.buffer.remove(prev);
        self.cursor = prev;
    }

    /// Reset to inactive state.
    pub fn cancel(&mut self) {
        self.active = false;
        self.buffer.clear();
        self.cursor = 0;
    }
}

/// Per-line git change state for the file viewer gutter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitLineState {
    /// Line exists in HEAD unchanged.
    Unchanged,
    /// Line was added (not in HEAD).
    Added,
    /// Line was modified (content differs from HEAD version).
    Modified,
    /// Deletion marker: lines were removed just before this line in HEAD.
    Deleted,
}

/// Git diff state for the file viewer.
#[derive(Debug, Clone, Default)]
pub struct GitGutterState {
    /// One entry per line in the current buffer (0-indexed).
    /// Empty means git info not yet loaded or file is not in a git repo.
    pub line_states: Vec<GitLineState>,
    /// Whether git info has been computed at least once for this file.
    pub loaded: bool,
}

/// A source range displayed on one visual row of the file editor.
///
/// This is recorded by the renderer so mouse input can map terminal
/// coordinates back to the underlying logical cursor position.
#[derive(Debug, Clone, Copy)]
pub struct FileViewerMouseRow {
    pub line: usize,
    pub char_start: usize,
    pub char_end: usize,
}

/// The most recently rendered, mouse-interactive portion of a file viewer.
#[derive(Debug, Clone, Default)]
pub struct FileViewerMouseLayout {
    /// Includes the line-number gutter and excludes the search/command prompt.
    pub editor_area: Option<Rect>,
    /// Source text area, excluding the gutter.
    pub content_area: Option<Rect>,
    /// One entry for each displayed visual row, in screen-row order.
    pub rows: Vec<FileViewerMouseRow>,
}

/// State for the per-session file viewer/editor.
#[derive(Debug, Clone)]
pub struct FileViewerState {
    /// The vim-modal editing surface holding file content.
    pub surface: crate::input_surface::InputSurface,
    /// Absolute path to the file being viewed/edited.
    pub file_path: std::path::PathBuf,
    /// Whether the content has been modified since last save.
    pub dirty: bool,
    /// Content as last read from disk or written to disk. Used to detect
    /// external modifications by comparing against fresh disk reads.
    pub disk_content: String,
    /// When set, the file was externally modified after this viewer was
    /// loaded or last saved. Contains the conflicting disk content. The
    /// user's draft remains in `surface.textarea`. Cleared by revert,
    /// force-save, or a successful save after the conflict is resolved.
    pub external_conflict: Option<String>,
    /// Whether Space was pressed in normal mode (waiting for second key in Space+q sequence).
    pub pending_leader: bool,

    // --- Tree-sitter rendering fields ---
    /// File extension (lowercased, no dot) for language detection. None = unknown.
    pub language_ext: Option<String>,
    /// Source view: index of the first rendered visual row in the viewport.
    /// Markdown preview: index of the first rendered preview line.
    pub viewport_top: usize,
    /// Last rendered content height in rows. Used for page-style scrolling.
    pub viewport_height: usize,
    /// Cached highlighted lines. Invalidated when content_version changes.
    /// Rebuilt lazily in the renderer on first access after invalidation.
    pub highlight_cache: Option<Vec<ratatui::text::Line<'static>>>,
    /// Monotonically increasing version counter. Incremented on every buffer mutation.
    /// Cache is valid iff cached_content_version == content_version.
    pub content_version: u64,
    /// Version at which the highlight cache was built. Compared to content_version.
    pub cached_content_version: u64,

    // --- Vim navigation enhancement fields ---
    /// Detected indentation style (tabs vs spaces, indent width).
    pub indent_style: IndentStyle,

    // --- Search & folding fields (Plan 3) ---
    /// Search state (/, ?, n, N, *, #).
    pub search: FileSearchState,
    /// Code folding state (za, zo, zc, zM, zR).
    pub folds: FoldState,
    /// Whether `z` was pressed in normal mode (waiting for second key in z-sequence).
    pub pending_z: bool,

    // --- Plan 4 fields ---
    /// Command-line mode state (`:` commands).
    pub command: FileCommandState,
    /// Git gutter change indicators.
    pub git_gutter: GitGutterState,
    /// Whether bracket auto-pairing is enabled in insert mode.
    pub auto_pair: bool,
    /// Whether to show rendered markdown preview (only relevant for .md files).
    pub markdown_preview: bool,
    /// Cached rendered markdown lines. `None` means stale (re-render on next frame).
    pub markdown_cache: Option<Vec<ratatui::text::Line<'static>>>,
    /// Render-time geometry used to route mouse clicks and wheel events.
    pub mouse_layout: FileViewerMouseLayout,
}

impl FileViewerState {
    pub fn new(file_path: std::path::PathBuf, content: String) -> Self {
        let language_ext = file_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase());

        let lines: Vec<String> = content.lines().map(str::to_string).collect();
        // If file is empty, start with one empty line
        let lines = if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        };
        let indent_style = detect_indent_style(&lines, 100);
        let mut surface = crate::input_surface::InputSurface::new_insert_with_content(lines);
        // File viewer starts in normal mode (vim convention for viewing files)
        surface.mode = PopupMode::Normal;

        let file_path_ref = &file_path;
        let line_count = surface.textarea.lines().len();
        let git_gutter = GitGutterState {
            line_states: crate::git_gutter::compute_git_gutter(file_path_ref, line_count),
            loaded: true,
        };

        Self {
            surface,
            file_path,
            dirty: false,
            disk_content: content,
            external_conflict: None,
            pending_leader: false,
            language_ext,
            viewport_top: 0,
            viewport_height: 12,
            highlight_cache: None,
            content_version: 0,
            cached_content_version: u64::MAX, // force initial build
            indent_style,
            search: FileSearchState::default(),
            folds: FoldState::default(),
            pending_z: false,
            command: FileCommandState::default(),
            git_gutter,
            auto_pair: true,
            markdown_preview: false,
            markdown_cache: None,
            mouse_layout: FileViewerMouseLayout::default(),
        }
    }

    /// Call after any buffer mutation to invalidate the highlight cache.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
        self.content_version = self.content_version.wrapping_add(1);
        self.markdown_cache = None; // invalidate markdown preview cache on any edit
    }

    /// Whether the file is a markdown file (by extension).
    pub fn is_markdown(&self) -> bool {
        matches!(
            self.file_path.extension().and_then(|e| e.to_str()),
            Some("md" | "markdown" | "mdx")
        )
    }
}

/// Modal editing mode inside a popup overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PopupMode {
    /// Typing text — keys go to tui-textarea.
    Insert,
    /// Vim-like normal mode — keys navigate/submit/cancel.
    Normal,
}

/// Selection state for a question in the overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionSelection {
    Single(Option<usize>),
    Multi(Vec<usize>),
}

/// State for the persistent input bar in session detail view.
///
/// Thin wrapper around [`InputSurface`](crate::input_surface::InputSurface).
/// All editing state lives in `surface`; callers access fields via `surface.*`.
#[derive(Debug, Clone)]
pub struct InputBarState {
    pub surface: crate::input_surface::InputSurface,
}

impl Default for InputBarState {
    fn default() -> Self {
        Self {
            surface: crate::input_surface::InputSurface::default(),
        }
    }
}

/// Delta offsets from the dynamically computed base popup rect.
/// Terminal resize recomputes the base; deltas apply on top.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModalGeometry {
    /// Horizontal offset from computed center.
    pub dx: i16,
    /// Vertical offset from computed top.
    pub dy: i16,
    /// Width delta (positive = wider).
    pub dw: i16,
    /// Height delta (positive = taller).
    pub dh: i16,
}

/// The purpose of a prompt popup — determines title and submit behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptPurpose {
    /// Continuing an existing session with a follow-up query.
    ContinueSession(uuid::Uuid),
    /// Launching a one-shot TaskRabbit task.
    TaskRabbit,
    /// General-purpose blank session — empty prompt, no pre-filled commands.
    Blank,
    /// Launching a typed leaf session (Story / Task / Bug …) under an optional
    /// hierarchical parent. Phase 4 of the hierarchical-session-organization plan.
    CreateTyped {
        kind: rsi_common::types::SessionKind,
        parent_id: Option<uuid::Uuid>,
    },
}

/// One tag chip in the unified entity-creation modal's tag row.
/// `Pending` is the live input buffer; `Committed` is post-`normalize_tag`;
/// `Invalid` carries the normalization error for inline display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagChip {
    pub value: String,
    pub status: ChipStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChipStatus {
    /// Live input buffer — last chip in `tags` when typing.
    Pending,
    /// Committed via Space/Tab/,/Enter; immutable until Backspace deletes.
    Committed,
    /// Failed `normalize_tag`; carries the error message inline.
    Invalid(String),
}

/// Field currently focused in the `CreateEntityForm` overlay.
/// Tab cycles through the kind-specific visibility list returned by
/// `overlay::create_entity_form::visibility::field_visibility(kind)`;
/// S-Tab reverses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateEntityField {
    Kind,
    Name,
    /// Multi-line prompt body (leaf kinds only; S1 session-modal-unify).
    Body,
    Tag,
    Parent,
    Topology,
    Provider,
    Model,
    Effort,
    Sandbox,
}

/// Auto-draft state for the unified creation modal. Persisted to
/// `DevState` on every keystroke per locked decision §B7. Only
/// `Committed` chip values survive serialization — `Pending` and
/// `Invalid` are transient. P2.3 extends with topology / execution
/// fields; serde defaults keep older dev-state.json files compatible.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateEntityDraft {
    pub kind: rsi_common::types::SessionKind,
    pub name: String,
    /// Multi-line prompt body, one entry per textarea line (S1). Serde
    /// default keeps older dev-state.json files compatible (F-003/F-030).
    #[serde(default)]
    pub body: Vec<String>,
    pub tags: Vec<String>,
    pub parent_id: Option<Uuid>,
    #[serde(default)]
    pub topology_id: Option<Uuid>,
    #[serde(default)]
    pub provider: Option<rsi_common::types::SessionProvider>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub sandbox: bool,
}

// === Notification System ===

/// Categorizes the source/nature of a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    TaskRabbitComplete,
    TaskRabbitFailed,
    BugComplete,
    BugFailed,
    SessionLaunching,
    SessionResuming,
    Connected,
    ConnectionFailed,
    ConnectionLost,
    OperationSuccess,
    OperationFailed,
    Info,
    SessionStalled,
    /// Stall classifier emitted a verdict (RSI-0XX). Ambient — never modal.
    SessionClassified,
}

/// Determines rendering treatment and TTL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NotificationPriority {
    /// Status line badge only. TTL: 5s.
    Low,
    /// Command bar message with auto-clear. TTL: 5s.
    Medium,
    /// Command bar message with emphasis. TTL: 10s.
    High,
}

/// A single notification in the queue.
#[derive(Debug, Clone)]
pub struct Notification {
    pub id: u64,
    pub kind: NotificationKind,
    pub message: String,
    pub priority: NotificationPriority,
    pub created_at: std::time::Instant,
    pub ttl: std::time::Duration,
    /// Source session (for navigation via Enter in notification overlay).
    pub session_id: Option<uuid::Uuid>,
    pub dismissed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::types::ContextUsageConfidence;

    // Relocated from the deleted `ui::vitals` module (ST-DEADCODE). These pin the
    // live `compute_recent_files` helper, whose only test coverage previously
    // lived in that dead UI module.
    mod data_helpers {
        use chrono::Utc;
        use rsi_common::types::{ConversationEvent, EventType};

        fn make_event(seq: i32, ev_type: EventType, tool_name: Option<&str>) -> ConversationEvent {
            ConversationEvent {
                id: 0,
                session_id: uuid::Uuid::nil(),
                sequence: seq,
                event_type: ev_type,
                role: None,
                content: String::new(),
                created_at: Utc::now(),
                tool_name: tool_name.map(String::from),
                tool_input: None,
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            }
        }

        #[test]
        fn compute_recent_files_returns_distinct_newest_first() {
            use crate::types::{RECENT_FILES_CAP, compute_recent_files};
            use serde_json::json;
            use std::path::PathBuf;

            fn ev_with_path(seq: i32, tool: &str, path: &str) -> ConversationEvent {
                let mut e = make_event(seq, EventType::ToolUse, Some(tool));
                e.tool_input = Some(Box::new(json!({ "file_path": path })));
                e
            }

            let events = vec![
                ev_with_path(1, "Read", "/a.rs"),
                ev_with_path(2, "Edit", "/b.rs"),
                ev_with_path(3, "Edit", "/a.rs"), // duplicate of /a.rs (older)
                ev_with_path(4, "Write", "/c.rs"),
                ev_with_path(5, "MultiEdit", "/d.rs"),
                ev_with_path(6, "Read", "/e.rs"),
                ev_with_path(7, "Read", "/f.rs"), // 6th distinct — should be cap-trimmed
            ];
            let files = compute_recent_files(&events);
            assert_eq!(files.len(), RECENT_FILES_CAP);
            // Newest-first traversal: f, e, d, c, a (latest occurrence of /a.rs is seq 3, not 1)
            assert_eq!(files[0], PathBuf::from("/f.rs"));
            assert_eq!(files[1], PathBuf::from("/e.rs"));
            assert_eq!(files[2], PathBuf::from("/d.rs"));
            assert_eq!(files[3], PathBuf::from("/c.rs"));
            assert_eq!(files[4], PathBuf::from("/a.rs"));
        }

        #[test]
        fn compute_recent_files_ignores_non_file_tools() {
            use crate::types::compute_recent_files;
            use serde_json::json;

            let mut bash_ev = make_event(1, EventType::ToolUse, Some("Bash"));
            bash_ev.tool_input = Some(Box::new(json!({ "command": "ls" })));
            let mut grep_ev = make_event(2, EventType::ToolUse, Some("Grep"));
            grep_ev.tool_input = Some(Box::new(json!({ "pattern": "foo", "path": "/x.rs" })));

            let files = compute_recent_files(&[bash_ev, grep_ev]);
            assert!(files.is_empty(), "non-file tools should be ignored");
        }
    }

    #[test]
    fn test_session_state_system_events_default_hidden() {
        let session = rsi_common::types::Session {
            context_fill_pct: None,
            id: uuid::Uuid::new_v4(),
            provider: rsi_common::types::SessionProvider::Claude,
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
            status: rsi_common::types::SessionStatus::Running,
            project_id: None,
            session_kind: rsi_common::types::SessionKind::Standard,
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
            context_usage_confidence: ContextUsageConfidence::default(),
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
            parent_id: None,
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
        };
        // Cold-start seeding is display-only: SessionState::new copies the
        // daemon-stamped `context_fill_pct` into the live counter verbatim and
        // never recomputes pct from token/window fields.
        let mut seeded = session.clone();
        seeded.context_fill_pct = Some(63.0);
        let seeded_state = SessionState::new(seeded);
        assert_eq!(
            seeded_state.live_context_pct,
            Some(63.0),
            "SessionState::new must seed live_context_pct from session.context_fill_pct"
        );

        // Absent daemon value → blank bar (no TUI-side fallback computation).
        let mut blank = session.clone();
        blank.context_fill_pct = None;
        blank.total_input_tokens = Some(100_000);
        blank.context_window = Some(200_000);
        let blank_state = SessionState::new(blank);
        assert_eq!(
            blank_state.live_context_pct, None,
            "TUI must not derive pct from token/window fields when the daemon value is absent"
        );

        let state = SessionState::new(session);
        assert!(
            !state.show_system_events,
            "system events should be hidden by default"
        );
    }

    #[test]
    fn test_split_node_serde_roundtrip() {
        let tree = SplitNode::Split {
            direction: SplitDirection::Vertical,
            first: Box::new(SplitNode::Leaf {
                pane: Pane::SessionList {
                    selected_index: 2,
                    selected_session: Some(uuid::Uuid::nil()),
                    scroll_offset: 0,
                    active_zone: Default::default(),
                    taskrabbit_selected_index: 0,
                    archive_selected_index: 0,
                    jobs_selected_index: 0,
                },
                id: PaneId(0),
            }),
            second: Box::new(SplitNode::Split {
                direction: SplitDirection::Horizontal,
                first: Box::new(SplitNode::Leaf {
                    pane: Pane::SessionDetail {
                        session_id: uuid::Uuid::nil(),
                    },
                    id: PaneId(1),
                }),
                second: Box::new(SplitNode::Leaf {
                    pane: Pane::SessionList {
                        selected_index: 0,
                        selected_session: None,
                        scroll_offset: 0,
                        active_zone: Default::default(),
                        taskrabbit_selected_index: 0,
                        archive_selected_index: 0,
                        jobs_selected_index: 0,
                    },
                    id: PaneId(2),
                }),
                id: PaneId(3),
            }),
            id: PaneId(4),
        };

        let json = serde_json::to_string(&tree).unwrap();
        let deserialized: SplitNode = serde_json::from_str(&json).unwrap();

        // Verify structure preserved
        assert_eq!(deserialized.leaf_ids().len(), 3);
        assert_eq!(deserialized.id(), PaneId(4));
        assert!(deserialized.find_pane(PaneId(1)).is_some());
    }

    #[test]
    fn test_tab_serde_with_project_id() {
        let project_id = uuid::Uuid::new_v4();
        let tab = Tab {
            name: "test".to_string(),
            session_list_state: Tab::default_session_list_state(),
            layout: SplitNode::Leaf {
                pane: Pane::SessionList {
                    selected_index: 0,
                    selected_session: None,
                    scroll_offset: 0,
                    active_zone: Default::default(),
                    taskrabbit_selected_index: 0,
                    archive_selected_index: 0,
                    jobs_selected_index: 0,
                },
                id: PaneId(0),
            },
            focused_pane: PaneId(0),
            project_id: Some(project_id),
            detail_split_offset: 0,
            layout_x_offset_adj: 0,
            session_list_width_pct: 0,
            descent_path: Vec::new(),
            bottom_focus_target: BottomZone::Off,
            mini_dag_focus: None,
        };

        let json = serde_json::to_string(&tab).unwrap();
        let restored: Tab = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.project_id, Some(project_id));
    }

    #[test]
    fn test_tab_serde_backward_compat_no_project_id() {
        // Old state without project_id field should deserialize to None
        let json = r#"{"name":"[1]","layout":{"Leaf":{"pane":{"SessionList":{"selected_index":0,"selected_session":null}},"id":0}},"focused_pane":0}"#;
        let tab: Tab = serde_json::from_str(json).unwrap();
        assert_eq!(tab.project_id, None);
        assert_eq!(tab.name, "[1]");
    }

    // ========================
    // IndentStyle detection tests
    // ========================

    #[test]
    fn test_detect_indent_style_tabs() {
        let lines: Vec<String> = vec![
            "fn main() {".into(),
            "\tprintln!(\"hello\");".into(),
            "\tif true {".into(),
            "\t\tdo_thing();".into(),
            "\t}".into(),
            "}".into(),
        ];
        let style = detect_indent_style(&lines, 100);
        assert_eq!(
            style,
            IndentStyle {
                use_tabs: true,
                width: 4
            }
        );
    }

    #[test]
    fn test_detect_indent_style_2_spaces() {
        let lines: Vec<String> = vec![
            "fn main() {".into(),
            "  let x = 1;".into(),
            "  if true {".into(),
            "    do_thing();".into(),
            "  }".into(),
            "}".into(),
        ];
        let style = detect_indent_style(&lines, 100);
        assert_eq!(
            style,
            IndentStyle {
                use_tabs: false,
                width: 2
            }
        );
    }

    #[test]
    fn test_detect_indent_style_4_spaces() {
        let lines: Vec<String> = vec![
            "fn main() {".into(),
            "    let x = 1;".into(),
            "    if true {".into(),
            "        do_thing();".into(),
            "    }".into(),
            "}".into(),
        ];
        let style = detect_indent_style(&lines, 100);
        assert_eq!(
            style,
            IndentStyle {
                use_tabs: false,
                width: 4
            }
        );
    }

    #[test]
    fn test_detect_indent_style_empty_file() {
        let lines: Vec<String> = vec!["".into()];
        let style = detect_indent_style(&lines, 100);
        assert_eq!(style, IndentStyle::default());
    }

    #[test]
    fn test_detect_indent_style_no_indentation() {
        let lines: Vec<String> = vec!["hello".into(), "world".into()];
        let style = detect_indent_style(&lines, 100);
        assert_eq!(style, IndentStyle::default());
    }
}

/// Per-tab rendering state for the session list: row geometry and scroll position.
#[derive(Debug, Clone, Default)]
pub struct ZoneRenderState {
    /// Per-row rendered heights (in terminal lines).
    pub card_heights: Vec<usize>,
    /// Cumulative line offsets — card_offsets[i] = sum of card_heights[0..i].
    pub card_offsets: Vec<usize>,
    /// Total content height (sum of all row heights plus section labels).
    pub total_content_height: usize,
    /// Last rendered session count for cache invalidation.
    pub last_session_count: usize,
    /// Last rendered width for cache invalidation.
    pub last_render_width: u16,
    /// Historical generation counter retained for compatibility with older code paths.
    pub last_card_generation: u64,
    /// Computed scroll offset (written by renderer, read by click handler).
    pub scroll_offset: usize,
    /// Label headers to render above specific row indices.
    /// Each entry is (row_index, label_name, color_hex, visible group count).
    /// Populated for label grouping and operational focus sections.
    pub label_headers: Vec<(usize, String, String, usize)>,
}

/// Cached render state for session list row layout and hit-testing.
/// Stored on App, updated by the renderer each frame.
#[derive(Debug, Default)]
pub struct SessionListRenderState {
    pub main: ZoneRenderState,
    pub taskrabbit: ZoneRenderState,
    pub archive: ZoneRenderState,
    pub jobs: ZoneRenderState,
    /// Full inner area of the list (below tab bar), updated each frame.
    pub last_list_area: Option<ratatui::layout::Rect>,
    /// Area used for row rendering in the active tab (for mouse hit-testing).
    pub cards_area: Option<ratatui::layout::Rect>,
    /// Cached operational hierarchy rollup used by full-list focus sections.
    pub focus_index: HashMap<Uuid, SessionFocusEntry>,
    /// `App.card_generation` value represented by `focus_index`.
    pub focus_generation: u64,
    /// Hour bucket represented by `focus_index`, so quiet-boundary aging does
    /// not depend on another session mutation occurring.
    pub focus_time_bucket: i64,
    /// Cached bounded ultra-wide activity view.
    pub activity: Option<SessionActivityViewModel>,
    /// `App.card_generation` represented by `activity`.
    pub activity_generation: u64,
    /// Minute bucket represented by `activity`, so relative activity stays fresh.
    pub activity_time_bucket: i64,
}

/// An item in the archive browser list (either a date header or a session reference).
#[derive(Debug, Clone)]
pub enum ArchiveListItem {
    Header(String),
    Session(usize),
}

/// Entry in the flattened file explorer tree.
#[derive(Debug, Clone)]
pub enum FileExplorerEntry {
    /// A directory node. When expanded, its children follow in the flat list.
    Directory {
        path: std::path::PathBuf,
        depth: usize,
        expanded: bool,
    },
    /// A file node.
    File {
        path: std::path::PathBuf,
        depth: usize,
    },
}

impl FileExplorerEntry {
    pub fn path(&self) -> &std::path::Path {
        match self {
            FileExplorerEntry::Directory { path, .. } => path,
            FileExplorerEntry::File { path, .. } => path,
        }
    }

    pub fn depth(&self) -> usize {
        match self {
            FileExplorerEntry::Directory { depth, .. } => *depth,
            FileExplorerEntry::File { depth, .. } => *depth,
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self, FileExplorerEntry::Directory { .. })
    }
}

/// Context for the project picker overlay.
#[derive(Clone, Debug, Default)]
pub enum ProjectPickerContext {
    /// Opened from session list — changes global project filter.
    #[default]
    GlobalFilter,
    /// Opened from session detail — reassigns the focused session's project.
    SessionReassign(uuid::Uuid),
}

/// Where the AI assistant was triggered from — determines result delivery target.
#[derive(Debug, Clone)]
pub enum AiAssistantSource {
    /// Triggered from the session detail input bar.
    InputBar(uuid::Uuid),
    /// Triggered from an overlay/input-overlay editing surface.
    OverlaySurface(uuid::Uuid),
}

/// Which field is being inline-edited in the graph review detail mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphEditField {
    Name,
    Instructions,
    IntegrationStrategy,
    VerificationStrategy,
}

/// Camera behavior for the graph view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphCamera {
    FollowSelection,
    Manual,
}

impl GraphCamera {
    pub fn label(self) -> &'static str {
        match self {
            Self::FollowSelection => "follow",
            Self::Manual => "manual",
        }
    }
}

/// Manual viewport offset relative to the follow-selection camera origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GraphViewport {
    pub offset_x: i32,
    pub offset_y: i32,
}

impl GraphViewport {
    pub fn pan(&mut self, dx: i32, dy: i32) {
        self.offset_x += dx;
        self.offset_y += dy;
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn is_centered(self) -> bool {
        self.offset_x == 0 && self.offset_y == 0
    }
}

/// Stable graph browsing modes that can survive child-mode transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphBrowseMode {
    Navigate,
    Detail,
}

impl GraphBrowseMode {
    pub fn with_camera(self, camera: GraphCamera) -> GraphMode {
        match self {
            Self::Navigate => GraphMode::Navigate { camera },
            Self::Detail => GraphMode::Detail { camera },
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Navigate => "navigate",
            Self::Detail => "detail",
        }
    }
}

/// Picker flows embedded in the graph review overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphPickerKind {
    Topology,
    SavedWorkflow,
    RecursiveGraphs,
}

impl GraphPickerKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Topology => "topology",
            Self::SavedWorkflow => "saved",
            Self::RecursiveGraphs => "recursive",
        }
    }
}

/// Explicit graph editor mode state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphMode {
    Navigate {
        camera: GraphCamera,
    },
    Detail {
        camera: GraphCamera,
    },
    EditField {
        field: GraphEditField,
        camera: GraphCamera,
    },
    Picker {
        kind: GraphPickerKind,
        selected_index: usize,
        previous_mode: GraphBrowseMode,
        camera: GraphCamera,
    },
    Executing {
        previous_mode: GraphBrowseMode,
        camera: GraphCamera,
    },
}

impl GraphMode {
    pub fn camera(self) -> GraphCamera {
        match self {
            Self::Navigate { camera }
            | Self::Detail { camera }
            | Self::EditField { camera, .. }
            | Self::Picker { camera, .. }
            | Self::Executing { camera, .. } => camera,
        }
    }

    pub fn browse_mode(self) -> GraphBrowseMode {
        match self {
            Self::Navigate { .. } => GraphBrowseMode::Navigate,
            Self::Detail { .. } | Self::EditField { .. } => GraphBrowseMode::Detail,
            Self::Picker { previous_mode, .. } | Self::Executing { previous_mode, .. } => {
                previous_mode
            }
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Navigate { .. } => "navigate",
            Self::Detail { .. } => "detail",
            Self::EditField { .. } => "edit",
            Self::Picker { .. } => "picker",
            Self::Executing { .. } => "executing",
        }
    }

    pub fn editing_field(self) -> Option<GraphEditField> {
        match self {
            Self::EditField { field, .. } => Some(field),
            _ => None,
        }
    }

    pub fn picker(self) -> Option<(GraphPickerKind, usize)> {
        match self {
            Self::Picker {
                kind,
                selected_index,
                ..
            } => Some((kind, selected_index)),
            _ => None,
        }
    }

    pub fn set_picker_index(self, selected_index: usize) -> Self {
        match self {
            Self::Picker {
                kind,
                previous_mode,
                camera,
                ..
            } => Self::Picker {
                kind,
                selected_index,
                previous_mode,
                camera,
            },
            _ => self,
        }
    }

    pub fn with_camera(self, camera: GraphCamera) -> Self {
        match self {
            Self::Navigate { .. } => Self::Navigate { camera },
            Self::Detail { .. } => Self::Detail { camera },
            Self::EditField { field, .. } => Self::EditField { field, camera },
            Self::Picker {
                kind,
                selected_index,
                previous_mode,
                ..
            } => Self::Picker {
                kind,
                selected_index,
                previous_mode,
                camera,
            },
            Self::Executing { previous_mode, .. } => Self::Executing {
                previous_mode,
                camera,
            },
        }
    }

    pub fn open_picker(self, kind: GraphPickerKind) -> Self {
        Self::Picker {
            kind,
            selected_index: 0,
            previous_mode: self.browse_mode(),
            camera: self.camera(),
        }
    }

    pub fn enter_detail(self) -> Self {
        match self {
            Self::Executing { camera, .. } => Self::Executing {
                previous_mode: GraphBrowseMode::Detail,
                camera,
            },
            _ => GraphBrowseMode::Detail.with_camera(self.camera()),
        }
    }

    pub fn back(self) -> Self {
        match self {
            Self::Detail { camera } => GraphBrowseMode::Navigate.with_camera(camera),
            Self::EditField { camera, .. } => GraphBrowseMode::Detail.with_camera(camera),
            Self::Picker {
                previous_mode,
                camera,
                ..
            }
            | Self::Executing {
                previous_mode,
                camera,
            } => previous_mode.with_camera(camera),
            Self::Navigate { .. } => self,
        }
    }

    pub fn into_executing(self) -> Self {
        Self::Executing {
            previous_mode: self.browse_mode(),
            camera: self.camera(),
        }
    }

    pub fn shows_detail_panel(self) -> bool {
        matches!(
            self,
            Self::Detail { .. }
                | Self::EditField { .. }
                | Self::Executing {
                    previous_mode: GraphBrowseMode::Detail,
                    ..
                }
        )
    }

    pub fn allows_overlay_close(self) -> bool {
        matches!(
            self,
            Self::Navigate { .. } | Self::Detail { .. } | Self::Executing { .. }
        )
    }

    pub fn allows_execution_start(self) -> bool {
        matches!(self, Self::Navigate { .. } | Self::Detail { .. })
    }

    pub fn is_picker(self) -> bool {
        matches!(self, Self::Picker { .. })
    }

    pub fn is_executing(self) -> bool {
        matches!(self, Self::Executing { .. })
    }
}

/// Persistence state for a graph draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GraphDraftPersistenceState {
    #[default]
    Clean,
    Dirty,
    Saving,
    SaveFailed,
}

impl GraphDraftPersistenceState {
    pub fn needs_emergency_save(self) -> bool {
        !matches!(self, Self::Clean)
    }
}

/// Provenance of the graph currently shown in `gv` / `GraphReview`.
///
/// Inert view-discriminant (Phase-1 V1): declared and serde-defaulted, but not
/// yet read by any render, edit, delete, undo, or write path -- that wiring is
/// V2. Default `AuthoredWorkflow` preserves today's behavior byte-for-byte; the
/// bridged origins are reserved for V2 read-only rendering of topology- and
/// recursive-bridged graphs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GraphViewOrigin {
    /// Native workflows row -- editable (today's only behavior).
    #[default]
    AuthoredWorkflow,
    /// Exists via `metadata.source_topology_id` -- read-only.
    BridgedTopology { topology_id: Uuid },
    /// V0 bridge output (`metadata.source_recursive_graph_id`) -- read-only.
    BridgedRecursive { recursive_graph_id: Uuid },
}

/// App-level graph draft state that survives overlay close/reopen.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GraphDraft {
    pub draft_id: Uuid,
    #[serde(default)]
    pub workflow_id: Option<Uuid>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
    pub workflow: rsi_graph::format::WorkflowDefinition,
    #[serde(default)]
    pub persistence_state: GraphDraftPersistenceState,
    pub last_execution_id: Option<Uuid>,
    /// Inert Phase-1 V1 view-discriminant; defaults to `AuthoredWorkflow` so
    /// drafts persisted without this key deserialize unchanged.
    #[serde(default)]
    pub view_origin: GraphViewOrigin,
}

impl GraphDraft {
    pub fn new(
        workflow: rsi_graph::format::WorkflowDefinition,
        workflow_id: Option<Uuid>,
        project_id: Option<Uuid>,
    ) -> Self {
        Self {
            draft_id: Uuid::new_v4(),
            workflow_id,
            project_id,
            workflow,
            persistence_state: GraphDraftPersistenceState::Clean,
            last_execution_id: None,
            view_origin: GraphViewOrigin::AuthoredWorkflow,
        }
    }

    pub fn with_persistence_state(mut self, persistence_state: GraphDraftPersistenceState) -> Self {
        self.persistence_state = persistence_state;
        self
    }

    pub fn normalize_after_restore(mut self) -> Self {
        if self.draft_id.is_nil() {
            self.draft_id = Uuid::new_v4();
        }
        if matches!(self.persistence_state, GraphDraftPersistenceState::Saving) {
            self.persistence_state = GraphDraftPersistenceState::SaveFailed;
        }
        self
    }
}

/// Identifies a particular (event, entry_index, hook_index) cell in the
/// hooks BTreeMap, used as the edit target for `OverlayState::HookForm`.
#[derive(Debug, Clone)]
pub struct HookFormEditingTarget {
    pub event: crate::claude_config::HookEvent,
    pub entry_index: usize,
    pub hook_index: usize,
}

/// State for the operator-only source-worktree settlement overlay. The typed
/// daemon reports remain authoritative; the TUI never synthesizes eligibility
/// or removes a cohort optimistically.
pub struct SourceWorktreeSettlementOverlayState {
    pub cohorts: Vec<rsi_common::cohort_settlement::SourceWorktreeCohortSummaryV1>,
    pub selected_index: usize,
    pub scroll_offset: usize,
    pub audit: Option<rsi_common::cohort_settlement::SourceWorktreeCohortAuditV1>,
    pub receipt: Option<rsi_common::cohort_settlement::SourceWorktreeSettlementRunV1>,
    pub authorization_input: String,
    pub authorization_active: bool,
    pub idempotency_key: Option<String>,
    pub last_error: Option<String>,
}

/// Active overlay state. Only one overlay can be active at a time.
/// When `OverlayState` is not `None`, the overlay captures all key input.
pub enum OverlayState {
    /// No overlay active — normal app operation.
    None,
    /// Fuzzy command finder, with the context captured below this overlay.
    CommandPalette {
        query: String,
        results: Vec<crate::action_registry::ActionId>,
        selected: usize,
        argument_edit: bool,
        argument_input: String,
        origin: crate::action_registry::ActionContext,
    },
    /// Operator appointment and versioned Epic scope for a project manager.
    HarnessManagerScope(Box<crate::overlay::harness_manager::HarnessManagerScopeState>),
    HarnessManagerV2(Box<crate::overlay::manager_v2::ManagerSurface>),
    /// Theme picker popup for Catppuccin + custom palettes.
    ThemePicker {
        /// Currently highlighted theme index.
        selected_index: usize,
        /// Theme index when the picker opened (for Esc revert).
        original_index: usize,
    },
    /// Editor for one semantic theme role with live preview and rollback.
    ThemeRoleEditor {
        role: crate::ui::theme_roles::ThemeRole,
        input: String,
        opening_overrides: Vec<(crate::ui::theme_roles::ThemeRole, [u8; 3])>,
        assessment: Option<crate::ui::theme_roles::ContrastAssessment>,
        committed: bool,
        pending_acknowledgement: Option<(
            crate::ui::theme_roles::ThemeRole,
            [u8; 3],
            crate::ui::theme_roles::ContrastAssessment,
        )>,
    },
    /// Color customizer overlay — lets the user set per-role message border colors via hex input.
    ColorCustomizer {
        /// Which field is focused: 0=assistant, 1=user, 2=tool_unselected, 3=tool_selected
        focused_field: usize,
        /// Current text input for each of the 4 fields (populated with active override hex on open).
        inputs: Vec<String>,
        /// Whether each field currently has a parse error.
        errors: Vec<bool>,
    },
    /// Single-field hex editor for the text-area backfill color
    /// (`UserSettings::text_area_backfill_hex`).
    TextAreaBgEditor {
        /// Current text input — `#RRGGBB` (or empty to clear).
        input: String,
        /// Whether the current input failed to parse.
        error: bool,
    },
    /// Prompt popup for entering a new session query.
    Prompt {
        /// Stable identity used to route async results back to this exact popup.
        overlay_id: uuid::Uuid,
        /// Shared editing surface (textarea, mode, vim state, suggestions, correction).
        surface: crate::input_surface::InputSurface,
        working_dir: std::path::PathBuf,
        /// Purpose of this prompt — new session or continue existing.
        purpose: PromptPurpose,
        /// All discovered commands (cloned from App at popup open time).
        available_commands: Vec<crate::suggestions::CommandSuggestion>,
        /// Per-session model override (None = use global app.selected_model).
        model_override: Option<String>,
        /// Per-session provider override (None = use global app.selected_provider).
        provider_override: Option<rsi_common::types::SessionProvider>,
        /// Dropdown widget state for inline model selection.
        model_dropdown: ModelDropdownState,
        /// Whether sandbox mode is requested for this launch.
        /// Only meaningful when `PromptPurpose::Blank | TaskRabbit`. Default: false.
        sandbox_enabled: bool,
    },
    /// Project picker popup (telescope-style).
    ProjectPicker {
        /// Fuzzy filter input.
        filter: String,
        /// Currently selected index in the filtered list.
        /// Index 0 = "All projects", then projects, then "(unassigned)" at the end.
        selected_index: usize,
        /// What happens when a project is selected.
        context: ProjectPickerContext,
    },
    /// Keybindings help popup.
    KeybindingsHelp {
        /// Contextual actions or the complete command reference.
        view: crate::overlay::keybindings_help::HelpView,
        /// Scroll offset in lines (0 = top of content).
        scroll_offset: usize,
        /// Fuzzy filter query string.
        filter: String,
        /// Whether the search input is currently active (accepting text input).
        search_active: bool,
        /// Target surface whose live actions are being described.
        origin: crate::action_registry::HelpOrigin,
    },
    /// Space+s sort order picker.
    SortPicker {
        /// Currently highlighted sort option index.
        selected_index: usize,
    },
    /// Prompt preview popup — shows full query for selected session.
    /// Content is derived at render time from the currently selected session.
    PromptPreview {
        /// Scroll offset in lines (for long prompts).
        scroll_offset: usize,
    },
    /// Destructive maintenance browser reached from Settings. Mutation stays
    /// disabled until a fresh typed audit and exact operator phrase exist.
    SourceWorktreeSettlement(SourceWorktreeSettlementOverlayState),
    /// Trash browser — browse, restore, and permanently purge soft-deleted sessions.
    TrashBrowser {
        sessions: Vec<Session>,
        items: Vec<ArchiveListItem>,
        selected_index: usize,
        scroll_offset: usize,
    },
    /// Notification history browser.
    NotificationBrowser {
        selected_index: usize,
        scroll_offset: usize,
    },
    /// Recent completions focus — navigate and jump to recently finished sessions.
    /// Renders in the right sidebar gutter (not a centered popup).
    RecentCompletions {
        /// Index into the recent completions list.
        selected_index: usize,
    },
    /// Project create/edit form.
    ProjectForm {
        /// Which field is focused (0=Name, 1=Path, 2=Color).
        focused_field: usize,
        /// Name field content.
        name: String,
        /// Path field content (optional).
        path: String,
        /// Selected color index in the palette.
        color_index: usize,
        /// If editing an existing project, its ID.
        editing_id: Option<uuid::Uuid>,
    },
    /// File explorer drawer — left-anchored tree view of session working directory.
    FileExplorer {
        /// Root directory being explored.
        root: std::path::PathBuf,
        /// Flattened tree entries (directories + files, depth-tracked).
        entries: Vec<FileExplorerEntry>,
        /// Index into `entries` of the highlighted row.
        selected_index: usize,
        /// Scroll offset for virtual scrolling.
        scroll_offset: usize,
        /// Whether to show hidden files (dotfiles). Default: false.
        show_hidden: bool,
        /// Trash buffer for undo: (original_path, file_contents).
        /// Scoped to the explorer's lifetime.
        trash: Vec<(std::path::PathBuf, Vec<u8>)>,
        /// First key of yy/yn chord pressed.
        pending_yank: bool,
        /// First key of dd chord pressed.
        pending_delete: bool,
        /// Whether the fuzzy finder sub-mode is active.
        finder_active: bool,
        /// Current finder query string.
        finder_query: String,
        /// Cached file paths from `ignore::Walk` (relative to root). Populated on first `/`.
        finder_cache: Vec<std::path::PathBuf>,
        /// Scored + sorted results: indices into `finder_cache`.
        finder_results: Vec<usize>,
        /// Selected index within `finder_results`.
        finder_selected: usize,
        /// Whether the explorer panel has keyboard focus (vs the file viewer).
        /// Ctrl+L shifts focus to the viewer, Ctrl+H shifts it back here.
        explorer_focused: bool,
    },
    /// Custom OpenAI-compatible provider add/edit form.
    ProviderForm {
        /// Which field is focused (0=Name, 1=URL, 2=API Key, 3=Default Model).
        focused_field: usize,
        /// Display name field.
        name: crate::input_surface::InputSurface,
        /// Base URL field.
        base_url: crate::input_surface::InputSurface,
        /// API key field (displayed masked).
        api_key: crate::input_surface::InputSurface,
        /// Default model ID field.
        default_model: crate::input_surface::InputSurface,
        /// If editing an existing provider, its UUID.
        editing_id: Option<uuid::Uuid>,
    },
    /// Signal/iMessage bridge connection form.
    MessageBridgeForm {
        bridge: crate::settings::MessageBridgeKind,
        focused_field: usize,
        enabled: bool,
        account: String,
        allow_from: String,
        working_dir: String,
    },
    /// Claude Code hook add/edit form. Edits ~/.claude/settings.json.
    HookForm {
        /// Which field is focused (0=Event, 1=Matcher, 2=Command, 3=Timeout).
        focused_field: usize,
        /// Index into `KnownHookEvent::ALL` for the cyclable picker.
        /// `None` = the user is editing an existing `Other(String)` event;
        /// the event name is preserved verbatim and not editable in v1.
        event_idx: Option<usize>,
        /// Frozen event name when editing an `Other(String)` row.
        event_name_other: Option<String>,
        /// Optional regex matcher (PreToolUse/PostToolUse).
        matcher: String,
        /// Shell command to execute.
        command: String,
        /// Timeout in seconds (display string; parsed at submit).
        timeout: String,
        /// If editing, the (event, entry_index, hook_index) tuple identifying the row.
        /// `None` = new entry.
        editing: Option<HookFormEditingTarget>,
        /// Snapshot of (mtime, original raw bytes) at form-open time, for Q7
        /// conflict detection. Bytes compared verbatim at save (no hash).
        snapshot_mtime: Option<std::time::SystemTime>,
        snapshot_bytes: Vec<u8>,
    },
    /// Budget policy add/edit form (Settings -> Budgets). Submits via
    /// `LcAction::SubmitBudgetPolicy` — the daemon's `UpdateModelControlPolicy`
    /// RPC with `replace_policies: true` is the actual persistence path; this
    /// overlay only stages the edit.
    BudgetPolicyForm {
        /// Which field is focused: 0=ScopeKind, 1=ScopeId, 2=Purpose,
        /// 3=ModelTier, 4=MaxTotalTokens, 5=MaxConcurrency,
        /// 6=MaxCallsPerWindow, 7=RateWindowSeconds, 8=AlertThresholdRatio.
        focused_field: usize,
        /// Index into `model_control_budgets::SCOPE_KINDS` for the
        /// cyclable scope-kind picker.
        scope_kind_idx: usize,
        /// Scope id text (ignored / not required at submit when `scope_kind`
        /// resolves to `BudgetScopeKind::Global`).
        scope_id: String,
        /// Free-text purpose; blank = all purposes. Validated at submit
        /// against `rsi_common::model_control::ModelInvocationPurpose`.
        purpose: String,
        /// Index into `model_control_budgets::MODEL_TIERS` for the
        /// cyclable model-tier picker (0 = "any").
        model_tier_idx: usize,
        /// Digits-only display string for `ModelBudgetPolicy::max_total_tokens`.
        max_total_tokens: String,
        /// Digits-only display string for `ModelBudgetPolicy::max_concurrency`.
        max_concurrency: String,
        /// Digits-only display string for `ModelBudgetPolicy::max_calls_per_window`.
        max_calls_per_window: String,
        /// Digits-only display string for `ModelBudgetPolicy::rate_window_seconds`.
        rate_window_seconds: String,
        /// Digits-and-one-'.'-only display string for
        /// `ModelBudgetPolicy::alert_threshold_ratio`.
        alert_threshold_ratio: String,
        /// `Some(index into the cached policies list)` when editing an
        /// existing row, `None` when adding a new policy.
        editing: Option<usize>,
        /// Full original policy being edited (all 21 fields), so submit can
        /// overwrite ONLY the 9 fields this form exposes and preserve every
        /// other field verbatim. `None` when adding a new policy.
        original: Option<rsi_common::model_control::ModelBudgetPolicy>,
    },
    /// Three-choice confirm modal for external-edit conflicts (Q7).
    HookConflictPrompt {
        pending: Box<crate::claude_config::PendingHookSave>,
    },
    /// Read-only viewer for `~/.claude/skills/<name>/SKILL.md`.
    SkillPreview {
        name: String,
        content: String,
        scroll_offset: usize,
    },
    /// Diagnostics overlay — displays AppMetrics (render/poll timings, cache hit rate).
    /// Only meaningful when RSI_PROFILE=1; shows zeroes otherwise.
    Diagnostics,
    /// Read-only recursive DAG browser. Opened by `:dag`.
    RecursiveDagBrowser(RecursiveDagBrowserState),
    /// Memory search overlay — live search against the indexed memory files.
    MemorySearch {
        /// Current search query string.
        query: String,
        /// Search results from last completed search.
        results: Vec<rsi_common::rpc::MemorySearchResult>,
        /// Index of highlighted result.
        selected_index: usize,
        /// True while a search RPC is in-flight.
        loading: bool,
    },
    /// Session rename overlay (F2 from session list).
    RenameSession {
        /// Session being renamed.
        session_id: uuid::Uuid,
        /// Current title text being edited.
        title: String,
    },
    /// Question modal for AskUserQuestion
    QuestionModal {
        session_id: uuid::Uuid,
        questions: Vec<rsi_common::types::QuestionItem>,
        /// Which question we're currently answering (index into questions)
        current_question: usize,
        /// Highlighted option row per question (j/k target)
        cursor: Vec<usize>,
        /// Committed selection per question
        selections: Vec<QuestionSelection>,
        /// Free-text textarea for "Other" responses
        textarea: Box<tui_textarea::TextArea<'static>>,
        mode: PopupMode,
        pending_operator: Option<char>,
    },
    /// ESP Square guessing game — 3×3 colored grid, 12 rounds.
    EspSquare {
        /// Current round (0-based, 0..12). 12 = game over.
        round: u8,
        /// Number of correct guesses so far.
        correct: u8,
        /// Whether to show running score (interactive mode).
        interactive: bool,
        /// Per-round results: None = pass/skipped, Some(true) = correct, Some(false) = miss.
        rounds: Vec<Option<bool>>,
        /// Transient status message (e.g., game-over summary).
        message: String,
        /// Flash state: Some((grid_index, hit)). hit=true → green fill,
        /// hit=false → red fill (miss/peek). Auto-cleared after 300ms.
        flash: Option<(usize, bool)>,
        /// Deadline after which flash is auto-cleared (300ms from trigger).
        flash_deadline: Option<std::time::Instant>,
        /// Pre-generated target for the current round. Regenerated on peek.
        target: usize,
        /// Per-round detail: (player_pick, computer_target, guess_ms). None for passes.
        /// guess_ms is milliseconds elapsed since game start.
        round_details: Vec<Option<(usize, usize, u64)>>,
        /// Arrow-key cursor position. None until first arrow press.
        cursor: Option<usize>,
        /// Tracks the last guess key and time for same-key debouncing.
        last_guess: Option<(char, std::time::Instant)>,
        /// Instant when the game started (or was last reset). Used for per-guess timing.
        started_at: std::time::Instant,
    },
    /// Label picker overlay (telescope-style, like ProjectPicker).
    LabelPicker {
        /// Fuzzy filter input.
        filter: String,
        /// Currently selected index in the filtered list.
        selected_index: usize,
    },
    /// Label create/edit form (like ProjectForm).
    LabelForm {
        /// Which field is focused (0=Name, 1=Description, 2=Color).
        focused_field: usize,
        /// Name field content.
        name: String,
        /// Description field content.
        description: String,
        /// Selected color index in the palette.
        color_index: usize,
        /// If editing an existing group, its ID.
        editing_id: Option<uuid::Uuid>,
    },
    /// Input modal — quarter-size centered popup for longer-form input to the current session.
    /// Activated by Ctrl+G from session detail. Hides the input bar while open.
    /// Does NOT show working directory or model name.
    InputModal {
        /// Stable identity used to route async results back to this exact popup.
        overlay_id: uuid::Uuid,
        /// Shared editing surface.
        surface: crate::input_surface::InputSurface,
        /// The session this modal will submit to.
        session_id: uuid::Uuid,
    },
    /// AI chat window — Q&A about the source text.
    /// Activated by Ctrl+Shift+A from any InputSurface context.
    AiChat {
        /// Conversation history: (role, content) pairs. Role is "user" or "assistant".
        messages: Vec<(String, String)>,
        /// Current input text.
        input: String,
        /// Snapshot of the source text.
        source_text: String,
        /// Where the chat was triggered from (for overlay restoration only).
        source: AiAssistantSource,
        /// True while waiting for model response.
        in_flight: bool,
        /// Scroll offset in the conversation display.
        scroll_offset: usize,
    },
    /// AI command input — small floating bar for text transformation instructions.
    /// Activated by Ctrl+A from any InputSurface context.
    AiCommand {
        /// Single-line input for the command.
        command: String,
        /// Snapshot of the source text at trigger time.
        source_text: String,
        /// Where to deliver the result.
        source: AiAssistantSource,
        /// True while the model is processing.
        in_flight: bool,
    },
    /// Graph review overlay — visual workflow editor for WorkflowDefinitions.
    /// Activated by `:graph` command or `gR` keybinding.
    GraphReview {
        /// Primary draft identity for the workflow being reviewed.
        draft_id: Uuid,
        /// Currently selected node index.
        selected_node: usize,
        /// Manual pan offset relative to the follow-selection camera.
        viewport: GraphViewport,
        /// Explicit graph navigation/edit/execution mode.
        mode: GraphMode,
        /// Edit history for undo.
        edit_history: Vec<rsi_graph::format::WorkflowDefinition>,
        /// Whether topology groups are collapsed.
        collapsed: HashSet<String>,
        /// Buffer for the current inline edit.
        edit_buffer: String,
        /// Selected edge index within the current node's edges.
        selected_edge: usize,
        /// Scroll/offset state for the embedded picker (`Topology` +
        /// `SavedWorkflow`). Sibling to `GraphMode::Picker.selected_index`;
        /// kept in sync at the `update_picker_index` boundary so the offset
        /// math stays consistent with the application source of truth.
        /// Lives outside `GraphMode` because `GraphMode` derives `Copy` and
        /// `ListState` is not `Copy`.
        picker_list_state: ratatui::widgets::ListState,
        /// Inert Phase-1 V1 view-discriminant (provenance of this graph view);
        /// not yet read by any render/edit path. Defaults to `AuthoredWorkflow`,
        /// preserving today's editable authored-workflow behavior.
        view_origin: GraphViewOrigin,
        /// Gated rendering of the info dashboard column on the right side of the
        /// Sugiyama canvas in `gv` overlay.
        gv_info_dashboard: bool,
        /// Tracks whether the right-hand dashboard column currently has user focus.
        dashboard_focused: bool,
        /// Embedded recursive DAG browser state to drive the dashboard scrolling, panel selection and details.
        dashboard_state: Option<RecursiveDagBrowserState>,
    },
    /// Entity card editor — view/edit facts for a project or user card.
    /// Activated by `:card` (project) or `:card user` (user card).
    CardEditor {
        /// "project" or "user"
        entity_type: String,
        /// project_id string or "self"
        entity_id: String,
        /// Display name for the header (project name or "User").
        display_name: String,
        /// Current facts being edited.
        facts: Vec<String>,
        /// Selected fact index (for navigation).
        selected_index: usize,
        /// Scroll offset for long lists.
        scroll_offset: usize,
        /// Edit mode: None = navigation, Some(text) = editing the selected fact inline.
        editing: Option<String>,
        /// True while loading card data from daemon.
        loading: bool,
        /// First key of `dd` chord pressed (pending delete).
        pending_delete: bool,
    },
    /// Telescope fuzzy file picker — activated by Space+Space.
    Telescope {
        /// Root directory for file scanning.
        root: std::path::PathBuf,
        /// Session ID to open files into (resolved at open time).
        session_id: uuid::Uuid,
        /// Current fuzzy query string.
        query: String,
        /// Cached file paths from ignore::Walk (relative to root).
        file_cache: Vec<std::path::PathBuf>,
        /// Scored results: indices into file_cache.
        results: Vec<usize>,
        /// Selected index within results.
        selected: usize,
    },
    /// Dialectic query interface -- natural-language Q&A against accumulated knowledge.
    /// Activated by `g?` keybinding or `:ask` command.
    Dialectic {
        /// Conversation history: (role, content) pairs. Role is "user" or "assistant".
        messages: Vec<(String, String)>,
        /// Sources from the most recent response.
        sources: Vec<rsi_common::rpc::DialecticSource>,
        /// Current input text.
        input: String,
        /// True while waiting for daemon response.
        in_flight: bool,
        /// Scroll offset in the conversation display.
        scroll_offset: usize,
        /// Whether the sources footer is expanded.
        sources_expanded: bool,
        /// Optional project scope for queries.
        project_id: Option<uuid::Uuid>,
    },
    /// Schedule Browser — list/manage scheduled jobs.
    ScheduleBrowser {
        jobs: Vec<rsi_common::types::ScheduledJob>,
        selected_index: usize,
        loading: bool,
        /// True after the first `d`, waiting for the second key of the `dd` delete chord.
        pending_delete: bool,
    },
    /// Schedule Form — create/edit a scheduled job.
    ScheduleForm {
        focused_field: usize,
        name: String,
        message: String,
        recurrence_index: usize,
        interval: String,
        anchor_date: String,
        anchor_time: String,
        editing_id: Option<uuid::Uuid>,
    },
    /// Embedded terminal overlay — runs user's shell in a PTY.
    Terminal,
    /// Session rating overlay — user picks a 1–10 rating and confirms with Enter.
    RatingOverlay {
        /// Session being rated.
        session_id: Uuid,
        /// Current digit selection (None until the user types a digit).
        selected_rating: Option<u32>,
    },
    /// Consolidated, read-only session info panel (F3) — id, provider/model,
    /// working dir, project, hierarchy, rating, label, tags, context usage.
    SessionInfoPanel {
        /// Session whose metadata is displayed.
        session_id: Uuid,
    },
    /// Unified entity-creation form (Group / Epic / Story / Task / Bug …).
    /// Phase 2 of the topology-on-epic project. V1 (P2.1) owns kind + name +
    /// tags; V2 (P2.3) extends with topology / provider / model / effort /
    /// sandbox / parent-picker integration.
    CreateEntityForm {
        kind: rsi_common::types::SessionKind,
        name: String,
        /// Multi-line prompt body surface (leaf kinds only; S1).
        /// `InputSurface` already boxes its `TextArea` — no extra Box needed.
        body: crate::input_surface::InputSurface,
        tags: Vec<TagChip>,
        /// Field currently focused for Tab cycling.
        focused_field: CreateEntityField,
        /// True when an insert-mode (text-field) edit is active.
        insert_mode: bool,
        /// Hierarchical parent — resolved at open time from descent head;
        /// user-editable via the form-local `gp` chord (P2.3 Phase 5).
        parent_id: Option<Uuid>,
        /// Open-precheck error or daemon rejection text (red banner).
        error: Option<String>,

        // === P2.3 new fields ===
        /// `g`-leader pending for form-local `gp` parent-picker chord.
        /// Set by `g` press, consumed/cleared by the next key.
        g_pending: bool,
        /// Topology selection (Epic only). NULL = no topology.
        topology_id: Option<Uuid>,
        /// Inline filter buffer for the topology dropdown (Pattern 2).
        topology_filter: String,
        /// Highlighted index in the filtered topology list; resets on
        /// filter mutation.
        topology_selected_index: usize,
        /// Cached topology list fetched once at overlay open. NOT persisted
        /// to DevState — re-fetched on every open per Decision 4.
        topology_choices: Vec<rsi_common::types::Topology>,
        /// `Space`-toggleable ASCII DAG preview pane below the popup.
        preview_open: bool,
        // === Execution group (leaf kinds only) ===
        provider: Option<rsi_common::types::SessionProvider>,
        model: Option<String>,
        effort: Option<String>,
        sandbox: bool,
        /// Active model sub-overlay widget state; populated when the user
        /// opens the model picker via `m`. Cleared on Selected/Dismissed.
        model_dropdown: Option<ModelDropdownState>,
    },
    /// Parent picker overlay — pick a new hierarchical parent for a session.
    /// `candidates` is precomputed from `legal_children(parent_kind)` filtering;
    /// a `None` slot represents the `[root]` option (when applicable).
    ParentPicker {
        session_id: Uuid,
        /// Candidate parent ids. Empty Uuid::nil() encodes the `[root]` option.
        candidates: Vec<Uuid>,
        selected: usize,
        query: String,
    },
}

#[cfg(test)]
mod graph_view_origin_tests {
    use super::*;

    #[test]
    fn graph_view_origin_serde_backward_compat_and_round_trip() {
        // (a) Backward-compat: a GraphDraft serialized WITHOUT a `view_origin`
        // key must still deserialize, defaulting the field to AuthoredWorkflow.
        let workflow = rsi_graph::format::WorkflowDefinition::new("v1-view-origin-test");
        let draft = GraphDraft::new(workflow, None, None);

        let mut value = serde_json::to_value(&draft).expect("serialize GraphDraft to value");
        let removed = value
            .as_object_mut()
            .expect("GraphDraft serializes as a JSON object")
            .remove("view_origin");
        assert!(
            removed.is_some(),
            "serialized GraphDraft must contain a view_origin key to strip",
        );
        let legacy: GraphDraft =
            serde_json::from_value(value).expect("deserialize GraphDraft missing view_origin");
        assert_eq!(
            legacy.view_origin,
            GraphViewOrigin::AuthoredWorkflow,
            "a missing view_origin key must default to AuthoredWorkflow",
        );

        // (b) Every GraphViewOrigin variant round-trips through serde_json.
        let topology_id = Uuid::from_u128(1);
        let recursive_graph_id = Uuid::from_u128(2);
        for origin in [
            GraphViewOrigin::AuthoredWorkflow,
            GraphViewOrigin::BridgedTopology { topology_id },
            GraphViewOrigin::BridgedRecursive { recursive_graph_id },
        ] {
            let json = serde_json::to_string(&origin).expect("serialize GraphViewOrigin");
            let round: GraphViewOrigin =
                serde_json::from_str(&json).expect("deserialize GraphViewOrigin");
            assert_eq!(
                origin, round,
                "GraphViewOrigin variant must round-trip: {}",
                json
            );
        }

        // (c) A default-constructed GraphDraft (and the enum default) is
        // AuthoredWorkflow, preserving today's behavior byte-for-byte.
        assert_eq!(draft.view_origin, GraphViewOrigin::AuthoredWorkflow);
        assert_eq!(
            GraphViewOrigin::default(),
            GraphViewOrigin::AuthoredWorkflow
        );
    }
}
