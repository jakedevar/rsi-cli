//! Shared provider capability and context-budget contracts.
//!
//! A model's advertised API envelope, an installed provider's defaults, and
//! an active session's runtime window answer different questions. These DTOs
//! keep those facts distinct while letting daemon, RPC, and TUI consumers
//! exchange one resolved active budget with explicit provenance.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

/// Semantically distinct context-capacity facts for one provider/model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextCapacity {
    /// Maximum context advertised by official product/API documentation.
    #[serde(default, deserialize_with = "deserialize_optional_positive_u64")]
    pub advertised_max_tokens: Option<u64>,
    /// Installed provider catalog's default context before its effective cap.
    #[serde(default, deserialize_with = "deserialize_optional_positive_u64")]
    pub provider_default_tokens: Option<u64>,
    /// Installed provider catalog's largest supported context.
    #[serde(default, deserialize_with = "deserialize_optional_positive_u64")]
    pub provider_max_tokens: Option<u64>,
    /// Provider percentage applied to the default/configured context window.
    #[serde(default, deserialize_with = "deserialize_optional_percent")]
    pub effective_percent: Option<u8>,
    /// Explicit context configured for this provider/session.
    #[serde(default, deserialize_with = "deserialize_optional_positive_u64")]
    pub configured_tokens: Option<u64>,
    /// Effective context reported for the active runtime session.
    #[serde(default, deserialize_with = "deserialize_optional_positive_u64")]
    pub runtime_effective_tokens: Option<u64>,
    /// Provider compaction threshold, distinct from the active window.
    #[serde(default, deserialize_with = "deserialize_optional_positive_u64")]
    pub compaction_limit_tokens: Option<u64>,
    /// Maximum output advertised for the model/API, when known.
    #[serde(default, deserialize_with = "deserialize_optional_positive_u64")]
    pub max_output_tokens: Option<u64>,
}

impl ContextCapacity {
    /// Resolve the installed provider's effective default without conflating it
    /// with the provider maximum or the advertised API envelope.
    #[must_use]
    pub fn provider_effective_default_tokens(&self) -> Option<u64> {
        let default = self.provider_default_tokens?;
        let percent = u64::from(self.effective_percent.unwrap_or(100));
        default.checked_mul(percent).map(|tokens| tokens / 100)
    }
}

/// Closed provenance vocabulary for an active context budget.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySource {
    /// Vendor documentation; descriptive unless provider/config/runtime agrees.
    OfficialDocumentation,
    /// Exact installed provider catalog.
    ProviderCatalog,
    /// Explicit provider/session configuration.
    Configured,
    /// Active provider session telemetry.
    RuntimeTelemetry,
    /// Versioned repository fallback used before stronger evidence arrives.
    RepositoryFallback,
    /// Historical or otherwise unattributed numeric value.
    #[default]
    LegacyUnverified,
}

impl CapabilitySource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OfficialDocumentation => "official_documentation",
            Self::ProviderCatalog => "provider_catalog",
            Self::Configured => "configured",
            Self::RuntimeTelemetry => "runtime_telemetry",
            Self::RepositoryFallback => "repository_fallback",
            Self::LegacyUnverified => "legacy_unverified",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "official_documentation" => Ok(Self::OfficialDocumentation),
            "provider_catalog" => Ok(Self::ProviderCatalog),
            "configured" => Ok(Self::Configured),
            "runtime_telemetry" => Ok(Self::RuntimeTelemetry),
            "repository_fallback" => Ok(Self::RepositoryFallback),
            "legacy_unverified" => Ok(Self::LegacyUnverified),
            _ => Err("invalid capability source".to_string()),
        }
    }
}

/// Confidence in the evidence behind a resolved budget.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityConfidence {
    /// Exact fact for the active session.
    Authoritative,
    /// Versioned provider/product fact, but not live-session telemetry.
    Verified,
    /// Fallback or unattributed historical value.
    #[default]
    Degraded,
}

/// Provenance attached to one resolved context budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityEvidence {
    pub source: CapabilitySource,
    #[serde(default)]
    pub source_version: Option<String>,
    #[serde(default)]
    pub source_digest: Option<String>,
    #[serde(default)]
    pub observed_at: Option<DateTime<Utc>>,
    pub confidence: CapabilityConfidence,
}

impl CapabilityEvidence {
    #[must_use]
    pub const fn legacy_unverified() -> Self {
        Self {
            source: CapabilitySource::LegacyUnverified,
            source_version: None,
            source_digest: None,
            observed_at: None,
            confidence: CapabilityConfidence::Degraded,
        }
    }
}

