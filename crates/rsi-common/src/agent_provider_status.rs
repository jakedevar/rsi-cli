//! `AgentGetProviderStatus` (#1044): a secret-free, bounded read of whether a
//! provider can take work (configured, reachable, remaining credit, recent
//! 402/429 and launch failures). The daemon makes any remote call with its own
//! credentials; nothing in these types can carry a key.

use crate::provider_credentials::CredentialCheckClass;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// `refusal_detail` of a launch refused by an open auth-failure episode.
pub const PROVIDER_AUTH_INVALID_DETAIL: &str = "auth_invalid";

/// Error code for an unknown `provider` filter.
pub const PROVIDER_STATUS_UNKNOWN_PROVIDER: &str = "provider_status_unknown_provider";

/// Closed set of provider names the verb reports. Session providers use their
/// snake_case name; `anthropic` and `openai` are the Harness API credential
/// slots (no session provider of their own).
pub const PROVIDER_STATUS_NAMES: [&str; 11] = [
    "claude",
    "codex",
    "pioneer",
    "openrouter",
    "bedrock",
    "local",
    "antigravity",
    "codex_app_server",
    "harness",
    "anthropic",
    "openai",
];

/// Window for the launch and failure counters.
pub const PROVIDER_STATUS_FAILURE_WINDOW_SECS: i64 = 24 * 60 * 60;
/// Lookback for the last 402/429 timestamps.
pub const PROVIDER_STATUS_LAST_ERROR_LOOKBACK_SECS: i64 = 7 * 24 * 60 * 60;
/// How long after an episode's last auth failure one probe launch is admitted
/// when no login-state reading ends it first (hub #2957; the same one-hour
/// shape as the #2735 model probe, but shorter because a re-login is cheap).
pub const PROVIDER_AUTH_PROBE_INTERVAL_SECS: i64 = 60 * 60;
/// The least time after an auth failure before a logged-in reading re-opens
/// admission, so a login the CLI reports as fine while the provider still
/// rejects its token costs at most one failed launch per this interval.
pub const PROVIDER_AUTH_LOGIN_RECHECK_MIN_SECS: i64 = 5 * 60;
/// Response cache TTL for the remote (network) portion.
pub const PROVIDER_STATUS_CACHE_TTL_SECS: u64 = 60;

/// Strict request; omit `provider` for every provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentGetProviderStatusRequestV1 {
    #[serde(default)]
    pub provider: Option<String>,
}

impl AgentGetProviderStatusRequestV1 {
    /// # Errors
    /// [`PROVIDER_STATUS_UNKNOWN_PROVIDER`] when `provider` is not a known name.
    pub fn validate(&self) -> Result<(), &'static str> {
        match self.provider.as_deref() {
            Some(name) if !PROVIDER_STATUS_NAMES.contains(&name) => {
                Err(PROVIDER_STATUS_UNKNOWN_PROVIDER)
            }
            _ => Ok(()),
        }
    }
}

/// Remaining credit as the provider API reports it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderCreditV1 {
    /// Per-key spend limit (`null` when the key is unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_limit: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_usage: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_limit_remaining: Option<f64>,
    /// Account-level purchased credits and usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_credits: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_usage: Option<f64>,
    /// `total_credits - total_usage` when both are known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_remaining: Option<f64>,
    pub checked_at: DateTime<Utc>,
}

/// Secret-free summary of the vault's last credential check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderCredentialCheckV1 {
    pub class: CredentialCheckClass,
    pub detail_code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    pub checked_at: DateTime<Utc>,
}

/// Stable code of a model the provider refused as unsupported or unavailable
/// (#2735).
pub const PROVIDER_MODEL_UNAVAILABLE_CODE: &str = "model_unavailable";

/// A model the provider refused as unsupported or unavailable (#2735). New
/// launches of it are refused up front with the same code until a later launch
/// of the model gets past startup or the probe window runs out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModelUnavailableV1 {
    pub model: String,
    /// Always [`PROVIDER_MODEL_UNAVAILABLE_CODE`].
    pub code: String,
    /// The first refusal of the current episode.
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    /// The refused session that opened the episode.
    pub evidence_session_id: String,
    /// The closed stop reason that session carries.
    pub evidence_stop_reason: String,
}

