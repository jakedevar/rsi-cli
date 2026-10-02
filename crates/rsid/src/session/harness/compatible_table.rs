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
