//! Operator-only provider-credential (key vault) wire types (#694 K1).
//!
//! These types are shared by the daemon (`rsid::vault`) and the TUI. Every
//! *response* type here is metadata only: none of them has a field that can
//! carry secret bytes, so a response built from them cannot leak a key by
//! construction. The only type that carries a secret is the operator's
//! [`SetProviderCredentialParams`] request, whose `Debug` is redacted.
//!
//! The corresponding RPC methods are operator-only. They must stay absent from
//! the agent verb catalogs (`AGENT_VERBS`, `READ_VERBS`), native provider tools
//! and the `rsi-rpc agent` catalog, and a request carrying a session token is
//! refused.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// `daemon_settings` / `GetDaemonConfig` key: when `true` (default), legacy
/// provider env vars resolve a slot that has neither a vault entry nor a
/// tombstone.
pub const SETTING_VAULT_ENV_COMPAT: &str = "vault.env_compat";
/// `daemon_settings` / `GetDaemonConfig` key: seconds a validity/credit check
/// result stays fresh (default [`DEFAULT_CHECK_TTL_SECS`]).
pub const SETTING_VAULT_CHECK_TTL_SECS: &str = "vault.check_ttl_secs";
pub const DEFAULT_CHECK_TTL_SECS: u64 = 600;
pub const MIN_CHECK_TTL_SECS: u64 = 1;
pub const MAX_CHECK_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Operator RPC method names. Operator-only; never agent-callable.
pub const METHOD_SET: &str = "SetProviderCredential";
pub const METHOD_ROTATE: &str = "RotateProviderCredential";
pub const METHOD_CLEAR: &str = "ClearProviderCredential";
pub const METHOD_LIST: &str = "ListProviderCredentials";
pub const METHOD_CHECK: &str = "CheckProviderCredential";
pub const METHOD_IMPORT: &str = "ImportProviderCredentialsFromEnv";
pub const OPERATOR_METHODS: [&str; 6] = [
    METHOD_SET,
    METHOD_ROTATE,
    METHOD_CLEAR,
    METHOD_LIST,
    METHOD_CHECK,
    METHOD_IMPORT,
];

/// Closed, validated set of credential slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCredentialSlot {
    Openrouter,
    Anthropic,
    Openai,
    Bedrock,
    Pioneer,
    Inception,
    Deepseek,
    Groq,
    Mistral,
    Xai,
    Cerebras,
    Perplexity,
    Together,
    Fireworks,
    Cohere,
    Sambanova,
    Lepton,
    Nvidia,
    Siliconflow,
    Huggingface,
    Cloudflare,
    Anyscale,
    Octoai,
}

impl ProviderCredentialSlot {
    pub const ALL: [Self; 23] = [
        Self::Openrouter,
        Self::Anthropic,
        Self::Openai,
        Self::Bedrock,
        Self::Pioneer,
        Self::Inception,
        Self::Deepseek,
        Self::Groq,
        Self::Mistral,
        Self::Xai,
        Self::Cerebras,
        Self::Perplexity,
        Self::Together,
        Self::Fireworks,
        Self::Cohere,
        Self::Sambanova,
        Self::Lepton,
        Self::Nvidia,
        Self::Siliconflow,
        Self::Huggingface,
        Self::Cloudflare,
        Self::Anyscale,
        Self::Octoai,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Openrouter => "openrouter",
            Self::Anthropic => "anthropic",
            Self::Openai => "openai",
            Self::Bedrock => "bedrock",
            Self::Pioneer => "pioneer",
            Self::Inception => "inception",
            Self::Deepseek => "deepseek",
            Self::Groq => "groq",
            Self::Mistral => "mistral",
            Self::Xai => "xai",
            Self::Cerebras => "cerebras",
            Self::Perplexity => "perplexity",
            Self::Together => "together",
            Self::Fireworks => "fireworks",
            Self::Cohere => "cohere",
            Self::Sambanova => "sambanova",
            Self::Lepton => "lepton",
            Self::Nvidia => "nvidia",
            Self::Siliconflow => "siliconflow",
            Self::Huggingface => "huggingface",
            Self::Cloudflare => "cloudflare",
            Self::Anyscale => "anyscale",
            Self::Octoai => "octoai",
        }
    }

    /// Parse a slot name; the set is closed, so unknown names are rejected.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|slot| slot.as_str() == value)
    }
}

impl fmt::Display for ProviderCredentialSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Where a slot currently resolves from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialState {
    Vault,
    EnvCompat,
    Generator,
    Cleared,
    Absent,
}

/// Result class of a validity/credit check. Only `Invalid` and `Exhausted`
/// are authoritative (they refuse a launch while fresh); `Unknown` records a
/// transient failure and always admits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialCheckClass {
    Valid,
    Invalid,
    Exhausted,
    Unknown,
}

impl CredentialCheckClass {
    #[must_use]
    pub const fn is_authoritative_refusal(self) -> bool {
        matches!(self, Self::Invalid | Self::Exhausted)
    }
}

