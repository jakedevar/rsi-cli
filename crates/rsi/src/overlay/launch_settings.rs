//! Settings side ("back") of a new-session prompt and the prompt's own help.
//!
//! A launch prompt keeps its text on the front. `Ctrl+O` (any mode) or `Tab`
//! (normal mode) flips it to a short settings list: model, effort, sandbox
//! and, for blank sessions, a manager appointment with its scope and policy
//! preset. `?` in normal mode opens contextual help for whichever side is
//! showing, so the modal itself carries no key hints.
//!
//! The manager appointment is performed after the daemon accepts the launch,
//! on the same task, through the operator RPCs the `:manager appoint` and
//! `:manager policy` editors use (`ConfigureHarnessManager`, then
//! `ConfigureHarnessManagerPolicy`).

use std::collections::BTreeSet;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rsi_common::harness_manager::{ConfigureHarnessManagerRequestV1, HarnessManagerScopeModeV1};
use rsi_common::harness_manager_presets::{
    ManagerPolicyOrigin, ManagerPolicyPreset, ManagerPresetContext, apply_manager_policy_preset,
};
use rsi_common::harness_manager_v2::{ConfigureHarnessManagerPolicyRequestV2, ManagerPolicyV2};
use rsi_common::types::{Session, SessionKind, SessionStatus};
use uuid::Uuid;

use crate::app::App;
use crate::types::{
    ManagerLaunchPlan, ManagerLaunchScope, OverlayState, PopupMode, PromptLaunchSettings,
    PromptPurpose,
};

/// Policy presets offered at launch, in cycle order. `Custom` policies stay
/// in the full editor (`:manager policy`).
pub(crate) const MANAGER_LAUNCH_PRESETS: [ManagerPolicyPreset; 3] = [
    ManagerPolicyPreset::Observe,
    ManagerPolicyPreset::Execute,
    ManagerPolicyPreset::FullProjectControl,
];

/// Preset a newly planned appointment starts with.
pub(crate) const DEFAULT_MANAGER_LAUNCH_PRESET: ManagerPolicyPreset = ManagerPolicyPreset::Execute;

/// One row of the settings side, top to bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaunchSettingRow {
    Model,
    Effort,
    Sandbox,
    Manager,
    ManagerScope,
    ManagerPolicy,
}

impl LaunchSettingRow {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Model => "Model",
            Self::Effort => "Effort",
            Self::Sandbox => "Sandbox",
            Self::Manager => "Manager",
            Self::ManagerScope => "Scope",
            Self::ManagerPolicy => "Policy",
        }
    }

    /// One-line explanation shown under the list for the selected row.
    pub(crate) const fn description(self) -> &'static str {
        match self {
            Self::Model => "Provider and model for this launch only.",
            Self::Effort => "Reasoning effort for new launches of this model.",
            Self::Sandbox => "Run in its own git worktree on an rsi/<session> branch.",
            Self::Manager => "Appoint this session as the project manager at launch.",
            Self::ManagerScope => "What the manager supervises: project, Group or Epic.",
            Self::ManagerPolicy => "Observe watches; Execute runs work; Full grants all.",
        }
    }

    /// Rows whose value steps through a list (rendered with `‹ ›`).
    pub(crate) const fn cycles(self) -> bool {
        matches!(
            self,
            Self::Effort | Self::ManagerScope | Self::ManagerPolicy
        )
    }
}

/// Which prompt a key is routed to: the regular overlay slot (`app.overlay`)
/// or one entry of the input-overlay stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptSlot {
    Regular,
    Input(usize),
}

impl PromptSlot {
    fn get(self, app: &App) -> Option<&OverlayState> {
        match self {
            Self::Regular => Some(&app.overlay),
            Self::Input(index) => app.input_overlays.get(index),
        }
        .filter(|overlay| matches!(overlay, OverlayState::Prompt { .. }))
    }

    fn get_mut(self, app: &mut App) -> Option<&mut OverlayState> {
        match self {
            Self::Regular => Some(&mut app.overlay),
            Self::Input(index) => app.input_overlays.get_mut(index),
        }
        .filter(|overlay| matches!(overlay, OverlayState::Prompt { .. }))
    }
}

/// Whether a prompt launches a session and therefore has a settings side.
/// Matches the purposes the Ctrl+E / Ctrl+B / Ctrl+M controls apply to.
#[must_use]
pub const fn has_settings_side(purpose: &PromptPurpose) -> bool {
    matches!(purpose, PromptPurpose::Blank | PromptPurpose::TaskRabbit)
}

