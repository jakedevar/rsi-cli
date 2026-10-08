//! Instantiate a manager from the global manager workspace (#1231): one form
//! that picks the seat (the global seat or a project's PM), a launch from the
//! allowed catalog, the scope and the first instruction, then launches a fresh
//! session and appoints it through the existing operator RPCs
//! (`ConfigureGlobalManager`, or `ConfigureHarnessManager` plus its policy).
//!
//! Appointment needs an existing live leaf session, so the flow launches
//! first. A launch that succeeds but whose appointment is refused keeps the
//! launched session id: Enter retries only the appointment.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rsi_common::global_manager::{ConfigureGlobalManagerRequestV1, GlobalManagerGrantV1};
use rsi_common::harness_manager_presets::ManagerPolicyPreset;
use rsi_common::harness_manager_v2::ManagerLaunchChoiceV2;
use rsi_common::types::{Project, SessionKind, SessionProvider};
use uuid::Uuid;

use crate::app::App;
use crate::overlay::global_manager_command::{default_allowed_launches, default_project_policy};
use crate::types::{ManagerLaunchPlan, ManagerLaunchScope};

/// The most projects one global grant may hold (`ConfigureGlobalManager`).
pub const MAX_SCOPE_PROJECTS: usize = 64;

/// Which manager seat the form fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchRole {
    /// The global seat above every project.
    Global,
    /// The project manager (PM) seat of one project.
    Project(Uuid),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchField {
    Role,
    Provider,
    Model,
    Effort,
    Scope,
    /// The project the global seat's session lives in (global role only).
    Host,
    Prompt,
    Submit,
}

impl LaunchField {
    const ORDER: [Self; 8] = [
        Self::Role,
        Self::Provider,
        Self::Model,
        Self::Effort,
        Self::Scope,
        Self::Host,
        Self::Prompt,
        Self::Submit,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Role => "Seat",
            Self::Provider => "Provider",
            Self::Model => "Model",
            Self::Effort => "Effort",
            Self::Scope => "Scope",
            Self::Host => "Host",
            Self::Prompt => "Prompt",
            Self::Submit => "",
        }
    }
}

/// One project in the global scope checklist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeEntry {
    pub project_id: Uuid,
    pub name: String,
    pub checked: bool,
}

/// A validated request, ready to launch and appoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    pub role: LaunchRole,
    pub launch: ManagerLaunchChoiceV2,
    /// The launched session's project.
    pub home_project: Uuid,
    /// The global grant's projects (empty for a PM).
    pub scope: Vec<Uuid>,
    pub prompt: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct LaunchForm {
    pub role: LaunchRole,
    /// Seats the form can fill: the global seat, then each project's PM.
    pub roles: Vec<(LaunchRole, String)>,
    /// The allowed launches: the grant's catalog, then the operator default.
    pub catalog: Vec<ManagerLaunchChoiceV2>,
    pub launch: ManagerLaunchChoiceV2,
    pub scope: Vec<ScopeEntry>,
    pub scope_cursor: usize,
    /// #1627: the operator's explicit host project for the global seat; `None`
    /// takes the default (`default_host`). Always one of the checked scope.
    pub host: Option<Uuid>,
    pub prompt: String,
    /// The operator edited the prompt: stop regenerating it.
    pub prompt_edited: bool,
    pub field: LaunchField,
    /// Inline validation or RPC error.
    pub error: Option<String>,
    /// Launched but not yet appointed: Enter retries only the appointment.
    pub launched: Option<Uuid>,
    /// #1544: the daemon refused the appointment because it lowers project
    /// caps and `error` holds the preview: `y` resends with
    /// `confirm_cap_reductions:true`; Enter does not retry the refused request.
    pub cap_confirm: bool,
    /// The tab's project when the form opened: the global seat's home when
    /// it is in scope.
    pub active_project: Option<Uuid>,
}

fn dedup_catalog(
    choices: impl IntoIterator<Item = ManagerLaunchChoiceV2>,
) -> Vec<ManagerLaunchChoiceV2> {
    let mut out: Vec<ManagerLaunchChoiceV2> = Vec::new();
    for choice in choices {
        if !out.contains(&choice) {
            out.push(choice);
        }
    }
    out
}

fn provider_name(provider: SessionProvider) -> String {
    format!("{provider:?}")
}

