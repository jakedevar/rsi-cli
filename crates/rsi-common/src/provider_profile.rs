//! Operator provider profile (Issue #1407).
//!
//! `provider_profile` is an ordinary persisted daemon setting. `all` (the
//! default) keeps every provider exactly as before. `aws_only` admits one
//! launch shape: Claude Code on Amazon Bedrock, i.e. a Claude Code session
//! whose effective model is a Bedrock Claude model ID
//! (`[<geo>.]anthropic.claude-*`). Every other provider and model is refused
//! with [`PROVIDER_PROFILE_REFUSED`].
//!
//! The daemon checks the profile in `RuntimeConfig::launch_model_refusal`, the
//! one predicate every launch path already passes (the launch chokepoint, the
//! provider-spawn backstop, the continuation and rotation preflights and the
//! `AgentSpawnChild` pre-check). The TUI uses [`picker_allows`] to filter its
//! provider and model pickers. Both sides share this module so they cannot
//! drift.
//!
//! The first-run AWS setup stores the region in the daemon setting
//! [`BEDROCK_REGION_FIELD`] and the credential in the key vault (through the
//! operator-only `SetProviderCredential`), then calls the operator-only
//! [`METHOD_VERIFY_BEDROCK_SETUP`], whose result ([`BedrockSetupCheck`])
//! carries no secret.

use crate::bedrock_model::{BedrockVendor, bedrock_vendor};
use crate::provider_credentials::CredentialState;
use crate::types::SessionProvider;
use serde::{Deserialize, Serialize};

/// `UpdateDaemonConfig` / `GetDaemonConfig` field holding the profile.
pub const PROVIDER_PROFILE_FIELD: &str = "provider_profile";

/// `UpdateDaemonConfig` / `GetDaemonConfig` field holding the operator's AWS
/// region for Bedrock (empty = fall back to `AWS_REGION`, then
/// `AWS_DEFAULT_REGION`, then `aws configure get region`).
pub const BEDROCK_REGION_FIELD: &str = "bedrock_region";

/// Typed refusal code carried at the front of every profile refusal.
pub const PROVIDER_PROFILE_REFUSED: &str = "provider_profile_refused";

/// Operator-only RPC that verifies the Bedrock setup with one live call.
pub const METHOD_VERIFY_BEDROCK_SETUP: &str = "VerifyBedrockSetup";

/// Operator-only methods this module owns.
pub const OPERATOR_METHODS: [&str; 1] = [METHOD_VERIFY_BEDROCK_SETUP];

/// The Bedrock Claude model the setup check invokes, and the model an
/// `aws_only` launch runs when nothing names one.
pub const AWS_ONLY_DEFAULT_MODEL: &str = "us.anthropic.claude-sonnet-5-v1:0";

/// Bedrock Claude models the TUI offers under `aws_only` before discovery
/// fills in the account's own inference profiles.
pub const AWS_ONLY_MODELS: &[(&str, &str)] = &[
    (AWS_ONLY_DEFAULT_MODEL, "Claude Sonnet 5 (Bedrock, US)"),
    (
        "us.anthropic.claude-opus-5-5-v1:0",
        "Claude Opus 5.5 (Bedrock, US)",
    ),
    (
        "global.anthropic.claude-sonnet-5",
        "Claude Sonnet 5 (Bedrock, global)",
    ),
    (
        "global.anthropic.claude-opus-5-5",
        "Claude Opus 5.5 (Bedrock, global)",
    ),
];

/// The operator's provider profile.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProfile {
    /// Every provider, exactly as before the profile existed.
    #[default]
    All,
    /// Claude Code on Amazon Bedrock only.
    AwsOnly,
}

impl ProviderProfile {
    /// Every profile, in settings-cycle order.
    pub const ALL: [Self; 2] = [Self::All, Self::AwsOnly];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::AwsOnly => "aws_only",
        }
    }

    /// Parse the wire spelling (case-insensitive, surrounding space ignored).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        Self::ALL
            .into_iter()
            .find(|profile| profile.as_str().eq_ignore_ascii_case(text))
    }

    /// Parse an `UpdateDaemonConfig` value.
    ///
    /// # Errors
    /// A fixed message when the value is not one of the profile spellings.
    pub fn from_value(value: &serde_json::Value) -> Result<Self, String> {
        value
            .as_str()
            .and_then(Self::parse)
            .ok_or_else(|| "expected all or aws_only".to_string())
    }
}

/// Whether `model` is a Bedrock Claude model ID (`[<geo>.]anthropic.claude-*`).
#[must_use]
pub fn is_bedrock_claude_model(model: &str) -> bool {
    bedrock_vendor(model) == Some(BedrockVendor::Anthropic)
}

