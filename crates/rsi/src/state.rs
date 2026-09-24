//! TUI state persistence — saves/loads UI state to ~/.rsi/state.json

use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use uuid::Uuid;

#[cfg(not(test))]
use std::sync::atomic::AtomicU8;

use crate::app::SortOrder;
use crate::settings::UserSettings;
use crate::types::{PopupMode, Tab};
use crate::ui::theme;
use rsi_common::types::SessionProvider;

#[derive(Clone, Copy)]
enum StateFile {
    Persisted,
    Dev,
    EmergencyDrafts,
}

impl StateFile {
    fn production_path(self) -> PathBuf {
        match self {
            Self::Persisted => rsi_common::identity::data_path("state.json", "state.json"),
            Self::Dev => rsi_common::identity::data_path("dev-state.json", "dev-state.json"),
            Self::EmergencyDrafts => rsi_common::identity::data_path(
                "graph-drafts/emergency-drafts.json",
                "emergency-drafts.json",
            ),
        }
    }

    fn relative_path(self) -> &'static str {
        match self {
            Self::Persisted => "state.json",
            Self::Dev => "dev-state.json",
            Self::EmergencyDrafts => "graph-drafts/emergency-drafts.json",
        }
    }
}

#[cfg(not(test))]
const STATE_PATH_UNDECIDED: u8 = 0;
#[cfg(not(test))]
const STATE_PATH_PRODUCTION: u8 = 1;
#[cfg(not(test))]
const STATE_PATH_ISOLATED: u8 = 2;

#[cfg(not(test))]
static STATE_PATH_SELECTION: AtomicU8 = AtomicU8::new(STATE_PATH_UNDECIDED);
static PROCESS_ROOT: OnceLock<PathBuf> = OnceLock::new();
static PROCESS_ROOT_NONCE: AtomicU64 = AtomicU64::new(0);
static THREAD_NAMESPACE_ID: AtomicU64 = AtomicU64::new(0);
static PROCESS_ROOT_LIFECYCLE: ProcessRootLifecycle = ProcessRootLifecycle::new();

const PROCESS_ROOT_RECLAIMING: u64 = 1 << 63;
const PROCESS_ROOT_CREATOR_MASK: u64 = PROCESS_ROOT_RECLAIMING - 1;

struct ProcessRootLifecycle {
    state: AtomicU64,
}

impl ProcessRootLifecycle {
    const fn new() -> Self {
        Self {
            state: AtomicU64::new(0),
        }
    }

    fn acquire_creation(&self, on_reclaim: impl FnOnce()) -> ProcessRootCreationGuard<'_> {
        let mut on_reclaim = Some(on_reclaim);
        loop {
            let state = self.state.load(Ordering::Acquire);
            if state & PROCESS_ROOT_RECLAIMING != 0 {
                if let Some(on_reclaim) = on_reclaim.take() {
                    on_reclaim();
                }
                std::thread::yield_now();
                continue;
            }
            if state == PROCESS_ROOT_CREATOR_MASK {
                panic!("test state isolation process-root creator count overflow");
            }
            if self
                .state
                .compare_exchange_weak(state, state + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return ProcessRootCreationGuard { lifecycle: self };
            }
        }
    }

    fn try_acquire_reclaim(&self) -> Option<ProcessRootReclaimGuard<'_>> {
        self.state
            .compare_exchange(
                0,
                PROCESS_ROOT_RECLAIMING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok()
            .map(|_| ProcessRootReclaimGuard { lifecycle: self })
    }
}

struct ProcessRootCreationGuard<'a> {
    lifecycle: &'a ProcessRootLifecycle,
}

impl Drop for ProcessRootCreationGuard<'_> {
    fn drop(&mut self) {
        let previous = self.lifecycle.state.fetch_sub(1, Ordering::AcqRel);
        assert!(
            previous & PROCESS_ROOT_RECLAIMING == 0 && previous > 0,
            "invalid test state isolation process-root creator state: {previous}"
        );
    }
}

struct ProcessRootReclaimGuard<'a> {
    lifecycle: &'a ProcessRootLifecycle,
}

impl Drop for ProcessRootReclaimGuard<'_> {
    fn drop(&mut self) {
        self.lifecycle.state.store(0, Ordering::Release);
    }
}

struct StateNamespace {
    path: PathBuf,
    process_root: PathBuf,
}

impl Drop for StateNamespace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        try_reclaim_process_root(&self.process_root);
    }
}

thread_local! {
    static STATE_NAMESPACE: RefCell<Option<StateNamespace>> = const { RefCell::new(None) };
}

#[cfg(not(test))]
fn selected_state_path_mode() -> u8 {
    match STATE_PATH_SELECTION.compare_exchange(
        STATE_PATH_UNDECIDED,
        STATE_PATH_PRODUCTION,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) | Err(STATE_PATH_PRODUCTION) => STATE_PATH_PRODUCTION,
        Err(STATE_PATH_ISOLATED) => STATE_PATH_ISOLATED,
        Err(other) => panic!("invalid test state isolation selector state: {other}"),
    }
}

#[doc(hidden)]
pub fn enable_test_state_isolation() {
    #[cfg(not(test))]
    match STATE_PATH_SELECTION.compare_exchange(
        STATE_PATH_UNDECIDED,
        STATE_PATH_ISOLATED,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) | Err(STATE_PATH_ISOLATED) => {}
        Err(STATE_PATH_PRODUCTION) => {
            panic!("test state isolation must be enabled before state file I/O")
        }
        Err(other) => panic!("invalid test state isolation selector state: {other}"),
    }
}

fn state_file_path(file: StateFile) -> PathBuf {
    #[cfg(test)]
    {
        return isolated_state_file_path(file);
    }

    #[cfg(not(test))]
    match selected_state_path_mode() {
        STATE_PATH_PRODUCTION => file.production_path(),
        STATE_PATH_ISOLATED => isolated_state_file_path(file),
        other => panic!("invalid test state isolation selector state: {other}"),
    }
}

