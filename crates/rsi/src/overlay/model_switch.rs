//! Session-detail model/effort picker (Issue #681).
//!
//! Opens on the focused session, asks the daemon what a switch may use
//! (`GetSessionModelSwitchOptions`), offers the provider's models narrowed by
//! the operator launch-model allowlist and the chosen model's effort levels,
//! and queues the pick for the session's next turn (`QueueSessionModelUpdate`).
//! The daemon never interrupts a running turn: a switch applies at the next
//! turn boundary.

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent};
use rsi_common::rpc::{PendingSessionModelUpdate, SessionModelSwitchOptions};
use rsi_common::types::SessionProvider;
use uuid::Uuid;

use super::list;

/// State of the open model/effort picker.
#[derive(Debug)]
pub struct ModelSwitchState {
    pub session_id: Uuid,
    /// Daemon-reported current tuple, fence, context note and allowlist.
    pub options: SessionModelSwitchOptions,
    /// `(model id, label)` rows the operator may pick, allowlist applied.
    pub models: Vec<(String, String)>,
    pub model_index: usize,
    /// Effort choices for the highlighted model; `None` is the model default.
    pub efforts: Vec<Option<String>>,
    pub effort_index: usize,
}

impl ModelSwitchState {
    /// Build the picker for `options`, seeding the highlight from the
    /// session's current model and effort. `None` when the provider offers no
    /// model the operator may switch to.
    pub(crate) fn new(app: &App, options: SessionModelSwitchOptions) -> Option<Self> {
        let provider = options.provider;
        let mut models = provider_models(app, provider);
        if let Some(current) = options.model.as_deref()
            && !models.iter().any(|(id, _)| id == current)
        {
            models.insert(0, (current.to_string(), current.to_string()));
        }
        models.retain(|(id, _)| {
            rsi_common::launch_allowlist::launch_model_allowed(
                &options.model_allowlist,
                Some(provider),
                Some(id),
            )
        });
        if models.is_empty() {
            return None;
        }
        let model_index = options
            .model
            .as_deref()
            .and_then(|current| models.iter().position(|(id, _)| id == current))
            .unwrap_or(0);
        let efforts = effort_choices(app, provider, &models[model_index].0);
        let effort_index = options
            .effort
            .as_deref()
            .and_then(|current| {
                efforts
                    .iter()
                    .position(|choice| choice.as_deref() == Some(current))
            })
            .unwrap_or(0);
        Some(Self {
            session_id: options.session_id,
            options,
            models,
            model_index,
            efforts,
            effort_index,
        })
    }

    /// The `(model, effort)` the operator has highlighted.
    pub(crate) fn chosen(&self) -> (&str, Option<&str>) {
        (
            self.models[self.model_index].0.as_str(),
            self.efforts
                .get(self.effort_index)
                .and_then(|choice| choice.as_deref()),
        )
    }

    /// Whether the highlighted tuple is what the session already runs.
    fn is_current(&self) -> bool {
        let (model, effort) = self.chosen();
        self.options.model.as_deref() == Some(model) && self.options.effort.as_deref() == effort
    }
}

/// The provider's selectable models: the discovered list when the picker's
/// provider is the launch provider, else the static catalog.
fn provider_models(app: &App, provider: SessionProvider) -> Vec<(String, String)> {
    if provider == app.selected_provider
        && app.custom_provider_index.is_none()
        && !app.available_models.is_empty()
    {
        app.available_models.clone()
    } else {
        crate::app::models_for_provider(provider)
    }
}

/// Effort rows for `model`: the model default first, then every level both
/// the model's ladder and the provider's switchable levels allow.
fn effort_choices(app: &App, provider: SessionProvider, model: &str) -> Vec<Option<String>> {
    let allowed = rsi_common::model_utils::session_switch_effort_levels(provider).unwrap_or(&[]);
    std::iter::once(None)
        .chain(
            app.model_effort_ladder(provider, model)
                .into_iter()
                .filter(|level| allowed.contains(level))
                .map(|level| Some(level.to_string())),
        )
        .collect()
}

/// `"opus-5 · xhigh"`, or `"opus-5 · default effort"` without an effort.
pub(crate) fn tuple_label(model: &str, effort: Option<&str>) -> String {
    format!(
        "{} \u{00B7} {}",
        crate::ui::session::abbreviate_model_name(model),
        effort.unwrap_or("default effort")
    )
}

