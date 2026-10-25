//! `~/.claude/` configuration for the rsi TUI.
//!
//! Hooks (hub #3039): the daemon owns the `hooks` block of
//! `~/.claude/settings.json` (`ListClaudeHooks` / `PutClaudeHooks`), so the
//! TUI and the desktop edit one source. The TUI keeps the daemon's view in
//! `App::cached_claude_hooks` and expresses each change as a pure
//! [`HookEdit`] applied to that snapshot; the put names the snapshot's
//! revision, and a stale revision reapplies the edit to the current hooks and
//! opens the Overwrite / Reload / Cancel prompt (the Q7 conflict check).
//!
//! Skills (local #91): the daemon owns the user skill tier
//! (`~/.claude/skills` of the daemon user) through `ListSkills`, `GetSkill`,
//! `SetSkillEnabled` and `DeleteSkill`; the TUI keeps the daemon's listing
//! in `App::cached_user_skills` ([`skill_entries`]), like the desktop.

use std::path::PathBuf;

pub use rsi_common::claude_hooks::{
    ClaudeHooks, HookCommand, HookEntry, HookEvent, KnownHookEvent,
};

/// A hook save waiting on the conflict prompt: the edit, already reapplied to
/// the daemon's current hooks, and the revision an overwrite names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingHookSave {
    pub edit: HookEdit,
    pub revision: String,
    pub hooks: ClaudeHooks,
}

/// One change the hook form or the Hooks list makes. Pure, so a conflict can
/// replay it on top of the hooks another writer left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEdit {
    /// Add a hook (`editing: None`) or replace the one at `editing`.
    Upsert {
        editing: Option<crate::types::HookFormEditingTarget>,
        event: HookEvent,
        matcher: Option<String>,
        command: HookCommand,
    },
    /// Remove one hook command, dropping an emptied entry and event.
    Delete {
        event: HookEvent,
        entry_index: usize,
        hook_index: usize,
    },
}

impl HookEdit {
    /// Apply the edit to `hooks` and return the result.
    #[must_use]
    pub fn apply(&self, mut hooks: ClaudeHooks) -> ClaudeHooks {
        match self {
            Self::Delete {
                event,
                entry_index,
                hook_index,
            } => {
                remove_hook(&mut hooks, event, *entry_index, *hook_index);
            }
            Self::Upsert {
                editing: None,
                event,
                matcher,
                command,
            } => {
                // New entry: a fresh single-command bucket so each row is
                // independently editable.
                hooks.entry(event.clone()).or_default().push(HookEntry {
                    matcher: matcher.clone(),
                    hooks: vec![command.clone()],
                });
            }
            Self::Upsert {
                editing: Some(target),
                event,
                matcher,
                command,
            } => {
                if target.event != *event {
                    // The event changed: remove the old cell, append to the new bucket.
                    remove_hook(
                        &mut hooks,
                        &target.event,
                        target.entry_index,
                        target.hook_index,
                    );
                    hooks.entry(event.clone()).or_default().push(HookEntry {
                        matcher: matcher.clone(),
                        hooks: vec![command.clone()],
                    });
                } else {
                    let entries = hooks.entry(event.clone()).or_default();
                    if let Some(entry) = entries.get_mut(target.entry_index) {
                        entry.matcher = matcher.clone();
                        if let Some(slot) = entry.hooks.get_mut(target.hook_index) {
                            *slot = command.clone();
                        } else {
                            entry.hooks.push(command.clone());
                        }
                    } else {
                        entries.push(HookEntry {
                            matcher: matcher.clone(),
                            hooks: vec![command.clone()],
                        });
                    }
                }
            }
        }
        hooks
    }
}

fn remove_hook(hooks: &mut ClaudeHooks, event: &HookEvent, entry_index: usize, hook_index: usize) {
    if let Some(entries) = hooks.get_mut(event) {
        if let Some(entry) = entries.get_mut(entry_index) {
            if hook_index < entry.hooks.len() {
                entry.hooks.remove(hook_index);
            }
            if entry.hooks.is_empty() {
                entries.remove(entry_index);
            }
        }
        if entries.is_empty() {
            hooks.remove(event);
        }
    }
}

/// Flat row projection used by the Settings pane Hooks listing. Each row
/// references one (event, entry, hook) triple in the BTreeMap, so the
/// renderer / handlers can convert a flat `selected_index` back to the
/// nested coordinate.
#[derive(Debug, Clone)]
pub struct HookRow {
    pub event: HookEvent,
    pub entry_index: usize,
    pub hook_index: usize,
    pub matcher: Option<String>,
    pub command: String,
    pub timeout: u32,
}

/// Flatten the hooks BTreeMap into a deterministically-ordered Vec of rows.
pub fn flatten_hooks(hooks: &ClaudeHooks) -> Vec<HookRow> {
    let mut rows = Vec::new();
    for (event, entries) in hooks {
        for (entry_index, entry) in entries.iter().enumerate() {
            for (hook_index, cmd) in entry.hooks.iter().enumerate() {
                rows.push(HookRow {
                    event: event.clone(),
                    entry_index,
                    hook_index,
                    matcher: entry.matcher.clone(),
                    command: cmd.command.clone(),
                    timeout: cmd.timeout,
                });
            }
        }
    }
    rows
}

// ---------------------------------------------------------------------------
// Skills (Phase 2)
// ---------------------------------------------------------------------------

/// One skill of the daemon user's `~/.claude/skills/` (`ListSkills`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    /// Display name with any `.disabled` suffix stripped.
    pub name: String,
    /// Absolute path to the skill directory (with or without `.disabled`).
    pub path: PathBuf,
    /// True when the directory does NOT end in `.disabled`.
    pub enabled: bool,
    /// The `description` from the optional SKILL.md frontmatter, if any.
    pub description: Option<String>,
}