fn isolated_process_root() -> PathBuf {
    let root = PROCESS_ROOT.get_or_init(|| {
        let temp_dir = std::env::temp_dir();
        let pid = std::process::id();
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_else(|error| panic!("test state isolation clock failure: {error}"))
            .as_nanos();
        for collision in 0..64 {
            let nonce = PROCESS_ROOT_NONCE.fetch_add(1, Ordering::Relaxed);
            let path = temp_dir.join(format!(
                "rsi-test-state-{pid}-{started}-{nonce}-{collision}"
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return path,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!(
                    "test state isolation failed to create process root {}: {error}",
                    path.display()
                ),
            }
        }
        panic!(
            "test state isolation exhausted process-root collision budget in {}",
            temp_dir.display()
        );
    });
    root.clone()
}

fn create_isolated_namespace(process_root: &Path, namespace_id: u64) -> PathBuf {
    create_isolated_namespace_with(process_root, namespace_id, &PROCESS_ROOT_LIFECYCLE, || {})
}

fn create_isolated_namespace_with(
    process_root: &Path,
    namespace_id: u64,
    lifecycle: &ProcessRootLifecycle,
    on_reclaim: impl FnOnce(),
) -> PathBuf {
    let _creation = lifecycle.acquire_creation(on_reclaim);
    std::fs::create_dir_all(process_root).unwrap_or_else(|error| {
        panic!(
            "test state isolation failed to recreate process root {}: {error}",
            process_root.display()
        )
    });
    let namespace = process_root.join(format!("thread-{namespace_id}"));
    std::fs::create_dir(&namespace).unwrap_or_else(|error| {
        panic!(
            "test state isolation failed to create namespace {}: {error}",
            namespace.display()
        )
    });
    namespace
}

fn try_reclaim_process_root(process_root: &Path) {
    try_reclaim_process_root_with(process_root, &PROCESS_ROOT_LIFECYCLE, || {});
}

fn try_reclaim_process_root_with(
    process_root: &Path,
    lifecycle: &ProcessRootLifecycle,
    on_acquired: impl FnOnce(),
) {
    let Some(_reclaim) = lifecycle.try_acquire_reclaim() else {
        return;
    };
    on_acquired();
    if std::fs::read_dir(process_root)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
    {
        let _ = std::fs::remove_dir(process_root);
    }
}

fn isolated_state_file_path(file: StateFile) -> PathBuf {
    STATE_NAMESPACE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let process_root = isolated_process_root();
            let namespace = create_isolated_namespace(
                &process_root,
                THREAD_NAMESPACE_ID.fetch_add(1, Ordering::Relaxed),
            );
            *slot = Some(StateNamespace {
                path: namespace,
                process_root,
            });
        }
        slot.as_ref()
            .expect("state namespace was initialized")
            .path
            .join(file.relative_path())
    })
}

fn default_true() -> bool {
    true
}

/// Per-role message border color overrides. `None` per slot = use theme default.
/// Slots: [assistant, user, tool_unselected, tool_selected, normal cursor bg, insert cursor bg, visual selection bg]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BorderColorOverrides(pub [Option<[u8; 3]>; 7]);

impl Default for BorderColorOverrides {
    fn default() -> Self {
        Self([None; 7])
    }
}

impl BorderColorOverrides {
    /// Convert to the fixed-size array expected by `theme::apply_border_color_overrides`.
    pub fn as_array(&self) -> [Option<[u8; 3]>; 7] {
        self.0
    }
}

/// Committed semantic role overrides. Unknown or malformed entries are ignored
/// individually so one future role cannot invalidate the rest of state.json.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ThemeRoleOverrides(pub BTreeMap<String, [u8; 3]>);

impl<'de> Deserialize<'de> for ThemeRoleOverrides {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = BTreeMap::<String, serde_json::Value>::deserialize(deserializer)?;
        let mut overrides = BTreeMap::new();
        for (key, value) in raw {
            if crate::ui::theme_roles::ThemeRole::from_key(&key).is_none() {
                tracing::warn!(role = %key, "ignoring unknown persisted theme role");
                continue;
            }
            let Some(channels) = value.as_array().filter(|channels| channels.len() == 3) else {
                tracing::warn!(role = %key, "ignoring malformed persisted theme role color");
                continue;
            };
            let parsed = channels
                .iter()
                .map(|channel| channel.as_u64().and_then(|value| u8::try_from(value).ok()))
                .collect::<Option<Vec<_>>>();
            let Some(parsed) = parsed else {
                tracing::warn!(role = %key, "ignoring out-of-range persisted theme role color");
                continue;
            };
            overrides.insert(key, [parsed[0], parsed[1], parsed[2]]);
        }
        Ok(Self(overrides))
    }
}

impl ThemeRoleOverrides {
    pub fn capture() -> Self {
        Self(
            theme::snapshot_theme_role_overrides()
                .into_iter()
                .map(|(role, rgb)| (role.key().to_string(), rgb))
                .collect(),
        )
    }

    pub fn registered(&self) -> Vec<(crate::ui::theme_roles::ThemeRole, [u8; 3])> {
        self.0
            .iter()
            .filter_map(|(key, &rgb)| {
                crate::ui::theme_roles::ThemeRole::from_key(key).map(|role| (role, rgb))
            })
            .collect()
    }
}

/// Last-used values for the unified creation modal (P2.4).
///
/// Lifetime:
///  * Lives globally in `PersistedState` (one instance — not per-tab).
///  * Read on every modal open (P2.3) to prefill fields.
///  * Written on every successful modal submit; never written on cancel.
///  * The matching auto-draft (B7, P2.1) lives in `DevState` and is
///    independent — DevState clears on hot-reload load; ModalDefaults
///    survives restarts.
///
/// Cold-start semantics (per INDEX §3.2):
///  * `tags`: always `None` on first-ever modal open — forces user
///    attention. After first set, last-used value prefills.
///  * `topology_id`: `None` (no topology). Once set, persists.
///  * `kind`: respects descent context — see `cold_start_kind()` helper
///    below. After first set, last-used value prefills.
///  * `provider` / `model` / `effort`: existing TUI logic governs first-open
///    defaults; afterwards, last-used value prefills.
///  * `sandbox`: `false` by default; after first set, last-used.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModalDefaults {
    #[serde(default)]
    pub kind: Option<rsi_common::types::SessionKind>,
    #[serde(default)]
    pub topology_id: Option<Uuid>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub provider: Option<SessionProvider>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub sandbox: bool,
}

