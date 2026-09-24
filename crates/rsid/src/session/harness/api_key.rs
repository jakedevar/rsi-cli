//! API key resolution with cascading fallbacks.
//!
//! Resolution order:
//! 1. Explicit key (from LaunchConfig)
//! 2. Provider-specific env vars
//! 3. Generic fallbacks (RSI_API_KEY, MOTHERSHIP_API_KEY, FLYWHEEL_API_KEY, API_KEY)

// Provider layer is built ahead of resolve_provider() wiring (Phase 6+).
#![allow(dead_code)]

/// Resolve an API key for a provider.
///
/// Returns `None` if no key is found (valid for local models).
pub fn resolve_api_key(explicit: Option<&str>, provider_env_vars: &[&str]) -> Option<String> {
    // 1. Explicit key takes priority
    if let Some(key) = explicit.filter(|k| !k.is_empty()) {
        return Some(key.to_string());
    }

    // 2. Provider-specific env vars
    for var in provider_env_vars {
        if let Ok(key) = std::env::var(var)
            && !key.is_empty()
        {
            return Some(key);
        }
    }

    // 3. Generic fallbacks
    for var in &[
        "RSI_API_KEY",
        "MOTHERSHIP_API_KEY",
        "FLYWHEEL_API_KEY",
        "API_KEY",
    ] {
        if let Ok(key) = std::env::var(var)
            && !key.is_empty()
        {
            return Some(key);
        }
    }

    None
}

/// Anthropic API key env vars.
pub const ANTHROPIC_ENV_VARS: &[&str] = &["ANTHROPIC_API_KEY"];

/// OpenAI API key env vars.
pub const OPENAI_ENV_VARS: &[&str] = &["OPENAI_API_KEY"];

/// Groq API key env vars.
pub const GROQ_ENV_VARS: &[&str] = &["GROQ_API_KEY"];

/// Mistral API key env vars.
pub const MISTRAL_ENV_VARS: &[&str] = &["MISTRAL_API_KEY"];

/// DeepSeek API key env vars.
pub const DEEPSEEK_ENV_VARS: &[&str] = &["DEEPSEEK_API_KEY"];

/// xAI/Grok API key env vars.
pub const XAI_ENV_VARS: &[&str] = &["XAI_API_KEY", "GROK_API_KEY"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_explicit_key_takes_priority() {
        assert_eq!(
            resolve_api_key(Some("explicit-key"), &["NONEXISTENT_VAR"]),
            Some("explicit-key".to_string()),
        );
    }

    #[test]
    fn test_empty_explicit_falls_through() {
        assert_eq!(resolve_api_key(Some(""), &["NONEXISTENT_VAR"]), None);
    }

    #[test]
    fn test_none_explicit_falls_through() {
        assert_eq!(resolve_api_key(None, &["NONEXISTENT_VAR"]), None);
    }
}
