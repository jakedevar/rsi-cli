//! Utilities for parsing and comparing model identifiers.
//!
//! Handles IDs like:
//!   "claude-opus-4-6"             → (Opus, 4, 6)
//!   "claude-opus-4-7-200k"        → (Opus, 4, 7)
//!   "claude-opus-5"               → (Opus, 5, 0)
//!   "claude-sonnet-5"             → (Sonnet, 5, 0)
//!   "claude-haiku-5"              → (Haiku, 5, 0)
//!   "claude-opus-5-9"             → (Opus, 5, 9)

use crate::types::SessionProvider;

/// The backend that produced a model's weights, used to keep review authors
/// and reviewers on independent vendor families. Unknown model IDs fail closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VendorFamily {
    Anthropic,
    OpenAi,
    Google,
    DeepSeek,
    ZAi,
}

/// A routed id's vendor path segment (`deepseek/…`, `z-ai/…`), which is
/// authoritative when it names a known vendor.
fn vendor_segment(vendor: &str) -> Option<VendorFamily> {
    match vendor {
        "anthropic" => Some(VendorFamily::Anthropic),
        "openai" => Some(VendorFamily::OpenAi),
        "google" => Some(VendorFamily::Google),
        "deepseek" => Some(VendorFamily::DeepSeek),
        "z-ai" => Some(VendorFamily::ZAi),
        _ => None,
    }
}

/// A bare model id's family by prefix.
fn bare_model_family(model: &str) -> Option<VendorFamily> {
    if model.starts_with("claude-") || model.starts_with("anthropic:") {
        Some(VendorFamily::Anthropic)
    } else if model.starts_with("gpt-")
        || model.starts_with("o1-")
        || model.starts_with("o3-")
        || model.starts_with("o4-")
        || model.starts_with("codex-")
    {
        Some(VendorFamily::OpenAi)
    } else if model.starts_with("gemini-") {
        Some(VendorFamily::Google)
    } else if model.starts_with("deepseek-") {
        Some(VendorFamily::DeepSeek)
    } else if model.starts_with("glm-") {
        Some(VendorFamily::ZAi)
    } else {
        None
    }
}

/// Vendor family of `model` on `provider`: the routed vendor path segment
/// first, then the bare model prefix, then the provider's own family.
/// Unknown ids on routed providers return `None` (fail closed).
#[must_use]
pub fn vendor_family(provider: SessionProvider, model: &str) -> Option<VendorFamily> {
    let model = model.to_ascii_lowercase();
    if let Some(family) = model
        .split_once('/')
        .and_then(|(vendor, _)| vendor_segment(vendor))
    {
        return Some(family);
    }
    let bare = model.rsplit('/').next().unwrap_or(&model);
    if let Some(family) = bare_model_family(bare) {
        return Some(family);
    }
    match provider {
        SessionProvider::Claude => Some(VendorFamily::Anthropic),
        SessionProvider::Codex | SessionProvider::CodexAppServer => Some(VendorFamily::OpenAi),
        SessionProvider::Antigravity => Some(VendorFamily::Google),
        // Bedrock hosts several vendors' models, so the provider alone implies
        // no family; recognized model ids above still classify.
        SessionProvider::Pioneer
        | SessionProvider::OpenRouter
        | SessionProvider::Bedrock
        | SessionProvider::Local
        | SessionProvider::Harness => None,
    }
}

/// Claude model family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFamily {
    Fable,
    Opus,
    Sonnet,
    Haiku,
}

impl ModelFamily {
    /// Returns the lowercase family name substring used in model IDs.
    fn as_str(self) -> &'static str {
        match self {
            ModelFamily::Fable => "fable",
            ModelFamily::Opus => "opus",
            ModelFamily::Sonnet => "sonnet",
            ModelFamily::Haiku => "haiku",
        }
    }
}

