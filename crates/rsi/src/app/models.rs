//! Model discovery operations for App.

use super::App;
use rsi_common::{model_utils, types::SessionProvider};

/// Provider, custom endpoint and model of the launch default. Compared around
/// an edit to tell whether the default actually moved; effort follows the
/// model, so it is not part of the identity.
pub(crate) type DefaultModelIdentity = (SessionProvider, Option<usize>, Option<String>);

/// What the top bar shows about the model new sessions launch with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DefaultModelBadge {
    pub provider: SessionProvider,
    /// Compact model label (the list's MODEL vocabulary), or the provider name
    /// when no model is pinned and the provider picks its own default.
    pub model: String,
    /// Effort the launch uses and whether the operator chose it (`true`) or
    /// it is the model's own default (`false`).
    pub effort: Option<(String, bool)>,
}

impl DefaultModelBadge {
    /// Plain-text form, `✻ sonnet-5 · xhigh`, for toasts and assertions.
    pub(crate) fn text(&self) -> String {
        let glyph = crate::ui::glyphs::provider_glyph(self.provider);
        match &self.effort {
            Some((effort, _)) => format!("{glyph} {} · {effort}", self.model),
            None => format!("{glyph} {}", self.model),
        }
    }
}

impl App {
    /// Effort a launch of `model` on `provider` uses: the operator's selection
    /// when the model's ladder offers it (`true`), else the model's own
    /// default (`false`). `None` when the model takes no effort setting.
    pub(crate) fn resolved_launch_effort(
        &self,
        provider: SessionProvider,
        model: &str,
    ) -> Option<(String, bool)> {
        let ladder = self.model_effort_ladder(provider, model);
        if ladder.is_empty() {
            return None;
        }
        if let Some(effort) = self
            .selected_effort
            .as_deref()
            .filter(|effort| ladder.contains(effort))
        {
            return Some((effort.to_string(), true));
        }
        self.model_default_effort(provider, model)
            .filter(|effort| ladder.contains(effort))
            .map(|effort| (effort.to_string(), false))
    }

    /// The launch default as the top bar renders it.
    pub(crate) fn default_model_badge(&self) -> DefaultModelBadge {
        let provider = self.selected_provider;
        match self
            .selected_model
            .as_deref()
            .filter(|model| !model.is_empty())
        {
            Some(model) => DefaultModelBadge {
                provider,
                model: crate::ui::glyphs::list_model_label(model).to_string(),
                effort: self.resolved_launch_effort(provider, model),
            },
            None => DefaultModelBadge {
                provider,
                model: Self::provider_label(provider).to_string(),
                effort: None,
            },
        }
    }

    /// The default model when `dropdown` shows the default's own provider and
    /// endpoint, so the picker checks it there and nowhere else. Another
    /// provider's catalog is only a preview: Pioneer also serves `claude-*`
    /// IDs, and those are not what new sessions launch with.
    pub(crate) fn default_model_in(
        &self,
        dropdown: &crate::types::ModelDropdownState,
    ) -> Option<&str> {
        let showing_default_catalog = dropdown.provider == self.selected_provider
            && dropdown.custom_provider_index == self.custom_provider_index;
        showing_default_catalog
            .then_some(self.selected_model.as_deref())
            .flatten()
    }

    pub(crate) fn default_model_identity(&self) -> DefaultModelIdentity {
        (
            self.selected_provider,
            self.custom_provider_index,
            self.selected_model.clone(),
        )
    }

    /// Make `model` on `provider` the default for new launches. The one commit
    /// path for the global model picker (`Ctrl-M`) and Settings ▸ Model Roles:
    /// both leave the default untouched while the operator only browses
    /// providers, and land provider, endpoint, catalog and model together on
    /// a pick.
    pub(crate) fn set_default_model(
        &mut self,
        provider: SessionProvider,
        custom_provider_index: Option<usize>,
        models: Vec<(String, String)>,
        model: String,
    ) {
        let before = self.default_model_identity();
        self.selected_provider = provider;
        self.custom_provider_index = custom_provider_index;
        if !models.is_empty() {
            self.available_models = models;
        }
        self.reconcile_model_effort(provider, &model);
        self.selected_model = Some(model);
        self.finish_default_model_change(&before);
    }

    /// Persist a launch default that moved away from `before` and confirm it
    /// with a toast, so the change is visible the moment it lands (the
    /// top-bar chip shows it from the next frame on).
    pub(crate) fn finish_default_model_change(&mut self, before: &DefaultModelIdentity) {
        if self.default_model_identity() == *before {
            return;
        }
        crate::state::PersistedState::capture(self).save();
        let summary = self.default_model_badge().text();
        self.notify(format!("Default model {summary}"));
    }

    pub(crate) fn model_effort_bar_counts(
        &self,
        model_override: Option<&str>,
        override_provider: SessionProvider,
    ) -> (usize, usize) {
        let Some(model) = model_override.or(self.selected_model.as_deref()) else {
            return (0, 0);
        };
        let provider = if model_override.is_some() {
            override_provider
        } else {
            self.selected_provider
        };
        crate::ui::session::effort_bar_counts_for_ladder(
            self.selected_effort.as_deref(),
            &self.model_effort_ladder(provider, model),
            self.model_default_effort(provider, model),
        )
    }

    pub(crate) fn model_effort_ladder(&self, provider: SessionProvider, model: &str) -> Vec<&str> {
        if let Some(capabilities) = self
            .model_effort_capabilities
            .get(&provider)
            .and_then(|models| models.iter().find(|entry| entry.model == model))
        {
            return capabilities
                .supported_efforts
                .iter()
                .map(String::as_str)
                .collect();
        }
        model_utils::effort_ladder(model).to_vec()
    }

    pub(crate) fn model_default_effort(
        &self,
        provider: SessionProvider,
        model: &str,
    ) -> Option<&str> {
        if let Some(capabilities) = self
            .model_effort_capabilities
            .get(&provider)
            .and_then(|models| models.iter().find(|entry| entry.model == model))
        {
            return capabilities.default_effort.as_deref();
        }
        model_utils::default_effort_level(model)
    }

    pub(crate) fn reconcile_model_effort(&mut self, provider: SessionProvider, model: &str) {
        if self
            .selected_effort
            .as_deref()
            .is_some_and(|effort| !self.model_effort_ladder(provider, model).contains(&effort))
        {
            self.selected_effort = None;
        }
    }

    /// Get available models (dynamic if discovered, static as fallback).
    pub fn get_available_models(&self) -> &[(String, String)] {
        &self.available_models
    }
}
