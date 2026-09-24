//! Policy-owned catalog requests and exact tuple selection; never changes global model selection.
use super::policy::PROVIDERS;
use crate::{
    client::DaemonClient,
    types::{ModelDropdownState, OverlayState},
    widget::model_dropdown::{ModelDropdownAction, handle_model_dropdown_key_with_providers},
};
use crossterm::event::{KeyCode, KeyEvent};
use rsi_common::{harness_manager_v2::ManagerLaunchChoiceV2, model_utils, types::SessionProvider};
use std::path::PathBuf;
use tokio::{sync::oneshot, task::JoinHandle};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerEffortChoices {
    Known {
        levels: &'static [&'static str],
        default_hint: Option<&'static str>,
    },
    DefaultOnly {
        capability_unknown: bool,
    },
}
impl ManagerEffortChoices {
    pub fn values(&self) -> Vec<Option<String>> {
        let mut values = vec![None];
        if let Self::Known { levels, .. } = self {
            values.extend(levels.iter().map(|s| Some((*s).to_owned())));
        }
        values
    }
    pub fn description(&self) -> String {
        match self {
            Self::Known { default_hint, .. } => format!(
                "Default = no explicit effort{}; never an effort wildcard",
                default_hint
                    .map(|s| format!(" (currently {s})"))
                    .unwrap_or_default()
            ),
            Self::DefaultOnly {
                capability_unknown: true,
            } => "Exact effort capability unknown; choose Default (no explicit effort)".into(),
            Self::DefaultOnly {
                capability_unknown: false,
            } => "No explicit effort choices advertised; Default means no explicit effort".into(),
        }
    }
}
pub fn manager_launch_effort_choices(
    provider: SessionProvider,
    model: &str,
) -> ManagerEffortChoices {
    let levels = match provider {
        SessionProvider::Codex | SessionProvider::CodexAppServer => {
            model_utils::known_codex_effort_ladder(model)
        }
        SessionProvider::Claude | SessionProvider::Pioneer => {
            let claude =
                model_utils::parse_model_version(model).map(|_| model_utils::effort_ladder(model));
            if provider == SessionProvider::Pioneer {
                claude.or_else(|| model_utils::known_codex_effort_ladder(model))
            } else {
                claude
            }
        }
        _ => {
            return ManagerEffortChoices::DefaultOnly {
                capability_unknown: false,
            };
        }
    };
    match levels {
        Some(levels) => ManagerEffortChoices::Known {
            levels,
            default_hint: model_utils::default_effort_level(model),
        },
        None => ManagerEffortChoices::DefaultOnly {
            capability_unknown: true,
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogStatus {
    Loading,
    Loaded,
    Failed(String),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerStage {
    Model,
    Effort,
}
pub struct ManagerLaunchPickerState {
    pub row: Option<usize>,
    pub original: Option<ManagerLaunchChoiceV2>,
    pub model_dropdown: ModelDropdownState,
    pub candidate: Option<ManagerLaunchChoiceV2>,
    pub effort_choices: ManagerEffortChoices,
    pub effort_selected: usize,
    pub stage: PickerStage,
    pub request_generation: u64,
    pub needs_refresh: bool,
    pub status: CatalogStatus,
    pub error: Option<String>,
}
pub enum PickerOutcome {
    Consumed,
    Dismissed,
    Confirmed(ManagerLaunchChoiceV2),
}
impl ManagerLaunchPickerState {
    pub fn new(row: Option<usize>, original: Option<ManagerLaunchChoiceV2>) -> Self {
        let provider = original
            .as_ref()
            .map_or(SessionProvider::Claude, |c| c.provider);
        let model_dropdown = ModelDropdownState::new(
            provider,
            crate::app::models_for_provider(provider),
            original.as_ref().map(|c| c.model.as_str()),
        );
        Self {
            row,
            original,
            model_dropdown,
            candidate: None,
            effort_choices: ManagerEffortChoices::DefaultOnly {
                capability_unknown: true,
            },
            effort_selected: 0,
            stage: PickerStage::Model,
            request_generation: 0,
            needs_refresh: true,
            status: CatalogStatus::Loading,
            error: None,
        }
    }
    pub fn status_label(&self) -> String {
        match &self.status {
            CatalogStatus::Loading => "Loading catalog · fallback rows are unverified".into(),
            CatalogStatus::Loaded if self.model_dropdown.models.is_empty() => {
                "Catalog loaded: no selectable models; retained choice unchanged".into()
            }
            CatalogStatus::Loaded => "Catalog loaded · choose an exact model ID".into(),
            CatalogStatus::Failed(e) => {
                format!("Catalog unavailable: {e} · fallback rows unverified · r retry")
            }
        }
    }
    pub fn handle_key(&mut self, key: KeyEvent) -> PickerOutcome {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            return PickerOutcome::Dismissed;
        }
        if key.code == KeyCode::Char('r') {
            self.needs_refresh = true;
            self.status = CatalogStatus::Loading;
            self.stage = PickerStage::Model;
            self.error = None;
            return PickerOutcome::Consumed;
        }
        if self.stage == PickerStage::Effort {
            let values = self.effort_choices.values();
            if super::super::list::handle_list_nav_key(
                &mut self.effort_selected,
                values.len(),
                &key,
            ) {
                return PickerOutcome::Consumed;
            }
            if key.code == KeyCode::Backspace {
                self.stage = PickerStage::Model;
                return PickerOutcome::Consumed;
            }
            if key.code == KeyCode::Enter {
                if let Some(mut candidate) = self.candidate.clone() {
                    candidate.effort = values[self.effort_selected.min(values.len() - 1)].clone();
                    if let Err(e) = candidate.validate() {
                        self.error = Some(e.into());
                    } else {
                        return PickerOutcome::Confirmed(candidate);
                    }
                }
            }
            return PickerOutcome::Consumed;
        }
        match handle_model_dropdown_key_with_providers(
            &mut self.model_dropdown,
            &key,
            &PROVIDERS,
            &[],
        ) {
            ModelDropdownAction::ProviderCycled => {
                self.needs_refresh = true;
                self.status = CatalogStatus::Loading;
                self.candidate = None;
                self.error = None;
            }
            ModelDropdownAction::Selected(model) => {
                if self.status != CatalogStatus::Loaded {
                    self.error = Some(
                        "Wait for a successful catalog before selecting; Esc retains the policy."
                            .into(),
                    );
                    return PickerOutcome::Consumed;
                }
                let provider = self.model_dropdown.provider;
                self.effort_choices = manager_launch_effort_choices(provider, &model);
                let previous = self
                    .original
                    .as_ref()
                    .filter(|c| c.provider == provider && c.model == model)
                    .and_then(|c| c.effort.clone());
                self.effort_selected = self
                    .effort_choices
                    .values()
                    .iter()
                    .position(|v| *v == previous)
                    .unwrap_or(0);
                self.candidate = Some(ManagerLaunchChoiceV2 {
                    provider,
                    model,
                    effort: previous,
                });
                self.stage = PickerStage::Effort;
                self.error = None;
            }
            ModelDropdownAction::Dismissed => return PickerOutcome::Dismissed,
            _ => {}
        }
        PickerOutcome::Consumed
    }
}

pub struct ManagerCatalogResult {
    pub generation: u64,
    pub provider: SessionProvider,
    pub outcome: Result<Vec<(String, String)>, String>,
}
pub struct ManagerCatalogRequest {
    pub receiver: oneshot::Receiver<ManagerCatalogResult>,
    task: JoinHandle<()>,
}
impl Drop for ManagerCatalogRequest {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub fn request_catalog(
    socket: PathBuf,
    generation: u64,
    provider: SessionProvider,
) -> ManagerCatalogRequest {
    let (send, receiver) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut client = DaemonClient::new(socket);
        let outcome = match client.connect().await {
            Ok(()) => client
                .discover_models(provider)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let _ = send.send(ManagerCatalogResult {
            generation,
            provider,
            outcome,
        });
    });
    ManagerCatalogRequest { receiver, task }
}
pub async fn next_result(
    overlay: &mut OverlayState,
) -> Result<ManagerCatalogResult, oneshot::error::RecvError> {
    if let OverlayState::HarnessManagerV2(view) = overlay {
        if let Some(state) = view.policy.as_mut() {
            if let Some(request) = &mut state.catalog_request {
                return (&mut request.receiver).await;
            }
        }
    }
    std::future::pending().await
}
pub fn dispatch_result(
    overlay: &mut OverlayState,
    result: Result<ManagerCatalogResult, oneshot::error::RecvError>,
) {
    if let OverlayState::HarnessManagerV2(view) = overlay {
        if let Some(state) = view.policy.as_mut() {
            match result {
                Ok(result) => state.apply_catalog_result(result),
                Err(_) => {
                    state.catalog_request = None;
                    if let Some(picker) = &mut state.picker {
                        picker.status =
                            CatalogStatus::Failed("Catalog request ended; retry with r".into());
                    }
                }
            }
        }
    }
}
