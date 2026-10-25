//! One operator draft for global coverage, permissions, launches and limits.
//! Saving stays in the workspace's existing, version-fenced operator RPC path.
use crossterm::event::{KeyCode, KeyEvent};
use rsi_common::global_manager::ConfigureGlobalManagerRequestV1;
use rsi_common::harness_manager_presets::ManagerPolicyPreset;
use rsi_common::harness_manager_v2::{ManagerCapabilityV2, ManagerLaunchChoiceV2};
use rsi_common::types::Project;
use uuid::Uuid;

use super::global_manager_workspace_launch::{LaunchRequest, ScopeEntry};
use super::grant_editor::{GrantEditor, GrantRow};
use super::manager_v2::policy::CAPABILITIES;

pub const STEPS: [&str; 5] = ["Projects", "Permissions", "Launches", "Limits", "Review"];

pub fn permission_label(capability: ManagerCapabilityV2) -> &'static str {
    use ManagerCapabilityV2::*;
    match capability {
        WorkPlan => "Plan work and record reviews",
        LeadControl => "Manage Epic leads",
        Topology => "Create and edit Groups/Epics",
        SessionCreate => "Launch workers",
        LeadAssign => "Assign Epic leads",
        Integration => "Record integration evidence",
        SelfSuccession => "Hand off to a successor",
        GitEffect => "Advance integration refs through merge engine",
        SessionControl => "Halt, continue and message scoped sessions",
        IssueCoordinate => "Coordinate project Issues",
        OperatorDelegation => "Use allowlisted operator actions",
        Automation => "Run durable workflows",
        StorageControl => "Inspect/reclaim build storage within limits",
        DaemonSettings => "Adjust explicitly bounded daemon settings",
        Deploy => "Request daemon deployment",
    }
}

/// Named exact tuples; saved choices outside these remain individually editable.
fn named_launches() -> Vec<(&'static str, ManagerLaunchChoiceV2)> {
    let defaults = super::global_manager_command::default_allowed_launches();
    [
        ("Claude manager", 0),
        ("Claude worker", 2),
        ("Deep reasoning", 3),
        ("Coding", 6),
        ("Fast tasks", 8),
    ]
    .into_iter()
    .map(|(label, index)| (label, defaults[index].clone()))
    .collect()
}

#[derive(Debug, Clone)]
pub struct CoverageForm {
    pub original: ConfigureGlobalManagerRequestV1,
    pub editor: GrantEditor,
    pub projects: Vec<ScopeEntry>,
    pub launches: Vec<(String, ManagerLaunchChoiceV2)>,
    pub step: usize,
    pub selected: usize,
    pub query: String,
    pub searching: bool,
    pub error: Option<String>,
    pub note: Option<String>,
    pub cap_confirm: bool,
    /// Retained on transport retries. Editing invalidates the prepared request.
    pub prepared: Option<ConfigureGlobalManagerRequestV1>,
    /// Present only when this draft is the final step of a fresh launch.
    pub launch: Option<LaunchRequest>,
    pub launched: Option<Uuid>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Stay,
    Cancel,
    Budget,
    Save,
}

