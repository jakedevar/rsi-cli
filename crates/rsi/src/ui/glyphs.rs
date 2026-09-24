//! Shared symbol and color vocabulary for the session browser.
//!
//! One vocabulary spans the navigator list, the selected-session inspector and
//! the live-activity pane: a glyph means the same thing everywhere it appears,
//! and every glyph keeps a distinct *shape* so it still reads without color.
//!
//! Rules of thumb:
//! - closed sets of states (lifecycle, attention, provider) are symbols;
//! - the open-ended set of agent roles is a two-letter colored code, because
//!   single letters collide (`Reviewer` / `Refactorer` / `Researcher`);
//! - compaction never destroys identity: a shortened list value is always a
//!   verbatim suffix of the canonical value, which the inspector shows in full.
//!
//! Every glyph here is a single-cell, text-presentation codepoint. Emoji-default
//! codepoints (`⏱`, `⚠`, `✉`, …) are deliberately excluded because terminals
//! disagree on their width and they break column alignment.

use ratatui::style::Color;
use rsi_common::types::{SessionProvider, SessionStatus};

use super::theme;

// ---------------------------------------------------------------------------
// Attention and flags
// ---------------------------------------------------------------------------

/// Needs the operator: pending question, approval, or failure.
pub const NEEDS_YOU: &str = "!";
/// Unread output since the operator last looked.
pub const UNREAD: &str = "•";
/// Automatic retry pending. Distinct from the rotation marker `↻`.
pub const RETRY: &str = "↺";
/// No recent output; the session may be stalled.
pub const STALLED: &str = "⧗";
/// Pinned session.
pub const PIN: &str = "◆";
/// Context rotation depth marker (matches the existing title suffix).
pub const ROTATION: &str = "↻";
/// Context size is unknown for this session.
pub const CONTEXT_UNKNOWN: &str = "◌";

// ---------------------------------------------------------------------------
// Column and field markers
// ---------------------------------------------------------------------------

pub const CONTEXT: &str = "◔";
pub const TURNS: &str = "⇄";
pub const EFFORT: &str = "▮";
pub const COST: &str = "$";
pub const WORK_TIME: &str = "◷";
pub const CREATED: &str = "+";
pub const UPDATED: &str = "Δ";
pub const SESSION_ID: &str = "#";
pub const WORKING_DIR: &str = "⌂";
pub const SANDBOX: &str = "⊡";
pub const BRANCH: &str = "⎇";
pub const LOCATION: &str = "›";
pub const QUOTE_RAIL: &str = "▎";
pub const QUEUE: &str = "⚑";
pub const CHANGES: &str = "Δ";
pub const NEXT: &str = "→";
pub const MANAGERS: &str = "★";
pub const BELOW: &str = "↓";
pub const FAILED: &str = "×";
pub const DONE: &str = "✓";
pub const TESTING: &str = "☐";
pub const ROTATION_OFF: &str = "⊘";
pub const ARCHIVING: &str = "▽";
pub const LEAD: &str = "◈";
pub const ARTIFACT: &str = "▤";
pub const HANDOFF: &str = "⇥";
pub const FOLLOW_UP: &str = "⚑";
pub const REQUIRED: &str = "?";
pub const PREVIOUS: &str = "↳";
pub const DESCENDANTS: &str = "Σ";

/// Height glyphs for a one-cell effort gauge, lowest to highest.
const EFFORT_LEVELS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// Glyph and color for one inspector signal chip.
#[must_use]
pub fn signal_glyph(signal: crate::types::InspectorSignal) -> (&'static str, Color) {
    use crate::types::InspectorSignal;
    match signal {
        InspectorSignal::NeedsInput => (NEEDS_YOU, theme::status_waiting()),
        InspectorSignal::Failed => (FAILED, theme::status_failed()),
        InspectorSignal::Retry { .. } => (RETRY, theme::yellow()),
        InspectorSignal::Stalled => (STALLED, theme::status_stalled()),
        InspectorSignal::Unread => (UNREAD, theme::accent()),
        InspectorSignal::Pinned => (PIN, theme::pin()),
        InspectorSignal::TestingNeeded => (TESTING, theme::yellow()),
        InspectorSignal::RotationDisabled => (ROTATION_OFF, theme::subtext0()),
        InspectorSignal::PendingArchive => (ARCHIVING, theme::subtext0()),
        InspectorSignal::EpicLead => (LEAD, theme::mauve()),
    }
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Lowercase lifecycle word used beside the lifecycle glyph in headers.
#[must_use]
pub const fn status_word(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Starting => "starting",
        SessionStatus::Running => "running",
        SessionStatus::WaitingApproval => "waiting",
        SessionStatus::Completed => "done",
        SessionStatus::Failed => "failed",
        SessionStatus::Interrupted => "stopped",
        SessionStatus::Archived => "archived",
        SessionStatus::Deleted => "deleted",
        _ => "inactive",
    }
}

