//! modalkit integration types for flywheel.
//!
//! Defines the ApplicationInfo implementation that connects flywheel's
//! custom actions to modalkit's vim keybinding infrastructure.

use std::fmt;

use editor_types::application::{
    ApplicationAction, ApplicationContentId, ApplicationError, ApplicationInfo, ApplicationStore,
    ApplicationWindowId,
};
use editor_types::prelude::CommandType;
use keybindings::SequenceStatus;
use modalkit::editing::context::EditContext;

/// Vim insert-mode entry style, mirroring standard vim semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InsertStyle {
    /// `i` — insert before cursor.
    Insert,
    /// `a` — append after cursor.
    Append,
    /// `o` — open line below and enter insert mode.
    OpenBelow,
    /// `O` — open line above and enter insert mode.
    OpenAbove,
}

/// Application-specific actions beyond standard vim operations.
///
/// These are triggered by custom keybindings or custom `:` commands.
/// Standard vim operations (splits, tabs, scrolling, command bar) are
/// handled by modalkit's Action::Tab, Action::Window, Action::Scroll, etc.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LcAction {
    // --- Session Lifecycle ---
    /// Interrupt the session under the cursor.
    InterruptSession,

    /// Continue a completed/interrupted session with a follow-up query.
    ContinueSession(Option<String>),

    /// Quick-continue: send "continue" to idle session in detail view.
    QuickContinue,

    // --- View Navigation ---
    /// Enter the session detail view for the session under cursor.
    EnterSession,

    /// Return to the session list from a detail view.
    BackToList,

    // --- Focus & Attention ---
    /// Open the question modal for the focused/selected WaitingApproval session
    OpenQuestionModal,

    /// Jump to the next session that needs attention.
    NextAttention,

    /// Jump to the previous session that needs attention.
    PrevAttention,

    /// Toggle the notification history overlay.
    ToggleNotifications,

    /// Dismiss all notifications.
    DismissAllNotifications,

    /// List all sessions (switch focused pane to session list).
    ListSessions,

    // --- Detail View Navigation ---
    /// Jump to next conversation event boundary in detail view.
    NextEvent,

    /// Jump to previous conversation event boundary in detail view.
    PrevEvent,

    /// Jump to next user message in detail view.
    NextUserMessage,

    /// Jump to previous user message in detail view.
    PrevUserMessage,

    // --- Fold Operations ---
    /// Open fold on current event (expand collapsed/truncated).
    OpenFold,

    /// Close fold on current event (collapse tool event / re-truncate).
    CloseFold,

    /// Toggle fold on current event.
    ToggleFold,

    /// Close all folds (collapse all tool events + truncate all).
    CloseAllFolds,

    /// Open all folds (expand everything).
    OpenAllFolds,

    /// Toggle system event visibility in session detail view.
    ToggleSystemEvents,

    /// Toggle thinking event visibility in session detail view.
    ToggleThinkingEvents,

    // --- Session Management ---
    /// Delete the selected session (completed/failed/interrupted only).
    DeleteSession,

    /// Archive the selected session (soft delete — hides from list, preserves data).
    ArchiveSession,

    /// Toggle pin on the selected session (keeps it at top of list).
    TogglePinSession,

    /// Toggle "manual testing needed" marker on the selected session.
    ToggleTestingNeeded,

    /// Toggle auto-rotation disabled on the selected session.
    ToggleRotationDisabled,

    /// Cancel a pending automatic retry on the focused session.
    CancelRetry,

    /// Open the session-rating overlay for the focused session.
    OpenRatingOverlay,

    /// Open the consolidated session info panel for the focused session.
    OpenSessionInfoPanel,

    /// Rate the focused session on a 1–10 scale (dispatched directly from `:rate N`).
    RateSession(u32),

    /// Open project picker to reassign the focused session's project.
    ReassignSessionProject,

    /// Manually trigger context rotation on the focused session.
    RotateSession,

    /// Open the sort order picker overlay.
    OpenSortPicker,

    /// Open the diagnostics overlay (render/poll timings, cache stats).
    OpenDiagnostics,

    /// Open the read-only recursive DAG browser overlay.
    OpenRecursiveDagBrowser,

    /// Open the memory search overlay.
    OpenMemorySearch,

    /// Open the operator-only source-worktree settlement audit/apply overlay.
    OpenSourceWorktreeSettlement,

    // --- Model Selection ---
    /// Toggle the model dropdown widget below the status bar (or prompt model badge).
    ToggleModelDropdown,

    /// Select a model (None = clear model selection).
    SelectModel(Option<String>),

    /// Open the theme picker overlay.
    OpenThemePicker,

    /// Open the color customizer overlay for setting per-role message border colors.
    OpenColorCustomizer,

    /// Select a Catppuccin flavor by name.
    SelectTheme(String),

    // --- Project Navigation ---
    /// Open the project picker overlay (opens/focuses workspace).
    OpenProjectPicker,

    /// Switch to a project by name (opens/focuses workspace).
    SwitchProject(String),

    /// Create a project from command mode.
    CreateProject { name: String, path: Option<String> },

    /// Open the edit form for a project by name.
    EditProjectByName(String),

    /// Delete a project by name.
    DeleteProjectByName(String),

    // --- Tab Operations ---
    /// Open the selected session in a new tab (detail view).
    OpenSessionInNewTab,

    /// Switch to the next tab (`>` key — always switches tab regardless of context).
    NextTab,

    /// Switch to the previous tab (`<` key — always switches tab regardless of context).
    PrevTab,

    // --- Context-Sensitive Navigation ---
    /// l key: enter session detail from list, or next tab from detail.
    NavigateRight,

    // --- Input Bar ---
    /// Enter insert mode in the focused session detail's input bar.
    /// If in session list, navigate to last viewed session and enter insert.
    /// The InsertStyle determines cursor positioning (i/a/o/O vim semantics).
    EnterInputBarInsert(InsertStyle),

    /// Enter session detail in normal mode (like EnterSession but context-aware).
    /// If in session list, navigate to selected or last viewed session.
    /// If already in session detail, no-op (stay in normal mode).
    EnterSessionNormalMode,

    /// Toggle the keybindings help overlay.
    ToggleKeybindingsHelp,
    /// Open the complete command reference.
    OpenAllCommands,
    /// Open the generated manual in the requested format.
    OpenManual(crate::manual::open::ManualMode),

    /// Toggle the prompt preview overlay (shows full query for selected session).
    TogglePromptPreview,

    // --- DocRegBlock Execution ---
    /// Execute all detected <docregblock> tags in the current session.
    ExecuteDocRegBlocks,

    /// Continue session with /ci_commit to commit and push changes.
    CommitAndPush,

    // --- Jumplist Navigation ---
    /// Jump backward in session history (Ctrl+O).
    JumpBack,

    /// Jump forward in session history (Ctrl+I).
    JumpForward,

    // --- TaskRabbit ---
    /// Open the TaskRabbit popup.
    TaskRabbitPrompt,

    /// Launch a TaskRabbit session with inline query (from :task <query>).
    LaunchTaskRabbit(String),

    // --- Durable Work Context ---
    /// Set or clear the active task context for the focused session.
    /// Some(text) sets it, None clears it.
    SetActiveTask(Option<String>),

    // --- Blank ---
    /// Open the Blank general-purpose prompt popup (empty, no pre-filled commands).
    BlankPrompt,

    /// Launch a Blank session with the given query (from :blank <query>).
    LaunchBlank(String),

    // --- Pane Management ---
    /// Close the focused pane. Triggered by <Space>qq.
    CloseFocusedPane,

    // --- Archive Zone ---
    /// Switch to the archive zone in the session list.
    GoToArchiveZone,

    /// Open the trash browser overlay (soft-deleted sessions).
    GoToTrash,

    // --- Direct Zone Navigation ---
    /// Switch to the sessions (main) zone in the session list.
    GoToSessionsZone,

    /// Switch to the TaskRabbit zone in the session list.
    GoToTaskRabbitZone,

    /// Switch to the jobs zone in the session list.
    GoToJobsZone,

    /// Unarchive the selected session in the archive browser.
    UnarchiveSession,

    // --- Recent Completions ---
    /// Focus the recent completions section in the right sidebar.
    OpenRecentCompletions,

    // --- Settings ---
    /// Open settings pane (replaces current pane content).
    OpenSettings,

    /// Open the settings pane and jump straight to a specific category in
    /// `Items` focus. Used by `:hooks` / `:skills` ex-commands so the user
    /// lands on the right panel without an extra `j/l` chord.
    OpenSettingsAt(crate::settings_registry::SettingsSection),

    // --- Yank ---
    /// Copy the content of the event under the cursor to the system clipboard (OSC 52).
    YankEventContent,

    // --- Search ---
    /// Enter / search mode.
    EnterSearch,

    /// Jump to next search match in session detail.
    NextSearchMatch,

    /// Jump to previous search match in session detail.
    PrevSearchMatch,

    // --- File Explorer ---
    /// Toggle the file explorer drawer overlay.
    ToggleFileExplorer,

    // --- Detail View Scroll ---
    /// Scroll session detail content down one line (J key).
    DetailScrollDown,

    /// Scroll session detail content up one line (K key).
    DetailScrollUp,

    // --- Application Control ---
    /// Quit the application. Triggered by ZQ, ZZ, :quit, :q.
    Quit,

    // --- Session List Zone Navigation ---
    /// Switch to the next session list zone (Main → TaskRabbit).
    SessionListZoneNext,
    /// Switch to the previous session list zone (TaskRabbit → Main).
    SessionListZonePrev,

    // --- Session Rename ---
    /// Open rename overlay for the selected session (F2).
    RenameSession,

    /// Submit a new title for a session.
    SubmitSessionTitle(uuid::Uuid, String),

    // --- Session Labeling ---
    /// Open the label picker overlay to assign the focused session to a label.
    OpenLabelPicker,

    /// Navigate to the next label boundary in the session list.
    NextLabelBoundary,

    /// Navigate to the previous label boundary in the session list.
    PrevLabelBoundary,

    // --- Games ---
    /// Open the ESP Square game overlay.
    OpenEspSquare,

    // --- File Navigation ---
    /// Open the telescope fuzzy file picker overlay.
    OpenTelescope,
    /// Open the fuzzy command palette.
    OpenCommandPalette,

    // --- External Tools ---
    /// Suspend TUI and launch lazygit in the focused session's working directory.
    ToggleGitPanel,

    // --- Sidebar Resize ---
    /// Grow the sidebar (shift content right). Ctrl+Shift+Right.
    GrowSidebar,

    /// Shrink the sidebar (shift content left). Ctrl+Shift+Left.
    ShrinkSidebar,

    // --- Graph Review ---
    /// Open the graph review overlay for visual workflow editing.
    OpenGraphReview,
    /// `:topology-resolve [<execution_id>] <action> [<preserved_commit>]` —
    /// operator resolution of a blocked durable topology execution (#634).
    ResolveTopologyAttempt(String),

    // --- Entity Cards ---
    /// Open the card editor for the current project.
    OpenProjectCard,
    /// Open the user card editor.
    OpenUserCard,
    /// Add a fact to the current project's card (from :card add "fact").
    AddProjectCardFact(String),
    /// Add a fact to the user card (from :card user add "fact").
    AddUserCardFact(String),

    // --- Issue Tracker ---
    /// Open or focus the first-class Issue workspace pane.
    OpenIssuesWorkspace,

    // --- Dialectic Query ---
    /// Open the dialectic query overlay (natural-language Q&A).
    OpenDialectic,

    /// Open the dialectic query overlay with a pre-filled query (from :ask).
    AskQuery(String),

    // --- Daemon Config ---
    /// Toggle the daemon feature at the given index in `App::daemon_features`.
    ToggleDaemonFeature(usize),

    /// Trigger a global model-control emergency stop.
    EmergencyStopAll,

    /// Cancel a model invocation by durable invocation id.
    CancelModelInvocation(uuid::Uuid),

    /// Refresh `App::daemon_features` from the daemon (re-fetch GetDaemonConfig).
    RefreshDaemonFeatures,

    /// Sync title model configuration from UserSettings to daemon RuntimeConfig.
    SyncTitleModelConfig,

    /// Sync memory/dream configuration from UserSettings to daemon RuntimeConfig.
    SyncMemoryModelConfig,

    /// Sync prompt processor model/provider configuration to daemon RuntimeConfig.
    SyncPromptProcessorConfig,

    /// Sync a stall-classifier model selection (Agent Actors dropdown) to the
    /// daemon-owned `stall_classifier_model` field. Carries the selected
    /// model id directly since this row has no `UserSettings` backing.
    SyncClassifierModelConfig(String),

    /// Cycle the daemon-owned `system_prompt_preset` to the next value and
    /// dispatch `UpdateDaemonConfig` (RSI-026). Emitted from the Settings
    /// panel SystemPrompt category's Enter handler. The async dispatcher
    /// handles the RPC roundtrip and updates the local TUI cache on success.
    CycleSystemPromptPreset,

    /// Open the scheduled jobs browser overlay.
    OpenScheduleBrowser,

    /// Open the prompt creator/editor view.
    OpenPromptCreator,

    /// Toggle the embedded terminal overlay.
    ToggleTerminal,

    // --- Hierarchy Navigation & Creation (Phase 4) ---
    /// Pop one level off the current tab's descent_path. No-op when empty.
    AscendContainer,

    /// If descent_path is non-empty, ascend; otherwise fall through to BackToList.
    /// Mapped to Backspace as a context-sensitive shortcut.
    AscendOrBack,

    /// Open the typed-creation flow for a Bug session (leaf).
    CreateBug,

    /// Open the container creation form for a new Epic.
    /// Requires the current descent head to be a Group.
    CreateEpic,

    /// Open the container creation form for a new Group.
    CreateGroup,

    /// Open the typed-creation flow for a Story session (leaf).
    CreateStory,

    /// Open the typed-creation flow for a Task session (leaf).
    CreateTask,

    /// Open the parent picker overlay for the focused session.
    MoveToParent,

    /// Move the focused session to the root (parent_id = None).
    MoveToRoot,

    /// Set the focused leaf session as the lead of its parent Epic.
    /// No-op if the focused session has no parent or the parent is not an Epic.
    SetEpicLead,
    /// Open the project's appointed manager conversation.
    OpenHarnessManager,
    /// Appoint the focused ordinary leaf and choose its Epic scope.
    AppointHarnessManager,
    /// Inspect or edit the appointed manager's Epic scope.
    EditHarnessManagerScope,
    /// Revoke supervision by saving an empty scope.
    ClearHarnessManagerScope,
    /// Edit the operator's complete versioned manager grant and resource policy.
    EditHarnessManagerPolicy,
    /// Inspect scoped work, descendants and durable coordination evidence.
    OpenHarnessManagerBoard,
    /// Review and answer an exact version/digest-bound operator decision.
    OpenHarnessManagerDecisions,
    /// Open manager requests in the unified operator surface.
    OpenHarnessManagerInbox,
    /// Open manager inspection in the unified operator surface.
    OpenHarnessManagerInspect,

    /// P1.12: fire `ExecuteTopology` against the focused Epic's bound topology
    /// with `parent_id = Epic.id`. No-op (with error toast) if the focused
    /// session is not an Epic/Group, or the Epic has no `workflow_id` binding.
    RunEpicTopology,

    /// Jump to attention slot N (1..9). Bound to `<Space>1..<Space>9` in normal
    /// mode. Overlays can own bare `1..9` while they are open.
    JumpAttentionN(u8),

    /// Open recent-file slot N (1..9) in the file viewer. Bound to `gf1..gf9`.
    /// Composable with the existing `g`-prefix grammar (`gv`, `gd`, …).
    OpenRecentFileN(u8),

    // --- Event-Driven Navigation Refresh Actions ---
    /// Manual refresh of all navigation data (sessions, projects, labels).
    /// Replaces periodic polling for user-controlled updates.
    RefreshNavigation,

    /// Manual refresh of the current view only (current session or hierarchy).
    /// Instant refresh without waiting for next poll cycle.
    RefreshCurrentView,

    /// Manual refresh of projects and labels metadata only.
    /// Lightweight refresh for project/label changes.
    RefreshMetadata,

    /// Refresh `App::cached_usage_stats` from the daemon (re-fetch
    /// `GetUsageStats`). Queued on every entry to the Settings -> Stats
    /// category (T8; 1:1 clone of `RefreshDaemonFeatures`).
    RefreshUsageStats,

    /// Delete the budget policy at this index into the CURRENT
    /// `App::cached_model_control_status.policies` snapshot (captured at
    /// keypress time — the async handler re-reads the cache and no-ops with
    /// a warning if the index has since gone stale).
    DeleteBudgetPolicy(usize),

    /// Add or update a budget policy. `editing_index` is `Some(idx)` when
    /// editing an existing row in the current `policies` snapshot, `None`
    /// for a new policy (appended).
    SubmitBudgetPolicy {
        policy: rsi_common::model_control::ModelBudgetPolicy,
        editing_index: Option<usize>,
    },
}

