//! Action dispatch for flywheel.
//!
//! Takes `Action<LcInfo>` values produced by the KeyManager and dispatches
//! them to App methods. The top-level `dispatch_action` routes action variants
//! to four domain sub-modules:
//!
//! - `session`    — session lifecycle (launch, continue, approve, archive …)
//! - `navigation` — pane/event navigation, folds, jumplist, search
//! - `overlay`    — overlay open/close, model/theme/project pickers
//! - `window`     — tabs, splits, input bar, settings, quit

mod cohort_settlement;
pub(crate) mod daemon_config;
mod navigation;
mod overlay;
pub(crate) mod session;
mod window;

pub(crate) use window::maybe_auto_expand_session_list;

use editor_types::Action;

use crate::app::App;
use crate::commands::{CommandResult, parse_command};
use crate::modalkit_types::{LcAction, LcInfo};
use crate::types::{InputMode, SplitDirection};

/// Dispatch a single action from the keybinding system.
/// Returns true if the action was handled (even as a no-op).
pub async fn dispatch_action(app: &mut App, action: Action<LcInfo>) {
    match action {
        Action::Application(lc) => dispatch_lc_action(app, lc).await,

        Action::CommandBar(editor_types::CommandBarAction::Focus(..)) => {
            crate::overlay::command_palette::open_command_palette(app);
        }

        Action::CommandBar(editor_types::CommandBarAction::Unfocus) => {
            app.input_mode = InputMode::Normal;
        }

        Action::Editor(ref editor_action) => {
            dispatch_editor(app, editor_action);
        }

        Action::Scroll(style) => {
            dispatch_scroll(app, style);
        }

        Action::Tab(tab_action) => {
            dispatch_tab(app, tab_action);
        }

        Action::Window(win_action) => {
            dispatch_window(app, win_action);
        }

        Action::Command(editor_types::CommandAction::Execute(_)) => {
            // Command submitted from modalkit's command mode.
            // We handle command execution in our own command mode instead.
        }

        Action::Command(editor_types::CommandAction::Run(cmd)) => {
            // A command string from modalkit — parse and execute it
            dispatch_command(app, &cmd).await;
        }

        // Standard vim actions we don't need yet
        Action::NoOp
        | Action::Macro(_)
        | Action::Jump(..)
        | Action::Repeat(_)
        | Action::KeywordLookup(_)
        | Action::RedrawScreen
        | Action::ShowInfoMessage(_)
        | Action::Suspend
        | Action::Search(..)
        | Action::Prompt(_) => {}

        // Catch-all for any future Action variants (#[non_exhaustive])
        _ => {}
    }
}

/// Route a flywheel-specific action to the appropriate domain sub-dispatcher.
pub(crate) async fn dispatch_lc_action(app: &mut App, action: LcAction) {
    if let Some(request) = crate::action_registry::request_from_lc_action(&action) {
        let context = crate::action_registry::ActionContext::from_app(app);
        if crate::action_registry::descriptor_for_request(&context, request).is_some()
            && let crate::action_registry::ActionAvailability::Unavailable { reason } =
                crate::action_registry::recheck_request(&context, request)
        {
            app.notify(reason);
            return;
        }
    }
    match action {
        // Session lifecycle: launch, interact, manage, docregblock exec, yank
        LcAction::TaskRabbitPrompt
        | LcAction::LaunchTaskRabbit(_)
        | LcAction::BlankPrompt
        | LcAction::LaunchBlank(_)
        | LcAction::InterruptSession
        | LcAction::QuickContinue
        | LcAction::ContinueSession(_)
        | LcAction::OpenQuestionModal
        | LcAction::DeleteSession
        | LcAction::ArchiveSession
        | LcAction::UnarchiveSession
        | LcAction::TogglePinSession
        | LcAction::ToggleTestingNeeded
        | LcAction::ToggleRotationDisabled
        | LcAction::CancelRetry
        | LcAction::ReassignSessionProject
        | LcAction::RotateSession
        | LcAction::CommitAndPush
        | LcAction::ExecuteDocRegBlocks
        | LcAction::YankEventContent
        | LcAction::RenameSession
        | LcAction::SubmitSessionTitle(_, _)
        | LcAction::SetActiveTask(_)
        // Phase 4 hierarchy creation chords
        | LcAction::CreateGroup
        | LcAction::CreateEpic
        | LcAction::CreateStory
        | LcAction::CreateTask
        | LcAction::CreateBug
        // Phase 5 epic lead assignment
        | LcAction::SetEpicLead
        // P1.12 — fire focused Epic's bound topology
        | LcAction::RunEpicTopology => session::dispatch(app, action).await,

        // Navigation: session/event/fold traversal, jumplist, search
        LcAction::EnterSession
        | LcAction::BackToList
        | LcAction::ListSessions
        | LcAction::NextAttention
        | LcAction::PrevAttention
        | LcAction::NextEvent
        | LcAction::PrevEvent
        | LcAction::NextUserMessage
        | LcAction::PrevUserMessage
        | LcAction::OpenFold
        | LcAction::CloseFold
        | LcAction::ToggleFold
        | LcAction::CloseAllFolds
        | LcAction::OpenAllFolds
        | LcAction::ToggleSystemEvents
        | LcAction::ToggleThinkingEvents
        | LcAction::JumpBack
        | LcAction::JumpForward
        | LcAction::EnterSearch
        | LcAction::NextSearchMatch
        | LcAction::PrevSearchMatch
        | LcAction::DetailScrollDown
        | LcAction::DetailScrollUp
        | LcAction::NextLabelBoundary
        | LcAction::PrevLabelBoundary
        // Phase 4 hierarchy navigation
        | LcAction::AscendContainer
        | LcAction::AscendOrBack
        // Numbered navigation and recent-file jumps
        | LcAction::JumpAttentionN(_)
        | LcAction::OpenRecentFileN(_) => {
            navigation::dispatch(app, action);
            window::maybe_auto_expand_session_list(app);
        }

        // Event-driven navigation refresh actions — async RPC calls to daemon
        LcAction::RefreshNavigation
        | LcAction::RefreshCurrentView
        | LcAction::RefreshMetadata => {
            navigation::dispatch_async_refresh(app, action).await;
        }

        // Zone cycling — sync dispatch + async archive refresh when landing on Archive
        LcAction::SessionListZoneNext | LcAction::SessionListZonePrev => {
            navigation::dispatch(app, action);
            window::maybe_auto_expand_session_list(app);
            // If we landed on the Archive zone, fetch archived sessions from daemon
            let landed_on_archive = if let crate::types::Pane::SessionList { active_zone, .. } =
                app.session_list_pane_mut()
            {
                matches!(active_zone, crate::types::SessionListZone::Archive)
            } else {
                false
            };
            if landed_on_archive {
                app.refresh_archived_sessions().await;
                // Update selected session now that archived order is populated
                let ar_idx = if let crate::types::Pane::SessionList {
                    archive_selected_index,
                    ..
                } = app.session_list_pane_mut()
                {
                    Some(*archive_selected_index)
                } else {
                    None
                };
                if let Some(idx) = ar_idx {
                    let new_sel = app.filtered_archived_order.get(idx).copied();
                    if let crate::types::Pane::SessionList {
                        selected_session, ..
                    } = app.session_list_pane_mut()
                    {
                        *selected_session = new_sel;
                    }
                }
            }
        }

        // Overlays: popups, pickers, project management, notifications
        LcAction::ToggleNotifications
        | LcAction::DismissAllNotifications
        | LcAction::ToggleFileExplorer
        | LcAction::OpenSortPicker
        | LcAction::GoToArchiveZone
        | LcAction::GoToSessionsZone
        | LcAction::GoToTaskRabbitZone
        | LcAction::GoToJobsZone
        | LcAction::OpenDiagnostics
        | LcAction::OpenRecursiveDagBrowser
        | LcAction::OpenMemorySearch
        | LcAction::OpenRecentCompletions
        | LcAction::ToggleModelDropdown
        | LcAction::SelectModel(_)
        | LcAction::OpenThemePicker
        | LcAction::SelectTheme(_)
        | LcAction::OpenProjectPicker
        | LcAction::SwitchProject(_)
        | LcAction::CreateProject { .. }
        | LcAction::EditProjectByName(_)
        | LcAction::DeleteProjectByName(_)
        | LcAction::ToggleKeybindingsHelp
        | LcAction::OpenAllCommands
        | LcAction::OpenManual(_)
        | LcAction::TogglePromptPreview
        | LcAction::OpenEspSquare
        | LcAction::OpenLabelPicker
        | LcAction::OpenTelescope
        | LcAction::OpenCommandPalette
        | LcAction::ToggleGitPanel
        | LcAction::OpenGraphReview
        | LcAction::ResolveTopologyAttempt(_)
        | LcAction::GoToTrash
        | LcAction::OpenProjectCard
        | LcAction::OpenUserCard
        | LcAction::AddProjectCardFact(_)
        | LcAction::AddUserCardFact(_)
        | LcAction::OpenIssuesWorkspace
        | LcAction::OpenDialectic
        | LcAction::AskQuery(_)
        | LcAction::OpenScheduleBrowser
        | LcAction::OpenColorCustomizer
        | LcAction::ToggleTerminal
        | LcAction::OpenRatingOverlay
        | LcAction::OpenSessionInfoPanel
        | LcAction::RateSession(_)
        // Phase 4 hierarchy reassignment chords
        | LcAction::MoveToParent
        | LcAction::MoveToRoot => overlay::dispatch(app, action).await,

        LcAction::OpenSourceWorktreeSettlement => {
            cohort_settlement::open(app).await;
        }

        LcAction::EditHarnessManagerPolicy
        | LcAction::OpenHarnessManagerBoard
        | LcAction::OpenHarnessManagerDecisions
        | LcAction::OpenHarnessManagerInbox
        | LcAction::OpenHarnessManagerInspect => {
            crate::overlay::manager_v2::open(app, action).await;
        }
        LcAction::OpenHarnessManager
        | LcAction::AppointHarnessManager
        | LcAction::EditHarnessManagerScope
        | LcAction::ClearHarnessManagerScope => {
            crate::overlay::harness_manager::dispatch(app, action).await;
        }

        // Window/layout: tabs, splits, input bar, settings, quit
        LcAction::OpenSessionInNewTab
        | LcAction::NavigateRight
        | LcAction::NextTab
        | LcAction::PrevTab
        | LcAction::EnterInputBarInsert(_)
        | LcAction::EnterSessionNormalMode
        | LcAction::OpenSettings
        | LcAction::OpenSettingsAt(_)
        | LcAction::OpenPromptCreator
        | LcAction::CloseFocusedPane
        | LcAction::GrowSidebar
        | LcAction::ShrinkSidebar
        | LcAction::Quit => window::dispatch(app, action),

        // Daemon config: toggle a feature flag or refresh the feature list
        LcAction::ToggleDaemonFeature(idx) => {
            daemon_config::toggle_daemon_feature(app, idx).await;
        }
        LcAction::EmergencyStopAll => {
            daemon_config::emergency_stop_all(app).await;
        }
        LcAction::CancelModelInvocation(invocation_id) => {
            daemon_config::cancel_model_invocation(app, invocation_id).await;
        }
        LcAction::RefreshDaemonFeatures => {
            daemon_config::refresh_daemon_features(app).await;
        }
        LcAction::SyncTitleModelConfig => {
            daemon_config::sync_title_model_config(app).await;
        }
        LcAction::SyncMemoryModelConfig => {
            daemon_config::sync_memory_model_config(app).await;
        }
        LcAction::SyncPromptProcessorConfig => {
            daemon_config::sync_prompt_processor_config(app).await;
        }
        LcAction::SyncClassifierModelConfig(model_id) => {
            daemon_config::sync_classifier_model_config(app, model_id);
        }
        LcAction::CycleSystemPromptPreset => {
            daemon_config::cycle_system_prompt_preset(app).await;
        }
        LcAction::RefreshUsageStats => {
            daemon_config::refresh_usage_stats(app).await;
        }
        LcAction::DeleteBudgetPolicy(idx) => {
            daemon_config::delete_model_budget_policy(app, idx).await;
        }
        LcAction::SubmitBudgetPolicy {
            policy,
            editing_index,
        } => {
            daemon_config::submit_model_budget_policy(app, policy, editing_index).await;
        }
    }
}

