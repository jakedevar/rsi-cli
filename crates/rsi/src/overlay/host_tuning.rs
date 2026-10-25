//! Operator host resource/profile/preset editor, reached from Host & Build settings.
use crate::{app::App, types::OverlayState};
use crossterm::event::{KeyCode, KeyEvent};
use rsi_common::host_tuning::*;

#[derive(Debug, Clone, Copy)]
pub enum Edit {
    Name,
    Reserve,
    Guidance,
    Value,
    Min,
    Max,
}
#[derive(Debug, Clone)]
pub struct State {
    pub view: TuningView,
    pub draft: TuningConfiguration,
    pub preset: usize,
    pub key: usize,
    pub settings_focus: bool,
    pub editing: Option<Edit>,
    pub input: String,
    pub message: String,
}
impl State {
    pub fn new(view: TuningView) -> Self {
        Self {
            draft: view.state.configuration.clone(),
            view,
            preset: 0,
            key: 0,
            settings_focus: false,
            editing: None,
            input: String::new(),
            message: String::new(),
        }
    }
    pub fn name(&self) -> Option<String> {
        self.draft.presets.keys().nth(self.preset).cloned()
    }
    fn begin(&mut self, edit: Edit) {
        let key = RESOURCE_KEYS[self.key].0;
        self.input = match edit {
            Edit::Name => String::new(),
            Edit::Reserve => match self.draft.profile {
                HostProfile::Shared { reserve_percent } => reserve_percent.to_string(),
                _ => "0".into(),
            },
            Edit::Guidance => self
                .draft
                .live_session_cap_guidance
                .map_or(String::new(), |v| v.to_string()),
            Edit::Value => self
                .name()
                .and_then(|n| self.draft.presets[&n].get(key).copied())
                .map_or(String::new(), |v| v.to_string()),
            Edit::Min => self
                .draft
                .bounds
                .get(key)
                .map_or(String::new(), |b| b.min.to_string()),
            Edit::Max => self
                .draft
                .bounds
                .get(key)
                .map_or(String::new(), |b| b.max.to_string()),
        };
        self.editing = Some(edit);
    }
    fn finish_edit(&mut self) -> Result<(), String> {
        let edit = self.editing.ok_or("No edit")?;
        let key = RESOURCE_KEYS[self.key].0.to_string();
        match edit {
            Edit::Name => {
                let name = self.input.trim().to_string();
                if self.draft.presets.contains_key(&name) {
                    return Err("That preset exists; select it to edit its values.".into());
                }
                let mut candidate = self.draft.clone();
                for (key, value) in &self.view.actual {
                    candidate.bounds.entry(key.clone()).or_insert(Bounds {
                        min: *value,
                        max: *value,
                    });
                }
                candidate
                    .presets
                    .insert(name.clone(), self.view.actual.clone());
                candidate.validate()?;
                self.preset = candidate
                    .presets
                    .keys()
                    .position(|n| n == &name)
                    .unwrap_or(0);
                self.draft = candidate;
            }
            Edit::Reserve => {
                let reserve_percent = self
                    .input
                    .parse::<u8>()
                    .ok()
                    .filter(|v| *v <= 100)
                    .ok_or("Type a reserve from 0 to 100 percent.")?;
                self.draft.profile = HostProfile::Shared { reserve_percent };
            }
            Edit::Guidance => {
                self.draft.live_session_cap_guidance = if self.input.is_empty() {
                    None
                } else {
                    Some(
                        self.input
                            .parse()
                            .map_err(|_| "Type a session count, or leave blank for no guidance.")?,
                    )
                }
            }
            Edit::Value => {
                let value = self
                    .input
                    .parse()
                    .map_err(|_| "Type a whole nonnegative number.")?;
                let name = self.name().ok_or("Create a preset with n first.")?;
                self.draft
                    .presets
                    .get_mut(&name)
                    .unwrap()
                    .insert(key, value);
            }
            Edit::Min | Edit::Max => {
                let value = self
                    .input
                    .parse()
                    .map_err(|_| "Type a whole nonnegative number.")?;
                let current = self.view.actual.get(&key).copied().unwrap_or(value);
                let bound = self.draft.bounds.entry(key).or_insert(Bounds {
                    min: current,
                    max: current,
                });
                if matches!(edit, Edit::Min) {
                    bound.min = value;
                } else {
                    bound.max = value;
                }
            }
        }
        self.editing = None;
        self.message =
            "Draft changed. w saves the profile, bounds and presets; a applies a saved preset."
                .into();
        Ok(())
    }
}