/// Whether `profile` admits a launch of `model` on the runtime `provider`.
///
/// The runtime provider is the one a Bedrock Claude launch resolves to
/// (Claude Code or the Harness). `model` must be the effective model; `None`
/// (no model could be determined) is refused under `aws_only`.
#[must_use]
pub fn launch_allowed(
    profile: ProviderProfile,
    provider: SessionProvider,
    model: Option<&str>,
) -> bool {
    match profile {
        ProviderProfile::All => true,
        ProviderProfile::AwsOnly => {
            provider == SessionProvider::Claude && model.is_some_and(is_bedrock_claude_model)
        }
    }
}

/// Whether a TUI picker should offer `model` on `provider` under `profile`.
///
/// The Bedrock provider stays selectable under `aws_only` for its Claude
/// models, which the daemon runs on Claude Code (`api_route.bedrock =
/// codex_cli`); the launch check still has the final word.
#[must_use]
pub fn picker_allows(profile: ProviderProfile, provider: SessionProvider, model: &str) -> bool {
    match profile {
        ProviderProfile::All => true,
        ProviderProfile::AwsOnly => {
            provider_offered(profile, provider) && is_bedrock_claude_model(model)
        }
    }
}

/// Whether a TUI picker should offer `provider` at all under `profile`.
#[must_use]
pub const fn provider_offered(profile: ProviderProfile, provider: SessionProvider) -> bool {
    match profile {
        ProviderProfile::All => true,
        ProviderProfile::AwsOnly => {
            matches!(provider, SessionProvider::Claude | SessionProvider::Bedrock)
        }
    }
}

/// The refusal message: the typed code, what was requested, what is allowed.
#[must_use]
pub fn launch_refusal(
    profile: ProviderProfile,
    provider: SessionProvider,
    model: Option<&str>,
) -> String {
    let model = model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map_or_else(
            || "no model".to_string(),
            |model| format!("model '{model}'"),
        );
    format!(
        "{PROVIDER_PROFILE_REFUSED}: provider {provider:?} with {model} is not allowed under \
         provider_profile={}; only Claude Code on Amazon Bedrock may launch (a Bedrock Claude \
         model id such as {AWS_ONLY_DEFAULT_MODEL})",
        profile.as_str()
    )
}

/// The stable code at the front of a `RuntimeConfig::launch_model_refusal`.
///
/// [`PROVIDER_PROFILE_REFUSED`] for a profile refusal, otherwise the
/// allowlist's `launch_model_not_allowed`.
#[must_use]
pub fn launch_refusal_code(reason: &str) -> &'static str {
    if reason.starts_with(PROVIDER_PROFILE_REFUSED) {
        PROVIDER_PROFILE_REFUSED
    } else {
        crate::launch_allowlist::LAUNCH_MODEL_NOT_ALLOWED
    }
}

/// Validate and canonicalise a `bedrock_region` write: `null` or an empty
/// string clears it; otherwise a lowercase AWS region name
/// (`[a-z0-9-]{1,32}`, e.g. `us-east-1`).
///
/// # Errors
/// A fixed message when the value is not a string or not a region name.
pub fn normalize_bedrock_region(value: &serde_json::Value) -> Result<String, String> {
    let text = match value {
        serde_json::Value::Null => return Ok(String::new()),
        serde_json::Value::String(text) => text.trim(),
        _ => return Err("expected an AWS region string such as us-east-1".to_string()),
    };
    if text.is_empty() {
        return Ok(String::new());
    }
    if bedrock_region_valid(text) {
        Ok(text.to_string())
    } else {
        Err("expected an AWS region name such as us-east-1 (lowercase letters, digits, '-')".into())
    }
}

/// Whether `value` has the shape of an AWS region name.
#[must_use]
pub fn bedrock_region_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// `VerifyBedrockSetup` request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyBedrockSetupParams {
    /// Bedrock Claude model to invoke; [`AWS_ONLY_DEFAULT_MODEL`] when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Which step of the setup check decided the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BedrockSetupStage {
    /// The model named is not a Bedrock Claude model ID.
    Model,
    /// No valid AWS region is configured.
    Region,
    /// No Bedrock credential resolves.
    Credential,
    /// The live Bedrock call ran; see `ok` and `detail_code`.
    Invoke,
}

