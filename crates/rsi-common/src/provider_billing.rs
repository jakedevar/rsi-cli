//! Billing classification (#3533). Provider telemetry is not a bill.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingMode {
    Subscription,
    MeteredApi,
    Cloud,
}

impl BillingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Subscription => "subscription",
            Self::MeteredApi => "metered_api",
            Self::Cloud => "cloud",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "subscription" => Some(Self::Subscription),
            "metered_api" => Some(Self::MeteredApi),
            "cloud" => Some(Self::Cloud),
            _ => None,
        }
    }
    pub fn is_metered(self) -> bool {
        self != Self::Subscription
    }
}

pub const BILLING_PROVIDERS: &[&str] = &[
    "claude",
    "codex",
    "codexappserver",
    "pioneer",
    "openrouter",
    "bedrock",
    "local",
    "antigravity",
    "harness",
];
pub const BILLING_OVERRIDE_PROVIDERS: &[&str] = &[
    "claude",
    "codex",
    "codexappserver",
    "pioneer",
    "antigravity",
];
pub const BILLING_OPTIONS: &[&str] = &["auto", "subscription", "metered_api", "cloud"];

/// Cloud model routes remain cloud even when launched by a subscription CLI.
/// For an unprobed CLI login, conservatively withhold dollar accounting until
/// an API login is detected or the operator sets an override.
pub fn resolve(
    provider: &str,
    model: Option<&str>,
    configured: Option<&str>,
    login: Option<&str>,
) -> BillingMode {
    use BillingMode::*;
    let provider = provider.to_ascii_lowercase();
    if provider == "bedrock"
        || model.is_some_and(|m| {
            crate::bedrock_model::bedrock_vendor(m.strip_prefix("bedrock/").unwrap_or(m)).is_some()
        })
    {
        return Cloud;
    }
    if matches!(provider.as_str(), "openrouter" | "harness" | "local") {
        return MeteredApi;
    }
    if let Some(mode) = configured.and_then(BillingMode::parse) {
        return mode;
    }
    match provider.as_str() {
        "claude" | "codex" | "codexappserver" => match login {
            Some("api_key" | "api_key_helper") => MeteredApi,
            Some("third_party") => Cloud,
            _ => Subscription,
        },
        "antigravity" => Subscription,
        _ => MeteredApi,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn login_and_override_classify_subscription_api_and_cloud() {
        for (provider, login, want) in [
            ("Claude", "claude.ai", BillingMode::Subscription),
            ("Claude", "oauth_token", BillingMode::Subscription),
            ("Codex", "chatgpt", BillingMode::Subscription),
            ("Claude", "api_key", BillingMode::MeteredApi),
            ("Codex", "api_key", BillingMode::MeteredApi),
            ("OpenRouter", "unknown", BillingMode::MeteredApi),
            ("Bedrock", "unknown", BillingMode::Cloud),
        ] {
            assert_eq!(resolve(provider, None, None, Some(login)), want);
        }
        assert_eq!(
            resolve("Claude", None, Some("metered_api"), Some("claude.ai")),
            BillingMode::MeteredApi
        );
        assert_eq!(
            resolve(
                "Claude",
                Some("us.anthropic.claude-sonnet-4-20250514-v1:0"),
                Some("subscription"),
                Some("claude.ai")
            ),
            BillingMode::Cloud
        );
    }
}
