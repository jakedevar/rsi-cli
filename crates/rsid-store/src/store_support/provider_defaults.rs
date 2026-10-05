//! Per-provider default models, effort-level maps and error-class names the
//! store reads (moved down from `codex`, `claude`, `openrouter` and `pioneer`,
//! which re-export them so callers keep their paths).

pub const CODEX_TOOL_HISTORY_ERROR_CLASS: &str = "codex_resume_tool_history_invalid";
pub const CODEX_USAGE_LIMIT_STOP_REASON: &str = "provider_error:codex_usage_limit";

pub fn codex_reasoning_effort(effort: Option<&str>) -> Option<&'static str> {
    match effort {
        Some("low") => Some("low"),
        Some("medium") => Some("medium"),
        Some("high") => Some("high"),
        Some("xhigh") => Some("xhigh"),
        Some("max") => Some("max"),
        Some("ultra") => Some("ultra"),
        _ => None,
    }
}

/// Map an effort string onto the value the Claude CLI accepts, or `None` when
/// the CLI would reject it.
///
/// The accepted set is `low`, `medium`, `high`, `xhigh`, `max` (observed from
/// `claude --help`, CLI 2.1.259). Deliberately exact-match: the CLI is
/// case-sensitive, so a near-miss is a miss.
pub fn claude_effort_level(effort: Option<&str>) -> Option<&'static str> {
    match effort {
        Some("low") => Some("low"),
        Some("medium") => Some("medium"),
        Some("high") => Some("high"),
        Some("xhigh") => Some("xhigh"),
        Some("max") => Some("max"),
        _ => None,
    }
}

pub const OPENROUTER_DEFAULT_MODEL: &str = "openai/gpt-5.2";

pub const PIONEER_DEFAULT_MODEL: &str = "claude-sonnet-5";

pub const PIONEER_RETIRED_AUTO_MODEL: &str = "pioneer/auto";

#[must_use]
pub fn pioneer_launch_model(model: Option<&str>) -> &str {
    match model {
        None | Some(PIONEER_RETIRED_AUTO_MODEL) => PIONEER_DEFAULT_MODEL,
        Some(model) => model,
    }
}

/// Whether `model` is an Anthropic adaptive-thinking model (moved down from
/// `session::harness::providers::anthropic`, #1021 S3a).
pub fn uses_adaptive_thinking(model: &str) -> bool {
    // Bedrock IDs (`us.anthropic.claude-opus-5-5-v1:0`) name the same models.
    let model = crate::bedrock::anthropic_model_name(model).unwrap_or(model);
    // Entries are matched as prefixes, so `claude-fable-5` covers the
    // offered `claude-fable-5-1` and any later Fable 5.x alongside the
    // retired bare id — both reject `temperature`/`budget_tokens`.
    [
        "claude-fable-5",
        "claude-opus-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-sonnet-5",
    ]
    .iter()
    .any(|prefix| {
        model == *prefix
            || model
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('-'))
    })
}
