//! Operator text-editing mode (`editing_mode` daemon setting, Issue #1628).
//!
//! `standard` edits like a normal text field (typing inserts, arrows move, no
//! modal states); `vim` is the modal editing RSI was built around. The setting
//! is `unset` until the operator answers the first-start prompt (or an
//! existing install is detected and kept on Vim). The vocabulary lives here
//! because the daemon validator and the TUI cycle row must not drift.

use serde::{Deserialize, Serialize};

/// The daemon-config field name (`daemon_settings` key).
pub const EDITING_MODE_FIELD: &str = "editing_mode";

/// The stored value before the operator has chosen.
pub const EDITING_MODE_UNSET: &str = "unset";

/// Every stored value, `unset` first (the default).
pub const EDITING_MODE_CHOICES: &[&str] = &[EDITING_MODE_UNSET, "standard", "vim"];

/// A chosen editing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditingMode {
    Standard,
    /// The behaviour of every build before the setting existed.
    #[default]
    Vim,
}

impl EditingMode {
    /// The two choosable modes in prompt order (Standard left, Vim right).
    pub const ALL: [Self; 2] = [Self::Standard, Self::Vim];

    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Vim => "vim",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Standard => "Standard",
            Self::Vim => "Vim",
        }
    }

    /// Parse a stored value: `Some(mode)` for `standard`/`vim`, `None` for
    /// `unset` and anything else.
    #[must_use]
    pub fn from_slug(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "standard" => Some(Self::Standard),
            "vim" => Some(Self::Vim),
            _ => None,
        }
    }
}

/// Normalize an operator-supplied value to its canonical stored spelling, or
/// `None` when it names no legal value. The empty string and JSON `null` are
/// spellings of `unset` at the caller's discretion; the empty string is
/// accepted here.
#[must_use]
pub fn normalize_editing_mode(raw: &str) -> Option<&'static str> {
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() || normalized == EDITING_MODE_UNSET {
        return Some(EDITING_MODE_UNSET);
    }
    EDITING_MODE_CHOICES
        .iter()
        .find(|choice| **choice == normalized)
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_accepts_every_choice_and_case() {
        for choice in EDITING_MODE_CHOICES {
            assert_eq!(normalize_editing_mode(choice), Some(*choice));
        }
        assert_eq!(normalize_editing_mode(" VIM "), Some("vim"));
        assert_eq!(normalize_editing_mode("Standard"), Some("standard"));
        assert_eq!(normalize_editing_mode(""), Some(EDITING_MODE_UNSET));
    }

    #[test]
    fn normalize_rejects_unknown() {
        assert_eq!(normalize_editing_mode("emacs"), None);
    }

    #[test]
    fn slug_round_trips_and_prompt_order_is_standard_first() {
        for mode in EditingMode::ALL {
            assert_eq!(EditingMode::from_slug(mode.slug()), Some(mode));
        }
        assert_eq!(EditingMode::ALL[0], EditingMode::Standard);
        assert_eq!(EditingMode::from_slug(EDITING_MODE_UNSET), None);
    }
}
