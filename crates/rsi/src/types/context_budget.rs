//! TUI-only projection of the daemon-resolved context budget.
//!
//! The daemon owns provider math and source resolution. This module only
//! labels the already-resolved facts for compact and detailed operator views.

use chrono::SecondsFormat;
use rsi_common::provider_capabilities::{
    CapabilityConfidence, CapabilitySource, ResolvedContextBudget,
};
use rsi_common::types::ContextUsageConfidence;

use crate::types::SessionState;

/// One positively labelled row shared by F3 Session Info and the wide inspector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextDetailRow {
    pub label: &'static str,
    pub value: String,
}

/// Display-only projection of daemon-owned context usage and capability facts.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextBudgetViewModel {
    pub percent: Option<f64>,
    pub usage_tokens: Option<u64>,
    pub usage_confidence: ContextUsageConfidence,
    pub resolved: Option<ResolvedContextBudget>,
}

impl ContextBudgetViewModel {
    /// Compact meter text. Usage confidence and capability provenance occupy
    /// separate suffix positions so, for example, `42%≈·R` cannot conflate an
    /// approximate numerator (`≈`) with repository-fallback evidence (`R`).
    #[must_use]
    pub fn compact_label(&self) -> Option<String> {
        if self.percent.is_none() && self.resolved.is_none() {
            return None;
        }

        let mut label = self
            .percent
            .map(|pct| {
                if pct > 0.0 && pct < 1.0 {
                    "<1%".to_string()
                } else {
                    format!("{:.0}%", pct.clamp(0.0, 100.0))
                }
            })
            .unwrap_or_else(|| "—".to_string());
        label.push_str(usage_indicator(self.usage_confidence));
        if let Some(resolved) = &self.resolved {
            label.push('·');
            label.push(capability_indicator(resolved.evidence.source));
        }
        Some(label)
    }

    /// Semantic detail rows. The active denominator comes exclusively from
    /// `ResolvedContextBudget.active_tokens`; maxima are labelled separately.
    #[must_use]
    pub fn detail_rows(&self) -> Vec<ContextDetailRow> {
        let mut rows = Vec::new();
        let Some(resolved) = &self.resolved else {
            if let Some(percent) = self.percent {
                rows.push(ContextDetailRow {
                    label: "context",
                    value: format!("{:.0}% runtime meter", percent.clamp(0.0, 100.0)),
                });
            }
            return rows;
        };

        let usage = self
            .usage_tokens
            .map(format_tokens)
            .unwrap_or_else(|| "—".to_string());
        rows.push(ContextDetailRow {
            label: "context",
            value: format!(
                "{usage} / {} {}",
                format_tokens(resolved.active_tokens),
                active_budget_label(resolved.evidence.source)
            ),
        });

        let capacity = &resolved.capacity;
        let mut provider = Vec::new();
        if let Some(tokens) = capacity.provider_default_tokens {
            provider.push(format!("default {}", format_tokens(tokens)));
        }
        if let Some(tokens) = capacity.provider_max_tokens {
            provider.push(format!("max {}", format_tokens(tokens)));
        }
        if let Some(percent) = capacity.effective_percent {
            provider.push(format!("effective factor {percent}%"));
        }
        if !provider.is_empty() {
            rows.push(ContextDetailRow {
                label: "provider",
                value: provider.join(" · "),
            });
        }

        let mut api = Vec::new();
        if let Some(tokens) = capacity.advertised_max_tokens {
            api.push(format!("context max {}", format_tokens(tokens)));
        }
        if let Some(tokens) = capacity.max_output_tokens {
            api.push(format!("output max {}", format_tokens(tokens)));
        }
        if !api.is_empty() {
            rows.push(ContextDetailRow {
                label: "API",
                value: api.join(" · "),
            });
        }

        if let Some(tokens) = capacity.compaction_limit_tokens {
            rows.push(ContextDetailRow {
                label: "compaction",
                value: format!("limit {}", format_tokens(tokens)),
            });
        }

        rows.push(ContextDetailRow {
            label: "source",
            value: capability_source_label(resolved.evidence.source).to_string(),
        });
        if let Some(version) = &resolved.evidence.source_version {
            rows.push(ContextDetailRow {
                label: "version",
                value: version.clone(),
            });
        }
        if let Some(digest) = &resolved.evidence.source_digest {
            rows.push(ContextDetailRow {
                label: "digest",
                value: digest.clone(),
            });
        }

        let mut freshness = vec![capability_freshness_label(resolved).to_string()];
        if let Some(observed_at) = resolved.evidence.observed_at {
            freshness.push(format!(
                "observed {}",
                observed_at.to_rfc3339_opts(SecondsFormat::Secs, true)
            ));
        }
        freshness.push(format!(
            "usage {}",
            usage_confidence_label(self.usage_confidence)
        ));
        rows.push(ContextDetailRow {
            label: "freshness",
            value: freshness.join(" · "),
        });

        rows
    }
}