// ---------------------------------------------------------------------------
// Provider and model
// ---------------------------------------------------------------------------

/// One-cell provider mark. The provider name moves into the glyph so the
/// model column can drop the vendor namespace without losing the vendor.
#[must_use]
pub const fn provider_glyph(provider: SessionProvider) -> &'static str {
    match provider {
        SessionProvider::Claude => "✻",
        SessionProvider::Codex => "◎",
        SessionProvider::CodexAppServer => "◉",
        SessionProvider::Pioneer => "◇",
        SessionProvider::OpenRouter => "⋈",
        SessionProvider::Bedrock => "☁",
        SessionProvider::Local => "⌂",
        SessionProvider::Antigravity => "△",
        SessionProvider::Harness => "⌘",
        _ => "?",
    }
}

#[must_use]
pub fn provider_color(provider: SessionProvider) -> Color {
    match provider {
        SessionProvider::Claude => theme::peach(),
        SessionProvider::Codex => theme::green(),
        SessionProvider::CodexAppServer => theme::teal(),
        SessionProvider::Pioneer => theme::lavender(),
        SessionProvider::OpenRouter => theme::sapphire(),
        SessionProvider::Bedrock => theme::peach(),
        SessionProvider::Local => theme::yellow(),
        SessionProvider::Antigravity => theme::sky(),
        SessionProvider::Harness => theme::mauve(),
        _ => theme::subtext0(),
    }
}

/// Compact list label for a canonical model ID.
///
/// The result is always a verbatim suffix of `model`: only the vendor
/// namespace (`org/`) and the vendor family prefix (`claude-`, `gpt-`) are
/// dropped, because the provider glyph already carries the vendor. Version
/// segments are never rewritten, so a family shorthand can never stand in for
/// the actual model version. The inspector keeps the complete canonical ID.
#[must_use]
pub fn list_model_label(model: &str) -> &str {
    let tail = model.rsplit('/').next().unwrap_or(model);
    for prefix in ["claude-", "gpt-"] {
        if let Some(rest) = tail.strip_prefix(prefix)
            && !rest.is_empty()
        {
            return rest;
        }
    }
    tail
}

/// Cross-provider effort words, lowest to highest, used when a model's own
/// ladder is unknown. `max` (Claude) and `ultra` (Codex) are both the top.
const FALLBACK_EFFORT_SCALE: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

fn fallback_effort_position(effort: &str) -> Option<(usize, usize)> {
    let effort = if effort == "ultra" { "max" } else { effort };
    FALLBACK_EFFORT_SCALE
        .iter()
        .position(|candidate| *candidate == effort)
        .map(|index| (index + 1, FALLBACK_EFFORT_SCALE.len()))
}

/// One-cell effort gauge scaled to the model's own effort ladder, or to the
/// cross-provider effort scale when the model's ladder is unknown.
///
/// Returns `None` when no effort is recorded — a default is never invented.
/// An effort word neither scale knows renders as a dim `?`; the inspector
/// still shows the raw value.
#[must_use]
pub fn effort_glyph(effort: Option<&str>, model: Option<&str>) -> Option<(&'static str, Color)> {
    let effort = effort?;
    let ladder = model.and_then(|model| {
        rsi_common::model_utils::known_codex_effort_ladder(model).or_else(|| {
            rsi_common::model_utils::parse_model_version(model)
                .map(|_| rsi_common::model_utils::effort_ladder(model))
        })
    });
    let position = ladder
        .and_then(|ladder| {
            ladder
                .iter()
                .position(|candidate| *candidate == effort)
                .map(|index| (index + 1, ladder.len()))
        })
        .or_else(|| fallback_effort_position(effort));
    let Some((filled, total)) = position else {
        return Some(("?", theme::dim_metadata()));
    };
    let steps = EFFORT_LEVELS.len();
    let level = (filled * steps).div_ceil(total).clamp(1, steps);
    Some((EFFORT_LEVELS[level - 1], theme::yellow()))
}

// ---------------------------------------------------------------------------
// Roles
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoleFamily {
    Lead,
    Plan,
    Build,
    Debug,
    Check,
    Other,
}