#[cfg(test)]
impl LcAction {
    /// Stable per-variant name, used only by `test_all_variants_exist` below.
    ///
    /// This match has NO wildcard arm and matches on `self`'s constructor
    /// directly (not on anything in the test's vector), so adding a new
    /// `LcAction` variant makes this fail to compile ("non-exhaustive
    /// patterns") until an arm naming it is added here. That keeps the
    /// variant inventory self-maintaining instead of a hand-bumped literal
    /// count that can silently drift (see #679).
    fn variant_name(&self) -> &'static str {
        match self {
            LcAction::InterruptSession => "InterruptSession",
            LcAction::ContinueSession(..) => "ContinueSession",
            LcAction::QuickContinue => "QuickContinue",
            LcAction::EnterSession => "EnterSession",
            LcAction::BackToList => "BackToList",
            LcAction::OpenQuestionModal => "OpenQuestionModal",
            LcAction::NextAttention => "NextAttention",
            LcAction::PrevAttention => "PrevAttention",
            LcAction::ToggleNotifications => "ToggleNotifications",
            LcAction::DismissAllNotifications => "DismissAllNotifications",
            LcAction::ListSessions => "ListSessions",
            LcAction::NextEvent => "NextEvent",
            LcAction::PrevEvent => "PrevEvent",
            LcAction::NextUserMessage => "NextUserMessage",
            LcAction::PrevUserMessage => "PrevUserMessage",
            LcAction::OpenFold => "OpenFold",
            LcAction::CloseFold => "CloseFold",
            LcAction::ToggleFold => "ToggleFold",
            LcAction::CloseAllFolds => "CloseAllFolds",
            LcAction::OpenAllFolds => "OpenAllFolds",
            LcAction::ToggleSystemEvents => "ToggleSystemEvents",
            LcAction::ToggleThinkingEvents => "ToggleThinkingEvents",
            LcAction::DeleteSession => "DeleteSession",
            LcAction::ArchiveSession => "ArchiveSession",
            LcAction::TogglePinSession => "TogglePinSession",
            LcAction::ToggleTestingNeeded => "ToggleTestingNeeded",
            LcAction::ToggleRotationDisabled => "ToggleRotationDisabled",
            LcAction::CancelRetry => "CancelRetry",
            LcAction::OpenRatingOverlay => "OpenRatingOverlay",
            LcAction::OpenSessionInfoPanel => "OpenSessionInfoPanel",
            LcAction::RateSession(..) => "RateSession",
            LcAction::ReassignSessionProject => "ReassignSessionProject",
            LcAction::RotateSession => "RotateSession",
            LcAction::OpenSortPicker => "OpenSortPicker",
            LcAction::OpenDiagnostics => "OpenDiagnostics",
            LcAction::OpenRecursiveDagBrowser => "OpenRecursiveDagBrowser",
            LcAction::OpenMemorySearch => "OpenMemorySearch",
            LcAction::OpenSourceWorktreeSettlement => "OpenSourceWorktreeSettlement",
            LcAction::ToggleModelDropdown => "ToggleModelDropdown",
            LcAction::SelectModel(..) => "SelectModel",
            LcAction::OpenThemePicker => "OpenThemePicker",
            LcAction::OpenColorCustomizer => "OpenColorCustomizer",
            LcAction::SelectTheme(..) => "SelectTheme",
            LcAction::OpenProjectPicker => "OpenProjectPicker",
            LcAction::SwitchProject(..) => "SwitchProject",
            LcAction::CreateProject { .. } => "CreateProject",
            LcAction::EditProjectByName(..) => "EditProjectByName",
            LcAction::DeleteProjectByName(..) => "DeleteProjectByName",
            LcAction::OpenSessionInNewTab => "OpenSessionInNewTab",
            LcAction::NextTab => "NextTab",
            LcAction::PrevTab => "PrevTab",
            LcAction::NavigateRight => "NavigateRight",
            LcAction::EnterInputBarInsert(..) => "EnterInputBarInsert",
            LcAction::EnterSessionNormalMode => "EnterSessionNormalMode",
            LcAction::ToggleKeybindingsHelp => "ToggleKeybindingsHelp",
            Self::OpenAllCommands => "OpenAllCommands",
            Self::OpenManual(..) => "OpenManual",
            LcAction::TogglePromptPreview => "TogglePromptPreview",
            LcAction::ExecuteDocRegBlocks => "ExecuteDocRegBlocks",
            LcAction::CommitAndPush => "CommitAndPush",
            LcAction::JumpBack => "JumpBack",
            LcAction::JumpForward => "JumpForward",
            LcAction::TaskRabbitPrompt => "TaskRabbitPrompt",
            LcAction::LaunchTaskRabbit(..) => "LaunchTaskRabbit",
            LcAction::SetActiveTask(..) => "SetActiveTask",
            LcAction::BlankPrompt => "BlankPrompt",
            LcAction::LaunchBlank(..) => "LaunchBlank",
            LcAction::CloseFocusedPane => "CloseFocusedPane",
            LcAction::GoToArchiveZone => "GoToArchiveZone",
            LcAction::GoToTrash => "GoToTrash",
            LcAction::GoToSessionsZone => "GoToSessionsZone",
            LcAction::GoToTaskRabbitZone => "GoToTaskRabbitZone",
            LcAction::GoToJobsZone => "GoToJobsZone",
            LcAction::UnarchiveSession => "UnarchiveSession",
            LcAction::OpenRecentCompletions => "OpenRecentCompletions",
            LcAction::OpenSettings => "OpenSettings",
            LcAction::OpenSettingsAt(..) => "OpenSettingsAt",
            LcAction::YankEventContent => "YankEventContent",
            LcAction::EnterSearch => "EnterSearch",
            LcAction::NextSearchMatch => "NextSearchMatch",
            LcAction::PrevSearchMatch => "PrevSearchMatch",
            LcAction::ToggleFileExplorer => "ToggleFileExplorer",
            LcAction::DetailScrollDown => "DetailScrollDown",
            LcAction::DetailScrollUp => "DetailScrollUp",
            LcAction::Quit => "Quit",
            LcAction::SessionListZoneNext => "SessionListZoneNext",
            LcAction::SessionListZonePrev => "SessionListZonePrev",
            LcAction::RenameSession => "RenameSession",
            LcAction::SubmitSessionTitle(..) => "SubmitSessionTitle",
            LcAction::OpenLabelPicker => "OpenLabelPicker",
            LcAction::NextLabelBoundary => "NextLabelBoundary",
            LcAction::PrevLabelBoundary => "PrevLabelBoundary",
            LcAction::OpenEspSquare => "OpenEspSquare",
            LcAction::OpenTelescope => "OpenTelescope",
            LcAction::OpenCommandPalette => "OpenCommandPalette",
            LcAction::ToggleGitPanel => "ToggleGitPanel",
            LcAction::GrowSidebar => "GrowSidebar",
            LcAction::ShrinkSidebar => "ShrinkSidebar",
            LcAction::OpenGraphReview => "OpenGraphReview",
            LcAction::OpenProjectCard => "OpenProjectCard",
            LcAction::OpenUserCard => "OpenUserCard",
            LcAction::AddProjectCardFact(..) => "AddProjectCardFact",
            LcAction::AddUserCardFact(..) => "AddUserCardFact",
            LcAction::OpenIssuesWorkspace => "OpenIssuesWorkspace",
            LcAction::OpenDialectic => "OpenDialectic",
            LcAction::AskQuery(..) => "AskQuery",
            LcAction::ToggleDaemonFeature(..) => "ToggleDaemonFeature",
            LcAction::EmergencyStopAll => "EmergencyStopAll",
            LcAction::CancelModelInvocation(..) => "CancelModelInvocation",
            LcAction::RefreshDaemonFeatures => "RefreshDaemonFeatures",
            LcAction::SyncTitleModelConfig => "SyncTitleModelConfig",
            LcAction::SyncMemoryModelConfig => "SyncMemoryModelConfig",
            LcAction::SyncPromptProcessorConfig => "SyncPromptProcessorConfig",
            LcAction::SyncClassifierModelConfig(..) => "SyncClassifierModelConfig",
            LcAction::CycleSystemPromptPreset => "CycleSystemPromptPreset",
            LcAction::OpenScheduleBrowser => "OpenScheduleBrowser",
            LcAction::OpenPromptCreator => "OpenPromptCreator",
            LcAction::ToggleTerminal => "ToggleTerminal",
            LcAction::AscendContainer => "AscendContainer",
            LcAction::AscendOrBack => "AscendOrBack",
            LcAction::CreateBug => "CreateBug",
            LcAction::CreateEpic => "CreateEpic",
            LcAction::CreateGroup => "CreateGroup",
            LcAction::CreateStory => "CreateStory",
            LcAction::CreateTask => "CreateTask",
            LcAction::MoveToParent => "MoveToParent",
            LcAction::MoveToRoot => "MoveToRoot",
            LcAction::SetEpicLead => "SetEpicLead",
            LcAction::OpenHarnessManager => "OpenHarnessManager",
            LcAction::AppointHarnessManager => "AppointHarnessManager",
            LcAction::EditHarnessManagerScope => "EditHarnessManagerScope",
            LcAction::ClearHarnessManagerScope => "ClearHarnessManagerScope",
            LcAction::EditHarnessManagerPolicy => "EditHarnessManagerPolicy",
            LcAction::OpenHarnessManagerBoard => "OpenHarnessManagerBoard",
            LcAction::OpenHarnessManagerDecisions => "OpenHarnessManagerDecisions",
            LcAction::OpenHarnessManagerInbox => "OpenHarnessManagerInbox",
            LcAction::OpenHarnessManagerInspect => "OpenHarnessManagerInspect",
            LcAction::RunEpicTopology => "RunEpicTopology",
            LcAction::JumpAttentionN(..) => "JumpAttentionN",
            LcAction::OpenRecentFileN(..) => "OpenRecentFileN",
            LcAction::ResolveTopologyAttempt(..) => "ResolveTopologyAttempt",
            LcAction::RefreshNavigation => "RefreshNavigation",
            LcAction::RefreshCurrentView => "RefreshCurrentView",
            LcAction::RefreshMetadata => "RefreshMetadata",
            LcAction::RefreshUsageStats => "RefreshUsageStats",
            LcAction::DeleteBudgetPolicy(..) => "DeleteBudgetPolicy",
            LcAction::SubmitBudgetPolicy { .. } => "SubmitBudgetPolicy",
        }
    }
}

