//! Shared action identity, metadata, context, and availability policy.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use uuid::Uuid;

use crate::app::App;
use crate::modalkit_types::LcAction;
use crate::settings::DaemonFeatureValue;
use crate::settings_keys::DAEMON_FEATURE_SECTIONS;
use crate::settings_registry::SettingsSection;
use crate::types::{InputMode, OverlayState, Pane, SessionListZone, SettingsFocus};
use crate::ui::theme_roles::ThemeRole;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActionId {
    ContextHelp,
    AllCommands,
    OpenManual,
    MoveDown,
    MoveUp,
    JumpTop,
    JumpBottom,
    Search,
    Open,
    Close,
    NavigateBack,
    NavigateForward,
    ToggleSetting,
    SettingsAdd,
    SettingsDelete,
    SettingsEnable,
    SettingsRefresh,
    SettingsSearch,
    SettingsNextMatch,
    SettingsPrevMatch,
    Refresh,
    OpenThemePicker,
    OpenLegacyColors,
    OpenIssuesWorkspace,
    /// Harness manager views reached through the `<Space>g` leader subnamespace.
    ManagerPolicy,
    ManagerBoard,
    ManagerDecisions,
    ManagerInbox,
    ManagerInspect,
    EditThemeRole,
    ResetThemeRole,
    ResetActiveTheme,
    IssueOpenSession,
    IssueRefresh,
    IssueRunPoll,
    IssueSelectFormValue,
    IssueRetryMutation,
    IssueInspectorNextSection,
    IssueInspectorPreviousPage,
    IssueInspectorNextPage,
    IssueNew,
    IssueEdit,
    IssueStatus,
    IssueReopen,
    IssuePriority,
    IssueAssignee,
    IssueLabels,
    IssueDependencies,
    IssueReloadLatest,
    IssueRebaseDraft,
    IssueArmCancel,
    IssueCancel,
    IssueArchive,
    IssueConfirmArchive,
    IssueRestore,
    IssueYankArm,
    IssueCopyUuid,
    IssueCopyNumber,
    IssuePreviousTab,
    IssueNextTab,
    IssueSearch,
    IssueFilter,
    IssueSort,
    IssueJumpArm,
    IssueJumpStart,
    IssueJumpEnd,
    ScheduleNew,
    ScheduleEdit,
    ScheduleToggle,
    ScheduleTrigger,
    ScheduleArmDelete,
    ScheduleDelete,
    ScheduleRefresh,
    ThemeRoleCommit,
    ThemeRoleReset,
    InterruptSession,
    ContinueSession,
    TogglePin,
    ArchiveSession,
    UnarchiveSession,
    ToggleTestingNeeded,
    AscendHierarchy,
    CopySessionUuid,
    DeleteSession,
    RotateSession,
    Archives,
    Model,
    Sessions,
    Alerts,
    Quit,
    Projects,
    Project,
    Task,
    Blank,
    ProjectNew,
    ProjectEdit,
    ProjectDelete,
    SettingsCommand,
    StopAll,
    Hooks,
    Skills,
    Group,
    Diagnostics,
    Graph,
    TopologyResolve,
    Dag,
    Context,
    Card,
    Ask,
    Terminal,
    Lead,
    Manager,
    ManagerAppoint,
    ManagerScope,
    ManagerClear,
    Rate,
    Split,
    VSplit,
    ClosePane,
    OnlyPane,
    TabNew,
    TabClose,
    TabNext,
    TabPrev,
    SearchNext,
    SearchPrev,
    NextAttention,
    PrevAttention,
    JumpAttention,
    NextLabelGroup,
    PrevLabelGroup,
    GoToMainZone,
    GoToTaskRabbitZone,
    GoToJobsZone,
    RecentCompletions,
    NavigateRight,
    AscendOrBack,
    SortPicker,
    Trash,
    EnterInputInsert,
    EnterSessionNormal,
    PromptPreview,
    NextUserMessage,
    PrevUserMessage,
    OpenFold,
    CloseFold,
    ToggleFold,
    CloseAllFolds,
    OpenAllFolds,
    ToggleSystemEvents,
    ToggleThinkingEvents,
    SessionInfo,
    RenameSession,
    ModelDropdown,
    ReassignProject,
    ToggleRotation,
    CancelRetry,
    ExecuteDocRegBlocks,
    CommitAndPush,
    OpenSessionInNewTab,
    GitPanel,
    FileExplorer,
    Telescope,
    OpenRecentFile,
    PromptCreator,
    CommandPalette,
    QuestionModal,
    MemorySearch,
    ScheduleBrowserOpen,
    EspSquare,
    RunEpicTopology,
    CreateGroup,
    CreateEpic,
    CreateStory,
    CreateTask,
    CreateBug,
    MoveToParent,
    MoveToRoot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActionRoute {
    Normal,
    Settings,
    IssueTracker,
    ScheduleBrowser,
    ThemeRoleEditor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionBinding {
    pub sequence: &'static str,
    pub route: ActionRoute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionPayload {
    None,
    Uuid(Uuid),
    Index(usize),
    ThemeRole(ThemeRole),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionRequest {
    pub id: ActionId,
    pub payload: ActionPayload,
}

impl ActionRequest {
    pub const fn plain(id: ActionId) -> Self {
        Self {
            id,
            payload: ActionPayload::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvailabilitySelector {
    Always,
    ContextHelp,
    SessionList,
    Settings,
    IssueTracker,
    IssueLocal,
    IssueLocalSelection,
    IssueLocalActiveSelection,
    IssueLocalDependencySelection,
    IssueLocalTerminalSelection,
    IssueLocalArchivedTerminalSelection,
    IssueCancelConfirmation,
    IssueArchiveConfirmation,
    IssueSelection,
    IssueLinkedSession,
    IssueStaleEditor,
    IssueRefresh,
    IssueSyncPoll,
    IssueFormSelection,
    IssueRetryMutation,
    IssueLocalInspector,
    ScheduleBrowser,
    ScheduleSelection,
    SchedulePendingDelete,
    ThemeRoleEditor,
    ThemeRoleOverride,
    Connected,
    ContextualOpen,
    SettingsCategories,
    SettingsItems,
    SettingsEditable,
    SettingsAdd,
    SettingsDelete,
    SettingsEnable,
    SettingsRefresh,
    Navigation,
    SessionListConnected,
    ContinueSessionMutation,
    RunningLeafSessionMutation,
    PinSessionMutation,
    ArchiveSessionMutation,
    ArchivedSessionListMutation,
    TestingLeafSessionMutation,
    HierarchyAscend,
    CopySessionUuid,
    ScheduleMutation,
}

#[derive(Debug, Clone, Copy)]
pub struct ActionDescriptor {
    pub id: ActionId,
    pub label: &'static str,
    /// One operator-facing sentence: the manual body text and the
    /// all-commands row text. `label` stays the short name.
    pub summary: &'static str,
    pub category: &'static str,
    pub bindings: &'static [ActionBinding],
    pub command_aliases: &'static [&'static str],
    pub command_argument: CommandArgument,
    pub availability: AvailabilitySelector,
    pub show_in_help: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandArgument {
    None,
    Optional,
    Required,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionAvailability {
    Available(ActionRequest),
    Unavailable { reason: &'static str },
}

#[derive(Debug, Clone, Copy)]
pub struct AvailableAction {
    pub descriptor: &'static ActionDescriptor,
    pub request: ActionRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionSurface {
    SessionList,
    Settings,
    IssueTracker,
    ScheduleBrowser,
    ThemeRoleEditor,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpOrigin {
    SessionList,
    SessionDetail,
    PromptCreator,
    PromptEntry,
    InputOverlay,
    /// An overlay state with no discovery catalog route; the exemption names
    /// why and where its keys are documented.
    OverlayExempt(OverlayHelpExemption),
    /// An overlay whose own handler is described by `OVERLAY_HELP_ROUTES`.
    OverlayClass(OverlayHelpClass),
    SettingsCategories,
    SettingsItems,
    IssueTracker,
    ScheduleBrowser,
    ThemeRoleEditor,
    Other,
}

impl HelpOrigin {
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::OverlayClass(class) => {
                overlay_help_route(class).map_or("Active Overlay", |route| route.title)
            }
            Self::SessionList => "Session List",
            Self::SessionDetail => "Session Detail",
            Self::PromptCreator => "Prompt Creator",
            Self::PromptEntry => "Session Prompt",
            Self::InputOverlay => "Input Overlay",
            Self::OverlayExempt(_) => "Active Overlay",
            Self::SettingsCategories => "Settings / Categories",
            Self::SettingsItems => "Settings / Items",
            Self::IssueTracker => "Issues",
            Self::ScheduleBrowser => "Scheduled Jobs",
            Self::ThemeRoleEditor => "Theme Role Editor",
            Self::Other => "Current View",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionMode {
    Normal,
    Text,
    Search,
    PendingDelete,
    PendingArchive,
    PendingYank,
    PendingJump,
    RoleEdit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionPane {
    SessionList,
    SessionDetail,
    Settings,
    PromptCreator,
    Issues,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionFocus {
    SessionList,
    SessionDetail,
    DetailListProxy,
    SettingsCategories,
    SettingsItems,
    PromptCreator,
    IssueList,
    ScheduleList,
    ThemeRoleInput,
    ThemeRoleCommitted,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionSelectionKind {
    Session,
    SettingRow,
    Issue,
    ScheduledJob,
    ThemeRole,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionIssueStatus {
    Active,
    Terminal,
    Other,
}

fn pane_kind(pane: Option<&Pane>) -> ActionPane {
    match pane {
        Some(Pane::SessionList { .. }) => ActionPane::SessionList,
        Some(Pane::SessionDetail { .. }) => ActionPane::SessionDetail,
        Some(Pane::Settings) => ActionPane::Settings,
        Some(Pane::PromptCreator) => ActionPane::PromptCreator,
        Some(Pane::Issues(_)) => ActionPane::Issues,
        None => ActionPane::None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionContext {
    pub surface: ActionSurface,
    pub origin: HelpOrigin,
    pub mode: ActionMode,
    pub input_mode: InputMode,
    pub focus: ActionFocus,
    /// Physical focused pane and interaction-proxied pane are intentionally distinct.
    pub physical_pane: ActionPane,
    pub interaction_pane: ActionPane,
    pub active_tab_index: usize,
    pub project_id: Option<Uuid>,
    pub session_list_zone: Option<SessionListZone>,
    pub hierarchy_descended: bool,
    pub help_search_active: bool,
    pub role_editor_text_entry: bool,
    pub overlay_blocks_normal_actions: bool,
    pub operator: bool,
    pub connected: bool,
    pub authoritative_config_ready: bool,
    pub storage_refresh_in_flight: bool,
    pub batch_fetch_supported: bool,
    pub push_supported: bool,
    pub memory_search_supported: bool,
    pub sandbox_supported: bool,
    pub has_selection: bool,
    pub selection_kind: ActionSelectionKind,
    pub selected_id: Option<Uuid>,
    pub selected_session_kind: Option<rsi_common::types::SessionKind>,
    pub selected_session_status: Option<rsi_common::types::SessionStatus>,
    pub selected_session_pinned: bool,
    pub selected_session_leaf: bool,
    pub selected_session_container: bool,
    pub selected_session_has_lead: bool,
    pub selected_issue_session_id: Option<Uuid>,
    pub selected_issue_status: Option<ActionIssueStatus>,
    pub selected_issue_archived: Option<bool>,
    pub issue_loading: bool,
    pub issue_tab: Option<crate::types::IssueWorkspaceTab>,
    pub issue_stale_editor: bool,
    pub issue_form_selectable: bool,
    pub issue_retry_pending: bool,
    pub issue_inspector: bool,
    pub selected_schedule_enabled: Option<bool>,
    pub schedule_loading: bool,
    pub selected_theme_role: Option<ThemeRole>,
    pub selected_theme_role_overridden: bool,
    pub settings_selected_index: usize,
    pub settings_category: SettingsSection,
    pub settings_focus: SettingsFocus,
    pub settings_row_exists: bool,
    pub settings_row_editable: bool,
    pub settings_row_deletable: bool,
    pub settings_row_enableable: bool,
}

impl Default for ActionContext {
    fn default() -> Self {
        Self {
            surface: ActionSurface::Other,
            origin: HelpOrigin::Other,
            mode: ActionMode::Normal,
            input_mode: InputMode::Normal,
            focus: ActionFocus::None,
            physical_pane: ActionPane::None,
            interaction_pane: ActionPane::None,
            active_tab_index: 0,
            project_id: None,
            session_list_zone: None,
            hierarchy_descended: false,
            help_search_active: false,
            role_editor_text_entry: false,
            overlay_blocks_normal_actions: false,
            operator: true,
            connected: true,
            authoritative_config_ready: true,
            storage_refresh_in_flight: false,
            batch_fetch_supported: true,
            push_supported: false,
            memory_search_supported: false,
            sandbox_supported: false,
            has_selection: false,
            selection_kind: ActionSelectionKind::None,
            selected_id: None,
            selected_session_kind: None,
            selected_session_status: None,
            selected_session_pinned: false,
            selected_session_leaf: false,
            selected_session_container: false,
            selected_session_has_lead: false,
            selected_issue_session_id: None,
            selected_issue_status: None,
            selected_issue_archived: None,
            issue_loading: false,
            issue_tab: None,
            issue_stale_editor: false,
            issue_form_selectable: false,
            issue_retry_pending: false,
            issue_inspector: false,
            selected_schedule_enabled: None,
            schedule_loading: false,
            selected_theme_role: None,
            selected_theme_role_overridden: false,
            settings_selected_index: 0,
            settings_category: SettingsSection::ThemeColors,
            settings_focus: SettingsFocus::Categories,
            settings_row_exists: false,
            settings_row_editable: false,
            settings_row_deletable: false,
            settings_row_enableable: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct SettingsRowFacts {
    exists: bool,
    editable: bool,
    deletable: bool,
    enableable: bool,
}

fn daemon_feature_is_editable(entry: &crate::settings::DaemonFeatureEntry) -> bool {
    matches!(
        entry.value,
        DaemonFeatureValue::Bool(_) | DaemonFeatureValue::Cycle { .. }
    ) || matches!(
        entry.field.as_str(),
        "model_control_stop_all"
            | "sandbox_build_cache_dry_run"
            | "sandbox_build_cache_reclaim_now"
    )
}

fn settings_row_facts(app: &App) -> SettingsRowFacts {
    let index = app.settings_state.selected_index;
    let section = app.settings_state.section;
    let mut facts = SettingsRowFacts::default();
    if DAEMON_FEATURE_SECTIONS.contains(&section) {
        let rows = crate::settings_keys::daemon_feature_rows_for_section(app, section);
        if let Some((_, vec_idx)) = rows.get(index)
            && let Some(entry) = app.daemon_features.get(*vec_idx)
        {
            facts.exists = true;
            facts.editable = app.authoritative_config_ready() && daemon_feature_is_editable(entry);
        }
        return facts;
    }
    match section {
        SettingsSection::ThemeColors => {
            facts.exists = index < 20;
            facts.editable = facts.exists;
        }
        SettingsSection::Screen => {
            facts.exists = index < 4;
            facts.editable = facts.exists;
        }
        SettingsSection::TranscriptDefaults => {
            facts.exists = index < 3;
            facts.editable = facts.exists;
        }
        SettingsSection::InputPrompts => {
            facts.exists = index < 3;
            facts.editable = facts.exists;
        }
        SettingsSection::ApiProviders => {
            facts.exists = index < app.settings.custom_providers.len();
            facts.editable =
                facts.exists || (index == 0 && app.settings.custom_providers.is_empty());
            facts.deletable = facts.exists;
        }
        SettingsSection::SessionList => {
            facts.exists = index
                < crate::settings::NAVIGATOR_SETTINGS_ROW_COUNT + app.settings.card_fields.len();
            facts.editable = facts.exists;
        }
        SettingsSection::ModelRoles => {
            facts.exists = index < 6;
            // Only the classifier row is daemon-owned. The remaining actor
            // preferences retain their local UserSettings authority while a
            // bootstrap config request is pending or has failed.
            facts.editable = facts.exists && (index != 5 || app.authoritative_config_ready());
        }
        SettingsSection::ClaudeHooks => {
            facts.exists = index < app.cached_hook_rows.len();
            facts.editable = facts.exists || (index == 0 && app.cached_hook_rows.is_empty());
            facts.deletable = facts.exists;
        }
        SettingsSection::ClaudeSkills => {
            let count = app.cached_user_skills.as_ref().map_or(0, Vec::len);
            facts.exists = index < count;
            facts.editable = facts.exists;
            facts.deletable = facts.exists;
            facts.enableable = facts.exists;
        }
        SettingsSection::MemoryDreaming => {
            let count = crate::settings_keys::memory_dreaming_row_count();
            facts.exists = index < count;
            facts.editable = facts.exists
                && crate::settings_keys::daemon_feature_vec_index(app, section, index)
                    .is_none_or(|_| app.authoritative_config_ready());
        }
        SettingsSection::MessageBridges => {
            facts.exists = index < 2;
            facts.editable = facts.exists;
        }
        SettingsSection::SystemPrompt => {
            facts.exists = index == 0;
            facts.editable = facts.exists && app.authoritative_config_ready();
        }
        SettingsSection::Usage => {
            facts.exists = index < crate::model_control_stats::stats_row_count(app);
            facts.editable =
                facts.exists && crate::model_control_stats::stats_row_action(app, index).is_some();
        }
        SettingsSection::Budgets => {
            let count = app
                .cached_model_control_status
                .as_ref()
                .map_or(0, |status| status.policies.len());
            facts.exists = index < count;
            facts.editable = facts.exists || (index == 0 && count == 0);
            facts.deletable = facts.exists;
        }
        SettingsSection::ModelControl
        | SettingsSection::RetriesRecovery
        | SettingsSection::StallDetection
        | SettingsSection::Orchestration
        | SettingsSection::CodeIntelligence
        | SettingsSection::ProviderIsolation
        | SettingsSection::SandboxStorage => {
            unreachable!("handled by the DAEMON_FEATURE_SECTIONS early return above")
        }
    }
    facts
}

fn issue_status(status: &str) -> ActionIssueStatus {
    match status.to_ascii_lowercase().as_str() {
        "open" | "active" | "running" | "dispatched" | "in_progress" | "in progress" => {
            ActionIssueStatus::Active
        }
        "closed" | "completed" | "done" | "failed" | "cancelled" | "canceled" => {
            ActionIssueStatus::Terminal
        }
        _ => ActionIssueStatus::Other,
    }
}

fn focused_file_viewer_open(app: &App) -> bool {
    let session_id = match app.focused_pane() {
        Some(Pane::SessionDetail { session_id }) => *session_id,
        Some(Pane::SessionList {
            selected_session: Some(id),
            ..
        }) => *id,
        _ => return false,
    };
    app.sessions
        .get(&session_id)
        .is_some_and(|state| state.file_viewer.is_some())
}

fn populate_session_selection(context: &mut ActionContext, app: &App, session_id: Uuid) {
    context.selected_id = Some(session_id);
    context.selection_kind = ActionSelectionKind::Session;
    context.has_selection = true;
    let Some(state) = app.sessions.get(&session_id) else {
        return;
    };
    let session = &state.session;
    context.selected_session_kind = Some(session.session_kind);
    context.selected_session_status = Some(session.status);
    context.selected_session_pinned = session.pinned_at.is_some();
    context.selected_session_leaf = rsi_common::types::is_leaf_kind(session.session_kind);
    context.selected_session_container = rsi_common::types::is_container_kind(session.session_kind);
    context.selected_session_has_lead = session.lead_session_id.is_some();
}

impl ActionContext {
    pub fn from_app(app: &App) -> Self {
        let source_overlay = app.focused_input_overlay().or_else(|| {
            if matches!(app.overlay, OverlayState::KeybindingsHelp { .. }) {
                app.previous_overlay()
            } else {
                Some(&app.overlay)
            }
        });
        let physical_pane = pane_kind(
            app.active_tab()
                .layout
                .find_pane(app.active_tab().focused_pane),
        );
        let interaction_pane = pane_kind(app.focused_pane());
        let input_mode = app.input_mode;
        let mut context = Self {
            input_mode,
            connected: app.poll.connected,
            authoritative_config_ready: app.authoritative_config_ready(),
            storage_refresh_in_flight: app.storage_refresh_in_flight(),
            batch_fetch_supported: app.poll.batch_fetch_supported,
            push_supported: app.poll.push_supported,
            memory_search_supported: app.poll.memory_search_supported,
            sandbox_supported: app.poll.sandbox_supported,
            physical_pane,
            interaction_pane,
            active_tab_index: app.active_tab,
            project_id: app.active_tab().project_id,
            hierarchy_descended: !app.active_tab().descent_path.is_empty(),
            help_search_active: matches!(
                app.overlay,
                OverlayState::KeybindingsHelp {
                    search_active: true,
                    ..
                }
            ),
            mode: match input_mode {
                InputMode::Normal => ActionMode::Normal,
                InputMode::Search => ActionMode::Search,
                InputMode::Input | InputMode::Command => ActionMode::Text,
            },
            ..Self::default()
        };

        match app.focused_pane() {
            Some(Pane::SessionList {
                selected_session,
                active_zone,
                ..
            }) => {
                context.surface = ActionSurface::SessionList;
                context.origin = HelpOrigin::SessionList;
                context.focus = if physical_pane == ActionPane::SessionDetail {
                    ActionFocus::DetailListProxy
                } else {
                    ActionFocus::SessionList
                };
                context.session_list_zone = Some(*active_zone);
                if let Some(selected_id) = selected_session {
                    populate_session_selection(&mut context, app, *selected_id);
                }
            }
            Some(Pane::SessionDetail { session_id }) => {
                context.origin = HelpOrigin::SessionDetail;
                context.focus = ActionFocus::SessionDetail;
                if app.sessions.get(session_id).is_some_and(|state| {
                    state.input_bar.surface.mode == crate::types::PopupMode::Insert
                }) {
                    context.mode = ActionMode::Text;
                }
                populate_session_selection(&mut context, app, *session_id);
            }
            Some(Pane::Settings) => {
                let facts = settings_row_facts(app);
                context.surface = ActionSurface::Settings;
                context.origin = match app.settings_state.focus {
                    SettingsFocus::Categories => HelpOrigin::SettingsCategories,
                    SettingsFocus::Items => HelpOrigin::SettingsItems,
                };
                context.focus = match app.settings_state.focus {
                    SettingsFocus::Categories => ActionFocus::SettingsCategories,
                    SettingsFocus::Items => ActionFocus::SettingsItems,
                };
                context.has_selection = facts.exists;
                context.selection_kind = if facts.exists {
                    ActionSelectionKind::SettingRow
                } else {
                    ActionSelectionKind::None
                };
                context.settings_selected_index = app.settings_state.selected_index;
                context.settings_category = app.settings_state.section;
                context.settings_focus = app.settings_state.focus;
                context.settings_row_exists = facts.exists;
                context.settings_row_editable = facts.editable;
                context.settings_row_deletable = facts.deletable;
                context.settings_row_enableable = facts.enableable;
                if app.settings_state.section == SettingsSection::ThemeColors {
                    context.selected_theme_role =
                        crate::settings_keys::theme_role_for_settings_index(
                            app.settings_state.selected_index,
                        );
                    context.selected_theme_role_overridden =
                        context.selected_theme_role.is_some_and(|role| {
                            crate::ui::theme::get_theme_role_override(role).is_some()
                        });
                }
            }
            Some(Pane::PromptCreator) => {
                context.origin = HelpOrigin::PromptCreator;
                context.focus = ActionFocus::PromptCreator;
            }
            Some(Pane::Issues(state)) => {
                context.surface = ActionSurface::IssueTracker;
                context.origin = HelpOrigin::IssueTracker;
                context.focus = ActionFocus::IssueList;
                context.issue_loading = matches!(
                    state.load_state(match state.active_tab {
                        crate::types::IssueWorkspaceTab::Local => {
                            crate::types::IssueWorkspaceDataKind::LocalPage
                        }
                        crate::types::IssueWorkspaceTab::Dispatched => {
                            crate::types::IssueWorkspaceDataKind::Dispatched
                        }
                        crate::types::IssueWorkspaceTab::Sync => {
                            crate::types::IssueWorkspaceDataKind::SyncStatus
                        }
                    }),
                    crate::types::IssueWorkspaceLoadState::Loading
                );
                context.issue_tab = Some(state.active_tab);
                context.issue_stale_editor = state
                    .transient
                    .editor
                    .as_ref()
                    .is_some_and(|editor| editor.stale_conflict);
                context.issue_form_selectable =
                    state
                        .transient
                        .editor
                        .as_ref()
                        .is_some_and(|editor| match editor.mode {
                            crate::types::IssueEditorMode::Filter => {
                                matches!(editor.active_field, 0 | 4 | 8)
                            }
                            crate::types::IssueEditorMode::Status { .. } => true,
                            crate::types::IssueEditorMode::Dependency { .. } => {
                                editor.active_field == 0
                                    || (editor.active_field == 2
                                        && !editor.dependency_candidates.is_empty())
                            }
                            crate::types::IssueEditorMode::Create
                            | crate::types::IssueEditorMode::Edit { .. } => false,
                        });
                context.issue_retry_pending = state.transient.pending_mutation.is_some();
                context.issue_inspector = state.focus
                    == crate::types::IssueWorkspaceFocus::Inspector
                    && match state.active_tab {
                        crate::types::IssueWorkspaceTab::Local => state.local.inspector_open,
                        crate::types::IssueWorkspaceTab::Dispatched => {
                            state.dispatched.inspector_open
                        }
                        crate::types::IssueWorkspaceTab::Sync => false,
                    };
                context.has_selection = match state.active_tab {
                    crate::types::IssueWorkspaceTab::Local => {
                        state.local.selected_issue_id.is_some()
                    }
                    crate::types::IssueWorkspaceTab::Dispatched => {
                        state.dispatched.selected_session_id.is_some()
                    }
                    crate::types::IssueWorkspaceTab::Sync => false,
                };
                context.selection_kind = if context.has_selection {
                    ActionSelectionKind::Issue
                } else {
                    ActionSelectionKind::None
                };
                context.mode = if state.transient.cancel_confirmation.is_some() {
                    ActionMode::PendingDelete
                } else if state.transient.archive_confirmation.is_some() {
                    ActionMode::PendingArchive
                } else if state.transient.pending_yank {
                    ActionMode::PendingYank
                } else if state.transient.pending_jump {
                    ActionMode::PendingJump
                } else if state.transient.editor.is_some() {
                    ActionMode::Text
                } else {
                    ActionMode::Normal
                };
                if state.active_tab == crate::types::IssueWorkspaceTab::Local {
                    context.selected_id = state.local.selected_issue_id;
                    if let Some(issue_id) = state.local.selected_issue_id
                        && let Some(row) =
                            state.transient.current_page.as_ref().and_then(|page| {
                                page.rows.iter().find(|row| row.issue.id == issue_id)
                            })
                    {
                        context.selected_issue_status = Some(match row.issue.status {
                            rsi_common::types::IssueStatus::Open
                            | rsi_common::types::IssueStatus::InProgress => {
                                ActionIssueStatus::Active
                            }
                            rsi_common::types::IssueStatus::Closed
                            | rsi_common::types::IssueStatus::Cancelled => {
                                ActionIssueStatus::Terminal
                            }
                        });
                        context.selected_issue_archived = Some(row.issue.archived_at.is_some());
                    }
                } else if state.active_tab == crate::types::IssueWorkspaceTab::Dispatched {
                    context.selected_issue_session_id = state.dispatched.selected_session_id;
                    if let Some(session_id) = state.dispatched.selected_session_id
                        && let Some(record) = state
                            .transient
                            .dispatched
                            .iter()
                            .find(|record| record.session_id == session_id)
                    {
                        context.selected_issue_status = Some(issue_status(
                            record.terminal_state.as_deref().unwrap_or("Running"),
                        ));
                    }
                }
            }
            None => {}
        }

        // The model dropdown widget intercepts every key ahead of any overlay
        // (`overlay::handle_overlay_key`), so it owns help while open.
        if app.model_dropdown.open {
            context.origin = HelpOrigin::OverlayClass(OverlayHelpClass::ModelPicker);
            context.surface = ActionSurface::Other;
            context.mode = ActionMode::Text;
            context.overlay_blocks_normal_actions = true;
            return context;
        }

        match source_overlay {
            Some(OverlayState::None) => {
                // The per-session file viewer consumes keys ahead of the
                // input bar and Vim machine (`file_viewer::handle_file_viewer_key`).
                if focused_file_viewer_open(app) {
                    context.origin = HelpOrigin::OverlayClass(OverlayHelpClass::FileViewer);
                    context.surface = ActionSurface::Other;
                    context.mode = ActionMode::Text;
                    context.overlay_blocks_normal_actions = true;
                }
            }
            // A prompt's own model dropdown intercepts keys before prompt
            // editing (`overlay::handle_overlay_prompt_keys` and the
            // input-overlay stack path), so it owns help while open.
            Some(OverlayState::Prompt { model_dropdown, .. }) if model_dropdown.open => {
                context.origin = HelpOrigin::OverlayClass(OverlayHelpClass::ModelPicker);
                context.surface = ActionSurface::Other;
                context.mode = ActionMode::Text;
                context.overlay_blocks_normal_actions = true;
            }
            Some(OverlayState::Prompt { surface, .. }) => {
                context.origin = HelpOrigin::PromptEntry;
                context.overlay_blocks_normal_actions = true;
                if surface.mode == crate::types::PopupMode::Insert {
                    context.mode = ActionMode::Text;
                }
            }
            Some(OverlayState::ScheduleBrowser {
                jobs,
                selected_index,
                loading,
                pending_delete,
                ..
            }) => {
                context.surface = ActionSurface::ScheduleBrowser;
                context.origin = HelpOrigin::ScheduleBrowser;
                context.focus = ActionFocus::ScheduleList;
                context.schedule_loading = *loading;
                context.has_selection = jobs.get(*selected_index).is_some();
                context.selection_kind = if context.has_selection {
                    ActionSelectionKind::ScheduledJob
                } else {
                    ActionSelectionKind::None
                };
                if let Some(job) = jobs.get(*selected_index) {
                    context.selected_id = Some(job.id);
                    context.selected_schedule_enabled = Some(job.enabled);
                }
                if *pending_delete {
                    context.mode = ActionMode::PendingDelete;
                }
                return context;
            }
            Some(OverlayState::ThemeRoleEditor {
                role, committed, ..
            }) => {
                context.surface = ActionSurface::ThemeRoleEditor;
                context.origin = HelpOrigin::ThemeRoleEditor;
                context.focus = if *committed {
                    ActionFocus::ThemeRoleCommitted
                } else {
                    ActionFocus::ThemeRoleInput
                };
                context.role_editor_text_entry = !*committed;
                if !*committed {
                    context.mode = ActionMode::RoleEdit;
                }
                context.has_selection = true;
                context.selection_kind = ActionSelectionKind::ThemeRole;
                context.selected_theme_role = Some(*role);
                context.selected_theme_role_overridden =
                    crate::ui::theme::get_theme_role_override(*role).is_some();
                return context;
            }
            Some(overlay) => {
                context.origin = match overlay_help_routing(app, overlay) {
                    HelpRouting::Routed(class) => HelpOrigin::OverlayClass(class),
                    HelpRouting::Exempt(exemption) => HelpOrigin::OverlayExempt(exemption),
                };
                context.surface = ActionSurface::Other;
                context.mode = ActionMode::Text;
                context.overlay_blocks_normal_actions = true;
            }
            _ => {}
        }
        context
    }
}

const fn binding(sequence: &'static str, route: ActionRoute) -> ActionBinding {
    ActionBinding { sequence, route }
}

const CONTEXT_HELP: &[ActionBinding] = &[
    binding("?", ActionRoute::Normal),
    binding("Ctrl-Alt-G", ActionRoute::Normal),
    binding("Ctrl-Alt-G", ActionRoute::Settings),
    binding("?", ActionRoute::Settings),
    binding("Ctrl-Alt-G", ActionRoute::IssueTracker),
    binding("?", ActionRoute::IssueTracker),
    binding("Ctrl-Alt-G", ActionRoute::ScheduleBrowser),
    binding("?", ActionRoute::ScheduleBrowser),
    binding("Ctrl-Alt-G", ActionRoute::ThemeRoleEditor),
    binding("?", ActionRoute::ThemeRoleEditor),
];
const MOVE_DOWN: &[ActionBinding] = &[
    binding("j", ActionRoute::Normal),
    binding("j", ActionRoute::Settings),
    binding("j", ActionRoute::IssueTracker),
    binding("j", ActionRoute::ScheduleBrowser),
];
const MOVE_UP: &[ActionBinding] = &[
    binding("k", ActionRoute::Normal),
    binding("k", ActionRoute::Settings),
    binding("k", ActionRoute::IssueTracker),
    binding("k", ActionRoute::ScheduleBrowser),
];
const JUMP_TOP: &[ActionBinding] = &[binding("gg", ActionRoute::Normal)];
const JUMP_BOTTOM: &[ActionBinding] = &[binding("G", ActionRoute::Normal)];
const OPEN: &[ActionBinding] = &[
    binding("Enter", ActionRoute::Normal),
    binding("L", ActionRoute::Normal),
    binding("Enter", ActionRoute::Settings),
];
const SETTINGS_BACK: &[ActionBinding] = &[
    binding("h", ActionRoute::Settings),
    binding("Left", ActionRoute::Settings),
];
const SETTINGS_FORWARD: &[ActionBinding] = &[
    binding("l", ActionRoute::Settings),
    binding("Right", ActionRoute::Settings),
];
const SETTINGS_TOGGLE: &[ActionBinding] = &[binding("Space", ActionRoute::Settings)];
const SETTINGS_ADD: &[ActionBinding] = &[binding("a", ActionRoute::Settings)];
const SETTINGS_DELETE_ITEM: &[ActionBinding] = &[binding("d", ActionRoute::Settings)];
const SETTINGS_ENABLE: &[ActionBinding] = &[binding("e", ActionRoute::Settings)];
const SETTINGS_REFRESH: &[ActionBinding] = &[binding("R", ActionRoute::Settings)];
const SETTINGS_SEARCH: &[ActionBinding] = &[binding("/", ActionRoute::Settings)];
const SETTINGS_NEXT_MATCH: &[ActionBinding] = &[binding("n", ActionRoute::Settings)];
const SETTINGS_PREV_MATCH: &[ActionBinding] = &[binding("N", ActionRoute::Settings)];
const NORMAL_SEARCH: &[ActionBinding] = &[binding("/", ActionRoute::Normal)];
const NORMAL_REFRESH: &[ActionBinding] = &[binding("r", ActionRoute::Normal)];
const NORMAL_THEME_PICKER: &[ActionBinding] = &[binding("T", ActionRoute::Normal)];
const NORMAL_LEGACY_COLORS: &[ActionBinding] = &[binding("<Space>b", ActionRoute::Normal)];
const NORMAL_ISSUES_WORKSPACE: &[ActionBinding] = &[binding("<Space>i", ActionRoute::Normal)];
// Manager views live under the `<Space>g` leader subnamespace (`<Space>gg` already
// owns the Git panel, `gs`.. under the bare g leader are untouched) so the existing
// two-key Space chords keep their muscle memory. Chords dispatch the same LcAction
// variants `:manager policy` / `:manager board` / `:manager decisions` use.
const NORMAL_MANAGER_POLICY: &[ActionBinding] = &[binding("<Space>gp", ActionRoute::Normal)];
const NORMAL_MANAGER_BOARD: &[ActionBinding] = &[binding("<Space>gb", ActionRoute::Normal)];
const NORMAL_MANAGER_DECISIONS: &[ActionBinding] = &[binding("<Space>gd", ActionRoute::Normal)];
const NORMAL_INTERRUPT: &[ActionBinding] = &[binding("x", ActionRoute::Normal)];
const NORMAL_CONTINUE: &[ActionBinding] = &[
    binding("X", ActionRoute::Normal),
    binding("<Space>c", ActionRoute::Normal),
];
const NORMAL_TOGGLE_PIN: &[ActionBinding] = &[binding("P", ActionRoute::Normal)];
const NORMAL_ARCHIVE: &[ActionBinding] = &[binding("<Space>a", ActionRoute::Normal)];
const NORMAL_UNARCHIVE: &[ActionBinding] = &[binding("U", ActionRoute::Normal)];
const NORMAL_TOGGLE_TESTING: &[ActionBinding] = &[binding("<Space>t", ActionRoute::Normal)];
const NORMAL_ASCEND: &[ActionBinding] = &[
    binding("-", ActionRoute::Normal),
    binding("H", ActionRoute::Normal),
];
const NORMAL_COPY_SESSION_UUID: &[ActionBinding] = &[binding("yy", ActionRoute::Normal)];

// Epic M slice (a): Normal-mode chords migrated from hand-installed
// `add_mapping` calls in keybindings.rs. Their effects live in
// `keybindings::registered_normal_effect`.
const NORMAL_DELETE_SESSION: &[ActionBinding] = &[binding("DD", ActionRoute::Normal)];
const NORMAL_ROTATE_SESSION: &[ActionBinding] = &[binding("R", ActionRoute::Normal)];
const NORMAL_ARCHIVES: &[ActionBinding] = &[binding("ga", ActionRoute::Normal)];
const NORMAL_QUIT: &[ActionBinding] = &[
    binding("ZQ", ActionRoute::Normal),
    binding("ZZ", ActionRoute::Normal),
];
const NORMAL_PROJECTS: &[ActionBinding] = &[binding("<Space>p", ActionRoute::Normal)];
const NORMAL_SETTINGS_COMMAND: &[ActionBinding] = &[binding("<Space>,", ActionRoute::Normal)];
const NORMAL_STOP_ALL: &[ActionBinding] = &[binding("<Space>S", ActionRoute::Normal)];
const NORMAL_ALERTS: &[ActionBinding] = &[binding("<Space>n", ActionRoute::Normal)];
const NORMAL_GRAPH: &[ActionBinding] = &[binding("<Space>v", ActionRoute::Normal)];
const NORMAL_LEAD: &[ActionBinding] = &[binding("gL", ActionRoute::Normal)];
const NORMAL_TASK: &[ActionBinding] = &[binding("<Space>o", ActionRoute::Normal)];
const NORMAL_BLANK: &[ActionBinding] = &[binding("<Space>m", ActionRoute::Normal)];
const NORMAL_TAB_PREV: &[ActionBinding] = &[binding("<", ActionRoute::Normal)];
const NORMAL_TAB_NEXT: &[ActionBinding] = &[binding(">", ActionRoute::Normal)];
const NORMAL_CLOSE_PANE: &[ActionBinding] = &[binding("<Space>q", ActionRoute::Normal)];
const NORMAL_SEARCH_NEXT: &[ActionBinding] = &[binding("n", ActionRoute::Normal)];
const NORMAL_SEARCH_PREV: &[ActionBinding] = &[binding("N", ActionRoute::Normal)];
const NORMAL_NEXT_ATTENTION: &[ActionBinding] = &[binding("]a", ActionRoute::Normal)];
const NORMAL_PREV_ATTENTION: &[ActionBinding] = &[binding("[a", ActionRoute::Normal)];
const NORMAL_JUMP_ATTENTION: &[ActionBinding] = &[
    binding("<Space>1", ActionRoute::Normal),
    binding("<Space>2", ActionRoute::Normal),
    binding("<Space>3", ActionRoute::Normal),
    binding("<Space>4", ActionRoute::Normal),
    binding("<Space>5", ActionRoute::Normal),
    binding("<Space>6", ActionRoute::Normal),
    binding("<Space>7", ActionRoute::Normal),
    binding("<Space>8", ActionRoute::Normal),
    binding("<Space>9", ActionRoute::Normal),
];
const NORMAL_NEXT_LABEL_GROUP: &[ActionBinding] = &[binding("]g", ActionRoute::Normal)];
const NORMAL_PREV_LABEL_GROUP: &[ActionBinding] = &[binding("[g", ActionRoute::Normal)];
const NORMAL_GO_TO_MAIN_ZONE: &[ActionBinding] = &[binding("gs", ActionRoute::Normal)];
const NORMAL_GO_TO_TASK_RABBIT_ZONE: &[ActionBinding] = &[binding("gt", ActionRoute::Normal)];
const NORMAL_GO_TO_JOBS_ZONE: &[ActionBinding] = &[binding("gj", ActionRoute::Normal)];
const NORMAL_RECENT_COMPLETIONS: &[ActionBinding] = &[binding("gr", ActionRoute::Normal)];
const NORMAL_NAVIGATE_RIGHT: &[ActionBinding] = &[binding("l", ActionRoute::Normal)];
const NORMAL_ASCEND_OR_BACK: &[ActionBinding] = &[binding("Backspace", ActionRoute::Normal)];
const NORMAL_SORT_PICKER: &[ActionBinding] = &[binding("<Space>s", ActionRoute::Normal)];
const NORMAL_TRASH: &[ActionBinding] = &[binding("<Space>gX", ActionRoute::Normal)];
const NORMAL_ENTER_INPUT_INSERT: &[ActionBinding] = &[
    binding("i", ActionRoute::Normal),
    binding("a", ActionRoute::Normal),
    binding("o", ActionRoute::Normal),
    binding("O", ActionRoute::Normal),
];
const NORMAL_ENTER_SESSION_NORMAL: &[ActionBinding] = &[binding("e", ActionRoute::Normal)];
const NORMAL_PROMPT_PREVIEW: &[ActionBinding] = &[binding("p", ActionRoute::Normal)];
const NORMAL_NEXT_USER_MESSAGE: &[ActionBinding] = &[binding("]u", ActionRoute::Normal)];
const NORMAL_PREV_USER_MESSAGE: &[ActionBinding] = &[binding("[u", ActionRoute::Normal)];
const NORMAL_OPEN_FOLD: &[ActionBinding] = &[binding("zo", ActionRoute::Normal)];
const NORMAL_CLOSE_FOLD: &[ActionBinding] = &[binding("zc", ActionRoute::Normal)];
const NORMAL_TOGGLE_FOLD: &[ActionBinding] = &[binding("za", ActionRoute::Normal)];
const NORMAL_CLOSE_ALL_FOLDS: &[ActionBinding] = &[binding("zM", ActionRoute::Normal)];
const NORMAL_OPEN_ALL_FOLDS: &[ActionBinding] = &[binding("zR", ActionRoute::Normal)];
const NORMAL_TOGGLE_SYSTEM_EVENTS: &[ActionBinding] = &[binding("zs", ActionRoute::Normal)];
const NORMAL_TOGGLE_THINKING_EVENTS: &[ActionBinding] = &[binding("zt", ActionRoute::Normal)];
const NORMAL_SESSION_INFO: &[ActionBinding] = &[binding("F3", ActionRoute::Normal)];
const NORMAL_RENAME_SESSION: &[ActionBinding] = &[binding("F2", ActionRoute::Normal)];
const NORMAL_MODEL_DROPDOWN: &[ActionBinding] = &[binding("Ctrl-M", ActionRoute::Normal)];
const NORMAL_REASSIGN_PROJECT: &[ActionBinding] = &[binding("<Space>C", ActionRoute::Normal)];
const NORMAL_TOGGLE_ROTATION: &[ActionBinding] = &[binding("<Space>r", ActionRoute::Normal)];
const NORMAL_CANCEL_RETRY: &[ActionBinding] = &[binding("<Space>k", ActionRoute::Normal)];
const NORMAL_EXECUTE_DOC_REG_BLOCKS: &[ActionBinding] = &[binding("<Space>x", ActionRoute::Normal)];
const NORMAL_COMMIT_AND_PUSH: &[ActionBinding] = &[binding("<Space>X", ActionRoute::Normal)];
const NORMAL_OPEN_SESSION_IN_NEW_TAB: &[ActionBinding] =
    &[binding("<Space>T", ActionRoute::Normal)];
const NORMAL_GIT_PANEL: &[ActionBinding] = &[binding("<Space>gg", ActionRoute::Normal)];
const NORMAL_FILE_EXPLORER: &[ActionBinding] = &[binding("<Space>e", ActionRoute::Normal)];
const NORMAL_TELESCOPE: &[ActionBinding] = &[binding("<Space><Space>", ActionRoute::Normal)];
const NORMAL_OPEN_RECENT_FILE: &[ActionBinding] = &[
    binding("gf1", ActionRoute::Normal),
    binding("gf2", ActionRoute::Normal),
    binding("gf3", ActionRoute::Normal),
    binding("gf4", ActionRoute::Normal),
    binding("gf5", ActionRoute::Normal),
    binding("gf6", ActionRoute::Normal),
    binding("gf7", ActionRoute::Normal),
    binding("gf8", ActionRoute::Normal),
    binding("gf9", ActionRoute::Normal),
];
const NORMAL_PROMPT_CREATOR: &[ActionBinding] = &[binding("<Space>gP", ActionRoute::Normal)];
const NORMAL_COMMAND_PALETTE: &[ActionBinding] = &[binding("<Space>;", ActionRoute::Normal)];
const NORMAL_QUESTION_MODAL: &[ActionBinding] = &[binding("<Space>gq", ActionRoute::Normal)];
const NORMAL_MEMORY_SEARCH: &[ActionBinding] = &[binding("<Space>M", ActionRoute::Normal)];
const NORMAL_SCHEDULE_BROWSER_OPEN: &[ActionBinding] = &[binding("<Space>K", ActionRoute::Normal)];
const NORMAL_ESP_SQUARE: &[ActionBinding] = &[binding("<Space>gc", ActionRoute::Normal)];
const NORMAL_RUN_EPIC_TOPOLOGY: &[ActionBinding] = &[binding("gR", ActionRoute::Normal)];
const NORMAL_CREATE_GROUP: &[ActionBinding] = &[binding("<Space>G", ActionRoute::Normal)];
const NORMAL_CREATE_EPIC: &[ActionBinding] = &[binding("<Space>E", ActionRoute::Normal)];
const NORMAL_CREATE_STORY: &[ActionBinding] = &[binding("<Space>gS", ActionRoute::Normal)];
const NORMAL_CREATE_TASK: &[ActionBinding] = &[binding("<Space>gT", ActionRoute::Normal)];
const NORMAL_CREATE_BUG: &[ActionBinding] = &[binding("<Space>B", ActionRoute::Normal)];
const NORMAL_MOVE_TO_PARENT: &[ActionBinding] = &[binding("mp", ActionRoute::Normal)];
const NORMAL_MOVE_TO_ROOT: &[ActionBinding] = &[binding("mo", ActionRoute::Normal)];
const SETTINGS_RESET: &[ActionBinding] = &[binding("Delete", ActionRoute::Settings)];
const ISSUE_OPEN: &[ActionBinding] = &[binding("Enter", ActionRoute::IssueTracker)];
const ISSUE_REFRESH: &[ActionBinding] = &[binding("r", ActionRoute::IssueTracker)];
const ISSUE_RUN_POLL: &[ActionBinding] = &[binding("P", ActionRoute::IssueTracker)];
const ISSUE_SELECT_FORM_VALUE: &[ActionBinding] = &[binding("Enter", ActionRoute::IssueTracker)];
const ISSUE_RETRY_MUTATION: &[ActionBinding] = &[binding("Ctrl-Enter", ActionRoute::IssueTracker)];
const ISSUE_INSPECTOR_NEXT_SECTION: &[ActionBinding] = &[binding("Tab", ActionRoute::IssueTracker)];
const ISSUE_INSPECTOR_PREVIOUS_PAGE: &[ActionBinding] =
    &[binding("PageUp", ActionRoute::IssueTracker)];
const ISSUE_INSPECTOR_NEXT_PAGE: &[ActionBinding] =
    &[binding("PageDown", ActionRoute::IssueTracker)];
const ISSUE_NEW: &[ActionBinding] = &[binding("n", ActionRoute::IssueTracker)];
const ISSUE_EDIT: &[ActionBinding] = &[binding("e", ActionRoute::IssueTracker)];
const ISSUE_STATUS: &[ActionBinding] = &[binding("S", ActionRoute::IssueTracker)];
const ISSUE_REOPEN: &[ActionBinding] = &[binding("S", ActionRoute::IssueTracker)];
const ISSUE_PRIORITY: &[ActionBinding] = &[binding("p", ActionRoute::IssueTracker)];
const ISSUE_ASSIGNEE: &[ActionBinding] = &[binding("a", ActionRoute::IssueTracker)];
const ISSUE_LABELS: &[ActionBinding] = &[binding("l", ActionRoute::IssueTracker)];
const ISSUE_DEPENDENCIES: &[ActionBinding] = &[binding("b", ActionRoute::IssueTracker)];
const ISSUE_RELOAD_LATEST: &[ActionBinding] = &[binding("Ctrl-L", ActionRoute::IssueTracker)];
const ISSUE_REBASE_DRAFT: &[ActionBinding] = &[binding("Ctrl-R", ActionRoute::IssueTracker)];
const ISSUE_ARM_CANCEL: &[ActionBinding] = &[binding("d", ActionRoute::IssueTracker)];
const ISSUE_CANCEL: &[ActionBinding] = &[binding("dd", ActionRoute::IssueTracker)];
const ISSUE_ARCHIVE: &[ActionBinding] = &[binding("A", ActionRoute::IssueTracker)];
const ISSUE_CONFIRM_ARCHIVE: &[ActionBinding] = &[binding("Enter", ActionRoute::IssueTracker)];
const ISSUE_RESTORE: &[ActionBinding] = &[binding("U", ActionRoute::IssueTracker)];
const ISSUE_YANK_ARM: &[ActionBinding] = &[binding("y", ActionRoute::IssueTracker)];
const ISSUE_COPY_UUID: &[ActionBinding] = &[binding("yy", ActionRoute::IssueTracker)];
const ISSUE_COPY_NUMBER: &[ActionBinding] = &[binding("y#", ActionRoute::IssueTracker)];
const ISSUE_PREVIOUS_TAB: &[ActionBinding] = &[binding("[", ActionRoute::IssueTracker)];
const ISSUE_NEXT_TAB: &[ActionBinding] = &[binding("]", ActionRoute::IssueTracker)];
const ISSUE_SEARCH: &[ActionBinding] = &[binding("/", ActionRoute::IssueTracker)];
const ISSUE_FILTER: &[ActionBinding] = &[binding("f", ActionRoute::IssueTracker)];
const ISSUE_SORT: &[ActionBinding] = &[binding("s", ActionRoute::IssueTracker)];
const ISSUE_JUMP_ARM: &[ActionBinding] = &[binding("g", ActionRoute::IssueTracker)];
const ISSUE_JUMP_START: &[ActionBinding] = &[binding("gg", ActionRoute::IssueTracker)];
const ISSUE_JUMP_END: &[ActionBinding] = &[binding("G", ActionRoute::IssueTracker)];
const SCHEDULE_NEW: &[ActionBinding] = &[binding("n", ActionRoute::ScheduleBrowser)];
const SCHEDULE_EDIT: &[ActionBinding] = &[
    binding("Enter", ActionRoute::ScheduleBrowser),
    binding("e", ActionRoute::ScheduleBrowser),
];
const SCHEDULE_TOGGLE: &[ActionBinding] = &[binding("Space", ActionRoute::ScheduleBrowser)];
const SCHEDULE_TRIGGER: &[ActionBinding] = &[binding("t", ActionRoute::ScheduleBrowser)];
const SCHEDULE_ARM_DELETE: &[ActionBinding] = &[binding("d", ActionRoute::ScheduleBrowser)];
const SCHEDULE_DELETE: &[ActionBinding] = &[binding("dd", ActionRoute::ScheduleBrowser)];
const SCHEDULE_REFRESH: &[ActionBinding] = &[binding("r", ActionRoute::ScheduleBrowser)];
const EDITOR_COMMIT: &[ActionBinding] = &[binding("Enter", ActionRoute::ThemeRoleEditor)];
const EDITOR_RESET: &[ActionBinding] = &[binding("Delete", ActionRoute::ThemeRoleEditor)];
const CLOSE: &[ActionBinding] = &[
    binding("q", ActionRoute::Settings),
    binding("Esc", ActionRoute::Settings),
    binding("q", ActionRoute::IssueTracker),
    binding("Esc", ActionRoute::IssueTracker),
    binding("q", ActionRoute::ScheduleBrowser),
    binding("Esc", ActionRoute::ScheduleBrowser),
    binding("Esc", ActionRoute::ThemeRoleEditor),
];

/// A key surface decoded by hand-written handlers, not a dispatch table.
///
/// Epic M design A.2. The manual renders each as a named
/// section that points at its hand-written narrative and says it is not
/// generated; `untabulated_surfaces_link_to_existing_narrative` keeps every
/// anchor pointing at a real `docs/keybindings.md` heading.
#[derive(Debug, Clone, Copy)]
pub struct UntabulatedKeySurface {
    pub surface: &'static str,
    pub reason: &'static str,
    /// `docs/keybindings.md` heading text (after the `#`s).
    pub narrative_anchor: &'static str,
}

pub static UNTABULATED_KEY_SURFACES: &[UntabulatedKeySurface] = &[
    UntabulatedKeySurface {
        surface: "Vim text editing (input bar and overlay editors)",
        reason: "The modalkit / textarea editing grammar (counts, motions, operators, visual mode) is not a key table.",
        narrative_anchor: "Vim Text Editing (Input Bar & Overlay)",
    },
    UntabulatedKeySurface {
        surface: "Session input bar",
        reason: "Decoded directly by the input bar and input surface handlers.",
        narrative_anchor: "Input Bar Keybindings (Session Detail View)",
    },
    UntabulatedKeySurface {
        surface: "Launch / continue prompt",
        reason: "An input-surface editor with its own mode handling.",
        narrative_anchor: "Prompt Overlay (New Session / Continue Session)",
    },
    UntabulatedKeySurface {
        surface: "Prompt creator",
        reason: "Decoded directly by the prompt creator's key handler.",
        narrative_anchor: "Prompt Creator (`<Space>gP`)",
    },
    UntabulatedKeySurface {
        surface: "File viewer (normal and insert keys)",
        reason: "Decoded directly by the file viewer; its `:` ex forms are generated.",
        narrative_anchor: "File Viewer (opened from File Explorer)",
    },
];

/// How a reserved Normal-mode sequence behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalReservationKind {
    /// Cleared so it waits for the next key (its default Vim meaning would
    /// otherwise fire before a longer registered chord completes).
    Prefix,
    /// A retired chord kept as an explicit no-op so it cannot fall through to
    /// its trailing key's single-key action.
    Inert,
}

/// A Normal-mode sequence that the key machine reserves without binding an
/// action. Installed by `keybindings::install_registered_normal_bindings`.
#[derive(Debug, Clone, Copy)]
pub struct NormalKeyReservation {
    pub sequence: &'static str,
    pub kind: NormalReservationKind,
    pub reason: &'static str,
}

const fn prefix(sequence: &'static str, reason: &'static str) -> NormalKeyReservation {
    NormalKeyReservation {
        sequence,
        kind: NormalReservationKind::Prefix,
        reason,
    }
}

const fn inert(sequence: &'static str, reason: &'static str) -> NormalKeyReservation {
    NormalKeyReservation {
        sequence,
        kind: NormalReservationKind::Inert,
        reason,
    }
}

const RETIRED_LAUNCHER: &str = "retired modal launcher; the command moved under <Space>g";

pub static NORMAL_KEY_RESERVATIONS: &[NormalKeyReservation] = &[
    prefix("D", "prefix of DD (Vim's D would delete to end of line)"),
    prefix("y", "prefix of yy (Vim's y is the yank operator)"),
    prefix("<Space>", "leader key"),
    prefix("gf", "prefix of gf1..gf9 (Vim's gf would open a file path)"),
    inert("<Space>gr", "retired; the label picker is `:group`"),
    inert("<Space>R", "retired; the rating overlay is `:rate`"),
    inert("gm", "retired merge-queue chord"),
    inert("g?", "retired; the dialectic overlay is `:ask`"),
    inert("gX", RETIRED_LAUNCHER),
    inert("gq", RETIRED_LAUNCHER),
    inert("gn", RETIRED_LAUNCHER),
    inert("gc", RETIRED_LAUNCHER),
    inert("gv", RETIRED_LAUNCHER),
    inert("gp", RETIRED_LAUNCHER),
    inert("gK", RETIRED_LAUNCHER),
    inert("gG", RETIRED_LAUNCHER),
    inert("gE", RETIRED_LAUNCHER),
    inert("gS", RETIRED_LAUNCHER),
    inert("gT", RETIRED_LAUNCHER),
    inert("gB", RETIRED_LAUNCHER),
];

pub static ACTION_DESCRIPTORS: &[ActionDescriptor] = &[
    ActionDescriptor {
        id: ActionId::ContextHelp,
        label: "Contextual help",
        summary: "Opens contextual help listing the keys and commands available in the current view; Ctrl-Alt-G works even while typing.",
        category: "DISCOVERY",
        bindings: CONTEXT_HELP,
        command_aliases: &["help", "keys"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::ContextHelp,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::AllCommands,
        label: "All commands",
        summary: "Opens the complete action, overlay, raw-key and file-viewer reference.",
        category: "DISCOVERY",
        bindings: &[],
        command_aliases: &["commands"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::OpenManual,
        label: "Open manual",
        summary: "Opens the generated manual in a browser, pager or PDF viewer.",
        category: "DISCOVERY",
        bindings: &[],
        command_aliases: &["manual", "man"],
        command_argument: CommandArgument::Optional,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::MoveDown,
        label: "Move selection or scroll down",
        summary: "Moves the selection down one row, or scrolls the focused view down.",
        category: "NAVIGATION",
        bindings: MOVE_DOWN,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Navigation,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::MoveUp,
        label: "Move selection or scroll up",
        summary: "Moves the selection up one row, or scrolls the focused view up.",
        category: "NAVIGATION",
        bindings: MOVE_UP,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Navigation,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::JumpTop,
        label: "Jump to first item",
        summary: "Jumps the session-list selection to the first row.",
        category: "NAVIGATION",
        bindings: JUMP_TOP,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::SessionList,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::JumpBottom,
        label: "Jump to last item",
        summary: "Jumps the session-list selection to the last row.",
        category: "NAVIGATION",
        bindings: JUMP_BOTTOM,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::SessionList,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::Open,
        label: "Open or activate selection",
        summary: "Opens the selected row: drills into a Group or Epic in place, opens a leaf session's detail, or activates the selected setting.",
        category: "CURRENT VIEW",
        bindings: OPEN,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::ContextualOpen,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::Search,
        label: "Filter sessions",
        summary: "Starts an incremental `/` filter over the session list; Enter keeps the filter, Esc clears it.",
        category: "SESSION LIST",
        bindings: NORMAL_SEARCH,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::SessionList,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::Refresh,
        label: "Refresh navigation",
        summary: "Re-fetches sessions, projects and labels from the daemon.",
        category: "SESSION LIST",
        bindings: NORMAL_REFRESH,
        command_aliases: &["refresh"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::SessionListConnected,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::OpenThemePicker,
        label: "Choose built-in theme",
        summary: "Opens the built-in theme picker; `:theme <name>` applies a theme directly.",
        category: "THEME & COLORS",
        bindings: NORMAL_THEME_PICKER,
        command_aliases: &["theme"],
        command_argument: CommandArgument::Optional,
        availability: AvailabilitySelector::SessionList,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::OpenLegacyColors,
        label: "Edit legacy message/editor colors",
        summary: "Opens the legacy message and editor color customizer.",
        category: "THEME & COLORS",
        bindings: NORMAL_LEGACY_COLORS,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::SessionList,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::OpenIssuesWorkspace,
        label: "Open or focus Issues workspace",
        summary: "Opens the Issues workspace pane for the current project, or focuses it if already open.",
        category: "OPERATOR VIEWS",
        bindings: NORMAL_ISSUES_WORKSPACE,
        command_aliases: &["issues"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ManagerPolicy,
        label: "Edit manager policy",
        summary: "Opens the harness manager policy editor (operator-owned manager limits and choices).",
        category: "MANAGER",
        bindings: NORMAL_MANAGER_POLICY,
        command_aliases: &["manager policy"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ManagerBoard,
        label: "Open manager board",
        summary: "Opens the harness manager work board.",
        category: "MANAGER",
        bindings: NORMAL_MANAGER_BOARD,
        command_aliases: &["manager board"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ManagerDecisions,
        label: "Open manager decisions",
        summary: "Opens the harness manager decisions queue awaiting the operator.",
        category: "MANAGER",
        bindings: NORMAL_MANAGER_DECISIONS,
        command_aliases: &["manager decisions"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ManagerInbox,
        label: "Open manager inbox",
        summary: "Opens the harness manager inbox of lead requests and replies.",
        category: "MANAGER",
        bindings: &[],
        command_aliases: &["manager inbox"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ManagerInspect,
        label: "Open manager inspect",
        summary: "Opens the harness manager inspect view (workers, work, requests, topology and events).",
        category: "MANAGER",
        bindings: &[],
        command_aliases: &["manager inspect"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::CopySessionUuid,
        label: "Copy Session UUID",
        summary: "Copies the selected session's UUID; in a transcript `yy` copies the selected event's content instead.",
        category: "SESSION",
        bindings: NORMAL_COPY_SESSION_UUID,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::CopySessionUuid,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::InterruptSession,
        label: "Interrupt running session",
        summary: "Interrupts the selected running session.",
        category: "SESSION",
        bindings: NORMAL_INTERRUPT,
        command_aliases: &["kill", "ki"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::RunningLeafSessionMutation,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ContinueSession,
        label: "Continue selected session",
        summary: "Continues the selected session: opens a continue prompt, or `:continue <text>` sends the text directly.",
        category: "SESSION",
        bindings: NORMAL_CONTINUE,
        command_aliases: &["continue", "cont"],
        command_argument: CommandArgument::Optional,
        availability: AvailabilitySelector::ContinueSessionMutation,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::TogglePin,
        label: "Pin or unpin selected session",
        summary: "Pins or unpins the selected session at the top of the list.",
        category: "SESSION",
        bindings: NORMAL_TOGGLE_PIN,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::PinSessionMutation,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ArchiveSession,
        label: "Archive selected session",
        summary: "Archives the selected session (reversible with U from the Archive zone).",
        category: "SESSION",
        bindings: NORMAL_ARCHIVE,
        command_aliases: &["archive", "arc"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::ArchiveSessionMutation,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::UnarchiveSession,
        label: "Unarchive selected session",
        summary: "Restores the selected archived session to the Main zone.",
        category: "SESSION",
        bindings: NORMAL_UNARCHIVE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::ArchivedSessionListMutation,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ToggleTestingNeeded,
        label: "Toggle testing-needed marker",
        summary: "Marks or clears the testing-needed flag on the selected leaf session.",
        category: "SESSION",
        bindings: NORMAL_TOGGLE_TESTING,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::TestingLeafSessionMutation,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::AscendHierarchy,
        label: "Ascend hierarchy",
        summary: "Ascends one container level in the session hierarchy; does nothing at the root.",
        category: "NAVIGATION",
        bindings: NORMAL_ASCEND,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::HierarchyAscend,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ResetThemeRole,
        label: "Reset selected role",
        summary: "Resets the selected theme role to the active theme's default color.",
        category: "THEME & COLORS",
        bindings: SETTINGS_RESET,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::ThemeRoleOverride,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::NavigateBack,
        label: "Return to categories",
        summary: "Returns focus from the settings items to the category rail.",
        category: "SETTINGS",
        bindings: SETTINGS_BACK,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::SettingsItems,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::NavigateForward,
        label: "Enter selected category",
        summary: "Moves focus from the category rail into the selected category's items.",
        category: "SETTINGS",
        bindings: SETTINGS_FORWARD,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::SettingsCategories,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ToggleSetting,
        label: "Toggle or activate setting",
        summary: "Toggles a boolean setting, cycles a choice, or activates the selected settings row.",
        category: "SETTINGS",
        bindings: SETTINGS_TOGGLE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::SettingsEditable,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::SettingsAdd,
        label: "Add item",
        summary: "Adds an item to the selected settings list (hooks, providers, budgets and similar lists).",
        category: "SETTINGS",
        bindings: SETTINGS_ADD,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::SettingsAdd,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::SettingsDelete,
        label: "Delete item",
        summary: "Deletes the selected item from a settings list.",
        category: "SETTINGS",
        bindings: SETTINGS_DELETE_ITEM,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::SettingsDelete,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::SettingsEnable,
        label: "Enable or disable skill",
        summary: "Enables or disables the selected Claude skill.",
        category: "SETTINGS",
        bindings: SETTINGS_ENABLE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::SettingsEnable,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::SettingsRefresh,
        label: "Refresh daemon-backed state",
        summary: "Re-reads daemon-backed settings state (config, hooks, skills, storage).",
        category: "SETTINGS",
        bindings: SETTINGS_REFRESH,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::SettingsRefresh,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::SettingsSearch,
        label: "Search settings",
        summary: "Opens an incremental settings query; Enter jumps to the first match at or after the cursor.",
        category: "SETTINGS",
        bindings: SETTINGS_SEARCH,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::SettingsItems,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::SettingsNextMatch,
        label: "Next settings match",
        summary: "Jumps to the next settings row matching the active query, wrapping with a notice.",
        category: "SETTINGS",
        bindings: SETTINGS_NEXT_MATCH,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::SettingsItems,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::SettingsPrevMatch,
        label: "Previous settings match",
        summary: "Jumps to the previous settings row matching the active query, wrapping with a notice.",
        category: "SETTINGS",
        bindings: SETTINGS_PREV_MATCH,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::SettingsItems,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueOpenSession,
        label: "Inspect or open linked session",
        summary: "Inspects the selected issue, or opens its linked session when one exists.",
        category: "ISSUES",
        bindings: ISSUE_OPEN,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueSelection,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueRefresh,
        label: "Refresh active Issue view",
        summary: "Refreshes the active Issues tab from the daemon.",
        category: "ISSUES",
        bindings: ISSUE_REFRESH,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueRefresh,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueRunPoll,
        label: "Run poll now",
        summary: "Runs the issue sync poll now instead of waiting for its schedule.",
        category: "ISSUES",
        bindings: ISSUE_RUN_POLL,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueSyncPoll,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueSelectFormValue,
        label: "Select highlighted form value",
        summary: "Selects the highlighted value in the open issue form field.",
        category: "ISSUES",
        bindings: ISSUE_SELECT_FORM_VALUE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueFormSelection,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueRetryMutation,
        label: "Retry identical Issue write",
        summary: "Retries the last failed issue write with the identical request and idempotency key.",
        category: "ISSUES",
        bindings: ISSUE_RETRY_MUTATION,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueRetryMutation,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueInspectorNextSection,
        label: "Select next inspector history section",
        summary: "Moves the issue inspector to its next history section.",
        category: "ISSUES",
        bindings: ISSUE_INSPECTOR_NEXT_SECTION,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocalInspector,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueInspectorPreviousPage,
        label: "Load previous inspector page",
        summary: "Loads the previous page of the issue inspector history.",
        category: "ISSUES",
        bindings: ISSUE_INSPECTOR_PREVIOUS_PAGE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocalInspector,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueInspectorNextPage,
        label: "Load next inspector page",
        summary: "Loads the next page of the issue inspector history.",
        category: "ISSUES",
        bindings: ISSUE_INSPECTOR_NEXT_PAGE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocalInspector,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueNew,
        label: "New issue",
        summary: "Opens the editor to create a new issue.",
        category: "ISSUES",
        bindings: ISSUE_NEW,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocal,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueEdit,
        label: "Edit issue",
        summary: "Opens the editor on the selected issue.",
        category: "ISSUES",
        bindings: ISSUE_EDIT,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocalActiveSelection,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueStatus,
        label: "Change lifecycle status",
        summary: "Changes the selected issue's lifecycle status.",
        category: "ISSUES",
        bindings: ISSUE_STATUS,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocalActiveSelection,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueReopen,
        label: "Reopen terminal issue",
        summary: "Reopens a closed or cancelled issue.",
        category: "ISSUES",
        bindings: ISSUE_REOPEN,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocalTerminalSelection,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssuePriority,
        label: "Change priority",
        summary: "Edits the selected issue's priority.",
        category: "ISSUES",
        bindings: ISSUE_PRIORITY,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocalActiveSelection,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueAssignee,
        label: "Assign or unassign",
        summary: "Assigns or unassigns the selected issue.",
        category: "ISSUES",
        bindings: ISSUE_ASSIGNEE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocalActiveSelection,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueLabels,
        label: "Edit labels",
        summary: "Edits the selected issue's labels.",
        category: "ISSUES",
        bindings: ISSUE_LABELS,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocalActiveSelection,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueDependencies,
        label: "Edit dependencies",
        summary: "Edits the selected issue's blocked-by and blocks dependencies.",
        category: "ISSUES",
        bindings: ISSUE_DEPENDENCIES,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocalDependencySelection,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueReloadLatest,
        label: "Reload latest Issue projection",
        summary: "Replaces a stale draft's base with the latest server version of the issue.",
        category: "ISSUES",
        bindings: ISSUE_RELOAD_LATEST,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueStaleEditor,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueRebaseDraft,
        label: "Rebase draft onto latest Issue",
        summary: "Rebases the stale draft's edits onto the latest server version of the issue.",
        category: "ISSUES",
        bindings: ISSUE_REBASE_DRAFT,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueStaleEditor,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueArmCancel,
        label: "Arm issue cancellation",
        summary: "First `d` of `dd`: arms cancellation of the selected issue.",
        category: "ISSUES",
        bindings: ISSUE_ARM_CANCEL,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocalActiveSelection,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::IssueCancel,
        label: "Cancel issue",
        summary: "Cancels the selected issue (`dd`).",
        category: "ISSUES",
        bindings: ISSUE_CANCEL,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueCancelConfirmation,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueArchive,
        label: "Archive terminal issue",
        summary: "Archives the selected terminal (closed or cancelled) issue after confirmation.",
        category: "ISSUES",
        bindings: ISSUE_ARCHIVE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocalTerminalSelection,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueConfirmArchive,
        label: "Confirm archive",
        summary: "Confirms the pending issue archive.",
        category: "ISSUES",
        bindings: ISSUE_CONFIRM_ARCHIVE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueArchiveConfirmation,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueRestore,
        label: "Restore archived issue",
        summary: "Restores the selected archived issue.",
        category: "ISSUES",
        bindings: ISSUE_RESTORE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocalArchivedTerminalSelection,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueYankArm,
        label: "Start issue copy command",
        summary: "First `y` of `yy` / `y#`: starts an issue copy command.",
        category: "ISSUES",
        bindings: ISSUE_YANK_ARM,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocalSelection,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::IssueCopyUuid,
        label: "Copy Issue UUID",
        summary: "Copies the selected issue's UUID (`yy`).",
        category: "ISSUES",
        bindings: ISSUE_COPY_UUID,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocalSelection,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueCopyNumber,
        label: "Copy Issue number",
        summary: "Copies the selected issue's display number (`y#`).",
        category: "ISSUES",
        bindings: ISSUE_COPY_NUMBER,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocalSelection,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssuePreviousTab,
        label: "Previous Issues tab",
        summary: "Switches to the previous Issues tab.",
        category: "ISSUES",
        bindings: ISSUE_PREVIOUS_TAB,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueTracker,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueNextTab,
        label: "Next Issues tab",
        summary: "Switches to the next Issues tab.",
        category: "ISSUES",
        bindings: ISSUE_NEXT_TAB,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueTracker,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueSearch,
        label: "Search local issues",
        summary: "Searches the locally loaded issues.",
        category: "ISSUES",
        bindings: ISSUE_SEARCH,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocal,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueFilter,
        label: "Edit filters and saved views",
        summary: "Edits the issue filters and saved views.",
        category: "ISSUES",
        bindings: ISSUE_FILTER,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocal,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueSort,
        label: "Cycle issue sort",
        summary: "Cycles the issue sort order.",
        category: "ISSUES",
        bindings: ISSUE_SORT,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocal,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueJumpArm,
        label: "Start first-page command",
        summary: "First `g` of `gg`: starts the first-page jump.",
        category: "ISSUES",
        bindings: ISSUE_JUMP_ARM,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocal,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::IssueJumpStart,
        label: "Load first issue page",
        summary: "Loads the first issue page (`gg`).",
        category: "ISSUES",
        bindings: ISSUE_JUMP_START,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocal,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::IssueJumpEnd,
        label: "Load last issue page",
        summary: "Loads the last issue page.",
        category: "ISSUES",
        bindings: ISSUE_JUMP_END,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::IssueLocal,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ScheduleNew,
        label: "Create scheduled job",
        summary: "Opens the form to create a scheduled job.",
        category: "SCHEDULED JOBS",
        bindings: SCHEDULE_NEW,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::ScheduleMutation,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ScheduleEdit,
        label: "Edit selected job",
        summary: "Opens the selected scheduled job in the edit form.",
        category: "SCHEDULED JOBS",
        bindings: SCHEDULE_EDIT,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::ScheduleMutation,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ScheduleToggle,
        label: "Enable or disable job",
        summary: "Enables or disables the selected scheduled job.",
        category: "SCHEDULED JOBS",
        bindings: SCHEDULE_TOGGLE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::ScheduleMutation,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ScheduleTrigger,
        label: "Run selected job now",
        summary: "Runs the selected scheduled job now.",
        category: "SCHEDULED JOBS",
        bindings: SCHEDULE_TRIGGER,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::ScheduleMutation,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ScheduleArmDelete,
        label: "Begin delete chord",
        summary: "First `d` of `dd`: arms deletion of the selected scheduled job.",
        category: "SCHEDULED JOBS",
        bindings: SCHEDULE_ARM_DELETE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        // The first `d` is only a local chord arm; the daemon-backed delete
        // itself remains gated by connection/loading state below.
        availability: AvailabilitySelector::ScheduleSelection,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::ScheduleDelete,
        label: "Delete selected job",
        summary: "Deletes the selected scheduled job (`dd`).",
        category: "SCHEDULED JOBS",
        bindings: SCHEDULE_DELETE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::ScheduleMutation,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ScheduleRefresh,
        label: "Refresh scheduled jobs",
        summary: "Re-reads the scheduled jobs from the daemon.",
        category: "SCHEDULED JOBS",
        bindings: SCHEDULE_REFRESH,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Connected,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ThemeRoleCommit,
        label: "Preview or commit color",
        summary: "Previews the typed color, or commits it to the role.",
        category: "THEME ROLE",
        bindings: EDITOR_COMMIT,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::ThemeRoleEditor,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ThemeRoleReset,
        label: "Reset this role",
        summary: "Resets this theme role to the active theme's default color.",
        category: "THEME ROLE",
        bindings: EDITOR_RESET,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::ThemeRoleEditor,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::Close,
        label: "Close or cancel",
        summary: "Closes the current view or cancels the pending action.",
        category: "CURRENT VIEW",
        bindings: CLOSE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::DeleteSession,
        label: "Delete selected session",
        summary: "Deletes the selected session after the double-tap `DD` (moves it to the trash).",
        category: "SESSION",
        bindings: NORMAL_DELETE_SESSION,
        command_aliases: &["delete", "del"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::SessionList,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::RotateSession,
        label: "Rotate session context",
        summary: "Rotates the selected session into a fresh context, carrying a handoff forward.",
        category: "SESSION",
        bindings: NORMAL_ROTATE_SESSION,
        command_aliases: &["rotate", "rot"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::SessionList,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::Archives,
        label: "Open archives",
        summary: "Switches the session list to the Archive zone.",
        category: "NAVIGATION",
        bindings: NORMAL_ARCHIVES,
        command_aliases: &["archives"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::Model,
        label: "Choose model",
        summary: "Opens the model picker, or `:model <name>` selects a model for new launches.",
        category: "SESSION",
        bindings: &[],
        command_aliases: &["model", "mod"],
        command_argument: CommandArgument::Optional,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::Sessions,
        label: "List sessions",
        summary: "Lists sessions in the session list.",
        category: "NAVIGATION",
        bindings: &[],
        command_aliases: &["sessions", "ls"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::Alerts,
        label: "Show attention alerts",
        summary: "Toggles the notification and attention history overlay.",
        category: "NAVIGATION",
        bindings: NORMAL_ALERTS,
        command_aliases: &["alerts", "att", "attention"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::Quit,
        label: "Quit",
        summary: "Quits rsi (the daemon and its sessions keep running).",
        category: "WINDOW",
        bindings: NORMAL_QUIT,
        command_aliases: &["quit", "q", "qall", "qa", "quit!", "q!"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::Projects,
        label: "Choose project",
        summary: "Opens the project picker.",
        category: "PROJECT",
        bindings: NORMAL_PROJECTS,
        command_aliases: &["projects"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::Project,
        label: "Switch project",
        summary: "Switches to the named project, or opens the picker without a name.",
        category: "PROJECT",
        bindings: &[],
        command_aliases: &["project"],
        command_argument: CommandArgument::Optional,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::Task,
        label: "Launch task session",
        summary: "Opens the TaskRabbit one-shot prompt; `:task <text>` launches it directly.",
        category: "SESSION",
        bindings: NORMAL_TASK,
        command_aliases: &["task", "ta"],
        command_argument: CommandArgument::Optional,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::Blank,
        label: "Launch blank session",
        summary: "Opens the blank general-purpose session prompt; `:blank <text>` launches it directly.",
        category: "SESSION",
        bindings: NORMAL_BLANK,
        command_aliases: &["blank", "bl"],
        command_argument: CommandArgument::Optional,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ProjectNew,
        label: "Create project",
        summary: "Creates a project: `:project-new <name> [path]`, or opens the form.",
        category: "PROJECT",
        bindings: &[],
        command_aliases: &["project-new"],
        command_argument: CommandArgument::Optional,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::ProjectEdit,
        label: "Edit project",
        summary: "Edits the named or current project.",
        category: "PROJECT",
        bindings: &[],
        command_aliases: &["project-edit"],
        command_argument: CommandArgument::Optional,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::ProjectDelete,
        label: "Delete project",
        summary: "Deletes the named project; the name argument is required.",
        category: "PROJECT",
        bindings: &[],
        command_aliases: &["project-delete"],
        command_argument: CommandArgument::Required,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::SettingsCommand,
        label: "Open settings",
        summary: "Opens the settings pane.",
        category: "SETTINGS",
        bindings: NORMAL_SETTINGS_COMMAND,
        command_aliases: &["set", "settings"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::StopAll,
        label: "Emergency stop all",
        summary: "Emergency stop: denies new paid work and cancels live model invocations.",
        category: "SESSION",
        bindings: NORMAL_STOP_ALL,
        command_aliases: &["stopall", "stop-all"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::Hooks,
        label: "Open hook settings",
        summary: "Opens settings at the Claude hooks list.",
        category: "SETTINGS",
        bindings: &[],
        command_aliases: &["hooks"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::Skills,
        label: "Open skill settings",
        summary: "Opens settings at the Claude skills list.",
        category: "SETTINGS",
        bindings: &[],
        command_aliases: &["skills"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::Group,
        label: "Choose group label",
        summary: "Opens the group label picker for the selected session.",
        category: "SESSION",
        bindings: &[],
        command_aliases: &["group", "groups"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::Diagnostics,
        label: "Open diagnostics",
        summary: "Opens the diagnostics overlay.",
        category: "OPERATOR VIEWS",
        bindings: &[],
        command_aliases: &["diagnostics", "diag"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::Graph,
        label: "Open graph review",
        summary: "Opens the visual workflow graph review editor.",
        category: "OPERATOR VIEWS",
        bindings: NORMAL_GRAPH,
        command_aliases: &["graph"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::TopologyResolve,
        label: "Resolve preserved topology work",
        summary: "Inspects, accepts, retries or discards preserved work on a blocked durable topology execution: `:topology-resolve [<execution_id>] inspect|accept|retry|discard [<commit>]` (the id may be omitted when exactly one is blocked; discard needs the full preserved commit).",
        category: "OPERATOR VIEWS",
        bindings: &[],
        command_aliases: &["topology-resolve"],
        command_argument: CommandArgument::Optional,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::Dag,
        label: "Open recursive DAG",
        summary: "Opens the recursive DAG browser.",
        category: "OPERATOR VIEWS",
        bindings: &[],
        command_aliases: &["dag"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::Context,
        label: "Set active task context",
        summary: "Sets or clears the active task context for new launches.",
        category: "SESSION",
        bindings: &[],
        command_aliases: &["context", "ctx"],
        command_argument: CommandArgument::Optional,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::Card,
        label: "Edit entity card",
        summary: "Opens the entity card editor for the current project or the named entity.",
        category: "PROJECT",
        bindings: &[],
        command_aliases: &["card"],
        command_argument: CommandArgument::Optional,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::Ask,
        label: "Ask a question",
        summary: "Opens the dialectic question overlay; `:ask <question>` asks directly.",
        category: "OPERATOR VIEWS",
        bindings: &[],
        command_aliases: &["ask"],
        command_argument: CommandArgument::Optional,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::Terminal,
        label: "Toggle terminal",
        summary: "Toggles the embedded terminal overlay (the shell keeps running when hidden).",
        category: "WINDOW",
        bindings: &[],
        command_aliases: &["term", "terminal", "shell"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::Lead,
        label: "Set Epic lead",
        summary: "Sets the focused leaf session as the lead of its parent Epic.",
        category: "MANAGER",
        bindings: NORMAL_LEAD,
        command_aliases: &["lead", "setlead"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::Manager,
        label: "Open harness manager",
        summary: "Opens the harness manager overlay.",
        category: "MANAGER",
        bindings: &[],
        command_aliases: &["manager"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::ManagerAppoint,
        label: "Appoint harness manager",
        summary: "Appoints the selected session as the harness manager.",
        category: "MANAGER",
        bindings: &[],
        command_aliases: &["manager appoint"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::ManagerScope,
        label: "Edit manager scope",
        summary: "Edits the harness manager's scope.",
        category: "MANAGER",
        bindings: &[],
        command_aliases: &["manager scope"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::ManagerClear,
        label: "Clear manager scope",
        summary: "Clears the harness manager's scope.",
        category: "MANAGER",
        bindings: &[],
        command_aliases: &["manager clear"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::Rate,
        label: "Rate selected session",
        summary: "Rates the selected session 1-10: `:rate <n>`, or opens the rating overlay (digits 1-9, 0 = 10).",
        category: "SESSION",
        bindings: &[],
        command_aliases: &["rate", "r"],
        command_argument: CommandArgument::Optional,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::Split,
        label: "Split pane horizontally",
        summary: "Splits the focused pane horizontally (handled by the pane's window commands).",
        category: "WINDOW",
        bindings: &[],
        command_aliases: &["split", "sp"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::VSplit,
        label: "Split pane vertically",
        summary: "Splits the focused pane vertically (handled by the pane's window commands).",
        category: "WINDOW",
        bindings: &[],
        command_aliases: &["vsplit", "vs"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::ClosePane,
        label: "Close pane",
        summary: "Closes the focused pane.",
        category: "WINDOW",
        bindings: NORMAL_CLOSE_PANE,
        command_aliases: &["close", "clo"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::OnlyPane,
        label: "Close other panes",
        summary: "Closes every pane except the focused one (handled by the pane's window commands).",
        category: "WINDOW",
        bindings: &[],
        command_aliases: &["only", "on"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::TabNew,
        label: "Create tab",
        summary: "Opens a new tab (handled by the pane's window commands).",
        category: "WINDOW",
        bindings: &[],
        command_aliases: &["tabnew", "tabe"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::TabClose,
        label: "Close tab",
        summary: "Closes the current tab (handled by the pane's window commands).",
        category: "WINDOW",
        bindings: &[],
        command_aliases: &["tabclose", "tabc"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: false,
    },
    ActionDescriptor {
        id: ActionId::TabNext,
        label: "Next tab",
        summary: "Switches to the next tab.",
        category: "WINDOW",
        bindings: NORMAL_TAB_NEXT,
        command_aliases: &["tabnext", "tabn"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::TabPrev,
        label: "Previous tab",
        summary: "Switches to the previous tab.",
        category: "WINDOW",
        bindings: NORMAL_TAB_PREV,
        command_aliases: &["tabprev", "tabp", "tabprevious"],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::SearchNext,
        label: "Next search match",
        summary: "Moves to the next match of the confirmed `/` search.",
        category: "SESSION LIST",
        bindings: NORMAL_SEARCH_NEXT,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::SearchPrev,
        label: "Previous search match",
        summary: "Moves to the previous match of the confirmed `/` search.",
        category: "SESSION LIST",
        bindings: NORMAL_SEARCH_PREV,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::NextAttention,
        label: "Next session needing attention",
        summary: "Jumps to the next session waiting for you (approval, question or failure).",
        category: "NAVIGATION",
        bindings: NORMAL_NEXT_ATTENTION,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::PrevAttention,
        label: "Previous session needing attention",
        summary: "Jumps to the previous session waiting for you.",
        category: "NAVIGATION",
        bindings: NORMAL_PREV_ATTENTION,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::JumpAttention,
        label: "Jump to attention slot N",
        summary: "Jumps to the Nth session in the attention queue (`<Space>1` to `<Space>9`; bare digits stay Vim counts).",
        category: "NAVIGATION",
        bindings: NORMAL_JUMP_ATTENTION,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::NextLabelGroup,
        label: "Next label group",
        summary: "Moves the selection to the first session of the next label group.",
        category: "SESSION LIST",
        bindings: NORMAL_NEXT_LABEL_GROUP,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::PrevLabelGroup,
        label: "Previous label group",
        summary: "Moves the selection to the first session of the previous label group.",
        category: "SESSION LIST",
        bindings: NORMAL_PREV_LABEL_GROUP,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::GoToMainZone,
        label: "Go to Main zone",
        summary: "Switches the session list to the Main zone.",
        category: "SESSION LIST",
        bindings: NORMAL_GO_TO_MAIN_ZONE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::GoToTaskRabbitZone,
        label: "Go to TaskRabbit zone",
        summary: "Switches the session list to the TaskRabbit zone of one-shot sessions.",
        category: "SESSION LIST",
        bindings: NORMAL_GO_TO_TASK_RABBIT_ZONE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::GoToJobsZone,
        label: "Go to Jobs zone",
        summary: "Switches the session list to the Jobs zone of scheduled-job sessions.",
        category: "SESSION LIST",
        bindings: NORMAL_GO_TO_JOBS_ZONE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::RecentCompletions,
        label: "Recent completions",
        summary: "Focuses the recent-completions list in the right sidebar.",
        category: "SESSION LIST",
        bindings: NORMAL_RECENT_COMPLETIONS,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::NavigateRight,
        label: "Enter from list",
        summary: "Enters the selected session from the list (moves focus right).",
        category: "NAVIGATION",
        bindings: NORMAL_NAVIGATE_RIGHT,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::AscendOrBack,
        label: "Ascend or go back",
        summary: "Ascends one container level, or returns from session detail to the list at the root.",
        category: "NAVIGATION",
        bindings: NORMAL_ASCEND_OR_BACK,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::SortPicker,
        label: "Choose sort order",
        summary: "Opens the session-list sort order picker.",
        category: "SESSION LIST",
        bindings: NORMAL_SORT_PICKER,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::Trash,
        label: "Open trash",
        summary: "Opens the trash browser of deleted sessions.",
        category: "SESSION LIST",
        bindings: NORMAL_TRASH,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::EnterInputInsert,
        label: "Type into the input bar",
        summary: "Enters the input bar in insert mode: `i` insert, `a` append, `o` / `O` open a new line below / above.",
        category: "TRANSCRIPT",
        bindings: NORMAL_ENTER_INPUT_INSERT,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::EnterSessionNormal,
        label: "Session normal mode",
        summary: "Enters the selected session's detail with the input bar in normal mode.",
        category: "TRANSCRIPT",
        bindings: NORMAL_ENTER_SESSION_NORMAL,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::PromptPreview,
        label: "Preview session prompt",
        summary: "Shows the full launch prompt of the selected session.",
        category: "TRANSCRIPT",
        bindings: NORMAL_PROMPT_PREVIEW,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::NextUserMessage,
        label: "Next user message",
        summary: "Selects the next user message in the transcript.",
        category: "TRANSCRIPT",
        bindings: NORMAL_NEXT_USER_MESSAGE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::PrevUserMessage,
        label: "Previous user message",
        summary: "Selects the previous user message in the transcript.",
        category: "TRANSCRIPT",
        bindings: NORMAL_PREV_USER_MESSAGE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::OpenFold,
        label: "Open fold",
        summary: "Expands the selected transcript event.",
        category: "TRANSCRIPT",
        bindings: NORMAL_OPEN_FOLD,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::CloseFold,
        label: "Close fold",
        summary: "Collapses the selected transcript event.",
        category: "TRANSCRIPT",
        bindings: NORMAL_CLOSE_FOLD,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ToggleFold,
        label: "Toggle fold",
        summary: "Toggles the selected transcript event between expanded and collapsed.",
        category: "TRANSCRIPT",
        bindings: NORMAL_TOGGLE_FOLD,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::CloseAllFolds,
        label: "Close all folds",
        summary: "Collapses every event in the transcript.",
        category: "TRANSCRIPT",
        bindings: NORMAL_CLOSE_ALL_FOLDS,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::OpenAllFolds,
        label: "Open all folds",
        summary: "Expands every event in the transcript.",
        category: "TRANSCRIPT",
        bindings: NORMAL_OPEN_ALL_FOLDS,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ToggleSystemEvents,
        label: "Toggle system events",
        summary: "Shows or hides system events in this transcript.",
        category: "TRANSCRIPT",
        bindings: NORMAL_TOGGLE_SYSTEM_EVENTS,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ToggleThinkingEvents,
        label: "Toggle thinking events",
        summary: "Shows or hides model thinking events in this transcript.",
        category: "TRANSCRIPT",
        bindings: NORMAL_TOGGLE_THINKING_EVENTS,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::SessionInfo,
        label: "Session info",
        summary: "Opens the session info panel: id, provider and model, working directory, project, hierarchy, rating (1-10), label, tags and context usage.",
        category: "TRANSCRIPT",
        bindings: NORMAL_SESSION_INFO,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::RenameSession,
        label: "Rename session",
        summary: "Renames the selected session inline.",
        category: "SESSION",
        bindings: NORMAL_RENAME_SESSION,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ModelDropdown,
        label: "Model dropdown",
        summary: "Opens or closes the model dropdown for the next launch (plain `M` stays free for text surfaces).",
        category: "SESSION",
        bindings: NORMAL_MODEL_DROPDOWN,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ReassignProject,
        label: "Change session project",
        summary: "Moves the selected session to another project.",
        category: "PROJECT",
        bindings: NORMAL_REASSIGN_PROJECT,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ToggleRotation,
        label: "Toggle auto-rotation",
        summary: "Disables or re-enables automatic context rotation for the selected session.",
        category: "SESSION",
        bindings: NORMAL_TOGGLE_ROTATION,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::CancelRetry,
        label: "Cancel pending retry",
        summary: "Cancels the selected session's pending automatic retry.",
        category: "SESSION",
        bindings: NORMAL_CANCEL_RETRY,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ExecuteDocRegBlocks,
        label: "Run docregblock tags",
        summary: "Launches a session for every docregblock tag in the selected session's output.",
        category: "SESSION",
        bindings: NORMAL_EXECUTE_DOC_REG_BLOCKS,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::CommitAndPush,
        label: "Commit and push",
        summary: "Continues the selected session with `/ci_commit` to commit and push its work.",
        category: "SESSION",
        bindings: NORMAL_COMMIT_AND_PUSH,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::OpenSessionInNewTab,
        label: "Open in new tab",
        summary: "Opens the selected session in a new tab.",
        category: "WINDOW",
        bindings: NORMAL_OPEN_SESSION_IN_NEW_TAB,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::GitPanel,
        label: "Git panel (lazygit)",
        summary: "Suspends the TUI and runs lazygit in the session's working directory.",
        category: "FILES",
        bindings: NORMAL_GIT_PANEL,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::FileExplorer,
        label: "File explorer",
        summary: "Toggles the left-anchored file explorer drawer.",
        category: "FILES",
        bindings: NORMAL_FILE_EXPLORER,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::Telescope,
        label: "Find file",
        summary: "Opens the fuzzy file finder.",
        category: "FILES",
        bindings: NORMAL_TELESCOPE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::OpenRecentFile,
        label: "Open recent file N",
        summary: "Opens the Nth most recent file (`gf1` to `gf9`).",
        category: "FILES",
        bindings: NORMAL_OPEN_RECENT_FILE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::PromptCreator,
        label: "Prompt creator",
        summary: "Opens the prompt creator and editor.",
        category: "FILES",
        bindings: NORMAL_PROMPT_CREATOR,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::CommandPalette,
        label: "Command palette",
        summary: "Opens the command palette, the same palette as `:`.",
        category: "DISCOVERY",
        bindings: NORMAL_COMMAND_PALETTE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::QuestionModal,
        label: "Answer waiting question",
        summary: "Opens the question modal for a session waiting on your answer.",
        category: "OPERATOR VIEWS",
        bindings: NORMAL_QUESTION_MODAL,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::MemorySearch,
        label: "Memory search",
        summary: "Opens memory search over the daemon's stored memories.",
        category: "OPERATOR VIEWS",
        bindings: NORMAL_MEMORY_SEARCH,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::ScheduleBrowserOpen,
        label: "Scheduled jobs",
        summary: "Opens the scheduled jobs browser.",
        category: "SCHEDULED JOBS",
        bindings: NORMAL_SCHEDULE_BROWSER_OPEN,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::EspSquare,
        label: "ESP Square game",
        summary: "Opens the ESP Square guessing game.",
        category: "OPERATOR VIEWS",
        bindings: NORMAL_ESP_SQUARE,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::RunEpicTopology,
        label: "Run Epic topology",
        summary: "Runs the focused Epic's bound workflow topology.",
        category: "HIERARCHY",
        bindings: NORMAL_RUN_EPIC_TOPOLOGY,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::CreateGroup,
        label: "Create Group",
        summary: "Creates a top-level Group container.",
        category: "HIERARCHY",
        bindings: NORMAL_CREATE_GROUP,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::CreateEpic,
        label: "Create Epic",
        summary: "Creates an Epic container under the current Group.",
        category: "HIERARCHY",
        bindings: NORMAL_CREATE_EPIC,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::CreateStory,
        label: "Create Story",
        summary: "Creates a Story leaf under the current Epic.",
        category: "HIERARCHY",
        bindings: NORMAL_CREATE_STORY,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::CreateTask,
        label: "Create Task",
        summary: "Creates a Task leaf under the current Epic.",
        category: "HIERARCHY",
        bindings: NORMAL_CREATE_TASK,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::CreateBug,
        label: "Create Bug",
        summary: "Creates a Bug leaf under the current Epic.",
        category: "HIERARCHY",
        bindings: NORMAL_CREATE_BUG,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::MoveToParent,
        label: "Move to parent",
        summary: "Opens the parent picker to move the focused session under another container.",
        category: "HIERARCHY",
        bindings: NORMAL_MOVE_TO_PARENT,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
    ActionDescriptor {
        id: ActionId::MoveToRoot,
        label: "Move to top level",
        summary: "Moves the focused session to the top level.",
        category: "HIERARCHY",
        bindings: NORMAL_MOVE_TO_ROOT,
        command_aliases: &[],
        command_argument: CommandArgument::None,
        availability: AvailabilitySelector::Always,
        show_in_help: true,
    },
];

fn route_for_surface(surface: ActionSurface) -> Option<ActionRoute> {
    match surface {
        ActionSurface::SessionList => Some(ActionRoute::Normal),
        ActionSurface::Settings => Some(ActionRoute::Settings),
        ActionSurface::IssueTracker => Some(ActionRoute::IssueTracker),
        ActionSurface::ScheduleBrowser => Some(ActionRoute::ScheduleBrowser),
        ActionSurface::ThemeRoleEditor => Some(ActionRoute::ThemeRoleEditor),
        // Session detail and Prompt Creator use the normal Vim machine after
        // their local key handlers decline a key. Their actions therefore use
        // the Normal route even though they have distinct help origins.
        ActionSurface::Other => Some(ActionRoute::Normal),
    }
}

fn materialize(context: &ActionContext, id: ActionId) -> ActionRequest {
    let payload = match id {
        ActionId::IssueOpenSession => context
            .selected_issue_session_id
            .map(ActionPayload::Uuid)
            .unwrap_or(ActionPayload::None),
        ActionId::IssueCopyUuid | ActionId::IssueCopyNumber => context
            .selected_id
            .map(ActionPayload::Uuid)
            .unwrap_or(ActionPayload::None),
        ActionId::CopySessionUuid
        | ActionId::ScheduleEdit
        | ActionId::ScheduleToggle
        | ActionId::ScheduleTrigger
        | ActionId::ScheduleDelete => context
            .selected_id
            .map(ActionPayload::Uuid)
            .unwrap_or(ActionPayload::None),
        ActionId::EditThemeRole | ActionId::ResetThemeRole | ActionId::ThemeRoleReset => context
            .selected_theme_role
            .map(ActionPayload::ThemeRole)
            .unwrap_or(ActionPayload::Index(context.settings_selected_index)),
        _ => ActionPayload::None,
    };
    ActionRequest { id, payload }
}

pub fn availability(descriptor: &ActionDescriptor, context: &ActionContext) -> ActionAvailability {
    use AvailabilitySelector::*;
    let available = match descriptor.availability {
        Always => context.mode == ActionMode::Normal,
        ContextHelp => true,
        SessionList => {
            context.surface == ActionSurface::SessionList && context.mode == ActionMode::Normal
        }
        SessionListConnected => {
            context.surface == ActionSurface::SessionList
                && context.mode == ActionMode::Normal
                && context.connected
        }
        Navigation => match context.surface {
            ActionSurface::SessionList => {
                context.mode == ActionMode::Normal && context.has_selection
            }
            ActionSurface::Settings => {
                context.settings_focus == SettingsFocus::Categories || context.has_selection
            }
            ActionSurface::IssueTracker => {
                (context.has_selection
                    || context.issue_tab == Some(crate::types::IssueWorkspaceTab::Sync))
                    && !context.issue_loading
            }
            ActionSurface::ScheduleBrowser => {
                context.has_selection && !context.issue_loading && !context.schedule_loading
            }
            _ => false,
        },
        ContinueSessionMutation => {
            context.surface == ActionSurface::SessionList
                && context.mode == ActionMode::Normal
                && context.operator
                && context.connected
                && context.has_selection
                && context.selected_session_leaf
                && matches!(
                    context.selected_session_status,
                    Some(
                        rsi_common::types::SessionStatus::Running
                            | rsi_common::types::SessionStatus::WaitingApproval
                            | rsi_common::types::SessionStatus::Completed
                            | rsi_common::types::SessionStatus::Failed
                            | rsi_common::types::SessionStatus::Interrupted
                    )
                )
        }
        RunningLeafSessionMutation => {
            context.surface == ActionSurface::SessionList
                && context.mode == ActionMode::Normal
                && context.operator
                && context.connected
                && context.has_selection
                && context.selected_session_leaf
                && matches!(
                    context.selected_session_status,
                    Some(
                        rsi_common::types::SessionStatus::Starting
                            | rsi_common::types::SessionStatus::Running
                            | rsi_common::types::SessionStatus::WaitingApproval
                    )
                )
        }
        PinSessionMutation => {
            context.surface == ActionSurface::SessionList
                && context.mode == ActionMode::Normal
                && context.operator
                && context.connected
                && context.has_selection
                && context.selected_session_status.is_some()
                && !matches!(
                    context.selected_session_status,
                    Some(rsi_common::types::SessionStatus::Deleted)
                )
        }
        ArchiveSessionMutation => {
            context.surface == ActionSurface::SessionList
                && context.mode == ActionMode::Normal
                && context.operator
                && context.connected
                && context.has_selection
                && context.selected_session_status.is_some()
                && !matches!(
                    context.selected_session_status,
                    Some(
                        rsi_common::types::SessionStatus::Archived
                            | rsi_common::types::SessionStatus::Deleted
                    )
                )
        }
        ArchivedSessionListMutation => {
            context.surface == ActionSurface::SessionList
                && context.mode == ActionMode::Normal
                && context.operator
                && context.connected
                && context.session_list_zone == Some(SessionListZone::Archive)
                && context.has_selection
                && matches!(
                    context.selected_session_status,
                    Some(rsi_common::types::SessionStatus::Archived)
                )
        }
        TestingLeafSessionMutation => {
            context.surface == ActionSurface::SessionList
                && context.mode == ActionMode::Normal
                && context.operator
                && context.connected
                && context.has_selection
                && context.selected_session_leaf
                && context.selected_session_status.is_some()
                && !matches!(
                    context.selected_session_status,
                    Some(rsi_common::types::SessionStatus::Deleted)
                )
        }
        HierarchyAscend => {
            context.surface == ActionSurface::SessionList
                && context.mode == ActionMode::Normal
                && context.hierarchy_descended
        }
        CopySessionUuid => {
            matches!(
                context.focus,
                ActionFocus::SessionList | ActionFocus::DetailListProxy
            ) && context.selected_id.is_some()
        }
        Settings => context.surface == ActionSurface::Settings,
        IssueTracker => context.surface == ActionSurface::IssueTracker,
        IssueLocal => {
            context.surface == ActionSurface::IssueTracker
                && context.issue_tab == Some(crate::types::IssueWorkspaceTab::Local)
                && context.mode != ActionMode::Text
        }
        IssueLocalSelection => {
            context.surface == ActionSurface::IssueTracker
                && context.issue_tab == Some(crate::types::IssueWorkspaceTab::Local)
                && context.has_selection
                && context.mode != ActionMode::Text
        }
        IssueLocalActiveSelection => {
            context.surface == ActionSurface::IssueTracker
                && context.issue_tab == Some(crate::types::IssueWorkspaceTab::Local)
                && context.has_selection
                && context.mode == ActionMode::Normal
                && context.connected
                && !context.issue_retry_pending
                && context.selected_issue_status == Some(ActionIssueStatus::Active)
                && context.selected_issue_archived == Some(false)
        }
        IssueLocalDependencySelection => {
            context.surface == ActionSurface::IssueTracker
                && context.issue_tab == Some(crate::types::IssueWorkspaceTab::Local)
                && context.has_selection
                && context.mode == ActionMode::Normal
                && context.connected
                && !context.issue_retry_pending
                && !context.issue_loading
        }
        IssueLocalTerminalSelection => {
            context.surface == ActionSurface::IssueTracker
                && context.issue_tab == Some(crate::types::IssueWorkspaceTab::Local)
                && context.has_selection
                && context.mode == ActionMode::Normal
                && context.connected
                && !context.issue_retry_pending
                && context.selected_issue_status == Some(ActionIssueStatus::Terminal)
                && context.selected_issue_archived == Some(false)
        }
        IssueLocalArchivedTerminalSelection => {
            context.surface == ActionSurface::IssueTracker
                && context.issue_tab == Some(crate::types::IssueWorkspaceTab::Local)
                && context.has_selection
                && context.mode == ActionMode::Normal
                && context.connected
                && !context.issue_retry_pending
                && context.selected_issue_status == Some(ActionIssueStatus::Terminal)
                && context.selected_issue_archived == Some(true)
        }
        IssueCancelConfirmation => {
            context.surface == ActionSurface::IssueTracker
                && context.issue_tab == Some(crate::types::IssueWorkspaceTab::Local)
                && context.has_selection
                && context.mode == ActionMode::PendingDelete
                && context.connected
                && !context.issue_retry_pending
                && context.selected_issue_status == Some(ActionIssueStatus::Active)
                && context.selected_issue_archived == Some(false)
        }
        IssueArchiveConfirmation => {
            context.surface == ActionSurface::IssueTracker
                && context.issue_tab == Some(crate::types::IssueWorkspaceTab::Local)
                && context.has_selection
                && context.mode == ActionMode::PendingArchive
                && context.connected
                && !context.issue_retry_pending
                && context.selected_issue_status == Some(ActionIssueStatus::Terminal)
                && context.selected_issue_archived == Some(false)
        }
        IssueSelection => {
            context.surface == ActionSurface::IssueTracker
                && context.has_selection
                && context.mode == ActionMode::Normal
        }
        IssueLinkedSession => {
            context.surface == ActionSurface::IssueTracker
                && context.selected_issue_session_id.is_some()
        }
        IssueStaleEditor => {
            context.surface == ActionSurface::IssueTracker
                && context.mode == ActionMode::Text
                && context.issue_stale_editor
        }
        IssueRefresh => {
            context.surface == ActionSurface::IssueTracker
                && context.connected
                && context.mode == ActionMode::Normal
        }
        IssueSyncPoll => {
            context.surface == ActionSurface::IssueTracker
                && context.issue_tab == Some(crate::types::IssueWorkspaceTab::Sync)
                && context.connected
                && context.mode == ActionMode::Normal
        }
        IssueFormSelection => {
            context.surface == ActionSurface::IssueTracker
                && context.mode == ActionMode::Text
                && context.issue_form_selectable
                && !context.issue_retry_pending
        }
        IssueRetryMutation => {
            context.surface == ActionSurface::IssueTracker
                && context.connected
                && context.issue_retry_pending
                && matches!(context.mode, ActionMode::Normal | ActionMode::Text)
        }
        IssueLocalInspector => {
            context.surface == ActionSurface::IssueTracker
                && context.issue_tab == Some(crate::types::IssueWorkspaceTab::Local)
                && context.issue_inspector
                && context.mode == ActionMode::Normal
                && context.has_selection
                && !context.issue_loading
        }
        ScheduleBrowser => context.surface == ActionSurface::ScheduleBrowser,
        ScheduleSelection => {
            context.surface == ActionSurface::ScheduleBrowser && context.has_selection
        }
        SchedulePendingDelete => {
            context.surface == ActionSurface::ScheduleBrowser
                && context.has_selection
                && context.mode == ActionMode::PendingDelete
        }
        ScheduleMutation => {
            context.surface == ActionSurface::ScheduleBrowser
                && context.connected
                && !context.schedule_loading
                && match descriptor.id {
                    ActionId::ScheduleNew => true,
                    ActionId::ScheduleDelete => {
                        context.has_selection && context.mode == ActionMode::PendingDelete
                    }
                    _ => context.has_selection,
                }
        }
        ThemeRoleEditor => context.surface == ActionSurface::ThemeRoleEditor,
        ThemeRoleOverride => {
            context.surface == ActionSurface::Settings
                && context.settings_focus == SettingsFocus::Items
                && context.selected_theme_role_overridden
        }
        Connected => context.connected,
        ContextualOpen => {
            (context.surface == ActionSurface::Settings
                && context.settings_focus == SettingsFocus::Items
                && context.settings_row_editable)
                || (context.surface == ActionSurface::Settings
                    && context.settings_focus == SettingsFocus::Categories)
                || (context.surface == ActionSurface::SessionList
                    && context.mode == ActionMode::Normal
                    && context.has_selection)
        }
        SettingsCategories => {
            context.surface == ActionSurface::Settings
                && context.settings_focus == SettingsFocus::Categories
        }
        SettingsItems => {
            context.surface == ActionSurface::Settings
                && context.settings_focus == SettingsFocus::Items
        }
        SettingsEditable => {
            context.surface == ActionSurface::Settings
                && context.settings_focus == SettingsFocus::Items
                && context.settings_row_editable
        }
        SettingsAdd => {
            context.surface == ActionSurface::Settings
                && context.settings_focus == SettingsFocus::Items
                && matches!(
                    context.settings_category,
                    SettingsSection::ApiProviders
                        | SettingsSection::ClaudeHooks
                        | SettingsSection::Budgets
                )
        }
        SettingsDelete => {
            context.surface == ActionSurface::Settings
                && context.settings_focus == SettingsFocus::Items
                && context.settings_row_deletable
                && matches!(
                    context.settings_category,
                    SettingsSection::ApiProviders
                        | SettingsSection::ClaudeHooks
                        | SettingsSection::ClaudeSkills
                        | SettingsSection::Budgets
                )
        }
        SettingsEnable => {
            context.surface == ActionSurface::Settings
                && context.settings_focus == SettingsFocus::Items
                && context.settings_row_enableable
                && context.settings_category == SettingsSection::ClaudeSkills
        }
        SettingsRefresh => {
            context.surface == ActionSurface::Settings
                && context.settings_focus == SettingsFocus::Items
                && context.connected
                && (DAEMON_FEATURE_SECTIONS.contains(&context.settings_category)
                    || matches!(
                        context.settings_category,
                        SettingsSection::Usage
                            | SettingsSection::Budgets
                            | SettingsSection::MemoryDreaming
                    ))
        }
    };
    if available {
        ActionAvailability::Available(materialize(context, descriptor.id))
    } else {
        ActionAvailability::Unavailable {
            reason: match descriptor.availability {
                Connected => "daemon is disconnected",
                SessionListConnected => "navigation refresh is unavailable while disconnected",
                Navigation => "no selectable row",
                ContinueSessionMutation
                | RunningLeafSessionMutation
                | PinSessionMutation
                | ArchiveSessionMutation
                | ArchivedSessionListMutation
                | TestingLeafSessionMutation => "selected session is not eligible for this action",
                HierarchyAscend => "already at the hierarchy root",
                ScheduleMutation => {
                    "scheduled job action is unavailable while loading or disconnected"
                }
                IssueLinkedSession => "selected issue has no linked session",
                IssueLocalSelection
                | IssueSelection
                | ScheduleSelection
                | SchedulePendingDelete => "no matching selected object",
                ThemeRoleOverride => "selected role has no override",
                ContextualOpen => "no selection to open",
                ContextHelp => "text entry owns printable question marks",
                SettingsRefresh => "daemon-backed settings are unavailable while disconnected",
                _ => "action is unavailable in this context",
            },
        }
    }
}

pub fn available_actions(context: &ActionContext) -> Vec<AvailableAction> {
    let route = route_for_surface(context.surface);
    ACTION_DESCRIPTORS
        .iter()
        .filter(|descriptor| descriptor.show_in_help)
        .filter(|descriptor| overlay_permits_descriptor(context, descriptor))
        .filter(|descriptor| {
            descriptor
                .bindings
                .iter()
                .any(|binding| Some(binding.route) == route)
        })
        .filter_map(|descriptor| match availability(descriptor, context) {
            ActionAvailability::Available(request) => Some(AvailableAction {
                descriptor,
                request,
            }),
            ActionAvailability::Unavailable { .. } => None,
        })
        .collect()
}

/// The complete help reference, grouped in the same chapter order as the manual.
/// Descriptor rows are intentionally independent of `show_in_help`.
#[must_use]
pub fn all_commands_sections(context: &ActionContext) -> Vec<(String, Vec<String>)> {
    use crate::manual::model::MANUAL_CHAPTERS;
    let mut sections = Vec::new();
    for chapter in MANUAL_CHAPTERS {
        let mut rows = Vec::new();
        for descriptor in ACTION_DESCRIPTORS
            .iter()
            .filter(|descriptor| chapter.categories.contains(&descriptor.category))
        {
            let marker = if matches!(
                availability(descriptor, context),
                ActionAvailability::Available(_)
            ) {
                '●'
            } else {
                '○'
            };
            let keys = descriptor
                .bindings
                .iter()
                .map(|binding| format!("{:?}: {}", binding.route, binding.sequence))
                .collect::<Vec<_>>()
                .join(", ");
            let argument = match descriptor.command_argument {
                CommandArgument::None => "",
                CommandArgument::Optional => " [arg]",
                CommandArgument::Required => " <arg>",
            };
            let aliases = descriptor
                .command_aliases
                .iter()
                .map(|alias| format!(":{alias}{argument}"))
                .collect::<Vec<_>>()
                .join(", ");
            rows.push(format!(
                "{marker} {}  {}  {} — {}",
                descriptor.label, keys, aliases, descriptor.summary
            ));
        }
        if !rows.is_empty() {
            sections.push((chapter.title.to_string(), rows));
        }
    }
    for route in OVERLAY_HELP_ROUTES {
        sections.push((
            format!("Overlay · {}", route.title),
            route
                .entries()
                .map(|entry| format!("{}  {}", entry.keys, entry.label))
                .collect(),
        ));
    }
    for table in crate::key_tables::KEY_TABLES {
        sections.push((
            format!("Raw keys · {}", table.title),
            (table.rows)()
                .into_iter()
                .map(|row| format!("{} [{}]  {}", row.chord, row.context, row.effect))
                .collect(),
        ));
    }
    sections.push((
        "File viewer commands".to_string(),
        crate::file_viewer_commands::FILE_VIEWER_COMMAND_DOCS
            .iter()
            .map(|doc| format!(":{}  {}", doc.example, doc.summary))
            .collect(),
    ));
    sections
}

/// Return every binding that applies to the captured help context.
#[must_use]
pub fn bindings_for_action(action: &AvailableAction, context: &ActionContext) -> Vec<&'static str> {
    if action.request.id == ActionId::ContextHelp
        && (matches!(context.mode, ActionMode::Text | ActionMode::Search)
            || context.role_editor_text_entry)
    {
        return vec!["Ctrl-Alt-G"];
    }
    let Some(route) = route_for_surface(context.surface) else {
        return Vec::new();
    };
    action
        .descriptor
        .bindings
        .iter()
        .filter(|binding| binding.route == route)
        .map(|binding| binding.sequence)
        .collect()
}

fn normalized_key(key: KeyEvent, context: &ActionContext) -> Option<&'static str> {
    // Legacy terminals report an uppercase byte either with no modifier or
    // with SHIFT, depending on their keyboard protocol. Treat both as the
    // same documented uppercase binding while continuing to reject every
    // other modified printable key.
    if key.modifiers == KeyModifiers::CONTROL {
        return match key.code {
            KeyCode::Char('l') => Some("Ctrl-L"),
            KeyCode::Char('r') => Some("Ctrl-R"),
            KeyCode::Enter => Some("Ctrl-Enter"),
            _ => None,
        };
    }
    if key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT)
        && key.code == KeyCode::Char('g')
    {
        return Some("Ctrl-Alt-G");
    }
    let shifted_uppercase = key.modifiers == KeyModifiers::SHIFT
        && matches!(key.code, KeyCode::Char(value) if value.is_ascii_uppercase());
    if key.modifiers != KeyModifiers::NONE && !shifted_uppercase {
        return None;
    }
    match key.code {
        KeyCode::Char(' ') => Some("Space"),
        KeyCode::Char('d') if context.mode == ActionMode::PendingDelete => Some("dd"),
        KeyCode::Char('y') if context.mode == ActionMode::PendingYank => Some("yy"),
        KeyCode::Char('#') if context.mode == ActionMode::PendingYank => Some("y#"),
        KeyCode::Char('g') if context.mode == ActionMode::PendingJump => Some("gg"),
        KeyCode::Char('?') => Some("?"),
        KeyCode::Char('j') => Some("j"),
        KeyCode::Char('k') => Some("k"),
        KeyCode::Char('q') => Some("q"),
        KeyCode::Char('h') => Some("h"),
        KeyCode::Char('l') => Some("l"),
        KeyCode::Char('a') => Some("a"),
        KeyCode::Char('e') => Some("e"),
        KeyCode::Char('R') => Some("R"),
        KeyCode::Char('r') => Some("r"),
        KeyCode::Char('n') => Some("n"),
        KeyCode::Char('S') => Some("S"),
        KeyCode::Char('p') => Some("p"),
        KeyCode::Char('b') => Some("b"),
        KeyCode::Char('A') => Some("A"),
        KeyCode::Char('y') => Some("y"),
        KeyCode::Char('#') => Some("#"),
        KeyCode::Char('[') => Some("["),
        KeyCode::Char(']') => Some("]"),
        KeyCode::Char('f') => Some("f"),
        KeyCode::Char('s') => Some("s"),
        KeyCode::Char('g') => Some("g"),
        KeyCode::Char('G') => Some("G"),
        KeyCode::Char('t') => Some("t"),
        KeyCode::Char('d') => Some("d"),
        KeyCode::Char('x') => Some("x"),
        KeyCode::Char('X') => Some("X"),
        KeyCode::Char('P') => Some("P"),
        KeyCode::Char('U') => Some("U"),
        KeyCode::Char('/') => Some("/"),
        KeyCode::Enter => Some("Enter"),
        KeyCode::Tab => Some("Tab"),
        KeyCode::PageUp => Some("PageUp"),
        KeyCode::PageDown => Some("PageDown"),
        KeyCode::Esc => Some("Esc"),
        KeyCode::Left => Some("Left"),
        KeyCode::Right => Some("Right"),
        KeyCode::Delete => Some("Delete"),
        KeyCode::Down => Some("j"),
        KeyCode::Up => Some("k"),
        _ => None,
    }
}

pub fn request_for_key(context: &ActionContext, key: KeyEvent) -> ActionAvailability {
    let Some(route) = route_for_surface(context.surface) else {
        return ActionAvailability::Unavailable {
            reason: "no registered surface",
        };
    };
    let Some(key) = normalized_key(key, context) else {
        return ActionAvailability::Unavailable {
            reason: "key is not registered",
        };
    };
    let mut matched_binding = false;
    for descriptor in ACTION_DESCRIPTORS.iter().filter(|descriptor| {
        overlay_permits_descriptor(context, descriptor)
            && descriptor.bindings.iter().any(|binding| {
                binding.route == route
                    && binding.sequence == key
                    && (descriptor.id != ActionId::ContextHelp
                        || context_help_binding_applies(key, context))
            })
    }) {
        matched_binding = true;
        if let available @ ActionAvailability::Available(_) = availability(descriptor, context) {
            return available;
        }
    }
    if !matched_binding {
        return ActionAvailability::Unavailable {
            reason: "key is not registered",
        };
    }
    ActionAvailability::Unavailable {
        reason: "action is unavailable in this context",
    }
}

pub fn has_binding_for_key(context: &ActionContext, key: KeyEvent) -> bool {
    let Some(route) = route_for_surface(context.surface) else {
        return false;
    };
    let Some(key) = normalized_key(key, context) else {
        return false;
    };
    ACTION_DESCRIPTORS.iter().any(|descriptor| {
        overlay_permits_descriptor(context, descriptor)
            && descriptor.bindings.iter().any(|binding| {
                binding.route == route
                    && binding.sequence == key
                    && (descriptor.id != ActionId::ContextHelp
                        || context_help_binding_applies(key, context))
            })
    })
}

/// Overlays that own their key handling expose only contextual help through
/// the registry; their documented keys are discovery-only catalog entries.
fn overlay_permits_descriptor(context: &ActionContext, descriptor: &ActionDescriptor) -> bool {
    !context.overlay_blocks_normal_actions || descriptor.id == ActionId::ContextHelp
}

fn context_help_binding_applies(key: &'static str, context: &ActionContext) -> bool {
    key != "?" || (context.mode == ActionMode::Normal && !context.role_editor_text_entry)
}

pub fn descriptor_for_request(
    context: &ActionContext,
    request: ActionRequest,
) -> Option<&'static ActionDescriptor> {
    let route = route_for_surface(context.surface)?;
    ACTION_DESCRIPTORS.iter().find(|descriptor| {
        descriptor.id == request.id
            && descriptor
                .bindings
                .iter()
                .any(|binding| binding.route == route)
    })
}

pub fn recheck_request(context: &ActionContext, request: ActionRequest) -> ActionAvailability {
    descriptor_for_request(context, request)
        .map(|descriptor| availability(descriptor, context))
        .unwrap_or(ActionAvailability::Unavailable {
            reason: "action is not registered for this surface",
        })
}

pub fn request_from_lc_action(action: &LcAction) -> Option<ActionRequest> {
    let id = match action {
        LcAction::ToggleKeybindingsHelp => ActionId::ContextHelp,
        LcAction::EnterSession => ActionId::Open,
        LcAction::EnterSearch => ActionId::Search,
        LcAction::RefreshNavigation => ActionId::Refresh,
        LcAction::OpenThemePicker => ActionId::OpenThemePicker,
        LcAction::OpenColorCustomizer => ActionId::OpenLegacyColors,
        LcAction::OpenIssuesWorkspace => ActionId::OpenIssuesWorkspace,
        LcAction::InterruptSession => ActionId::InterruptSession,
        LcAction::QuickContinue => ActionId::ContinueSession,
        LcAction::TogglePinSession => ActionId::TogglePin,
        LcAction::ArchiveSession => ActionId::ArchiveSession,
        LcAction::UnarchiveSession => ActionId::UnarchiveSession,
        LcAction::ToggleTestingNeeded => ActionId::ToggleTestingNeeded,
        LcAction::AscendContainer => ActionId::AscendHierarchy,
        LcAction::EditHarnessManagerPolicy => ActionId::ManagerPolicy,
        LcAction::OpenHarnessManagerBoard => ActionId::ManagerBoard,
        LcAction::OpenHarnessManagerDecisions => ActionId::ManagerDecisions,
        LcAction::OpenHarnessManagerInbox => ActionId::ManagerInbox,
        LcAction::OpenHarnessManagerInspect => ActionId::ManagerInspect,
        _ => return None,
    };
    Some(ActionRequest::plain(id))
}

pub fn request_for_alias(alias: &str) -> Option<ActionRequest> {
    ACTION_DESCRIPTORS
        .iter()
        .find(|descriptor| descriptor.command_aliases.contains(&alias))
        .map(|descriptor| ActionRequest::plain(descriptor.id))
}

/// Find the longest registered command spelling at a word boundary.
/// The rest of the input belongs to the selected descriptor's argument policy.
#[must_use]
pub fn resolve_command(input: &str) -> Option<(&'static ActionDescriptor, &str)> {
    let input = input.trim();
    ACTION_DESCRIPTORS
        .iter()
        .flat_map(|descriptor| {
            descriptor
                .command_aliases
                .iter()
                .map(move |alias| (descriptor, *alias))
        })
        .filter_map(|(descriptor, alias)| {
            input.strip_prefix(alias).and_then(|rest| {
                (rest.is_empty() || rest.starts_with(char::is_whitespace)).then_some((
                    descriptor,
                    alias.len(),
                    rest.trim(),
                ))
            })
        })
        .max_by_key(|(_, length, _)| *length)
        .map(|(descriptor, _, args)| (descriptor, args))
}

#[must_use]
pub fn command_descriptor(id: ActionId) -> Option<&'static ActionDescriptor> {
    ACTION_DESCRIPTORS
        .iter()
        .find(|descriptor| descriptor.id == id && !descriptor.command_aliases.is_empty())
}

pub fn binding_sequence(id: ActionId, route: ActionRoute) -> Option<&'static str> {
    ACTION_DESCRIPTORS
        .iter()
        .filter(|descriptor| descriptor.id == id)
        .flat_map(|descriptor| descriptor.bindings)
        .find(|binding| binding.route == route)
        .map(|binding| binding.sequence)
}

// ─── Overlay contextual help catalog (Issue #547) ───────────────────────────
//
// Overlays other than Scheduled Jobs and the Theme Role Editor own their key
// handling in `overlay::handle_overlay_key` match arms rather than through
// the action registry. The entries below describe those handlers so
// contextual help is truthful for every focusable overlay. They are
// DISCOVERY-ONLY: they are not `ActionDescriptor`s, carry no route, and are
// never consulted by `request_for_key`, `has_binding_for_key`, or the command
// palette. The overlay's own handler remains the sole executor of each key.

/// What an overlay help entry does, so help can group and tests can verify
/// every overlay advertises a way out plus at least one real action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayHelpRole {
    /// Leaves the overlay or current sub-mode.
    Close,
    /// Moves selection, focus, scroll, or field.
    Navigate,
    /// Performs an overlay-specific operation.
    Action,
    /// Edits text or a value in place.
    Edit,
}

/// One discovery-only help row: display keys, short description, role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayHelpEntry {
    pub keys: &'static str,
    pub label: &'static str,
    pub role: OverlayHelpRole,
}

const fn nav(keys: &'static str, label: &'static str) -> OverlayHelpEntry {
    OverlayHelpEntry {
        keys,
        label,
        role: OverlayHelpRole::Navigate,
    }
}
const fn act(keys: &'static str, label: &'static str) -> OverlayHelpEntry {
    OverlayHelpEntry {
        keys,
        label,
        role: OverlayHelpRole::Action,
    }
}
const fn edit(keys: &'static str, label: &'static str) -> OverlayHelpEntry {
    OverlayHelpEntry {
        keys,
        label,
        role: OverlayHelpRole::Edit,
    }
}
const fn close(keys: &'static str, label: &'static str) -> OverlayHelpEntry {
    OverlayHelpEntry {
        keys,
        label,
        role: OverlayHelpRole::Close,
    }
}

/// Distinct key-handling classes for overlays and overlay-like focus targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayHelpClass {
    CreateEntityNormal,
    CreateEntityInsert,
    CreateEntityBody,
    CreateEntityTopology,
    ModelPicker,
    SortPicker,
    ThemePicker,
    ProjectPicker,
    LabelPicker,
    ParentPicker,
    FileExplorer,
    FileExplorerFinder,
    FileExplorerViewerFocus,
    FileViewer,
    Telescope,
    CommandPalette,
    ManagerScope,
    ManagerBoard,
    ManagerDecisions,
    ManagerPolicy,
    ManagerTextEntry,
    Notifications,
    RecentCompletions,
    QuestionNormal,
    QuestionInsert,
    TrashBrowser,
    SettlementBrowser,
    GraphNavigate,
    GraphEmpty,
    GraphDetail,
    GraphEditField,
    GraphPicker,
    DagBrowser,
    ProjectForm,
    LabelForm,
    ProviderForm,
    MessageBridgeForm,
    HookForm,
    HookConflict,
    BudgetPolicyForm,
    ScheduleForm,
    RenameSession,
    ColorCustomizer,
    TextAreaBgEditor,
    PromptPreview,
    SkillPreview,
    Diagnostics,
    SessionInfo,
    Rating,
    Terminal,
    MemorySearch,
    AiCommand,
    AiChat,
    CardEditor,
    Dialectic,
    InputModal,
}

impl OverlayHelpClass {
    /// Every overlay help class once, in `index()` order.
    pub const ALL: &'static [Self] = &[
        Self::CreateEntityNormal,
        Self::CreateEntityInsert,
        Self::CreateEntityBody,
        Self::CreateEntityTopology,
        Self::ModelPicker,
        Self::SortPicker,
        Self::ThemePicker,
        Self::ProjectPicker,
        Self::LabelPicker,
        Self::ParentPicker,
        Self::FileExplorer,
        Self::FileExplorerFinder,
        Self::FileExplorerViewerFocus,
        Self::FileViewer,
        Self::Telescope,
        Self::CommandPalette,
        Self::ManagerScope,
        Self::ManagerBoard,
        Self::ManagerDecisions,
        Self::ManagerPolicy,
        Self::ManagerTextEntry,
        Self::Notifications,
        Self::RecentCompletions,
        Self::QuestionNormal,
        Self::QuestionInsert,
        Self::TrashBrowser,
        Self::SettlementBrowser,
        Self::GraphNavigate,
        Self::GraphEmpty,
        Self::GraphDetail,
        Self::GraphEditField,
        Self::GraphPicker,
        Self::DagBrowser,
        Self::ProjectForm,
        Self::LabelForm,
        Self::ProviderForm,
        Self::MessageBridgeForm,
        Self::HookForm,
        Self::HookConflict,
        Self::BudgetPolicyForm,
        Self::ScheduleForm,
        Self::RenameSession,
        Self::ColorCustomizer,
        Self::TextAreaBgEditor,
        Self::PromptPreview,
        Self::SkillPreview,
        Self::Diagnostics,
        Self::SessionInfo,
        Self::Rating,
        Self::Terminal,
        Self::MemorySearch,
        Self::AiCommand,
        Self::AiChat,
        Self::CardEditor,
        Self::Dialectic,
        Self::InputModal,
    ];

    /// Exhaustive position in `ALL` (guards `ALL` against omissions).
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::CreateEntityNormal => 0,
            Self::CreateEntityInsert => 1,
            Self::CreateEntityBody => 2,
            Self::CreateEntityTopology => 3,
            Self::ModelPicker => 4,
            Self::SortPicker => 5,
            Self::ThemePicker => 6,
            Self::ProjectPicker => 7,
            Self::LabelPicker => 8,
            Self::ParentPicker => 9,
            Self::FileExplorer => 10,
            Self::FileExplorerFinder => 11,
            Self::FileExplorerViewerFocus => 12,
            Self::FileViewer => 13,
            Self::Telescope => 14,
            Self::CommandPalette => 15,
            Self::ManagerScope => 16,
            Self::ManagerBoard => 17,
            Self::ManagerDecisions => 18,
            Self::ManagerPolicy => 19,
            Self::ManagerTextEntry => 20,
            Self::Notifications => 21,
            Self::RecentCompletions => 22,
            Self::QuestionNormal => 23,
            Self::QuestionInsert => 24,
            Self::TrashBrowser => 25,
            Self::SettlementBrowser => 26,
            Self::GraphNavigate => 27,
            Self::GraphEmpty => 28,
            Self::GraphDetail => 29,
            Self::GraphEditField => 30,
            Self::GraphPicker => 31,
            Self::DagBrowser => 32,
            Self::ProjectForm => 33,
            Self::LabelForm => 34,
            Self::ProviderForm => 35,
            Self::MessageBridgeForm => 36,
            Self::HookForm => 37,
            Self::HookConflict => 38,
            Self::BudgetPolicyForm => 39,
            Self::ScheduleForm => 40,
            Self::RenameSession => 41,
            Self::ColorCustomizer => 42,
            Self::TextAreaBgEditor => 43,
            Self::PromptPreview => 44,
            Self::SkillPreview => 45,
            Self::Diagnostics => 46,
            Self::SessionInfo => 47,
            Self::Rating => 48,
            Self::Terminal => 49,
            Self::MemorySearch => 50,
            Self::AiCommand => 51,
            Self::AiChat => 52,
            Self::CardEditor => 53,
            Self::Dialectic => 54,
            Self::InputModal => 55,
        }
    }
}

/// A class's help title and ordered entry groups. Groups let classes share
/// one definition of common key sets instead of repeating them.
#[derive(Debug, Clone, Copy)]
pub struct OverlayHelpRoute {
    pub class: OverlayHelpClass,
    pub title: &'static str,
    pub groups: &'static [&'static [OverlayHelpEntry]],
}

impl OverlayHelpRoute {
    pub fn entries(&self) -> impl Iterator<Item = &'static OverlayHelpEntry> + '_ {
        self.groups.iter().flat_map(|group| group.iter())
    }
}

/// `overlay::list::handle_list_nav_key`.
const LIST_NAV: &[OverlayHelpEntry] = &[
    nav("j / k, Down / Up", "Move selection"),
    nav("g / G", "Jump to first / last"),
];
/// `overlay::handle_text_overlay_prompt_leader` for non-text overlays.
const OVERLAY_LEADER: &[OverlayHelpEntry] = &[
    act("Space o", "Close and open a TaskRabbit session prompt"),
    act("Space m", "Close and open a blank session prompt"),
    act("Space ;", "Open the command palette"),
];
const CREATE_ENTITY_COMMON: &[OverlayHelpEntry] = &[
    act("Ctrl-Enter", "Create entity"),
    close("Ctrl-D", "Discard draft and close"),
];
/// `input_surface::handle_key` shared vim text surface.
const TEXT_SURFACE: &[OverlayHelpEntry] = &[
    edit("i / a / o", "Enter insert mode (normal mode)"),
    nav("Esc", "Return to normal mode (insert mode)"),
    nav("h j k l, w b", "Move cursor (normal mode)"),
];
const FORM_FIELDS: &[OverlayHelpEntry] = &[
    nav("Tab / Shift-Tab", "Next / previous field"),
    edit("Type, Backspace", "Edit focused field"),
];
const DIALOG_TEXT: &[OverlayHelpEntry] = &[edit("Type, Backspace", "Edit text")];
const GRAPH_PAN: &[OverlayHelpEntry] = &[
    nav("H / J / K / L", "Pan graph view"),
    nav("c", "Camera follows selection"),
];
const GRAPH_TOOLS: &[OverlayHelpEntry] = &[
    act("t", "Open topology picker"),
    act("o", "Open saved-workflow picker"),
    act("R", "Open recursive graph picker"),
    act("r", "Run authored draft"),
    act("x", "Interrupt running execution"),
];
const MANAGER_SECTIONS: &[OverlayHelpEntry] = &[
    nav(
        "1 / 2 / 3 / 4 / 5",
        "Board / Decisions / Inbox / Inspect / Policy",
    ),
    nav("Tab / Shift-Tab", "Next / previous section"),
    nav("[ / ]", "Previous / next Inspect subsection (Inspect)"),
];
const MANAGER_LEDGER: &[OverlayHelpEntry] = &[
    act("o", "Open the selected row's session"),
    nav("n / p", "Next / previous page"),
    act("r", "Reload section"),
    nav("PageDown / PageUp", "Scroll detail"),
    close("Esc / q", "Close manager"),
];

pub static OVERLAY_HELP_ROUTES: &[OverlayHelpRoute] = &[
    OverlayHelpRoute {
        class: OverlayHelpClass::CreateEntityNormal,
        title: "Create Entity / Normal",
        groups: &[
            &[
                nav("Tab / Shift-Tab", "Next / previous field"),
                nav(
                    "n / b / k / T / t",
                    "Focus name / body / kind / tag / topology",
                ),
                edit("i", "Edit focused text field"),
                edit("h / l", "Previous / next kind (kind field)"),
                edit("Ctrl-P / Ctrl-Shift-P", "Cycle provider forward / backward"),
                edit("e / E", "Cycle effort forward / backward"),
                edit("s", "Toggle sandbox"),
                act("m", "Open model picker"),
                act("gp", "Open parent picker"),
                act("Enter", "Create entity"),
                close("Esc", "Save draft and close"),
            ],
            CREATE_ENTITY_COMMON,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::CreateEntityInsert,
        title: "Create Entity / Insert",
        groups: &[
            &[
                edit("Type, Backspace", "Edit name or tag"),
                edit("Space / , / Tab", "Commit tag chip (tag field)"),
                nav("Esc", "Return to normal mode"),
            ],
            CREATE_ENTITY_COMMON,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::CreateEntityBody,
        title: "Create Entity / Body",
        groups: &[
            &[
                nav("Tab / Shift-Tab", "Next / previous field"),
                edit("Enter", "Insert newline (insert mode)"),
                close("Esc", "Save draft and close (normal mode)"),
            ],
            TEXT_SURFACE,
            CREATE_ENTITY_COMMON,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::CreateEntityTopology,
        title: "Create Entity / Topology",
        groups: &[
            &[
                edit("Type, Backspace", "Filter topologies"),
                nav("j / k, Down / Up, G", "Move selection"),
                act("Space", "Toggle topology preview"),
                act("Enter", "Choose topology and advance"),
                nav("Esc", "Clear topology and focus kind"),
            ],
            CREATE_ENTITY_COMMON,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::ModelPicker,
        title: "Model Picker",
        groups: &[
            LIST_NAV,
            &[
                nav("Tab / Shift-Tab", "Next / previous provider"),
                act("1-9", "Select numbered model"),
                act("Enter", "Select model"),
                close("Esc / q", "Close model picker"),
            ],
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::SortPicker,
        title: "Sort Picker",
        groups: &[
            LIST_NAV,
            &[
                act("Enter", "Apply sort order"),
                close("Esc / q", "Close without changing"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::ThemePicker,
        title: "Theme Picker",
        groups: &[
            &[
                nav("j / k, Down / Up", "Move and preview theme"),
                nav("g / G", "Preview first / last theme"),
                act("1-9", "Apply numbered theme"),
                act("Enter", "Apply theme"),
                close("Esc / q", "Revert preview and close"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::ProjectPicker,
        title: "Project Picker",
        groups: &[
            LIST_NAV,
            &[
                edit("Type, Backspace", "Filter projects"),
                act("Enter", "Select project"),
                act("Ctrl-N", "New project"),
                act("Ctrl-E", "Edit highlighted project"),
                act("Ctrl-D", "Delete highlighted project"),
                close("Esc", "Close picker"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::LabelPicker,
        title: "Label Picker",
        groups: &[
            LIST_NAV,
            &[
                edit("Type, Backspace", "Filter labels"),
                act("Enter", "Assign label"),
                act("Ctrl-N", "New label"),
                act("Ctrl-E", "Edit highlighted label"),
                act("Ctrl-D", "Delete highlighted label"),
                close("Esc", "Close picker"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::ParentPicker,
        title: "Parent Picker",
        groups: &[
            LIST_NAV,
            &[
                edit("Type, Backspace", "Filter parents"),
                act("Enter", "Set parent"),
                close("Esc", "Close picker"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::FileExplorer,
        title: "File Explorer",
        groups: &[
            LIST_NAV,
            &[
                act("Enter / l", "Open file or directory"),
                nav("h", "Go to parent directory"),
                act("yy / yn", "Copy path / file name"),
                act("dd", "Delete file to trash buffer"),
                act("u", "Restore last deleted file"),
                act(".", "Toggle hidden files"),
                act("/", "Open fuzzy finder"),
                nav("Ctrl-L", "Focus file viewer"),
                close("Esc / q, Space / Space Space", "Close explorer"),
            ],
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::FileExplorerFinder,
        title: "File Explorer / Finder",
        groups: &[&[
            edit("Type, Backspace", "Filter files"),
            nav("j / k, Down / Up", "Move selection"),
            act("Enter", "Open selected file"),
            nav("Esc", "Return to explorer tree"),
            close("Esc Esc", "Return to tree, then close explorer"),
        ]],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::FileExplorerViewerFocus,
        title: "File Explorer / Viewer Focus",
        groups: &[&[
            nav("Ctrl-H", "Focus explorer tree"),
            act("Other keys", "Go to the file viewer"),
            close("Esc / q", "Close explorer"),
        ]],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::FileViewer,
        title: "File Viewer",
        groups: &[
            &[
                close("q, Space q", "Close viewer (normal mode)"),
                act("Space m", "Toggle markdown preview"),
                act("Space Space", "Open telescope file picker"),
                act("Ctrl-S", "Save file"),
                act(":", "Command mode (:w, :wq, :q, :q!, :e!, :N)"),
                act("/ / ?", "Search forward / backward"),
                nav("n / N", "Next / previous match"),
                nav("* / #", "Search word under cursor"),
                act("za / zo / zc", "Toggle / open / close fold"),
                act("zM / zR", "Close / open all folds"),
                nav("PageDown / PageUp", "Scroll one page"),
                nav("Backspace", "Back to session list"),
            ],
            TEXT_SURFACE,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::Telescope,
        title: "Telescope",
        groups: &[
            &[
                edit("Type, Backspace", "Filter files"),
                nav("j / k, Down / Up", "Move selection"),
                act("Enter", "Open file in viewer"),
                close("Esc", "Close telescope"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::CommandPalette,
        title: "Command Palette",
        groups: &[&[
            edit("Type, Backspace", "Filter commands"),
            nav("Down / Ctrl-N, Up / Ctrl-P", "Move selection"),
            edit("Tab", "Edit command argument"),
            act("Enter", "Run selected command"),
            close("Esc", "Leave argument edit, then close"),
        ]],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::ManagerScope,
        title: "Manager Scope",
        groups: &[
            LIST_NAV,
            &[
                act("/", "Search Epics"),
                act("Space", "Toggle selected Epic"),
                act("a", "Select whole project"),
                act("Enter", "Save manager scope"),
                nav("Esc / Enter", "Finish search (search mode)"),
                close("Esc / q", "Close"),
            ],
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::ManagerBoard,
        title: "Manager Board",
        groups: &[MANAGER_SECTIONS, LIST_NAV, MANAGER_LEDGER],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::ManagerDecisions,
        title: "Manager Decisions",
        groups: &[
            MANAGER_SECTIONS,
            LIST_NAV,
            &[act("Enter / a", "Answer selected decision")],
            MANAGER_LEDGER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::ManagerPolicy,
        title: "Manager Policy",
        groups: &[
            MANAGER_SECTIONS,
            LIST_NAV,
            &[
                edit("Enter / Space", "Edit selected policy field"),
                act("s", "Save policy"),
                act("r", "Reload policy"),
                close("Esc / q", "Close manager"),
            ],
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::ManagerTextEntry,
        title: "Manager / Text Entry",
        groups: &[&[
            edit("Type, Backspace", "Edit value"),
            edit("Ctrl-U", "Clear value"),
            act("Enter", "Submit"),
            close("Esc", "Cancel entry"),
        ]],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::Notifications,
        title: "Notifications",
        groups: &[
            LIST_NAV,
            &[
                act("x", "Dismiss selected notification"),
                act("N", "Dismiss all active notifications"),
                act("Enter", "Open source session"),
                close("Esc / q", "Close"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::RecentCompletions,
        title: "Recent Completions",
        groups: &[
            LIST_NAV,
            &[
                act("Enter", "Open selected session"),
                act("i", "Return to input bar in insert mode"),
                close("Esc / q", "Close"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::QuestionNormal,
        title: "Question / Normal",
        groups: &[&[
            nav("j / k, Down / Up", "Move option cursor"),
            act("Space", "Select or toggle option"),
            act("1-9", "Select numbered option"),
            nav("Enter / Backspace", "Next / previous question"),
            edit("i", "Type a custom answer"),
            act("Ctrl-Enter", "Submit answers"),
            act("d", "Decline question"),
            close("Esc / q", "Hide question"),
        ]],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::QuestionInsert,
        title: "Question / Insert",
        groups: &[&[
            edit("Type", "Write custom answer"),
            act("Enter / Ctrl-Enter", "Submit answers"),
            edit("Shift-Enter", "Insert newline"),
            close("Esc", "Return to normal mode"),
        ]],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::TrashBrowser,
        title: "Trash",
        groups: &[
            &[
                nav("j / k, Down / Up", "Move selection"),
                nav("G", "Jump to last"),
                act("U", "Restore session"),
                act("D", "Purge session permanently"),
                close("Esc / q", "Close"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::SettlementBrowser,
        title: "Source Worktree Settlement",
        groups: &[
            &[
                nav("j / k, Down / Up", "Move selection"),
                nav("J / K, PageDown / PageUp", "Page selection"),
                act("Enter", "Audit selected worktree"),
                act("A", "Begin authorization"),
                act("r", "Refresh receipt"),
                close("Esc / q", "Close"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::GraphNavigate,
        title: "Graph Review",
        groups: &[
            &[
                nav("h j k l, arrows", "Move to neighboring node"),
                nav("[ / ]", "Predecessor / successor"),
                nav("g / G", "First / last node"),
                act("Enter", "Open node detail"),
                act("d", "Delete node"),
                act("u", "Undo edit"),
                act("z", "Toggle collapse"),
                nav("Tab", "Focus dashboard (bridged graphs)"),
            ],
            GRAPH_TOOLS,
            GRAPH_PAN,
            &[close("q", "Close graph review")],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::GraphEmpty,
        title: "Graph Review / Empty",
        groups: &[
            &[
                act("t", "Open topology picker"),
                act("o", "Open saved-workflow picker"),
            ],
            GRAPH_PAN,
            &[close("q", "Close graph review")],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::GraphDetail,
        title: "Graph Review / Node Detail",
        groups: &[
            &[
                edit("i / I", "Edit node name / instructions"),
                edit("s / S", "Edit integration / verification strategy"),
                nav("j / k, Down / Up", "Move edge selection"),
                nav("[ / ]", "Predecessor / successor"),
                act("x", "Delete edge, or interrupt while running"),
            ],
            GRAPH_TOOLS,
            GRAPH_PAN,
            &[
                nav("Esc", "Back out of detail"),
                close("q", "Close graph review"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::GraphEditField,
        title: "Graph Review / Edit Field",
        groups: &[&[
            edit("Type, Backspace", "Edit field"),
            act("Enter", "Commit field"),
            close("Esc", "Cancel edit"),
        ]],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::GraphPicker,
        title: "Graph Review / Picker",
        groups: &[
            LIST_NAV,
            &[
                nav("PageDown / Ctrl-D, PageUp / Ctrl-U", "Page selection"),
                act("Enter", "Choose item"),
                close("Esc", "Close picker"),
            ],
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::DagBrowser,
        title: "Recursive DAG Browser",
        groups: &[
            &[
                nav("Tab / l / Right", "Next panel"),
                nav("Shift-Tab / h / Left", "Previous panel"),
                nav("j / k, Down / Up", "Move selection"),
                nav("g / G", "Jump to first / last"),
                nav("Ctrl-D / Ctrl-U", "Scroll inspector"),
                nav("]a / [a", "Next / previous artifact row"),
                act("Enter", "Open selection"),
                act("p / m / t / d", "Preview / metadata / test / diff view"),
                act("r", "Reload graph"),
                act("R / L", "Start fake / live run"),
                act("C g / C r", "Cancel graph / run (prompts for reason)"),
                act("c", "Continue recovery"),
                act("!", "Reveal control details"),
                close("Esc / q", "Close inspector, then browser"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::ProjectForm,
        title: "Project Form",
        groups: &[
            FORM_FIELDS,
            &[
                edit("Left / Right", "Cycle color (color field)"),
                act("Enter", "Save project"),
                close("Esc", "Cancel"),
            ],
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::LabelForm,
        title: "Label Form",
        groups: &[
            FORM_FIELDS,
            &[
                edit("Left / Right", "Cycle color (color field)"),
                act("Enter", "Save label"),
                close("Esc", "Cancel"),
            ],
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::ProviderForm,
        title: "Provider Form",
        groups: &[
            &[
                nav("Tab / Shift-Tab", "Next / previous field"),
                act("Enter / Ctrl-Enter", "Save provider"),
                close("Esc (normal mode), Ctrl-Q", "Close"),
            ],
            TEXT_SURFACE,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::MessageBridgeForm,
        title: "Message Bridge Form",
        groups: &[
            FORM_FIELDS,
            &[
                edit("Space", "Toggle enabled (first field)"),
                act("Enter", "Save bridge"),
                close("Esc", "Close"),
            ],
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::HookForm,
        title: "Hook Form",
        groups: &[
            FORM_FIELDS,
            &[
                edit("Up / Down", "Cycle event (event field)"),
                act("Enter", "Save hook"),
                close("Esc", "Close"),
            ],
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::HookConflict,
        title: "Hook Save Conflict",
        groups: &[
            &[
                act("o", "Overwrite settings on disk"),
                act("r", "Reload settings from disk"),
                close("c / Esc", "Cancel save"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::BudgetPolicyForm,
        title: "Budget Policy Form",
        groups: &[
            FORM_FIELDS,
            &[
                edit("Up / Down", "Cycle scope kind / model tier"),
                act("Enter", "Save policy"),
                close("Esc", "Close"),
            ],
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::ScheduleForm,
        title: "Schedule Form",
        groups: &[
            FORM_FIELDS,
            &[
                edit("Left / Right", "Cycle recurrence (recurrence field)"),
                act("Enter", "Save scheduled job"),
                close("Esc", "Return to Scheduled Jobs"),
            ],
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::RenameSession,
        title: "Rename Session",
        groups: &[
            DIALOG_TEXT,
            &[act("Enter", "Rename session"), close("Esc", "Cancel")],
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::ColorCustomizer,
        title: "Color Customizer",
        groups: &[
            &[
                nav("Tab / j / Down", "Next field"),
                nav("Shift-Tab / k / Up", "Previous field"),
                edit("Type, Backspace", "Edit hex color"),
                act("Enter", "Apply color"),
                act("Delete", "Reset field to default"),
                close("Esc / q", "Close"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::TextAreaBgEditor,
        title: "Text Area Background",
        groups: &[
            &[
                edit("Type, Backspace", "Edit hex color"),
                act("Enter", "Apply color"),
                act("Delete", "Clear override"),
                close("Esc / q", "Close"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::PromptPreview,
        title: "Prompt Preview",
        groups: &[
            &[
                nav("j / k, Down / Up", "Preview next / previous session"),
                nav("G", "Preview last session"),
                nav("Ctrl-D / Ctrl-U", "Scroll preview down / up"),
                act("Enter", "Open selected session"),
                close("p / Esc / q", "Close"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::SkillPreview,
        title: "Skill Preview",
        groups: &[
            &[
                nav("j / k, Down / Up", "Scroll"),
                nav("Ctrl-D / Ctrl-U", "Half page down / up"),
                nav("g / Home, G / End", "Jump to top / bottom"),
                close("Esc / q", "Close"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::Diagnostics,
        title: "Diagnostics",
        groups: &[&[close("Esc / q", "Close")], OVERLAY_LEADER],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::SessionInfo,
        title: "Session Info",
        groups: &[
            &[
                act("R", "Rate session"),
                act("G", "Edit labels"),
                close("Esc / q", "Close"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::Rating,
        title: "Rate Session",
        groups: &[
            &[
                edit("1-9 / 0", "Set rating (0 = 10)"),
                edit("h / l, Left / Right", "Lower / raise rating"),
                act("Enter", "Save rating"),
                close("Esc / q", "Close"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::Terminal,
        title: "Terminal",
        groups: &[&[
            act("Other keys", "Send to the shell"),
            act("Ctrl-C", "Send interrupt to the shell"),
            close("Ctrl-\\", "Close terminal"),
        ]],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::MemorySearch,
        title: "Memory Search",
        groups: &[&[
            edit("Type, Backspace", "Search memory"),
            nav("j / k, Down / Up", "Move selection"),
            close("Esc / q", "Close"),
        ]],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::AiCommand,
        title: "AI Command",
        groups: &[
            DIALOG_TEXT,
            &[act("Enter", "Run instruction"), close("Esc", "Cancel")],
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::AiChat,
        title: "AI Chat",
        groups: &[
            DIALOG_TEXT,
            &[
                act("Enter", "Send question"),
                nav("Up / Down", "Scroll conversation"),
                close("Esc", "Clear input, then close"),
                close("q", "Close (empty input)"),
            ],
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::CardEditor,
        title: "Card Editor",
        groups: &[
            LIST_NAV,
            &[
                act("a", "Add fact"),
                edit("e / i", "Edit selected fact"),
                act("dd", "Delete selected fact"),
                act("J / K", "Move fact down / up"),
                act("Ctrl-S", "Save card"),
                act("Enter", "Commit fact (editing)"),
                nav("Esc", "Cancel fact edit (editing)"),
                close("Esc / q", "Save and close"),
            ],
            OVERLAY_LEADER,
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::Dialectic,
        title: "Dialectic",
        groups: &[
            DIALOG_TEXT,
            &[
                act("Enter", "Send message"),
                nav("Up / Down", "Scroll"),
                close("Esc", "Clear input, then close"),
                close("q", "Close (empty input)"),
            ],
        ],
    },
    OverlayHelpRoute {
        class: OverlayHelpClass::InputModal,
        title: "Input Modal",
        groups: &[
            &[
                act("Enter / Ctrl-Enter", "Send message"),
                close("Ctrl-Q", "Close and return text to the input bar"),
                act("Ctrl-A", "AI command on text"),
                act("Ctrl-Shift-A", "Ask AI about text"),
                act("Ctrl-Y", "Compile prompt"),
                act("Ctrl-Shift-G", "Grammar and spelling correction"),
                act("Ctrl-V", "Paste from clipboard"),
            ],
            TEXT_SURFACE,
        ],
    },
];

/// The single catalog route for an overlay help class.
#[must_use]
pub fn overlay_help_route(class: OverlayHelpClass) -> Option<&'static OverlayHelpRoute> {
    OVERLAY_HELP_ROUTES
        .iter()
        .find(|route| route.class == class)
}

/// Discovery-only entries for the overlay described by `context`, if any.
#[must_use]
pub fn overlay_help_for(context: &ActionContext) -> Option<&'static OverlayHelpRoute> {
    match context.origin {
        HelpOrigin::OverlayClass(class) => overlay_help_route(class),
        _ => None,
    }
}

/// How contextual help treats an overlay state.
///
/// Either a catalog class or a named exemption. Total by construction (Epic M
/// design A.2 rev3): there is no
/// `Option` and no top-level wildcard, so a new `OverlayState` variant does
/// not compile until it is classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpRouting {
    Routed(OverlayHelpClass),
    Exempt(OverlayHelpExemption),
}

/// Overlays whose keys are registry routes rendered as generated tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryOverlayRoute {
    ScheduleBrowser,
    ThemeRoleEditor,
}

impl RegistryOverlayRoute {
    #[must_use]
    pub const fn action_route(self) -> ActionRoute {
        match self {
            Self::ScheduleBrowser => ActionRoute::ScheduleBrowser,
            Self::ThemeRoleEditor => ActionRoute::ThemeRoleEditor,
        }
    }
}

/// Where an exempt screen's keys are documented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocAnchor {
    /// A generated region id of `docs/keybindings.md` / the manual.
    GeneratedRegion(&'static str),
    /// A hand-written `docs/keybindings.md` heading (text after the `#`s).
    Narrative(&'static str),
}

/// Why an overlay state has no discovery catalog route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayHelpExemption {
    /// No overlay is open.
    NoOverlay,
    /// The overlay's keys are an `ActionRoute` of the registry.
    RegistryRoute(RegistryOverlayRoute),
    /// The launch / continue prompt editor.
    PromptEntry,
    /// The help overlay itself.
    HelpItself,
    /// The ESP Square game.
    Game,
    /// The manager policy launch-choice catalog picker sub-mode.
    ManagerCatalogPicker,
    /// The source-worktree settlement authorization sub-mode.
    SettlementAuthorization,
    /// The graph review info dashboard while it holds focus.
    GraphDashboardFocus,
}

impl OverlayHelpExemption {
    /// Every exemption once, in `index()` order.
    pub const ALL: &'static [Self] = &[
        Self::NoOverlay,
        Self::RegistryRoute(RegistryOverlayRoute::ScheduleBrowser),
        Self::RegistryRoute(RegistryOverlayRoute::ThemeRoleEditor),
        Self::PromptEntry,
        Self::HelpItself,
        Self::Game,
        Self::ManagerCatalogPicker,
        Self::SettlementAuthorization,
        Self::GraphDashboardFocus,
    ];

    /// Exhaustive position in `ALL` (guards `ALL` against omissions).
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::NoOverlay => 0,
            Self::RegistryRoute(RegistryOverlayRoute::ScheduleBrowser) => 1,
            Self::RegistryRoute(RegistryOverlayRoute::ThemeRoleEditor) => 2,
            Self::PromptEntry => 3,
            Self::HelpItself => 4,
            Self::Game => 5,
            Self::ManagerCatalogPicker => 6,
            Self::SettlementAuthorization => 7,
            Self::GraphDashboardFocus => 8,
        }
    }

    /// The affected overlay state or sub-mode.
    #[must_use]
    pub const fn state(self) -> &'static str {
        match self {
            Self::NoOverlay => "no overlay open",
            Self::RegistryRoute(RegistryOverlayRoute::ScheduleBrowser) => "Scheduled Jobs browser",
            Self::RegistryRoute(RegistryOverlayRoute::ThemeRoleEditor) => "Theme role editor",
            Self::PromptEntry => "Launch / continue prompt",
            Self::HelpItself => "Keybindings help",
            Self::Game => "ESP Square game",
            Self::ManagerCatalogPicker => "Manager policy: launch-choice catalog picker",
            Self::SettlementAuthorization => "Source-worktree settlement: authorization",
            Self::GraphDashboardFocus => "Graph review: info dashboard focused",
        }
    }

    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::NoOverlay => "The focused pane's keys apply; see the Normal-mode table.",
            Self::RegistryRoute(_) => {
                "Its keys are registry routes, rendered as their own generated table."
            }
            Self::PromptEntry => "A text editor surface; its keys are documented by hand.",
            Self::HelpItself => "Help documents its own scroll, search and close keys.",
            Self::Game => "A game with its own single-screen key legend.",
            Self::ManagerCatalogPicker => {
                "A picker sub-mode of the manager policy editor with its own key handling."
            }
            Self::SettlementAuthorization => {
                "A confirmation sub-mode of the settlement browser with its own key handling."
            }
            Self::GraphDashboardFocus => {
                "The dashboard panel owns keys while focused, separate from graph editing."
            }
        }
    }

    #[must_use]
    pub const fn documented_by(self) -> DocAnchor {
        match self {
            Self::NoOverlay => DocAnchor::GeneratedRegion("normal"),
            Self::RegistryRoute(RegistryOverlayRoute::ScheduleBrowser) => {
                DocAnchor::GeneratedRegion("schedule-browser")
            }
            Self::RegistryRoute(RegistryOverlayRoute::ThemeRoleEditor) => {
                DocAnchor::GeneratedRegion("theme-role-editor")
            }
            Self::PromptEntry => {
                DocAnchor::Narrative("Prompt Overlay (New Session / Continue Session)")
            }
            Self::HelpItself => DocAnchor::Narrative("Keybindings Help Overlay"),
            Self::Game => DocAnchor::Narrative("ESP Square Overlay (`<Space>gc`)"),
            Self::ManagerCatalogPicker => {
                DocAnchor::Narrative("Harness Manager Policy (`:manager policy`)")
            }
            Self::SettlementAuthorization => {
                DocAnchor::Narrative("Source-Worktree Settlement Authorization")
            }
            Self::GraphDashboardFocus => {
                DocAnchor::Narrative("Graph Review Overlay (`<Space>v` or `:graph`)")
            }
        }
    }
}

/// Mirrors `create_entity_form::handle_create_entity_form_key` routing. Takes
/// the destructured form fields, so it cannot be handed another overlay.
fn create_entity_help_class(
    model_dropdown_open: bool,
    focused_field: crate::types::CreateEntityField,
    insert_mode: bool,
    body_mode: crate::types::PopupMode,
) -> OverlayHelpClass {
    use crate::types::{CreateEntityField, PopupMode};
    use OverlayHelpClass as C;
    if model_dropdown_open {
        return C::ModelPicker;
    }
    match focused_field {
        CreateEntityField::Body if body_mode == PopupMode::Insert => C::CreateEntityInsert,
        CreateEntityField::Body => C::CreateEntityBody,
        CreateEntityField::Topology => C::CreateEntityTopology,
        CreateEntityField::Name | CreateEntityField::Tag if insert_mode => C::CreateEntityInsert,
        _ => C::CreateEntityNormal,
    }
}

/// Mirrors `overlay::manager_v2::handle_key` sub-mode and section routing.
fn manager_help_routing(surface: &crate::overlay::manager_v2::ManagerSurface) -> HelpRouting {
    let policy = surface.policy.as_ref();
    manager_routing(
        surface.section,
        surface.ledger.answer.is_some() || policy.is_some_and(|policy| policy.edit.is_some()),
        policy.is_some_and(|policy| policy.picker.is_some()),
    )
}

/// Manager routing from the facts `manager_help_routing` extracts: the
/// section, whether a text entry (decision answer or policy field edit) is
/// open, and whether the policy launch-choice catalog picker is open.
const fn manager_routing(
    section: crate::overlay::manager_v2::ManagerSection,
    text_entry: bool,
    catalog_picker: bool,
) -> HelpRouting {
    use crate::overlay::manager_v2::ManagerSection;
    if text_entry {
        return HelpRouting::Routed(OverlayHelpClass::ManagerTextEntry);
    }
    match section {
        ManagerSection::Decisions => HelpRouting::Routed(OverlayHelpClass::ManagerDecisions),
        ManagerSection::Policy if catalog_picker => {
            HelpRouting::Exempt(OverlayHelpExemption::ManagerCatalogPicker)
        }
        ManagerSection::Policy => HelpRouting::Routed(OverlayHelpClass::ManagerPolicy),
        ManagerSection::Board | ManagerSection::Inbox | ManagerSection::Inspect(_) => {
            HelpRouting::Routed(OverlayHelpClass::ManagerBoard)
        }
    }
}

/// Mirrors `overlay::graph::handle_graph_review_key` mode routing (the
/// dashboard focus is handled by the caller).
fn graph_help_class(mode: crate::types::GraphMode, empty: bool) -> OverlayHelpClass {
    use crate::types::GraphMode;
    match mode {
        GraphMode::EditField { .. } => OverlayHelpClass::GraphEditField,
        GraphMode::Picker { .. } => OverlayHelpClass::GraphPicker,
        _ if empty => OverlayHelpClass::GraphEmpty,
        mode if mode.shows_detail_panel() => OverlayHelpClass::GraphDetail,
        _ => OverlayHelpClass::GraphNavigate,
    }
}

/// Total help routing for an overlay state.
#[allow(clippy::too_many_lines)] // One arm per `OverlayState` variant, no wildcard.
pub fn overlay_help_routing(app: &App, overlay: &OverlayState) -> HelpRouting {
    use HelpRouting::{Exempt, Routed};
    use OverlayHelpClass as C;
    use OverlayHelpExemption as X;
    match overlay {
        OverlayState::None => Exempt(X::NoOverlay),
        OverlayState::CreateEntityForm {
            model_dropdown,
            focused_field,
            insert_mode,
            body,
            ..
        } => Routed(create_entity_help_class(
            model_dropdown
                .as_ref()
                .is_some_and(|dropdown| dropdown.open),
            *focused_field,
            *insert_mode,
            body.mode,
        )),
        OverlayState::SortPicker { .. } => Routed(C::SortPicker),
        OverlayState::ThemePicker { .. } => Routed(C::ThemePicker),
        OverlayState::ProjectPicker { .. } => Routed(C::ProjectPicker),
        OverlayState::LabelPicker { .. } => Routed(C::LabelPicker),
        OverlayState::ParentPicker { .. } => Routed(C::ParentPicker),
        OverlayState::FileExplorer {
            explorer_focused: false,
            ..
        } => Routed(C::FileExplorerViewerFocus),
        OverlayState::FileExplorer {
            finder_active: true,
            ..
        } => Routed(C::FileExplorerFinder),
        OverlayState::FileExplorer { .. } => Routed(C::FileExplorer),
        OverlayState::Telescope { .. } => Routed(C::Telescope),
        OverlayState::CommandPalette { .. } => Routed(C::CommandPalette),
        OverlayState::HarnessManagerScope(..) => Routed(C::ManagerScope),
        OverlayState::HarnessManagerV2(surface) => manager_help_routing(surface),
        OverlayState::NotificationBrowser { .. } => Routed(C::Notifications),
        OverlayState::RecentCompletions { .. } => Routed(C::RecentCompletions),
        OverlayState::QuestionModal { mode, .. } => {
            if *mode == crate::types::PopupMode::Insert {
                Routed(C::QuestionInsert)
            } else {
                Routed(C::QuestionNormal)
            }
        }
        OverlayState::TrashBrowser { .. } => Routed(C::TrashBrowser),
        OverlayState::SourceWorktreeSettlement(state) => {
            if state.authorization_active {
                Exempt(X::SettlementAuthorization)
            } else {
                Routed(C::SettlementBrowser)
            }
        }
        OverlayState::GraphReview {
            dashboard_focused: true,
            ..
        } => Exempt(X::GraphDashboardFocus),
        OverlayState::GraphReview { mode, draft_id, .. } => Routed(graph_help_class(
            *mode,
            app.graph_draft(draft_id)
                .is_none_or(|draft| draft.workflow.nodes.is_empty()),
        )),
        OverlayState::RecursiveDagBrowser(..) => Routed(C::DagBrowser),
        OverlayState::ProjectForm { .. } => Routed(C::ProjectForm),
        OverlayState::LabelForm { .. } => Routed(C::LabelForm),
        OverlayState::ProviderForm { .. } => Routed(C::ProviderForm),
        OverlayState::MessageBridgeForm { .. } => Routed(C::MessageBridgeForm),
        OverlayState::HookForm { .. } => Routed(C::HookForm),
        OverlayState::HookConflictPrompt { .. } => Routed(C::HookConflict),
        OverlayState::BudgetPolicyForm { .. } => Routed(C::BudgetPolicyForm),
        OverlayState::ScheduleForm { .. } => Routed(C::ScheduleForm),
        OverlayState::RenameSession { .. } => Routed(C::RenameSession),
        OverlayState::ColorCustomizer { .. } => Routed(C::ColorCustomizer),
        OverlayState::TextAreaBgEditor { .. } => Routed(C::TextAreaBgEditor),
        OverlayState::PromptPreview { .. } => Routed(C::PromptPreview),
        OverlayState::SkillPreview { .. } => Routed(C::SkillPreview),
        OverlayState::Diagnostics => Routed(C::Diagnostics),
        OverlayState::SessionInfoPanel { .. } => Routed(C::SessionInfo),
        OverlayState::RatingOverlay { .. } => Routed(C::Rating),
        OverlayState::Terminal => Routed(C::Terminal),
        OverlayState::MemorySearch { .. } => Routed(C::MemorySearch),
        OverlayState::AiCommand { .. } => Routed(C::AiCommand),
        OverlayState::AiChat { .. } => Routed(C::AiChat),
        OverlayState::CardEditor { .. } => Routed(C::CardEditor),
        OverlayState::Dialectic { .. } => Routed(C::Dialectic),
        OverlayState::InputModal { .. } => Routed(C::InputModal),
        // A prompt's own model dropdown intercepts keys before prompt editing.
        OverlayState::Prompt { model_dropdown, .. } if model_dropdown.open => {
            Routed(C::ModelPicker)
        }
        OverlayState::Prompt { .. } => Exempt(X::PromptEntry),
        OverlayState::ScheduleBrowser { .. } => {
            Exempt(X::RegistryRoute(RegistryOverlayRoute::ScheduleBrowser))
        }
        OverlayState::ThemeRoleEditor { .. } => {
            Exempt(X::RegistryRoute(RegistryOverlayRoute::ThemeRoleEditor))
        }
        OverlayState::KeybindingsHelp { .. } => Exempt(X::HelpItself),
        OverlayState::EspSquare { .. } => Exempt(X::Game),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// T11c: `OverlayHelpClass::ALL` is exhaustive (guarded by `index()`).
    #[test]
    fn overlay_help_class_all_is_exhaustive() {
        for (position, class) in OverlayHelpClass::ALL.iter().enumerate() {
            assert_eq!(class.index(), position, "{class:?} out of place in ALL");
        }
        let max_index = OverlayHelpClass::ALL
            .iter()
            .map(|class| class.index())
            .max()
            .expect("classes exist");
        assert_eq!(OverlayHelpClass::ALL.len(), max_index + 1);
    }

    /// T11d: `OverlayHelpExemption::ALL` is exhaustive (guarded by `index()`).
    #[test]
    fn overlay_help_exemption_all_is_exhaustive() {
        for (position, exemption) in OverlayHelpExemption::ALL.iter().enumerate() {
            assert_eq!(
                exemption.index(),
                position,
                "{exemption:?} out of place in ALL"
            );
        }
        let max_index = OverlayHelpExemption::ALL
            .iter()
            .map(|exemption| exemption.index())
            .max()
            .expect("exemptions exist");
        assert_eq!(OverlayHelpExemption::ALL.len(), max_index + 1);
    }

    #[test]
    fn every_overlay_help_class_has_one_route_with_a_close_key_and_an_action() {
        let classes = OverlayHelpClass::ALL;
        assert_eq!(classes.len(), OVERLAY_HELP_ROUTES.len());
        for &class in classes {
            assert_eq!(
                OVERLAY_HELP_ROUTES
                    .iter()
                    .filter(|route| route.class == class)
                    .count(),
                1,
                "{class:?} needs exactly one catalog route"
            );
            let Some(route) = overlay_help_route(class) else {
                panic!("{class:?} has no catalog route");
            };
            assert_eq!(HelpOrigin::OverlayClass(class).title(), route.title);
            assert!(
                route
                    .entries()
                    .any(|entry| entry.role == OverlayHelpRole::Close),
                "{class:?} help lists a close key"
            );
            assert!(
                route
                    .entries()
                    .any(|entry| entry.role != OverlayHelpRole::Close),
                "{class:?} help lists an overlay action"
            );
            for entry in route.entries() {
                assert!(!entry.keys.trim().is_empty(), "{class:?} has a keyless row");
                assert!(
                    !entry.label.trim().is_empty(),
                    "{class:?} has an empty label"
                );
            }
        }
    }

    #[test]
    fn overlay_catalog_entries_are_not_registry_actions() {
        // Discovery-only: catalog rows never become descriptors, aliases, or
        // palette commands; the registry keeps exactly its own commands.
        for route in OVERLAY_HELP_ROUTES {
            for entry in route.entries() {
                assert!(
                    !ACTION_DESCRIPTORS.iter().any(|descriptor| {
                        (descriptor.label, descriptor.category) == (entry.label, route.title)
                    }),
                    "{:?} row {:?} duplicates a registry descriptor",
                    route.class,
                    entry.label
                );
            }
        }
        let context = ActionContext {
            origin: HelpOrigin::OverlayClass(OverlayHelpClass::TrashBrowser),
            mode: ActionMode::Text,
            overlay_blocks_normal_actions: true,
            ..ActionContext::default()
        };
        let ids: Vec<_> = available_actions(&context)
            .into_iter()
            .map(|action| action.request.id)
            .collect();
        assert_eq!(ids, vec![ActionId::ContextHelp]);
        assert!(overlay_help_for(&context).is_some());
    }

    #[test]
    fn command_aliases_are_unique_and_resolve_by_longest_word_boundary() {
        let mut aliases = BTreeSet::new();
        for descriptor in ACTION_DESCRIPTORS {
            for alias in descriptor.command_aliases {
                assert!(aliases.insert(*alias), "duplicate command alias: {alias}");
                assert_eq!(
                    resolve_command(alias).map(|(found, rest)| (found.id, rest)),
                    Some((descriptor.id, ""))
                );
            }
        }
        assert_eq!(
            resolve_command("manager policy extra").map(|(descriptor, rest)| (descriptor.id, rest)),
            Some((ActionId::ManagerPolicy, "extra"))
        );
        assert!(resolve_command("refreshing").is_none());
        assert_eq!(
            resolve_command("rate 7").map(|(descriptor, rest)| (descriptor.id, rest)),
            Some((ActionId::Rate, "7"))
        );
    }

    #[test]
    fn daemon_feature_editability_requires_authoritative_config() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        let focused = app.active_tab().focused_pane;
        *app.active_tab_mut()
            .layout
            .find_pane_mut(focused)
            .expect("focused pane") = Pane::Settings;
        app.settings_state.section = SettingsSection::RetriesRecovery;
        app.settings_state.focus = SettingsFocus::Items;
        app.settings_state.selected_index = 0;
        app.poll.connected = true;

        let pending = ActionContext::from_app(&app);
        assert!(!pending.authoritative_config_ready);
        assert!(!pending.settings_row_editable);

        app.poll.authoritative_config_ready = true;
        let ready = ActionContext::from_app(&app);
        assert!(ready.authoritative_config_ready);
        assert!(ready.settings_row_editable);
    }

    #[test]
    fn system_prompt_and_classifier_require_authoritative_config_only() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        let focused = app.active_tab().focused_pane;
        *app.active_tab_mut()
            .layout
            .find_pane_mut(focused)
            .expect("focused pane") = Pane::Settings;
        app.settings_state.focus = SettingsFocus::Items;
        app.poll.connected = true;

        app.settings_state.section = SettingsSection::SystemPrompt;
        app.settings_state.selected_index = 0;
        assert!(!ActionContext::from_app(&app).settings_row_editable);

        app.settings_state.section = SettingsSection::ModelRoles;
        app.settings_state.selected_index = 0;
        assert!(ActionContext::from_app(&app).settings_row_editable);
        app.settings_state.selected_index = 5;
        assert!(!ActionContext::from_app(&app).settings_row_editable);

        app.poll.authoritative_config_ready = true;
        app.settings_state.section = SettingsSection::SystemPrompt;
        app.settings_state.selected_index = 0;
        assert!(ActionContext::from_app(&app).settings_row_editable);
        app.settings_state.section = SettingsSection::ModelRoles;
        app.settings_state.selected_index = 5;
        assert!(ActionContext::from_app(&app).settings_row_editable);
    }

    #[test]
    fn t30_descriptor_ids_are_unique_and_context_overloads_are_explicit() {
        let mut ids = BTreeSet::new();
        let mut bindings = BTreeSet::new();
        for descriptor in ACTION_DESCRIPTORS {
            assert!(
                ids.insert(descriptor.id),
                "duplicate action id {:?}",
                descriptor.id
            );
            for binding in descriptor.bindings {
                if !bindings.insert((binding.route, binding.sequence)) {
                    assert!(
                        binding.route == ActionRoute::IssueTracker
                            && matches!(binding.sequence, "S" | "Enter"),
                        "unexpected context-overloaded binding {:?} {}",
                        binding.route,
                        binding.sequence
                    );
                }
            }
        }
        for (route, sequence, expected) in [
            (ActionRoute::IssueTracker, "S", 2),
            (ActionRoute::IssueTracker, "Enter", 3),
        ] {
            assert_eq!(
                ACTION_DESCRIPTORS
                    .iter()
                    .flat_map(|descriptor| descriptor.bindings)
                    .filter(|binding| binding.route == route && binding.sequence == sequence)
                    .count(),
                expected,
                "context overload count changed for {route:?} {sequence}"
            );
        }
    }

    #[test]
    fn available_actions_equal_available_resolver_results() {
        for surface in [
            ActionSurface::SessionList,
            ActionSurface::Settings,
            ActionSurface::IssueTracker,
            ActionSurface::ScheduleBrowser,
            ActionSurface::ThemeRoleEditor,
        ] {
            let context = ActionContext {
                surface,
                has_selection: true,
                connected: true,
                selected_id: Some(Uuid::nil()),
                selected_issue_session_id: Some(Uuid::nil()),
                selected_theme_role: Some(ThemeRole::Accent),
                selected_theme_role_overridden: true,
                ..ActionContext::default()
            };
            for action in available_actions(&context) {
                assert_eq!(
                    availability(action.descriptor, &context),
                    ActionAvailability::Available(action.request)
                );
                assert_eq!(
                    recheck_request(&context, action.request),
                    ActionAvailability::Available(action.request)
                );
            }
        }
    }

    #[test]
    fn contextual_results_follow_selection_pending_and_authority() {
        let mut context = ActionContext {
            surface: ActionSurface::ScheduleBrowser,
            connected: true,
            ..ActionContext::default()
        };
        let empty: BTreeSet<_> = available_actions(&context)
            .into_iter()
            .map(|a| a.request.id)
            .collect();
        assert!(!empty.contains(&ActionId::ScheduleEdit));
        context.has_selection = true;
        context.selected_id = Some(Uuid::nil());
        let selected: BTreeSet<_> = available_actions(&context)
            .into_iter()
            .map(|a| a.request.id)
            .collect();
        assert!(selected.contains(&ActionId::ScheduleEdit));
        assert!(!selected.contains(&ActionId::ScheduleDelete));
        context.mode = ActionMode::PendingDelete;
        let pending: BTreeSet<_> = available_actions(&context)
            .into_iter()
            .map(|a| a.request.id)
            .collect();
        assert!(pending.contains(&ActionId::ScheduleDelete));
        context.connected = false;
        let disconnected: BTreeSet<_> = available_actions(&context)
            .into_iter()
            .map(|a| a.request.id)
            .collect();
        assert!(!disconnected.contains(&ActionId::ScheduleRefresh));
        assert!(!disconnected.contains(&ActionId::ScheduleNew));
        assert!(!disconnected.contains(&ActionId::ScheduleEdit));
        assert!(!disconnected.contains(&ActionId::ScheduleToggle));
        assert!(!disconnected.contains(&ActionId::ScheduleTrigger));
        assert!(!disconnected.contains(&ActionId::ScheduleDelete));

        context.connected = true;
        context.schedule_loading = true;
        let loading: BTreeSet<_> = available_actions(&context)
            .into_iter()
            .map(|a| a.request.id)
            .collect();
        assert!(!loading.contains(&ActionId::MoveDown));
        assert!(!loading.contains(&ActionId::ScheduleNew));
        assert!(!loading.contains(&ActionId::ScheduleEdit));
    }

    #[test]
    fn issue_workspace_registry_bindings_are_unique_context_fenced_and_raw_keyed() {
        let selected_id = Uuid::new_v4();
        let local = ActionContext {
            surface: ActionSurface::IssueTracker,
            issue_tab: Some(crate::types::IssueWorkspaceTab::Local),
            has_selection: true,
            selected_id: Some(selected_id),
            selected_issue_status: Some(ActionIssueStatus::Active),
            selected_issue_archived: Some(false),
            connected: true,
            ..ActionContext::default()
        };
        let dispatched = ActionContext {
            issue_tab: Some(crate::types::IssueWorkspaceTab::Dispatched),
            ..local
        };
        let text_editor = ActionContext {
            mode: ActionMode::Text,
            ..local
        };
        let stale_editor = ActionContext {
            mode: ActionMode::Text,
            issue_stale_editor: true,
            ..local
        };
        let local_only = [
            ActionId::IssueNew,
            ActionId::IssueEdit,
            ActionId::IssueStatus,
            ActionId::IssuePriority,
            ActionId::IssueAssignee,
            ActionId::IssueLabels,
            ActionId::IssueDependencies,
            ActionId::IssueArmCancel,
            ActionId::IssueYankArm,
            ActionId::IssueCopyUuid,
            ActionId::IssueCopyNumber,
            ActionId::IssueSearch,
            ActionId::IssueFilter,
            ActionId::IssueSort,
            ActionId::IssueJumpArm,
            ActionId::IssueJumpStart,
            ActionId::IssueJumpEnd,
        ];
        for id in local_only {
            let descriptor = ACTION_DESCRIPTORS
                .iter()
                .find(|descriptor| descriptor.id == id)
                .unwrap_or_else(|| panic!("missing Issues descriptor {id:?}"));
            assert!(matches!(
                availability(descriptor, &local),
                ActionAvailability::Available(_)
            ));
            assert!(matches!(
                availability(descriptor, &dispatched),
                ActionAvailability::Unavailable { .. }
            ));
            assert!(matches!(
                availability(descriptor, &text_editor),
                ActionAvailability::Unavailable { .. }
            ));
            assert_eq!(
                ACTION_DESCRIPTORS
                    .iter()
                    .filter(|candidate| candidate.id == id)
                    .count(),
                1,
                "{id:?} must have exactly one descriptor"
            );
        }

        for id in [ActionId::IssuePreviousTab, ActionId::IssueNextTab] {
            let descriptor = ACTION_DESCRIPTORS
                .iter()
                .find(|descriptor| descriptor.id == id)
                .expect("tab descriptor");
            assert!(matches!(
                availability(descriptor, &dispatched),
                ActionAvailability::Available(_)
            ));
        }

        let raw_cases = [
            (ActionMode::Normal, KeyCode::Char('n'), ActionId::IssueNew),
            (
                ActionMode::Normal,
                KeyCode::Char('d'),
                ActionId::IssueArmCancel,
            ),
            (
                ActionMode::PendingDelete,
                KeyCode::Char('d'),
                ActionId::IssueCancel,
            ),
            (
                ActionMode::PendingYank,
                KeyCode::Char('y'),
                ActionId::IssueCopyUuid,
            ),
            (
                ActionMode::PendingYank,
                KeyCode::Char('#'),
                ActionId::IssueCopyNumber,
            ),
            (
                ActionMode::PendingJump,
                KeyCode::Char('g'),
                ActionId::IssueJumpStart,
            ),
            (
                ActionMode::Normal,
                KeyCode::Char('G'),
                ActionId::IssueJumpEnd,
            ),
        ];
        for (mode, key, expected) in raw_cases {
            let context = ActionContext { mode, ..local };
            assert_eq!(
                request_for_key(&context, KeyEvent::new(key, KeyModifiers::NONE)),
                ActionAvailability::Available(materialize(&context, expected))
            );
        }
        assert_eq!(
            request_for_key(
                &local,
                KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT)
            ),
            ActionAvailability::Available(materialize(&local, ActionId::IssueStatus))
        );
        for (key, expected) in [
            (KeyCode::Char('l'), ActionId::IssueReloadLatest),
            (KeyCode::Char('r'), ActionId::IssueRebaseDraft),
        ] {
            let request = materialize(&stale_editor, expected);
            assert_eq!(
                request_for_key(&stale_editor, KeyEvent::new(key, KeyModifiers::CONTROL)),
                ActionAvailability::Available(request)
            );
            assert!(
                available_actions(&stale_editor)
                    .iter()
                    .any(|action| action.request == request)
            );
            assert_eq!(
                ACTION_DESCRIPTORS
                    .iter()
                    .filter(|descriptor| descriptor.id == expected)
                    .count(),
                1
            );
        }
    }

    #[test]
    fn issue_workspace_mutation_eligibility_and_confirmation_routes_are_exact() {
        let selected_id = Uuid::new_v4();
        let base = ActionContext {
            surface: ActionSurface::IssueTracker,
            issue_tab: Some(crate::types::IssueWorkspaceTab::Local),
            has_selection: true,
            selected_id: Some(selected_id),
            connected: true,
            ..ActionContext::default()
        };
        let guarded = [
            ActionId::IssueEdit,
            ActionId::IssueStatus,
            ActionId::IssueReopen,
            ActionId::IssuePriority,
            ActionId::IssueAssignee,
            ActionId::IssueLabels,
            ActionId::IssueDependencies,
            ActionId::IssueArmCancel,
            ActionId::IssueCancel,
            ActionId::IssueArchive,
            ActionId::IssueConfirmArchive,
            ActionId::IssueRestore,
        ];
        let cases = [
            (
                "active",
                ActionContext {
                    selected_issue_status: Some(ActionIssueStatus::Active),
                    selected_issue_archived: Some(false),
                    ..base
                },
                &[
                    ActionId::IssueEdit,
                    ActionId::IssueStatus,
                    ActionId::IssuePriority,
                    ActionId::IssueAssignee,
                    ActionId::IssueLabels,
                    ActionId::IssueDependencies,
                    ActionId::IssueArmCancel,
                ][..],
            ),
            (
                "terminal",
                ActionContext {
                    selected_issue_status: Some(ActionIssueStatus::Terminal),
                    selected_issue_archived: Some(false),
                    ..base
                },
                &[
                    ActionId::IssueReopen,
                    ActionId::IssueDependencies,
                    ActionId::IssueArchive,
                ][..],
            ),
            (
                "archived terminal",
                ActionContext {
                    selected_issue_status: Some(ActionIssueStatus::Terminal),
                    selected_issue_archived: Some(true),
                    ..base
                },
                &[ActionId::IssueDependencies, ActionId::IssueRestore][..],
            ),
            (
                "pending cancellation",
                ActionContext {
                    mode: ActionMode::PendingDelete,
                    selected_issue_status: Some(ActionIssueStatus::Active),
                    selected_issue_archived: Some(false),
                    ..base
                },
                &[ActionId::IssueCancel][..],
            ),
            (
                "pending archive",
                ActionContext {
                    mode: ActionMode::PendingArchive,
                    selected_issue_status: Some(ActionIssueStatus::Terminal),
                    selected_issue_archived: Some(false),
                    ..base
                },
                &[ActionId::IssueConfirmArchive][..],
            ),
        ];
        for (name, context, expected) in cases {
            for id in guarded {
                let descriptor = ACTION_DESCRIPTORS
                    .iter()
                    .find(|descriptor| descriptor.id == id)
                    .unwrap_or_else(|| panic!("missing descriptor {id:?}"));
                assert_eq!(
                    matches!(
                        availability(descriptor, &context),
                        ActionAvailability::Available(_)
                    ),
                    expected.contains(&id),
                    "{name} eligibility mismatch for {id:?}"
                );
            }
        }

        let terminal = cases[1].1;
        assert_eq!(
            request_for_key(
                &terminal,
                KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT)
            ),
            ActionAvailability::Available(materialize(&terminal, ActionId::IssueReopen))
        );
        let archived = cases[2].1;
        assert_eq!(
            request_for_key(
                &archived,
                KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)
            ),
            ActionAvailability::Available(materialize(&archived, ActionId::IssueDependencies))
        );
        assert_eq!(
            request_for_key(
                &archived,
                KeyEvent::new(KeyCode::Char('U'), KeyModifiers::SHIFT)
            ),
            ActionAvailability::Available(materialize(&archived, ActionId::IssueRestore))
        );
        let pending_archive = cases[4].1;
        assert_eq!(
            request_for_key(
                &pending_archive,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            ActionAvailability::Available(materialize(
                &pending_archive,
                ActionId::IssueConfirmArchive
            ))
        );
        let help = available_actions(&pending_archive);
        assert!(help.iter().any(|action| {
            action.request.id == ActionId::IssueConfirmArchive
                && action.descriptor.label == "Confirm archive"
        }));

        let loading_archived = ActionContext {
            issue_loading: true,
            ..archived
        };
        let dependency = ACTION_DESCRIPTORS
            .iter()
            .find(|descriptor| descriptor.id == ActionId::IssueDependencies)
            .expect("dependency descriptor");
        assert!(matches!(
            availability(dependency, &loading_archived),
            ActionAvailability::Unavailable { .. }
        ));

        let sync = ActionContext {
            surface: ActionSurface::IssueTracker,
            issue_tab: Some(crate::types::IssueWorkspaceTab::Sync),
            mode: ActionMode::Normal,
            connected: true,
            ..ActionContext::default()
        };
        assert_eq!(
            request_for_key(&sync, KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
            ActionAvailability::Available(materialize(&sync, ActionId::MoveDown))
        );
    }

    #[test]
    fn issue_sync_dispatched_form_inspector_and_retry_help_match_raw_routes() {
        let sync = ActionContext {
            surface: ActionSurface::IssueTracker,
            issue_tab: Some(crate::types::IssueWorkspaceTab::Sync),
            connected: true,
            ..ActionContext::default()
        };
        for (key, modifiers, expected) in [
            (
                KeyCode::Char('r'),
                KeyModifiers::NONE,
                ActionId::IssueRefresh,
            ),
            (
                KeyCode::Char('P'),
                KeyModifiers::SHIFT,
                ActionId::IssueRunPoll,
            ),
        ] {
            assert_eq!(
                request_for_key(&sync, KeyEvent::new(key, modifiers)),
                ActionAvailability::Available(ActionRequest::plain(expected))
            );
            assert!(
                available_actions(&sync)
                    .iter()
                    .any(|action| action.request.id == expected)
            );
        }
        assert!(matches!(
            request_for_key(&sync, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ActionAvailability::Unavailable { .. }
        ));

        let form = ActionContext {
            surface: ActionSurface::IssueTracker,
            issue_tab: Some(crate::types::IssueWorkspaceTab::Local),
            mode: ActionMode::Text,
            issue_form_selectable: true,
            connected: true,
            ..ActionContext::default()
        };
        assert_eq!(
            request_for_key(&form, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ActionAvailability::Available(ActionRequest::plain(ActionId::IssueSelectFormValue))
        );

        let retry = ActionContext {
            mode: ActionMode::Normal,
            issue_retry_pending: true,
            ..form
        };
        assert_eq!(
            request_for_key(&retry, KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)),
            ActionAvailability::Available(ActionRequest::plain(ActionId::IssueRetryMutation))
        );

        let inspector = ActionContext {
            mode: ActionMode::Normal,
            has_selection: true,
            issue_retry_pending: false,
            issue_inspector: true,
            ..form
        };
        for (key, expected) in [
            (KeyCode::Tab, ActionId::IssueInspectorNextSection),
            (KeyCode::PageUp, ActionId::IssueInspectorPreviousPage),
            (KeyCode::PageDown, ActionId::IssueInspectorNextPage),
        ] {
            assert_eq!(
                request_for_key(&inspector, KeyEvent::new(key, KeyModifiers::NONE)),
                ActionAvailability::Available(ActionRequest::plain(expected))
            );
        }

        let dispatched = ActionContext {
            issue_tab: Some(crate::types::IssueWorkspaceTab::Dispatched),
            issue_inspector: true,
            selected_issue_session_id: Some(Uuid::new_v4()),
            ..inspector
        };
        assert_eq!(
            request_for_key(
                &dispatched,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            ActionAvailability::Available(materialize(&dispatched, ActionId::IssueOpenSession))
        );
    }

    #[test]
    fn session_text_modes_own_keys_instead_of_advertising_normal_actions() {
        let normal = ActionContext {
            surface: ActionSurface::SessionList,
            mode: ActionMode::Normal,
            has_selection: true,
            ..ActionContext::default()
        };
        assert!(!available_actions(&normal).is_empty());

        for mode in [ActionMode::Search, ActionMode::Text] {
            let context = ActionContext { mode, ..normal };
            let help_actions = available_actions(&context);
            assert_eq!(help_actions.len(), 1);
            assert_eq!(help_actions[0].request.id, ActionId::ContextHelp);
            assert_eq!(
                bindings_for_action(&help_actions[0], &context),
                ["Ctrl-Alt-G"]
            );
            assert!(matches!(
                request_for_key(
                    &context,
                    KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)
                ),
                ActionAvailability::Unavailable { .. }
            ));
            assert!(!has_binding_for_key(
                &context,
                KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)
            ));
            assert!(has_binding_for_key(
                &context,
                KeyEvent::new(
                    KeyCode::Char('g'),
                    KeyModifiers::CONTROL | KeyModifiers::ALT
                )
            ));
        }
    }

    #[test]
    fn target_context_matrix_changes_help_and_preserves_dispatch_equality() {
        let base = ActionContext {
            surface: ActionSurface::SessionList,
            has_selection: true,
            selected_session_leaf: true,
            selected_session_status: Some(rsi_common::types::SessionStatus::Running),
            connected: true,
            operator: true,
            ..ActionContext::default()
        };
        let contexts = [
            base,
            ActionContext {
                has_selection: false,
                ..base
            },
            ActionContext {
                selected_session_leaf: false,
                selected_session_container: true,
                ..base
            },
            ActionContext {
                selected_session_status: Some(rsi_common::types::SessionStatus::Archived),
                ..base
            },
            ActionContext {
                operator: false,
                ..base
            },
            ActionContext {
                connected: false,
                ..base
            },
            ActionContext {
                hierarchy_descended: true,
                ..base
            },
            ActionContext {
                mode: ActionMode::Search,
                ..base
            },
        ];
        let baseline: BTreeSet<_> = available_actions(&base)
            .into_iter()
            .map(|action| action.request.id)
            .collect();
        for context in contexts {
            let ids: BTreeSet<_> = available_actions(&context)
                .into_iter()
                .map(|action| action.request.id)
                .collect();
            for request in available_actions(&context) {
                assert_eq!(
                    recheck_request(&context, request.request),
                    ActionAvailability::Available(request.request),
                    "{:?} must dispatch exactly as help advertises",
                    request.request.id
                );
            }
            if context != base {
                assert_ne!(ids, baseline, "context transition must change help");
            }
        }
    }

    #[test]
    fn t31_list_and_detail_proxy_help_expose_copy_uuid_with_yy_and_recheck() {
        use crate::app::app_test_helpers::with_session_list;

        let mut app = with_session_list(1);
        app.enter_session();
        app.detail_list_focused = true;
        let proxy = ActionContext::from_app(&app);
        assert_eq!(proxy.physical_pane, ActionPane::SessionDetail);
        assert_eq!(proxy.interaction_pane, ActionPane::SessionList);
        assert_eq!(proxy.surface, ActionSurface::SessionList);
        let copy = available_actions(&proxy)
            .into_iter()
            .find(|action| action.request.id == ActionId::CopySessionUuid)
            .expect("detail-list proxy must expose Copy Session UUID");
        assert_eq!(
            binding_sequence(copy.request.id, ActionRoute::Normal),
            Some("yy")
        );
        assert_eq!(
            recheck_request(&proxy, copy.request),
            ActionAvailability::Available(copy.request)
        );

        let focused = app.active_tab().focused_pane;
        *app.active_tab_mut()
            .layout
            .find_pane_mut(focused)
            .expect("focused pane") = Pane::Settings;
        app.detail_list_focused = false;
        app.settings_state.focus = SettingsFocus::Categories;
        let categories = ActionContext::from_app(&app);
        let category_ids: BTreeSet<_> = available_actions(&categories)
            .into_iter()
            .map(|action| action.request.id)
            .collect();
        app.settings_state.focus = SettingsFocus::Items;
        let items = ActionContext::from_app(&app);
        let item_ids: BTreeSet<_> = available_actions(&items)
            .into_iter()
            .map(|action| action.request.id)
            .collect();
        assert!(category_ids.contains(&ActionId::NavigateForward));
        assert!(!category_ids.contains(&ActionId::NavigateBack));
        assert!(item_ids.contains(&ActionId::NavigateBack));
        assert_ne!(category_ids, item_ids);

        let pane_id = app.active_tab().focused_pane;
        *app.active_tab_mut()
            .layout
            .find_pane_mut(pane_id)
            .expect("focused pane") =
            Pane::Issues(crate::types::IssueWorkspaceState::new(Some(Uuid::new_v4())));
        let empty_issue = ActionContext::from_app(&app);
        assert!(
            !available_actions(&empty_issue)
                .iter()
                .any(|action| action.request.id == ActionId::IssueOpenSession)
        );
    }

    #[test]
    fn context_captures_tab_zone_hierarchy_session_and_authority_facts() {
        use crate::app::app_test_helpers::with_session_list;
        use chrono::Utc;

        let mut app = with_session_list(2);
        let selected_id = app.selected_session_id().expect("selected session");
        let other_id = app
            .filtered_session_order
            .iter()
            .copied()
            .find(|id| *id != selected_id)
            .expect("second session");
        let project_id = Uuid::new_v4();
        app.active_tab_mut().project_id = Some(project_id);
        app.active_tab_mut().descent_path = vec![Uuid::new_v4()];
        if let Some(Pane::SessionList { active_zone, .. }) = app.focused_pane_mut() {
            *active_zone = SessionListZone::Jobs;
        }
        if let Some(state) = app.sessions.get_mut(&selected_id) {
            state.session.session_kind = rsi_common::types::SessionKind::Epic;
            state.session.status = rsi_common::types::SessionStatus::WaitingApproval;
            state.session.pinned_at = Some(Utc::now());
            state.session.lead_session_id = Some(other_id);
        }
        if let Some(state) = app.sessions.get_mut(&other_id) {
            state.session.session_kind = rsi_common::types::SessionKind::Group;
            state.session.lead_session_id = Some(selected_id);
        }
        app.poll.connected = true;
        app.poll.batch_fetch_supported = false;
        app.poll.push_supported = true;
        app.poll.memory_search_supported = true;
        app.poll.sandbox_supported = true;

        let context = ActionContext::from_app(&app);
        assert_eq!(context.active_tab_index, 0);
        assert_eq!(context.project_id, Some(project_id));
        assert_eq!(context.session_list_zone, Some(SessionListZone::Jobs));
        assert!(context.hierarchy_descended);
        assert_eq!(context.focus, ActionFocus::SessionList);
        assert_eq!(context.selection_kind, ActionSelectionKind::Session);
        assert_eq!(
            context.selected_session_kind,
            Some(rsi_common::types::SessionKind::Epic)
        );
        assert_eq!(
            context.selected_session_status,
            Some(rsi_common::types::SessionStatus::WaitingApproval)
        );
        assert!(context.selected_session_pinned);
        assert!(context.selected_session_container);
        assert!(!context.selected_session_leaf);
        assert!(context.selected_session_has_lead);
        assert!(context.operator);
        assert!(context.connected);
        assert!(!context.batch_fetch_supported);
        assert!(context.push_supported);
        assert!(context.memory_search_supported);
        assert!(context.sandbox_supported);
    }

    #[test]
    fn target_context_matrix_tracks_object_loading_pending_and_text_modes() {
        use crate::app::app_test_helpers::with_session_list;
        use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};

        let mut app = with_session_list(0);
        let linked_session = Uuid::new_v4();
        let pane_id = app.active_tab().focused_pane;
        let mut state = crate::types::IssueWorkspaceState::new(Some(Uuid::new_v4()));
        state.active_tab = crate::types::IssueWorkspaceTab::Dispatched;
        state.dispatched.selected_session_id = Some(linked_session);
        state
            .data_state_mut(crate::types::IssueWorkspaceDataKind::Dispatched)
            .load_state = crate::types::IssueWorkspaceLoadState::Fresh;
        state.transient.dispatched = vec![rsi_common::issue_workspace::IssueDispatchRecordV1 {
            issue_id: Uuid::new_v4().to_string(),
            issue_identifier: "ISSUE-1".to_string(),
            tracker: "fixture".to_string(),
            session_id: linked_session,
            dispatched_at: chrono::Utc::now(),
            last_reconciled_at: None,
            terminal_state: Some("completed".to_string()),
        }];
        *app.active_tab_mut()
            .layout
            .find_pane_mut(pane_id)
            .expect("focused pane") = Pane::Issues(state);
        let issue = ActionContext::from_app(&app);
        assert_eq!(issue.focus, ActionFocus::IssueList);
        assert_eq!(issue.selection_kind, ActionSelectionKind::Issue);
        assert_eq!(issue.selected_issue_session_id, Some(linked_session));
        assert_eq!(
            issue.selected_issue_status,
            Some(ActionIssueStatus::Terminal)
        );
        assert!(!issue.issue_loading);

        let now = chrono::Utc::now();
        let job_id = Uuid::new_v4();
        app.overlay = OverlayState::ScheduleBrowser {
            jobs: vec![ScheduledJob {
                id: job_id,
                name: "nightly".to_string(),
                message: "run".to_string(),
                schedule: ScheduleSpec {
                    recurrence: Recurrence::Once,
                    anchor: now,
                },
                last_fired_at: None,
                next_fire_at: now,
                enabled: false,
                working_dir: None,
                provider: None,
                model: None,
                project_id: None,
                created_at: now,
                updated_at: now,
                wake_mode: WakeMode::Fresh,
                wake_session_id: None,
            }],
            selected_index: 0,
            loading: false,
            pending_delete: true,
        };
        let schedule = ActionContext::from_app(&app);
        assert_eq!(schedule.focus, ActionFocus::ScheduleList);
        assert_eq!(schedule.selection_kind, ActionSelectionKind::ScheduledJob);
        assert_eq!(schedule.selected_id, Some(job_id));
        assert_eq!(schedule.selected_schedule_enabled, Some(false));
        assert_eq!(schedule.mode, ActionMode::PendingDelete);
        assert!(!schedule.schedule_loading);

        app.push_current_overlay();
        app.overlay = OverlayState::KeybindingsHelp {
            view: crate::overlay::keybindings_help::HelpView::Contextual,
            scroll_offset: 0,
            filter: "delete".to_string(),
            search_active: true,
            origin: HelpOrigin::ScheduleBrowser,
        };
        let help = ActionContext::from_app(&app);
        assert_eq!(help.surface, ActionSurface::ScheduleBrowser);
        assert!(help.help_search_active);
        assert_eq!(help.mode, ActionMode::PendingDelete);
        assert_eq!(help.selected_id, Some(job_id));

        app.overlay_stack.clear();
        app.overlay = OverlayState::ThemeRoleEditor {
            role: ThemeRole::Accent,
            input: "#010203".to_string(),
            opening_overrides: Vec::new(),
            assessment: None,
            committed: false,
            pending_acknowledgement: None,
        };
        let editing = ActionContext::from_app(&app);
        assert_eq!(editing.focus, ActionFocus::ThemeRoleInput);
        assert_eq!(editing.mode, ActionMode::RoleEdit);
        assert!(editing.role_editor_text_entry);

        if let OverlayState::ThemeRoleEditor { committed, .. } = &mut app.overlay {
            *committed = true;
        }
        let committed = ActionContext::from_app(&app);
        assert_eq!(committed.focus, ActionFocus::ThemeRoleCommitted);
        assert_eq!(committed.mode, ActionMode::Normal);
        assert!(!committed.role_editor_text_entry);
    }

    #[test]
    fn settings_empty_and_read_only_rows_omit_no_op_actions() {
        use crate::app::app_test_helpers::with_session_list;

        let mut app = with_session_list(0);
        let focused = app.active_tab().focused_pane;
        *app.active_tab_mut()
            .layout
            .find_pane_mut(focused)
            .expect("focused pane") = Pane::Settings;
        app.settings_state.focus = SettingsFocus::Items;
        app.settings_state.selected_index = 0;

        let ids_for = |app: &App| {
            available_actions(&ActionContext::from_app(app))
                .into_iter()
                .map(|action| action.request.id)
                .collect::<BTreeSet<_>>()
        };

        app.settings_state.section = SettingsSection::ClaudeSkills;
        app.cached_user_skills = Some(Vec::new());
        let skills = ids_for(&app);
        for id in [
            ActionId::Open,
            ActionId::ToggleSetting,
            ActionId::SettingsDelete,
            ActionId::SettingsEnable,
        ] {
            assert!(!skills.contains(&id), "empty skills advertised {id:?}");
        }

        app.settings_state.section = SettingsSection::RetriesRecovery;
        app.daemon_features.clear();
        let daemon = ids_for(&app);
        assert!(!daemon.contains(&ActionId::Open));
        assert!(!daemon.contains(&ActionId::ToggleSetting));

        app.settings_state.section = SettingsSection::Usage;
        let stats = ids_for(&app);
        assert!(!stats.contains(&ActionId::Open));
        assert!(!stats.contains(&ActionId::ToggleSetting));

        app.settings_state.section = SettingsSection::ApiProviders;
        app.settings.custom_providers.clear();
        let providers = ids_for(&app);
        assert!(providers.contains(&ActionId::Open));
        assert!(providers.contains(&ActionId::ToggleSetting));
        assert!(providers.contains(&ActionId::SettingsAdd));
        assert!(!providers.contains(&ActionId::SettingsDelete));

        app.settings_state.section = SettingsSection::ClaudeHooks;
        app.cached_hook_rows.clear();
        let hooks = ids_for(&app);
        assert!(hooks.contains(&ActionId::Open));
        assert!(hooks.contains(&ActionId::ToggleSetting));
        assert!(hooks.contains(&ActionId::SettingsAdd));
        assert!(!hooks.contains(&ActionId::SettingsDelete));

        app.settings_state.section = SettingsSection::Budgets;
        app.cached_model_control_status = None;
        let budgets = ids_for(&app);
        assert!(budgets.contains(&ActionId::Open));
        assert!(budgets.contains(&ActionId::ToggleSetting));
        assert!(budgets.contains(&ActionId::SettingsAdd));
        assert!(!budgets.contains(&ActionId::SettingsDelete));
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn role_editor_help_matches_text_ownership_state() {
        let text_context = ActionContext {
            surface: ActionSurface::ThemeRoleEditor,
            mode: ActionMode::RoleEdit,
            role_editor_text_entry: true,
            selected_theme_role: Some(ThemeRole::Accent),
            ..ActionContext::default()
        };
        let text_help = available_actions(&text_context);
        let help_action = text_help
            .iter()
            .find(|action| action.request.id == ActionId::ContextHelp)
            .expect("text-entry help remains discoverable");
        assert_eq!(
            bindings_for_action(help_action, &text_context),
            ["Ctrl-Alt-G"]
        );
        assert_eq!(
            request_for_key(
                &text_context,
                KeyEvent::new(
                    KeyCode::Char('g'),
                    KeyModifiers::CONTROL | KeyModifiers::ALT
                )
            ),
            ActionAvailability::Available(ActionRequest::plain(ActionId::ContextHelp))
        );
        assert!(matches!(
            request_for_key(
                &text_context,
                KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)
            ),
            ActionAvailability::Unavailable { .. }
        ));

        let committed_context = ActionContext {
            role_editor_text_entry: false,
            mode: ActionMode::Normal,
            ..text_context
        };
        let help_request = ActionRequest::plain(ActionId::ContextHelp);
        assert_eq!(
            request_for_key(
                &committed_context,
                KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)
            ),
            ActionAvailability::Available(help_request)
        );
        assert!(
            available_actions(&committed_context)
                .iter()
                .any(|action| action.request == help_request)
        );
    }

    #[test]
    fn session_mutations_advertise_only_their_executor_domains() {
        use rsi_common::types::SessionStatus;

        let session_ids = |context: ActionContext| {
            available_actions(&context)
                .into_iter()
                .map(|action| action.request.id)
                .filter(|id| {
                    matches!(
                        id,
                        ActionId::InterruptSession
                            | ActionId::ContinueSession
                            | ActionId::TogglePin
                            | ActionId::ArchiveSession
                            | ActionId::UnarchiveSession
                            | ActionId::ToggleTestingNeeded
                    )
                })
                .collect::<BTreeSet<_>>()
        };
        let context = |status, leaf, zone| ActionContext {
            surface: ActionSurface::SessionList,
            mode: ActionMode::Normal,
            connected: true,
            operator: true,
            has_selection: true,
            selected_session_leaf: leaf,
            selected_session_status: Some(status),
            session_list_zone: Some(zone),
            ..ActionContext::default()
        };

        assert_eq!(
            session_ids(context(SessionStatus::Running, true, SessionListZone::Main)),
            BTreeSet::from([
                ActionId::InterruptSession,
                ActionId::ContinueSession,
                ActionId::TogglePin,
                ActionId::ArchiveSession,
                ActionId::ToggleTestingNeeded,
            ])
        );
        assert_eq!(
            session_ids(context(
                SessionStatus::Starting,
                true,
                SessionListZone::Main
            )),
            BTreeSet::from([
                ActionId::InterruptSession,
                ActionId::TogglePin,
                ActionId::ArchiveSession,
                ActionId::ToggleTestingNeeded,
            ])
        );
        assert_eq!(
            session_ids(context(
                SessionStatus::Completed,
                true,
                SessionListZone::Main
            )),
            BTreeSet::from([
                ActionId::ContinueSession,
                ActionId::TogglePin,
                ActionId::ArchiveSession,
                ActionId::ToggleTestingNeeded,
            ])
        );
        assert_eq!(
            session_ids(context(
                SessionStatus::Archived,
                true,
                SessionListZone::Archive
            )),
            BTreeSet::from([
                ActionId::TogglePin,
                ActionId::ToggleTestingNeeded,
                ActionId::UnarchiveSession,
            ])
        );
        assert_eq!(
            session_ids(context(
                SessionStatus::Running,
                false,
                SessionListZone::Main
            )),
            BTreeSet::from([ActionId::TogglePin, ActionId::ArchiveSession])
        );

        let mut disconnected = context(SessionStatus::Running, true, SessionListZone::Main);
        disconnected.connected = false;
        assert!(session_ids(disconnected).is_empty());
        let mut non_operator = context(SessionStatus::Running, true, SessionListZone::Main);
        non_operator.operator = false;
        assert!(session_ids(non_operator).is_empty());
    }

    /// T4: every descriptor has an operator summary and a manual chapter.
    #[test]
    fn every_descriptor_has_summary_and_chapter() {
        for descriptor in ACTION_DESCRIPTORS {
            assert!(
                !descriptor.summary.trim().is_empty(),
                "{:?} has a summary",
                descriptor.id
            );
            assert!(
                crate::manual::model::chapter_for_category(descriptor.category).is_some(),
                "{:?} category {} has a manual chapter",
                descriptor.id,
                descriptor.category
            );
        }
    }

    /// T7: no two descriptors claim one Normal `(route, sequence)`, and no
    /// Normal binding is shadowed by a shorter binding that completes first
    /// (the failure that made the old `rc` / `rm` chords unreachable).
    /// Context-dependent duplicates on other routes are listed explicitly.
    #[test]
    fn normal_sequences_do_not_collide() {
        use std::collections::BTreeMap;

        let mut claims: BTreeMap<(ActionRoute, &str), Vec<ActionId>> = BTreeMap::new();
        for descriptor in ACTION_DESCRIPTORS {
            for binding in descriptor.bindings {
                claims
                    .entry((binding.route, binding.sequence))
                    .or_default()
                    .push(descriptor.id);
            }
        }
        let shared: BTreeMap<(ActionRoute, &str), Vec<ActionId>> = claims
            .iter()
            .filter(|(_, ids)| ids.len() > 1)
            .map(|(key, ids)| (*key, ids.clone()))
            .collect();
        assert_eq!(
            shared,
            BTreeMap::from([
                (
                    (ActionRoute::IssueTracker, "Enter"),
                    vec![
                        ActionId::IssueOpenSession,
                        ActionId::IssueSelectFormValue,
                        ActionId::IssueConfirmArchive,
                    ],
                ),
                (
                    (ActionRoute::IssueTracker, "S"),
                    vec![ActionId::IssueStatus, ActionId::IssueReopen],
                ),
            ])
        );

        // Key-level tokens: `<Space>` and named keys are one key each.
        fn keys(sequence: &str) -> Vec<String> {
            if matches!(
                sequence,
                "Enter" | "Backspace" | "F2" | "F3" | "Ctrl-M" | "Ctrl-Alt-G"
            ) {
                return vec![sequence.to_string()];
            }
            let mut rest = sequence;
            let mut keys = Vec::new();
            while let Some(first) = rest.chars().next() {
                if let Some(tail) = rest.strip_prefix("<Space>") {
                    keys.push("<Space>".to_string());
                    rest = tail;
                } else {
                    keys.push(first.to_string());
                    rest = &rest[first.len_utf8()..];
                }
            }
            keys
        }
        let normal: Vec<&str> = claims
            .keys()
            .filter(|(route, _)| *route == ActionRoute::Normal)
            .map(|(_, sequence)| *sequence)
            .collect();
        let reserved_prefixes: BTreeSet<&str> = NORMAL_KEY_RESERVATIONS
            .iter()
            .filter(|reservation| reservation.kind == NormalReservationKind::Prefix)
            .map(|reservation| reservation.sequence)
            .collect();
        let shadowed: Vec<(&str, &str)> = normal
            .iter()
            .flat_map(|short| {
                normal
                    .iter()
                    .filter(move |long| {
                        let (short_keys, long_keys) = (keys(short), keys(long));
                        long_keys.len() > short_keys.len() && long_keys.starts_with(&short_keys)
                    })
                    .map(move |long| (*short, *long))
            })
            .filter(|(short, _)| !reserved_prefixes.contains(short))
            .collect();
        assert_eq!(shadowed, Vec::<(&str, &str)>::new());
    }

    #[test]
    fn descriptor_order_is_stable_for_representative_context() {
        let context = ActionContext {
            surface: ActionSurface::SessionList,
            has_selection: true,
            ..ActionContext::default()
        };
        let ids: Vec<_> = available_actions(&context)
            .into_iter()
            .map(|action| action.request.id)
            .collect();
        assert_eq!(
            ids,
            vec![
                ActionId::ContextHelp,
                ActionId::MoveDown,
                ActionId::MoveUp,
                ActionId::JumpTop,
                ActionId::JumpBottom,
                ActionId::Open,
                ActionId::Search,
                ActionId::Refresh,
                ActionId::OpenThemePicker,
                ActionId::OpenLegacyColors,
                ActionId::OpenIssuesWorkspace,
                ActionId::ManagerPolicy,
                ActionId::ManagerBoard,
                ActionId::ManagerDecisions,
                ActionId::DeleteSession,
                ActionId::RotateSession,
                ActionId::Archives,
                ActionId::Alerts,
                ActionId::Quit,
                ActionId::Projects,
                ActionId::Task,
                ActionId::Blank,
                ActionId::SettingsCommand,
                ActionId::StopAll,
                ActionId::Graph,
                ActionId::Lead,
                ActionId::ClosePane,
                ActionId::TabNext,
                ActionId::TabPrev,
                ActionId::SearchNext,
                ActionId::SearchPrev,
                ActionId::NextAttention,
                ActionId::PrevAttention,
                ActionId::JumpAttention,
                ActionId::NextLabelGroup,
                ActionId::PrevLabelGroup,
                ActionId::GoToMainZone,
                ActionId::GoToTaskRabbitZone,
                ActionId::GoToJobsZone,
                ActionId::RecentCompletions,
                ActionId::NavigateRight,
                ActionId::AscendOrBack,
                ActionId::SortPicker,
                ActionId::Trash,
                ActionId::EnterInputInsert,
                ActionId::EnterSessionNormal,
                ActionId::PromptPreview,
                ActionId::NextUserMessage,
                ActionId::PrevUserMessage,
                ActionId::OpenFold,
                ActionId::CloseFold,
                ActionId::ToggleFold,
                ActionId::CloseAllFolds,
                ActionId::OpenAllFolds,
                ActionId::ToggleSystemEvents,
                ActionId::ToggleThinkingEvents,
                ActionId::SessionInfo,
                ActionId::RenameSession,
                ActionId::ModelDropdown,
                ActionId::ReassignProject,
                ActionId::ToggleRotation,
                ActionId::CancelRetry,
                ActionId::ExecuteDocRegBlocks,
                ActionId::CommitAndPush,
                ActionId::OpenSessionInNewTab,
                ActionId::GitPanel,
                ActionId::FileExplorer,
                ActionId::Telescope,
                ActionId::OpenRecentFile,
                ActionId::PromptCreator,
                ActionId::CommandPalette,
                ActionId::QuestionModal,
                ActionId::MemorySearch,
                ActionId::ScheduleBrowserOpen,
                ActionId::EspSquare,
                ActionId::RunEpicTopology,
                ActionId::CreateGroup,
                ActionId::CreateEpic,
                ActionId::CreateStory,
                ActionId::CreateTask,
                ActionId::CreateBug,
                ActionId::MoveToParent,
                ActionId::MoveToRoot,
            ]
        );
    }

    #[test]
    fn lc_action_and_aliases_share_action_identity() {
        assert_eq!(
            request_from_lc_action(&LcAction::ToggleKeybindingsHelp),
            Some(ActionRequest::plain(ActionId::ContextHelp))
        );
        assert_eq!(
            request_for_alias("help"),
            Some(ActionRequest::plain(ActionId::ContextHelp))
        );
        assert_eq!(
            request_for_alias("refresh"),
            Some(ActionRequest::plain(ActionId::Refresh))
        );
        assert_eq!(
            request_from_lc_action(&LcAction::OpenThemePicker),
            Some(ActionRequest::plain(ActionId::OpenThemePicker))
        );
        assert_eq!(
            request_from_lc_action(&LcAction::OpenColorCustomizer),
            Some(ActionRequest::plain(ActionId::OpenLegacyColors))
        );
        assert_eq!(
            request_from_lc_action(&LcAction::OpenIssuesWorkspace),
            Some(ActionRequest::plain(ActionId::OpenIssuesWorkspace))
        );
        assert_eq!(
            request_for_alias("issues"),
            Some(ActionRequest::plain(ActionId::OpenIssuesWorkspace))
        );
        assert_eq!(
            request_for_alias("manager inbox"),
            Some(ActionRequest::plain(ActionId::ManagerInbox))
        );
        assert_eq!(
            request_for_alias("manager inspect"),
            Some(ActionRequest::plain(ActionId::ManagerInspect))
        );
    }

    /// Exhaustive, wildcard-free `OverlayState` variant name (T11f): a new
    /// variant fails to compile here until it is named, then fails
    /// `overlay_state_fixtures_cover_every_variant` until it has a fixture.
    fn variant_name(overlay: &OverlayState) -> &'static str {
        match overlay {
            OverlayState::None => "None",
            OverlayState::CommandPalette { .. } => "CommandPalette",
            OverlayState::HarnessManagerScope(..) => "HarnessManagerScope",
            OverlayState::HarnessManagerV2(..) => "HarnessManagerV2",
            OverlayState::ThemePicker { .. } => "ThemePicker",
            OverlayState::ThemeRoleEditor { .. } => "ThemeRoleEditor",
            OverlayState::ColorCustomizer { .. } => "ColorCustomizer",
            OverlayState::TextAreaBgEditor { .. } => "TextAreaBgEditor",
            OverlayState::Prompt { .. } => "Prompt",
            OverlayState::ProjectPicker { .. } => "ProjectPicker",
            OverlayState::KeybindingsHelp { .. } => "KeybindingsHelp",
            OverlayState::SortPicker { .. } => "SortPicker",
            OverlayState::PromptPreview { .. } => "PromptPreview",
            OverlayState::SourceWorktreeSettlement(..) => "SourceWorktreeSettlement",
            OverlayState::TrashBrowser { .. } => "TrashBrowser",
            OverlayState::NotificationBrowser { .. } => "NotificationBrowser",
            OverlayState::RecentCompletions { .. } => "RecentCompletions",
            OverlayState::ProjectForm { .. } => "ProjectForm",
            OverlayState::FileExplorer { .. } => "FileExplorer",
            OverlayState::ProviderForm { .. } => "ProviderForm",
            OverlayState::MessageBridgeForm { .. } => "MessageBridgeForm",
            OverlayState::HookForm { .. } => "HookForm",
            OverlayState::BudgetPolicyForm { .. } => "BudgetPolicyForm",
            OverlayState::HookConflictPrompt { .. } => "HookConflictPrompt",
            OverlayState::SkillPreview { .. } => "SkillPreview",
            OverlayState::Diagnostics => "Diagnostics",
            OverlayState::RecursiveDagBrowser(..) => "RecursiveDagBrowser",
            OverlayState::MemorySearch { .. } => "MemorySearch",
            OverlayState::RenameSession { .. } => "RenameSession",
            OverlayState::QuestionModal { .. } => "QuestionModal",
            OverlayState::EspSquare { .. } => "EspSquare",
            OverlayState::LabelPicker { .. } => "LabelPicker",
            OverlayState::LabelForm { .. } => "LabelForm",
            OverlayState::InputModal { .. } => "InputModal",
            OverlayState::AiChat { .. } => "AiChat",
            OverlayState::AiCommand { .. } => "AiCommand",
            OverlayState::GraphReview { .. } => "GraphReview",
            OverlayState::CardEditor { .. } => "CardEditor",
            OverlayState::Telescope { .. } => "Telescope",
            OverlayState::Dialectic { .. } => "Dialectic",
            OverlayState::ScheduleBrowser { .. } => "ScheduleBrowser",
            OverlayState::ScheduleForm { .. } => "ScheduleForm",
            OverlayState::Terminal => "Terminal",
            OverlayState::RatingOverlay { .. } => "RatingOverlay",
            OverlayState::SessionInfoPanel { .. } => "SessionInfoPanel",
            OverlayState::CreateEntityForm { .. } => "CreateEntityForm",
            OverlayState::ParentPicker { .. } => "ParentPicker",
        }
    }

    /// One labelled overlay fixture: the routing computed from real state
    /// (or, for manager sub-modes that need daemon-loaded policy state, from
    /// the same facts `manager_help_routing` extracts) and its pinned value.
    struct OverlayFixture {
        label: &'static str,
        /// `Some` for fixtures built from a real `OverlayState`.
        variant: Option<&'static str>,
        routing: HelpRouting,
        expected: HelpRouting,
    }

    fn fixture_app() -> App {
        crate::app::app_test_helpers::with_session_list(1)
    }

    fn state_fixture(
        label: &'static str,
        app: &App,
        overlay: &OverlayState,
        expected: HelpRouting,
    ) -> OverlayFixture {
        OverlayFixture {
            label,
            variant: Some(variant_name(overlay)),
            routing: overlay_help_routing(app, overlay),
            expected,
        }
    }

    fn manager_config() -> rsi_common::harness_manager::HarnessManagerConfigV1 {
        let id = Uuid::new_v4();
        rsi_common::harness_manager::HarnessManagerConfigV1 {
            scope_mode: rsi_common::harness_manager::HarnessManagerScopeModeV1::Selected,
            selected_epic_ids: None,
            group_ids: Vec::new(),
            project_id: Uuid::new_v4(),
            manager_session_id: id,
            current_session_id: Some(id),
            epic_ids: Vec::new(),
            row_version: 1,
            updated_at: chrono::Utc::now(),
        }
    }

    fn manager_overlay(section: crate::overlay::manager_v2::ManagerSection) -> OverlayState {
        use rsi_common::harness_manager_v2::{
            AgentManagerInspectRequestV2, ManagerInspectSectionV2, ManagerInspectionV2,
        };
        let config = manager_config();
        let ledger = crate::overlay::manager_v2::board::BoardState::new(
            config.project_id,
            "Coordination desk".into(),
            AgentManagerInspectRequestV2::default(),
            ManagerInspectionV2 {
                observed_at: chrono::Utc::now(),
                scope_version: 1,
                policy: None,
                section: ManagerInspectSectionV2::Overview,
                rows: Vec::new(),
                next_cursor: None,
                complete: true,
            },
        );
        OverlayState::HarnessManagerV2(Box::new(crate::overlay::manager_v2::ManagerSurface {
            project_id: config.project_id,
            identity: "Coordination desk".into(),
            section,
            ledger,
            policy: None,
            config,
        }))
    }

    fn file_explorer(explorer_focused: bool, finder_active: bool) -> OverlayState {
        OverlayState::FileExplorer {
            root: std::path::PathBuf::from("/tmp"),
            entries: Vec::new(),
            selected_index: 0,
            scroll_offset: 0,
            show_hidden: false,
            trash: Vec::new(),
            pending_yank: false,
            pending_delete: false,
            finder_active,
            finder_query: String::new(),
            finder_cache: Vec::new(),
            finder_results: Vec::new(),
            finder_selected: 0,
            explorer_focused,
        }
    }

    fn settlement(authorization_active: bool) -> OverlayState {
        OverlayState::SourceWorktreeSettlement(crate::types::SourceWorktreeSettlementOverlayState {
            cohorts: Vec::new(),
            selected_index: 0,
            scroll_offset: 0,
            audit: None,
            receipt: None,
            authorization_input: String::new(),
            authorization_active,
            idempotency_key: None,
            last_error: None,
        })
    }

    fn question(mode: crate::types::PopupMode) -> OverlayState {
        OverlayState::QuestionModal {
            session_id: Uuid::nil(),
            questions: Vec::new(),
            current_question: 0,
            cursor: Vec::new(),
            selections: Vec::new(),
            textarea: Box::default(),
            mode,
            pending_operator: None,
        }
    }

    fn open_dropdown(app: &App) -> crate::types::ModelDropdownState {
        let mut dropdown = crate::types::ModelDropdownState::new(
            app.selected_provider,
            app.available_models.clone(),
            app.selected_model.as_deref(),
        );
        dropdown.open = true;
        dropdown
    }

    /// One fixture per `OverlayState` variant and per routing-relevant
    /// sub-mode, including every former `None` path (N1-N5).
    #[allow(clippy::too_many_lines)] // One auditable row per state.
    async fn overlay_state_fixtures() -> Vec<OverlayFixture> {
        use crate::overlay::manager_v2::ManagerSection;
        use crate::types::{CreateEntityField, GraphCamera, GraphMode, PopupMode};
        use HelpRouting::{Exempt, Routed};
        use OverlayHelpClass as C;
        use OverlayHelpExemption as X;

        let app = fixture_app();
        let session_id = app.selected_session_id().expect("fixture session");
        let mut fixtures = Vec::new();
        let mut push = |label, app: &App, overlay: OverlayState, expected| {
            fixtures.push(state_fixture(label, app, &overlay, expected));
        };

        // N5 catch-all states, each now a named exemption.
        push("None (N5)", &app, OverlayState::None, Exempt(X::NoOverlay));
        push(
            "ScheduleBrowser (N5)",
            &app,
            OverlayState::ScheduleBrowser {
                jobs: Vec::new(),
                selected_index: 0,
                loading: false,
                pending_delete: false,
            },
            Exempt(X::RegistryRoute(RegistryOverlayRoute::ScheduleBrowser)),
        );
        {
            let mut app = fixture_app();
            crate::overlay::theme_role_editor::open_theme_role_editor(&mut app, ThemeRole::Accent);
            let overlay = std::mem::replace(&mut app.overlay, OverlayState::None);
            push(
                "ThemeRoleEditor (N5)",
                &app,
                overlay,
                Exempt(X::RegistryRoute(RegistryOverlayRoute::ThemeRoleEditor)),
            );
        }
        {
            let mut app = fixture_app();
            crate::overlay::prompt::open_blank_popup(&mut app);
            let overlay = app.input_overlays.pop().expect("blank prompt opens");
            push("Prompt entry (N5)", &app, overlay, Exempt(X::PromptEntry));
            crate::overlay::prompt::open_blank_popup(&mut app);
            let mut overlay = app.input_overlays.pop().expect("blank prompt opens");
            if let OverlayState::Prompt { model_dropdown, .. } = &mut overlay {
                *model_dropdown = open_dropdown(&app);
            }
            push(
                "Prompt model dropdown",
                &app,
                overlay,
                Routed(C::ModelPicker),
            );
        }
        push(
            "KeybindingsHelp (N5)",
            &app,
            OverlayState::KeybindingsHelp {
                view: crate::overlay::keybindings_help::HelpView::Contextual,
                scroll_offset: 0,
                filter: String::new(),
                search_active: false,
                origin: HelpOrigin::SessionList,
            },
            Exempt(X::HelpItself),
        );
        push(
            "EspSquare (N5)",
            &app,
            OverlayState::EspSquare {
                round: 0,
                correct: 0,
                interactive: true,
                rounds: vec![None; 12],
                message: String::new(),
                flash: None,
                flash_deadline: None,
                target: 0,
                round_details: vec![None; 12],
                cursor: None,
                last_guess: None,
                started_at: std::time::Instant::now(),
            },
            Exempt(X::Game),
        );

        // N2: manager sections, text entry and the catalog picker.
        push(
            "HarnessManagerV2 Board",
            &app,
            manager_overlay(ManagerSection::Board),
            Routed(C::ManagerBoard),
        );
        push(
            "HarnessManagerV2 Decisions",
            &app,
            manager_overlay(ManagerSection::Decisions),
            Routed(C::ManagerDecisions),
        );
        push(
            "HarnessManagerV2 Inbox",
            &app,
            manager_overlay(ManagerSection::Inbox),
            Routed(C::ManagerBoard),
        );
        push(
            "HarnessManagerV2 Inspect",
            &app,
            manager_overlay(ManagerSection::Inspect(
                rsi_common::harness_manager_v2::ManagerInspectSectionV2::Health,
            )),
            Routed(C::ManagerBoard),
        );
        push(
            "HarnessManagerV2 Policy",
            &app,
            manager_overlay(ManagerSection::Policy),
            Routed(C::ManagerPolicy),
        );
        drop(push);
        for (label, section, text_entry, picker, expected) in [
            (
                "HarnessManagerV2 text entry (facts)",
                ManagerSection::Board,
                true,
                false,
                Routed(C::ManagerTextEntry),
            ),
            (
                "HarnessManagerV2 policy text entry (facts)",
                ManagerSection::Policy,
                true,
                true,
                Routed(C::ManagerTextEntry),
            ),
            (
                "HarnessManagerV2 catalog picker (N2, facts)",
                ManagerSection::Policy,
                false,
                true,
                Exempt(X::ManagerCatalogPicker),
            ),
        ] {
            fixtures.push(OverlayFixture {
                label,
                variant: None,
                routing: manager_routing(section, text_entry, picker),
                expected,
            });
        }
        let mut push = |label, app: &App, overlay: OverlayState, expected| {
            fixtures.push(state_fixture(label, app, &overlay, expected));
        };

        // N3: settlement authorization active / inactive.
        push(
            "SourceWorktreeSettlement browse",
            &app,
            settlement(false),
            Routed(C::SettlementBrowser),
        );
        push(
            "SourceWorktreeSettlement authorization (N3)",
            &app,
            settlement(true),
            Exempt(X::SettlementAuthorization),
        );

        // N4 and graph modes.
        {
            let mut app = fixture_app();
            crate::overlay::graph::open_graph_review(&mut app).await;
            let draft_id = match &app.overlay {
                OverlayState::GraphReview { draft_id, .. } => *draft_id,
                _ => panic!("graph review opens"),
            };
            let set = |app: &mut App, mode: Option<GraphMode>, dashboard: bool| {
                if let OverlayState::GraphReview {
                    mode: current,
                    dashboard_focused,
                    ..
                } = &mut app.overlay
                {
                    if let Some(mode) = mode {
                        *current = mode;
                    }
                    *dashboard_focused = dashboard;
                }
            };
            let camera = GraphCamera::FollowSelection;
            let snapshot = |app: &App| overlay_help_routing(app, &app.overlay);
            let mut graph_rows = Vec::new();
            graph_rows.push(("GraphReview empty", snapshot(&app), Routed(C::GraphEmpty)));
            set(&mut app, None, true);
            graph_rows.push((
                "GraphReview dashboard focused (N4)",
                snapshot(&app),
                Exempt(X::GraphDashboardFocus),
            ));
            set(&mut app, None, false);
            graph_rows.push((
                "GraphReview dashboard unfocused",
                snapshot(&app),
                Routed(C::GraphEmpty),
            ));
            app.graph_draft_mut(&draft_id)
                .expect("draft")
                .workflow
                .nodes
                .push(rsi_graph::format::NodeDef::action("fixture", "Fixture"));
            set(&mut app, Some(GraphMode::Navigate { camera }), false);
            graph_rows.push((
                "GraphReview navigate",
                snapshot(&app),
                Routed(C::GraphNavigate),
            ));
            set(&mut app, Some(GraphMode::Detail { camera }), false);
            graph_rows.push(("GraphReview detail", snapshot(&app), Routed(C::GraphDetail)));
            set(
                &mut app,
                Some(GraphMode::EditField {
                    field: crate::types::GraphEditField::Name,
                    camera,
                }),
                false,
            );
            graph_rows.push((
                "GraphReview edit field",
                snapshot(&app),
                Routed(C::GraphEditField),
            ));
            set(
                &mut app,
                Some(GraphMode::Picker {
                    kind: crate::types::GraphPickerKind::Topology,
                    selected_index: 0,
                    previous_mode: crate::types::GraphBrowseMode::Navigate,
                    camera,
                }),
                false,
            );
            graph_rows.push(("GraphReview picker", snapshot(&app), Routed(C::GraphPicker)));
            for (label, routing, expected) in graph_rows {
                fixtures.push(OverlayFixture {
                    label,
                    variant: Some("GraphReview"),
                    routing,
                    expected,
                });
            }
        }
        let mut push = |label, app: &App, overlay: OverlayState, expected| {
            fixtures.push(state_fixture(label, app, &overlay, expected));
        };

        // N1 (now unreachable by type) and the create-entity sub-modes.
        {
            let app = fixture_app();
            let variants: Vec<(&'static str, Box<dyn Fn(&mut OverlayState)>, HelpRouting)> = vec![
                (
                    "CreateEntityForm name normal (N1)",
                    Box::new(|overlay: &mut OverlayState| {
                        if let OverlayState::CreateEntityForm {
                            focused_field,
                            insert_mode,
                            ..
                        } = overlay
                        {
                            *focused_field = CreateEntityField::Name;
                            *insert_mode = false;
                        }
                    }),
                    Routed(C::CreateEntityNormal),
                ),
                (
                    "CreateEntityForm name insert",
                    Box::new(|overlay: &mut OverlayState| {
                        if let OverlayState::CreateEntityForm {
                            focused_field,
                            insert_mode,
                            ..
                        } = overlay
                        {
                            *focused_field = CreateEntityField::Name;
                            *insert_mode = true;
                        }
                    }),
                    Routed(C::CreateEntityInsert),
                ),
                (
                    "CreateEntityForm body insert",
                    Box::new(|overlay: &mut OverlayState| {
                        if let OverlayState::CreateEntityForm {
                            focused_field,
                            body,
                            ..
                        } = overlay
                        {
                            *focused_field = CreateEntityField::Body;
                            body.mode = PopupMode::Insert;
                        }
                    }),
                    Routed(C::CreateEntityInsert),
                ),
                (
                    "CreateEntityForm body normal",
                    Box::new(|overlay: &mut OverlayState| {
                        if let OverlayState::CreateEntityForm {
                            focused_field,
                            body,
                            ..
                        } = overlay
                        {
                            *focused_field = CreateEntityField::Body;
                            body.mode = PopupMode::Normal;
                        }
                    }),
                    Routed(C::CreateEntityBody),
                ),
                (
                    "CreateEntityForm topology",
                    Box::new(|overlay: &mut OverlayState| {
                        if let OverlayState::CreateEntityForm { focused_field, .. } = overlay {
                            *focused_field = CreateEntityField::Topology;
                        }
                    }),
                    Routed(C::CreateEntityTopology),
                ),
            ];
            let dropdown = open_dropdown(&app);
            let mut variants = variants;
            variants.push((
                "CreateEntityForm model dropdown",
                Box::new(move |overlay: &mut OverlayState| {
                    if let OverlayState::CreateEntityForm { model_dropdown, .. } = overlay {
                        *model_dropdown = Some(dropdown.clone());
                    }
                }),
                Routed(C::ModelPicker),
            ));
            for (label, mutate, expected) in variants {
                let mut app = fixture_app();
                crate::overlay::create_entity_form::open_create_entity_form(
                    &mut app,
                    rsi_common::types::SessionKind::Group,
                    None,
                )
                .await;
                let mut overlay = std::mem::replace(&mut app.overlay, OverlayState::None);
                mutate(&mut overlay);
                push(label, &app, overlay, expected);
            }
        }

        // FileExplorer sub-modes.
        push(
            "FileExplorer viewer focus",
            &app,
            file_explorer(false, false),
            Routed(C::FileExplorerViewerFocus),
        );
        push(
            "FileExplorer finder",
            &app,
            file_explorer(true, true),
            Routed(C::FileExplorerFinder),
        );
        push(
            "FileExplorer explorer",
            &app,
            file_explorer(true, false),
            Routed(C::FileExplorer),
        );

        // QuestionModal insert / normal.
        push(
            "QuestionModal insert",
            &app,
            question(PopupMode::Insert),
            Routed(C::QuestionInsert),
        );
        push(
            "QuestionModal normal",
            &app,
            question(PopupMode::Normal),
            Routed(C::QuestionNormal),
        );

        // Every remaining variant.
        {
            let mut app = fixture_app();
            crate::overlay::command_palette::open_command_palette(&mut app);
            let overlay = std::mem::replace(&mut app.overlay, OverlayState::None);
            push("CommandPalette", &app, overlay, Routed(C::CommandPalette));
        }
        push(
            "HarnessManagerScope",
            &app,
            OverlayState::HarnessManagerScope(Box::new(
                crate::overlay::harness_manager::HarnessManagerScopeState {
                    project_id: Uuid::nil(),
                    project_name: "Project".into(),
                    manager_name: "Manager".into(),
                    manager_session_id: Uuid::nil(),
                    expected_row_version: 0,
                    appointing: false,
                    rows: Vec::new(),
                    selected_epics: std::collections::HashSet::new(),
                    selected_groups: std::collections::HashSet::new(),
                    all_project: false,
                    selected: 0,
                    query: String::new(),
                    searching: false,
                    error: None,
                },
            )),
            Routed(C::ManagerScope),
        );
        push(
            "ThemePicker",
            &app,
            OverlayState::ThemePicker {
                selected_index: 0,
                original_index: 0,
            },
            Routed(C::ThemePicker),
        );
        {
            let mut app = fixture_app();
            crate::overlay::color_customizer::open_color_customizer(&mut app);
            let overlay = std::mem::replace(&mut app.overlay, OverlayState::None);
            push("ColorCustomizer", &app, overlay, Routed(C::ColorCustomizer));
        }
        push(
            "TextAreaBgEditor",
            &app,
            OverlayState::TextAreaBgEditor {
                input: String::new(),
                error: false,
            },
            Routed(C::TextAreaBgEditor),
        );
        push(
            "ProjectPicker",
            &app,
            OverlayState::ProjectPicker {
                filter: String::new(),
                selected_index: 0,
                context: crate::types::ProjectPickerContext::GlobalFilter,
            },
            Routed(C::ProjectPicker),
        );
        push(
            "SortPicker",
            &app,
            OverlayState::SortPicker { selected_index: 0 },
            Routed(C::SortPicker),
        );
        push(
            "PromptPreview",
            &app,
            OverlayState::PromptPreview { scroll_offset: 0 },
            Routed(C::PromptPreview),
        );
        push(
            "TrashBrowser",
            &app,
            OverlayState::TrashBrowser {
                sessions: Vec::new(),
                items: Vec::new(),
                selected_index: 0,
                scroll_offset: 0,
            },
            Routed(C::TrashBrowser),
        );
        push(
            "NotificationBrowser",
            &app,
            OverlayState::NotificationBrowser {
                selected_index: 0,
                scroll_offset: 0,
            },
            Routed(C::Notifications),
        );
        push(
            "RecentCompletions",
            &app,
            OverlayState::RecentCompletions { selected_index: 0 },
            Routed(C::RecentCompletions),
        );
        push(
            "ProjectForm",
            &app,
            OverlayState::ProjectForm {
                focused_field: 0,
                name: String::new(),
                path: String::new(),
                color_index: 0,
                editing_id: None,
            },
            Routed(C::ProjectForm),
        );
        {
            let mut app = fixture_app();
            crate::overlay::provider_form::open_provider_form(&mut app, None);
            let overlay = std::mem::replace(&mut app.overlay, OverlayState::None);
            push("ProviderForm", &app, overlay, Routed(C::ProviderForm));
        }
        push(
            "MessageBridgeForm",
            &app,
            OverlayState::MessageBridgeForm {
                bridge: crate::settings::MessageBridgeKind::Signal,
                focused_field: 0,
                enabled: false,
                account: String::new(),
                allow_from: String::new(),
                working_dir: String::new(),
            },
            Routed(C::MessageBridgeForm),
        );
        push(
            "HookForm",
            &app,
            OverlayState::HookForm {
                focused_field: 0,
                event_idx: None,
                event_name_other: None,
                matcher: String::new(),
                command: String::new(),
                timeout: String::new(),
                editing: None,
                snapshot_mtime: None,
                snapshot_bytes: Vec::new(),
            },
            Routed(C::HookForm),
        );
        {
            let mut app = fixture_app();
            crate::overlay::budget_policy_form::open_budget_policy_form(&mut app, None);
            let overlay = std::mem::replace(&mut app.overlay, OverlayState::None);
            push(
                "BudgetPolicyForm",
                &app,
                overlay,
                Routed(C::BudgetPolicyForm),
            );
        }
        push(
            "HookConflictPrompt",
            &app,
            OverlayState::HookConflictPrompt {
                pending: Box::new(crate::claude_config::PendingHookSave {
                    data: crate::claude_config::ClaudeSettings::default(),
                }),
            },
            Routed(C::HookConflict),
        );
        push(
            "SkillPreview",
            &app,
            OverlayState::SkillPreview {
                name: "skill".into(),
                content: String::new(),
                scroll_offset: 0,
            },
            Routed(C::SkillPreview),
        );
        push(
            "Diagnostics",
            &app,
            OverlayState::Diagnostics,
            Routed(C::Diagnostics),
        );
        push(
            "RecursiveDagBrowser",
            &app,
            OverlayState::RecursiveDagBrowser(crate::types::RecursiveDagBrowserState::loading(
                None,
            )),
            Routed(C::DagBrowser),
        );
        push(
            "MemorySearch",
            &app,
            OverlayState::MemorySearch {
                query: String::new(),
                results: Vec::new(),
                selected_index: 0,
                loading: false,
            },
            Routed(C::MemorySearch),
        );
        push(
            "RenameSession",
            &app,
            OverlayState::RenameSession {
                session_id,
                title: String::new(),
            },
            Routed(C::RenameSession),
        );
        push(
            "LabelPicker",
            &app,
            OverlayState::LabelPicker {
                filter: String::new(),
                selected_index: 0,
            },
            Routed(C::LabelPicker),
        );
        push(
            "LabelForm",
            &app,
            OverlayState::LabelForm {
                focused_field: 0,
                name: String::new(),
                description: String::new(),
                color_index: 0,
                editing_id: None,
            },
            Routed(C::LabelForm),
        );
        push(
            "InputModal",
            &app,
            OverlayState::InputModal {
                overlay_id: Uuid::nil(),
                surface: crate::input_surface::InputSurface::new_insert(),
                session_id,
            },
            Routed(C::InputModal),
        );
        push(
            "AiChat",
            &app,
            OverlayState::AiChat {
                messages: Vec::new(),
                input: String::new(),
                source_text: String::new(),
                source: crate::types::AiAssistantSource::InputBar(session_id),
                in_flight: false,
                scroll_offset: 0,
            },
            Routed(C::AiChat),
        );
        push(
            "AiCommand",
            &app,
            OverlayState::AiCommand {
                command: String::new(),
                source_text: String::new(),
                source: crate::types::AiAssistantSource::InputBar(session_id),
                in_flight: false,
            },
            Routed(C::AiCommand),
        );
        push(
            "CardEditor",
            &app,
            OverlayState::CardEditor {
                entity_type: "project".into(),
                entity_id: String::new(),
                display_name: String::new(),
                facts: Vec::new(),
                selected_index: 0,
                scroll_offset: 0,
                editing: None,
                loading: false,
                pending_delete: false,
            },
            Routed(C::CardEditor),
        );
        push(
            "Telescope",
            &app,
            OverlayState::Telescope {
                root: std::path::PathBuf::from("/tmp"),
                session_id,
                query: String::new(),
                file_cache: Vec::new(),
                results: Vec::new(),
                selected: 0,
            },
            Routed(C::Telescope),
        );
        push(
            "Dialectic",
            &app,
            OverlayState::Dialectic {
                messages: Vec::new(),
                sources: Vec::new(),
                input: String::new(),
                in_flight: false,
                scroll_offset: 0,
                sources_expanded: false,
                project_id: None,
            },
            Routed(C::Dialectic),
        );
        {
            let mut app = fixture_app();
            crate::overlay::schedule_form::open_schedule_form_new(&mut app);
            let overlay = std::mem::replace(&mut app.overlay, OverlayState::None);
            push("ScheduleForm", &app, overlay, Routed(C::ScheduleForm));
        }
        push(
            "Terminal",
            &app,
            OverlayState::Terminal,
            Routed(C::Terminal),
        );
        push(
            "RatingOverlay",
            &app,
            OverlayState::RatingOverlay {
                session_id,
                selected_rating: None,
            },
            Routed(C::Rating),
        );
        push(
            "SessionInfoPanel",
            &app,
            OverlayState::SessionInfoPanel { session_id },
            Routed(C::SessionInfo),
        );
        push(
            "ParentPicker",
            &app,
            OverlayState::ParentPicker {
                session_id,
                candidates: Vec::new(),
                selected: 0,
                query: String::new(),
            },
            Routed(C::ParentPicker),
        );
        fixtures
    }

    /// T11a: every overlay fixture routes to its pinned value; routed
    /// classes have a catalog route and every exemption is exercised.
    #[tokio::test]
    async fn every_overlay_state_fixture_has_expected_routing() {
        let fixtures = Box::pin(overlay_state_fixtures()).await;
        for fixture in &fixtures {
            assert_eq!(fixture.routing, fixture.expected, "{}", fixture.label);
            match fixture.expected {
                HelpRouting::Routed(class) => assert!(
                    overlay_help_route(class).is_some(),
                    "{} routes to {class:?}, which has a catalog route",
                    fixture.label
                ),
                HelpRouting::Exempt(exemption) => assert!(
                    OverlayHelpExemption::ALL.contains(&exemption),
                    "{} exemption {exemption:?} is listed in ALL",
                    fixture.label
                ),
            }
        }
        let exercised: BTreeSet<usize> = fixtures
            .iter()
            .filter_map(|fixture| match fixture.expected {
                HelpRouting::Exempt(exemption) => Some(exemption.index()),
                HelpRouting::Routed(_) => None,
            })
            .collect();
        assert_eq!(
            exercised,
            (0..OverlayHelpExemption::ALL.len()).collect::<BTreeSet<_>>()
        );
    }

    /// T11f: the fixtures cover exactly the `OverlayState` variants declared
    /// in `types/mod.rs`.
    #[tokio::test]
    async fn overlay_state_fixtures_cover_every_variant() {
        let source = include_str!("types/mod.rs");
        let start = source
            .find("pub enum OverlayState {")
            .expect("OverlayState enum present");
        let body = &source[start..];
        let body = &body[..body.find("\n}\n").expect("enum ends")];
        let declared: BTreeSet<&str> = body
            .lines()
            .skip(1)
            .filter(|line| line.starts_with("    ") && !line.starts_with("     "))
            .map(str::trim_start)
            .filter(|line| line.starts_with(|c: char| c.is_ascii_uppercase()))
            .map(|line| {
                let end = line
                    .find(|c: char| !c.is_ascii_alphanumeric())
                    .unwrap_or(line.len());
                &line[..end]
            })
            .collect();
        let fixtures = Box::pin(overlay_state_fixtures()).await;
        let covered: BTreeSet<&str> = fixtures
            .iter()
            .filter_map(|fixture| fixture.variant)
            .collect();
        assert_eq!(covered, declared);
    }
}