const KNOWN_ROLES: &[(&str, &str, RoleFamily)] = &[
    ("Manager", "Mg", RoleFamily::Lead),
    ("Orchestrator", "Or", RoleFamily::Lead),
    ("Demiurge", "Dm", RoleFamily::Lead),
    ("Coordinator", "Co", RoleFamily::Lead),
    ("Lead", "Ld", RoleFamily::Lead),
    ("Planner", "Pl", RoleFamily::Plan),
    ("Researcher", "Rs", RoleFamily::Plan),
    ("Investigator", "Iv", RoleFamily::Plan),
    ("Architect", "Ar", RoleFamily::Plan),
    ("Analyst", "An", RoleFamily::Plan),
    ("Designer", "Ds", RoleFamily::Plan),
    ("Implementer", "Im", RoleFamily::Build),
    ("Refactorer", "Rf", RoleFamily::Build),
    ("Builder", "Bd", RoleFamily::Build),
    ("Engineer", "En", RoleFamily::Build),
    ("Developer", "Dv", RoleFamily::Build),
    ("Fixer", "Fx", RoleFamily::Build),
    ("Writer", "Wr", RoleFamily::Build),
    ("Debugger", "Db", RoleFamily::Debug),
    ("Reviewer", "Rv", RoleFamily::Check),
    ("Verifier", "Vf", RoleFamily::Check),
    ("Tester", "Ts", RoleFamily::Check),
    ("Validator", "Vl", RoleFamily::Check),
    ("Auditor", "Au", RoleFamily::Check),
];

/// Whether `role` is one of the fixed, mnemonic-coded roles.
#[must_use]
pub fn is_known_role(role: &str) -> bool {
    known_role(role).is_some()
}

fn known_role(role: &str) -> Option<(&'static str, RoleFamily)> {
    KNOWN_ROLES
        .iter()
        .find(|(name, ..)| name.eq_ignore_ascii_case(role))
        .map(|(_, code, family)| (*code, *family))
}

/// Two-letter role code: a fixed mnemonic for known roles, otherwise the
/// role word's first two letters (`Agent` → `Ag`).
#[must_use]
pub fn role_code(role: &str) -> String {
    if let Some((code, _)) = known_role(role) {
        return code.to_string();
    }
    let mut chars = role.chars();
    let first = chars.next().map(|ch| ch.to_ascii_uppercase());
    let second = chars.next().map(|ch| ch.to_ascii_lowercase());
    first.into_iter().chain(second).collect()
}

/// Role color by family: lead, plan, build, debug, check, other.
#[must_use]
pub fn role_color(role: &str) -> Color {
    match known_role(role).map_or(RoleFamily::Other, |(_, family)| family) {
        RoleFamily::Lead => theme::mauve(),
        RoleFamily::Plan => theme::blue(),
        RoleFamily::Build => theme::teal(),
        RoleFamily::Debug => theme::peach(),
        RoleFamily::Check => theme::yellow(),
        RoleFamily::Other => theme::overlay1(),
    }
}

// ---------------------------------------------------------------------------
// Gauges and path compaction
// ---------------------------------------------------------------------------

/// Ten-cell `▰▱` gauge for a 0–100 percentage.
#[must_use]
pub fn percent_gauge(percent: f64, cells: u8) -> String {
    let filled = ((percent.clamp(0.0, 100.0) / 100.0) * f64::from(cells)).round();
    // `filled` is clamped to `0..=cells`, so the narrowing cast is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let filled = (filled as u8).min(cells);
    format!(
        "{}{}",
        "▰".repeat(usize::from(filled)),
        "▱".repeat(usize::from(cells - filled))
    )
}

/// Replace a leading home directory with `~`.
#[must_use]
pub fn home_relative(path: &str) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return path.to_string();
    };
    let home = home.to_string_lossy();
    if home.is_empty() || home == "/" {
        return path.to_string();
    }
    match path.strip_prefix(home.as_ref()) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// Sandbox worktrees are named after the session UUID and their branch is