impl CoverageForm {
    pub fn new(request: ConfigureGlobalManagerRequestV1, projects: &[Project]) -> Self {
        let mut entries: Vec<_> = projects
            .iter()
            .map(|project| ScopeEntry {
                project_id: project.id,
                name: if projects
                    .iter()
                    .filter(|other| other.name == project.name)
                    .count()
                    > 1
                {
                    format!(
                        "{} ({})",
                        project.name,
                        project.path.as_deref().map_or_else(
                            || "no path".to_string(),
                            |path| path.display().to_string(),
                        )
                    )
                } else {
                    project.name.clone()
                },
                checked: request.project_ids.contains(&project.id),
            })
            .collect();
        // A project disappearing must never silently shrink saved coverage.
        for id in &request.project_ids {
            if !entries.iter().any(|entry| entry.project_id == *id) {
                entries.push(ScopeEntry {
                    project_id: *id,
                    name: "Unavailable project (saved coverage)".into(),
                    checked: true,
                });
            }
        }
        let mut launches: Vec<_> = named_launches()
            .into_iter()
            .map(|(label, launch)| (label.to_string(), launch))
            .collect();
        for launch in &request.allowed_launches {
            if !launches.iter().any(|(_, choice)| choice == launch) {
                launches.push(("Custom saved choice".into(), launch.clone()));
            }
        }
        let mut editor = GrantEditor::for_global(
            request.project_policy.clone(),
            request.allowed_launches.clone(),
            launches.iter().map(|(_, launch)| launch.clone()).collect(),
        );
        editor.set_preset_origin(rsi_common::harness_manager_presets::ManagerPolicyOrigin::Saved);
        Self {
            editor,
            original: request,
            projects: entries,
            launches,
            step: 0,
            selected: 0,
            query: String::new(),
            searching: false,
            error: None,
            note: None,
            cap_confirm: false,
            prepared: None,
            launch: None,
            launched: None,
        }
    }

    pub fn new_defaults(&mut self) {
        self.editor
            .set_preset_origin(rsi_common::harness_manager_presets::ManagerPolicyOrigin::New);
        self.editor
            .apply_preset(ManagerPolicyPreset::FullProjectControl);
        self.editor
            .set_preset_origin(rsi_common::harness_manager_presets::ManagerPolicyOrigin::Saved);
    }

    pub fn visible_projects(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        self.projects
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.name.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect()
    }

    fn permission_rows() -> Vec<GrantRow> {
        [GrantRow::Preset, GrantRow::Mode]
            .into_iter()
            .chain((0..CAPABILITIES.len()).map(GrantRow::Capability))
            .collect()
    }

    fn limit_rows() -> [GrantRow; 4] {
        [
            GrantRow::Active,
            GrantRow::Sessions,
            GrantRow::Containers,
            GrantRow::Spend,
        ]
    }

    pub fn rows(&self) -> Vec<String> {
        match self.step {
            0 => self
                .visible_projects()
                .iter()
                .map(|index| {
                    let entry = &self.projects[*index];
                    let was = self.original.project_ids.contains(&entry.project_id);
                    format!(
                        "[{}] {}{}",
                        if entry.checked { 'x' } else { ' ' },
                        entry.name,
                        if was == entry.checked {
                            ""
                        } else if entry.checked {
                            "  + added"
                        } else {
                            "  - removed"
                        }
                    )
                })
                .collect(),
            1 => {
                let mut rows: Vec<_> = Self::permission_rows()
                    .into_iter()
                    .map(|row| {
                        if let GrantRow::Capability(index) = row {
                            format!(
                                "{} {}",
                                self.editor.value(row),
                                permission_label(CAPABILITIES[index])
                            )
                        } else {
                            format!("{}: {}", self.editor.label(row), self.editor.value(row))
                        }
                    })
                    .collect();
                rows.insert(
                    2,
                    format!(
                        "[{}] Create project groups",
                        if self.editor.policy().allow_create_groups {
                            'x'
                        } else {
                            ' '
                        }
                    ),
                );
                rows
            }
            2 => self
                .launches
                .iter()
                .map(|(label, launch)| {
                    format!(
                        "[{}] {label}: {}",
                        if self.editor.launches().contains(launch) {
                            'x'
                        } else {
                            ' '
                        },
                        launch.display()
                    )
                })
                .collect(),
            3 => {
                let mut rows: Vec<_> = Self::limit_rows()
                    .iter()
                    .map(|row| format!("{}: {}", self.editor.label(*row), self.editor.value(*row)))
                    .collect();
                rows.push(format!(
                    "Creation window: {} hours",
                    self.editor.policy().created_sessions_window_hours
                ));
                rows.push(format!(
                    "Recovery attempts: {}",
                    self.editor.policy().max_recovery_attempts
                ));
                rows
            }
            _ => self.review(),
        }
    }