/// Whether a prompt may appoint its session as manager. `TaskRabbit`
/// sessions are one-shot tasks and never become a project's manager.
#[must_use]
pub const fn offers_manager(purpose: &PromptPurpose) -> bool {
    matches!(purpose, PromptPurpose::Blank)
}

/// The rows the settings side shows for this prompt, top to bottom.
#[must_use]
pub(crate) fn setting_rows(
    purpose: &PromptPurpose,
    launch: &PromptLaunchSettings,
) -> Vec<LaunchSettingRow> {
    if !has_settings_side(purpose) {
        return Vec::new();
    }
    let mut rows = vec![
        LaunchSettingRow::Model,
        LaunchSettingRow::Effort,
        LaunchSettingRow::Sandbox,
    ];
    if offers_manager(purpose) {
        rows.push(LaunchSettingRow::Manager);
        if launch.manager.is_some() {
            rows.push(LaunchSettingRow::ManagerScope);
            rows.push(LaunchSettingRow::ManagerPolicy);
        }
    }
    rows
}

impl ManagerLaunchPlan {
    /// Scope mode of the appointment.
    #[must_use]
    pub(crate) fn scope_mode(&self) -> HarnessManagerScopeModeV1 {
        if self.scope == ManagerLaunchScope::Project {
            HarnessManagerScopeModeV1::Project
        } else {
            HarnessManagerScopeModeV1::Selected
        }
    }

    /// The `ConfigureHarnessManager` request appointing `session_id`, in the
    /// shape the `:manager scope` editor builds: project scope omits Epics,
    /// a Group selection sends an explicit empty Epic list.
    #[must_use]
    pub(crate) fn scope_request(
        &self,
        session_id: Uuid,
        expected_row_version: i64,
    ) -> ConfigureHarnessManagerRequestV1 {
        let (epic_ids, group_ids) = match self.scope {
            ManagerLaunchScope::Project => (None, Vec::new()),
            ManagerLaunchScope::Group(id) => (Some(Vec::new()), vec![id]),
            ManagerLaunchScope::Epic(id) => (Some(vec![id]), Vec::new()),
        };
        ConfigureHarnessManagerRequestV1 {
            project_id: self.project_id,
            session_id,
            epic_ids,
            group_ids,
            expected_row_version,
        }
    }

    /// The policy granted at launch: the preset applied to a fresh default
    /// policy, exactly as the policy editor applies it to a new draft
    /// (suggested allowances included).
    #[must_use]
    pub(crate) fn policy(&self) -> ManagerPolicyV2 {
        let touched = BTreeSet::new();
        apply_manager_policy_preset(
            &ManagerPolicyV2::default(),
            self.preset,
            &ManagerPresetContext {
                origin: ManagerPolicyOrigin::New,
                touched: &touched,
                scope_mode: self.scope_mode(),
            },
        )
        .policy
    }
}

fn live_in_project(session: &Session, project_id: Uuid) -> bool {
    session.project_id == Some(project_id)
        && !matches!(
            session.status,
            SessionStatus::Archived | SessionStatus::Deleted
        )
}

fn display_name(app: &App, session: &Session) -> String {
    crate::types::session_display::resolve_session_display_identity(session, &app.sessions)
        .effective_title
}

/// Scope choices for a manager appointed in `project_id`, in cycle order:
/// the whole project, then each live Group followed by its live Epics (the
/// daemon only accepts Epics that sit under a Group).
#[must_use]
pub(crate) fn manager_scope_choices(app: &App, project_id: Uuid) -> Vec<ManagerLaunchScope> {
    let sorted = |kind: SessionKind, parent: Option<Uuid>| {
        let mut rows: Vec<&Session> = app
            .sessions
            .values()
            .map(|state| &state.session)
            .filter(|session| {
                session.session_kind == kind
                    && live_in_project(session, project_id)
                    && (kind == SessionKind::Group || session.parent_id == parent)
            })
            .collect();
        rows.sort_by_cached_key(|session| (display_name(app, session).to_lowercase(), session.id));
        rows.into_iter()
            .map(|session| session.id)
            .collect::<Vec<_>>()
    };
    let mut choices = vec![ManagerLaunchScope::Project];
    for group in sorted(SessionKind::Group, None) {
        choices.push(ManagerLaunchScope::Group(group));
        choices.extend(
            sorted(SessionKind::Epic, Some(group))
                .into_iter()
                .map(ManagerLaunchScope::Epic),
        );
    }
    choices
}