/// Step `current` through `values` by `delta`, wrapping.
fn step<T: PartialEq + Clone>(values: &[T], current: &T, delta: isize) -> Option<T> {
    if values.is_empty() {
        return None;
    }
    let len = values.len() as isize;
    let index = values
        .iter()
        .position(|value| value == current)
        .unwrap_or(0) as isize;
    Some(values[((index + delta).rem_euclid(len)) as usize].clone())
}

impl LaunchForm {
    /// A form for `role`. The catalog is the grant's allowed launches plus
    /// the operator's default manager launch; the scope defaults to the
    /// grant's projects, else every project.
    #[must_use]
    pub fn new(
        role: LaunchRole,
        projects: &[Project],
        grant: Option<&GlobalManagerGrantV1>,
        active_project: Option<Uuid>,
    ) -> Self {
        let catalog = dedup_catalog(
            grant
                .map(|grant| grant.allowed_launches.clone())
                .unwrap_or_default()
                .into_iter()
                .chain(default_allowed_launches()),
        );
        let granted: Vec<Uuid> = grant
            .filter(|grant| grant.state == "active")
            .map(|grant| grant.project_ids.clone())
            .unwrap_or_default();
        let scope = projects
            .iter()
            .map(|project| ScopeEntry {
                project_id: project.id,
                name: project.name.clone(),
                checked: granted.is_empty() || granted.contains(&project.id),
            })
            .collect();
        let roles = std::iter::once((LaunchRole::Global, "Global manager".to_string()))
            .chain(projects.iter().map(|project| {
                (
                    LaunchRole::Project(project.id),
                    format!("Project manager · {}", project.name),
                )
            }))
            .collect();
        let launch = catalog[0].clone();
        let mut form = Self {
            role,
            roles,
            catalog,
            launch,
            scope,
            scope_cursor: 0,
            host: None,
            prompt: String::new(),
            prompt_edited: false,
            field: LaunchField::Role,
            error: None,
            launched: None,
            cap_confirm: false,
            active_project,
        };
        form.regenerate_prompt();
        form
    }

    /// The fields shown for the current role (scope is global-only).
    #[must_use]
    pub fn fields(&self) -> Vec<LaunchField> {
        LaunchField::ORDER
            .into_iter()
            .filter(|field| {
                !matches!(field, LaunchField::Scope | LaunchField::Host)
                    || self.role == LaunchRole::Global
            })
            .collect()
    }

    #[must_use]
    pub fn role_label(&self) -> String {
        self.roles
            .iter()
            .find(|(role, _)| *role == self.role)
            .map_or_else(
                || "Project manager · (unknown project)".into(),
                |(_, label)| label.clone(),
            )
    }

    fn project_name(&self, id: Uuid) -> Option<&str> {
        self.scope
            .iter()
            .find(|entry| entry.project_id == id)
            .map(|entry| entry.name.as_str())
    }

    fn checked(&self) -> Vec<&ScopeEntry> {
        self.scope.iter().filter(|entry| entry.checked).collect()
    }

    /// #1627: the host project when the operator does not pick one: the
    /// tab's project if it is in scope, else the first project in scope.
    #[must_use]
    pub fn default_host(&self) -> Option<Uuid> {
        let checked = self.checked();
        self.active_project
            .filter(|id| checked.iter().any(|e| e.project_id == *id))
            .or_else(|| checked.first().map(|e| e.project_id))
    }

    /// The host the global seat will launch in: the explicit choice, else
    /// the default.
    #[must_use]
    pub fn effective_host(&self) -> Option<Uuid> {
        self.host
            .filter(|id| self.scope.iter().any(|e| e.checked && e.project_id == *id))
            .or_else(|| self.default_host())
    }

    /// Drop an explicit host that left the scope.
    fn drop_stale_host(&mut self) {
        if self.host != self.effective_host() && self.host.is_some() {
            self.host = None;
        }
    }

    /// The default first instruction for the current role and scope.
    #[must_use]
    pub fn default_prompt(&self) -> String {
        match self.role {
            LaunchRole::Global => {
                let names: Vec<&str> = self.checked().iter().map(|e| e.name.as_str()).collect();
                format!(
                    "You are the operator-appointed global manager for: {}. Load the rsi-portfolio-manager skill, call AgentGetAuthorityCatalog to confirm your seat, then report a short portfolio digest.",
                    names.join(", ")
                )
            }
            LaunchRole::Project(id) => format!(
                "You are the operator-appointed project manager for {}. Load the rsi-project-manager skill, call AgentGetAuthorityCatalog to verify your seat, then start the integrator loop.",
                self.project_name(id).unwrap_or("this project")
            ),
        }
    }