/// Compute the cold-start `Kind` for a fresh modal open, honoring the
/// descent context.
///
/// Per INDEX §3.2:
///  * root (None)             -> `legal_children(None)[0]`  (Standard)
///  * Group as descent head   -> `SessionKind::Epic`
///  * Epic as descent head    -> last-used leaf if any; else `SessionKind::Task`
///  * any leaf descent head   -> returns the descent-head kind (defensive)
///
/// Note: the caller is responsible for pre-filtering `last_used_leaf` via
/// `is_leaf_kind` — this helper trusts the input. See the Phase 3 prefill
/// site for the consumer-side filter.
pub fn cold_start_kind(
    descent_head_kind: Option<rsi_common::types::SessionKind>,
    last_used_leaf: Option<rsi_common::types::SessionKind>,
) -> rsi_common::types::SessionKind {
    use rsi_common::types::{SessionKind, legal_children};
    match descent_head_kind {
        None => legal_children(None)
            .first()
            .copied()
            .unwrap_or(SessionKind::Standard),
        Some(SessionKind::Group) => SessionKind::Epic,
        Some(SessionKind::Epic) => last_used_leaf.unwrap_or(SessionKind::Task),
        Some(other) => other,
    }
}

/// Persisted TUI state that survives restarts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedState {
    /// Current project filter. None = show all sessions.
    #[serde(default)]
    pub current_project_id: Option<Uuid>,

    /// Session list sort order.
    #[serde(default)]
    pub sort_order: SortOrder,

    // --- Navigation state (promoted from DevState for restart persistence) ---
    /// Session jumplist for Ctrl+O / Ctrl+I navigation.
    #[serde(default)]
    pub session_jumplist: Vec<Uuid>,

    /// Current position in the jumplist.
    #[serde(default)]
    pub jumplist_cursor: usize,

    /// Tab/split layout.
    #[serde(default)]
    pub tabs: Vec<Tab>,

    /// Active tab index.
    #[serde(default)]
    pub active_tab: usize,

    /// Next pane ID counter (monotonically increasing).
    #[serde(default)]
    pub next_pane_id: u64,

    /// Last session that was viewed in detail.
    #[serde(default)]
    pub last_viewed_session: Option<Uuid>,

    /// Selected model for new session launches.
    #[serde(default)]
    pub selected_model: Option<String>,
    /// Selected provider for new session launches.
    #[serde(default)]
    pub selected_provider: Option<SessionProvider>,
    /// Selected effort level for new reasoning-capable sessions.
    #[serde(default)]
    pub selected_effort: Option<String>,
    /// Selected theme key ("latte", "goth", "junkyard", ...).
    #[serde(default)]
    /// Selected theme key ("latte", "goth", "junkyard", ...).
    pub theme_flavor: Option<String>,

    /// User settings (display, defaults, status bar).
    #[serde(default)]
    pub settings: UserSettings,

    /// Per-session fold state for list cards. Key is session UUID string, value is expanded.
    #[serde(default)]
    pub session_fold_states: HashMap<String, bool>,

    /// Per-role message border color overrides (survives restarts).
    #[serde(default)]
    pub border_color_overrides: BorderColorOverrides,

    /// Per-theme semantic color overrides. Additive and safe for old files.
    #[serde(default)]
    pub theme_role_overrides: ThemeRoleOverrides,

    /// Per-modal geometry deltas keyed by PromptPurpose variant name.
    #[serde(default)]
    pub modal_geometries: HashMap<String, crate::types::ModalGeometry>,

    /// Last-used modal dropdown values (P2.4). See `ModalDefaults` doc.
    #[serde(default)]
    pub modal_defaults: ModalDefaults,
}

impl PersistedState {
    /// Capture current App state into a PersistedState for saving.
    pub fn capture(app: &crate::app::App) -> Self {
        let session_fold_states: HashMap<String, bool> = app
            .sessions
            .iter()
            .map(|(uuid, state)| (uuid.to_string(), state.list_card_expanded))
            .collect();

        Self {
            current_project_id: app.active_project_id(),
            sort_order: app.settings.sort_order,
            session_jumplist: app.session_jumplist.clone(),
            jumplist_cursor: app.jumplist_cursor,
            tabs: app.tabs.clone(),
            active_tab: app.active_tab,
            next_pane_id: app.next_pane_id,
            last_viewed_session: app.last_viewed_session,
            selected_model: app.selected_model.clone(),
            selected_provider: Some(app.selected_provider),
            selected_effort: app.selected_effort.clone(),
            theme_flavor: Some(theme::active_theme_key().to_string()),
            settings: app.settings.clone(),
            session_fold_states,
            border_color_overrides: {
                let arr = [
                    crate::ui::theme::get_border_color_override(0),
                    crate::ui::theme::get_border_color_override(1),
                    crate::ui::theme::get_border_color_override(2),
                    crate::ui::theme::get_border_color_override(3),
                    crate::ui::theme::get_border_color_override(4),
                    crate::ui::theme::get_border_color_override(5),
                    crate::ui::theme::get_border_color_override(6),
                ];
                BorderColorOverrides(arr)
            },
            theme_role_overrides: ThemeRoleOverrides::capture(),
            modal_geometries: app.modal_geometries.clone(),
            modal_defaults: app.modal_defaults.clone(),
        }
    }

    /// Load state from disk, returning default if file doesn't exist or is invalid.
    pub fn load() -> Self {
        let path = Self::state_path();
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Save state to disk. Errors are silently ignored (non-critical).
    pub fn save(&self) {
        let path = Self::state_path();

        // Ensure directory exists
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        if let Ok(content) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, content);
        }
    }

    /// Path to state file: ~/.rsi/state.json
    fn state_path() -> PathBuf {
        state_file_path(StateFile::Persisted)
    }
}

