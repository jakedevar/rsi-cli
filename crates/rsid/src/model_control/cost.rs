//! Per-invocation cost resolution (#584, #585).
//!
//! One rule decides `model_invocations.estimated_cost_usd` for every provider
//! path that settles through [`crate::model_control::call_control`]:
//!
//! 1. a **provider-reported** cost (for example OpenRouter `usage.cost`) is
//!    used verbatim;
//! 2. a loopback `Local` model costs a real `0.0`;
//! 3. otherwise the cost is **estimated** from token usage with a price from
//!    [`price_for_model`] (documented list prices only);
//! 4. a model with no known price stays `None` (NULL). A price is never
//!    invented.
//!
//! The session total (`sessions.cost_usd`) for these providers is the sum of
//! the session's settled invocation costs, so both columns always agree.

use crate::model_control::call_control::ModelCallUsage;

/// USD per 1M tokens for one model. `cache_read` is `None` when the source
/// documents no cached-input rate; a usage with cache-read tokens then has no
/// sound estimate and resolves to `None` rather than a guess.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ModelPrice {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cache_read_per_mtok: Option<f64>,
}

/// Documented list prices, USD per 1M tokens, keyed by model id.
///
/// Source: OpenRouter `GET https://openrouter.ai/api/v1/models`, fetched
/// 2026-09-24T02:48Z (`pricing.prompt`, `pricing.completion`,
/// `pricing.input_cache_read`, converted per-token -> per-1M), recorded with
/// the same values in `scripts/bench-session-metrics.py`.
///
/// Every other model in use (Codex `gpt-*`, direct-API Harness `gpt-*` and
/// `claude-*`, any unlisted OpenRouter id) has no price documented in this
/// repo and therefore stays NULL until a cited price is added here.
const PRICES: &[(&str, ModelPrice)] = &[
    (
        "z-ai/glm-5.3-flashx",
        ModelPrice {
            input_per_mtok: 0.37,
            output_per_mtok: 1.25,
            cache_read_per_mtok: Some(0.09),
        },
    ),
    (
        "deepseek/deepseek-v4.1-flash",
        ModelPrice {
            input_per_mtok: 0.14,
            output_per_mtok: 0.42,
            cache_read_per_mtok: Some(0.0042),
        },
    ),
    (
        "qwen/qwen3.8-flash",
        ModelPrice {
            input_per_mtok: 0.15,
            output_per_mtok: 0.47,
            cache_read_per_mtok: Some(0.016),
        },
    ),
    (
        "minimax/minimax-m3",
        ModelPrice {
            input_per_mtok: 0.30,
            output_per_mtok: 1.20,
            cache_read_per_mtok: Some(0.06),
        },
    ),
    (
        "qwen/qwen3-coder-next",
        ModelPrice {
            input_per_mtok: 0.12,
            output_per_mtok: 0.80,
            cache_read_per_mtok: Some(0.07),
        },
    ),
];

/// The documented price for `model`, or `None` when none is known.
pub(crate) fn price_for_model(model: &str) -> Option<ModelPrice> {
    PRICES
        .iter()
        .find(|(id, _)| *id == model)
        .map(|(_, price)| *price)
}

/// Whether `provider` is the loopback `Local` backend (free by construction).
fn is_local_provider(provider: &str) -> bool {
    provider.eq_ignore_ascii_case("local")
}

fn reported_cost(usage: &ModelCallUsage) -> Option<f64> {
    usage
        .estimated_cost_usd
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
}

/// Estimate from token usage with a documented price; `None` without one.
///
/// The OpenAI-compatible `input_tokens` already include cached prompt tokens
/// (`cache_read_tokens` is a subset), so uncached input is the difference.
/// Reasoning tokens are part of `output_tokens` and are not added again.
fn estimate_from_tokens(model: &str, usage: &ModelCallUsage) -> Option<f64> {
    let price = price_for_model(model)?;
    let input = usage.input_tokens?;
    let output = usage.output_tokens?;
    let cache_read = usage.cache_read_tokens.unwrap_or(0);
    let cache_read_cost = match (cache_read, price.cache_read_per_mtok) {
        (0, _) => 0.0,
        (tokens, Some(rate)) => tokens as f64 * rate,
        (_, None) => return None,
    };
    let uncached_input = input.saturating_sub(cache_read);
    let micro_usd = uncached_input as f64 * price.input_per_mtok
        + output as f64 * price.output_per_mtok
        + cache_read_cost;
    Some(micro_usd / 1_000_000.0)
}