    fn regenerate_prompt(&mut self) {
        if !self.prompt_edited {
            self.prompt = self.default_prompt();
        }
    }

    fn providers(&self) -> Vec<SessionProvider> {
        let mut out = Vec::new();
        for choice in &self.catalog {
            if !out.contains(&choice.provider) {
                out.push(choice.provider);
            }
        }
        out
    }

    fn models(&self) -> Vec<String> {
        let mut out = Vec::new();
        for choice in self
            .catalog
            .iter()
            .filter(|c| c.provider == self.launch.provider)
        {
            if !out.contains(&choice.model) {
                out.push(choice.model.clone());
            }
        }
        out
    }

    fn efforts(&self) -> Vec<Option<String>> {
        self.catalog
            .iter()
            .filter(|c| c.provider == self.launch.provider && c.model == self.launch.model)
            .map(|c| c.effort.clone())
            .collect()
    }

    /// Snap the launch to the first catalog entry matching the fields above
    /// the one just changed.
    fn snap_launch(&mut self, keep_model: bool) {
        if self.catalog.contains(&self.launch) {
            return;
        }
        let provider = self.launch.provider;
        let model = self.launch.model.clone();
        self.launch = self
            .catalog
            .iter()
            .find(|c| c.provider == provider && (!keep_model || c.model == model))
            .or_else(|| self.catalog.iter().find(|c| c.provider == provider))
            .unwrap_or(&self.catalog[0])
            .clone();
    }

    /// The display value of `field`.
    #[must_use]
    pub fn value(&self, field: LaunchField) -> String {
        match field {
            LaunchField::Role => self.role_label(),
            LaunchField::Provider => provider_name(self.launch.provider),
            LaunchField::Model => self.launch.model.clone(),
            LaunchField::Effort => self
                .launch
                .effort
                .clone()
                .unwrap_or_else(|| "default".into()),
            LaunchField::Scope => {
                let checked = self.checked().len();
                format!("{checked} of {} projects", self.scope.len())
            }
            LaunchField::Host => match self.effective_host() {
                Some(id) => {
                    let name = self.project_name(id).unwrap_or("project");
                    if self.host.is_some() {
                        name.to_string()
                    } else {
                        format!("{name} (default)")
                    }
                }
                None => "(no project in scope)".into(),
            },
            LaunchField::Prompt => self.prompt.clone(),
            LaunchField::Submit => if self.launched.is_some() {
                "[ Retry the appointment ]"
            } else {
                "[ Launch and appoint ]"
            }
            .into(),
        }
    }

    /// Cycle the focused field's value (`h`/`l`, Left/Right).
    pub fn cycle(&mut self, delta: isize) {
        match self.field {
            LaunchField::Role => {
                if self.launched.is_some() {
                    self.error = Some(
                        "The session is already launched for this seat; Esc to start over".into(),
                    );
                    return;
                }
                let roles: Vec<LaunchRole> = self.roles.iter().map(|(role, _)| *role).collect();
                if let Some(role) = step(&roles, &self.role, delta) {
                    self.role = role;
                    self.regenerate_prompt();
                }
            }
            LaunchField::Provider => {
                if let Some(provider) = step(&self.providers(), &self.launch.provider, delta) {
                    self.launch.provider = provider;
                    self.snap_launch(false);
                }
            }
            LaunchField::Model => {
                if let Some(model) = step(&self.models(), &self.launch.model, delta) {
                    self.launch.model = model;
                    self.snap_launch(true);
                }
            }
            LaunchField::Effort => {
                if let Some(effort) = step(&self.efforts(), &self.launch.effort, delta) {
                    self.launch.effort = effort;
                }
            }
            LaunchField::Scope => {
                let len = self.scope.len();
                if len > 0 {
                    self.scope_cursor =
                        ((self.scope_cursor as isize + delta).rem_euclid(len as isize)) as usize;
                }
            }
            LaunchField::Host => {
                let ids: Vec<Uuid> = self.checked().iter().map(|e| e.project_id).collect();
                if let Some(current) = self.effective_host() {
                    if let Some(next) = step(&ids, &current, delta) {
                        self.host = Some(next);
                    }
                }
            }
            LaunchField::Prompt | LaunchField::Submit => {}
        }
    }

    fn move_field(&mut self, delta: isize) {
        let fields = self.fields();
        if let Some(field) = step(&fields, &self.field, delta) {
            self.field = field;
        }
    }

