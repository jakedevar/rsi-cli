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
use rsi_common::provider_contract::{
    PROVIDER_CONTRACT_NAMES, ProviderContractRowV1, ProviderContractState,
};
use rsi_common::provider_credentials::{
    CliExposure, CredentialCheckClass, CredentialCheckMetadata, CredentialRoute, CredentialState,
    ProviderCredentialMetadata, ProviderCredentialSlot,
};
use rsi_common::provider_login::{PROVIDER_LOGIN_NAMES, ProviderLoginRowV1, ProviderLoginState};

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

/// Rows in the Provider Keys list: one per credential slot, then one login
/// row per probed provider CLI (`claude`, `codex`, #2753), then one CLI
/// contract row per checked provider CLI (#2755). Fixed, like the slot set.
pub(crate) const fn provider_key_row_count() -> usize {
    login_row_end() + PROVIDER_CONTRACT_NAMES.len()
}

/// Index one past the last login row (where the contract rows start).
pub(crate) const fn login_row_end() -> usize {
    ProviderCredentialSlot::ALL.len() + PROVIDER_LOGIN_NAMES.len()
}

/// The cached contract row of `provider`, if the first response arrived.
fn contract_row<'a>(app: &'a App, provider: &str) -> Option<&'a ProviderContractRowV1> {
    app.cached_provider_contract
        .as_ref()
        .and_then(|result| result.providers.iter().find(|row| row.provider == provider))
}

/// The one-line value of a contract row: state, the reason code when there is
/// one, the degraded detail line when there is one, the CLI version and the check age, then what a degraded Codex custody
/// hook does to launches. The Codex row also shows the model catalog's trust
/// and reason (#2761). Nothing here comes from the CLIs beyond the facts the
/// daemon serves.
pub(crate) fn format_contract_value(
    row: &ProviderContractRowV1,
    refuse_untrusted_custody_hook: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let contract = &row.contract;
    let mut parts = vec![contract.contract_state.as_str().to_string()];
    if let Some(reason) = &contract.reason {
        parts.push(format!("reason:{reason}"));
    }
    if let Some(detail) = &contract.detail {
        // The failing contract element, one bounded line (hub #2981).
        parts.push(format!("detail:{detail}"));
    }
    parts.push(
        contract
            .cli_version
            .clone()
            .unwrap_or_else(|| "version unknown".to_string()),
    );
    parts.push(contract.checked_at.map_or_else(
        || "never checked".to_string(),
        |at| format!("checked {} ago", format_age(now - at)),
    ));
    if let Some(catalog) = &contract.catalog {
        // The Codex model catalog's trust and why (#2761).
        parts.push(format!(
            "catalog:{} reason:{}",
            catalog.trust.as_str(),
            catalog.reason
        ));
    }
    if contract.custody_failed() {
        parts.push(if refuse_untrusted_custody_hook {
            "Codex launches are refused".to_string()
        } else {
            "Codex launches run without custody".to_string()
        });
    }
    parts.join("  ")
}

/// The (label, value) of the contract row for `provider`. Before the first
/// `GetProviderContractState` response the row shows a loading placeholder.
fn provider_contract_row(
    app: &App,
    provider: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> (String, String) {
    let refuse = app
        .cached_provider_contract
        .as_ref()
        .is_none_or(|result| result.codex_refuse_untrusted_custody_hook);
    let value = contract_row(app, provider).map_or_else(
        || "loading…".to_string(),
        |row| format_contract_value(row, refuse, now),
    );
    (format!("{provider} contract"), value)
}

/// The state of the contract row at `index` (counted from the first contract
/// row), for the row badge.
pub(crate) fn contract_row_state(app: &App, index: usize) -> Option<ProviderContractState> {
    let provider = PROVIDER_CONTRACT_NAMES.get(index)?;
    contract_row(app, provider).map(|row| row.contract.contract_state)
}

pub(crate) fn login_state_label(state: ProviderLoginState) -> &'static str {
    state.as_str()
}

/// The cached login row of `provider`, if the first response arrived.
fn login_row<'a>(app: &'a App, provider: &str) -> Option<&'a ProviderLoginRowV1> {
    app.cached_provider_login
        .as_ref()
        .and_then(|result| result.providers.iter().find(|row| row.provider == provider))
}

/// The one-line value of a login row: state, method and check age, then the
/// host-side fix when logged out (`unknown` says launches are not refused),
/// then each satellite's own state. Nothing here comes from the CLI's output
/// beyond the three facts the daemon serves.
pub(crate) fn format_login_value(
    row: &ProviderLoginRowV1,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let login = &row.login;
    let age = login.checked_at.map_or_else(
        || "never probed".to_string(),
        |at| format!("checked {} ago", format_age(now - at)),
    );
    let mut parts = vec![
        login_state_label(login.state).to_string(),
        format!("method:{}", login.method.as_str()),
        age,
    ];
    match login.state {
        ProviderLoginState::LoggedOut => parts.push(format!("fix: {}", row.remedy)),
        ProviderLoginState::Unknown => parts.push("launches are not refused".to_string()),
        ProviderLoginState::LoggedIn => {}
    }
    for host in &login.hosts {
        parts.push(format!("{}:{}", host.host, login_state_label(host.state)));
    }
    parts.join("  ")
}