#[must_use]
pub fn compute_context_budget_view(state: &SessionState) -> ContextBudgetViewModel {
    ContextBudgetViewModel {
        percent: state.live_context_pct.or(state.session.context_fill_pct),
        usage_tokens: state.session.input_tokens,
        usage_confidence: state.session.context_usage_confidence,
        resolved: state.session.resolved_context_budget.clone(),
    }
}

fn usage_indicator(confidence: ContextUsageConfidence) -> &'static str {
    match confidence {
        ContextUsageConfidence::Counted | ContextUsageConfidence::Full => "",
        ContextUsageConfidence::Partial => "≈",
        ContextUsageConfidence::Stale => "!",
        ContextUsageConfidence::Missing => "?",
        _ => "?",
    }
}

fn capability_indicator(source: CapabilitySource) -> char {
    match source {
        CapabilitySource::RuntimeTelemetry => 'T',
        CapabilitySource::Configured => 'K',
        CapabilitySource::ProviderCatalog => 'C',
        CapabilitySource::RepositoryFallback => 'R',
        CapabilitySource::LegacyUnverified => 'L',
        CapabilitySource::OfficialDocumentation => 'O',
    }
}

fn capability_source_label(source: CapabilitySource) -> &'static str {
    match source {
        CapabilitySource::RuntimeTelemetry => "runtime telemetry",
        CapabilitySource::Configured => "configured",
        CapabilitySource::ProviderCatalog => "provider catalog",
        CapabilitySource::RepositoryFallback => "repository fallback",
        CapabilitySource::LegacyUnverified => "legacy unverified",
        CapabilitySource::OfficialDocumentation => "official documentation",
    }
}

fn active_budget_label(source: CapabilitySource) -> &'static str {
    match source {
        CapabilitySource::RuntimeTelemetry => "runtime",
        CapabilitySource::Configured => "configured",
        CapabilitySource::ProviderCatalog => "provider default",
        CapabilitySource::RepositoryFallback => "repository fallback",
        CapabilitySource::LegacyUnverified => "legacy unverified",
        CapabilitySource::OfficialDocumentation => "documented maximum",
    }
}

fn capability_freshness_label(resolved: &ResolvedContextBudget) -> &'static str {
    match (resolved.evidence.source, resolved.evidence.confidence) {
        (
            CapabilitySource::RuntimeTelemetry | CapabilitySource::Configured,
            CapabilityConfidence::Authoritative,
        ) => "fresh",
        (CapabilitySource::ProviderCatalog, CapabilityConfidence::Verified) => "cold",
        (CapabilitySource::RepositoryFallback, _) => "degraded",
        (CapabilitySource::LegacyUnverified, _) => "legacy-unverified",
        (_, CapabilityConfidence::Authoritative) => "fresh",
        (_, CapabilityConfidence::Verified) => "verified",
        (_, CapabilityConfidence::Degraded) => "degraded",
    }
}