/// Gate a registry request once, then delegate to the existing ungated domain executor.
pub(crate) async fn dispatch_registered_action(
    app: &mut App,
    request: crate::action_registry::ActionRequest,
) -> bool {
    use crate::action_registry::{ActionAvailability, ActionId, ActionSurface};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let context = crate::action_registry::ActionContext::from_app(app);
    if let ActionAvailability::Unavailable { reason } =
        crate::action_registry::recheck_request(&context, request)
    {
        app.notify(reason);
        return false;
    }

    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    match request.id {
        ActionId::ContextHelp => crate::overlay::keybindings_help::open_contextual_help(app),
        ActionId::CopySessionUuid if context.surface == ActionSurface::SessionList => {
            if let Some(crate::action_registry::ActionPayload::Uuid(session_id)) =
                Some(request.payload)
            {
                crate::clipboard::osc52_copy(&session_id.to_string());
                app.notify(format!("session {session_id} copied"));
            } else {
                return false;
            }
        }
        ActionId::MoveDown if context.surface == ActionSurface::SessionList => app.nav_down(),
        ActionId::MoveUp if context.surface == ActionSurface::SessionList => app.nav_up(),
        ActionId::JumpTop if context.surface == ActionSurface::SessionList => app.jump_to_top(),
        ActionId::JumpBottom if context.surface == ActionSurface::SessionList => {
            app.jump_to_bottom();
        }
        ActionId::Open if context.surface == ActionSurface::SessionList => app.enter_session(),
        ActionId::Search if context.surface == ActionSurface::SessionList => {
            navigation::dispatch(app, LcAction::EnterSearch);
            window::maybe_auto_expand_session_list(app);
        }
        ActionId::Refresh if context.surface == ActionSurface::SessionList => {
            navigation::dispatch_async_refresh(app, LcAction::RefreshNavigation).await;
        }
        ActionId::OpenThemePicker if context.surface == ActionSurface::SessionList => {
            if matches!(&app.overlay, crate::types::OverlayState::ThemePicker { .. }) {
                app.overlay = crate::types::OverlayState::None;
            } else {
                crate::overlay::open_theme_picker(app);
            }
        }
        ActionId::OpenLegacyColors if context.surface == ActionSurface::SessionList => {
            if matches!(
                &app.overlay,
                crate::types::OverlayState::ColorCustomizer { .. }
            ) {
                app.overlay = crate::types::OverlayState::None;
            } else {
                crate::overlay::open_color_customizer(app);
            }
        }
        ActionId::OpenIssuesWorkspace if context.surface == ActionSurface::SessionList => {
            app.open_or_focus_issues();
        }
        ActionId::ManagerPolicy
        | ActionId::ManagerBoard
        | ActionId::ManagerDecisions
        | ActionId::ManagerInbox
        | ActionId::ManagerInspect
            if context.surface == ActionSurface::SessionList =>
        {
            // Same real executor as the <Space>gp/gb/gd chords and the
            // `:manager policy` / `:manager board` / `:manager decisions`
            // commands — no duplicated implementation.
            let action = match request.id {
                ActionId::ManagerPolicy => LcAction::EditHarnessManagerPolicy,
                ActionId::ManagerBoard => LcAction::OpenHarnessManagerBoard,
                ActionId::ManagerDecisions => LcAction::OpenHarnessManagerDecisions,
                ActionId::ManagerInbox => LcAction::OpenHarnessManagerInbox,
                ActionId::ManagerInspect => LcAction::OpenHarnessManagerInspect,
                _ => unreachable!(),
            };
            crate::overlay::manager_v2::open(app, action).await;
        }
        ActionId::MoveDown if context.surface == ActionSurface::Settings => app.nav_down(),
        ActionId::MoveUp if context.surface == ActionSurface::Settings => app.nav_up(),
        ActionId::Open
        | ActionId::ResetThemeRole
        | ActionId::NavigateBack
        | ActionId::NavigateForward
        | ActionId::ToggleSetting
        | ActionId::SettingsAdd
        | ActionId::SettingsDelete
        | ActionId::SettingsEnable
        | ActionId::SettingsRefresh
        | ActionId::SettingsSearch
        | ActionId::SettingsNextMatch
        | ActionId::SettingsPrevMatch
        | ActionId::Close
            if context.surface == ActionSurface::Settings =>
        {
            let code = match request.id {
                ActionId::ResetThemeRole => KeyCode::Delete,
                ActionId::NavigateBack => KeyCode::Char('h'),
                ActionId::NavigateForward => KeyCode::Char('l'),
                ActionId::ToggleSetting => KeyCode::Char(' '),
                ActionId::SettingsAdd => KeyCode::Char('a'),
                ActionId::SettingsDelete => KeyCode::Char('d'),
                ActionId::SettingsEnable => KeyCode::Char('e'),
                ActionId::SettingsRefresh => KeyCode::Char('R'),
                ActionId::SettingsSearch => KeyCode::Char('/'),
                ActionId::SettingsNextMatch => KeyCode::Char('n'),
                ActionId::SettingsPrevMatch => KeyCode::Char('N'),
                ActionId::Close => KeyCode::Char('q'),
                _ => KeyCode::Enter,
            };
            crate::settings_keys::handle_settings_key(app, key(code));
        }
        ActionId::MoveDown
        | ActionId::MoveUp
        | ActionId::IssueOpenSession
        | ActionId::IssueRefresh
        | ActionId::IssueRunPoll
        | ActionId::IssueSelectFormValue
        | ActionId::IssueRetryMutation
        | ActionId::IssueInspectorNextSection
        | ActionId::IssueInspectorPreviousPage
        | ActionId::IssueInspectorNextPage
        | ActionId::IssueNew
        | ActionId::IssueEdit
        | ActionId::IssueStatus
        | ActionId::IssueReopen
        | ActionId::IssuePriority
        | ActionId::IssueAssignee
        | ActionId::IssueLabels
        | ActionId::IssueDependencies
        | ActionId::IssueReloadLatest
        | ActionId::IssueRebaseDraft
        | ActionId::IssueArmCancel
        | ActionId::IssueCancel
        | ActionId::IssueArchive
        | ActionId::IssueConfirmArchive
        | ActionId::IssueRestore
        | ActionId::IssueYankArm
        | ActionId::IssueCopyUuid
        | ActionId::IssueCopyNumber
        | ActionId::IssuePreviousTab
        | ActionId::IssueNextTab
        | ActionId::IssueSearch
        | ActionId::IssueFilter
        | ActionId::IssueSort
        | ActionId::IssueJumpArm
        | ActionId::IssueJumpStart
        | ActionId::IssueJumpEnd
        | ActionId::Close
            if context.surface == ActionSurface::IssueTracker
                && matches!(
                    context.physical_pane,
                    crate::action_registry::ActionPane::Issues
                ) =>
        {
            return app.handle_issue_workspace_action(request.id).await;
        }
        ActionId::MoveDown
        | ActionId::MoveUp
        | ActionId::ScheduleNew
        | ActionId::ScheduleEdit
        | ActionId::ScheduleToggle
        | ActionId::ScheduleTrigger
        | ActionId::ScheduleArmDelete
        | ActionId::ScheduleDelete
        | ActionId::ScheduleRefresh
        | ActionId::Close
            if context.surface == ActionSurface::ScheduleBrowser =>
        {
            let code = match request.id {
                ActionId::MoveDown => KeyCode::Char('j'),
                ActionId::MoveUp => KeyCode::Char('k'),
                ActionId::ScheduleNew => KeyCode::Char('n'),
                ActionId::ScheduleEdit => KeyCode::Enter,
                ActionId::ScheduleToggle => KeyCode::Char(' '),
                ActionId::ScheduleTrigger => KeyCode::Char('t'),
                ActionId::ScheduleArmDelete | ActionId::ScheduleDelete => KeyCode::Char('d'),
                ActionId::ScheduleRefresh => KeyCode::Char('r'),
                _ => KeyCode::Char('q'),
            };
            crate::overlay::schedule_browser::handle_schedule_browser_key(app, key(code)).await;
        }
        ActionId::ThemeRoleCommit | ActionId::ThemeRoleReset | ActionId::Close
            if context.surface == ActionSurface::ThemeRoleEditor =>
        {
            let code = match request.id {
                ActionId::ThemeRoleCommit => KeyCode::Enter,
                ActionId::ThemeRoleReset => KeyCode::Delete,
                _ => KeyCode::Esc,
            };
            crate::overlay::theme_role_editor::handle_theme_role_editor_key(app, key(code));
        }
        _ => return false,
    }
    true
}