    fn toggle_scope(&mut self) {
        if let Some(entry) = self.scope.get_mut(self.scope_cursor) {
            entry.checked = !entry.checked;
            self.drop_stale_host();
            self.regenerate_prompt();
        }
    }

    fn toggle_all_scope(&mut self) {
        let all = self.scope.iter().all(|entry| entry.checked);
        for entry in &mut self.scope {
            entry.checked = !all;
        }
        self.drop_stale_host();
        self.regenerate_prompt();
    }

    /// Validate the form; the error is shown inline.
    ///
    /// # Errors
    /// One actionable sentence naming the field to fix.
    pub fn validate(&self, projects: &[Project]) -> Result<LaunchRequest, String> {
        if !self.catalog.contains(&self.launch) {
            return Err("Pick a launch from the allowed catalog (Provider/Model/Effort)".into());
        }
        let prompt = self.prompt.trim().to_string();
        if prompt.is_empty() {
            return Err("Write the manager's first instruction in Prompt".into());
        }
        let exists = |id: Uuid| projects.iter().any(|project| project.id == id);
        match self.role {
            LaunchRole::Global => {
                let scope: Vec<Uuid> = self.checked().iter().map(|e| e.project_id).collect();
                if scope.is_empty() {
                    return Err(
                        "Choose at least one project in Scope (Space toggles, a toggles all)"
                            .into(),
                    );
                }
                if scope.len() > MAX_SCOPE_PROJECTS {
                    return Err(format!(
                        "A global grant holds at most {MAX_SCOPE_PROJECTS} projects; uncheck {}",
                        scope.len() - MAX_SCOPE_PROJECTS
                    ));
                }
                if let Some(gone) = scope.iter().find(|id| !exists(**id)) {
                    return Err(format!(
                        "Project {} no longer exists; uncheck it or press r",
                        &gone.to_string()[..8]
                    ));
                }
                let home = self.effective_host().unwrap_or(scope[0]);
                Ok(LaunchRequest {
                    role: self.role,
                    launch: self.launch.clone(),
                    home_project: home,
                    scope,
                    prompt,
                    title: "Global manager".into(),
                })
            }
            LaunchRole::Project(id) => {
                if !exists(id) {
                    return Err("That project no longer exists; pick another Seat".into());
                }
                Ok(LaunchRequest {
                    role: self.role,
                    launch: self.launch.clone(),
                    home_project: id,
                    scope: Vec::new(),
                    prompt,
                    title: format!(
                        "Project manager · {}",
                        self.project_name(id).unwrap_or("project")
                    ),
                })
            }
        }
    }
}

/// What a form key asks the workspace to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormOutcome {
    Stay,
    Cancel,
    Submit,
    /// `y` while a cap-reduction preview is shown (#1544).
    ConfirmCaps,
}

/// Edit the form. Tab/Shift-Tab and Up/Down move between fields; h/l and
/// Left/Right cycle a value; Space toggles a scope project; Enter submits.
/// On Prompt, printable keys type and Backspace deletes.
pub fn handle_form_key(form: &mut LaunchForm, key: KeyEvent) -> FormOutcome {
    let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
    if form.cap_confirm {
        // The preview is a modal step: Enter would only repeat the refused
        // request, so only y (confirm), n (back to the form) and Esc act.
        return match key.code {
            KeyCode::Char('y') if plain => FormOutcome::ConfirmCaps,
            KeyCode::Char('n') if plain => {
                form.cap_confirm = false;
                form.error = None;
                FormOutcome::Stay
            }
            KeyCode::Esc => FormOutcome::Cancel,
            _ => FormOutcome::Stay,
        };
    }
    match key.code {
        KeyCode::Esc => return FormOutcome::Cancel,
        KeyCode::Enter => return FormOutcome::Submit,
        KeyCode::Tab | KeyCode::Down => form.move_field(1),
        KeyCode::BackTab | KeyCode::Up => form.move_field(-1),
        KeyCode::Left => form.cycle(-1),
        KeyCode::Right => form.cycle(1),
        KeyCode::Backspace if form.field == LaunchField::Prompt => {
            form.prompt.pop();
            form.prompt_edited = true;
        }
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
            if form.field == LaunchField::Prompt {
                form.prompt.clear();
                form.prompt_edited = true;
            }
        }
        KeyCode::Char(ch) if plain && form.field == LaunchField::Prompt => {
            form.prompt.push(ch);
            form.prompt_edited = true;
        }
        KeyCode::Char('h') if plain => form.cycle(-1),
        KeyCode::Char('l') if plain => form.cycle(1),
        KeyCode::Char('j') if plain => form.move_field(1),
        KeyCode::Char('k') if plain => form.move_field(-1),
        KeyCode::Char(' ') if form.field == LaunchField::Scope => form.toggle_scope(),
        KeyCode::Char('a') if form.field == LaunchField::Scope => form.toggle_all_scope(),
        _ => return FormOutcome::Stay,
    }
    if !matches!(key.code, KeyCode::Enter | KeyCode::Esc) && form.launched.is_none() {
        form.error = None;
    }
    FormOutcome::Stay
}