/// Where an auth-failure episode stands (hub #2957).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAuthEpisodeStateV1 {
    /// The CLI reads logged out: log in on the host. Admission stays refused.
    ReloginNeeded,
    /// Refused; nothing shows the credential works again. A logged-in reading
    /// taken after the last failure, or `next_probe_at`, re-opens admission.
    AwaitingProbe,
    /// A logged-in reading taken after the last failure ended the refusal;
    /// admission is open. A later success closes the episode.
    ClearedByLogin,
    /// The probe interval passed with no success: admission is open for one
    /// probe launch (a new auth failure refuses again for another interval).
    ProbeAdmitted,
}

/// An open auth-failure episode of one provider (hub #1610 / #2957).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAuthEpisodeV1 {
    /// The first auth failure after the latest launch that got past startup.
    pub started_at: DateTime<Utc>,
    pub last_failure_at: DateTime<Utc>,
    pub state: ProviderAuthEpisodeStateV1,
    /// When admission re-opens for a probe launch; absent when it needs a
    /// re-login or admission is already open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_probe_at: Option<DateTime<Utc>>,
}

impl ProviderAuthEpisodeV1 {
    /// The episode as the launch history alone shows it at `now`: refused until
    /// [`PROVIDER_AUTH_PROBE_INTERVAL_SECS`] after the last failure, then open.
    #[must_use]
    pub fn from_failures(
        started_at: DateTime<Utc>,
        last_failure_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Self {
        let probe_at =
            last_failure_at + chrono::Duration::seconds(PROVIDER_AUTH_PROBE_INTERVAL_SECS);
        let waiting = now < probe_at;
        Self {
            started_at,
            last_failure_at,
            state: if waiting {
                ProviderAuthEpisodeStateV1::AwaitingProbe
            } else {
                ProviderAuthEpisodeStateV1::ProbeAdmitted
            },
            next_probe_at: waiting.then_some(probe_at),
        }
    }

    /// Whether the episode refuses launches.
    #[must_use]
    pub fn refuses(&self) -> bool {
        matches!(
            self.state,
            ProviderAuthEpisodeStateV1::ReloginNeeded | ProviderAuthEpisodeStateV1::AwaitingProbe
        )
    }

    /// Fold in the host's CLI login reading (#2753). Logged out needs a
    /// re-login and no probe; logged in with a check time after the last
    /// failure (and [`PROVIDER_AUTH_LOGIN_RECHECK_MIN_SECS`] past it) ends the
    /// refusal; anything else leaves the time-based state alone.
    pub fn apply_login(
        &mut self,
        login: &crate::provider_login::ProviderLoginV1,
        now: DateTime<Utc>,
    ) {
        use crate::provider_login::ProviderLoginState;
        if login.state == ProviderLoginState::LoggedOut {
            self.state = ProviderAuthEpisodeStateV1::ReloginNeeded;
            self.next_probe_at = None;
            return;
        }
        let checked_after_failure = login.checked_at.is_some_and(|at| at > self.last_failure_at);
        if login.state != ProviderLoginState::LoggedIn || !checked_after_failure {
            return;
        }
        let reopen_at =
            self.last_failure_at + chrono::Duration::seconds(PROVIDER_AUTH_LOGIN_RECHECK_MIN_SECS);
        if now < reopen_at {
            self.state = ProviderAuthEpisodeStateV1::AwaitingProbe;
            self.next_probe_at = Some(reopen_at);
            return;
        }
        self.state = ProviderAuthEpisodeStateV1::ClearedByLogin;
        self.next_probe_at = None;
    }
}

/// One provider's status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderStatusEntryV1 {
    pub provider: String,
    /// Whether the daemon holds a usable credential; `null` when the daemon
    /// holds none for this provider (the CLI or a local server authenticates).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured: Option<bool>,
    /// The credential slot behind this provider, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_slot: Option<String>,
    /// `false` when the last remote check timed out or could not connect;
    /// `null` when the daemon makes no remote call for this provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reachable: Option<bool>,
    /// `open`, or `refused` when a launch would be refused up front.
    pub launch_admission: String,
    /// Stable detail code when `launch_admission` is `refused`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit: Option<ProviderCreditV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_check: Option<ProviderCredentialCheckV1>,
    /// Last credit-exhausted (402-class) failure inside the lookback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_402_at: Option<DateTime<Utc>>,
    /// Last rate-limited (429-class) failure inside the lookback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_429_at: Option<DateTime<Utc>>,
    /// Last auth-rejected (401-class) startup inside the lookback (#1610).
    /// `launch_admission` is `refused` with detail `auth_invalid` from the
    /// first such failure until a later launch of this provider gets past
    /// startup; the credential itself stays operator-owned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_auth_failure_at: Option<DateTime<Utc>>,
    /// The open auth-failure episode (hub #2957), present from its first
    /// failure until a later launch gets past startup. It names its start, its
    /// last failure, whether the provider needs a re-login or is waiting on a
    /// probe, and when the next probe launch is admitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_episode: Option<ProviderAuthEpisodeV1>,
    pub launches_24h: u32,
    pub failed_launches_24h: u32,
    /// `failed_launches_24h / launches_24h`, `null` with no launches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_rate_24h: Option<f64>,
    /// Models the provider refused as unsupported or unavailable (#2735);
    /// `launch_admission` stays `open` for the provider's other models.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models_unavailable: Vec<ProviderModelUnavailableV1>,
    /// CLI login state (#2753) on the `claude`, `codex` and `codex_app_server`
    /// entries: `logged_out` refuses launches up front with
    /// `provider_login_required`; `unknown` never does. Absent on every other
    /// provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<crate::provider_login::ProviderLoginV1>,
    /// CLI contract canary (#2755) on the `claude`, `codex` and
    /// `codex_app_server` entries: `degraded` means the installed CLI no longer
    /// honours something RSI relies on. Only a degraded Codex custody hook
    /// refuses launches (`codex` entry, `launch_admission: refused`). Absent on
    /// every other provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<crate::provider_contract::ProviderContractV1>,
}

