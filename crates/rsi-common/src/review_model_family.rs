//! Bounded provider-family classification for independent review admission.

use crate::types::SessionProvider;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewModelFamily {
    Anthropic,
    OpenAI,
    Google,
    ZAi,
    DeepSeek,
    Qwen,
    Meta,
    Mistral,
    Unknown,
}

/// A recognized model identity wins over the transport provider. An unrecognized
/// nonempty model is unknown, even on a vendor-specific transport.
pub fn review_model_family(provider: SessionProvider, model: Option<&str>) -> ReviewModelFamily {
    let model = model.map(str::trim).filter(|model| !model.is_empty());
    if let Some(model) = model {
        let lower = model.to_ascii_lowercase();
        let name = lower.rsplit('/').next().unwrap_or(&lower);
        if name.starts_with("claude-")
            || ["opus", "sonnet", "haiku", "fable"]
                .iter()
                .any(|family| name.starts_with(family))
        {
            return ReviewModelFamily::Anthropic;
        }
        if name.starts_with("gpt-")
            || name.starts_with("codex")
            || (name.starts_with('o') && name.as_bytes().get(1).is_some_and(u8::is_ascii_digit))
        {
            return ReviewModelFamily::OpenAI;
        }
        if name.starts_with("gemini-") {
            return ReviewModelFamily::Google;
        }
        if name.starts_with("glm-") {
            return ReviewModelFamily::ZAi;
        }
        if name.starts_with("deepseek-") {
            return ReviewModelFamily::DeepSeek;
        }
        if name.starts_with("qwen") {
            return ReviewModelFamily::Qwen;
        }
        if name.starts_with("llama") {
            return ReviewModelFamily::Meta;
        }
        if ["mistral", "mixtral", "codestral", "ministral", "pixtral"]
            .iter()
            .any(|family| name.starts_with(family))
        {
            return ReviewModelFamily::Mistral;
        }
        return ReviewModelFamily::Unknown;
    }
    match provider {
        SessionProvider::Claude => ReviewModelFamily::Anthropic,
        SessionProvider::Codex | SessionProvider::CodexAppServer => ReviewModelFamily::OpenAI,
        SessionProvider::Antigravity => ReviewModelFamily::Google,
        _ => ReviewModelFamily::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_wins_over_transport_and_vendor_prefix_is_stripped() {
        assert_eq!(
            review_model_family(SessionProvider::Codex, Some("sonnet")),
            ReviewModelFamily::Anthropic
        );
        assert_eq!(
            review_model_family(SessionProvider::OpenRouter, Some("z-ai/glm-5.3-flashx")),
            ReviewModelFamily::ZAi
        );
        assert_eq!(
            review_model_family(SessionProvider::OpenRouter, Some("openai/gpt-6-sol")),
            ReviewModelFamily::OpenAI
        );
        assert_eq!(
            review_model_family(
                SessionProvider::Pioneer,
                Some("deepseek-ai/DeepSeek-V4-Flash")
            ),
            ReviewModelFamily::DeepSeek
        );
        assert_eq!(
            review_model_family(SessionProvider::Local, Some("mystery")),
            ReviewModelFamily::Unknown
        );
    }

    #[test]
    fn supported_families_and_missing_models_are_bounded() {
        for (model, expected) in [
            ("o3", ReviewModelFamily::OpenAI),
            ("gemini-3.5-pro-high", ReviewModelFamily::Google),
            ("qwen3-coder", ReviewModelFamily::Qwen),
            ("llama-4", ReviewModelFamily::Meta),
            ("mistral-large", ReviewModelFamily::Mistral),
        ] {
            assert_eq!(
                review_model_family(SessionProvider::Local, Some(model)),
                expected
            );
        }
        assert_eq!(
            review_model_family(SessionProvider::Claude, None),
            ReviewModelFamily::Anthropic
        );
        assert_eq!(
            review_model_family(SessionProvider::Pioneer, None),
            ReviewModelFamily::Unknown
        );
        assert_eq!(
            review_model_family(SessionProvider::Codex, Some("mystery")),
            ReviewModelFamily::Unknown
        );
    }
}