/// Handle editor actions from modalkit (motions like j/k/G/gg).
///
/// In a full modalkit integration, these would be handled by the `Scrollable` trait
/// on `LcWindow`. In our hybrid approach, we intercept motion-related editor actions
/// and translate them to App navigation methods.
fn dispatch_editor(app: &mut App, action: &editor_types::EditorAction) {
    use editor_types::EditorAction;
    use editor_types::prelude::{EditTarget, MoveDir1D, MovePosition, MoveType};

    match action {
        EditorAction::Edit(_, EditTarget::Motion(move_type, count)) => {
            let n = match count {
                editor_types::prelude::Count::Exact(n) => *n,
                _ => 1,
            };
            match move_type {
                MoveType::Line(MoveDir1D::Next) => {
                    // j navigates session list and settings; J handles detail scroll.
                    // When detail_list_focused is true, focused_pane() proxies to the
                    // SessionList pane via interaction_pane_id(), so this fires for list nav.
                    if !matches!(
                        app.focused_pane(),
                        Some(crate::types::Pane::SessionDetail { .. })
                    ) {
                        for _ in 0..n {
                            app.nav_down();
                        }
                    }
                }
                MoveType::Line(MoveDir1D::Previous) => {
                    if !matches!(
                        app.focused_pane(),
                        Some(crate::types::Pane::SessionDetail { .. })
                    ) {
                        for _ in 0..n {
                            app.nav_up();
                        }
                    }
                }
                MoveType::BufferPos(MovePosition::Beginning) => {
                    dispatch_registered_session_jump(
                        app,
                        crate::action_registry::ActionId::JumpTop,
                    );
                }
                MoveType::BufferPos(MovePosition::End) => {
                    dispatch_registered_session_jump(
                        app,
                        crate::action_registry::ActionId::JumpBottom,
                    );
                }
                _ => {} // Other motions (word, paragraph, etc.) not yet mapped
            }
        }
        EditorAction::History(_) => {
            // History checkpoint actions are emitted after every edit action;
            // silently ignore since we have no undo/redo.
        }
        _ => {} // Other editor actions (Complete, Cursor, InsertText, etc.) not applicable
    }
}

fn dispatch_registered_session_jump(app: &mut App, id: crate::action_registry::ActionId) {
    use crate::action_registry::{ActionAvailability, ActionRequest, ActionSurface};

    let context = crate::action_registry::ActionContext::from_app(app);
    if context.surface != ActionSurface::SessionList {
        match id {
            crate::action_registry::ActionId::JumpTop => app.jump_to_top(),
            crate::action_registry::ActionId::JumpBottom => app.jump_to_bottom(),
            _ => {}
        }
        return;
    }
    let request = ActionRequest::plain(id);
    if matches!(
        crate::action_registry::recheck_request(&context, request),
        ActionAvailability::Available(_)
    ) {
        match id {
            crate::action_registry::ActionId::JumpTop => app.jump_to_top(),
            crate::action_registry::ActionId::JumpBottom => app.jump_to_bottom(),
            _ => {}
        }
    }
}

/// Handle scroll actions from modalkit.
fn dispatch_scroll(app: &mut App, style: editor_types::prelude::ScrollStyle) {
    use editor_types::prelude::{MoveDir2D, MovePosition, ScrollSize, ScrollStyle};

    match style {
        ScrollStyle::Direction2D(dir, size, _count) => {
            let viewport = app.focused_session_viewport().unwrap_or(30);
            let amount = match size {
                ScrollSize::Cell => 1,
                ScrollSize::HalfPage => viewport / 2,
                ScrollSize::Page => viewport,
            };
            match dir {
                MoveDir2D::Up => {
                    for _ in 0..amount {
                        app.nav_up();
                    }
                }
                MoveDir2D::Down => {
                    for _ in 0..amount {
                        app.nav_down();
                    }
                }
                MoveDir2D::Left | MoveDir2D::Right => {
                    // Horizontal scrolling not yet implemented
                }
            }
        }
        ScrollStyle::CursorPos(pos, _axis) => match pos {
            MovePosition::Beginning => {
                dispatch_registered_session_jump(app, crate::action_registry::ActionId::JumpTop)
            }
            MovePosition::End => {
                dispatch_registered_session_jump(app, crate::action_registry::ActionId::JumpBottom)
            }
            MovePosition::Middle => {
                // Jump to middle — approximate
            }
        },
        ScrollStyle::LinePos(pos, _count) => match pos {
            MovePosition::Beginning => {
                dispatch_registered_session_jump(app, crate::action_registry::ActionId::JumpTop)
            }
            MovePosition::End => {
                dispatch_registered_session_jump(app, crate::action_registry::ActionId::JumpBottom)
            }
            MovePosition::Middle => {}
        },
    }
}

