//! #3199 (program #3135, milestone M6): the Harness compaction policy.
//!
//! The constants that shape Tier 1 compaction (#3166, #3167) as one per-launch
//! value: when it fires (a share of the model window, a message count), how much
//! history it keeps verbatim and how the summary is asked for. The default is the
//! behaviour compaction had before the policy existed, byte for byte, so a launch
//! that sets nothing is unchanged. A bench run overlay (`[compaction]`,
//! `bench_overlay`) sets it per launch; nothing here writes a daemon setting.

use serde::{Deserialize, Serialize};

/// Messages in the history at which Tier 1 compaction fires regardless of tokens.
/// It sits below the 200-message `hard_trim` cap so the summarizing path runs
/// first (#1058, #3167).
pub const DEFAULT_MESSAGE_TRIGGER: usize = 180;
/// Conversation messages kept verbatim after the summary.
pub const DEFAULT_KEEP_RECENT: usize = 20;
/// Share of the model's window, in percent, at which Tier 1 compaction fires.
pub const DEFAULT_WINDOW_PERCENT: u32 = 75;

/// `message_trigger` range.
pub const MESSAGE_TRIGGER_MIN: usize = 40;
pub const MESSAGE_TRIGGER_MAX: usize = 1000;
/// `keep_recent` range. It must stay below the effective `message_trigger`.
pub const KEEP_RECENT_MIN: usize = 4;
pub const KEEP_RECENT_MAX: usize = 100;
/// `window_percent` range.
pub const WINDOW_PERCENT_MIN: u32 = 25;
pub const WINDOW_PERCENT_MAX: u32 = 95;
/// `summary_tokens` range (the output budget of the summary call).
pub const SUMMARY_TOKENS_MIN: u32 = 500;
pub const SUMMARY_TOKENS_MAX: u32 = 16_000;
/// Longest custom summary instruction, in bytes.
pub const SUMMARY_INSTRUCTION_MAX_BYTES: usize = 4096;
/// The summary length written into a custom instruction in place of this token.
pub const SUMMARY_INSTRUCTION_MAX_TOKENS_PLACEHOLDER: &str = "{max_tokens}";

/// A rejected field and why. `field` is the policy field name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionPolicyError {
    pub field: &'static str,
    pub reason: String,
}

impl std::fmt::Display for CompactionPolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "compaction.{}: {}", self.field, self.reason)
    }
}

impl std::error::Error for CompactionPolicyError {}

/// What one Harness loop compacts with. `Default` is today's behaviour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionPolicy {
    /// History length (system prompt included) that triggers compaction.
    pub message_trigger: usize,
    /// Conversation messages kept verbatim after the summary.
    pub keep_recent: usize,
    /// Percent of the model window at which compaction fires.
    pub window_percent: u32,
    /// Output budget of the summary call; `None` derives it from the threshold.
    pub summary_tokens: Option<u32>,
    /// Replaces the built-in summary instruction; `None` keeps it.
    pub summary_instruction: Option<String>,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            message_trigger: DEFAULT_MESSAGE_TRIGGER,
            keep_recent: DEFAULT_KEEP_RECENT,
            window_percent: DEFAULT_WINDOW_PERCENT,
            summary_tokens: None,
            summary_instruction: None,
        }
    }
}

impl CompactionPolicy {
    /// Live-context size at which Tier 1 compaction fires: `window_percent` of the
    /// model's window, or the absolute budget when that is lower.
    #[must_use]
    pub fn threshold_tokens(&self, token_limit: u64, budget_tokens: Option<u64>) -> u64 {
        let window = (token_limit as f32 * (self.window_percent as f32 / 100.0)) as u64;
        budget_tokens.map_or(window, |budget| window.min(budget))
    }

    /// Every field in its range and `keep_recent` below `message_trigger`.
    ///
    /// # Errors
    /// The first field outside its range.
    pub fn validate(&self) -> Result<(), CompactionPolicyError> {
        let range = |field: &'static str, ok: bool, detail: String| {
            if ok {
                Ok(())
            } else {
                Err(CompactionPolicyError {
                    field,
                    reason: detail,
                })
            }
        };
        range(
            "message_trigger",
            (MESSAGE_TRIGGER_MIN..=MESSAGE_TRIGGER_MAX).contains(&self.message_trigger),
            format!(
                "{} is outside {MESSAGE_TRIGGER_MIN}..={MESSAGE_TRIGGER_MAX}",
                self.message_trigger
            ),
        )?;
        range(
            "keep_recent",
            (KEEP_RECENT_MIN..=KEEP_RECENT_MAX).contains(&self.keep_recent),
            format!(
                "{} is outside {KEEP_RECENT_MIN}..={KEEP_RECENT_MAX}",
                self.keep_recent
            ),
        )?;
        range(
            "keep_recent",
            self.keep_recent < self.message_trigger,
            format!(
                "{} must be below message_trigger {}",
                self.keep_recent, self.message_trigger
            ),
        )?;
        range(
            "window_percent",
            (WINDOW_PERCENT_MIN..=WINDOW_PERCENT_MAX).contains(&self.window_percent),
            format!(
                "{} is outside {WINDOW_PERCENT_MIN}..={WINDOW_PERCENT_MAX}",
                self.window_percent
            ),
        )?;
        if let Some(tokens) = self.summary_tokens {
            range(
                "summary_tokens",
                (SUMMARY_TOKENS_MIN..=SUMMARY_TOKENS_MAX).contains(&tokens),
                format!("{tokens} is outside {SUMMARY_TOKENS_MIN}..={SUMMARY_TOKENS_MAX}"),
            )?;
        }
        if let Some(text) = &self.summary_instruction {
            range(
                "summary_instruction",
                !text.trim().is_empty() && text.len() <= SUMMARY_INSTRUCTION_MAX_BYTES,
                format!("must be 1..={SUMMARY_INSTRUCTION_MAX_BYTES} bytes and not blank"),
            )?;
        }
        Ok(())
    }
}