/// Parse the model family and (major, minor) version from a Claude model identifier.
///
/// An absent minor component is treated as `.0` for every Claude family. When
/// present, the minor must be 1–2 digits to distinguish real version numbers
/// from 8-digit date suffixes (e.g. `20250929`).
///
/// Returns `None` for non-Claude IDs or if parsing fails.
pub fn parse_model_version(model_id: &str) -> Option<(ModelFamily, u8, u8)> {
    let lower = model_id.to_ascii_lowercase();

    let family = if lower.contains("opus") {
        ModelFamily::Opus
    } else if lower.contains("sonnet") {
        ModelFamily::Sonnet
    } else if lower.contains("haiku") {
        ModelFamily::Haiku
    } else if lower.contains("fable") {
        ModelFamily::Fable
    } else {
        return None;
    };

    let pos = lower.find(family.as_str())?;
    let after_family = &lower[pos + family.as_str().len()..];

    // Skip the separator character(s) between the family name and the major digit.
    let version_part = after_family.trim_start_matches(|c: char| !c.is_ascii_digit());

    // Parse major version number — may run to the end of the string (e.g. the
    // single-component `claude-sonnet-5`).
    let major_end = version_part
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(version_part.len());
    if major_end == 0 {
        return None;
    }
    let major: u8 = version_part[..major_end].parse().ok()?;

    // Skip separator between major and minor.
    let rest = version_part[major_end..].trim_start_matches(|c: char| !c.is_ascii_digit());

    // Parse minor version — must be 1–2 digits to exclude 8-digit date suffixes.
    let minor_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let minor_str = &rest[..minor_end];

    let minor: u8 = if minor_str.is_empty() {
        0
    } else if minor_str.len() > 2 {
        return None;
    } else {
        minor_str.parse().ok()?
    };

    Some((family, major, minor))
}

static CLAUDE_FIVE_LEVEL_EFFORT_LADDER: &[&str] = &["low", "medium", "high", "xhigh", "max"];
static CLAUDE_FOUR_LEVEL_EFFORT_LADDER: &[&str] = &["low", "medium", "high", "max"];
static CLAUDE_THREE_LEVEL_EFFORT_LADDER: &[&str] = &["low", "medium", "high"];
// These Codex ladders and defaults mirror the installed CLI bundled catalog,
// not the OpenAI API's model catalog.
static CODEX_ULTRA_EFFORT_LADDER: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];
static CODEX_LEGACY_EFFORT_LADDER: &[&str] = &["low", "medium", "high", "xhigh"];
static NO_EFFORT_LADDER: &[&str] = &[];

/// Returns the ordered effort levels the model supports for `--effort`.
pub fn effort_ladder(model_id: &str) -> &'static [&'static str] {
    if is_codex_reasoning_model(model_id) {
        return known_codex_effort_ladder(model_id).unwrap_or(CODEX_LEGACY_EFFORT_LADDER);
    }

    match parse_model_version(model_id) {
        Some((ModelFamily::Fable, _, _)) => CLAUDE_FIVE_LEVEL_EFFORT_LADDER,
        Some((ModelFamily::Opus | ModelFamily::Sonnet, major, minor))
            if (major, minor) >= (4, 7) =>
        {
            CLAUDE_FIVE_LEVEL_EFFORT_LADDER
        }
        Some((ModelFamily::Opus | ModelFamily::Sonnet, 4, 6)) => CLAUDE_FOUR_LEVEL_EFFORT_LADDER,
        Some((ModelFamily::Opus, 4, 5)) => CLAUDE_THREE_LEVEL_EFFORT_LADDER,
        _ => NO_EFFORT_LADDER,
    }
}

/// Returns the bundled-Codex capability ladder when RSI knows the exact model.
///
/// `None` deliberately means capability data is unavailable, not that the
/// model supports no effort. This lets the CLI validate explicit custom or
/// future model IDs while still rejecting invalid pairs for known models.
pub fn known_codex_effort_ladder(model_id: &str) -> Option<&'static [&'static str]> {
    match model_id.to_ascii_lowercase().as_str() {
        "gpt-6-astra" | "gpt-6-sol" => Some(CODEX_ULTRA_EFFORT_LADDER),
        "gpt-6-luna" => Some(CODEX_LEGACY_EFFORT_LADDER),
        // Retain legacy capability knowledge for persisted hidden models.
        "gpt-5.5" | "gpt-5.4" | "gpt-5.4-mini" | "gpt-5.2" | "codex-auto-review" => {
            Some(CODEX_LEGACY_EFFORT_LADDER)
        }
        _ => None,
    }
}

/// Returns whether `effort` is supported by the selected model's ordered ladder.
pub fn supports_effort(model_id: &str, effort: &str) -> bool {
    effort_ladder(model_id).contains(&effort)
}