/// Handle tab actions from modalkit.
fn dispatch_tab(app: &mut App, action: editor_types::TabAction<LcInfo>) {
    use editor_types::TabAction;
    use editor_types::prelude::{FocusChange, MoveDir1D};

    app.cancel_search_if_active();

    match action {
        TabAction::Focus(FocusChange::Direction1D(MoveDir1D::Next, ..)) => {
            app.next_tab();
        }
        TabAction::Focus(FocusChange::Direction1D(MoveDir1D::Previous, ..)) => {
            app.prev_tab();
        }
        TabAction::Focus(FocusChange::Offset(count, _)) => {
            // {count}gt — go to tab by number
            let n = match count {
                editor_types::prelude::Count::Contextual => 0,
                editor_types::prelude::Count::MinusOne => 0,
                editor_types::prelude::Count::Exact(n) => n.saturating_sub(1),
            };
            if n < app.tabs.len() {
                app.active_tab = n;
                app.detail_list_focused = false;
                app.run_navigation_effect_if_changed();
            }
        }
        TabAction::Open(..) => {
            app.create_tab();
        }
        TabAction::Close(..) => {
            app.close_tab();
        }
        _ => {}
    }
}

/// Handle window actions from modalkit.
fn dispatch_window(app: &mut App, action: editor_types::WindowAction<LcInfo>) {
    use crate::app::NavDirection;
    use editor_types::WindowAction;
    use editor_types::prelude::{Axis, FocusChange, MoveDir1D, MoveDir2D};

    app.cancel_search_if_active();

    match action {
        WindowAction::Split(_, Axis::Horizontal, ..) => {
            app.split_focused(SplitDirection::Horizontal);
        }
        WindowAction::Split(_, Axis::Vertical, ..) => {
            app.split_focused(SplitDirection::Vertical);
        }
        WindowAction::Close(editor_types::prelude::WindowTarget::Single(..), _) => {
            app.close_focused_pane();
        }
        WindowAction::Close(editor_types::prelude::WindowTarget::AllBut(..), _) => {
            app.close_other_panes();
        }
        WindowAction::Focus(FocusChange::Direction2D(dir, ..)) => {
            let nav = match dir {
                MoveDir2D::Left => NavDirection::Left,
                MoveDir2D::Right => NavDirection::Right,
                MoveDir2D::Up => NavDirection::Up,
                MoveDir2D::Down => NavDirection::Down,
            };
            app.focus_neighbor(nav);
        }
        WindowAction::Focus(FocusChange::Direction1D(MoveDir1D::Next, ..)) => {
            app.focus_neighbor(NavDirection::Right);
        }
        WindowAction::Focus(FocusChange::Direction1D(MoveDir1D::Previous, ..)) => {
            app.focus_neighbor(NavDirection::Left);
        }
        _ => {}
    }
}

/// Execute a resolved catalog command from any command entry point.
#[allow(clippy::future_not_send)] // App is owned by the single-threaded event loop.
pub async fn dispatch_catalog_command(app: &mut App, command: &str) {
    match parse_command(command) {
        CommandResult::LcAction(action) => {
            dispatch_lc_action(app, action).await;
        }
        CommandResult::Unhandled(cmd) => {
            // Fall through to basic vim command handling
            dispatch_vim_command(app, &cmd);
        }
    }
}

pub use dispatch_catalog_command as dispatch_command;