    pub fn review(&self) -> Vec<String> {
        let policy = self.editor.policy();
        let mut lines = vec![format!(
            "Covers: {}",
            self.projects
                .iter()
                .filter(|entry| entry.checked)
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )];
        for entry in &self.projects {
            let was = self.original.project_ids.contains(&entry.project_id);
            if was != entry.checked {
                lines.push(format!(
                    "{} {}",
                    if entry.checked { "+ Cover" } else { "- Remove" },
                    entry.name
                ));
            }
        }
        lines.extend([
            format!(
                "Project managers: {} / {:?}",
                self.editor.preset().label(),
                policy.mode
            ),
            format!(
                "Create project groups: {}",
                if policy.allow_create_groups {
                    "allowed"
                } else {
                    "off"
                }
            ),
            format!(
                "Draft limits: {} active; {} sessions/{}h; {} containers; spend {}",
                policy.max_active_sessions,
                policy.max_created_sessions,
                policy.created_sessions_window_hours,
                policy.max_created_containers,
                self.editor.value(GrantRow::Spend)
            ),
            format!("Recovery attempts: {}", policy.max_recovery_attempts),
            "Permissions (each remains subject to its scope):".into(),
        ]);
        for capability in &policy.capabilities {
            lines.push(format!("  {}", permission_label(*capability)));
        }
        for capability in CAPABILITIES {
            let before = self
                .original
                .project_policy
                .capabilities
                .contains(&capability);
            let after = policy.capabilities.contains(&capability);
            if before != after {
                lines.push(format!(
                    "{} {}",
                    if after {
                        "+ Allow"
                    } else {
                        "- Remove permission:"
                    },
                    permission_label(capability)
                ));
            }
        }
        lines.push(format!(
            "Group permission: {} -> {}",
            self.original.project_policy.allow_create_groups, policy.allow_create_groups
        ));
        lines.extend(self.editor.warnings());
        lines.push("Exact delegated launches:".into());
        for launch in self.editor.launches() {
            lines.push(format!("  {}", launch.display()));
        }
        if !policy.allowed_launches.is_empty() {
            lines.push("Additional saved policy launch restriction (kept):".into());
            for launch in &policy.allowed_launches {
                lines.push(format!("  {}", launch.display()));
            }
        }
        lines.push(
            "Existing PM policies are not rewritten; coverage enters this ancestor ceiling.".into(),
        );
        if let Some(launch) = &self.launch {
            lines.push(format!("Initial launch: {}", launch.launch.display()));
            lines.push(format!(
                "Host: {}",
                self.projects
                    .iter()
                    .find(|entry| entry.project_id == launch.home_project)
                    .map_or("unavailable", |entry| entry.name.as_str())
            ));
            lines.push(format!("First instruction: {}", launch.prompt));
        } else {
            lines.push(format!(
                "Same manager seat; expected grant v{}.",
                self.original.expected_grant_version
            ));
        }
        lines.push("Advanced policy fields are retained (pauses, selectors, provider caps, timeouts, setting bounds).".into());
        if !policy.allow_create_groups {
            lines.push("Without a usable Group, groups OFF may prevent bootstrapping work.".into());
        }
        if let Some(note) = &self.note {
            lines.push(note.clone());
        }
        if let Some(error) = &self.error {
            lines.push(error.clone());
        }
        lines
    }