fn usage_confidence_label(confidence: ContextUsageConfidence) -> &'static str {
    match confidence {
        ContextUsageConfidence::Counted => "counted",
        ContextUsageConfidence::Full => "full",
        ContextUsageConfidence::Partial => "partial",
        ContextUsageConfidence::Stale => "stale",
        ContextUsageConfidence::Missing => "missing",
        _ => "unknown",
    }
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        let value = tokens as f64 / 1_000_000.0;
        return format!("{value:.2}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
            + "m";
    }
    if tokens >= 1_000 {
        return format!("{}k", tokens.saturating_add(500) / 1_000);
    }
    tokens.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::app_test_helpers::baseline_session;
    use rsi_common::provider_capabilities::{CapabilityEvidence, ContextCapacity};
    use rsi_common::types::SessionKind;
    use uuid::Uuid;

    fn budget(source: CapabilitySource, confidence: CapabilityConfidence) -> ResolvedContextBudget {
        ResolvedContextBudget {
            active_tokens: 258_400,
            capacity: ContextCapacity {
                advertised_max_tokens: Some(1_050_000),
                provider_default_tokens: Some(272_000),
                provider_max_tokens: Some(872_000),
                effective_percent: Some(95),
                configured_tokens: None,
                runtime_effective_tokens: Some(258_400),
                compaction_limit_tokens: Some(230_000),
                max_output_tokens: Some(128_000),
            },
            evidence: CapabilityEvidence {
                source,
                source_version: Some("codex-cli 0.155.1".to_string()),
                source_digest: Some(format!("sha256:{}", "a".repeat(64))),
                observed_at: Some(
                    "2026-09-02T12:34:56Z"
                        .parse()
                        .expect("fixed capability timestamp"),
                ),
                confidence,
            },
        }
    }

    fn view(
        source: CapabilitySource,
        capability_confidence: CapabilityConfidence,
        usage_confidence: ContextUsageConfidence,
        percent: Option<f64>,
    ) -> ContextBudgetViewModel {
        let mut session = baseline_session(Uuid::new_v4(), SessionKind::Standard);
        session.input_tokens = Some(34_000);
        session.context_usage_confidence = usage_confidence;
        session.resolved_context_budget = Some(budget(source, capability_confidence));
        let mut state = SessionState::new(session);
        state.live_context_pct = percent;
        compute_context_budget_view(&state)
    }

    #[test]
    fn detail_rows_keep_active_provider_api_and_compaction_semantics_distinct() {
        let view = view(
            CapabilitySource::RuntimeTelemetry,
            CapabilityConfidence::Authoritative,
            ContextUsageConfidence::Full,
            Some(9.0),
        );
        let rows = view.detail_rows();
        let value = |label| {
            rows.iter()
                .find(|row| row.label == label)
                .map(|row| row.value.as_str())
                .unwrap_or_else(|| panic!("missing {label} detail row"))
        };

        assert_eq!(value("context"), "34k / 258k runtime");
        assert_eq!(
            value("provider"),
            "default 272k · max 872k · effective factor 95%"
        );
        assert_eq!(value("API"), "context max 1.05m · output max 128k");
        assert_eq!(value("compaction"), "limit 230k");
        assert_eq!(value("source"), "runtime telemetry");
        assert_eq!(
            value("freshness"),
            "fresh · observed 2026-09-02T12:34:56Z · usage full"
        );
    }

    #[test]
    fn detail_rows_label_each_active_denominator_source() {
        for (source, confidence, expected) in [
            (
                CapabilitySource::RuntimeTelemetry,
                CapabilityConfidence::Authoritative,
                "34k / 258k runtime",
            ),
            (
                CapabilitySource::Configured,
                CapabilityConfidence::Authoritative,
                "34k / 258k configured",
            ),
            (
                CapabilitySource::ProviderCatalog,
                CapabilityConfidence::Verified,
                "34k / 258k provider default",
            ),
            (
                CapabilitySource::RepositoryFallback,
                CapabilityConfidence::Degraded,
                "34k / 258k repository fallback",
            ),
            (
                CapabilitySource::LegacyUnverified,
                CapabilityConfidence::Degraded,
                "34k / 258k legacy unverified",
            ),
        ] {
            let rows =
                view(source, confidence, ContextUsageConfidence::Full, Some(9.0)).detail_rows();
            assert_eq!(
                rows.iter()
                    .find(|row| row.label == "context")
                    .map(|row| row.value.as_str()),
                Some(expected)
            );
        }
    }

    #[test]
    fn compact_states_keep_usage_confidence_and_capability_source_separate() {
        let cases = [
            (
                CapabilitySource::RuntimeTelemetry,
                CapabilityConfidence::Authoritative,
                ContextUsageConfidence::Full,
                Some(42.0),
                "42%·T",
            ),
            (
                CapabilitySource::RuntimeTelemetry,
                CapabilityConfidence::Authoritative,
                ContextUsageConfidence::Stale,
                Some(42.0),
                "42%!·T",
            ),
            (
                CapabilitySource::ProviderCatalog,
                CapabilityConfidence::Verified,
                ContextUsageConfidence::Missing,
                None,
                "—?·C",
            ),
            (
                CapabilitySource::RepositoryFallback,
                CapabilityConfidence::Degraded,
                ContextUsageConfidence::Partial,
                Some(42.0),
                "42%≈·R",
            ),
            (
                CapabilitySource::LegacyUnverified,
                CapabilityConfidence::Degraded,
                ContextUsageConfidence::Missing,
                None,
                "—?·L",
            ),
        ];

        for (source, evidence, usage, percent, expected) in cases {
            let label = view(source, evidence, usage, percent)
                .compact_label()
                .expect("capability state has a compact label");
            assert_eq!(label, expected);
            assert!(
                label.chars().count() <= 7,
                "compact context label must fit the existing cell: {label}"
            );
        }
    }
}