/// `rsi/<same-uuid>`. When that holds, the two lines collapse into one shared
/// identifier; otherwise `None` and both render in full.
#[must_use]
pub fn shared_sandbox_identifier<'a>(root: &'a str, branch: &str) -> Option<&'a str> {
    let leaf = root.trim_end_matches('/').rsplit('/').next()?;
    (!leaf.is_empty() && branch.strip_prefix("rsi/") == Some(leaf)).then_some(leaf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn every_vocabulary_glyph_is_one_cell_wide() {
        let providers = [
            SessionProvider::Claude,
            SessionProvider::Codex,
            SessionProvider::CodexAppServer,
            SessionProvider::Pioneer,
            SessionProvider::OpenRouter,
            SessionProvider::Local,
            SessionProvider::Antigravity,
            SessionProvider::Harness,
        ];
        let mut glyphs = vec![
            NEEDS_YOU,
            UNREAD,
            RETRY,
            STALLED,
            PIN,
            ROTATION,
            CONTEXT_UNKNOWN,
            CONTEXT,
            TURNS,
            EFFORT,
            WORK_TIME,
            UPDATED,
            WORKING_DIR,
            SANDBOX,
            BRANCH,
            QUOTE_RAIL,
            QUEUE,
            NEXT,
            MANAGERS,
            BELOW,
            FAILED,
            DONE,
            TESTING,
            ROTATION_OFF,
            ARCHIVING,
            LEAD,
            ARTIFACT,
            HANDOFF,
            FOLLOW_UP,
            REQUIRED,
            PREVIOUS,
            DESCENDANTS,
        ];
        glyphs.extend(providers.into_iter().map(provider_glyph));
        glyphs.extend(EFFORT_LEVELS);
        for glyph in glyphs {
            assert_eq!(glyph.width(), 1, "{glyph:?} must occupy one cell");
        }
    }

    #[test]
    fn provider_glyphs_are_distinct_per_provider() {
        let providers = [
            SessionProvider::Claude,
            SessionProvider::Codex,
            SessionProvider::CodexAppServer,
            SessionProvider::Pioneer,
            SessionProvider::OpenRouter,
            SessionProvider::Local,
            SessionProvider::Antigravity,
            SessionProvider::Harness,
        ];
        let glyphs = providers
            .into_iter()
            .map(provider_glyph)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(glyphs.len(), providers.len());
    }

    #[test]
    fn list_model_label_is_a_verbatim_suffix_that_keeps_the_version() {
        for (canonical, label) in [
            ("claude-opus-5-5", "opus-5-5"),
            ("claude-fable-5-1", "fable-5-1"),
            ("gpt-6-luna", "6-luna"),
            ("gpt-5.6-sol", "5.6-sol"),
            ("deepseek/deepseek-v4.1-terminus", "deepseek-v4.1-terminus"),
            ("z-ai/glm-5.3-flash", "glm-5.3-flash"),
            ("openai/gpt-5", "5"),
            ("o3", "o3"),
            ("claude-", "claude-"),
        ] {
            assert_eq!(list_model_label(canonical), label, "{canonical}");
            assert!(canonical.ends_with(label), "{canonical} keeps a suffix");
        }
    }

    #[test]
    fn effort_glyph_scales_to_the_ladder_and_never_invents_a_default() {
        let ladder = rsi_common::model_utils::effort_ladder("claude-opus-5-5");
        let top = ladder.last().copied().expect("opus has an effort ladder");
        assert_eq!(
            effort_glyph(Some(top), Some("claude-opus-5-5")).unwrap().0,
            "█"
        );
        let bottom = ladder.first().copied().unwrap();
        let low = effort_glyph(Some(bottom), Some("claude-opus-5-5"))
            .unwrap()
            .0;
        assert_ne!(low, "█", "lowest effort renders shorter than the top");
        assert_eq!(effort_glyph(None, Some("claude-opus-5-5")), None);
        assert_eq!(
            effort_glyph(Some("future"), Some("gpt-6-astra")).unwrap().0,
            "?"
        );
        // A model without a known ladder still grades standard effort words.
        let unknown_high = effort_glyph(Some("high"), Some("vendor/unknown-model"))
            .unwrap()
            .0;
        let unknown_low = effort_glyph(Some("low"), Some("vendor/unknown-model"))
            .unwrap()
            .0;
        assert_ne!(unknown_high, "?");
        assert!(
            EFFORT_LEVELS
                .iter()
                .position(|level| *level == unknown_high)
                > EFFORT_LEVELS.iter().position(|level| *level == unknown_low),
            "high renders taller than low"
        );
        assert_eq!(effort_glyph(Some("max"), None).unwrap().0, "█");
    }

    #[test]
    fn role_codes_disambiguate_colliding_initials() {
        let codes = ["Reviewer", "Refactorer", "Researcher"]
            .map(role_code)
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(codes.len(), 3);
        assert_eq!(role_code("Manager"), "Mg");
        assert_eq!(role_code("Agent"), "Ag");
    }

    #[test]
    fn known_role_codes_are_unique() {
        let codes = KNOWN_ROLES
            .iter()
            .map(|(_, code, _)| *code)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(codes.len(), KNOWN_ROLES.len());
    }

    #[test]
    fn sandbox_and_branch_collapse_only_when_they_share_the_session_uuid() {
        let uuid = "c3ddb5b9-653b-4c17-8cdf-e00ede098be2";
        assert_eq!(
            shared_sandbox_identifier(
                &format!("/home/u/.rsi/sandboxes/{uuid}"),
                &format!("rsi/{uuid}")
            ),
            Some(uuid)
        );
        assert_eq!(
            shared_sandbox_identifier(&format!("/home/u/.rsi/sandboxes/{uuid}"), "feature/x"),
            None
        );
    }

    #[test]
    fn percent_gauge_fills_proportionally() {
        assert_eq!(percent_gauge(42.0, 10), "▰▰▰▰▱▱▱▱▱▱");
        assert_eq!(percent_gauge(100.0, 4), "▰▰▰▰");
        assert_eq!(percent_gauge(0.0, 4), "▱▱▱▱");
    }
}