/// Serializable per-session view state (excludes events, caches, TextArea).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionViewState {
    pub scroll_offset: usize,
    pub collapsed_events: HashSet<i32>,
    pub expanded_events: HashSet<i32>,
    pub show_system_events: bool,
    #[serde(default = "default_true")]
    pub show_tool_results: bool,
    pub follow_tail: bool,
    pub input_bar_mode: PopupMode,
    pub input_bar_lines: Vec<String>,
    #[serde(default)]
    pub center_content: bool,
    /// Whether this session's list card is expanded (fold state).
    #[serde(default)]
    pub list_card_expanded: bool,
    /// File viewer state for hot-reload persistence.
    #[serde(default)]
    pub file_viewer_path: Option<String>,
    #[serde(default)]
    pub file_viewer_lines: Option<Vec<String>>,
    #[serde(default)]
    pub file_viewer_dirty: Option<bool>,
    #[serde(default)]
    pub file_viewer_mode: Option<PopupMode>,
}

/// Full UI snapshot for hot-reload. Saved on every exit, restored if fresh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevState {
    pub saved_at: i64,
    #[serde(default)]
    pub tabs: Vec<Tab>,
    #[serde(default)]
    pub active_tab: usize,
    #[serde(default)]
    pub next_pane_id: u64,
    #[serde(default)]
    pub session_views: HashMap<String, SessionViewState>,
    #[serde(default)]
    pub last_viewed_session: Option<Uuid>,
    #[serde(default)]
    pub selected_model: Option<String>,
    #[serde(default)]
    pub selected_provider: Option<SessionProvider>,
    #[serde(default)]
    pub selected_effort: Option<String>,
    #[serde(default)]
    pub current_project_id: Option<Uuid>,
    #[serde(default)]
    pub session_jumplist: Vec<Uuid>,
    #[serde(default)]
    pub jumplist_cursor: usize,
    #[serde(default)]
    pub sort_order: SortOrder,
    /// Selected theme key ("latte", "goth", "junkyard", ...).
    #[serde(default)]
    pub theme_flavor: Option<String>,
    #[serde(default)]
    pub settings: UserSettings,

    /// Graph editor drafts preserved across hot-reload.
    #[serde(default)]
    pub graph_drafts: Vec<crate::types::GraphDraft>,
    /// Most recently active graph draft.
    #[serde(default)]
    pub active_graph_draft_id: Option<Uuid>,

    /// Auto-drafted state of the unified entity-creation modal preserved
    /// across hot-reload (locked decision §B7).
    #[serde(default)]
    pub create_entity_draft: Option<crate::types::CreateEntityDraft>,
}

impl DevState {
    /// Snapshot current App state for serialization.
    pub fn capture(app: &crate::app::App) -> Self {
        let mut session_views = HashMap::new();
        for (uuid, state) in &app.sessions {
            let lines: Vec<String> = state
                .input_bar
                .surface
                .textarea
                .lines()
                .iter()
                .map(|s| s.to_string())
                .collect();
            let (fv_path, fv_lines, fv_dirty, fv_mode) = match &state.file_viewer {
                Some(viewer) => (
                    Some(viewer.file_path.display().to_string()),
                    Some(
                        viewer
                            .surface
                            .textarea
                            .lines()
                            .iter()
                            .map(|s| s.to_string())
                            .collect(),
                    ),
                    Some(viewer.dirty),
                    Some(viewer.surface.mode),
                ),
                None => (None, None, None, None),
            };

            session_views.insert(
                uuid.to_string(),
                SessionViewState {
                    scroll_offset: state.scroll_offset,
                    collapsed_events: state.collapsed_events.clone(),
                    expanded_events: state.expanded_events.clone(),
                    show_system_events: state.show_system_events,
                    show_tool_results: state.show_tool_results,
                    follow_tail: state.follow_tail,
                    input_bar_mode: state.input_bar.surface.mode,
                    input_bar_lines: lines,
                    center_content: state.center_content,
                    list_card_expanded: state.list_card_expanded,
                    file_viewer_path: fv_path,
                    file_viewer_lines: fv_lines,
                    file_viewer_dirty: fv_dirty,
                    file_viewer_mode: fv_mode,
                },
            );
        }

        Self {
            saved_at: chrono::Utc::now().timestamp(),
            tabs: app.tabs.clone(),
            active_tab: app.active_tab,
            next_pane_id: app.next_pane_id,
            session_views,
            last_viewed_session: app.last_viewed_session,
            selected_model: app.selected_model.clone(),
            selected_provider: Some(app.selected_provider),
            selected_effort: app.selected_effort.clone(),
            current_project_id: app.active_project_id(),
            session_jumplist: app.session_jumplist.clone(),
            jumplist_cursor: app.jumplist_cursor,
            sort_order: app.settings.sort_order,
            theme_flavor: Some(theme::active_theme_key().to_string()),
            settings: app.settings.clone(),
            graph_drafts: app.graph_drafts.values().cloned().collect(),
            active_graph_draft_id: app.active_graph_draft_id,
            create_entity_draft: app.create_entity_draft.clone(),
        }
    }

    /// Write snapshot to ~/.flywheel/dev-state.json.
    pub fn save(&self) {
        let path = Self::state_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(content) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, content);
        }
    }

    /// Load and validate: returns None if missing, unparseable, or stale (>10s).
    pub fn load() -> Option<Self> {
        let path = Self::state_path();
        let content = std::fs::read_to_string(&path).ok()?;
        let state: Self = serde_json::from_str(&content).ok()?;
        let now = chrono::Utc::now().timestamp();
        if now - state.saved_at > 10 {
            return None;
        }
        Some(state)
    }

    /// Delete the dev-state file after successful consumption.
    pub fn clear() {
        let _ = std::fs::remove_file(Self::state_path());
    }

    fn state_path() -> PathBuf {
        state_file_path(StateFile::Dev)
    }
}

/// Emergency graph draft persistence for crash recovery and daemon loss.
///
/// Unlike DevState (10-second hot-reload window), emergency drafts survive
/// indefinitely and are loaded on startup when no fresh DevState is available.
pub mod emergency_drafts {
    use crate::types::{GraphDraft, GraphDraftPersistenceState};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use uuid::Uuid;

    #[derive(Debug, Default)]
    pub struct LoadedEmergencyDrafts {
        pub active_graph_draft_id: Option<Uuid>,
        pub drafts: HashMap<Uuid, GraphDraft>,
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    struct EmergencyDraftEnvelope {
        #[serde(default)]
        active_graph_draft_id: Option<Uuid>,
        #[serde(default)]
        drafts: Vec<GraphDraft>,
    }