/// Resolve the cost recorded for one settled invocation (module docs).
pub(crate) fn resolve_invocation_cost(
    provider: &str,
    model: Option<&str>,
    usage: &ModelCallUsage,
) -> Option<f64> {
    if let Some(cost) = reported_cost(usage) {
        return Some(cost);
    }
    if is_local_provider(provider) {
        return Some(0.0);
    }
    estimate_from_tokens(model?, usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64) -> ModelCallUsage {
        ModelCallUsage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            ..ModelCallUsage::default()
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn provider_reported_cost_is_used_verbatim_over_any_estimate() {
        let mut call = usage(1_000_000, 1_000_000);
        call.estimated_cost_usd = Some(0.0123);
        let cost = resolve_invocation_cost("OpenRouter", Some("minimax/minimax-m3"), &call);
        assert_eq!(cost, Some(0.0123));
        // A reported cost is verbatim even when the model has no known price.
        let cost = resolve_invocation_cost("Codex", Some("unpriced-model"), &call);
        assert_eq!(cost, Some(0.0123));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn known_price_estimates_cost_from_tokens() {
        // minimax/minimax-m3: $0.30 in / $1.20 out per 1M tokens.
        let cost = resolve_invocation_cost(
            "OpenRouter",
            Some("minimax/minimax-m3"),
            &usage(2_000_000, 500_000),
        )
        .expect("known price estimates");
        assert!((cost - (0.60 + 0.60)).abs() < 1e-9, "cost was {cost}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn cache_read_tokens_are_billed_at_the_cached_rate_inside_the_prompt() {
        // 1M prompt tokens of which 600k are cache reads at $0.06; 400k plain
        // input at $0.30; 100k output at $1.20.
        let mut call = usage(1_000_000, 100_000);
        call.cache_read_tokens = Some(600_000);
        let cost = resolve_invocation_cost("OpenRouter", Some("minimax/minimax-m3"), &call)
            .expect("known price estimates");
        let expected = 0.4 * 0.30 + 0.6 * 0.06 + 0.1 * 1.20;
        assert!((cost - expected).abs() < 1e-9, "cost was {cost}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn unknown_price_stays_null_for_every_provider_path() {
        for (provider, model) in [
            ("Codex", "gpt-6-astra"),
            ("CodexAppServer", "gpt-6-sol"),
            ("Harness", "gpt-5.4"),
            ("OpenRouter", "vendor/unpriced-model"),
        ] {
            let cost = resolve_invocation_cost(provider, Some(model), &usage(1000, 1000));
            assert_eq!(cost, None, "{provider}/{model} has no documented price");
        }
        assert_eq!(
            resolve_invocation_cost("Harness", None, &usage(1000, 1000)),
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn missing_usage_does_not_guess_a_cost() {
        let model = Some("minimax/minimax-m3");
        assert_eq!(
            resolve_invocation_cost("OpenRouter", model, &ModelCallUsage::default()),
            None
        );
        assert!(price_for_model("minimax/minimax-m3").is_some());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn local_models_cost_a_real_zero() {
        assert_eq!(
            resolve_invocation_cost("Local", Some("qwen3:14b"), &usage(5000, 700)),
            Some(0.0)
        );
        // Even with no usage at all: free is not unknown.
        assert_eq!(
            resolve_invocation_cost("Local", None, &ModelCallUsage::default()),
            Some(0.0)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn negative_or_non_finite_reported_cost_is_ignored() {
        let mut call = usage(1_000_000, 0);
        call.estimated_cost_usd = Some(f64::NAN);
        let cost = resolve_invocation_cost("OpenRouter", Some("minimax/minimax-m3"), &call);
        assert!((cost.expect("non-finite falls back to estimate") - 0.30).abs() < 1e-9);
        call.estimated_cost_usd = Some(-1.0);
        let cost = resolve_invocation_cost("Codex", Some("gpt-6-astra"), &call);
        assert_eq!(cost, None);
    }
}