/// Display text for a scope choice, using the session browser's container
/// glyphs for Groups and Epics.
#[must_use]
pub(crate) fn manager_scope_label(app: &App, scope: ManagerLaunchScope) -> String {
    let named = |glyph: &str, id: Uuid| {
        let name = app
            .sessions
            .get(&id)
            .map_or_else(|| id.to_string(), |state| display_name(app, &state.session));
        format!("{glyph} {name}")
    };
    match scope {
        ManagerLaunchScope::Project => "whole project".to_string(),
        ManagerLaunchScope::Group(id) => named(crate::ui::glyphs::GROUP_CONTAINER, id),
        ManagerLaunchScope::Epic(id) => named(crate::ui::glyphs::EPIC_CONTAINER, id),
    }
}

/// Display name of the manager an appointment in `project_id` would replace.
#[must_use]
pub(crate) fn replaced_manager_name(app: &App, project_id: Uuid) -> Option<String> {
    let entry = app.manager_roster.by_project.get(&project_id)?;
    Some(app.sessions.get(&entry.session_id).map_or_else(
        || entry.session_id.to_string(),
        |state| display_name(app, &state.session),
    ))
}

fn is_help_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('?')
        && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
}

fn is_flip_chord(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('o' | 'O')) && key.modifiers == KeyModifiers::CONTROL
}

/// Chords the settings side leaves to the prompt's regular handlers: launch,
/// launch into a tab or split, close, and the effort / sandbox / model
/// controls. Everything else is a settings key or ignored, so nothing typed
/// on the back side edits the hidden prompt text.
fn passes_through_settings_side(key: KeyEvent) -> bool {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return false;
    }
    match key.code {
        KeyCode::Enter | KeyCode::Char('t' | 's') => true,
        KeyCode::Char('q' | 'b' | 'e' | 'E' | 'm' | 'M') => key.modifiers == KeyModifiers::CONTROL,
        _ => false,
    }
}

/// Handle a prompt's own `?` help, the settings flip, and every key while the
/// settings side shows. Returns `true` when the key was consumed; `false`
/// leaves it to the prompt's regular handlers (submit, close, Ctrl controls,
/// text editing).
pub(crate) fn handle_prompt_launch_keys(app: &mut App, slot: PromptSlot, key: KeyEvent) -> bool {
    let Some(OverlayState::Prompt {
        surface,
        purpose,
        launch,
        ..
    }) = slot.get(app)
    else {
        return false;
    };
    let settings_side = launch.open;
    let flippable = has_settings_side(purpose);
    // A plain key in normal mode is a command only when no vim command is
    // half-typed: `f?` still searches for `?`.
    let idle_normal = surface.mode == PopupMode::Normal && surface.vim_state.is_idle();

    if is_help_key(key) && (settings_side || idle_normal) {
        super::keybindings_help::open_contextual_help(app);
        return true;
    }
    let tab_flip =
        key.code == KeyCode::Tab && key.modifiers.is_empty() && (settings_side || idle_normal);
    if flippable && (is_flip_chord(key) || tab_flip) {
        set_settings_side(app, slot, !settings_side);
        return true;
    }
    if !settings_side {
        return false;
    }
    if passes_through_settings_side(key) {
        return false;
    }
    match key.code {
        KeyCode::Esc => set_settings_side(app, slot, false),
        KeyCode::Char('j') | KeyCode::Down => move_selection(app, slot, Move::Next),
        KeyCode::Char('k') | KeyCode::Up => move_selection(app, slot, Move::Previous),
        KeyCode::Char('g') | KeyCode::Home => move_selection(app, slot, Move::First),
        KeyCode::Char('G') | KeyCode::End => move_selection(app, slot, Move::Last),
        KeyCode::Char('h') | KeyCode::Left => change_selected(app, slot, Step::Back),
        KeyCode::Char('l') | KeyCode::Right => change_selected(app, slot, Step::Forward),
        KeyCode::Char(' ') | KeyCode::Enter => change_selected(app, slot, Step::Activate),
        _ => {}
    }
    app.mark_dirty();
    true
}