impl ApplicationAction for LcAction {
    fn is_edit_sequence(&self, _ctx: &EditContext) -> SequenceStatus {
        // flywheel has no editable text sequences — always break.
        SequenceStatus::Break
    }

    fn is_last_action(&self, _ctx: &EditContext) -> SequenceStatus {
        // No repeatable action sequences in a read-only session manager.
        SequenceStatus::Ignore
    }

    fn is_last_selection(&self, _ctx: &EditContext) -> SequenceStatus {
        // No text selections to repeat.
        SequenceStatus::Ignore
    }

    fn is_switchable(&self, _ctx: &EditContext) -> bool {
        // No WindowAction::Switch fallback behavior needed.
        false
    }
}

/// Application error type.
#[derive(Debug)]
pub struct LcError(pub String);

impl fmt::Display for LcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl ApplicationError for LcError {}

/// Application store (unused — marker trait).
pub struct LcStore;
impl ApplicationStore for LcStore {}

/// Window identifier for split panes.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LcWindowId(pub u64);
impl ApplicationWindowId for LcWindowId {}

/// Content identifier for command bar buffers.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LcContentId(pub String);
impl ApplicationContentId for LcContentId {}

/// The ApplicationInfo implementation that ties everything together.
///
/// This is the single type parameter used throughout all modalkit generics:
/// `Action<LcInfo>`, `TabAction<LcInfo>`, `WindowAction<LcInfo>`, etc.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LcInfo;