/// Returns the model's effort ladder only when RSI has authoritative
/// capability knowledge for that exact model.
///
/// `None` means capability data is unavailable — **not** that the model
/// supports no effort. Callers that enforce a requested effort must fail open
/// on `None` and let the provider CLI validate explicit custom or future model
/// IDs. A `Some` value is authoritative and safe to enforce.
///
/// This is deliberately stricter than [`effort_ladder`], which substitutes a
/// legacy fallback for unrecognized Codex reasoning models. Enforcing that
/// fallback would reject valid custom model IDs, so enforcement paths must use
/// this function instead.
pub fn known_effort_ladder(model_id: &str) -> Option<&'static [&'static str]> {
    if is_codex_reasoning_model(model_id) {
        return known_codex_effort_ladder(model_id);
    }
    match parse_model_version(model_id) {
        Some((ModelFamily::Fable, _, _)) => Some(CLAUDE_FIVE_LEVEL_EFFORT_LADDER),
        Some((ModelFamily::Opus | ModelFamily::Sonnet, major, minor))
            if (major, minor) >= (4, 7) =>
        {
            Some(CLAUDE_FIVE_LEVEL_EFFORT_LADDER)
        }
        Some((ModelFamily::Opus | ModelFamily::Sonnet, 4, 6)) => {
            Some(CLAUDE_FOUR_LEVEL_EFFORT_LADDER)
        }
        Some((ModelFamily::Opus, 4, 5)) => Some(CLAUDE_THREE_LEVEL_EFFORT_LADDER),
        // Includes Haiku and pre-4.5 Opus: RSI has no authoritative ladder, so
        // enforcement must fail open rather than reject every effort.
        _ => None,
    }
}

/// Clears an effort selection that is unsupported by `model_id`.
pub fn reconcile_effort(model_id: &str, effort: &mut Option<String>) {
    if effort
        .as_deref()
        .is_some_and(|selected| !supports_effort(model_id, selected))
    {
        *effort = None;
    }
}

/// Returns the number of effort levels the model supports for `--effort`.
///
/// Compatibility wrapper for callers that have not yet migrated to
/// [`effort_ladder`].
pub fn effort_level_count(model_id: &str) -> u8 {
    effort_ladder(model_id).len() as u8
}

/// Returns the default effort level string for the model, or `None` if unsupported.
///
/// - Opus ≥ 4.7   → `"xhigh"`
/// - Opus 4.6     → `"max"`
/// - Sonnet ≥ 5.0 → `"xhigh"`
/// - Sonnet ≥ 4.6 → `"high"`
/// - Fable        → `"max"`
/// - GPT-6 Astra/Sol → `"low"`
/// - other GPT-5 Codex/OpenAI reasoning models → `"medium"`
pub fn default_effort_level(model_id: &str) -> Option<&'static str> {
    if is_codex_reasoning_model(model_id) {
        return if matches!(
            model_id.to_ascii_lowercase().as_str(),
            "gpt-6-astra" | "gpt-6-sol"
        ) {
            Some("low")
        } else {
            Some("medium")
        };
    }

    match parse_model_version(model_id) {
        Some((ModelFamily::Fable, _, _)) => Some("max"),
        Some((ModelFamily::Opus, major, minor)) if (major, minor) >= (4, 7) => Some("xhigh"),
        Some((ModelFamily::Opus, 4, 6)) => Some("max"),
        Some((ModelFamily::Sonnet, major, minor)) if (major, minor) >= (5, 0) => Some("xhigh"),
        Some((ModelFamily::Sonnet, major, minor)) if (major, minor) >= (4, 6) => Some("high"),
        _ => None,
    }
}

fn is_codex_reasoning_model(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    lower.starts_with("gpt-6")
        || lower.starts_with("gpt-5")
        || lower.starts_with("codex-auto-review")
}