    #[derive(serde::Deserialize)]
    struct LegacyGraphDraftKey {
        #[serde(default)]
        workflow_id: Option<Uuid>,
        #[serde(default)]
        project_id: Option<Uuid>,
    }

    #[derive(serde::Deserialize)]
    struct LegacyGraphDraft {
        key: LegacyGraphDraftKey,
        workflow: rsi_graph::format::WorkflowDefinition,
        #[serde(default)]
        dirty: bool,
        #[serde(default)]
        last_execution_id: Option<Uuid>,
    }

    fn drafts_path() -> PathBuf {
        super::state_file_path(super::StateFile::EmergencyDrafts)
    }

    /// Save all dirty graph drafts to `~/.flywheel/graph-drafts/emergency-drafts.json`.
    ///
    /// Called at TUI shutdown. Only persists drafts that are not currently clean.
    /// If no drafts are dirty, removes any stale emergency file.
    pub fn save(active_graph_draft_id: Option<Uuid>, drafts: &HashMap<Uuid, GraphDraft>) {
        let dirty: Vec<GraphDraft> = drafts
            .values()
            .filter(|draft| draft.persistence_state.needs_emergency_save())
            .cloned()
            .collect();
        let path = drafts_path();

        if dirty.is_empty() {
            // No dirty drafts — clean up stale emergency file
            let _ = std::fs::remove_file(&path);
            return;
        }

        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let payload = EmergencyDraftEnvelope {
            active_graph_draft_id: active_graph_draft_id
                .filter(|draft_id| dirty.iter().any(|draft| draft.draft_id == *draft_id)),
            drafts: dirty,
        };
        if let Ok(content) = serde_json::to_string_pretty(&payload) {
            let _ = std::fs::write(&path, content);
        }
    }

    /// Load emergency graph drafts from `~/.flywheel/graph-drafts/emergency-drafts.json`.
    ///
    /// Returns drafts keyed by their identity. Removes the emergency file after
    /// successful load so drafts are not restored twice.
    pub fn load() -> LoadedEmergencyDrafts {
        let path = drafts_path();
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return LoadedEmergencyDrafts::default(),
        };

        let loaded = serde_json::from_str::<EmergencyDraftEnvelope>(&content)
            .map(|payload| LoadedEmergencyDrafts {
                active_graph_draft_id: payload.active_graph_draft_id,
                drafts: payload
                    .drafts
                    .into_iter()
                    .map(GraphDraft::normalize_after_restore)
                    .map(|draft| (draft.draft_id, draft))
                    .collect(),
            })
            .or_else(|_| {
                serde_json::from_str::<Vec<GraphDraft>>(&content).map(|drafts| {
                    LoadedEmergencyDrafts {
                        active_graph_draft_id: None,
                        drafts: drafts
                            .into_iter()
                            .map(GraphDraft::normalize_after_restore)
                            .map(|draft| (draft.draft_id, draft))
                            .collect(),
                    }
                })
            })
            .or_else(|_| {
                serde_json::from_str::<Vec<LegacyGraphDraft>>(&content).map(|drafts| {
                    let drafts = drafts
                        .into_iter()
                        .map(|legacy| {
                            let mut draft = GraphDraft::new(
                                legacy.workflow,
                                legacy.key.workflow_id,
                                legacy.key.project_id,
                            )
                            .with_persistence_state(if legacy.dirty {
                                GraphDraftPersistenceState::Dirty
                            } else {
                                GraphDraftPersistenceState::Clean
                            });
                            draft.last_execution_id = legacy.last_execution_id;
                            (draft.draft_id, draft)
                        })
                        .collect();
                    LoadedEmergencyDrafts {
                        active_graph_draft_id: None,
                        drafts,
                    }
                })
            });

        let Ok(mut loaded) = loaded else {
            return LoadedEmergencyDrafts::default();
        };

        // Successfully loaded — remove the file so we don't restore stale drafts next time.
        let _ = std::fs::remove_file(&path);

        loaded.active_graph_draft_id = loaded
            .active_graph_draft_id
            .filter(|draft_id| loaded.drafts.contains_key(draft_id))
            .or_else(|| loaded.drafts.keys().next().copied());

        loaded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_state_paths_preserve_identity_resolver_results() {
        assert_eq!(
            StateFile::Persisted.production_path(),
            rsi_common::identity::data_path("state.json", "state.json")
        );
        assert_eq!(
            StateFile::Dev.production_path(),
            rsi_common::identity::data_path("dev-state.json", "dev-state.json")
        );
        assert_eq!(
            StateFile::EmergencyDrafts.production_path(),
            rsi_common::identity::data_path(
                "graph-drafts/emergency-drafts.json",
                "emergency-drafts.json"
            )
        );
    }

    #[test]
    fn unit_tests_select_an_isolated_state_namespace_automatically() {
        let path = state_file_path(StateFile::Persisted);
        assert!(
            path.starts_with(isolated_process_root()),
            "unit test state path escaped isolated root: {}",
            path.display()
        );
    }