/// Launch the session (unless an earlier attempt already did) and appoint it.
///
/// # Errors
/// An operator sentence naming the refused step. When the launch succeeded,
/// `launched` holds the session so a retry only appoints.
pub async fn launch_and_appoint(
    app: &mut App,
    request: &LaunchRequest,
    launched: &mut Option<Uuid>,
    confirm_cap_reductions: bool,
) -> Result<(Uuid, String), String> {
    let session_id = match *launched {
        Some(id) => id,
        None => {
            let id = app
                .client
                .launch_session_with_opts_response(
                    &request.prompt,
                    Some(&request.title),
                    None,
                    request.launch.provider,
                    Some(&request.launch.model),
                    None,
                    Some(SessionKind::Standard),
                    Some(request.home_project),
                    None,
                    request.launch.effort.as_deref(),
                    None,
                    None,
                    &[crate::types::PHASE1_PLACEHOLDER_TAG.to_string()],
                    None,
                    None,
                )
                .await
                .map_err(|error| format!("Launch refused: {error}"))?;
            *launched = Some(id);
            id
        }
    };
    let short = &session_id.to_string()[..8];
    let not_appointed = |error: String| {
        if error
            .contains(rsi_common::portfolio_nodes::PORTFOLIO_CAP_REDUCTION_CONFIRMATION_REQUIRED)
        {
            let preview = error
                .split_once(". Confirm with")
                .map_or(error.as_str(), |(preview, _)| preview);
            return format!(
                "Launched session {short} but the appointment lowers project caps: {preview}. Press y to confirm the lower caps; n returns to the form; Esc keeps the session unappointed."
            );
        }
        format!(
            "Launched session {short} but the appointment was refused: {error}. Enter retries the appointment; Esc keeps the session unappointed."
        )
    };
    match request.role {
        LaunchRole::Global => {
            let current = app
                .client
                .get_global_manager()
                .await
                .map_err(|error| not_appointed(error.to_string()))?;
            let active = current.filter(|grant| grant.state == "active");
            let replaced = active
                .as_ref()
                .is_some_and(|grant| grant.seat_session_id != session_id);
            let grant = app
                .client
                .configure_global_manager_confirmed(
                    ConfigureGlobalManagerRequestV1 {
                        session_id,
                        project_ids: request.scope.clone(),
                        allowed_launches: active
                            .as_ref()
                            .map_or_else(default_allowed_launches, |g| g.allowed_launches.clone()),
                        project_policy: active
                            .as_ref()
                            .map_or_else(default_project_policy, |g| g.project_policy.clone()),
                        expected_grant_version: active.as_ref().map_or(0, |g| g.grant_version),
                        idempotency_key: Uuid::new_v4().to_string(),
                    },
                    confirm_cap_reductions,
                )
                .await
                .map_err(|error| not_appointed(error.to_string()))?;
            Ok((
                session_id,
                format!(
                    "Global manager {short} appointed (grant v{}, {} project{}){}",
                    grant.grant_version,
                    grant.project_ids.len(),
                    if grant.project_ids.len() == 1 {
                        ""
                    } else {
                        "s"
                    },
                    if replaced {
                        "; it replaces the previous seat"
                    } else {
                        ""
                    }
                ),
            ))
        }
        LaunchRole::Project(project_id) => {
            let plan = ManagerLaunchPlan {
                project_id,
                scope: ManagerLaunchScope::Project,
                preset: ManagerPolicyPreset::Execute,
            };
            let socket = app.client.socket_path().to_path_buf();
            let message =
                crate::overlay::launch_settings::appoint_launched_manager(socket, session_id, plan)
                    .await
                    .map_err(|error| format!("Launched session {short}. {error}"))?;
            Ok((session_id, format!("Project manager {short}: {message}")))
        }
    }
}

#[cfg(test)]
#[path = "global_manager_workspace_launch_tests.rs"]
mod tests;
