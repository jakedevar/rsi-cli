//! "Provider Keys" settings category — read-only row derivation (#694 K1b).
//!
//! Reads over `App::cached_provider_credentials`
//! (`rsi_common::provider_credentials::ListProviderCredentialsResult`),
//! mirroring `model_control_budgets.rs`'s shape. The write side (set,
//! rotate, clear, check, import) lives in
//! `crate::overlay::provider_credential_form` and
//! `crate::action_handler::provider_credentials`; this module owns only the
//! shared row-label/value formatting so the settings-pane row list and the
//! set/rotate form never drift from each other.

use crate::app::App;
use rsi_common::provider_credentials::{
    CliExposure, CredentialCheckClass, CredentialCheckMetadata, CredentialRoute, CredentialState,
    ProviderCredentialMetadata, ProviderCredentialSlot,
};

pub(crate) const fn state_label(state: CredentialState) -> &'static str {
    match state {
        CredentialState::Vault => "vault",
        CredentialState::EnvCompat => "env_compat",
        CredentialState::Generator => "generator",
        CredentialState::Cleared => "cleared",
        CredentialState::Absent => "absent",
    }
}

pub(crate) const fn route_label(route: CredentialRoute) -> &'static str {
    match route {
        CredentialRoute::CodexCli => "codex_cli",
        CredentialRoute::Harness => "harness",
    }
}

pub(crate) const fn cli_exposure_label(exposure: CliExposure) -> &'static str {
    match exposure {
        CliExposure::Always => "always",
        CliExposure::OnFallback => "on_fallback",
        CliExposure::None => "none",
    }
}

pub(crate) const fn check_class_label(class: CredentialCheckClass) -> &'static str {
    match class {
        CredentialCheckClass::Valid => "valid",
        CredentialCheckClass::Invalid => "invalid",
        CredentialCheckClass::Exhausted => "exhausted",
        CredentialCheckClass::Unknown => "unknown",
    }
}

