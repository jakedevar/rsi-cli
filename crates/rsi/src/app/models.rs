//! Model discovery operations for App.

use super::App;
use rsi_common::{model_utils, types::SessionProvider};

impl App {
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