    pub fn request(&self) -> Result<ConfigureGlobalManagerRequestV1, String> {
        let mut request = self.original.clone();
        request.project_ids.retain(|id| {
            self.projects
                .iter()
                .any(|entry| entry.project_id == *id && entry.checked)
        });
        for entry in self.projects.iter().filter(|entry| entry.checked) {
            if !request.project_ids.contains(&entry.project_id) {
                request.project_ids.push(entry.project_id);
            }
        }
        if request.project_ids.is_empty() || request.project_ids.len() > 64 {
            return Err("Select 1-64 projects in Projects.".into());
        }
        self.editor.validate().map_err(|(_, error)| error)?;
        request.allowed_launches = self.editor.launches().to_vec();
        request.project_policy = self.editor.finish();
        Ok(request)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        if self.searching {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.query.pop();
                    self.selected = 0;
                }
                KeyCode::Char(ch) if self.query.chars().count() < 128 => {
                    self.query.push(ch);
                    self.selected = 0;
                }
                _ => {}
            }
            return Outcome::Stay;
        }
        match key.code {
            KeyCode::Esc if self.step == 4 => {
                self.step = 0;
                self.selected = 0;
            }
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Char('b') => return Outcome::Budget,
            KeyCode::Char('y') if self.step == 4 => return Outcome::Save,
            KeyCode::Enter | KeyCode::Tab => {
                if self.step < 4 {
                    self.step += 1;
                    self.selected = 0;
                }
                if self.step == 4 {
                    match self.request() {
                        Ok(mut request) => {
                            if self.prepared.is_none() {
                                request.idempotency_key = Uuid::new_v4().to_string();
                                self.prepared = Some(request);
                            }
                        }
                        Err(error) => {
                            self.error = Some(error);
                            self.step = 0;
                        }
                    }
                }
            }
            KeyCode::BackTab => {
                self.step = self.step.saturating_sub(1);
                self.selected = 0;
            }
            KeyCode::Char('r') if self.step < 4 => {
                self.step = 3;
                return self.handle_key(KeyEvent::from(KeyCode::Tab));
            }
            KeyCode::Char('/') if self.step == 0 => {
                self.searching = true;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.rows().len().saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('g') => self.selected = 0,
            KeyCode::Char('G') => self.selected = self.rows().len().saturating_sub(1),
            _ if self.step < 4 => {
                self.edit(key);
            }
            _ => {}
        }
        Outcome::Stay
    }

    fn edit(&mut self, key: KeyEvent) {
        let toggles = matches!(
            key.code,
            KeyCode::Char(' ' | 'h' | 'l') | KeyCode::Left | KeyCode::Right
        );
        match self.step {
            0 => {
                let visible = self.visible_projects();
                if key.code == KeyCode::Char('a') {
                    let all = visible.iter().all(|index| self.projects[*index].checked);
                    for index in visible {
                        self.projects[index].checked = !all;
                    }
                } else if toggles && let Some(index) = visible.get(self.selected) {
                    self.projects[*index].checked = !self.projects[*index].checked;
                }
            }
            1 if self.selected == 2 && toggles => {
                let policy = self.editor.policy_mut();
                if !policy.allow_create_groups
                    && !policy.capabilities.contains(&ManagerCapabilityV2::Topology)
                {
                    self.error = Some("Create groups needs Create and edit Groups/Epics. Enable that permission first.".into());
                    return;
                }
                policy.allow_create_groups = !policy.allow_create_groups;
            }
            1 => {
                let index = self.selected - usize::from(self.selected > 2);
                if let Some(row) = Self::permission_rows().get(index) {
                    // Keep the dependency visible rather than silently changing groups.
                    if matches!(row, GrantRow::Capability(index) if CAPABILITIES[*index] == ManagerCapabilityV2::Topology)
                        && self.editor.policy().allow_create_groups
                        && toggles
                    {
                        self.error = Some("Turn Create project groups off before removing Create and edit Groups/Epics.".into());
                        return;
                    }
                    if self.editor.selected_row() != *row {
                        self.editor.select_row(*row);
                    }
                    self.editor.handle_key(key);
                }
            }
            2 if toggles => {
                if let Some((_, launch)) = self.launches.get(self.selected) {
                    self.editor.toggle_allowed_launch(launch.clone());
                }
            }
            3 if self.selected < 4 => {
                let row = Self::limit_rows()[self.selected];
                if self.editor.selected_row() != row {
                    self.editor.select_row(row);
                }
                self.editor.handle_key(key);
            }
            3 if toggles => {
                let delta: i32 = if matches!(key.code, KeyCode::Char('h') | KeyCode::Left) {
                    -1
                } else {
                    1
                };
                let policy = self.editor.policy_mut();
                if self.selected == 4 {
                    policy.created_sessions_window_hours =
                        (i32::from(policy.created_sessions_window_hours) + delta).clamp(
                            1,
                            i32::from(
                                rsi_common::harness_manager_v2::MAX_CREATED_SESSIONS_WINDOW_HOURS,
                            ),
                        ) as u16;
                } else {
                    policy.max_recovery_attempts =
                        (i32::from(policy.max_recovery_attempts) + delta).clamp(0, 32) as u16;
                }
            }
            _ => return,
        }
        self.prepared = None;
        self.cap_confirm = false;
        self.error = self.editor.error.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::global_manager_command::{
        default_allowed_launches, default_project_policy,
    };

    fn project(name: &str) -> Project {
        let now = chrono::Utc::now();
        Project {
            id: Uuid::new_v4(),
            name: name.into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn form() -> CoverageForm {
        let projects = [project("Alpha"), project("Beta")];
        let mut policy = default_project_policy();
        policy.paused = true;
        policy.created_sessions_window_hours = 72;
        policy.max_recovery_attempts = 7;
        policy.allowed_launches = vec![default_allowed_launches()[1].clone()];
        CoverageForm::new(
            ConfigureGlobalManagerRequestV1 {
                session_id: Uuid::new_v4(),
                project_ids: vec![projects[0].id],
                allowed_launches: default_allowed_launches(),
                project_policy: policy,
                expected_grant_version: 7,
                idempotency_key: "test-coverage".into(),
            },
            &projects,
        )
    }

    fn press(form: &mut CoverageForm, code: KeyCode) -> Outcome {
        form.handle_key(KeyEvent::from(code))
    }

    #[test]
    fn adding_a_project_preserves_the_exact_custom_grant() {
        let mut form = form();
        let original = form.original.clone();
        form.selected = 1;
        press(&mut form, KeyCode::Char(' '));
        let mut expected = original;
        expected.project_ids.push(form.projects[1].project_id);
        assert_eq!(form.request().unwrap(), expected);
        assert!(form.review().iter().any(|line| line == "+ Cover Beta"));
    }

    #[test]
    fn duplicate_project_names_show_paths_and_select_the_exact_project() {
        let mut form = form();
        let mut alpha = project("Same name");
        let mut beta = project("Same name");
        alpha.path = Some(std::path::PathBuf::from("workspace").join("alpha"));
        beta.path = Some(std::path::PathBuf::from("workspace").join("beta"));
        let mut request = form.original.clone();
        request.project_ids = vec![alpha.id];
        form = CoverageForm::new(request, &[alpha.clone(), beta.clone()]);
        assert!(form.rows()[0].contains(&alpha.path.unwrap().display().to_string()));
        assert!(form.rows()[1].contains(&beta.path.unwrap().display().to_string()));
        form.selected = 1;
        press(&mut form, KeyCode::Char(' '));
        assert_eq!(form.request().unwrap().project_ids, vec![alpha.id, beta.id]);
    }

    #[test]
    fn full_new_draft_has_all_permissions_and_group_creation() {
        let mut form = form();
        form.new_defaults();
        assert_eq!(
            form.editor.preset(),
            ManagerPolicyPreset::FullProjectControl
        );
        assert!(form.editor.policy().allow_create_groups);
        assert_eq!(form.editor.policy().capabilities.len(), CAPABILITIES.len());
        assert_eq!(form.editor.policy().created_sessions_window_hours, 72);
        assert!(form.request().is_ok());
    }

    #[test]
    fn group_creation_is_explicit_and_topology_dependency_is_visible() {
        let mut form = form();
        form.step = 1;
        form.selected = 2;
        press(&mut form, KeyCode::Char(' '));
        assert!(
            form.error
                .as_ref()
                .unwrap()
                .contains("Enable that permission first")
        );
        assert_eq!(form.editor.policy(), &form.original.project_policy);
        let topology = CAPABILITIES
            .iter()
            .position(|value| *value == ManagerCapabilityV2::Topology)
            .unwrap();
        form.selected = topology + 3;
        press(&mut form, KeyCode::Char(' '));
        form.selected = 2;
        press(&mut form, KeyCode::Char(' '));
        assert!(form.editor.policy().allow_create_groups);
        form.selected = topology + 3;
        press(&mut form, KeyCode::Char(' '));
        assert!(
            form.error
                .as_ref()
                .unwrap()
                .contains("Turn Create project groups off")
        );
        assert!(
            form.editor
                .policy()
                .capabilities
                .contains(&ManagerCapabilityV2::Topology)
        );
    }

    #[test]
    fn named_choices_keep_custom_tuples_and_separate_policy_restrictions() {
        let mut form = form();
        let saved = form.original.allowed_launches.clone();
        assert_eq!(form.editor.launches(), saved);
        form.step = 2;
        press(&mut form, KeyCode::Char(' '));
        let request = form.request().unwrap();
        let mut expected = saved;
        expected.remove(0);
        assert_eq!(request.allowed_launches, expected);
        assert_eq!(request.project_policy, form.original.project_policy);
        assert!(
            form.launches
                .iter()
                .any(|(label, launch)| label == "Custom saved choice"
                    && launch.effort.as_deref() == Some("xhigh"))
        );
    }

    #[test]
    fn review_is_required_and_retries_keep_the_prepared_request() {
        let mut form = form();
        assert_eq!(press(&mut form, KeyCode::Char('y')), Outcome::Stay);
        press(&mut form, KeyCode::Char('r'));
        assert_eq!(form.step, 4);
        let prepared = form.prepared.clone().unwrap();
        assert_eq!(press(&mut form, KeyCode::Enter), Outcome::Stay);
        assert_eq!(press(&mut form, KeyCode::Char('y')), Outcome::Save);
        assert_eq!(form.prepared, Some(prepared));
        press(&mut form, KeyCode::Esc);
        press(&mut form, KeyCode::Char(' '));
        assert!(form.prepared.is_none());
    }

    #[test]
    fn limits_accept_multiple_digits_and_preserve_the_creation_window() {
        let mut form = form();
        form.step = 3;
        form.selected = 1;
        for ch in "24".chars() {
            press(&mut form, KeyCode::Char(ch));
        }
        assert_eq!(form.editor.policy().max_created_sessions, 24);
        assert_eq!(form.editor.policy().created_sessions_window_hours, 72);
        form.selected = 4;
        press(&mut form, KeyCode::Right);
        assert_eq!(form.editor.policy().created_sessions_window_hours, 73);
        form.selected = 5;
        press(&mut form, KeyCode::Left);
        assert_eq!(form.editor.policy().max_recovery_attempts, 6);
    }

    #[test]
    fn searching_keeps_navigation_letters_as_text_and_hidden_coverage() {
        let mut form = form();
        press(&mut form, KeyCode::Char('/'));
        for ch in "Beta".chars() {
            press(&mut form, KeyCode::Char(ch));
        }
        press(&mut form, KeyCode::Enter);
        assert_eq!(form.visible_projects(), vec![1]);
        press(&mut form, KeyCode::Char(' '));
        assert_eq!(form.request().unwrap().project_ids.len(), 2);
        press(&mut form, KeyCode::Char('/'));
        for ch in "jkg".chars() {
            press(&mut form, KeyCode::Char(ch));
        }
        assert_eq!(form.query, "Betajkg");
    }

    #[test]
    fn missing_projects_are_preserved_until_explicitly_removed() {
        let mut form = form();
        let missing = Uuid::new_v4();
        let mut request = form.original.clone();
        request.project_ids.push(missing);
        form = CoverageForm::new(request, &[]);
        assert_eq!(
            form.request().unwrap().project_ids,
            form.original.project_ids
        );
        assert!(form.rows()[0].contains("Unavailable project"));
        form.selected = 1;
        press(&mut form, KeyCode::Char(' '));
        assert_eq!(
            form.request().unwrap().project_ids,
            vec![form.original.project_ids[0]]
        );
    }
}