/// Open the picker for the focused session. A session that cannot switch
/// (unsupported provider, no invocation yet, not running or terminal) gets the
/// daemon's reason as an error toast instead of an overlay.
pub async fn open_model_switch_picker(app: &mut App) {
    let Some(session_id) = app.selected_session_id() else {
        app.notify("No session selected");
        return;
    };
    let options = match app
        .client
        .get_session_model_switch_options(session_id)
        .await
    {
        Ok(options) => options,
        Err(error) => {
            app.notify_error(format!("Model switch unavailable: {error}"));
            return;
        }
    };
    record_pending(app, session_id, options.pending.clone());
    if !options.switchable {
        let reason = options
            .unavailable_reason
            .clone()
            .unwrap_or_else(|| "model switch unavailable for this session".to_string());
        app.notify_error(reason);
        return;
    }
    let Some(state) = ModelSwitchState::new(app, options) else {
        app.notify_error("No models are available to switch to for this session");
        return;
    };
    app.overlay = OverlayState::ModelSwitch(Box::new(state));
    app.mark_dirty();
}

/// Keep the status line's queued-switch marker in step with the daemon.
fn record_pending(app: &mut App, session_id: Uuid, pending: Option<PendingSessionModelUpdate>) {
    match pending {
        Some(pending) => {
            app.pending_model_switches.insert(session_id, pending);
        }
        None => {
            app.pending_model_switches.remove(&session_id);
        }
    }
}

/// Re-derive the effort rows after the model highlight moved, keeping the
/// highlighted effort when the new model also offers it.
fn refresh_efforts(app: &mut App) {
    let OverlayState::ModelSwitch(state) = &app.overlay else {
        return;
    };
    let provider = state.options.provider;
    let model = state.models[state.model_index].0.clone();
    let kept = state.efforts.get(state.effort_index).cloned().flatten();
    let efforts = effort_choices(app, provider, &model);
    let effort_index = kept
        .as_ref()
        .and_then(|level| {
            efforts
                .iter()
                .position(|choice| choice.as_deref() == Some(level.as_str()))
        })
        .unwrap_or(0);
    if let OverlayState::ModelSwitch(state) = &mut app.overlay {
        state.efforts = efforts;
        state.effort_index = effort_index;
    }
}

/// Keys: `j`/`k`/`g`/`G` pick the model, `h`/`l` step the effort, `Enter`
/// queues the switch, `Esc`/`q` cancels.
pub(super) async fn handle_model_switch_key(app: &mut App, key: KeyEvent) {
    let moved_model = if let OverlayState::ModelSwitch(state) = &mut app.overlay {
        let count = state.models.len();
        let before = state.model_index;
        list::handle_list_nav_key(&mut state.model_index, count, &key)
            .then_some(before != state.model_index)
    } else {
        return;
    };
    if let Some(changed) = moved_model {
        if changed {
            refresh_efforts(app);
        }
        app.mark_dirty();
        return;
    }
    match key.code {
        KeyCode::Char('h') | KeyCode::Left => step_effort(app, false),
        KeyCode::Char('l') | KeyCode::Right => step_effort(app, true),
        KeyCode::Enter => submit_model_switch(app).await,
        KeyCode::Esc | KeyCode::Char('q') => {
            app.overlay = OverlayState::None;
            app.mark_dirty();
        }
        _ => {}
    }
}

fn step_effort(app: &mut App, forward: bool) {
    if let OverlayState::ModelSwitch(state) = &mut app.overlay {
        let last = state.efforts.len().saturating_sub(1);
        state.effort_index = if forward {
            (state.effort_index + 1).min(last)
        } else {
            state.effort_index.saturating_sub(1)
        };
        app.mark_dirty();
    }
}

async fn submit_model_switch(app: &mut App) {
    let OverlayState::ModelSwitch(state) = &app.overlay else {
        return;
    };
    let session_id = state.session_id;
    let Some(fence) = state.options.model_invocation_id else {
        app.notify_error("Model switch unavailable: no model invocation to fence it");
        return;
    };
    let (model, effort) = state.chosen();
    let (model, effort) = (model.to_string(), effort.map(str::to_string));
    let unchanged = state.is_current() && state.options.pending.is_none();
    app.overlay = OverlayState::None;
    app.mark_dirty();
    if unchanged {
        app.notify(format!(
            "Already running {}",
            tuple_label(&model, effort.as_deref())
        ));
        return;
    }
    match app
        .client
        .queue_session_model_update(session_id, fence, &model, effort.as_deref(), Uuid::new_v4())
        .await
    {
        Ok(receipt) => {
            let label = tuple_label(&model, effort.as_deref());
            if receipt["state"] == "applied" {
                app.pending_model_switches.remove(&session_id);
                app.notify(format!("Already running {label}"));
            } else {
                app.pending_model_switches.insert(
                    session_id,
                    PendingSessionModelUpdate {
                        model: model.clone(),
                        effort: effort.clone(),
                    },
                );
                app.notify_success(format!("Switching to {label} from the next turn"));
            }
        }
        Err(error) => app.notify_error(format!("Model switch failed: {error}")),
    }
}

#[cfg(test)]
#[path = "model_switch_tests.rs"]
mod tests;