/// Return a short human-readable display label for a Claude model ID.
///
/// Examples:
/// - `"claude-opus-4-6"`       → `"Opus 4.6 (1M)"`
/// - `"claude-opus-4-7-200k"`  → `"Opus 4.7 (200K)"`
/// - `"claude-sonnet-5"`       → `"Sonnet 5"`
/// - `"claude-haiku-4-7"`      → `"Haiku 4.7"`
///
/// Returns the raw `model_id` if parsing fails (non-Claude models fall through).
pub fn abbreviate_model(model_id: &str) -> String {
    let lower = model_id.to_ascii_lowercase();
    match parse_model_version(model_id) {
        Some((ModelFamily::Fable, major, minor)) => {
            // Fable may version as a single component (Fable 5) or carry a
            // minor (Fable 5.1); show the minor only when present (non-zero).
            // Fable's context window is 1M.
            if minor == 0 {
                format!("Fable {} (1M)", major)
            } else {
                format!("Fable {}.{} (1M)", major, minor)
            }
        }
        Some((ModelFamily::Opus, major, minor)) => {
            let size = if lower.contains("200k") {
                "(200K)"
            } else {
                "(1M)"
            };
            if lower == "claude-opus-5" {
                format!("Opus {} {}", major, size)
            } else {
                format!("Opus {}.{} {}", major, minor, size)
            }
        }
        Some((ModelFamily::Sonnet, major, minor)) => {
            // Sonnet 5+ versions as a single component (Sonnet 5); show the
            // minor only when present (non-zero), same convention as Fable.
            if minor == 0 {
                format!("Sonnet {}", major)
            } else {
                format!("Sonnet {}.{}", major, minor)
            }
        }
        Some((ModelFamily::Haiku, major, minor)) => format!("Haiku {}.{}", major, minor),
        None if lower.starts_with("gpt-oss-") => model_id
            .split('-')
            .map(|part| match part {
                part if part.eq_ignore_ascii_case("gpt") => "GPT".to_string(),
                part if part.eq_ignore_ascii_case("oss") => "OSS".to_string(),
                _ => uppercase_suffix_or_title(part),
            })
            .collect::<Vec<_>>()
            .join(" "),
        None if lower.starts_with("gpt-") => {
            let suffix = &model_id[4..];
            format!("GPT-{}", title_model_suffix(suffix))
        }
        None => model_id.to_string(),
    }
}

fn title_model_suffix(suffix: &str) -> String {
    suffix
        .split('-')
        .map(uppercase_suffix_or_title)
        .collect::<Vec<_>>()
        .join(" ")
}