/// The Settings › Claude Skills rows of a daemon `ListSkills` listing
/// (`user: true`), in the daemon's (alphabetical) order.
#[must_use]
pub fn skill_entries(listing: &rsi_common::input_assets::ListSkillsResponseV1) -> Vec<SkillEntry> {
    use rsi_common::input_assets::DISABLED_SKILL_SUFFIX;
    let dir = PathBuf::from(&listing.skills_dir);
    listing
        .skills
        .iter()
        .map(|skill| {
            let dir_name = if skill.enabled {
                skill.name.clone()
            } else {
                format!("{}{DISABLED_SKILL_SUFFIX}", skill.name)
            };
            SkillEntry {
                path: dir.join(dir_name),
                name: skill.name.clone(),
                enabled: skill.enabled,
                description: skill.description.clone(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn hook_command_default_timeout() {
        let json = r#"{"type":"command","command":"echo hi"}"#;
        let cmd: HookCommand = serde_json::from_str(json).unwrap();
        assert_eq!(cmd.timeout, 60);
    }

    #[test]
    fn hook_command_default_timeout_round_trips_without_drift() {
        // A hook entry stored without a `timeout` field must serialize back
        // without injecting `"timeout": 60` (preserves user's original shape).
        let json = r#"{"type":"command","command":"echo hi"}"#;
        let cmd: HookCommand = serde_json::from_str(json).unwrap();
        let back = serde_json::to_string(&cmd).unwrap();
        assert!(!back.contains("timeout"));
    }

    #[test]
    fn hook_event_unknown_event_round_trips() {
        // Untagged enum: unknown event names deserialize into Other(String) and
        // serialize back to the same string when used as a JSON object key.
        let json = r#"{"NewEventType": [{"hooks": [{"type":"command","command":"x"}]}]}"#;
        let map: BTreeMap<HookEvent, Vec<HookEntry>> = serde_json::from_str(json).unwrap();
        let back = serde_json::to_string(&map).unwrap();
        assert!(back.contains("NewEventType"));
        let key = map.keys().next().unwrap();
        match key {
            HookEvent::Other(s) => assert_eq!(s, "NewEventType"),
            HookEvent::Known(_) => panic!("expected Other variant for unknown event name"),
        }
    }

    fn command(text: &str) -> HookCommand {
        HookCommand {
            kind: "command".to_string(),
            command: text.to_string(),
            timeout: 60,
        }
    }

    #[test]
    fn flatten_hooks_orders_deterministically() {
        let mut hooks = ClaudeHooks::new();
        hooks.insert(
            HookEvent::Known(KnownHookEvent::Stop),
            vec![HookEntry {
                matcher: None,
                hooks: vec![command("a")],
            }],
        );
        hooks.insert(
            HookEvent::Known(KnownHookEvent::PreToolUse),
            vec![HookEntry {
                matcher: Some("Bash.*".to_string()),
                hooks: vec![command("b")],
            }],
        );

        let rows = flatten_hooks(&hooks);
        assert_eq!(rows.len(), 2);
        // KnownHookEvent ordering: PreToolUse < Stop.
        assert_eq!(rows[0].command, "b");
        assert_eq!(rows[1].command, "a");
    }

    #[test]
    fn hook_edits_add_replace_move_and_delete() {
        let stop = HookEvent::Known(KnownHookEvent::Stop);
        let start = HookEvent::Known(KnownHookEvent::SessionStart);
        let added = HookEdit::Upsert {
            editing: None,
            event: stop.clone(),
            matcher: None,
            command: command("one"),
        }
        .apply(ClaudeHooks::new());
        assert_eq!(added[&stop][0].hooks[0].command, "one");

        let target = crate::types::HookFormEditingTarget {
            event: stop.clone(),
            entry_index: 0,
            hook_index: 0,
        };
        let replaced = HookEdit::Upsert {
            editing: Some(target.clone()),
            event: stop.clone(),
            matcher: None,
            command: command("two"),
        }
        .apply(added);
        assert_eq!(replaced[&stop][0].hooks[0].command, "two");

        let moved = HookEdit::Upsert {
            editing: Some(target),
            event: start.clone(),
            matcher: None,
            command: command("three"),
        }
        .apply(replaced);
        assert_eq!(
            moved.keys().cloned().collect::<Vec<_>>(),
            vec![start.clone()]
        );
        assert_eq!(moved[&start][0].hooks[0].command, "three");

        let deleted = HookEdit::Delete {
            event: start,
            entry_index: 0,
            hook_index: 0,
        }
        .apply(moved);
        assert_eq!(deleted, ClaudeHooks::new());
    }

    // --- Skills tests --------------------------------------------------------

    #[test]
    fn skill_entries_map_the_daemon_listing_to_rows() {
        let listing: rsi_common::input_assets::ListSkillsResponseV1 =
            serde_json::from_value(serde_json::json!({
                "skills_dir": "/home/op/.claude/skills",
                "skills": [
                    {"name": "alpha", "enabled": true, "description": "Alpha skill", "has_skill_md": true},
                    {"name": "beta", "enabled": false, "description": null, "has_skill_md": true}
                ],
                "truncated": false,
                "skipped_outside_root": 0
            }))
            .unwrap();
        assert_eq!(
            skill_entries(&listing),
            vec![
                SkillEntry {
                    name: "alpha".to_string(),
                    path: PathBuf::from("/home/op/.claude/skills/alpha"),
                    enabled: true,
                    description: Some("Alpha skill".to_string()),
                },
                SkillEntry {
                    name: "beta".to_string(),
                    path: PathBuf::from("/home/op/.claude/skills/beta.disabled"),
                    enabled: false,
                    description: None,
                },
            ]
        );
    }
}