/// Format an age (now minus a past instant) as a compact `NNs`/`NNm`/`NNh`/`NNd`.
fn format_age(age: chrono::Duration) -> String {
    let secs = age.num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Format a check's class plus its age, e.g. `valid(2m)`. `now` is injected
/// for testability (no wall-clock read inside this pure function).
pub(crate) fn format_check(
    check: Option<&CredentialCheckMetadata>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    check.map_or_else(
        || "no check".to_string(),
        |c| format!("{}({})", check_class_label(c.class), format_age(now - c.at)),
    )
}

pub(crate) fn format_fingerprint(fingerprint: Option<&str>) -> &str {
    fingerprint.unwrap_or("-")
}

pub(crate) fn format_credit(credit: Option<f64>) -> String {
    credit.map_or_else(|| "-".to_string(), |v| format!("{v:.2}"))
}

/// Compose the full one-line row value: state, fingerprint, check
/// class+age, credit remaining, route, and CLI exposure — the exact field
/// set `ListProviderCredentials` returns for the slot (#694 design note,
/// "K: Key vault" / "TUI" bullet).
pub(crate) fn format_credential_value(meta: &ProviderCredentialMetadata) -> String {
    let credit = meta.check.as_ref().and_then(|c| c.credit_remaining);
    [
        state_label(meta.state).to_string(),
        format!("fp:{}", format_fingerprint(meta.fingerprint.as_deref())),
        format!(
            "check:{}",
            format_check(meta.check.as_ref(), chrono::Utc::now())
        ),
        format!("credit:{}", format_credit(credit)),
        format!("route:{}", route_label(meta.route)),
        format!("cli:{}", cli_exposure_label(meta.cli_exposure)),
    ]
    .join("  ")
}

/// Look up the cached metadata for one slot. `None` before the first
/// `ListProviderCredentials` response, or if the daemon's response omitted
/// the slot (should not happen — the slot set is closed and the daemon
/// always returns all of them, but this stays defensive).
pub(crate) fn credential_metadata(
    app: &App,
    slot: ProviderCredentialSlot,
) -> Option<&ProviderCredentialMetadata> {
    app.cached_provider_credentials
        .as_ref()
        .and_then(|list| list.credentials.iter().find(|meta| meta.slot == slot))
}

/// One (label, value) row per credential slot. Row count is the fixed
/// 23-slot enum (`ProviderCredentialSlot::ALL`) — unlike Budgets, this list
/// is never empty and never grows/shrinks with daemon state. Before the
/// first `ListProviderCredentials` response (or if a slot is somehow
/// missing from it), the row shows a loading placeholder instead of
/// asserting any real state.
pub(crate) fn provider_credential_rows(app: &App) -> Vec<(String, String)> {
    ProviderCredentialSlot::ALL
        .iter()
        .map(|slot| {
            let label = slot.as_str().to_string();
            let value = credential_metadata(app, *slot)
                .map_or_else(|| "loading…".to_string(), format_credential_value);
            (label, value)
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use rsi_common::provider_credentials::{
        CredentialCheckMetadata, ListProviderCredentialsResult,
    };

    /// `format_credential_value` always reads the real wall clock (it has no
    /// injected-`now` parameter — only the lower-level `format_check` does),
    /// so this helper anchors `check.at` off real "now" rather than a fixed
    /// timestamp to keep the age assertion below correct regardless of when
    /// the suite runs.
    fn vault_meta(slot: ProviderCredentialSlot) -> ProviderCredentialMetadata {
        let now = chrono::Utc::now();
        ProviderCredentialMetadata {
            slot,
            state: CredentialState::Vault,
            fingerprint: Some("ab12ef34".to_string()),
            set_at: Some(now),
            rotated_from_fingerprint: None,
            cleared_at: None,
            check: Some(CredentialCheckMetadata {
                generation: 1,
                at: now - chrono::Duration::minutes(2),
                class: CredentialCheckClass::Valid,
                http_status: Some(200),
                credit_remaining: Some(12.5),
                detail_code: "ok".to_string(),
                fingerprint: Some("ab12ef34".to_string()),
            }),
            generation: 1,
            route: CredentialRoute::CodexCli,
            cli_exposure: CliExposure::Always,
            last_cli_exposure_at: None,
        }
    }

    #[test]
    fn format_credential_value_shows_positive_state_fingerprint_and_exposure() {
        let meta = vault_meta(ProviderCredentialSlot::Openrouter);
        let value = format_credential_value(&meta);
        assert!(value.contains("vault"), "{value}");
        assert!(value.contains("fp:ab12ef34"), "{value}");
        assert!(value.contains("check:valid(2m)"), "{value}");
        assert!(value.contains("credit:12.50"), "{value}");
        assert!(value.contains("route:codex_cli"), "{value}");
        assert!(value.contains("cli:always"), "{value}");
    }

    #[test]
    fn format_credential_value_absent_slot_shows_absent_and_placeholders() {
        let meta = ProviderCredentialMetadata {
            slot: ProviderCredentialSlot::Anthropic,
            state: CredentialState::Absent,
            fingerprint: None,
            set_at: None,
            rotated_from_fingerprint: None,
            cleared_at: None,
            check: None,
            generation: 0,
            route: CredentialRoute::Harness,
            cli_exposure: CliExposure::None,
            last_cli_exposure_at: None,
        };
        let value = format_credential_value(&meta);
        assert!(value.contains("absent"), "{value}");
        assert!(value.contains("fp:-"), "{value}");
        assert!(value.contains("check:no check"), "{value}");
        assert!(value.contains("route:harness"), "{value}");
        assert!(value.contains("cli:none"), "{value}");
    }

    #[test]
    fn provider_credential_rows_covers_every_slot_with_loading_placeholder() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.cached_provider_credentials = None;

        let rows = provider_credential_rows(&app);
        assert_eq!(rows.len(), ProviderCredentialSlot::ALL.len());
        assert!(rows.iter().all(|(_, value)| value == "loading…"));
    }

    #[test]
    fn provider_credential_rows_reflects_populated_cache() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.cached_provider_credentials = Some(ListProviderCredentialsResult {
            env_compat: true,
            check_ttl_secs: 600,
            credentials: vec![vault_meta(ProviderCredentialSlot::Openrouter)],
        });

        let rows = provider_credential_rows(&app);
        let (label, value) = rows
            .iter()
            .find(|(label, _)| label == "openrouter")
            .expect("openrouter row present");
        assert_eq!(label, "openrouter");
        assert!(value.contains("vault"), "{value}");

        // Every other slot is still present, still showing the loading
        // placeholder (not silently dropped from the fixed 23-row list).
        let anthropic = rows
            .iter()
            .find(|(label, _)| label == "anthropic")
            .expect("anthropic row present");
        assert_eq!(anthropic.1, "loading…");
    }
}