    #[test]
    fn state_namespaces_are_thread_local_and_reclaimed() {
        use crate::types::{GraphDraft, GraphDraftPersistenceState};
        use std::sync::{Arc, Barrier, mpsc};

        let barrier = Arc::new(Barrier::new(2));
        let (sender, receiver) = mpsc::channel();
        let mut workers = Vec::new();
        for label in ["first", "second"] {
            let barrier = Arc::clone(&barrier);
            let sender = sender.clone();
            workers.push(std::thread::spawn(move || {
                let persisted = PersistedState::state_path();
                let dev = DevState::state_path();
                let emergency = state_file_path(StateFile::EmergencyDrafts);
                for path in [&persisted, &dev, &emergency] {
                    std::fs::create_dir_all(path.parent().expect("state file has parent")).unwrap();
                }
                std::fs::write(&persisted, label).unwrap();
                std::fs::write(&dev, label).unwrap();
                let draft = GraphDraft::new(
                    rsi_graph::format::WorkflowDefinition::new(label),
                    None,
                    None,
                )
                .with_persistence_state(GraphDraftPersistenceState::Dirty);
                let draft_id = draft.draft_id;
                emergency_drafts::save(None, &HashMap::from([(draft_id, draft)]));
                barrier.wait();
                assert_eq!(std::fs::read_to_string(&persisted).unwrap(), label);
                assert_eq!(std::fs::read_to_string(&dev).unwrap(), label);
                assert!(emergency_drafts::load().drafts.contains_key(&draft_id));
                sender.send((persisted, dev, emergency)).unwrap();
            }));
        }
        drop(sender);
        let paths: Vec<_> = receiver.iter().collect();
        for worker in workers {
            worker.join().expect("state isolation worker panicked");
        }
        assert_ne!(paths[0].0.parent(), paths[1].0.parent());
        for (persisted, dev, emergency) in paths {
            assert!(
                !persisted.exists(),
                "persisted file survived: {}",
                persisted.display()
            );
            assert!(!dev.exists(), "dev file survived: {}", dev.display());
            assert!(
                !emergency.exists(),
                "draft file survived: {}",
                emergency.display()
            );
            assert!(
                !persisted.parent().expect("namespace parent").exists(),
                "namespace survived thread exit: {}",
                persisted.parent().expect("namespace parent").display()
            );
        }
    }

    #[test]
    fn process_root_teardown_cannot_race_later_first_resolution() {
        use std::sync::mpsc;

        let root = std::env::temp_dir().join(format!(
            "rsi-test-state-lifecycle-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir(&root).unwrap_or_else(|error| {
            panic!(
                "failed to create lifecycle regression root {}: {error}",
                root.display()
            )
        });
        let lifecycle = std::sync::Arc::new(ProcessRootLifecycle::new());

        let (reclaim_acquired_tx, reclaim_acquired_rx) = mpsc::channel();
        let (allow_reclaim_tx, allow_reclaim_rx) = mpsc::channel();
        let reclaimer_root = root.clone();
        let reclaimer_lifecycle = std::sync::Arc::clone(&lifecycle);
        let reclaimer = std::thread::spawn(move || {
            try_reclaim_process_root_with(&reclaimer_root, &reclaimer_lifecycle, || {
                reclaim_acquired_tx.send(()).unwrap();
                allow_reclaim_rx.recv().unwrap();
            });
        });
        reclaim_acquired_rx.recv().unwrap();

        let (creator_blocked_tx, creator_blocked_rx) = mpsc::channel();
        let creator_root = root.clone();
        let creator_lifecycle = std::sync::Arc::clone(&lifecycle);
        let creator = std::thread::spawn(move || {
            create_isolated_namespace_with(&creator_root, u64::MAX, &creator_lifecycle, || {
                creator_blocked_tx.send(()).unwrap();
            })
        });
        creator_blocked_rx.recv().unwrap();
        allow_reclaim_tx.send(()).unwrap();

        reclaimer.join().expect("process-root reclaimer panicked");
        let namespace = creator.join().expect("namespace creator panicked");
        assert!(
            namespace.exists(),
            "later namespace was not created after root teardown: {}",
            namespace.display()
        );
        assert_eq!(namespace.parent(), Some(root.as_path()));
        std::fs::remove_dir_all(&root).unwrap_or_else(|error| {
            panic!(
                "failed to clean lifecycle regression root {}: {error}",
                root.display()
            )
        });
    }

    #[test]
    fn state_namespace_is_stable_for_one_thread() {
        let first = state_file_path(StateFile::Persisted);
        let second = state_file_path(StateFile::Dev);
        let third = state_file_path(StateFile::EmergencyDrafts);
        assert_eq!(first.parent(), second.parent());
        assert_eq!(
            first.parent(),
            third.parent().and_then(std::path::Path::parent)
        );
    }

    #[test]
    fn test_persisted_state_default() {
        let state = PersistedState::default();
        assert!(state.current_project_id.is_none());
    }

    #[test]
    fn test_persisted_state_serde_roundtrip() {
        let project_id = Uuid::new_v4();
        let state = PersistedState {
            current_project_id: Some(project_id),
            sort_order: SortOrder::FreshestFirst,
            ..Default::default()
        };

        let json = serde_json::to_string(&state).unwrap();
        let deserialized: PersistedState = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.current_project_id, Some(project_id));
        assert_eq!(deserialized.sort_order, SortOrder::FreshestFirst);
    }

    #[test]
    fn test_persisted_state_missing_field_defaults() {
        // JSON with no fields should deserialize with defaults
        let json = "{}";
        let state: PersistedState = serde_json::from_str(json).unwrap();
        assert!(state.current_project_id.is_none());
        assert_eq!(state.sort_order, SortOrder::StalestFirst);
    }

    #[test]
    fn test_persisted_state_with_navigation_fields() {
        let state = PersistedState {
            current_project_id: None,
            sort_order: SortOrder::FreshestFirst,
            session_jumplist: vec![Uuid::new_v4(), Uuid::new_v4()],
            jumplist_cursor: 1,
            tabs: vec![],
            active_tab: 0,
            next_pane_id: 5,
            last_viewed_session: Some(Uuid::new_v4()),
            selected_model: Some("claude-opus-4-6".to_string()),
            selected_provider: Some(SessionProvider::Claude),
            selected_effort: None,
            theme_flavor: Some("latte".to_string()),
            settings: UserSettings::default(),
            session_fold_states: HashMap::new(),
            border_color_overrides: BorderColorOverrides::default(),
            theme_role_overrides: ThemeRoleOverrides::default(),
            modal_geometries: HashMap::new(),
            modal_defaults: ModalDefaults::default(),
        };

        let json = serde_json::to_string(&state).unwrap();
        let restored: PersistedState = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.session_jumplist.len(), 2);
        assert_eq!(restored.jumplist_cursor, 1);
        assert_eq!(restored.next_pane_id, 5);
        assert!(restored.last_viewed_session.is_some());
        assert_eq!(restored.selected_model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(restored.theme_flavor.as_deref(), Some("latte"));
    }