/// Show (`open`) or hide the settings side, keeping the selection in range.
/// The prompt keeps its editing mode, so flipping back resumes typing.
pub(crate) fn set_settings_side(app: &mut App, slot: PromptSlot, open: bool) {
    if let Some(OverlayState::Prompt {
        surface,
        purpose,
        launch,
        ..
    }) = slot.get_mut(app)
    {
        if open && !has_settings_side(purpose) {
            return;
        }
        launch.open = open;
        let rows = setting_rows(purpose, launch).len();
        launch.selected = launch.selected.min(rows.saturating_sub(1));
        surface.suggestions_visible = false;
    }
    app.mark_dirty();
}

#[derive(Debug, Clone, Copy)]
enum Move {
    Next,
    Previous,
    First,
    Last,
}

fn move_selection(app: &mut App, slot: PromptSlot, movement: Move) {
    let Some(OverlayState::Prompt {
        purpose, launch, ..
    }) = slot.get_mut(app)
    else {
        return;
    };
    let last = setting_rows(purpose, launch).len().saturating_sub(1);
    launch.selected = match movement {
        Move::Next => (launch.selected + 1).min(last),
        Move::Previous => launch.selected.saturating_sub(1),
        Move::First => 0,
        Move::Last => last,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Back,
    Forward,
    /// Space / Enter: toggle, open, or step forward.
    Activate,
}

fn selected_row(app: &App, slot: PromptSlot) -> Option<LaunchSettingRow> {
    let Some(OverlayState::Prompt {
        purpose, launch, ..
    }) = slot.get(app)
    else {
        return None;
    };
    setting_rows(purpose, launch).get(launch.selected).copied()
}

fn change_selected(app: &mut App, slot: PromptSlot, step: Step) {
    match selected_row(app, slot) {
        Some(LaunchSettingRow::Model) => toggle_model_picker(app, slot),
        Some(LaunchSettingRow::Effort) => step_effort(app, slot, step != Step::Back),
        Some(LaunchSettingRow::Sandbox) => toggle_sandbox(app, slot),
        Some(LaunchSettingRow::Manager) => toggle_manager(app, slot),
        Some(LaunchSettingRow::ManagerScope) => step_scope(app, slot, step != Step::Back),
        Some(LaunchSettingRow::ManagerPolicy) => step_preset(app, slot, step != Step::Back),
        None => {}
    }
}

fn toggle_model_picker(app: &mut App, slot: PromptSlot) {
    let opened = if let Some(OverlayState::Prompt { model_dropdown, .. }) = slot.get_mut(app) {
        model_dropdown.toggle();
        model_dropdown.open
    } else {
        false
    };
    if opened {
        app.needs_model_refresh = true;
    }
}

fn step_effort(app: &mut App, slot: PromptSlot, forward: bool) {
    let (model_override, provider_override) = match slot.get(app) {
        Some(OverlayState::Prompt {
            model_override,
            provider_override,
            ..
        }) => (model_override.clone(), *provider_override),
        _ => return,
    };
    super::step_effort(app, model_override.as_deref(), provider_override, forward);
}

fn toggle_sandbox(app: &mut App, slot: PromptSlot) {
    if !app.poll.sandbox_supported {
        app.notify("Sandbox unavailable: the daemon does not advertise sandbox support");
        return;
    }
    if let Some(OverlayState::Prompt {
        sandbox_enabled, ..
    }) = slot.get_mut(app)
    {
        *sandbox_enabled = !*sandbox_enabled;
    }
}

fn toggle_manager(app: &mut App, slot: PromptSlot) {
    let planned = match slot.get(app) {
        Some(OverlayState::Prompt {
            purpose, launch, ..
        }) if offers_manager(purpose) => launch.manager.is_some(),
        _ => return,
    };
    let project_id = app.current_project_id;
    if !planned && project_id.is_none() {
        app.notify(
            "A manager is appointed for a project: pick one with :projects, then reopen the prompt",
        );
        return;
    }
    if let Some(OverlayState::Prompt {
        purpose, launch, ..
    }) = slot.get_mut(app)
    {
        launch.manager = match (planned, project_id) {
            (false, Some(project_id)) => Some(ManagerLaunchPlan {
                project_id,
                scope: ManagerLaunchScope::Project,
                preset: DEFAULT_MANAGER_LAUNCH_PRESET,
            }),
            _ => None,
        };
        let rows = setting_rows(purpose, launch).len();
        launch.selected = launch.selected.min(rows.saturating_sub(1));
    }
}

const fn wrapped(index: usize, len: usize, forward: bool) -> usize {
    if forward {
        (index + 1) % len
    } else {
        (index + len - 1) % len
    }
}

fn step_scope(app: &mut App, slot: PromptSlot, forward: bool) {
    let Some(plan) = planned_manager(app, slot) else {
        return;
    };
    let choices = manager_scope_choices(app, plan.project_id);
    let current = choices
        .iter()
        .position(|scope| *scope == plan.scope)
        .unwrap_or(0);
    let scope = choices[wrapped(current, choices.len(), forward)];
    set_plan(app, slot, ManagerLaunchPlan { scope, ..plan });
}

fn step_preset(app: &mut App, slot: PromptSlot, forward: bool) {
    let Some(plan) = planned_manager(app, slot) else {
        return;
    };
    let current = MANAGER_LAUNCH_PRESETS
        .iter()
        .position(|preset| *preset == plan.preset)
        .unwrap_or(0);
    let preset = MANAGER_LAUNCH_PRESETS[wrapped(current, MANAGER_LAUNCH_PRESETS.len(), forward)];
    set_plan(app, slot, ManagerLaunchPlan { preset, ..plan });
}

fn planned_manager(app: &App, slot: PromptSlot) -> Option<ManagerLaunchPlan> {
    match slot.get(app) {
        Some(OverlayState::Prompt { launch, .. }) => launch.manager,
        _ => None,
    }
}

fn set_plan(app: &mut App, slot: PromptSlot, plan: ManagerLaunchPlan) {
    if let Some(OverlayState::Prompt { launch, .. }) = slot.get_mut(app) {
        launch.manager = Some(plan);
    }
}

/// Appoint the accepted `session_id` as its project's manager with the
/// planned scope, then grant the planned policy preset. Runs on the launch
/// task right after `LaunchSession` returns, so the operator sees one result.
///
/// # Errors
/// A sentence for the operator naming which step was refused; the session
/// itself is launched either way.
pub(crate) async fn appoint_launched_manager(
    socket_path: PathBuf,
    session_id: Uuid,
    plan: ManagerLaunchPlan,
) -> Result<String, String> {
    let not_appointed = |error: String| {
        format!(
            "Session launched but not appointed manager: {error}. Retry with :manager appoint on it."
        )
    };
    let mut client = crate::client::DaemonClient::new(socket_path);
    client
        .connect()
        .await
        .map_err(|error| not_appointed(error.to_string()))?;
    let previous = client
        .get_harness_manager(plan.project_id)
        .await
        .map_err(|error| not_appointed(error.to_string()))?;
    let replaced = previous.as_ref().is_some_and(|config| {
        !config.is_revoked()
            && config
                .current_session_id
                .unwrap_or(config.manager_session_id)
                != session_id
    });
    let config = client
        .configure_harness_manager(
            plan.scope_request(session_id, previous.as_ref().map_or(0, |c| c.row_version)),
        )
        .await
        .map_err(|error| not_appointed(error.to_string()))?;
    let preset = plan.preset.label();
    let not_granted = |error: String| {
        format!(
            "Manager appointed, but the {preset} policy was not granted: {error}. Set it with :manager policy."
        )
    };
    let stored = client
        .get_harness_manager_policy(plan.project_id)
        .await
        .map_err(|error| not_granted(error.to_string()))?;
    client
        .configure_harness_manager_policy(ConfigureHarnessManagerPolicyRequestV2 {
            project_id: plan.project_id,
            expected_scope_version: config.row_version,
            expected_policy_version: stored.map_or(0, |policy| policy.row_version),
            idempotency_key: Uuid::new_v4().to_string(),
            policy: plan.policy(),
        })
        .await
        .map_err(|error| not_granted(error.to_string()))?;
    let scope = if config.scope_mode == HarnessManagerScopeModeV1::Project {
        "the whole project".to_string()
    } else {
        format!(
            "{} Group(s) + {} Epic(s)",
            config.group_ids.len(),
            config.explicit_epic_ids().len()
        )
    };
    Ok(format!(
        "Manager appointed for {scope} with the {preset} policy{}",
        if replaced {
            "; it replaces the previous manager"
        } else {
            ""
        }
    ))
}

#[cfg(test)]
mod tests;
