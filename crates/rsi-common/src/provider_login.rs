//! Typed login state of the provider CLIs (#2753): whether `claude` and
//! `codex` are logged in on a host, before a session is spent on a launch that
//! would fail.
//!
//! The daemon reads the CLIs' own status output (`claude auth status --json`,
//! `codex login status`) and keeps exactly three facts per provider: the
//! [`ProviderLoginState`], the [`ProviderLoginMethod`] (the CLI's closed
//! `authMethod` set) and the check time. Nothing else from the CLI output
//! (account e-mail, organisation, directory, token) has a field here, so no
//! response built from these types can carry it.
//!
//! The operator methods ([`METHOD_GET`], [`METHOD_REFRESH`]) are operator-only:
//! they stay out of `AGENT_VERBS`, `READ_VERBS`, the native tools and the agent
//! CLI catalog. Agents read the same state through `AgentGetProviderStatus`
//! (`login` on the `claude`, `codex` and `codex_app_server` entries).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Operator RPC: the login state of every probed provider.
pub const METHOD_GET: &str = "GetProviderLoginState";
/// Operator RPC: probe now (bypassing the cache) and return the new state.
pub const METHOD_REFRESH: &str = "RefreshProviderLoginState";
/// Operator-only methods this module owns.
pub const OPERATOR_METHODS: [&str; 2] = [METHOD_GET, METHOD_REFRESH];

/// Stable code of a launch refused because the provider CLI is logged out.
/// Distinct from `provider_auth_invalid`, a credential the provider rejected.
pub const PROVIDER_LOGIN_REQUIRED_CODE: &str = "provider_login_required";

/// Error code for an unknown or non-probed `provider` on a refresh.
pub const PROVIDER_LOGIN_UNKNOWN_PROVIDER: &str = "provider_login_unknown_provider";

/// The providers whose CLI login is probed, in stable order.
pub const PROVIDER_LOGIN_NAMES: [&str; 2] = ["claude", "codex"];

/// Default freshness of a probe result.
pub const PROVIDER_LOGIN_CACHE_TTL_SECS: u64 = 60;

/// Most per-host rows one provider's `login` carries.
pub const PROVIDER_LOGIN_MAX_HOSTS: usize = 16;

/// The probed provider behind a `AgentGetProviderStatus` entry name, if any.
/// The Codex app-server runs the same CLI login as `codex`.
#[must_use]
pub fn login_provider_for_status_name(name: &str) -> Option<&'static str> {
    match name {
        "claude" => Some("claude"),
        "codex" | "codex_app_server" => Some("codex"),
        _ => None,
    }
}

/// The host-side fix for a logged-out provider.
#[must_use]
pub fn login_remedy(provider: &str) -> &'static str {
    match provider {
        "claude" => "run `claude auth login` on the host, then refresh the login state",
        "codex" => "run `codex login` on the host, then refresh the login state",
        _ => "log the provider CLI in on the host, then refresh the login state",
    }
}

/// The message of a launch refused `provider_login_required`.
#[must_use]
pub fn login_required_message(provider: &str) -> String {
    format!(
        "{PROVIDER_LOGIN_REQUIRED_CODE}: the {provider} CLI is not logged in on this host; {}",
        login_remedy(provider)
    )
}

/// Whether the CLI is logged in. `unknown` means the probe could not tell (the
/// CLI is missing, timed out, or printed something unreadable); a launch is
/// never refused on `unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderLoginState {
    LoggedIn,
    LoggedOut,
    #[default]
    Unknown,
}

impl ProviderLoginState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LoggedIn => "logged_in",
            Self::LoggedOut => "logged_out",
            Self::Unknown => "unknown",
        }
    }
}

/// How the CLI is logged in: the closed `authMethod` set of
/// `claude auth status` (`none|claude.ai|oauth_token|api_key|api_key_helper|
/// third_party`) plus `chatgpt` for Codex. Any other value reads as `unknown`;
/// the CLI's text is never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ProviderLoginMethod {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "claude.ai")]
    ClaudeAi,
    #[serde(rename = "oauth_token")]
    OauthToken,
    #[serde(rename = "api_key")]
    ApiKey,
    #[serde(rename = "api_key_helper")]
    ApiKeyHelper,
    #[serde(rename = "third_party")]
    ThirdParty,
    #[serde(rename = "chatgpt")]
    Chatgpt,
    #[default]
    #[serde(rename = "unknown")]
    Unknown,
}

