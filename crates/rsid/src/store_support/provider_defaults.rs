//! Per-provider default models, effort-level maps and error-class names the
//! store reads (moved down from `codex`, `claude`, `openrouter` and `pioneer`,
//! which re-export them so callers keep their paths).

pub(crate) const CODEX_TOOL_HISTORY_ERROR_CLASS: &str = "codex_resume_tool_history_invalid";

pub(crate) fn codex_reasoning_effort(effort: Option<&str>) -> Option<&'static str> {
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
pub(crate) fn claude_effort_level(effort: Option<&str>) -> Option<&'static str> {
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