/// Secret-free result of `VerifyBedrockSetup`. Every field is metadata: the
/// response body of the Bedrock call and the credential are never copied in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BedrockSetupCheck {
    /// The live call succeeded: the region, credential and model work.
    pub ok: bool,
    pub stage: BedrockSetupStage,
    /// The region the check used, when one is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    pub model: String,
    /// Where the Bedrock credential resolves from.
    pub credential_state: CredentialState,
    /// The vault's short credential fingerprint, when the vault holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    /// Stable detail code (`ok`, `http_401`, `model_access_denied`, ...).
    pub detail_code: String,
    /// Fixed operator-facing text for `detail_code`.
    pub message: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn profile_parses_round_trips_and_defaults_to_all() {
        assert_eq!(ProviderProfile::default(), ProviderProfile::All);
        for profile in ProviderProfile::ALL {
            assert_eq!(ProviderProfile::parse(profile.as_str()), Some(profile));
            assert_eq!(
                serde_json::to_value(profile).unwrap(),
                json!(profile.as_str())
            );
        }
        assert_eq!(
            ProviderProfile::from_value(&json!(" AWS_ONLY ")),
            Ok(ProviderProfile::AwsOnly)
        );
        assert!(ProviderProfile::from_value(&json!("aws")).is_err());
        assert!(ProviderProfile::from_value(&json!(true)).is_err());
    }

    #[test]
    fn aws_only_admits_only_claude_code_on_a_bedrock_claude_model() {
        let aws = ProviderProfile::AwsOnly;
        for model in [
            "us.anthropic.claude-sonnet-5-v1:0",
            "anthropic.claude-haiku-4-5-20251001-v1:0",
            "global.anthropic.claude-opus-5-5",
        ] {
            assert!(
                launch_allowed(aws, SessionProvider::Claude, Some(model)),
                "{model}"
            );
        }
        assert!(!launch_allowed(
            aws,
            SessionProvider::Claude,
            Some("claude-opus-5-5")
        ));
        assert!(!launch_allowed(aws, SessionProvider::Claude, None));
        assert!(!launch_allowed(
            aws,
            SessionProvider::Claude,
            Some("global.openai.gpt-5.6-sol")
        ));
        for provider in [
            SessionProvider::Codex,
            SessionProvider::Bedrock,
            SessionProvider::Harness,
            SessionProvider::OpenRouter,
            SessionProvider::Local,
        ] {
            assert!(
                !launch_allowed(aws, provider, Some("us.anthropic.claude-sonnet-5-v1:0")),
                "{provider:?}"
            );
        }
    }

    #[test]
    fn all_admits_every_launch_and_every_picker_entry() {
        for provider in [
            SessionProvider::Claude,
            SessionProvider::Codex,
            SessionProvider::Local,
        ] {
            assert!(launch_allowed(ProviderProfile::All, provider, None));
            assert!(provider_offered(ProviderProfile::All, provider));
            assert!(picker_allows(ProviderProfile::All, provider, "gpt-6-sol"));
        }
    }

    #[test]
    fn aws_only_picker_offers_bedrock_claude_models_on_claude_and_bedrock() {
        let aws = ProviderProfile::AwsOnly;
        assert!(provider_offered(aws, SessionProvider::Claude));
        assert!(provider_offered(aws, SessionProvider::Bedrock));
        assert!(!provider_offered(aws, SessionProvider::Codex));
        assert!(picker_allows(
            aws,
            SessionProvider::Bedrock,
            AWS_ONLY_DEFAULT_MODEL
        ));
        assert!(!picker_allows(
            aws,
            SessionProvider::Bedrock,
            "global.openai.gpt-5.6-sol"
        ));
        assert!(!picker_allows(
            aws,
            SessionProvider::Claude,
            "claude-opus-5-5"
        ));
        assert!(!picker_allows(
            aws,
            SessionProvider::Codex,
            AWS_ONLY_DEFAULT_MODEL
        ));
        for (model, _) in AWS_ONLY_MODELS {
            assert!(
                picker_allows(aws, SessionProvider::Claude, model),
                "{model}"
            );
        }
    }

    #[test]
    fn refusal_carries_the_stable_code() {
        let reason = launch_refusal(
            ProviderProfile::AwsOnly,
            SessionProvider::Codex,
            Some("gpt-6-sol"),
        );
        assert!(reason.starts_with(PROVIDER_PROFILE_REFUSED), "{reason}");
        assert!(reason.contains("gpt-6-sol"));
        assert_eq!(launch_refusal_code(&reason), PROVIDER_PROFILE_REFUSED);
        assert_eq!(
            launch_refusal_code("launch_model_not_allowed: x"),
            crate::launch_allowlist::LAUNCH_MODEL_NOT_ALLOWED
        );
    }

    #[test]
    fn region_normalises_and_rejects_malformed_values() {
        assert_eq!(
            normalize_bedrock_region(&json!(" us-east-1 ")).unwrap(),
            "us-east-1"
        );
        assert_eq!(normalize_bedrock_region(&json!("")).unwrap(), "");
        assert_eq!(normalize_bedrock_region(&json!(null)).unwrap(), "");
        for bad in [
            json!("US-EAST-1"),
            json!("us east"),
            json!(1),
            json!("a".repeat(33)),
        ] {
            assert!(normalize_bedrock_region(&bad).is_err(), "{bad}");
        }
    }
}
