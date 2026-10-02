//! Amazon Bedrock model-ID grammar shared by the daemon and the TUI.
//!
//! Bedrock addresses a model as `[<geo>.]<vendor>.<name>[-v<N>[:<M>]]`, e.g.
//! `global.openai.gpt-5.6-sol`, `us.anthropic.claude-sonnet-5-v1:0` or the
//! bare foundation-model form `anthropic.claude-haiku-4-5-20251001-v1:0`.

/// Which vendor API family a Bedrock model speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BedrockVendor {
    /// OpenAI GPT models (Responses / Chat Completions under `/openai/v1`).
    OpenAi,
    /// Anthropic Claude models (Messages format through `InvokeModel`).
    Anthropic,
}

/// The `<vendor>.<name>` remainder after an optional geography prefix.
fn vendor_and_name(model: &str) -> Option<(&str, &str)> {
    let model = model.trim();
    let (first, rest) = model.split_once('.')?;
    if first == "openai" || first == "anthropic" {
        return Some((first, rest));
    }
    // A geography prefix is a short lowercase word (`us`, `eu`, `apac`, `global`).
    if first.is_empty() || !first.bytes().all(|byte| byte.is_ascii_lowercase()) {
        return None;
    }
    rest.split_once('.')
}

/// The vendor family of a Bedrock model ID, or `None` for anything that is not
/// a Bedrock OpenAI GPT or Anthropic Claude model ID.
///
/// GPT OSS is excluded: its Responses support lives on `bedrock-mantle`, not
/// the `bedrock-runtime` endpoints RSI uses.
#[must_use]
pub fn bedrock_vendor(model: &str) -> Option<BedrockVendor> {
    match vendor_and_name(model)? {
        ("openai", name) if name.starts_with("gpt-") && !name.starts_with("gpt-oss") => {
            Some(BedrockVendor::OpenAi)
        }
        ("anthropic", name) if name.starts_with("claude-") => Some(BedrockVendor::Anthropic),
        _ => None,
    }
}

/// The Anthropic model name inside a Bedrock Claude ID, without the Bedrock
/// `-v<N>[:<M>]` revision: `us.anthropic.claude-opus-5-5-v1:0` →
/// `claude-opus-5-5`. Dated IDs keep their date so they still match the
/// catalog (`claude-haiku-4-5-20251001`).
#[must_use]
pub fn anthropic_model_name(model: &str) -> Option<&str> {
    if bedrock_vendor(model) != Some(BedrockVendor::Anthropic) {
        return None;
    }
    let (_, name) = vendor_and_name(model)?;
    let name = name.split_once(':').map_or(name, |(base, _)| base);
    let name = match name.rsplit_once("-v") {
        Some((base, revision))
            if !revision.is_empty() && revision.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            base
        }
        _ => name,
    };
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_profile_and_foundation_ids() {
        assert_eq!(
            bedrock_vendor("global.openai.gpt-5.6-sol"),
            Some(BedrockVendor::OpenAi)
        );
        assert_eq!(
            bedrock_vendor("us.anthropic.claude-sonnet-5-v1:0"),
            Some(BedrockVendor::Anthropic)
        );
        assert_eq!(
            bedrock_vendor("anthropic.claude-haiku-4-5-20251001-v1:0"),
            Some(BedrockVendor::Anthropic)
        );
        assert_eq!(bedrock_vendor("us.openai.gpt-oss-120b"), None);
        assert_eq!(bedrock_vendor("us.meta.llama4-maverick-v1:0"), None);
        assert_eq!(bedrock_vendor("claude-sonnet-5"), None);
        assert_eq!(bedrock_vendor("gpt-6-astra"), None);
        assert_eq!(bedrock_vendor("openai/gpt-6-astra"), None);
    }

    #[test]
    fn anthropic_name_drops_bedrock_revision_only() {
        assert_eq!(
            anthropic_model_name("us.anthropic.claude-opus-5-5-v1:0"),
            Some("claude-opus-5-5")
        );
        assert_eq!(
            anthropic_model_name("anthropic.claude-haiku-4-5-20251001-v1:0"),
            Some("claude-haiku-4-5-20251001")
        );
        assert_eq!(
            anthropic_model_name("global.anthropic.claude-sonnet-5"),
            Some("claude-sonnet-5")
        );
        assert_eq!(anthropic_model_name("global.openai.gpt-5.6-sol"), None);
    }
}
