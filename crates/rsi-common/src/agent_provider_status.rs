//! `AgentGetProviderStatus` (#1044): a secret-free, bounded read of whether a
//! provider can take work (configured, reachable, remaining credit, recent
//! 402/429 and launch failures). The daemon makes any remote call with its own
//! credentials; nothing in these types can carry a key.

use crate::provider_credentials::CredentialCheckClass;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
    pub launches_24h: u32,
    pub failed_launches_24h: u32,
    /// `failed_launches_24h / launches_24h`, `null` with no launches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_rate_24h: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentGetProviderStatusResultV1 {
    pub generated_at: DateTime<Utc>,
    pub cache_ttl_secs: u64,
    /// At most `PROVIDER_STATUS_NAMES.len()` entries.
    pub providers: Vec<ProviderStatusEntryV1>,
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