fn uppercase_suffix_or_title(part: &str) -> String {
    if part.chars().any(|ch| ch.is_ascii_digit()) {
        part.to_ascii_uppercase()
    } else {
        let mut chars = part.chars();
        match chars.next() {
            Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
            None => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_family_uses_model_for_routed_backends_and_fails_closed_on_unknowns() {
        assert_eq!(
            vendor_family(SessionProvider::Claude, "claude-opus-5"),
            Some(VendorFamily::Anthropic)
        );
        assert_eq!(
            vendor_family(SessionProvider::Codex, "gpt-6-sol"),
            Some(VendorFamily::OpenAi)
        );
        assert_eq!(
            vendor_family(SessionProvider::Antigravity, "gemini-3-pro"),
            Some(VendorFamily::Google)
        );
        assert_eq!(
            vendor_family(SessionProvider::OpenRouter, "anthropic/claude-sonnet-5"),
            Some(VendorFamily::Anthropic)
        );
        assert_eq!(
            vendor_family(SessionProvider::Harness, "openai/gpt-6-astra"),
            Some(VendorFamily::OpenAi)
        );
        assert_eq!(
            vendor_family(SessionProvider::OpenRouter, "vendor/unknown"),
            None
        );
    }

    /// Bedrock is multi-vendor: recognized model ids classify by their own
    /// prefix, and an unrecognized id stays unclassified (fail closed).
    #[test]
    fn vendor_family_classifies_bedrock_by_model_and_fails_closed() {
        assert_eq!(
            vendor_family(SessionProvider::Bedrock, "claude-opus-5-5"),
            Some(VendorFamily::Anthropic)
        );
        assert_eq!(
            vendor_family(SessionProvider::Bedrock, "gpt-6-luna"),
            Some(VendorFamily::OpenAi)
        );
        assert_eq!(
            vendor_family(SessionProvider::Bedrock, "amazon.nova-pro-v1:0"),
            None
        );
    }

    /// The operator's routed models classify by their vendor path segment
    /// (plan §5.3), so reviewer-family != author-family is provable for them.
    #[test]
    fn vendor_family_classifies_routed_vendor_segments() {
        assert_eq!(
            vendor_family(SessionProvider::OpenRouter, "deepseek/deepseek-v4.1-flash"),
            Some(VendorFamily::DeepSeek)
        );
        assert_eq!(
            vendor_family(SessionProvider::OpenRouter, "z-ai/glm-5.3-flashx"),
            Some(VendorFamily::ZAi)
        );
        assert_eq!(
            vendor_family(SessionProvider::OpenRouter, "openai/gpt-6-sol"),
            Some(VendorFamily::OpenAi)
        );
        assert_eq!(
            vendor_family(SessionProvider::OpenRouter, "google/gemini-3-pro"),
            Some(VendorFamily::Google)
        );
        assert_eq!(
            vendor_family(SessionProvider::Local, "deepseek-coder-v3"),
            Some(VendorFamily::DeepSeek)
        );
        assert_eq!(
            vendor_family(SessionProvider::Harness, "glm-5.3"),
            Some(VendorFamily::ZAi)
        );
        assert_eq!(
            vendor_family(SessionProvider::OpenRouter, "mistralai/mistral-large-3"),
            None
        );
    }

    #[test]
    fn test_parse_standard() {
        assert_eq!(
            parse_model_version("claude-opus-4-6"),
            Some((ModelFamily::Opus, 4, 6))
        );
        assert_eq!(
            parse_model_version("claude-opus-4-7"),
            Some((ModelFamily::Opus, 4, 7))
        );
        assert_eq!(
            parse_model_version("claude-opus-5-0"),
            Some((ModelFamily::Opus, 5, 0))
        );
        assert_eq!(
            parse_model_version("claude-opus-5-9"),
            Some((ModelFamily::Opus, 5, 9))
        );
        assert_eq!(
            parse_model_version("claude-sonnet-5-9"),
            Some((ModelFamily::Sonnet, 5, 9))
        );
        assert_eq!(
            parse_model_version("claude-haiku-4-5"),
            Some((ModelFamily::Haiku, 4, 5))
        );
    }

    #[test]
    fn test_parse_fable() {
        // The catalogued model carries a minor component.
        assert_eq!(
            parse_model_version("claude-fable-5-1"),
            Some((ModelFamily::Fable, 5, 1))
        );
        // Legacy single-component ids (persisted sessions) still parse,
        // with an absent minor treated as `.0`.
        assert_eq!(
            parse_model_version("claude-fable-5"),
            Some((ModelFamily::Fable, 5, 0))
        );
    }

    #[test]
    fn test_parse_sonnet_single_component() {
        // Sonnet 5+ versions a single component (no minor), same as Fable —
        // treated as `.0`.
        assert_eq!(
            parse_model_version("claude-sonnet-5"),
            Some((ModelFamily::Sonnet, 5, 0))
        );
    }

    #[test]
    fn test_parse_opus_and_haiku_single_component() {
        assert_eq!(
            parse_model_version("claude-opus-5"),
            Some((ModelFamily::Opus, 5, 0))
        );
        assert_eq!(
            parse_model_version("claude-haiku-5"),
            Some((ModelFamily::Haiku, 5, 0))
        );
    }

    #[test]
    fn test_parse_rejects_dated_legacy_opus_id() {
        assert_eq!(parse_model_version("claude-3-opus-20240229"), None);
    }

    #[test]
    fn test_parse_with_size_suffix() {
        assert_eq!(
            parse_model_version("claude-opus-4-7-200k"),
            Some((ModelFamily::Opus, 4, 7))
        );
        assert_eq!(
            parse_model_version("claude-opus-4-6-200k"),
            Some((ModelFamily::Opus, 4, 6))
        );
    }

    #[test]
    fn test_parse_with_date_suffix() {
        // Date suffix (8 digits) must not be confused with minor version.
        assert_eq!(
            parse_model_version("claude-haiku-4-5-20251001"),
            Some((ModelFamily::Haiku, 4, 5))
        );
    }

    #[test]
    fn test_parse_dot_separator() {
        assert_eq!(
            parse_model_version("claude-opus-4.7"),
            Some((ModelFamily::Opus, 4, 7))
        );
        assert_eq!(
            parse_model_version("claude-sonnet-5.9"),
            Some((ModelFamily::Sonnet, 5, 9))
        );
    }

    #[test]
    fn test_effort_ladder() {
        let five_levels = &["low", "medium", "high", "xhigh", "max"];
        let four_claude_levels = &["low", "medium", "high", "max"];
        let three_levels = &["low", "medium", "high"];
        let codex_legacy_levels = &["low", "medium", "high", "xhigh"];
        let codex_ultra_levels = &["low", "medium", "high", "xhigh", "max", "ultra"];

        assert_eq!(effort_ladder("claude-fable-5-1"), five_levels);
        assert_eq!(effort_ladder("claude-opus-4-7"), five_levels);
        assert_eq!(effort_ladder("claude-opus-4-8"), five_levels);
        assert_eq!(effort_ladder("claude-opus-5"), five_levels);
        assert_eq!(effort_ladder("claude-sonnet-5"), five_levels);
        assert_eq!(effort_ladder("claude-opus-4-6"), four_claude_levels);
        assert_eq!(effort_ladder("claude-sonnet-4-6"), four_claude_levels);
        assert_eq!(effort_ladder("claude-opus-4-5"), three_levels);
        assert_eq!(effort_ladder("gpt-6-astra"), codex_ultra_levels);
        assert_eq!(effort_ladder("gpt-6-sol"), codex_ultra_levels);
        assert_eq!(effort_ladder("gpt-6-luna"), codex_legacy_levels);
        assert_eq!(effort_ladder("codex-auto-review"), codex_legacy_levels);
        // This legacy GPT-5 fallback remains available to the UI, but it is
        // absent from the installed bundled catalog and must not be daemon-known.
        assert_eq!(effort_ladder("gpt-5.3-codex"), codex_legacy_levels);
        assert_eq!(known_codex_effort_ladder("gpt-5.3-codex"), None);
        assert_eq!(effort_ladder("claude-sonnet-4-5"), NO_EFFORT_LADDER);
        assert_eq!(effort_ladder("claude-haiku-5"), NO_EFFORT_LADDER);
        assert_eq!(effort_ladder("gpt-4"), NO_EFFORT_LADDER);
    }

    #[test]
    fn effort_support_is_model_aware() {
        assert!(supports_effort("gpt-6-astra", "ultra"));
        assert!(supports_effort("gpt-6-sol", "ultra"));
        assert!(!supports_effort("gpt-6-luna", "ultra"));
        assert!(supports_effort("gpt-6-astra", "max"));
        assert!(!supports_effort("gpt-5.5", "max"));

        let mut effort = Some("ultra".to_string());
        reconcile_effort("gpt-5.5", &mut effort);
        assert_eq!(effort, None);
    }

    #[test]
    fn known_effort_ladder_is_authoritative_or_absent() {
        // Known models expose their real ladder for enforcement.
        assert!(known_effort_ladder("gpt-6-astra").is_some_and(|l| l.contains(&"ultra")));
        assert!(known_effort_ladder("claude-fable-5").is_some_and(|l| !l.contains(&"ultra")));
        assert!(known_effort_ladder("claude-opus-4-6").is_some_and(|l| !l.contains(&"xhigh")));

        // Unknown/custom models must fail open so the CLI validates them.
        assert_eq!(known_effort_ladder("gpt-5.3-codex"), None);
        assert_eq!(known_effort_ladder("some-vendor/custom-model"), None);
        assert_eq!(known_effort_ladder("claude-haiku-5"), None);

        // Every authoritative ladder agrees with effort_ladder, so enforcement
        // can never contradict the picker.
        for model in ["gpt-6-astra", "claude-fable-5", "claude-opus-4-5"] {
            assert_eq!(known_effort_ladder(model), Some(effort_ladder(model)));
        }
    }

    #[test]
    fn test_effort_level_count() {
        assert_eq!(effort_level_count("claude-fable-5-1"), 5);
        assert_eq!(effort_level_count("claude-opus-4-6"), 4);
        assert_eq!(effort_level_count("claude-sonnet-4-6"), 4);
        assert_eq!(effort_level_count("claude-opus-4-7"), 5);
        assert_eq!(effort_level_count("claude-opus-5"), 5);
        assert_eq!(effort_level_count("claude-opus-5-0"), 5);
        assert_eq!(effort_level_count("claude-opus-5-9"), 5);
        assert_eq!(effort_level_count("claude-sonnet-5"), 5);
        assert_eq!(effort_level_count("claude-sonnet-5-9"), 5);
        assert_eq!(effort_level_count("claude-opus-4-5"), 3);
        assert_eq!(effort_level_count("claude-sonnet-4-5"), 0);
        // Haiku: no effort
        assert_eq!(effort_level_count("claude-haiku-4-7"), 0);
        assert_eq!(effort_level_count("claude-haiku-5-9"), 0);
        // Codex/OpenAI reasoning models: current variants are model-specific.
        assert_eq!(effort_level_count("gpt-6-astra"), 6);
        assert_eq!(effort_level_count("gpt-6-sol"), 6);
        assert_eq!(effort_level_count("gpt-6-luna"), 4);
        assert_eq!(effort_level_count("gpt-5.5"), 4);
        assert_eq!(effort_level_count("gpt-5.4-mini"), 4);
        assert_eq!(effort_level_count("gpt-5.3-codex"), 4);
        assert_eq!(effort_level_count("codex-auto-review"), 4);
        // Unsupported non-Claude: no effort
        assert_eq!(effort_level_count("gpt-4"), 0);
    }

    #[test]
    fn test_default_effort_level() {
        assert_eq!(default_effort_level("claude-fable-5-1"), Some("max"));
        assert_eq!(default_effort_level("claude-opus-4-6"), Some("max"));
        assert_eq!(default_effort_level("claude-opus-4-7"), Some("xhigh"));
        assert_eq!(default_effort_level("claude-opus-4-8"), Some("xhigh"));
        assert_eq!(default_effort_level("claude-opus-5"), Some("xhigh"));
        assert_eq!(default_effort_level("claude-opus-5-9"), Some("xhigh"));
        assert_eq!(default_effort_level("claude-sonnet-4-6"), Some("high"));
        assert_eq!(default_effort_level("claude-sonnet-5"), Some("xhigh"));
        assert_eq!(default_effort_level("claude-sonnet-5-9"), Some("xhigh"));
        assert_eq!(default_effort_level("gpt-6-astra"), Some("low"));
        assert_eq!(default_effort_level("gpt-6-sol"), Some("low"));
        assert_eq!(default_effort_level("gpt-6-luna"), Some("medium"));
        assert_eq!(default_effort_level("gpt-5.5"), Some("medium"));
        assert_eq!(default_effort_level("gpt-5.3-codex"), Some("medium"));
        assert_eq!(default_effort_level("claude-haiku-4-7"), None);
        assert_eq!(default_effort_level("claude-opus-4-5"), None);
        assert_eq!(default_effort_level("gpt-4"), None);
    }

    #[test]
    fn test_abbreviate_model() {
        assert_eq!(abbreviate_model("claude-fable-5-1"), "Fable 5.1 (1M)");
        // Legacy single-component id still renders without a minor.
        assert_eq!(abbreviate_model("claude-fable-5"), "Fable 5 (1M)");
        assert_eq!(abbreviate_model("claude-opus-4-6"), "Opus 4.6 (1M)");
        assert_eq!(abbreviate_model("claude-opus-4-7"), "Opus 4.7 (1M)");
        assert_eq!(abbreviate_model("claude-opus-5"), "Opus 5 (1M)");
        assert_eq!(abbreviate_model("claude-opus-5-9"), "Opus 5.9 (1M)");
        assert_eq!(abbreviate_model("claude-opus-4-7-200k"), "Opus 4.7 (200K)");
        assert_eq!(abbreviate_model("claude-sonnet-5"), "Sonnet 5");
        assert_eq!(abbreviate_model("claude-sonnet-5-9"), "Sonnet 5.9");
        assert_eq!(abbreviate_model("claude-haiku-4-5"), "Haiku 4.5");
        assert_eq!(abbreviate_model("claude-haiku-5-9"), "Haiku 5.9");
        assert_eq!(abbreviate_model("gpt-6-astra"), "GPT-6 Astra");
        assert_eq!(abbreviate_model("gpt-6-sol"), "GPT-6 Sol");
        assert_eq!(abbreviate_model("gpt-6-luna"), "GPT-6 Luna");
        assert_eq!(abbreviate_model("gpt-5.5"), "GPT-5.5");
        assert_eq!(abbreviate_model("gpt-5.4-mini"), "GPT-5.4 Mini");
        assert_eq!(abbreviate_model("gpt-5.3-codex"), "GPT-5.3 Codex");
        assert_eq!(
            abbreviate_model("gpt-oss-120b-medium"),
            "GPT OSS 120B Medium"
        );
    }
}