impl ProviderStatusEntryV1 {
    /// Fold the CLI login reading into an open auth episode and move
    /// `launch_admission` with it (hub #2957): a cleared episode re-opens a
    /// launch the episode alone refused, a re-login-needed one refuses.
    fn apply_login_to_auth_episode(
        &mut self,
        login: &crate::provider_login::ProviderLoginV1,
        now: DateTime<Utc>,
    ) {
        let Some(episode) = self.auth_episode.as_mut() else {
            return;
        };
        episode.apply_login(login, now);
        let auth_refused = self.refusal_detail.as_deref() == Some(PROVIDER_AUTH_INVALID_DETAIL);
        if episode.refuses() && self.launch_admission == "open" {
            self.launch_admission = "refused".to_string();
            self.refusal_detail = Some(PROVIDER_AUTH_INVALID_DETAIL.to_string());
        } else if !episode.refuses() && auth_refused {
            self.launch_admission = "open".to_string();
            self.refusal_detail = None;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentGetProviderStatusResultV1 {
    pub generated_at: DateTime<Utc>,
    pub cache_ttl_secs: u64,
    /// At most `PROVIDER_STATUS_NAMES.len()` entries.
    pub providers: Vec<ProviderStatusEntryV1>,
    /// Issue #1407: the operator provider profile in force. Under `aws_only`
    /// every provider but `claude` and `bedrock` (Claude on a Bedrock Claude
    /// model) reports `launch_admission: refused` with
    /// `refusal_detail: provider_profile_refused`.
    #[serde(default)]
    pub provider_profile: crate::provider_profile::ProviderProfile,
}

/// The status-entry name of a canonical `SessionProvider` spelling.
fn provider_status_name(session_provider: &str) -> Option<&'static str> {
    Some(match session_provider {
        "Claude" => "claude",
        "Codex" => "codex",
        "Pioneer" => "pioneer",
        "OpenRouter" => "openrouter",
        "Bedrock" => "bedrock",
        "Local" => "local",
        "Antigravity" => "antigravity",
        "CodexAppServer" => "codex_app_server",
        "Harness" => "harness",
        _ => return None,
    })
}

impl AgentGetProviderStatusResultV1 {
    /// Attach the CLI contract canary state (#2755) to the entries that have
    /// one, each `(checked provider name, contract)`. The `codex` entry reports
    /// `launch_admission: refused` with the custody reason as `refusal_detail`
    /// when `refuse_untrusted_custody_hook` is on and the canary found the
    /// custody hook untrusted, unless something already refuses it.
    pub fn apply_provider_contract(
        &mut self,
        contracts: impl IntoIterator<Item = (String, crate::provider_contract::ProviderContractV1)>,
        refuse_untrusted_custody_hook: bool,
    ) {
        let contracts: Vec<_> = contracts.into_iter().collect();
        for entry in &mut self.providers {
            let Some(checked) =
                crate::provider_contract::contract_provider_for_status_name(&entry.provider)
            else {
                continue;
            };
            let Some((_, contract)) = contracts.iter().find(|(name, _)| name == checked) else {
                continue;
            };
            if entry.provider == "codex"
                && refuse_untrusted_custody_hook
                && contract.custody_failed()
                && entry.launch_admission == "open"
            {
                entry.launch_admission = "refused".to_string();
                entry.refusal_detail = contract.reason.clone();
            }
            entry.contract = Some(contract.clone());
        }
    }

    /// Attach the CLI login state (#2753) to the entries that have one, each
    /// `(probed provider name, login)`. A logged-out provider reports
    /// `launch_admission: refused` with `refusal_detail: login_required`
    /// unless something already refuses it; `unknown` changes nothing.
    pub fn apply_provider_login(
        &mut self,
        logins: impl IntoIterator<Item = (String, crate::provider_login::ProviderLoginV1)>,
    ) {
        let logins: Vec<_> = logins.into_iter().collect();
        let now = self.generated_at;
        for entry in &mut self.providers {
            let Some(probed) =
                crate::provider_login::login_provider_for_status_name(&entry.provider)
            else {
                continue;
            };
            let Some((_, login)) = logins.iter().find(|(name, _)| name == probed) else {
                continue;
            };
            entry.apply_login_to_auth_episode(login, now);
            if login.state == crate::provider_login::ProviderLoginState::LoggedOut
                && entry.launch_admission == "open"
            {
                entry.launch_admission = "refused".to_string();
                entry.refusal_detail =
                    Some(crate::provider_login::PROVIDER_LOGIN_REQUIRED_CODE.to_string());
            }
            entry.login = Some(login.clone());
        }
    }

    /// Attach the unavailable models, each `(canonical SessionProvider
    /// spelling, record)`, to the provider they belong to.
    pub fn apply_models_unavailable(
        &mut self,
        models: impl IntoIterator<Item = (String, ProviderModelUnavailableV1)>,
    ) {
        for (provider, record) in models {
            let Some(name) = provider_status_name(&provider) else {
                continue;
            };
            if let Some(entry) = self
                .providers
                .iter_mut()
                .find(|entry| entry.provider == name)
            {
                entry.models_unavailable.push(record);
            }
        }
    }

    /// Record `profile` and mark every provider it refuses as refused up
    /// front. `all` changes nothing.
    pub fn apply_provider_profile(&mut self, profile: crate::provider_profile::ProviderProfile) {
        self.provider_profile = profile;
        if profile == crate::provider_profile::ProviderProfile::All {
            return;
        }
        for entry in &mut self.providers {
            if !matches!(entry.provider.as_str(), "claude" | "bedrock") {
                entry.launch_admission = "refused".to_string();
                entry.refusal_detail =
                    Some(crate::provider_profile::PROVIDER_PROFILE_REFUSED.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aws_only_marks_every_provider_but_claude_and_bedrock_refused() {
        let entry = |provider: &str| ProviderStatusEntryV1 {
            provider: provider.to_string(),
            configured: None,
            credential_slot: None,
            reachable: None,
            launch_admission: "open".to_string(),
            refusal_detail: None,
            credit: None,
            credential_check: None,
            last_402_at: None,
            last_429_at: None,
            last_auth_failure_at: None,
            auth_episode: None,
            launches_24h: 0,
            failed_launches_24h: 0,
            failure_rate_24h: None,
            models_unavailable: Vec::new(),
            login: None,
            contract: None,
        };
        let mut report = AgentGetProviderStatusResultV1 {
            generated_at: Utc::now(),
            cache_ttl_secs: 60,
            providers: PROVIDER_STATUS_NAMES
                .iter()
                .map(|name| entry(name))
                .collect(),
            provider_profile: crate::provider_profile::ProviderProfile::All,
        };
        let unchanged = report.clone();
        report.apply_provider_profile(crate::provider_profile::ProviderProfile::All);
        assert_eq!(report, unchanged, "`all` changes nothing");

        report.apply_provider_profile(crate::provider_profile::ProviderProfile::AwsOnly);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["provider_profile"], "aws_only");
        for status in &report.providers {
            if matches!(status.provider.as_str(), "claude" | "bedrock") {
                assert_eq!(status.launch_admission, "open", "{}", status.provider);
            } else {
                assert_eq!(status.launch_admission, "refused", "{}", status.provider);
                assert_eq!(
                    status.refusal_detail.as_deref(),
                    Some(crate::provider_profile::PROVIDER_PROFILE_REFUSED)
                );
            }
        }
    }

    #[test]
    fn unavailable_models_attach_to_their_provider_and_leave_it_open() {
        let entry = |provider: &str| ProviderStatusEntryV1 {
            provider: provider.to_string(),
            configured: None,
            credential_slot: None,
            reachable: None,
            launch_admission: "open".to_string(),
            refusal_detail: None,
            credit: None,
            credential_check: None,
            last_402_at: None,
            last_429_at: None,
            last_auth_failure_at: None,
            auth_episode: None,
            launches_24h: 0,
            failed_launches_24h: 0,
            failure_rate_24h: None,
            models_unavailable: Vec::new(),
            login: None,
            contract: None,
        };
        let mut report = AgentGetProviderStatusResultV1 {
            generated_at: Utc::now(),
            cache_ttl_secs: 60,
            providers: vec![entry("claude"), entry("codex")],
            provider_profile: crate::provider_profile::ProviderProfile::All,
        };
        let at = Utc::now();
        report.apply_models_unavailable([(
            "Codex".to_string(),
            ProviderModelUnavailableV1 {
                model: "gpt-6.1-sol".into(),
                code: PROVIDER_MODEL_UNAVAILABLE_CODE.into(),
                first_seen: at,
                last_seen: at,
                evidence_session_id: "s".into(),
                evidence_stop_reason: "provider_error:model_unavailable:gpt-6.1-sol".into(),
            },
        )]);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(
            json["providers"][1]["models_unavailable"][0]["model"],
            "gpt-6.1-sol"
        );
        assert_eq!(
            json["providers"][1]["models_unavailable"][0]["code"],
            "model_unavailable"
        );
        assert_eq!(json["providers"][1]["launch_admission"], "open");
        assert!(json["providers"][0].get("models_unavailable").is_none());
    }

    #[test]
    fn login_attaches_to_cli_entries_and_only_logged_out_refuses() {
        use crate::provider_login::{
            PROVIDER_LOGIN_REQUIRED_CODE, ProviderLoginMethod, ProviderLoginState, ProviderLoginV1,
        };
        let entry = |provider: &str| ProviderStatusEntryV1 {
            provider: provider.to_string(),
            configured: None,
            credential_slot: None,
            reachable: None,
            launch_admission: "open".to_string(),
            refusal_detail: None,
            credit: None,
            credential_check: None,
            last_402_at: None,
            last_429_at: None,
            last_auth_failure_at: None,
            auth_episode: None,
            launches_24h: 0,
            failed_launches_24h: 0,
            failure_rate_24h: None,
            models_unavailable: Vec::new(),
            login: None,
            contract: None,
        };
        let mut report = AgentGetProviderStatusResultV1 {
            generated_at: Utc::now(),
            cache_ttl_secs: 60,
            providers: vec![
                entry("claude"),
                entry("codex"),
                entry("codex_app_server"),
                entry("openrouter"),
            ],
            provider_profile: crate::provider_profile::ProviderProfile::All,
        };
        let at = Utc::now();
        let login = |state| ProviderLoginV1 {
            state,
            method: ProviderLoginMethod::None,
            checked_at: Some(at),
            hosts: Vec::new(),
        };
        report.apply_provider_login([
            ("claude".to_string(), login(ProviderLoginState::Unknown)),
            ("codex".to_string(), login(ProviderLoginState::LoggedOut)),
        ]);
        let json = serde_json::to_value(&report).unwrap();
        // `unknown` is reported and fails open.
        assert_eq!(json["providers"][0]["login"]["state"], "unknown");
        assert_eq!(json["providers"][0]["launch_admission"], "open");
        // `logged_out` refuses, and the app-server shares the Codex login.
        for index in [1, 2] {
            assert_eq!(json["providers"][index]["login"]["state"], "logged_out");
            assert_eq!(json["providers"][index]["launch_admission"], "refused");
            assert_eq!(
                json["providers"][index]["refusal_detail"],
                PROVIDER_LOGIN_REQUIRED_CODE
            );
        }
        assert!(json["providers"][3].get("login").is_none());
        assert_eq!(json["providers"][3]["launch_admission"], "open");
    }

    #[test]
    fn contract_attaches_to_cli_entries_and_only_a_custody_failure_refuses_codex() {
        use crate::provider_contract::{
            ProviderContractState, ProviderContractV1, REASON_CODEX_HANDSHAKE_FAILED,
            REASON_CODEX_HOOK_UNTRUSTED,
        };
        let entry = |provider: &str| ProviderStatusEntryV1 {
            provider: provider.to_string(),
            configured: None,
            credential_slot: None,
            reachable: None,
            launch_admission: "open".to_string(),
            refusal_detail: None,
            credit: None,
            credential_check: None,
            last_402_at: None,
            last_429_at: None,
            last_auth_failure_at: None,
            auth_episode: None,
            launches_24h: 0,
            failed_launches_24h: 0,
            failure_rate_24h: None,
            models_unavailable: Vec::new(),
            login: None,
            contract: None,
        };
        let report = || AgentGetProviderStatusResultV1 {
            generated_at: Utc::now(),
            cache_ttl_secs: 60,
            providers: vec![
                entry("claude"),
                entry("codex"),
                entry("codex_app_server"),
                entry("openrouter"),
            ],
            provider_profile: crate::provider_profile::ProviderProfile::All,
        };
        let degraded = |reason: &str| ProviderContractV1 {
            contract_state: ProviderContractState::Degraded,
            reason: Some(reason.to_string()),
            cli_version: Some("codex-cli 9.9.9".into()),
            detail: None,
            checked_at: Some(Utc::now()),
            catalog: None,
        };
        let mut custody = report();
        custody.apply_provider_contract(
            [
                ("claude".to_string(), ProviderContractV1::default()),
                ("codex".to_string(), degraded(REASON_CODEX_HOOK_UNTRUSTED)),
            ],
            true,
        );
        let json = serde_json::to_value(&custody).unwrap();
        assert_eq!(
            json["providers"][0]["contract"]["contract_state"],
            "unchecked"
        );
        assert_eq!(json["providers"][0]["launch_admission"], "open");
        assert_eq!(
            json["providers"][1]["contract"]["contract_state"],
            "degraded"
        );
        assert_eq!(json["providers"][1]["launch_admission"], "refused");
        assert_eq!(
            json["providers"][1]["refusal_detail"],
            REASON_CODEX_HOOK_UNTRUSTED
        );
        // The app-server has no custody hook: it reports the state, never refuses.
        assert_eq!(
            json["providers"][2]["contract"]["contract_state"],
            "degraded"
        );
        assert_eq!(json["providers"][2]["launch_admission"], "open");
        assert!(json["providers"][3].get("contract").is_none());

        // The refusal setting off, or a non-custody reason, leaves admission open.
        for (reason, refuse) in [
            (REASON_CODEX_HOOK_UNTRUSTED, false),
            (REASON_CODEX_HANDSHAKE_FAILED, true),
        ] {
            let mut open = report();
            open.apply_provider_contract([("codex".to_string(), degraded(reason))], refuse);
            let json = serde_json::to_value(&open).unwrap();
            assert_eq!(json["providers"][1]["contract"]["reason"], reason);
            assert_eq!(json["providers"][1]["launch_admission"], "open");
        }
    }

    #[test]
    fn request_rejects_unknown_fields_and_names() {
        assert!(
            serde_json::from_value::<AgentGetProviderStatusRequestV1>(
                serde_json::json!({"caller": "x"})
            )
            .is_err()
        );
        let ok: AgentGetProviderStatusRequestV1 =
            serde_json::from_value(serde_json::json!({"provider": "openrouter"})).unwrap();
        assert_eq!(ok.validate(), Ok(()));
        let bad = AgentGetProviderStatusRequestV1 {
            provider: Some("nope".into()),
        };
        assert_eq!(bad.validate(), Err(PROVIDER_STATUS_UNKNOWN_PROVIDER));
        assert_eq!(
            AgentGetProviderStatusRequestV1::default().validate(),
            Ok(())
        );
    }
    fn auth_entry(episode: Option<ProviderAuthEpisodeV1>) -> ProviderStatusEntryV1 {
        let refused = episode.as_ref().is_some_and(ProviderAuthEpisodeV1::refuses);
        ProviderStatusEntryV1 {
            provider: "codex".to_string(),
            configured: None,
            credential_slot: None,
            reachable: None,
            launch_admission: if refused { "refused" } else { "open" }.to_string(),
            refusal_detail: refused.then(|| PROVIDER_AUTH_INVALID_DETAIL.to_string()),
            credit: None,
            credential_check: None,
            last_402_at: None,
            last_429_at: None,
            last_auth_failure_at: episode.as_ref().map(|e| e.last_failure_at),
            auth_episode: episode,
            launches_24h: 0,
            failed_launches_24h: 0,
            failure_rate_24h: None,
            models_unavailable: Vec::new(),
            login: None,
            contract: None,
        }
    }

    fn codex_login(
        state: crate::provider_login::ProviderLoginState,
        checked_at: Option<DateTime<Utc>>,
    ) -> crate::provider_login::ProviderLoginV1 {
        crate::provider_login::ProviderLoginV1 {
            state,
            checked_at,
            ..Default::default()
        }
    }

    #[test]
    fn auth_episode_from_failures_refuses_until_the_probe_interval_passes() {
        let now = Utc::now();
        let last = now - chrono::Duration::minutes(10);
        let early = ProviderAuthEpisodeV1::from_failures(last, last, now);
        assert_eq!(early.state, ProviderAuthEpisodeStateV1::AwaitingProbe);
        assert_eq!(
            early.next_probe_at,
            Some(last + chrono::Duration::seconds(PROVIDER_AUTH_PROBE_INTERVAL_SECS))
        );
        let old = now - chrono::Duration::seconds(PROVIDER_AUTH_PROBE_INTERVAL_SECS + 1);
        let late = ProviderAuthEpisodeV1::from_failures(old, old, now);
        assert_eq!(late.state, ProviderAuthEpisodeStateV1::ProbeAdmitted);
        assert_eq!((late.refuses(), late.next_probe_at), (false, None));
    }

    #[test]
    fn a_logged_in_check_after_the_last_failure_reopens_admission() {
        use crate::provider_login::ProviderLoginState::LoggedIn;
        let now = Utc::now();
        let failed = now - chrono::Duration::minutes(30);
        let episode = ProviderAuthEpisodeV1::from_failures(failed, failed, now);
        let mut entry = auth_entry(Some(episode));
        assert_eq!(entry.launch_admission, "refused");
        let mut report = AgentGetProviderStatusResultV1 {
            generated_at: now,
            cache_ttl_secs: 60,
            providers: vec![entry.clone()],
            provider_profile: crate::provider_profile::ProviderProfile::All,
        };
        let checked = now - chrono::Duration::minutes(1);
        report.apply_provider_login([("codex".to_string(), codex_login(LoggedIn, Some(checked)))]);
        entry = report.providers.remove(0);
        assert_eq!(entry.launch_admission, "open");
        assert_eq!(entry.refusal_detail, None);
        let episode = entry.auth_episode.unwrap();
        assert_eq!(episode.state, ProviderAuthEpisodeStateV1::ClearedByLogin);
        assert_eq!(episode.last_failure_at, failed);
    }

    #[test]
    fn a_login_check_older_than_the_failure_or_a_logged_out_one_keeps_it_refused() {
        use crate::provider_login::ProviderLoginState::{LoggedIn, LoggedOut, Unknown};
        let now = Utc::now();
        let failed = now - chrono::Duration::minutes(30);
        for (login, state) in [
            // Checked before the failure: no evidence the credential changed.
            (
                codex_login(LoggedIn, Some(failed - chrono::Duration::minutes(1))),
                ProviderAuthEpisodeStateV1::AwaitingProbe,
            ),
            (
                codex_login(LoggedIn, None),
                ProviderAuthEpisodeStateV1::AwaitingProbe,
            ),
            (
                codex_login(Unknown, Some(now)),
                ProviderAuthEpisodeStateV1::AwaitingProbe,
            ),
            // Logged out: re-login, and no timed probe either.
            (
                codex_login(LoggedOut, Some(now)),
                ProviderAuthEpisodeStateV1::ReloginNeeded,
            ),
        ] {
            let episode = ProviderAuthEpisodeV1::from_failures(failed, failed, now);
            let mut report = AgentGetProviderStatusResultV1 {
                generated_at: now,
                cache_ttl_secs: 60,
                providers: vec![auth_entry(Some(episode))],
                provider_profile: crate::provider_profile::ProviderProfile::All,
            };
            report.apply_provider_login([("codex".to_string(), login)]);
            let entry = &report.providers[0];
            assert_eq!(entry.launch_admission, "refused", "{state:?}");
            assert_eq!(entry.refusal_detail.as_deref(), Some("auth_invalid"));
            let episode = entry.auth_episode.as_ref().unwrap();
            assert_eq!(episode.state, state);
            assert_eq!(
                episode.next_probe_at.is_some(),
                state == ProviderAuthEpisodeStateV1::AwaitingProbe
            );
        }
    }

    #[test]
    fn a_just_failed_launch_waits_the_recheck_gap_before_a_login_check_reopens() {
        use crate::provider_login::ProviderLoginState::LoggedIn;
        let now = Utc::now();
        let failed = now - chrono::Duration::seconds(30);
        let mut episode = ProviderAuthEpisodeV1::from_failures(failed, failed, now);
        episode.apply_login(&codex_login(LoggedIn, Some(now)), now);
        assert!(episode.refuses());
        assert_eq!(
            episode.next_probe_at,
            Some(failed + chrono::Duration::seconds(PROVIDER_AUTH_LOGIN_RECHECK_MIN_SECS))
        );
    }

    #[test]
    fn a_logged_out_login_turns_an_admitted_probe_back_into_a_relogin() {
        use crate::provider_login::ProviderLoginState::LoggedOut;
        let now = Utc::now();
        let failed = now - chrono::Duration::hours(2);
        let episode = ProviderAuthEpisodeV1::from_failures(failed, failed, now);
        assert_eq!(episode.state, ProviderAuthEpisodeStateV1::ProbeAdmitted);
        let mut report = AgentGetProviderStatusResultV1 {
            generated_at: now,
            cache_ttl_secs: 60,
            providers: vec![auth_entry(Some(episode))],
            provider_profile: crate::provider_profile::ProviderProfile::All,
        };
        report.apply_provider_login([("codex".to_string(), codex_login(LoggedOut, Some(now)))]);
        let entry = &report.providers[0];
        assert_eq!(entry.launch_admission, "refused");
        assert_eq!(entry.refusal_detail.as_deref(), Some("auth_invalid"));
        assert_eq!(
            entry.auth_episode.as_ref().unwrap().state,
            ProviderAuthEpisodeStateV1::ReloginNeeded
        );
    }
}