impl ProviderLoginMethod {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ClaudeAi => "claude.ai",
            Self::OauthToken => "oauth_token",
            Self::ApiKey => "api_key",
            Self::ApiKeyHelper => "api_key_helper",
            Self::ThirdParty => "third_party",
            Self::Chatgpt => "chatgpt",
            Self::Unknown => "unknown",
        }
    }

    /// Map a Claude `authMethod` onto the closed set; anything else is
    /// `unknown`.
    #[must_use]
    pub fn from_claude_auth_method(value: &str) -> Self {
        match value {
            "none" => Self::None,
            "claude.ai" => Self::ClaudeAi,
            "oauth_token" => Self::OauthToken,
            "api_key" => Self::ApiKey,
            "api_key_helper" => Self::ApiKeyHelper,
            "third_party" => Self::ThirdParty,
            _ => Self::Unknown,
        }
    }
}

/// One host's login state for one provider, on a hub's status next to the
/// hub's own (a satellite's probe, reported through its health snapshot).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderHostLoginV1 {
    /// The satellite's label.
    pub host: String,
    pub state: ProviderLoginState,
    pub method: ProviderLoginMethod,
    pub checked_at: DateTime<Utc>,
}

/// The login state of one provider on the daemon's own host.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProviderLoginV1 {
    pub state: ProviderLoginState,
    pub method: ProviderLoginMethod,
    /// `null` until the first probe finished.
    #[serde(default)]
    pub checked_at: Option<DateTime<Utc>>,
    /// Paired satellites' own probes (hub status only; empty on a satellite).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<ProviderHostLoginV1>,
}

/// One provider's probe, as a satellite reports it in its health snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderLoginHealthV1 {
    pub provider: String,
    pub state: ProviderLoginState,
    pub method: ProviderLoginMethod,
    pub checked_at: DateTime<Utc>,
}

impl ProviderLoginHealthV1 {
    /// Whether the report names a probed provider.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        PROVIDER_LOGIN_NAMES.contains(&self.provider.as_str())
    }
}

/// The per-host rows of `provider` from each paired satellite's reported
/// probes, `(label, probes)`, in the order given and bounded by
/// [`PROVIDER_LOGIN_MAX_HOSTS`].
#[must_use]
pub fn host_logins(
    provider: &str,
    satellites: &[(String, Vec<ProviderLoginHealthV1>)],
) -> Vec<ProviderHostLoginV1> {
    satellites
        .iter()
        .flat_map(|(label, probes)| {
            probes
                .iter()
                .filter(|probe| probe.provider == provider)
                .map(|probe| ProviderHostLoginV1 {
                    host: label.clone(),
                    state: probe.state,
                    method: probe.method,
                    checked_at: probe.checked_at,
                })
        })
        .take(PROVIDER_LOGIN_MAX_HOSTS)
        .collect()
}

/// One provider's row of `GetProviderLoginState`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderLoginRowV1 {
    pub provider: String,
    pub login: ProviderLoginV1,
    /// The host-side fix, shown whatever the state.
    pub remedy: String,
}

/// Result of `GetProviderLoginState` and `RefreshProviderLoginState`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderLoginStateResultV1 {
    pub generated_at: DateTime<Utc>,
    pub cache_ttl_secs: u64,
    /// One row per entry of [`PROVIDER_LOGIN_NAMES`] (or the one asked for).
    pub providers: Vec<ProviderLoginRowV1>,
}

/// Strict request of both methods; omit `provider` for every provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderLoginRequestV1 {
    #[serde(default)]
    pub provider: Option<String>,
}

