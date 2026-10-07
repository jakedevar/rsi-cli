//! The TUI's view of the operator provider profile (Issue #1407).
//!
//! The daemon enforces `provider_profile` on every launch; the TUI only
//! filters its pickers so an `aws_only` operator is offered Claude on Bedrock
//! Claude models and nothing else. The value is mirrored from the daemon
//! config (`App::apply_authoritative_daemon_config` and the settings-row
//! toggle) into a thread-local, because the picker helpers
//! (`models_for_provider`, `ModelDropdownState`) take no `App`. The TUI event
//! loop is single-threaded (`tokio::main(flavor = "current_thread")`), and a
//! thread-local keeps parallel unit tests from seeing each other's profile.

use std::cell::Cell;

use rsi_common::provider_profile::{PROVIDER_PROFILE_FIELD, ProviderProfile};
use rsi_common::types::SessionProvider;

use crate::settings::{DaemonFeatureEntry, DaemonFeatureValue};

thread_local! {
    static PROFILE: Cell<ProviderProfile> = const { Cell::new(ProviderProfile::All) };
}

/// The profile the pickers filter by.
#[must_use]
pub fn current() -> ProviderProfile {
    PROFILE.with(Cell::get)
}

/// Replace the mirrored profile; returns whether it changed.
pub fn set(profile: ProviderProfile) -> bool {
    PROFILE.with(|cell| cell.replace(profile) != profile)
}

/// Mirror the profile from the daemon-features rows (the settings row is the
/// `provider_profile` cycle). Returns whether it changed.
pub fn sync_from_features(entries: &[DaemonFeatureEntry]) -> bool {
    let profile = entries
        .iter()
        .find(|entry| entry.field == PROVIDER_PROFILE_FIELD)
        .and_then(|entry| match &entry.value {
            DaemonFeatureValue::Cycle { options, current } => options.get(*current),
            DaemonFeatureValue::Display(text) => Some(text),
            DaemonFeatureValue::Bool(_) => None,
        })
        .and_then(|text| ProviderProfile::parse(text))
        .unwrap_or_default();
    set(profile)
}

/// Whether the pickers offer `provider` under the current profile.
#[must_use]
pub fn provider_offered(provider: SessionProvider) -> bool {
    rsi_common::provider_profile::provider_offered(current(), provider)
}

/// Whether the pickers offer `model` on `provider` under the current profile.
#[must_use]
pub fn model_offered(provider: SessionProvider, model: &str) -> bool {
    rsi_common::provider_profile::picker_allows(current(), provider, model)
}

/// Whether user-defined (custom/local) providers are offered.
#[must_use]
pub fn custom_providers_offered() -> bool {
    current() == ProviderProfile::All
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_reads_the_settings_row_and_defaults_to_all() {
        let mut entries = DaemonFeatureEntry::defaults();
        assert!(!sync_from_features(&entries), "default stays all");
        assert_eq!(current(), ProviderProfile::All);
        DaemonFeatureEntry::update_from_json(
            &mut entries,
            &serde_json::json!({ "provider_profile": "aws_only" }),
        );
        assert!(sync_from_features(&entries));
        assert_eq!(current(), ProviderProfile::AwsOnly);
        assert!(provider_offered(SessionProvider::Claude));
        assert!(!provider_offered(SessionProvider::Codex));
        assert!(!custom_providers_offered());
        set(ProviderProfile::All);
    }

    /// The pickers show only Bedrock Claude models under `aws_only`: the
    /// static lists, discovered lists (dropdown filter) and provider cycling.
    #[test]
    fn aws_only_filters_the_pickers_and_all_keeps_them() {
        use crate::types::ModelDropdownState;
        set(ProviderProfile::All);
        let codex_all = crate::app::models_for_provider(SessionProvider::Codex);
        assert!(!codex_all.is_empty(), "`all` keeps the Codex catalog");
        let claude_all = crate::app::models_for_provider(SessionProvider::Claude);

        set(ProviderProfile::AwsOnly);
        assert!(crate::app::models_for_provider(SessionProvider::Codex).is_empty());
        let claude = crate::app::models_for_provider(SessionProvider::Claude);
        assert!(!claude.is_empty());
        assert!(
            claude
                .iter()
                .all(|(id, _)| rsi_common::provider_profile::is_bedrock_claude_model(id)),
            "{claude:?}"
        );

        // A discovered list mixing Anthropic-API and Bedrock ids shows only
        // the Bedrock Claude entries.
        let mut discovered = claude_all.clone();
        discovered.push((
            "us.anthropic.claude-opus-5-5-v1:0".to_string(),
            "Bedrock Opus".to_string(),
        ));
        let state = ModelDropdownState::new(SessionProvider::Claude, discovered.clone(), None);
        let visible: Vec<&str> = state
            .filtered_indices()
            .into_iter()
            .map(|index| discovered[index].0.as_str())
            .collect();
        assert_eq!(visible, vec!["us.anthropic.claude-opus-5-5-v1:0"]);

        // Provider cycling stays on Claude and Bedrock.
        let mut state = ModelDropdownState::new(SessionProvider::Claude, claude, None);
        let mut seen = Vec::new();
        for _ in 0..4 {
            crate::widget::model_dropdown::cycle_provider(&mut state, true, &[]);
            seen.push(state.provider);
        }
        assert!(
            seen.iter().all(|provider| matches!(
                provider,
                SessionProvider::Claude | SessionProvider::Bedrock
            )),
            "{seen:?}"
        );
        assert!(seen.contains(&SessionProvider::Bedrock));

        set(ProviderProfile::All);
        let state = ModelDropdownState::new(SessionProvider::Claude, claude_all.clone(), None);
        assert_eq!(state.filtered_indices().len(), claude_all.len());
    }
}