impl ApplicationInfo for LcInfo {
    type Error = LcError;
    type Action = LcAction;
    type Store = LcStore;
    type WindowId = LcWindowId;
    type ContentId = LcContentId;

    fn content_of_command(cmdtype: CommandType) -> LcContentId {
        match cmdtype {
            CommandType::Search => LcContentId("*search*".into()),
            CommandType::Command => LcContentId("*command*".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lc_action_clone_eq() {
        let a = LcAction::TaskRabbitPrompt;
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn test_lc_action_debug() {
        let a = LcAction::LaunchTaskRabbit("test query".to_string());
        let debug = format!("{:?}", a);
        assert!(debug.contains("LaunchTaskRabbit"));
        assert!(debug.contains("test query"));
    }

    #[test]
    fn test_lc_action_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<LcAction>();
    }

    #[test]
    fn test_lc_info_content_of_command() {
        let cmd = LcInfo::content_of_command(CommandType::Command);
        assert_eq!(cmd, LcContentId("*command*".into()));

        let search = LcInfo::content_of_command(CommandType::Search);
        assert_eq!(search, LcContentId("*search*".into()));
    }

    #[test]
    fn test_application_action_impl() {
        let ctx = EditContext::default();
        let action = LcAction::InterruptSession;
        assert!(matches!(
            action.is_edit_sequence(&ctx),
            SequenceStatus::Break
        ));
        assert!(matches!(
            action.is_last_action(&ctx),
            SequenceStatus::Ignore
        ));
        assert!(matches!(
            action.is_last_selection(&ctx),
            SequenceStatus::Ignore
        ));
        assert!(!action.is_switchable(&ctx));
    }

    #[test]
    fn test_all_variants_exist() {
        let _variants: Vec<LcAction> = vec![
            LcAction::InterruptSession,
            LcAction::ContinueSession(None),
            LcAction::QuickContinue,
            LcAction::OpenQuestionModal,
            LcAction::EnterSession,
            LcAction::BackToList,
            LcAction::NextAttention,
            LcAction::PrevAttention,
            LcAction::ToggleNotifications,
            LcAction::DismissAllNotifications,
            LcAction::ListSessions,
            LcAction::NextEvent,
            LcAction::PrevEvent,
            LcAction::NextUserMessage,
            LcAction::PrevUserMessage,
            LcAction::OpenFold,
            LcAction::CloseFold,
            LcAction::ToggleFold,
            LcAction::CloseAllFolds,
            LcAction::OpenAllFolds,
            LcAction::ToggleSystemEvents,
            LcAction::ToggleThinkingEvents,
            LcAction::DeleteSession,
            LcAction::ArchiveSession,
            LcAction::TogglePinSession,
            LcAction::ToggleTestingNeeded,
            LcAction::ToggleRotationDisabled,
            LcAction::OpenRatingOverlay,
            LcAction::OpenSessionInfoPanel,
            LcAction::RateSession(7),
            LcAction::ReassignSessionProject,
            LcAction::RotateSession,
            LcAction::OpenSortPicker,
            LcAction::OpenDiagnostics,
            LcAction::OpenRecursiveDagBrowser,
            LcAction::OpenMemorySearch,
            LcAction::ToggleModelDropdown,
            LcAction::SelectModel(None),
            LcAction::OpenThemePicker,
            LcAction::SelectTheme("mocha".into()),
            LcAction::OpenProjectPicker,
            LcAction::SwitchProject("x".into()),
            LcAction::CreateProject {
                name: "x".into(),
                path: None,
            },
            LcAction::EditProjectByName("x".into()),
            LcAction::DeleteProjectByName("x".into()),
            LcAction::OpenSessionInNewTab,
            LcAction::NextTab,
            LcAction::PrevTab,
            LcAction::NavigateRight,
            LcAction::EnterInputBarInsert(InsertStyle::Insert),
            LcAction::EnterSessionNormalMode,
            LcAction::ToggleKeybindingsHelp,
            LcAction::OpenAllCommands,
            LcAction::OpenManual(crate::manual::open::ManualMode::Browser),
            LcAction::TogglePromptPreview,
            LcAction::ExecuteDocRegBlocks,
            LcAction::CommitAndPush,
            LcAction::TaskRabbitPrompt,
            LcAction::LaunchTaskRabbit("q".into()),
            LcAction::JumpBack,
            LcAction::JumpForward,
            LcAction::GoToArchiveZone,
            LcAction::GoToTrash,
            LcAction::GoToSessionsZone,
            LcAction::GoToTaskRabbitZone,
            LcAction::GoToJobsZone,
            LcAction::UnarchiveSession,
            LcAction::OpenRecentCompletions,
            LcAction::CloseFocusedPane,
            LcAction::OpenSettings,
            LcAction::OpenSettingsAt(crate::settings_registry::SettingsSection::ClaudeHooks),
            LcAction::YankEventContent,
            LcAction::EnterSearch,
            LcAction::NextSearchMatch,
            LcAction::PrevSearchMatch,
            LcAction::ToggleFileExplorer,
            LcAction::DetailScrollDown,
            LcAction::DetailScrollUp,
            LcAction::Quit,
            LcAction::SessionListZoneNext,
            LcAction::SessionListZonePrev,
            LcAction::RenameSession,
            LcAction::SubmitSessionTitle(uuid::Uuid::nil(), String::new()),
            LcAction::OpenLabelPicker,
            LcAction::NextLabelBoundary,
            LcAction::PrevLabelBoundary,
            LcAction::SetActiveTask(None),
            LcAction::BlankPrompt,
            LcAction::LaunchBlank("q".into()),
            LcAction::OpenEspSquare,
            LcAction::OpenTelescope,
            LcAction::OpenCommandPalette,
            LcAction::ToggleGitPanel,
            LcAction::GrowSidebar,
            LcAction::ShrinkSidebar,
            LcAction::OpenGraphReview,
            LcAction::ResolveTopologyAttempt("inspect".into()),
            LcAction::OpenProjectCard,
            LcAction::OpenUserCard,
            LcAction::AddProjectCardFact("fact".into()),
            LcAction::AddUserCardFact("fact".into()),
            LcAction::OpenIssuesWorkspace,
            LcAction::OpenDialectic,
            LcAction::AskQuery("test".into()),
            LcAction::ToggleDaemonFeature(0),
            LcAction::EmergencyStopAll,
            LcAction::CancelModelInvocation(uuid::Uuid::nil()),
            LcAction::RefreshDaemonFeatures,
            LcAction::OpenScheduleBrowser,
            LcAction::OpenColorCustomizer,
            LcAction::SyncMemoryModelConfig,
            LcAction::SyncPromptProcessorConfig,
            LcAction::SyncClassifierModelConfig("m".into()),
            LcAction::CycleSystemPromptPreset,
            LcAction::OpenPromptCreator,
            LcAction::ToggleTerminal,
            // --- Hierarchy actions (Phase 4) ---
            LcAction::AscendContainer,
            LcAction::AscendOrBack,
            LcAction::CreateBug,
            LcAction::CreateEpic,
            LcAction::CreateGroup,
            LcAction::CreateStory,
            LcAction::CreateTask,
            LcAction::MoveToParent,
            LcAction::MoveToRoot,
            LcAction::SetEpicLead,
            LcAction::RunEpicTopology,
            // --- Event-driven refresh actions ---
            LcAction::RefreshNavigation,
            LcAction::RefreshCurrentView,
            LcAction::RefreshMetadata,
            LcAction::RefreshUsageStats,
            LcAction::DeleteBudgetPolicy(0),
            LcAction::SubmitBudgetPolicy {
                policy: rsi_common::model_control::ModelBudgetPolicy {
                    scope_kind: rsi_common::model_control::BudgetScopeKind::Global,
                    scope_id: Some("global".to_string()),
                    purpose: None,
                    model_tier: None,
                    effort: None,
                    ceiling_model_tier: None,
                    ceiling_effort: None,
                    max_calls: None,
                    max_total_tokens: Some(1000),
                    max_input_tokens: None,
                    max_output_tokens: None,
                    max_cache_creation_tokens: None,
                    max_cache_read_tokens: None,
                    max_reasoning_tokens: None,
                    max_embedding_inputs: None,
                    max_wall_time_ms: None,
                    max_concurrency: None,
                    max_retries: None,
                    max_calls_per_window: None,
                    rate_window_seconds: None,
                    alert_threshold_ratio: None,
                },
                editing_index: None,
            },
            // --- Restored coverage (#679): these variants existed but were
            // missing from this vector, so `test_all_variants_exist` was not
            // actually exhaustive. ---
            LcAction::CancelRetry,
            LcAction::OpenSourceWorktreeSettlement,
            LcAction::SyncTitleModelConfig,
            LcAction::OpenHarnessManager,
            LcAction::AppointHarnessManager,
            LcAction::EditHarnessManagerScope,
            LcAction::ClearHarnessManagerScope,
            LcAction::EditHarnessManagerPolicy,
            LcAction::OpenHarnessManagerBoard,
            LcAction::OpenHarnessManagerDecisions,
            LcAction::OpenHarnessManagerInbox,
            LcAction::OpenHarnessManagerInspect,
            LcAction::JumpAttentionN(1),
            LcAction::OpenRecentFileN(1),
        ];

        // `LcAction::variant_name` is an exhaustive match with no wildcard
        // arm over `self` (not over this vector) — see its doc comment.
        // Adding a new `LcAction` variant therefore fails to compile until
        // it is named there, regardless of whether this vector is updated.
        //
        // That alone doesn't prove THIS vector is complete: a variant named
        // in `variant_name` but never added above would still pass a
        // dedup-only check — exactly the drift that once let 18 variants go
        // missing silently (#679). So compare two independently derived
        // sets: the vector's own names (via `variant_name`, deduped) against
        // the full variant inventory scanned directly out of the `enum
        // LcAction { .. }` source text. Neither side is a hand-bumped
        // literal.
        let vector_names: std::collections::BTreeSet<&'static str> =
            _variants.iter().map(LcAction::variant_name).collect();
        assert_eq!(
            vector_names.len(),
            _variants.len(),
            "test_all_variants_exist must list each LcAction variant exactly once \
             (found a duplicate entry)"
        );

        let scanned_names = scan_lc_action_variant_names();
        let vector_names_owned: std::collections::BTreeSet<String> =
            vector_names.iter().map(|s| (*s).to_string()).collect();
        assert_eq!(
            vector_names_owned, scanned_names,
            "test_all_variants_exist's vector must list every LcAction variant found in \
             `enum LcAction {{ .. }}` (scanned from source) — add the missing variant(s) above, \
             or remove any name here that no longer names a real variant"
        );
    }

    /// Scans `enum LcAction { .. }` directly out of this file's own source
    /// text and returns the full set of top-level variant identifiers.
    ///
    /// This is the independent "ground truth" `test_all_variants_exist`
    /// checks the hand-maintained vector against, so the total is never
    /// itself a hand-bumped literal. It is a plain brace/paren/angle-depth
    /// scan (not a real parser): it strips `//`/`///` line comments, then
    /// walks to the enum's matching closing brace, then splits the body on
    /// top-level commas (tracking nested `()`, `{}`, `<>`) and takes each
    /// chunk's leading identifier as the variant name.
    fn scan_lc_action_variant_names() -> std::collections::BTreeSet<String> {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/modalkit_types.rs"
        ));

        let marker = "pub enum LcAction {";
        let marker_at = source
            .find(marker)
            .expect("locate `pub enum LcAction {` in modalkit_types.rs");

        let mut without_comments = String::with_capacity(source.len() - marker_at);
        for line in source[marker_at..].lines() {
            match line.find("//") {
                Some(idx) => without_comments.push_str(&line[..idx]),
                None => without_comments.push_str(line),
            }
            without_comments.push('\n');
        }

        // Find the enum's own `{ .. }` span (brace depth only — `()`/`<>`
        // inside variant payloads are always paired within a `{}` pair, so
        // they never affect this outer match).
        let mut depth = 0i32;
        let mut body_start = None;
        let mut body_end = None;
        for (i, ch) in without_comments.char_indices() {
            match ch {
                '{' => {
                    depth += 1;
                    if depth == 1 {
                        body_start = Some(i + 1);
                    }
                }
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        body_end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let body_start = body_start.expect("find LcAction enum opening brace");
        let body_end = body_end.expect("find LcAction enum closing brace");
        let body = &without_comments[body_start..body_end];

        // Split the body on top-level commas, tracking `()`/`{}`/`<>`
        // nesting so multi-field and generic variant payloads (e.g.
        // `Option<String>`, `CreateProject { name: String, .. }`) stay
        // intact as single chunks.
        let mut names = std::collections::BTreeSet::new();
        let mut d_paren = 0i32;
        let mut d_brace = 0i32;
        let mut d_angle = 0i32;
        let mut current = String::new();
        let push_current = |chunk: &str, names: &mut std::collections::BTreeSet<String>| {
            let ident: String = chunk
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !ident.is_empty() {
                names.insert(ident);
            }
        };
        for ch in body.chars() {
            match ch {
                '(' => d_paren += 1,
                ')' => d_paren -= 1,
                '{' => d_brace += 1,
                '}' => d_brace -= 1,
                '<' => d_angle += 1,
                '>' => d_angle -= 1,
                _ => {}
            }
            if ch == ',' && d_paren == 0 && d_brace == 0 && d_angle == 0 {
                push_current(&current, &mut names);
                current.clear();
            } else {
                current.push(ch);
            }
        }
        push_current(&current, &mut names);
        names
    }
}