/// One active context denominator plus its semantically separated envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedContextBudget {
    #[serde(deserialize_with = "deserialize_positive_u64")]
    pub active_tokens: u64,
    #[serde(default)]
    pub capacity: ContextCapacity,
    pub evidence: CapabilityEvidence,
}

impl ResolvedContextBudget {
    pub fn new(
        active_tokens: u64,
        capacity: ContextCapacity,
        evidence: CapabilityEvidence,
    ) -> Result<Self, String> {
        if active_tokens == 0 {
            return Err("active context budget must be positive".to_string());
        }
        Ok(Self {
            active_tokens,
            capacity,
            evidence,
        })
    }

    /// Only active-session/config evidence may drive threshold rotation.
    #[must_use]
    pub const fn authorizes_threshold_rotation(&self) -> bool {
        matches!(
            self.evidence.source,
            CapabilitySource::RuntimeTelemetry | CapabilitySource::Configured
        ) && matches!(
            self.evidence.confidence,
            CapabilityConfidence::Authoritative
        )
    }
}

fn deserialize_optional_positive_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<u64>::deserialize(deserializer)?;
    if value == Some(0) {
        return Err(serde::de::Error::custom(
            "context token values must be positive",
        ));
    }
    Ok(value)
}

fn deserialize_positive_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = u64::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom(
            "active context budget must be positive",
        ));
    }
    Ok(value)
}

fn deserialize_optional_percent<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<u8>::deserialize(deserializer)?;
    if value.is_some_and(|percent| !(1..=100).contains(&percent)) {
        return Err(serde::de::Error::custom(
            "effective context percentage must be in 1..=100",
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_serde_preserves_missing_optional_fields() {
        let capacity: ContextCapacity = serde_json::from_str("{}").unwrap();
        assert_eq!(capacity, ContextCapacity::default());
        assert_eq!(
            serde_json::to_value(capacity).unwrap()["provider_max_tokens"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn capacity_rejects_zero_overflow_and_invalid_percentages() {
        for raw in [
            r#"{"provider_default_tokens":0}"#,
            r#"{"provider_max_tokens":18446744073709551616}"#,
            r#"{"effective_percent":0}"#,
            r#"{"effective_percent":101}"#,
        ] {
            assert!(
                serde_json::from_str::<ContextCapacity>(raw).is_err(),
                "invalid capacity must fail: {raw}"
            );
        }
    }

    #[test]
    fn provider_default_and_effective_percent_stay_distinct() {
        let capacity = ContextCapacity {
            provider_default_tokens: Some(272_000),
            provider_max_tokens: Some(872_000),
            effective_percent: Some(95),
            advertised_max_tokens: Some(1_050_000),
            ..ContextCapacity::default()
        };

        assert_eq!(capacity.provider_effective_default_tokens(), Some(258_400));
        assert_eq!(capacity.provider_max_tokens, Some(872_000));
        assert_eq!(capacity.advertised_max_tokens, Some(1_050_000));
    }

    #[test]
    fn capability_source_serde_is_closed_and_stable() {
        assert_eq!(
            serde_json::to_string(&CapabilitySource::RuntimeTelemetry).unwrap(),
            r#""runtime_telemetry""#
        );
        assert!(serde_json::from_str::<CapabilitySource>(r#""guessed""#).is_err());
    }

    #[test]
    fn only_authoritative_session_evidence_allows_rotation() {
        let capacity = ContextCapacity {
            runtime_effective_tokens: Some(258_400),
            ..ContextCapacity::default()
        };
        let runtime = ResolvedContextBudget::new(
            258_400,
            capacity.clone(),
            CapabilityEvidence {
                source: CapabilitySource::RuntimeTelemetry,
                source_version: Some("codex-cli 0.155.1".to_string()),
                source_digest: None,
                observed_at: Some(Utc::now()),
                confidence: CapabilityConfidence::Authoritative,
            },
        )
        .unwrap();
        assert!(runtime.authorizes_threshold_rotation());

        let fallback = ResolvedContextBudget::new(
            258_400,
            capacity,
            CapabilityEvidence {
                source: CapabilitySource::RepositoryFallback,
                source_version: Some("rsi-provider-capabilities-v1".to_string()),
                source_digest: None,
                observed_at: Some(Utc::now()),
                confidence: CapabilityConfidence::Degraded,
            },
        )
        .unwrap();
        assert!(!fallback.authorizes_threshold_rotation());
    }
}