/// The (label, value) of the login row for `provider`. Before the first
/// `GetProviderLoginState` response the row shows a loading placeholder.
fn provider_login_row(
    app: &App,
    provider: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> (String, String) {
    let value = login_row(app, provider).map_or_else(
        || "loading…".to_string(),
        |row| format_login_value(row, now),
    );
    (format!("{provider} login"), value)
}

/// The state of the login row at `index` (counted from the first login row),
/// for the row badge.
pub(crate) fn login_row_state(app: &App, index: usize) -> Option<ProviderLoginState> {
    let provider = PROVIDER_LOGIN_NAMES.get(index)?;
    login_row(app, provider).map(|row| row.login.state)
}

/// One (label, value) row per credential slot, then one per probed provider
/// CLI login, then one per checked provider CLI contract. Row count is fixed (`provider_key_row_count`) — unlike Budgets,
/// this list is never empty and never grows/shrinks with daemon state. Before
/// the first `ListProviderCredentials` / `GetProviderLoginState` response (or
/// if a slot is somehow missing from it), the row shows a loading placeholder
/// instead of asserting any real state.
pub(crate) fn provider_credential_rows(app: &App) -> Vec<(String, String)> {
    let now = chrono::Utc::now();
    ProviderCredentialSlot::ALL
        .iter()
        .map(|slot| {
            let label = slot.as_str().to_string();
            let value = credential_metadata(app, *slot)
                .map_or_else(|| "loading…".to_string(), format_credential_value);
            (label, value)
        })
        .chain(
            PROVIDER_LOGIN_NAMES
                .iter()
                .map(|provider| provider_login_row(app, provider, now)),
        )
        .chain(
            PROVIDER_CONTRACT_NAMES
                .iter()
                .map(|provider| provider_contract_row(app, provider, now)),
        )
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use rsi_common::provider_credentials::{
        CredentialCheckMetadata, ListProviderCredentialsResult,
    };
    use rsi_common::provider_login::login_remedy;

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
        assert_eq!(rows.len(), provider_key_row_count());
        assert_eq!(
            rows.len(),
            ProviderCredentialSlot::ALL.len()
                + PROVIDER_LOGIN_NAMES.len()
                + PROVIDER_CONTRACT_NAMES.len()
        );
        assert!(rows.iter().all(|(_, value)| value == "loading…"));
    }

    fn login_row_v1(
        provider: &str,
        state: ProviderLoginState,
        method: rsi_common::provider_login::ProviderLoginMethod,
        hosts: Vec<rsi_common::provider_login::ProviderHostLoginV1>,
    ) -> ProviderLoginRowV1 {
        ProviderLoginRowV1 {
            provider: provider.to_string(),
            login: rsi_common::provider_login::ProviderLoginV1 {
                state,
                method,
                checked_at: Some(chrono::Utc::now() - chrono::Duration::minutes(3)),
                hosts,
            },
            remedy: login_remedy(provider).to_string(),
        }
    }

    #[test]
    fn login_rows_show_state_method_age_and_the_host_side_fix() {
        use rsi_common::provider_login::{
            ProviderHostLoginV1, ProviderLoginMethod, ProviderLoginStateResultV1,
        };
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        let now = chrono::Utc::now();
        app.cached_provider_login = Some(ProviderLoginStateResultV1 {
            generated_at: now,
            cache_ttl_secs: 60,
            providers: vec![
                login_row_v1(
                    "claude",
                    ProviderLoginState::LoggedIn,
                    ProviderLoginMethod::ClaudeAi,
                    vec![ProviderHostLoginV1 {
                        host: "laptop".into(),
                        state: ProviderLoginState::LoggedOut,
                        method: ProviderLoginMethod::None,
                        checked_at: now,
                    }],
                ),
                login_row_v1(
                    "codex",
                    ProviderLoginState::LoggedOut,
                    ProviderLoginMethod::None,
                    Vec::new(),
                ),
            ],
        });
        let rows = provider_credential_rows(&app);
        let value_of = |label: &str| {
            rows.iter()
                .find(|(row_label, _)| row_label == label)
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| panic!("row {label} present"))
        };
        let claude = value_of("claude login");
        assert!(
            claude.starts_with("logged_in  method:claude.ai  checked 3m ago"),
            "{claude}"
        );
        assert!(claude.contains("laptop:logged_out"), "{claude}");
        let codex = value_of("codex login");
        assert!(codex.starts_with("logged_out  method:none"), "{codex}");
        assert!(codex.contains("`codex login`"), "{codex}");
        assert_eq!(
            login_row_state(&app, 1),
            Some(ProviderLoginState::LoggedOut)
        );

        let unknown = format_login_value(
            &login_row_v1(
                "codex",
                ProviderLoginState::Unknown,
                ProviderLoginMethod::Unknown,
                Vec::new(),
            ),
            now,
        );
        assert!(unknown.contains("launches are not refused"), "{unknown}");
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
        // placeholder (not silently dropped from the fixed row list).
        let anthropic = rows
            .iter()
            .find(|(label, _)| label == "anthropic")
            .expect("anthropic row present");
        assert_eq!(anthropic.1, "loading…");
    }
}