    #[test]
    fn test_persisted_state_backward_compat_no_new_fields() {
        // Simulate loading an old state.json that only has the original 3 fields
        let json = r#"{"sort_order":"NewestFirst"}"#;
        let state: PersistedState = serde_json::from_str(json).unwrap();

        assert!(state.session_jumplist.is_empty());
        assert_eq!(state.jumplist_cursor, 0);
        assert!(state.tabs.is_empty());
        assert_eq!(state.active_tab, 0);
        assert_eq!(state.next_pane_id, 0);
        assert!(state.last_viewed_session.is_none());
        assert!(state.selected_model.is_none());
        assert!(state.theme_role_overrides.0.is_empty());
    }

    #[test]
    fn semantic_theme_overrides_round_trip_and_filter_unknown_entries() {
        let state = PersistedState {
            theme_role_overrides: ThemeRoleOverrides(BTreeMap::from([
                ("accent".to_string(), [1, 2, 3]),
                ("error".to_string(), [4, 5, 6]),
            ])),
            ..PersistedState::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        let restored: PersistedState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.theme_role_overrides, state.theme_role_overrides);

        let tolerant: PersistedState = serde_json::from_str(
            r##"{
                "theme_flavor":"goth",
                "theme_role_overrides":{
                    "accent":[9,8,7],
                    "future_role":[1,2,3],
                    "error":[999,0,0],
                    "warning":"#ffffff"
                }
            }"##,
        )
        .unwrap();
        assert_eq!(
            tolerant.theme_role_overrides.0,
            BTreeMap::from([("accent".to_string(), [9, 8, 7])])
        );
    }

    #[test]
    fn test_dev_state_serde_roundtrip() {
        use crate::types::{Pane, PaneId, SplitNode};

        let mut session_views = HashMap::new();
        session_views.insert(
            Uuid::nil().to_string(),
            SessionViewState {
                scroll_offset: 42,
                collapsed_events: HashSet::from([1, 3, 5]),
                expanded_events: HashSet::from([2]),
                show_system_events: true,
                show_tool_results: true,
                follow_tail: false,
                input_bar_mode: PopupMode::Insert,
                input_bar_lines: vec!["draft text".to_string(), "line 2".to_string()],
                center_content: true,
                file_viewer_path: None,
                file_viewer_lines: None,
                file_viewer_dirty: None,
                file_viewer_mode: None,
                list_card_expanded: true,
            },
        );

        let state = DevState {
            saved_at: chrono::Utc::now().timestamp(),
            tabs: vec![Tab {
                name: "[1]".to_string(),
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
                project_id: None,
                detail_split_offset: 0,
                layout_x_offset_adj: 0,
                session_list_width_pct: 0,
                descent_path: Vec::new(),
                bottom_focus_target: crate::types::BottomZone::Off,
                mini_dag_focus: None,
            }],
            active_tab: 0,
            next_pane_id: 1,
            session_views,
            last_viewed_session: Some(Uuid::nil()),
            selected_model: Some("claude-opus-4-6".to_string()),
            selected_provider: Some(SessionProvider::Claude),
            selected_effort: None,
            current_project_id: None,
            session_jumplist: vec![Uuid::nil()],
            jumplist_cursor: 0,
            sort_order: SortOrder::FreshestFirst,
            theme_flavor: Some("macchiato".to_string()),
            settings: UserSettings::default(),
            graph_drafts: Vec::new(),
            active_graph_draft_id: None,
            create_entity_draft: None,
        };

        let json = serde_json::to_string(&state).unwrap();
        let restored: DevState = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.tabs.len(), 1);
        assert_eq!(restored.active_tab, 0);
        assert_eq!(restored.next_pane_id, 1);
        assert_eq!(restored.last_viewed_session, Some(Uuid::nil()));
        assert_eq!(restored.selected_model.as_deref(), Some("claude-opus-4-6"));

        let view = &restored.session_views[&Uuid::nil().to_string()];
        assert_eq!(view.scroll_offset, 42);
        assert!(view.collapsed_events.contains(&3));
        assert!(view.show_system_events);
        assert!(!view.follow_tail);
        assert_eq!(view.input_bar_mode, PopupMode::Insert);
        assert_eq!(view.input_bar_lines.len(), 2);

        assert_eq!(restored.sort_order, SortOrder::FreshestFirst);
    }

    #[test]
    fn test_dev_state_staleness() {
        let stale = DevState {
            saved_at: chrono::Utc::now().timestamp() - 15, // 15 seconds ago
            tabs: vec![],
            active_tab: 0,
            next_pane_id: 0,
            session_views: HashMap::new(),
            last_viewed_session: None,
            selected_model: None,
            selected_provider: None,
            selected_effort: None,
            current_project_id: None,
            session_jumplist: Vec::new(),
            jumplist_cursor: 0,
            sort_order: SortOrder::default(),
            theme_flavor: Some("mocha".to_string()),
            settings: UserSettings::default(),
            graph_drafts: Vec::new(),
            active_graph_draft_id: None,
            create_entity_draft: None,
        };

        // Write then try to load — should be None due to staleness
        let path = DevState::state_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let json = serde_json::to_string(&stale).unwrap();
        let _ = std::fs::write(&path, &json);

        assert!(DevState::load().is_none(), "stale state should return None");

        // Clean up
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_dev_state_missing_field_defaults() {
        // Minimal JSON with only saved_at — all other fields should default
        let json = format!(r#"{{"saved_at": {}}}"#, chrono::Utc::now().timestamp());
        let state: DevState = serde_json::from_str(&json).unwrap();
        assert!(state.tabs.is_empty());
        assert_eq!(state.active_tab, 0);
        assert_eq!(state.next_pane_id, 0);
        assert!(state.session_views.is_empty());
        assert!(state.last_viewed_session.is_none());
        assert!(state.selected_model.is_none());
        assert!(state.current_project_id.is_none());
    }

    #[test]
    fn tab_descent_persistence_roundtrip() {
        use crate::types::{Pane, PaneId, SplitNode};
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let tab = crate::types::Tab {
            name: "test".to_string(),
            session_list_state: crate::types::Tab::default_session_list_state(),
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
            project_id: None,
            detail_split_offset: 0,
            layout_x_offset_adj: 0,
            session_list_width_pct: 0,
            descent_path: vec![id1, id2],
            bottom_focus_target: crate::types::BottomZone::Off,
            mini_dag_focus: None,
        };
        let state = PersistedState {
            tabs: vec![tab],
            ..Default::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        let restored: PersistedState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.tabs.len(), 1);
        assert_eq!(restored.tabs[0].descent_path, vec![id1, id2]);
    }

    #[test]
    fn tab_descent_path_defaults_empty_on_old_json() {
        // Old state JSON without descent_path field should deserialize to empty vec
        let json = r#"{"tabs":[{"name":"[1]","layout":{"Leaf":{"pane":{"SessionList":{"selected_index":0,"selected_session":null}},"id":0}},"focused_pane":0}]}"#;
        let state: PersistedState = serde_json::from_str(json).unwrap();
        assert_eq!(state.tabs.len(), 1);
        assert!(
            state.tabs[0].descent_path.is_empty(),
            "descent_path should default to empty vec"
        );
    }

    mod modal_defaults {
        use super::*;
        use rsi_common::types::SessionKind;

        #[test]
        fn cold_start_tag_is_none() {
            assert!(ModalDefaults::default().tags.is_none());
        }

        #[test]
        fn cold_start_topology_id_is_none() {
            assert!(ModalDefaults::default().topology_id.is_none());
        }

        #[test]
        fn cold_start_kind_root_is_standard() {
            assert_eq!(cold_start_kind(None, None), SessionKind::Standard);
        }

        #[test]
        fn cold_start_kind_group_descent_is_epic() {
            assert_eq!(
                cold_start_kind(Some(SessionKind::Group), None),
                SessionKind::Epic
            );
        }

        #[test]
        fn cold_start_kind_epic_descent_with_last_leaf() {
            assert_eq!(
                cold_start_kind(Some(SessionKind::Epic), Some(SessionKind::Bug)),
                SessionKind::Bug
            );
        }

        #[test]
        fn cold_start_kind_epic_descent_no_history_is_task() {
            assert_eq!(
                cold_start_kind(Some(SessionKind::Epic), None),
                SessionKind::Task
            );
        }

        #[test]
        fn cold_start_kind_ignores_non_leaf_last_used() {
            use rsi_common::types::is_leaf_kind;
            // Helper itself is not filter-aware: passes through whatever caller gave.
            assert_eq!(
                cold_start_kind(Some(SessionKind::Epic), Some(SessionKind::Epic)),
                SessionKind::Epic
            );
            // Consumer-side filter (the contract Phase 3 prefill enforces):
            let filtered = Some(SessionKind::Epic).filter(|k| is_leaf_kind(*k));
            assert_eq!(filtered, None);
            assert_eq!(
                cold_start_kind(Some(SessionKind::Epic), filtered),
                SessionKind::Task
            );
        }

        #[test]
        fn serde_roundtrip_full() {
            use rsi_common::types::{SessionKind, SessionProvider};
            let state = PersistedState {
                modal_defaults: ModalDefaults {
                    kind: Some(SessionKind::Task),
                    topology_id: Some(Uuid::new_v4()),
                    tags: Some(vec!["foo".into(), "bar".into()]),
                    provider: Some(SessionProvider::Codex),
                    model: Some("gpt-5".into()),
                    effort: Some("high".into()),
                    sandbox: true,
                },
                ..Default::default()
            };
            let json = serde_json::to_string(&state).unwrap();
            let restored: PersistedState = serde_json::from_str(&json).unwrap();
            assert_eq!(restored.modal_defaults, state.modal_defaults);
        }

        #[test]
        fn tag_prefill_after_first_set() {
            let defaults = ModalDefaults {
                tags: Some(vec!["foo".into(), "bar".into()]),
                ..Default::default()
            };
            // Mirror the Phase 3 prefill mapping.
            let chips: Vec<_> = defaults
                .tags
                .clone()
                .map(|vs| vs.into_iter().map(|v| (v, "Committed")).collect::<Vec<_>>())
                .unwrap_or_default();
            assert_eq!(chips.len(), 2);
            assert_eq!(chips[0].0, "foo");
            assert!(
                !chips.is_empty(),
                "B7 red border should NOT trigger after first set"
            );
        }

        #[test]
        fn legacy_state_json_missing_field_defaults() {
            // Mirror test_persisted_state_backward_compat_no_new_fields.
            // Confirm pre-P2.4 state.json files load cleanly.
            let json = r#"{"sort_order":"NewestFirst"}"#;
            let state: PersistedState = serde_json::from_str(json).unwrap();
            assert_eq!(state.modal_defaults, ModalDefaults::default());
        }

        #[test]
        fn post_submit_values_persist_via_capture() {
            // Simulate the Phase 4 write-back's two-step pattern (mutate +
            // capture/save round-trip), but skip the actual disk save by going
            // through the serde layer directly.
            use rsi_common::types::{SessionKind, SessionProvider};
            let defaults = ModalDefaults {
                kind: Some(SessionKind::Task),
                topology_id: None,
                tags: Some(vec!["alpha".into()]),
                provider: Some(SessionProvider::Codex),
                model: Some("gpt-5".into()),
                effort: Some("high".into()),
                sandbox: true,
            };
            let state = PersistedState {
                modal_defaults: defaults.clone(),
                ..Default::default()
            };
            let json = serde_json::to_string(&state).unwrap();
            let reloaded: PersistedState = serde_json::from_str(&json).unwrap();
            assert_eq!(reloaded.modal_defaults, defaults);
        }

        #[test]
        fn partial_serde_individual_field_defaults() {
            // Confirm each per-field `#[serde(default)]` works in isolation —
            // critical for forward-compat in case a future ticket adds a field
            // to ModalDefaults and existing state.json files only carry the
            // current 7 fields.
            let json = r#"{"kind":"Task"}"#;
            let defaults: ModalDefaults = serde_json::from_str(json).unwrap();
            assert_eq!(defaults.kind, Some(rsi_common::types::SessionKind::Task));
            assert!(defaults.topology_id.is_none());
            assert!(defaults.tags.is_none());
            assert!(defaults.provider.is_none());
            assert!(defaults.model.is_none());
            assert!(defaults.effort.is_none());
            assert!(!defaults.sandbox);
        }
    }
}