/// The fields a bench overlay sets; every field optional, an unset one keeps the
/// default. Validated values only (the overlay resolves them).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionOverlay {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_trigger: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_percent: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_instruction: Option<String>,
}

impl CompactionOverlay {
    /// Whether the overlay sets nothing, so it is no difference from no overlay.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }

    /// The default policy with this overlay's values on top.
    #[must_use]
    pub fn policy(&self) -> CompactionPolicy {
        let default = CompactionPolicy::default();
        CompactionPolicy {
            message_trigger: self.message_trigger.unwrap_or(default.message_trigger),
            keep_recent: self.keep_recent.unwrap_or(default.keep_recent),
            window_percent: self.window_percent.unwrap_or(default.window_percent),
            summary_tokens: self.summary_tokens.or(default.summary_tokens),
            summary_instruction: self
                .summary_instruction
                .clone()
                .or(default.summary_instruction),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_policy_is_valid_and_keeps_the_pre_policy_values() {
        let policy = CompactionPolicy::default();
        assert_eq!(policy.validate(), Ok(()));
        assert_eq!(policy.message_trigger, 180);
        assert_eq!(policy.keep_recent, 20);
        assert_eq!(policy.window_percent, 75);
        assert_eq!(policy.summary_tokens, None);
        assert_eq!(policy.summary_instruction, None);
        // 75% of the window, or the budget when lower: the pre-policy rule.
        assert_eq!(policy.threshold_tokens(200_000, None), 150_000);
        assert_eq!(policy.threshold_tokens(200_000, Some(96_000)), 96_000);
        assert_eq!(policy.threshold_tokens(200_000, Some(400_000)), 150_000);
    }

    #[test]
    fn an_unset_overlay_is_the_default_policy() {
        let overlay = CompactionOverlay::default();
        assert!(overlay.is_unset());
        assert_eq!(overlay.policy(), CompactionPolicy::default());
    }

    #[test]
    fn a_set_overlay_layers_on_the_default() {
        let overlay = CompactionOverlay {
            keep_recent: Some(8),
            window_percent: Some(50),
            summary_instruction: Some("Be brief.".into()),
            ..CompactionOverlay::default()
        };
        assert!(!overlay.is_unset());
        let policy = overlay.policy();
        assert_eq!(policy.keep_recent, 8);
        assert_eq!(policy.window_percent, 50);
        assert_eq!(policy.message_trigger, DEFAULT_MESSAGE_TRIGGER);
        assert_eq!(policy.summary_instruction.as_deref(), Some("Be brief."));
        assert_eq!(policy.threshold_tokens(200_000, None), 100_000);
    }

    #[test]
    fn out_of_range_fields_are_named() {
        let bad = |edit: &dyn Fn(&mut CompactionPolicy)| {
            let mut policy = CompactionPolicy::default();
            edit(&mut policy);
            policy.validate().expect_err("refused").field
        };
        assert_eq!(bad(&|p| p.message_trigger = 39), "message_trigger");
        assert_eq!(bad(&|p| p.message_trigger = 1001), "message_trigger");
        assert_eq!(bad(&|p| p.keep_recent = 3), "keep_recent");
        assert_eq!(bad(&|p| p.keep_recent = 101), "keep_recent");
        assert_eq!(bad(&|p| p.window_percent = 24), "window_percent");
        assert_eq!(bad(&|p| p.window_percent = 96), "window_percent");
        assert_eq!(bad(&|p| p.summary_tokens = Some(499)), "summary_tokens");
        assert_eq!(bad(&|p| p.summary_tokens = Some(16_001)), "summary_tokens");
        assert_eq!(
            bad(&|p| p.summary_instruction = Some("  ".into())),
            "summary_instruction"
        );
        assert_eq!(
            bad(&|p| p.summary_instruction = Some("x".repeat(4097))),
            "summary_instruction"
        );
        // keep_recent must leave something to summarize.
        assert_eq!(
            bad(&|p| {
                p.message_trigger = 40;
                p.keep_recent = 40;
            }),
            "keep_recent"
        );
    }
}