impl ProviderLoginRequestV1 {
    /// # Errors
    /// [`PROVIDER_LOGIN_UNKNOWN_PROVIDER`] when `provider` is not probed.
    pub fn validate(&self) -> Result<(), &'static str> {
        match self.provider.as_deref() {
            Some(name) if !PROVIDER_LOGIN_NAMES.contains(&name) => {
                Err(PROVIDER_LOGIN_UNKNOWN_PROVIDER)
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shape_is_the_closed_auth_method_set() {
        let login = ProviderLoginV1 {
            state: ProviderLoginState::LoggedOut,
            method: ProviderLoginMethod::ClaudeAi,
            checked_at: None,
            hosts: Vec::new(),
        };
        let json = serde_json::to_value(&login).unwrap();
        assert_eq!(json["state"], "logged_out");
        assert_eq!(json["method"], "claude.ai");
        assert!(json.get("hosts").is_none());
        let back: ProviderLoginV1 = serde_json::from_value(json).unwrap();
        assert_eq!(back, login);
        for (value, method) in [
            ("none", ProviderLoginMethod::None),
            ("claude.ai", ProviderLoginMethod::ClaudeAi),
            ("oauth_token", ProviderLoginMethod::OauthToken),
            ("api_key", ProviderLoginMethod::ApiKey),
            ("api_key_helper", ProviderLoginMethod::ApiKeyHelper),
            ("third_party", ProviderLoginMethod::ThirdParty),
            ("a.future.method", ProviderLoginMethod::Unknown),
        ] {
            assert_eq!(ProviderLoginMethod::from_claude_auth_method(value), method);
            assert_eq!(
                serde_json::to_value(method).unwrap(),
                serde_json::Value::String(method.as_str().into())
            );
        }
    }

    #[test]
    fn host_rows_come_from_each_satellites_own_probe_of_that_provider() {
        let at = Utc::now();
        let probe = |provider: &str, state| ProviderLoginHealthV1 {
            provider: provider.to_string(),
            state,
            method: ProviderLoginMethod::Chatgpt,
            checked_at: at,
        };
        let satellites = vec![
            (
                "laptop".to_string(),
                vec![
                    probe("claude", ProviderLoginState::LoggedIn),
                    probe("codex", ProviderLoginState::LoggedOut),
                ],
            ),
            (
                "desktop".to_string(),
                vec![probe("codex", ProviderLoginState::LoggedIn)],
            ),
        ];
        let codex = host_logins("codex", &satellites);
        assert_eq!(
            codex
                .iter()
                .map(|host| (host.host.as_str(), host.state))
                .collect::<Vec<_>>(),
            [
                ("laptop", ProviderLoginState::LoggedOut),
                ("desktop", ProviderLoginState::LoggedIn)
            ]
        );
        assert_eq!(host_logins("claude", &satellites).len(), 1);
        assert!(host_logins("claude", &[]).is_empty());
        let unprobed = ProviderLoginHealthV1 {
            provider: "openrouter".into(),
            state: ProviderLoginState::Unknown,
            method: ProviderLoginMethod::Unknown,
            checked_at: at,
        };
        assert!(!unprobed.is_valid());
    }

    #[test]
    fn status_names_map_to_the_probed_provider() {
        assert_eq!(login_provider_for_status_name("claude"), Some("claude"));
        assert_eq!(login_provider_for_status_name("codex"), Some("codex"));
        assert_eq!(
            login_provider_for_status_name("codex_app_server"),
            Some("codex")
        );
        assert_eq!(login_provider_for_status_name("openrouter"), None);
    }

    #[test]
    fn the_refusal_message_names_the_code_and_the_host_side_fix() {
        let message = login_required_message("codex");
        assert!(message.starts_with("provider_login_required:"), "{message}");
        assert!(message.contains("`codex login`"), "{message}");
        assert!(login_remedy("claude").contains("`claude auth login`"));
    }

    #[test]
    fn request_rejects_unknown_fields_and_names() {
        assert!(
            serde_json::from_value::<ProviderLoginRequestV1>(serde_json::json!({"caller": "x"}))
                .is_err()
        );
        let bad = ProviderLoginRequestV1 {
            provider: Some("openrouter".into()),
        };
        assert_eq!(bad.validate(), Err(PROVIDER_LOGIN_UNKNOWN_PROVIDER));
        assert_eq!(ProviderLoginRequestV1::default().validate(), Ok(()));
    }
}