/// How a slot's provider is executed today. Slice R extends the routing
/// decision; until then OpenRouter/Bedrock/Pioneer always use the Codex CLI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialRoute {
    CodexCli,
    Harness,
}

/// Residual exposure R1, derived from capability rather than the configured
/// route alone.
///
/// A CLI process that receives a key holds it in its own environment (its
/// tool shells do not).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliExposure {
    /// The route is `codex_cli`, or some CLI consumer (e.g. the memory LLM)
    /// launches a CLI with this slot.
    Always,
    /// The route is `harness` and the Codex fallback is enabled.
    OnFallback,
    /// No CLI consumer can receive the key.
    None,
}

/// Which CLI consumer received an injected key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliExposureConsumer {
    /// A session's Codex CLI launch.
    SessionCodexCli,
    /// The memory LLM's Codex CLI.
    MemoryCodexCli,
}

/// Why a key was injected into a CLI process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliExposureReason {
    Route,
    Fallback,
    Memory,
}

/// Secret-free record of the last validity/credit check for a slot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CredentialCheckMetadata {
    /// The slot generation the check started under. A result commits only if
    /// the slot's generation is unchanged (compare-and-set).
    #[serde(default)]
    pub generation: u64,
    pub at: DateTime<Utc>,
    pub class: CredentialCheckClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    /// Remaining credit as reported by the provider (`OpenRouter`'s
    /// `limit_remaining`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit_remaining: Option<f64>,
    pub detail_code: String,
    /// Redacted fingerprint of the credential that was checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

/// Per-slot, secret-free view returned by `ListProviderCredentials`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderCredentialMetadata {
    pub slot: ProviderCredentialSlot,
    pub state: CredentialState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotated_from_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleared_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<CredentialCheckMetadata>,
    /// Bumped on every Set, Rotate, Clear and import.
    #[serde(default)]
    pub generation: u64,
    pub route: CredentialRoute,
    pub cli_exposure: CliExposure,
    /// Last time this daemon injected the slot's key into a CLI process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_cli_exposure_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ListProviderCredentialsResult {
    pub env_compat: bool,
    pub check_ttl_secs: u64,
    pub credentials: Vec<ProviderCredentialMetadata>,
}

/// `SetProviderCredential` / `RotateProviderCredential` request. The only
/// secret-bearing wire type; its `Debug` never prints the secret.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetProviderCredentialParams {
    pub slot: ProviderCredentialSlot,
    pub secret: String,
}

impl fmt::Debug for SetProviderCredentialParams {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SetProviderCredentialParams")
            .field("slot", &self.slot)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// `ClearProviderCredential` / `CheckProviderCredential` request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCredentialSlotParams {
    pub slot: ProviderCredentialSlot,
}

/// Why `ImportProviderCredentialsFromEnv` did not import a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportSkipReason {
    PresentInVault,
    Cleared,
    AbsentFromEnv,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportSkipped {
    pub slot: ProviderCredentialSlot,
    pub reason: ImportSkipReason,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedCredential {
    pub slot: ProviderCredentialSlot,
    pub fingerprint: String,
    /// Name of the env var the value came from (a name, never a value).
    pub env_var: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportProviderCredentialsResult {
    pub imported: Vec<ImportedCredential>,
    pub skipped: Vec<ImportSkipped>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn slot_names_round_trip_and_set_is_closed() {
        for slot in ProviderCredentialSlot::ALL {
            assert_eq!(ProviderCredentialSlot::parse(slot.as_str()), Some(slot));
            let json = serde_json::to_string(&slot).unwrap();
            assert_eq!(json, format!("\"{}\"", slot.as_str()));
            assert_eq!(
                serde_json::from_str::<ProviderCredentialSlot>(&json).unwrap(),
                slot
            );
        }
        assert_eq!(ProviderCredentialSlot::parse("OpenRouter"), None);
        assert!(serde_json::from_str::<ProviderCredentialSlot>("\"mystery\"").is_err());
    }

    /// Leaked-authority check: a manager's `OperatorDelegation` grant can
    /// never reach the key vault.
    #[test]
    fn vault_methods_are_not_manager_delegable() {
        for method in OPERATOR_METHODS {
            assert!(
                !crate::manager_operator_delegation::DELEGABLE_OPERATOR_METHODS.contains(&method),
                "{method}"
            );
        }
    }

    #[test]
    fn set_params_debug_is_redacted() {
        let params = SetProviderCredentialParams {
            slot: ProviderCredentialSlot::Openrouter,
            secret: "sk-test-debug-leak-canary".into(),
        };
        let debug = format!("{params:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("sk-test-debug-leak-canary"));
    }

    #[test]
    fn set_params_reject_unknown_fields() {
        let error = serde_json::from_value::<SetProviderCredentialParams>(serde_json::json!({
            "slot": "openrouter",
            "secret": "sk-test-x",
            "session_token": "t",
        }));
        assert!(error.is_err());
    }
}
