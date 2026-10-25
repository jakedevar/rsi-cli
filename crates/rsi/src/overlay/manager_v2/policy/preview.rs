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
                    "Saved{}: {} · scope {} / policy {}{}",
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
    /// Preset of the draft, with the permission profile when validation or
    /// explicit overrides make it Custom.
    fn draft_preset_label(&self) -> String {
        let c = self.classification();
        format!(
            "{}{}",
            c.preset.label(),
            c.permission_profile
                .filter(|p| *p != c.preset)
                .map(|p| format!(" / {} permissions", p.label()))
                .unwrap_or_default()
        )
    }
    /// Summary block line 1: the mode and preset the draft would save.
    pub fn mode_summary(&self) -> String {
        format!(
            "Mode {:?} · Preset {}{}",
            self.draft.mode,
            self.draft_preset_label(),
            if self.draft.paused {
                " · OPERATOR PAUSE"
            } else {
                ""
            }
        )
    }
    /// Summary block line 3: the spend cap and how many models are allowed.
    pub fn limits_summary(&self) -> String {
        let models = if self.draft.allowed_launches.is_empty() {
            "Allowed models: not restricted (any valid manual choice)".to_string()
        } else {
            format!(
                "Allowed models: {} ({})",
                self.draft.allowed_launches.len(),
                self.launch_counts()
            )
        };
        format!(
            "Spend {} · {models}",
            self.draft
                .max_spend_usd
                .map_or_else(|| "uncapped".into(), |v| format!("cap USD {v}")),
        )
    }
    /// The saved line, ending in whether the draft still equals it.
    pub fn saved_line(&self) -> String {
        format!(
            "{}{}",
            self.saved_summary(),
            if self.draft == self.opened_draft {
                " · draft unchanged"
            } else {
                ""
            }
        )
    }
    /// What the draft changes against the saved policy, one entry per change.
    pub fn draft_diff(&self) -> Vec<String> {
        let (d, o) = (&self.draft, &self.opened_draft);
        let state = |v: &str| match v {
            "true" => "on".to_string(),
            "false" => "off".to_string(),
            other => other.to_string(),
        };
        let mut diff = vec![];
        for row in self.rows() {
            if !row.modified
                || matches!(
                    row.field,
                    Field::Capability(_)
                        | Field::EditLaunch(_)
                        | Field::LaunchProvider(_)
                        | Field::LaunchModel(_)
                        | Field::LaunchEffort(_)
                        | Field::AddLaunch
                )
            {
                continue;
            }
            if let Some(saved) = &row.saved {
                diff.push(format!(
                    "{} {}→{}",
                    row.label,
                    state(saved),
                    state(&row.value)
                ));
            }
        }
        let grants = |from: &[ManagerCapabilityV2], to: &[ManagerCapabilityV2], sign: char| {
            to.iter()
                .filter(|c| !from.contains(c))
                .map(|c| format!("{sign}{}", capability_label(*c)))
                .collect::<Vec<_>>()
        };
        let mut changed = grants(&o.capabilities, &d.capabilities, '+');
        changed.extend(grants(&d.capabilities, &o.capabilities, '−'));
        if !changed.is_empty() {
            diff.push(format!("grants {}", changed.join(" ")));
        }
        let added = d
            .allowed_launches
            .iter()
            .filter(|c| !o.allowed_launches.contains(c))
            .count();
        let removed = o
            .allowed_launches
            .iter()
            .filter(|c| !d.allowed_launches.contains(c))
            .count();
        if added + removed > 0 {
            diff.push(format!("allowed models +{added} −{removed}"));
        }
        diff
    }
    /// Summary block diff line; present only while the draft differs from the
    /// saved policy.
    pub fn draft_line(&self) -> Option<String> {
        if self.draft == self.opened_draft {
            return None;
        }
        let diff = self.draft_diff();
        Some(format!(
            "Draft: {} · unsaved changes: {}",
            self.draft_preset_label(),
            if diff.is_empty() {
                "see the * rows".to_string()
            } else {
                diff.join(" · ")
            }
        ))
    }
    pub fn suggestions(&self) -> Vec<ManagerAllowanceSuggestion> {
        self.classification()
            .permission_profile
            .map(|p| suggested_manager_allowances(&self.draft, p))
            .unwrap_or_default()
    }
    /// What the list shows beside a launch choice's effort: the catalog label
    /// or why the choice cannot be checked. Silent until a catalog was read.
    pub fn choice_note(&self, choice: &ManagerLaunchChoiceV2) -> Option<String> {
        if let Some(label) = self
            .choice_labels
            .get(&(choice.provider, choice.model.clone()))
        {
            return Some(label.clone());
        }
        match self.catalog_status.get(&choice.provider) {
            Some(CatalogStatus::Loaded) => Some("not in the current catalog".into()),
            Some(CatalogStatus::Failed(_)) => Some("catalog unavailable".into()),
            _ => None,
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