/// Handle standard vim commands that modalkit would normally handle,
/// but since we intercept command mode ourselves, we handle the common
/// ones here.
fn dispatch_vim_command(app: &mut App, command: &str) {
    let id = crate::action_registry::resolve_command(command).map(|(descriptor, _)| descriptor.id);
    match id {
        Some(crate::action_registry::ActionId::Split) => {
            app.split_focused(SplitDirection::Horizontal);
        }
        Some(crate::action_registry::ActionId::VSplit) => {
            app.split_focused(SplitDirection::Vertical);
        }
        Some(crate::action_registry::ActionId::ClosePane) => app.close_focused_pane(),
        Some(crate::action_registry::ActionId::OnlyPane) => app.close_other_panes(),
        Some(crate::action_registry::ActionId::TabNew) => app.create_tab(),
        Some(crate::action_registry::ActionId::TabClose) => app.close_tab(),
        Some(crate::action_registry::ActionId::TabNext) => app.next_tab(),
        Some(crate::action_registry::ActionId::TabPrev) => app.prev_tab(),
        _ => {
            app.notify_error(format!("Unknown command: {}", command));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DaemonClient;
    use crate::state::{DevState, PersistedState};
    use std::path::PathBuf;

    fn test_app() -> App {
        DevState::clear();
        PersistedState::default().save();
        let mut app = App::new(DaemonClient::new(PathBuf::from("/tmp/test.sock")));
        if let Some(crate::types::Pane::SessionList {
            selected_index,
            selected_session,
            ..
        }) = app.focused_pane_mut()
        {
            *selected_index = 0;
            *selected_session = None;
        }
        app
    }

    #[tokio::test]
    async fn test_quit_action() {
        let mut app = test_app();
        dispatch_action(&mut app, Action::Application(LcAction::Quit)).await;
        assert!(app.quit);
    }

    #[tokio::test]
    async fn test_colon_opens_command_palette() {
        let mut app = test_app();
        dispatch_action(
            &mut app,
            Action::CommandBar(editor_types::CommandBarAction::Focus(
                ":".into(),
                editor_types::prelude::CommandType::Command,
                Box::new(Action::NoOp),
            )),
        )
        .await;
        assert!(
            matches!(&app.overlay, crate::types::OverlayState::CommandPalette { query, selected: 0, .. } if query.is_empty())
        );

        let mut leader_app = test_app();
        dispatch_lc_action(&mut leader_app, LcAction::OpenCommandPalette).await;
        let (
            crate::types::OverlayState::CommandPalette {
                query: colon_query,
                results: colon_results,
                selected: colon_selected,
                origin: colon_origin,
                ..
            },
            crate::types::OverlayState::CommandPalette {
                query: leader_query,
                results: leader_results,
                selected: leader_selected,
                origin: leader_origin,
                ..
            },
        ) = (&app.overlay, &leader_app.overlay)
        else {
            panic!("both routes open a command palette")
        };
        assert_eq!(colon_query, leader_query);
        assert_eq!(colon_results, leader_results);
        assert_eq!(colon_selected, leader_selected);
        assert_eq!(colon_origin, leader_origin);
    }

    #[tokio::test]
    async fn test_back_to_list_from_detail() {
        let mut app = test_app();
        // Add a session and enter detail view
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
            working_dir: PathBuf::from("/tmp"),
            git_branch: None,
            status: rsi_common::types::SessionStatus::Running,
            project_id: None,
            session_kind: rsi_common::types::SessionKind::Standard,
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
            context_usage_confidence: rsi_common::types::ContextUsageConfidence::default(),
            continued_from: None,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
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
        let id = session.id;
        app.sessions
            .insert(id, crate::types::SessionState::new(session));
        app.session_order.push(id);

        // Set up detail view
        let tab = &mut app.tabs[app.active_tab];
        let focused = tab.focused_pane;
        if let Some(pane) = tab.layout.find_pane_mut(focused) {
            *pane = crate::types::Pane::SessionDetail { session_id: id };
        }

        // BackToList is now a no-op from SessionDetail (unified navigation)
        assert!(!app.detail_list_focused);
        dispatch_action(&mut app, Action::Application(LcAction::BackToList)).await;
        assert!(!app.detail_list_focused); // Unchanged — no toggle
        // focused_pane() still returns SessionDetail (no proxy redirect)
        assert!(matches!(
            app.focused_pane(),
            Some(crate::types::Pane::SessionDetail { .. })
        ));
    }

    #[tokio::test]
    async fn test_back_to_list_noop_when_already_in_list() {
        let mut app = test_app();
        // Already in session list — BackToList should be a no-op
        dispatch_action(&mut app, Action::Application(LcAction::BackToList)).await;
        assert!(matches!(
            app.focused_pane(),
            Some(crate::types::Pane::SessionList { .. })
        ));
    }

    #[tokio::test]
    async fn test_scroll_down() {
        use editor_types::prelude::{Count, MoveDir2D, ScrollSize, ScrollStyle};

        let mut app = test_app();
        // Add sessions so nav_down has somewhere to go
        for i in 0..5 {
            let session = rsi_common::types::Session {
                context_fill_pct: None,
                id: uuid::Uuid::new_v4(),
                provider: rsi_common::types::SessionProvider::Claude,
                claude_session_id: None,
                query: format!("query {}", i),
                title: None,
                agent_role: None,
                epic_spawn_ordinal: None,
                description: None,
                short_summary: None,
                pending_question: None,
                pending_archive: false,
                working_dir: PathBuf::from("/tmp"),
                git_branch: None,
                status: rsi_common::types::SessionStatus::Completed,
                project_id: None,
                session_kind: rsi_common::types::SessionKind::Standard,
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
                context_usage_confidence: rsi_common::types::ContextUsageConfidence::default(),
                continued_from: None,
                pinned_at: None,
                testing_needed_at: None,
                rotation_disabled_at: None,
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
            let id = session.id;
            app.session_order.push(id);
            app.sessions
                .insert(id, crate::types::SessionState::new(session));
        }

        // Recalculate filtered order after adding sessions
        app.set_project_filter(None);

        dispatch_action(
            &mut app,
            Action::Scroll(ScrollStyle::Direction2D(
                MoveDir2D::Down,
                ScrollSize::Cell,
                Count::Contextual,
            )),
        )
        .await;

        match app.focused_pane() {
            Some(crate::types::Pane::SessionList { selected_index, .. }) => {
                assert_eq!(*selected_index, 1)
            }
            _ => panic!("Expected SessionList"),
        }
    }

    #[test]
    fn test_dispatch_vim_command_split() {
        let mut app = test_app();
        dispatch_vim_command(&mut app, "split");
        assert_eq!(app.active_tab().layout.leaf_ids().len(), 2);
    }

    #[test]
    fn test_dispatch_vim_command_unknown() {
        let mut app = test_app();
        dispatch_vim_command(&mut app, "foobar");
        assert!(
            app.notifications
                .back()
                .unwrap()
                .message
                .contains("Unknown")
        );
    }

    #[tokio::test]
    async fn test_navigate_attention_empty() {
        let mut app = test_app();
        // No sessions — should be a no-op, no panic
        dispatch_action(&mut app, Action::Application(LcAction::NextAttention)).await;
    }

    fn add_session(app: &mut App, status: rsi_common::types::SessionStatus) -> uuid::Uuid {
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
            working_dir: PathBuf::from("/tmp"),
            git_branch: None,
            status,
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
            context_usage_confidence: rsi_common::types::ContextUsageConfidence::default(),
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
        let id = session.id;
        app.session_order.push(id);
        app.sessions
            .insert(id, crate::types::SessionState::new(session));
        id
    }

    #[tokio::test]
    async fn t21_t25_list_yy_copies_selected_uuid_and_preserves_selection_state() {
        let mut app = test_app();
        let session_id = add_session(&mut app, rsi_common::types::SessionStatus::Running);
        app.filtered_session_order = vec![session_id];
        if let crate::types::Pane::SessionList {
            selected_index,
            selected_session,
            scroll_offset,
            active_zone,
            ..
        } = app.session_list_pane_mut()
        {
            *selected_index = 0;
            *selected_session = Some(session_id);
            *scroll_offset = 7;
            *active_zone = crate::types::SessionListZone::Main;
        }
        let before = (
            app.selected_session_id(),
            app.focused_pane().expect("list pane").clone(),
        );
        dispatch_action(&mut app, Action::Application(LcAction::YankEventContent)).await;
        let expected_toast = format!("session {session_id} copied");
        assert_eq!(
            app.notifications
                .back()
                .map(|notice| notice.message.as_str()),
            Some(expected_toast.as_str())
        );
        assert_eq!(app.selected_session_id(), before.0);
        assert_eq!(app.focused_pane().expect("list pane"), &before.1);
    }

    #[tokio::test]
    async fn t22_detail_list_proxy_yy_copies_the_selected_session_uuid() {
        let mut app = test_app();
        let session_id = add_session(&mut app, rsi_common::types::SessionStatus::Running);
        app.filtered_session_order = vec![session_id];
        if let crate::types::Pane::SessionList {
            selected_index,
            selected_session,
            ..
        } = app.session_list_pane_mut()
        {
            *selected_index = 0;
            *selected_session = Some(session_id);
        }
        app.enter_session();
        app.detail_list_focused = true;
        dispatch_action(&mut app, Action::Application(LcAction::YankEventContent)).await;
        let expected_toast = format!("session {session_id} copied");
        assert_eq!(
            app.notifications
                .back()
                .map(|notice| notice.message.as_str()),
            Some(expected_toast.as_str())
        );
    }

    #[tokio::test]
    async fn t23_t24_split_list_leaf_and_all_list_contexts_copy_the_same_uuid() {
        use crate::types::SplitDirection;

        let mut app = test_app();
        let session_id = add_session(&mut app, rsi_common::types::SessionStatus::Running);
        let expected_toast = format!("session {session_id} copied");
        app.filtered_session_order = vec![session_id];
        if let crate::types::Pane::SessionList {
            selected_index,
            selected_session,
            ..
        } = app.session_list_pane_mut()
        {
            *selected_index = 0;
            *selected_session = Some(session_id);
        }
        let mut copied_toasts = Vec::new();
        dispatch_action(&mut app, Action::Application(LcAction::YankEventContent)).await;
        copied_toasts.push(app.notifications.back().expect("toast").message.clone());

        app.enter_session();
        app.detail_list_focused = true;
        dispatch_action(&mut app, Action::Application(LcAction::YankEventContent)).await;
        copied_toasts.push(app.notifications.back().expect("toast").message.clone());

        app.back_to_list();
        app.split_focused(SplitDirection::Vertical);
        let split_leaf = app.active_tab().layout.leaf_ids()[1];
        app.active_tab_mut().focused_pane = split_leaf;
        if let Some(crate::types::Pane::SessionList {
            selected_session,
            selected_index,
            ..
        }) = app.focused_pane_mut()
        {
            *selected_index = 0;
            *selected_session = Some(session_id);
        }
        dispatch_action(&mut app, Action::Application(LcAction::YankEventContent)).await;
        copied_toasts.push(app.notifications.back().expect("toast").message.clone());
        assert_eq!(copied_toasts.len(), 3);
        assert!(
            copied_toasts
                .iter()
                .all(|toast| toast.ends_with(&expected_toast)),
            "each list context must copy the same complete UUID: {copied_toasts:?}"
        );
    }

    #[tokio::test]
    async fn t27_list_yy_without_selection_is_unavailable_and_emits_no_toast() {
        let mut app = test_app();
        dispatch_action(&mut app, Action::Application(LcAction::YankEventContent)).await;
        assert!(app.notifications.is_empty());
    }

    #[tokio::test]
    async fn t29_refresh_reconciliation_then_yy_uses_pane_selected_session_uuid() {
        let mut app = test_app();
        app.settings.sort_order = crate::app::SortOrder::StalestFirst;
        let selected_id = add_session(&mut app, rsi_common::types::SessionStatus::Completed);
        let mut selected = app
            .sessions
            .get(&selected_id)
            .expect("selected")
            .session
            .clone();
        selected.updated_at = chrono::Utc::now();
        app.update_sessions(vec![selected.clone()]);
        if let crate::types::Pane::SessionList {
            selected_index,
            selected_session,
            ..
        } = app.session_list_pane_mut()
        {
            *selected_index = 0;
            *selected_session = Some(selected_id);
        }
        let earlier_id = uuid::Uuid::new_v4();
        let mut earlier = selected.clone();
        earlier.id = earlier_id;
        earlier.query = "earlier refreshed row".to_string();
        earlier.updated_at = selected.updated_at - chrono::Duration::hours(1);
        app.update_sessions(vec![selected, earlier]);
        let pane_selected = app
            .selected_session_id()
            .expect("reconciled pane selection");
        assert_eq!(pane_selected, earlier_id);
        assert_eq!(
            match app.focused_pane() {
                Some(crate::types::Pane::SessionList { selected_index, .. }) => *selected_index,
                _ => unreachable!("list pane"),
            },
            0,
            "refresh kept the visual row while reconciling its selected UUID"
        );
        dispatch_action(&mut app, Action::Application(LcAction::YankEventContent)).await;
        let expected = format!("session {pane_selected} copied");
        assert!(
            app.notifications
                .back()
                .is_some_and(|notice| notice.message.ends_with(&expected))
        );
    }

    #[tokio::test]
    async fn t32_each_contextual_list_help_action_enters_a_registered_executor_arm() {
        use crate::action_registry::{ActionContext, available_actions};

        let mut exercised = 0usize;
        for request in {
            let mut app = test_app();
            let id = add_session(&mut app, rsi_common::types::SessionStatus::Completed);
            app.filtered_session_order = vec![id];
            if let crate::types::Pane::SessionList {
                selected_session,
                selected_index,
                ..
            } = app.session_list_pane_mut()
            {
                *selected_session = Some(id);
                *selected_index = 0;
            }
            available_actions(&ActionContext::from_app(&app))
                .into_iter()
                .map(|action| action.request)
                // Migrated Normal chords (Epic M A.1.3) are executed by their
                // LcAction handlers, not by a registry executor arm.
                .filter(|request| !crate::keybindings::is_lc_dispatch_only(request.id))
                .collect::<Vec<_>>()
        } {
            let mut app = test_app();
            let id = add_session(&mut app, rsi_common::types::SessionStatus::Completed);
            app.filtered_session_order = vec![id];
            if let crate::types::Pane::SessionList {
                selected_session,
                selected_index,
                ..
            } = app.session_list_pane_mut()
            {
                *selected_session = Some(id);
                *selected_index = 0;
            }
            assert!(
                dispatch_registered_action(&mut app, request).await,
                "help action {:?} did not reach an executor arm",
                request.id
            );
            exercised += 1;
        }
        assert!(
            exercised > 0,
            "list contextual help must expose executable actions"
        );
    }

    #[tokio::test]
    async fn issue_workspace_registered_open_reaches_first_class_pane_executor() {
        let mut app = test_app();
        assert!(
            dispatch_registered_action(
                &mut app,
                crate::action_registry::ActionRequest::plain(
                    crate::action_registry::ActionId::OpenIssuesWorkspace,
                ),
            )
            .await
        );
        assert!(matches!(
            app.focused_pane(),
            Some(crate::types::Pane::Issues(_))
        ));
    }

    #[tokio::test]
    async fn t26_transcript_yy_keeps_yanking_event_content_and_empty_notice() {
        let mut app = test_app();
        let session_id = add_session(&mut app, rsi_common::types::SessionStatus::Running);
        if let Some(state) = app.sessions.get_mut(&session_id) {
            state.events.push(rsi_common::types::ConversationEvent {
                id: 0,
                session_id,
                sequence: 1,
                event_type: rsi_common::types::EventType::Message,
                role: Some(rsi_common::types::Role::Assistant),
                content: "positive transcript destination".to_string(),
                tool_name: None,
                tool_input: None,
                created_at: chrono::Utc::now(),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            });
            state.current_event_index = Some(0);
        }
        let focused = app.active_tab().focused_pane;
        *app.active_tab_mut()
            .layout
            .find_pane_mut(focused)
            .expect("focused pane") = crate::types::Pane::SessionDetail { session_id };
        dispatch_action(&mut app, Action::Application(LcAction::YankEventContent)).await;
        assert_eq!(
            app.notifications
                .back()
                .map(|notice| notice.message.as_str()),
            Some("1 line yanked")
        );
        app.sessions.get_mut(&session_id).expect("session").events[0]
            .content
            .clear();
        dispatch_action(&mut app, Action::Application(LcAction::YankEventContent)).await;
        assert!(
            app.notifications
                .back()
                .is_some_and(|notice| notice.message.contains("Nothing to yank"))
        );
    }

    #[tokio::test]
    async fn test_navigate_attention_jumps_to_waiting() {
        use rsi_common::types::SessionStatus;
        let mut app = test_app();

        let _id1 = add_session(&mut app, SessionStatus::Running);
        let id2 = add_session(&mut app, SessionStatus::WaitingApproval);
        let _id3 = add_session(&mut app, SessionStatus::Completed);

        dispatch_action(&mut app, Action::Application(LcAction::NextAttention)).await;

        // Should jump to the WaitingApproval session
        assert_eq!(app.selected_session_id(), Some(id2));
    }

    #[tokio::test]
    async fn test_navigate_attention_jumps_to_pending_question_even_before_status_update() {
        use rsi_common::types::{QuestionItem, SessionStatus};
        let mut app = test_app();

        let _id1 = add_session(&mut app, SessionStatus::Running);
        let id2 = add_session(&mut app, SessionStatus::Running);
        app.sessions.get_mut(&id2).unwrap().session.pending_question =
            Some(rsi_common::types::PendingQuestion {
                questions: vec![QuestionItem {
                    question: "Choose?".to_string(),
                    header: "Q".to_string(),
                    options: Vec::new(),
                    multi_select: false,
                }],
            });

        dispatch_action(&mut app, Action::Application(LcAction::NextAttention)).await;

        assert_eq!(app.selected_session_id(), Some(id2));
    }

    #[tokio::test]
    async fn test_navigate_attention_cycles() {
        use rsi_common::types::SessionStatus;
        let mut app = test_app();

        let id1 = add_session(&mut app, SessionStatus::WaitingApproval);
        let _id2 = add_session(&mut app, SessionStatus::Running);
        let id3 = add_session(&mut app, SessionStatus::Failed);

        // First forward jump
        dispatch_action(&mut app, Action::Application(LcAction::NextAttention)).await;
        assert_eq!(app.selected_session_id(), Some(id1));

        // Second forward jump should go to id3 (Failed)
        dispatch_action(&mut app, Action::Application(LcAction::NextAttention)).await;
        assert_eq!(app.selected_session_id(), Some(id3));

        // Third forward wraps to id1
        dispatch_action(&mut app, Action::Application(LcAction::NextAttention)).await;
        assert_eq!(app.selected_session_id(), Some(id1));
    }

    #[tokio::test]
    async fn test_dispatch_command_task() {
        let mut app = test_app();
        dispatch_command(&mut app, "task").await;
        assert!(matches!(
            app.focused_input_overlay(),
            Some(crate::types::OverlayState::Prompt { .. })
        ));
    }

    #[tokio::test]
    async fn test_dispatch_command_quit() {
        let mut app = test_app();
        dispatch_command(&mut app, "q").await;
        assert!(app.quit);
    }

    #[tokio::test]
    async fn test_dispatch_command_split_fallthrough() {
        let mut app = test_app();
        dispatch_command(&mut app, "vsplit").await;
        assert_eq!(app.active_tab().layout.leaf_ids().len(), 2);
    }

    #[tokio::test]
    async fn test_open_recursive_dag_browser_action_opens_disconnected_overlay() {
        let mut app = test_app();

        dispatch_action(
            &mut app,
            Action::Application(LcAction::OpenRecursiveDagBrowser),
        )
        .await;

        match &app.overlay {
            crate::types::OverlayState::RecursiveDagBrowser(state) => {
                assert_eq!(
                    state.load_status,
                    crate::types::RecursiveDagLoadStatus::Disconnected
                );
                assert!(
                    state
                        .message
                        .as_deref()
                        .is_some_and(|message| message.contains("daemon is not connected"))
                );
            }
            _ => panic!("expected recursive DAG browser overlay"),
        }
        assert!(app.recursive_dag_rx.is_none());
    }

    #[tokio::test]
    async fn test_tab_action_next() {
        let mut app = test_app();
        app.create_tab();
        app.active_tab = 0;

        dispatch_action(
            &mut app,
            Action::Tab(editor_types::TabAction::Focus(
                editor_types::prelude::FocusChange::Direction1D(
                    editor_types::prelude::MoveDir1D::Next,
                    editor_types::prelude::Count::Contextual,
                    true,
                ),
            )),
        )
        .await;

        assert_eq!(app.active_tab, 1);
    }

    #[tokio::test]
    async fn test_window_action_split() {
        let mut app = test_app();
        dispatch_action(
            &mut app,
            Action::Window(editor_types::WindowAction::Split(
                editor_types::prelude::OpenTarget::Current,
                editor_types::prelude::Axis::Vertical,
                editor_types::prelude::MoveDir1D::Previous,
                editor_types::prelude::Count::Contextual,
            )),
        )
        .await;

        assert_eq!(app.active_tab().layout.leaf_ids().len(), 2);
    }

    #[tokio::test]
    async fn test_window_action_close() {
        let mut app = test_app();
        app.split_focused(SplitDirection::Vertical);
        assert_eq!(app.active_tab().layout.leaf_ids().len(), 2);

        dispatch_action(
            &mut app,
            Action::Window(editor_types::WindowAction::Close(
                editor_types::prelude::WindowTarget::Single(
                    editor_types::prelude::FocusChange::Current,
                ),
                editor_types::prelude::CloseFlags::QUIT,
            )),
        )
        .await;

        assert_eq!(app.active_tab().layout.leaf_ids().len(), 1);
    }

    #[tokio::test]
    async fn test_jump_to_top() {
        use editor_types::prelude::{Axis, MovePosition, ScrollStyle};

        let mut app = test_app();
        for _ in 0..5 {
            add_session(&mut app, rsi_common::types::SessionStatus::Completed);
        }
        // Navigate down a few times
        app.nav_down();
        app.nav_down();

        // Jump to top via CursorPos
        dispatch_action(
            &mut app,
            Action::Scroll(ScrollStyle::CursorPos(
                MovePosition::Beginning,
                Axis::Vertical,
            )),
        )
        .await;

        match app.focused_pane() {
            Some(crate::types::Pane::SessionList { selected_index, .. }) => {
                assert_eq!(*selected_index, 0)
            }
            _ => panic!("Expected SessionList"),
        }
    }

    #[tokio::test]
    async fn test_interrupt_session() {
        let mut app = test_app();
        add_session(&mut app, rsi_common::types::SessionStatus::Running);

        // InterruptSession won't actually work without daemon, but should not panic
        dispatch_action(&mut app, Action::Application(LcAction::InterruptSession)).await;
        // No assertion — just verifying no panic
    }

    #[tokio::test]
    async fn test_enter_session() {
        let mut app = test_app();
        let id = add_session(&mut app, rsi_common::types::SessionStatus::Running);

        // Set selected session
        let tab = &mut app.tabs[app.active_tab];
        let focused = tab.focused_pane;
        if let Some(pane) = tab.layout.find_pane_mut(focused) {
            *pane = crate::types::Pane::SessionList {
                selected_index: 0,
                selected_session: Some(id),
                scroll_offset: 0,
                active_zone: Default::default(),
                taskrabbit_selected_index: 0,
                archive_selected_index: 0,
                jobs_selected_index: 0,
            };
        }

        dispatch_action(&mut app, Action::Application(LcAction::EnterSession)).await;

        assert!(matches!(
            app.focused_pane(),
            Some(crate::types::Pane::SessionDetail { .. })
        ));
    }

    #[tokio::test]
    async fn test_continue_session_no_query_opens_popup() {
        use rsi_common::types::SessionStatus;
        let mut app = test_app();
        let id = add_session(&mut app, SessionStatus::Completed);

        // Select the session
        let tab = &mut app.tabs[app.active_tab];
        let focused = tab.focused_pane;
        if let Some(pane) = tab.layout.find_pane_mut(focused) {
            *pane = crate::types::Pane::SessionList {
                selected_index: 0,
                selected_session: Some(id),
                scroll_offset: 0,
                active_zone: Default::default(),
                taskrabbit_selected_index: 0,
                archive_selected_index: 0,
                jobs_selected_index: 0,
            };
        }

        dispatch_action(
            &mut app,
            Action::Application(LcAction::ContinueSession(None)),
        )
        .await;

        // Should open continue popup with the correct session
        match &app.overlay {
            crate::types::OverlayState::Prompt { purpose, .. } => {
                assert_eq!(*purpose, crate::types::PromptPurpose::ContinueSession(id));
            }
            _ => panic!("Expected Prompt overlay"),
        }
    }

    #[tokio::test]
    async fn test_continue_session_no_session_selected() {
        let mut app = test_app();
        // No sessions added — no session selected

        dispatch_action(
            &mut app,
            Action::Application(LcAction::ContinueSession(None)),
        )
        .await;

        assert!(
            app.notifications
                .back()
                .unwrap()
                .message
                .contains("No session selected")
        );
        // Should NOT enter input mode
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    #[tokio::test]
    async fn test_input_purpose_resets_on_esc() {
        let mut app = test_app();
        let id = add_session(&mut app, rsi_common::types::SessionStatus::Completed);

        // Set input_purpose to ContinueSession
        app.input_purpose = crate::types::InputPurpose::ContinueSession(id);
        app.input_mode = InputMode::Input;

        // Simulate Esc key
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let esc_key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        crate::event::test_handle_input_mode(&mut app, esc_key).await;

        assert_eq!(app.input_purpose, crate::types::InputPurpose::NewSession);
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    #[tokio::test]
    async fn test_input_purpose_resets_on_submit() {
        let mut app = test_app();
        let id = add_session(&mut app, rsi_common::types::SessionStatus::Completed);

        // Set input_purpose to ContinueSession
        app.input_purpose = crate::types::InputPurpose::ContinueSession(id);
        app.input_mode = InputMode::Input;
        app.input_buffer = "follow up".to_string();

        // Simulate Enter key
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let enter_key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        crate::event::test_handle_input_mode(&mut app, enter_key).await;

        assert_eq!(app.input_purpose, crate::types::InputPurpose::NewSession);
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    // --- Event navigation tests ---

    fn make_event(
        event_type: rsi_common::types::EventType,
        content: &str,
        tool_input: Option<serde_json::Value>,
    ) -> rsi_common::types::ConversationEvent {
        rsi_common::types::ConversationEvent {
            id: 0,
            session_id: uuid::Uuid::new_v4(),
            sequence: 1,
            event_type,
            role: Some(rsi_common::types::Role::Assistant),
            content: content.to_string(),
            tool_name: None,
            tool_input: tool_input.map(Box::new),
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    // padding-aware-bounds (RSI-020): heights shifted +1 per event after b4f1cf74
    // added a top-padding row in build_event_lines(). update_event_heights() also
    // adds +2 per event for the block border, so display_height = render_height + 2.
    // Concrete offsets are derived empirically by running the tests; the comments
    // beside each event below reflect the actual `event_offsets` produced by
    // `update_event_heights(state, 80)` at the time of writing.
    fn setup_detail_with_events(app: &mut App) -> uuid::Uuid {
        use rsi_common::types::{EventType, SessionStatus};

        let id = add_session(app, SessionStatus::Running);

        // Switch to detail view
        let tab = &mut app.tabs[app.active_tab];
        let focused = tab.focused_pane;
        if let Some(pane) = tab.layout.find_pane_mut(focused) {
            *pane = crate::types::Pane::SessionDetail { session_id: id };
        }

        // Add events to session state. Empirical heights from
        // `update_event_heights(state, 80)` after b4f1cf74 padding:
        //   - Message "hello":          render=4 + border=2 = 6 lines, offset 0
        //   - ToolUse "x\ny" w/ {}:     observed empirically (see Phase 3 log)
        //   - System "info":            observed empirically (see Phase 3 log)
        // Tests below assert against the live values so future render changes
        // surface as targeted failures instead of stale arithmetic.
        if let Some(state) = app.sessions.get_mut(&id) {
            state.events = vec![
                make_event(EventType::Message, "hello", None),
                make_event(EventType::ToolUse, "x\ny", Some(serde_json::json!({}))),
                make_event(EventType::System, "info", None),
            ];
            // Pre-populate height cache (simulates what the renderer does each frame)
            crate::ui::height::update_event_heights(state, 80);
        }

        id
    }

    /// padding-aware-bounds (RSI-020): helper to read event 1's actual offset
    /// after `update_event_heights`. Lets tests assert against live cache rather
    /// than baked-in numbers that drift every time the renderer changes padding.
    fn event_offset(app: &App, id: uuid::Uuid, idx: usize) -> usize {
        app.sessions
            .get(&id)
            .and_then(|s| s.event_offsets.get(idx).copied())
            .unwrap_or_default()
    }

    fn setup_detail_with_interleaved_messages(app: &mut App) -> uuid::Uuid {
        use rsi_common::types::{EventType, Role, SessionStatus};

        let id = add_session(app, SessionStatus::Running);
        let tab = &mut app.tabs[app.active_tab];
        let focused = tab.focused_pane;
        if let Some(pane) = tab.layout.find_pane_mut(focused) {
            *pane = crate::types::Pane::SessionDetail { session_id: id };
        }

        let first_created_at = chrono::DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let events = (0..52)
            .map(|idx| {
                let mut event = make_event(EventType::Message, &format!("message {idx}"), None);
                event.sequence = idx + 1;
                event.role = Some(if (idx % 2 == 0 && idx != 50) || idx == 51 {
                    Role::User
                } else {
                    Role::Assistant
                });
                event.created_at = first_created_at + chrono::Duration::hours(i64::from(idx));
                event
            })
            .collect();

        let state = app.sessions.get_mut(&id).unwrap();
        state.events = events;
        crate::ui::height::update_event_heights(state, 80);
        assert_eq!(state.events.len(), 52);
        assert!(state.event_heights.iter().all(|&height| height > 0));

        id
    }

    #[tokio::test]
    async fn test_next_event_jumps_to_second_event() {
        let mut app = test_app();
        let id = setup_detail_with_events(&mut app);
        // padding-aware-bounds (RSI-020): expected offset is event_offsets[1].
        let expected_offset_1 = event_offset(&app, id, 1);

        // Start at event 0, offset 0
        let state = app.sessions.get_mut(&id).unwrap();
        state.scroll_offset = 0;
        state.current_event_index = Some(0);

        dispatch_action(&mut app, Action::Application(LcAction::NextEvent)).await;

        let state = app.sessions.get(&id).unwrap();
        assert_eq!(state.scroll_offset, expected_offset_1);
        assert_eq!(state.current_event_index, Some(1));
    }

    #[tokio::test]
    async fn test_prev_event_jumps_to_first_event() {
        let mut app = test_app();
        let id = setup_detail_with_events(&mut app);
        // padding-aware-bounds (RSI-020): start/end against live offsets.
        let start_offset_2 = event_offset(&app, id, 2);
        let expected_offset_1 = event_offset(&app, id, 1);

        // Start at event 2
        let state = app.sessions.get_mut(&id).unwrap();
        state.scroll_offset = start_offset_2;
        state.current_event_index = Some(2);

        dispatch_action(&mut app, Action::Application(LcAction::PrevEvent)).await;

        let state = app.sessions.get(&id).unwrap();
        assert_eq!(state.scroll_offset, expected_offset_1);
        assert_eq!(state.current_event_index, Some(1));
    }

    #[tokio::test]
    async fn test_next_event_noop_at_last() {
        let mut app = test_app();
        let id = setup_detail_with_events(&mut app);
        // padding-aware-bounds (RSI-020): start at last event's live offset.
        let start_offset_2 = event_offset(&app, id, 2);

        // Start at event 2 (last)
        let state = app.sessions.get_mut(&id).unwrap();
        state.scroll_offset = start_offset_2;
        state.current_event_index = Some(2);

        dispatch_action(&mut app, Action::Application(LcAction::NextEvent)).await;

        // Should stay at event 2, offset unchanged
        let state = app.sessions.get(&id).unwrap();
        assert_eq!(state.scroll_offset, start_offset_2);
        assert_eq!(state.current_event_index, Some(2));
    }

    #[tokio::test]
    async fn test_prev_event_jumps_to_zero() {
        let mut app = test_app();
        let id = setup_detail_with_events(&mut app);
        // padding-aware-bounds (RSI-020): start at event 1's live offset.
        let start_offset_1 = event_offset(&app, id, 1);

        // Start at event 1
        let state = app.sessions.get_mut(&id).unwrap();
        state.scroll_offset = start_offset_1;
        state.current_event_index = Some(1);

        dispatch_action(&mut app, Action::Application(LcAction::PrevEvent)).await;

        // Should jump to event 0, offset 0
        let state = app.sessions.get(&id).unwrap();
        assert_eq!(state.scroll_offset, 0);
        assert_eq!(state.current_event_index, Some(0));
    }

    #[tokio::test]
    async fn test_next_event_noop_empty_events() {
        let mut app = test_app();
        let id = add_session(&mut app, rsi_common::types::SessionStatus::Running);

        // Switch to detail view (no events)
        let tab = &mut app.tabs[app.active_tab];
        let focused = tab.focused_pane;
        if let Some(pane) = tab.layout.find_pane_mut(focused) {
            *pane = crate::types::Pane::SessionDetail { session_id: id };
        }

        dispatch_action(&mut app, Action::Application(LcAction::NextEvent)).await;

        // Should remain at 0 — no panic, silent no-op
        assert_eq!(app.sessions.get(&id).unwrap().scroll_offset, 0);
    }

    #[tokio::test]
    async fn test_user_message_navigation_cycles_interleaved_events() {
        let mut app = test_app();
        let id = setup_detail_with_interleaved_messages(&mut app);
        let last_user_idx = 51;

        // Rendering assigns the live tail cursor to the newest event. A user
        // pressing [u while that lock is active must land on that newest user
        // before subsequent presses use ordinary strictly-older navigation.
        let state = app.sessions.get_mut(&id).unwrap();
        state.scroll_offset = state.event_offsets[last_user_idx];
        state.current_event_index = Some(last_user_idx);
        state.follow_tail = true;

        dispatch_action(&mut app, Action::Application(LcAction::PrevUserMessage)).await;
        let state = app.sessions.get(&id).unwrap();
        assert_eq!(state.current_event_index, Some(last_user_idx));
        assert_eq!(state.scroll_offset, event_offset(&app, id, last_user_idx));
        assert!(!state.follow_tail);
        assert!(state.follow_tail_hold);

        dispatch_action(&mut app, Action::Application(LcAction::PrevUserMessage)).await;
        let state = app.sessions.get(&id).unwrap();
        assert_eq!(state.current_event_index, Some(48));
        assert_eq!(state.scroll_offset, event_offset(&app, id, 48));

        dispatch_action(&mut app, Action::Application(LcAction::NextUserMessage)).await;
        assert_eq!(
            app.sessions.get(&id).unwrap().current_event_index,
            Some(last_user_idx)
        );
        assert_eq!(
            app.sessions.get(&id).unwrap().scroll_offset,
            event_offset(&app, id, last_user_idx)
        );

        dispatch_action(&mut app, Action::Application(LcAction::NextUserMessage)).await;
        let state = app.sessions.get(&id).unwrap();
        assert_eq!(state.current_event_index, Some(0));
        assert_eq!(state.scroll_offset, event_offset(&app, id, 0));

        dispatch_action(&mut app, Action::Application(LcAction::PrevUserMessage)).await;
        let state = app.sessions.get(&id).unwrap();
        assert_eq!(state.current_event_index, Some(last_user_idx));
        assert_eq!(state.scroll_offset, event_offset(&app, id, last_user_idx));
    }

    #[tokio::test]
    async fn test_continue_session_from_detail_view() {
        use rsi_common::types::SessionStatus;
        let mut app = test_app();

        let id = add_session(&mut app, SessionStatus::Completed);

        // Switch to detail view
        let tab = &mut app.tabs[app.active_tab];
        let focused = tab.focused_pane;
        if let Some(pane) = tab.layout.find_pane_mut(focused) {
            *pane = crate::types::Pane::SessionDetail { session_id: id };
        }

        // Dispatch ContinueSession(None) — should detect detail view context
        dispatch_action(
            &mut app,
            Action::Application(LcAction::ContinueSession(None)),
        )
        .await;

        // Should open continue popup with the correct session
        match &app.overlay {
            crate::types::OverlayState::Prompt { purpose, .. } => {
                assert_eq!(*purpose, crate::types::PromptPurpose::ContinueSession(id));
            }
            _ => panic!("Expected Prompt overlay"),
        }
    }

    #[tokio::test]
    async fn test_toggle_system_events() {
        use rsi_common::types::SessionStatus;
        let mut app = test_app();
        let id = add_session(&mut app, SessionStatus::Running);

        // Switch to detail view
        let tab = &mut app.tabs[app.active_tab];
        let focused = tab.focused_pane;
        if let Some(pane) = tab.layout.find_pane_mut(focused) {
            *pane = crate::types::Pane::SessionDetail { session_id: id };
        }

        // Default: hidden
        assert!(!app.sessions.get(&id).unwrap().show_system_events);

        // Toggle on
        dispatch_action(&mut app, Action::Application(LcAction::ToggleSystemEvents)).await;
        assert!(app.sessions.get(&id).unwrap().show_system_events);

        // Toggle off
        dispatch_action(&mut app, Action::Application(LcAction::ToggleSystemEvents)).await;
        assert!(!app.sessions.get(&id).unwrap().show_system_events);
    }

    #[tokio::test]
    async fn test_toggle_system_events_noop_in_list_view() {
        let mut app = test_app();
        // In list view — toggle should be a silent no-op
        dispatch_action(&mut app, Action::Application(LcAction::ToggleSystemEvents)).await;
        // No panic, no status message change beyond what's expected
    }

    #[tokio::test]
    async fn test_toggle_thinking_events() {
        use rsi_common::types::SessionStatus;
        let mut app = test_app();
        let id = add_session(&mut app, SessionStatus::Running);

        // Switch to detail view
        let tab = &mut app.tabs[app.active_tab];
        let focused = tab.focused_pane;
        if let Some(pane) = tab.layout.find_pane_mut(focused) {
            *pane = crate::types::Pane::SessionDetail { session_id: id };
        }

        // Default: collapsed
        assert!(!app.sessions.get(&id).unwrap().show_thinking_events);

        // Toggle on
        dispatch_action(
            &mut app,
            Action::Application(LcAction::ToggleThinkingEvents),
        )
        .await;
        assert!(app.sessions.get(&id).unwrap().show_thinking_events);

        // Toggle off
        dispatch_action(
            &mut app,
            Action::Application(LcAction::ToggleThinkingEvents),
        )
        .await;
        assert!(!app.sessions.get(&id).unwrap().show_thinking_events);
    }

    #[tokio::test]
    async fn test_continue_session_shows_status_warning_for_running() {
        use rsi_common::types::SessionStatus;
        let mut app = test_app();

        let id = add_session(&mut app, SessionStatus::Running);

        // Switch to detail view
        let tab = &mut app.tabs[app.active_tab];
        let focused = tab.focused_pane;
        if let Some(pane) = tab.layout.find_pane_mut(focused) {
            *pane = crate::types::Pane::SessionDetail { session_id: id };
        }

        // Dispatch ContinueSession(None)
        dispatch_action(
            &mut app,
            Action::Application(LcAction::ContinueSession(None)),
        )
        .await;

        // Running sessions no longer show a warning — daemon handles interrupt-then-continue.
        // Popup should still open normally.
        assert!(matches!(
            app.overlay,
            crate::types::OverlayState::Prompt { .. }
        ));
    }
}
