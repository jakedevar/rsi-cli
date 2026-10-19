//! Claude Code hook types shared by the daemon and every frontend (hub #3039).
//!
//! The `hooks` block of `~/.claude/settings.json` is owned by the daemon's
//! operator RPCs (`ListClaudeHooks` / `PutClaudeHooks`,
//! [`crate::operator_config`]); the TUI and the desktop edit it through them.
//! Unknown event names and unknown hook `type`s round-trip verbatim.

use serde::de::{self, Deserializer, Visitor};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// The typed `hooks` block: event name to its ordered hook entries.
pub type ClaudeHooks = BTreeMap<HookEvent, Vec<HookEntry>>;

/// Claude Code hook event identifier.
///
/// Modelled as Known + Other so unknown event names round-trip verbatim
/// instead of being silently rewritten when the TUI loads + saves
/// `~/.claude/settings.json`. Custom `Serialize` / `Deserialize` impls treat
/// the value as a plain string so it can serve as a JSON object key (which
/// `#[serde(untagged)]` cannot do directly).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HookEvent {
    /// Strongly-typed event names we model directly. Order on `KnownHookEvent`
    /// drives display order; `Known` sorts before `Other` so known events
    /// always come first in the flattened picker.
    Known(KnownHookEvent),
    /// Forward-compat fallback: any future event name we don't model is
    /// preserved verbatim.
    Other(String),
}

/// Strongly-typed Claude Code hook event names (as of plan v2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KnownHookEvent {
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
    SessionStart,
    SessionEnd,
    Stop,
    SubagentStop,
    Notification,
    PreCompact,
}

impl KnownHookEvent {
    /// Display order for the cyclable picker. Stable and exhaustive.
    pub const ALL: &[KnownHookEvent] = &[
        Self::PreToolUse,
        Self::PostToolUse,
        Self::UserPromptSubmit,
        Self::SessionStart,
        Self::SessionEnd,
        Self::Stop,
        Self::SubagentStop,
        Self::Notification,
        Self::PreCompact,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::Stop => "Stop",
            Self::SubagentStop => "SubagentStop",
            Self::Notification => "Notification",
            Self::PreCompact => "PreCompact",
        }
    }

    #[must_use]
    pub fn from_label(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.label() == s)
    }

    /// Whether this event supports a regex `matcher` (`PreToolUse` / `PostToolUse` only).
    #[must_use]
    pub const fn supports_matcher(self) -> bool {
        matches!(self, Self::PreToolUse | Self::PostToolUse)
    }
}

impl HookEvent {
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Known(k) => k.label().to_string(),
            Self::Other(s) => s.clone(),
        }
    }

    #[must_use]
    pub fn supports_matcher(&self) -> bool {
        match self {
            Self::Known(k) => k.supports_matcher(),
            // Be permissive for forward-compat: unknown event names may carry a matcher.
            Self::Other(_) => true,
        }
    }

    /// Build from a raw event-name string (Claude Code uses `PascalCase`).
    #[must_use]
    pub fn from_name(s: &str) -> Self {
        match KnownHookEvent::from_label(s) {
            Some(k) => Self::Known(k),
            None => Self::Other(s.to_string()),
        }
    }
}

impl Serialize for HookEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            HookEvent::Known(k) => serializer.serialize_str(k.label()),
            HookEvent::Other(s) => serializer.serialize_str(s),
        }
    }
}

impl<'de> Deserialize<'de> for HookEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct HookEventVisitor;

        impl<'de> Visitor<'de> for HookEventVisitor {
            type Value = HookEvent;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a Claude Code hook event name")
            }

            fn visit_str<E>(self, v: &str) -> Result<HookEvent, E>
            where
                E: de::Error,
            {
                Ok(HookEvent::from_name(v))
            }

            fn visit_string<E>(self, v: String) -> Result<HookEvent, E>
            where
                E: de::Error,
            {
                Ok(match KnownHookEvent::from_label(&v) {
                    Some(k) => HookEvent::Known(k),
                    None => HookEvent::Other(v),
                })
            }
        }

        deserializer.deserialize_str(HookEventVisitor)
    }
}

/// A single shell-command hook. `kind` is "command" today; we keep it as a
/// String so future Claude Code hook types (e.g. "rpc") round-trip.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookCommand {
    #[serde(rename = "type")]
    pub kind: String,
    pub command: String,
    /// Timeout in seconds. Defaults to 60 when missing on disk; the default
    /// is also `skip_serializing_if`-elided so we don't introduce drift in
    /// previously-unset entries.
    #[serde(
        default = "default_timeout",
        skip_serializing_if = "is_default_timeout"
    )]
    pub timeout: u32,
}

/// One bucket of hook commands under a given event. PreToolUse / PostToolUse
/// entries can carry a regex matcher; other events ignore it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    pub hooks: Vec<HookCommand>,
}

const fn default_timeout() -> u32 {
    60
}
const fn is_default_timeout(t: &u32) -> bool {
    *t == 60
}
