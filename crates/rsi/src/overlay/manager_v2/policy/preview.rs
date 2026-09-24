use super::*;
use rsi_common::harness_manager::HarnessManagerScopeModeV1;

impl PolicyState {
    pub fn classification(&self) -> ManagerPresetClassification {
        classify_manager_policy(&self.draft, self.config.scope_mode)
    }
    pub fn saved_summary(&self) -> String {
        match &self.saved {
            None => "Saved: no policy · draft starts at legacy Status defaults".into(),
            Some(saved) => {
                let moved = (saved.scope_version != self.config.row_version).then(|| {
                    format!(
                        " · scope moved {} -> {}",
                        saved.scope_version, self.config.row_version
                    )
                });
                format!(
                    "Saved{}: {} · scope {} · policy {}{}",
                    if saved.revoked { " — revoked" } else { "" },
                    classify_manager_policy(&saved.policy, self.config.scope_mode)
                        .preset
                        .label(),
                    saved.scope_version,
                    saved.row_version,
                    moved.unwrap_or_default()
                )
            }
        }
    }
    pub fn draft_summary(&self) -> String {
        let c = self.classification();
        format!(
            "Draft: {}{} · {} · scope {} / policy {}{}",
            c.preset.label(),
            c.permission_profile
                .filter(|p| *p != c.preset)
                .map(|p| format!(" / {} permissions", p.label()))
                .unwrap_or_default(),
            if self.draft == self.opened_draft {
                "unchanged"
            } else {
                "unsaved changes"
            },
            self.config.row_version,
            self.policy_version,
            if self.draft.paused {
                " · OPERATOR PAUSE"
            } else {
                ""
            }
        )
    }
    pub fn suggestions(&self) -> Vec<ManagerAllowanceSuggestion> {
        self.classification()
            .permission_profile
            .map(|p| suggested_manager_allowances(&self.draft, p))
            .unwrap_or_default()
    }
    pub fn choice_status(&self, choice: &ManagerLaunchChoiceV2) -> String {
        if let Some(label) = self
            .choice_labels
            .get(&(choice.provider, choice.model.clone()))
        {
            return label.clone();
        }
        match self.catalog_status.get(&choice.provider) {
            Some(CatalogStatus::Loaded) => {
                "Retained exact restriction; unavailable in current catalog".into()
            }
            Some(CatalogStatus::Failed(_)) => {
                "Retained exact restriction; catalog unavailable".into()
            }
            _ => "Retained exact restriction; catalog not checked".into(),
        }
    }
    pub fn preview_rows(&self) -> Vec<(String, String)> {
        let p = &self.draft;
        let c = self.classification();
        let names = |ids: &[Uuid], choices: &[(Uuid, String)]| {
            ids.iter()
                .map(|id| {
                    choices
                        .iter()
                        .find(|(i, _)| i == id)
                        .map(|(_, name)| format!("{name} ({id})"))
                        .unwrap_or_else(|| id.to_string())
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let scope = match self.config.scope_mode {
            HarnessManagerScopeModeV1::Project => {
                "Current project, including future Groups/Epics".into()
            }
            HarnessManagerScopeModeV1::Selected => format!(
                "Selected Groups: {}; Epics: {}",
                names(&self.config.group_ids, &self.groups),
                names(self.config.explicit_epic_ids(), &self.epics)
            ),
        };
        let mut rows = vec![
            ("Scope".into(), scope),
            (
                "Current manager".into(),
                self.config
                    .current_session_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "Unknown — authority requires repair".into()),
            ),
            (
                "Mode / draft grants".into(),
                format!(
                    "{:?} / {}",
                    p.mode,
                    if p.capabilities.is_empty() {
                        "No write grants".into()
                    } else {
                        p.capabilities
                            .iter()
                            .map(|c| format!("{c:?}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                ),
            ),
            (
                "Root Group creation".into(),
                if p.allow_create_groups {
                    if self.config.scope_mode == HarnessManagerScopeModeV1::Selected {
                        "Additional explicit root-Group grant retained"
                    } else {
                        "Granted within current project"
                    }
                } else {
                    "Not granted"
                }
                .into(),
            ),
            (
                "Active / provider ceilings".into(),
                format!(
                    "{} / {}",
                    p.max_active_sessions,
                    if p.provider_limits.is_empty() {
                        "inherit".into()
                    } else {
                        p.provider_limits
                            .iter()
                            .map(|l| format!("{:?}: {}", l.provider, l.max_active))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                ),
            ),
            (
                "Creation allowances".into(),
                format!(
                    "{} containers / {} sessions",
                    p.max_created_containers, p.max_created_sessions
                ),
            ),
            (
                "Self-succession".into(),
                format!(
                    "{}; consumes session creation, not automatic recovery",
                    if p.capabilities
                        .contains(&ManagerCapabilityV2::SelfSuccession)
                    {
                        "Configured permission; admission rechecked"
                    } else {
                        "Not granted"
                    }
                ),
            ),
            (
                "Automatic recovery".into(),
                if p.max_recovery_attempts == 0 {
                    "Disabled by retained limit 0".into()
                } else {
                    format!(
                        "{} attempts · retry {}s · deadline {}s",
                        p.max_recovery_attempts, p.retry_delay_seconds, p.request_timeout_seconds
                    )
                },
            ),
            (
                "Spend / observed usage".into(),
                format!(
                    "{} / Unknown — admission rechecked",
                    p.max_spend_usd
                        .map(|v| format!("USD {v} cap"))
                        .unwrap_or_else(|| "No manager USD cap".into())
                ),
            ),
            (
                "Operator pause / Epic pauses".into(),
                format!(
                    "{} / {}",
                    p.paused,
                    if p.paused_epic_ids.is_empty() {
                        "none".into()
                    } else {
                        names(&p.paused_epic_ids, &self.epics)
                    }
                ),
            ),
        ];
        if !p.group_ids.is_empty() {
            rows.push((
                "Additional policy Group grants".into(),
                names(&p.group_ids, &self.groups),
            ));
        }
        for field in c.zero_conflicts {
            rows.push((
                field.label().into(),
                "Disabled by retained limit 0 · Use suggested allowances to edit this draft".into(),
            ));
        }
        if c.root_group_permission_mismatch && !p.allow_create_groups {
            rows.push((
                "Custom root permission".into(),
                "Root creation not granted; explicit overrides retained".into(),
            ));
        }
        if let Some(error) = c.validation_error {
            rows.push(("Draft validation".into(), error.into()));
        }
        if self.saved.as_ref().is_some_and(|p| p.revoked) {
            rows.push((
                "Revoked saved grant".into(),
                "s explicitly saves and regrants this draft under current scope/policy fences"
                    .into(),
            ));
        }
        for (_, name) in &self.custom_provider_names {
            rows.push((format!("Custom endpoint: {name}"), "Endpoint-specific manager restrictions unsupported; use daemon-configured Local catalog".into()));
        }
        rows
    }
}
