//! Table of 80+ OpenAI-compatible services with per-provider quirk flags. The
//! table lives in `store_support::compatible_table`; this module re-exports it.

pub use crate::store_support::compatible_table::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn test_lookup_by_name() {
        let entry = lookup("groq").unwrap();
        assert_eq!(entry.name, "groq");
        assert!(entry.quirks.native_tools);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn test_lookup_by_model_prefix() {
        // "deepseek-chat" should match "deepseek"
        let entry = lookup("deepseek-chat").unwrap();
        assert_eq!(entry.name, "deepseek");
        assert!(entry.quirks.strip_think_tags);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn test_lookup_mercury_routes_to_inception() {
        let entry = lookup("mercury-2").unwrap();
        assert_eq!(entry.name, "mercury");
        assert_eq!(entry.base_url, "https://api.inceptionlabs.ai/v1");
        assert_eq!(entry.env_vars, &["INCEPTION_API_KEY", "MERCURY_API_KEY"]);
        assert!(entry.quirks.native_tools);
    }

    /// #2253: the Mercury route's wire profile, and no other route's changes.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn mercury_entry_carries_the_inception_wire_profile() {
        let quirks = &lookup("mercury-2.5").unwrap().quirks;
        assert_eq!(
            quirks.reasoning_effort,
            ReasoningEffortWire::OpenAiWithInstant
        );
        assert_eq!(quirks.max_tokens_field, MaxTokensField::MaxCompletionTokens);
        assert_eq!(quirks.default_max_output_tokens, Some(16_384));
        assert!(quirks.tool_choice_none);
        for entry in COMPATIBLE_TABLE
            .iter()
            .filter(|entry| entry.name != "mercury")
        {
            let quirks = &entry.quirks;
            assert_eq!(
                quirks.reasoning_effort,
                ReasoningEffortWire::None,
                "{}",
                entry.name
            );
            assert_eq!(
                quirks.max_tokens_field,
                MaxTokensField::MaxTokens,
                "{}",
                entry.name
            );
            assert_eq!(quirks.default_max_output_tokens, None, "{}", entry.name);
            assert!(!quirks.tool_choice_none, "{}", entry.name);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn reasoning_effort_wire_values() {
        use ReasoningEffortWire::{None as NoEffort, OpenAi, OpenAiWithInstant, OpenAiWithNone};
        for (wire, effort, expected) in [
            (OpenAiWithInstant, "minimal", Some("instant")),
            (OpenAiWithInstant, "none", Some("instant")),
            (OpenAiWithInstant, "low", Some("low")),
            (OpenAiWithInstant, "medium", Some("medium")),
            (OpenAiWithInstant, "high", Some("high")),
            (OpenAiWithInstant, "xhigh", Some("high")),
            (OpenAiWithInstant, "max", Some("high")),
            (OpenAiWithInstant, " High ", Some("high")),
            (OpenAiWithInstant, "ultra", None),
            (OpenAi, "minimal", Some("low")),
            (OpenAi, "none", Some("low")),
            (OpenAi, "medium", Some("medium")),
            (OpenAi, "max", Some("high")),
            (NoEffort, "high", None),
            (OpenAiWithNone, "none", Some("none")),
            (OpenAiWithNone, "minimal", Some("none")),
            (OpenAiWithNone, "high", Some("high")),
            (OpenAiWithNone, "ultra", None),
        ] {
            assert_eq!(wire.wire_value(effort), expected, "{wire:?} {effort}");
        }
        assert_eq!(MaxTokensField::MaxTokens.field_name(), "max_tokens");
        assert_eq!(
            MaxTokensField::MaxCompletionTokens.field_name(),
            "max_completion_tokens"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn test_lookup_case_insensitive() {
        assert!(lookup("Groq").is_some());
        assert!(lookup("DEEPSEEK").is_some());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn test_lookup_unknown_returns_none() {
        assert!(lookup("unknown-provider-xyz").is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn test_perplexity_no_streaming() {
        let entry = lookup("perplexity").unwrap();
        assert!(entry.quirks.disable_streaming);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn test_local_providers_no_auth() {
        for name in &["lmstudio", "llamacpp", "vllm", "ollama"] {
            let entry = lookup(name).unwrap();
            assert!(
                matches!(entry.quirks.auth_style, AuthStyle::None),
                "{name} should use AuthStyle::None"
            );
        }
    }
}