pub async fn open(app: &mut App) {
    match app.client.get_host_tuning().await {
        Ok(view) => app.overlay = OverlayState::HostTuning(Box::new(State::new(view))),
        Err(error) => app.notify_error(format!("Host tuning: {error}")),
    }
}
pub async fn handle_key(app: &mut App, key: KeyEvent) {
    let OverlayState::HostTuning(state) = &mut app.overlay else {
        return;
    };
    if state.editing.is_some() {
        match key.code {
            KeyCode::Esc => state.editing = None,
            KeyCode::Enter => {
                if let Err(error) = state.finish_edit() {
                    state.message = error;
                }
            }
            KeyCode::Backspace => {
                state.input.pop();
            }
            KeyCode::Char(c) if !c.is_control() => state.input.push(c),
            _ => {}
        }
        return;
    }
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.overlay = OverlayState::None,
        KeyCode::Tab => state.settings_focus = !state.settings_focus,
        KeyCode::Down | KeyCode::Char('j') => {
            if state.settings_focus {
                state.key = (state.key + 1).min(RESOURCE_KEYS.len() - 1);
            } else {
                state.preset = (state.preset + 1).min(state.draft.presets.len().saturating_sub(1));
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if state.settings_focus {
                state.key = state.key.saturating_sub(1);
            } else {
                state.preset = state.preset.saturating_sub(1);
            }
        }
        KeyCode::Char('n') => state.begin(Edit::Name),
        KeyCode::Char('b') => state.begin(Edit::Reserve),
        KeyCode::Char('g') => state.begin(Edit::Guidance),
        KeyCode::Char('d') => {
            state.draft.profile = HostProfile::Dedicated;
            state.message = "Dedicated profile staged; w saves.".into();
        }
        KeyCode::Enter if state.settings_focus => state.begin(Edit::Value),
        KeyCode::Char('m') => state.begin(Edit::Min),
        KeyCode::Char('M') => state.begin(Edit::Max),
        KeyCode::Char('r') => open(app).await,
        KeyCode::Char('w') => {
            let params = ConfigureHostTuning {
                expected_revision: state.view.state.revision,
                configuration: state.draft.clone(),
            };
            match app.client.configure_host_tuning(&params).await {
                Ok(view) => {
                    let OverlayState::HostTuning(state) = &mut app.overlay else {
                        return;
                    };
                    state.draft = view.state.configuration.clone();
                    state.view = view;
                    state.message = "Saved. Select a preset and press a to apply.".into();
                }
                Err(error) => {
                    if let OverlayState::HostTuning(state) = &mut app.overlay {
                        state.message = format!("Save failed: {error}");
                    }
                }
            }
        }
        KeyCode::Char('a') | KeyCode::Char('u') => {
            if state.draft != state.view.state.configuration {
                state.message =
                    "Save the draft with w, or refresh with r, before switching.".into();
                return;
            }
            let action = if key.code == KeyCode::Char('u') {
                SwitchHostPreset::Undo {}
            } else {
                let Some(name) = state.name() else {
                    state.message = "Create and save a preset first (n, w).".into();
                    return;
                };
                SwitchHostPreset::Apply { name }
            };
            match app.client.switch_host_preset(&action).await {
                Ok(receipt) => {
                    let OverlayState::HostTuning(state) = &mut app.overlay else {
                        return;
                    };
                    state.message = receipt.error.unwrap_or_else(|| {
                        if receipt.changed {
                            "Applied; values read back below.".into()
                        } else {
                            "Already matches; no settings changed.".into()
                        }
                    });
                    if let Some(report) = receipt
                        .settings_result
                        .and_then(|r| r.get("build_slice").cloned())
                    {
                        state.message.push_str(&format!(" Build slice: {}", report));
                    }
                    state.view = receipt.view;
                    app.request_daemon_config_refresh();
                }
                Err(error) => {
                    if let OverlayState::HostTuning(state) = &mut app.overlay {
                        state.message = format!("Switch refused: {error}");
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn editor_stages_reserve_and_values_with_explicit_bounds() {
        let probe: ResourceProbe = serde_json::from_value(serde_json::json!({
            "effective_cpus":{"status":"unsupported"},"cgroup_cpu_quota":{"status":"unsupported"},"cgroup":{"status":"unsupported"},
            "total_ram_bytes":{"status":"unsupported"},"available_ram_bytes":{"status":"unsupported"},"data_dir_free_bytes":{"status":"unsupported"},
            "sandbox_base_free_bytes":{"status":"unsupported"},"load_average":{"status":"unsupported"},"cpu_temperature_c":{"status":"unsupported"}
        })).unwrap();
        let mut state = State::new(TuningView {
            state: TuningState::default(),
            probe,
            actual: [("host_load_admission_threshold".into(), 20)].into(),
            apply_classes: Default::default(),
            notes: vec![],
        });
        state.begin(Edit::Reserve);
        state.input = "15".into();
        state.finish_edit().unwrap();
        state.begin(Edit::Name);
        state.input = "day/shared".into();
        state.finish_edit().unwrap();
        assert_eq!(
            state.draft.profile,
            HostProfile::Shared {
                reserve_percent: 15
            }
        );
        assert_eq!(
            state.draft.presets["day/shared"]["host_load_admission_threshold"],
            20
        );
        assert_eq!(
            state.draft.bounds["host_load_admission_threshold"],
            Bounds { min: 20, max: 20 }
        );
        assert_eq!(state.view.state.revision, 0);
        state.draft.validate().unwrap();
    }
}

#[cfg(test)]
pub(crate) fn test_state() -> State {
    State::new(TuningView {
        state: TuningState::default(),
        probe: ResourceProbe {
            effective_cpus: Reading::Unsupported,
            cgroup_cpu_quota: Reading::Unsupported,
            cgroup: Reading::Unsupported,
            total_ram_bytes: Reading::Unsupported,
            available_ram_bytes: Reading::Unsupported,
            data_dir_free_bytes: Reading::Unsupported,
            sandbox_base_free_bytes: Reading::Unsupported,
            load_average: Reading::Unsupported,
            cpu_temperature_c: Reading::Unsupported,
        },
        actual: Default::default(),
        apply_classes: Default::default(),
        notes: vec![],
    })
}
