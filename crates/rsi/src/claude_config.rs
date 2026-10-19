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
//! Skills are still read from this host's `~/.claude/skills/`.

use std::io;
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

/// The home directory whose `.claude/skills` is the user skill tier.
pub(crate) fn skills_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Absolute path to the user-tier skills directory.
#[must_use]
pub fn skills_dir() -> PathBuf {
    skills_home().join(".claude").join("skills")
}

/// One user-installed skill under `~/.claude/skills/`.
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

/// List the skill directories of `~/.claude/skills/` through the shared
/// `rsi_common::input_assets::list_skills` that `rsid`'s `ListSkills` serves
/// (hub #3062): files are skipped, and so is any entry whose canonical path
/// leaves that directory. Order is alphabetical by stripped name.
pub fn list_user_skills() -> io::Result<Vec<SkillEntry>> {
    list_skills_in(&skills_home())
}

pub(crate) fn list_skills_in(home: &std::path::Path) -> io::Result<Vec<SkillEntry>> {
    use rsi_common::input_assets::{DISABLED_SKILL_SUFFIX, MAX_SKILLS, SkillScope, list_skills};
    let scope = SkillScope::User {
        home: home.to_path_buf(),
    };
    let listing = list_skills(&scope, MAX_SKILLS)?;
    let dir = PathBuf::from(&listing.skills_dir);
    Ok(listing
        .skills
        .into_iter()
        .map(|skill| {
            let dir_name = if skill.enabled {
                skill.name.clone()
            } else {
                format!("{}{DISABLED_SKILL_SUFFIX}", skill.name)
            };
            SkillEntry {
                path: dir.join(dir_name),
                name: skill.name,
                enabled: skill.enabled,
                description: skill.description,
            }
        })
        .collect())
}

/// Enable or disable a user skill (the `e` key) through the shared
/// `rsi_common::input_assets::toggle_skill` that `rsid`'s `SetSkillEnabled`
/// serves (#97): renames `<name>` <-> `<name>.disabled` inside the pinned
/// `~/.claude/skills`. Returns the new enabled state.
pub fn toggle_skill(name: &str) -> io::Result<bool> {
    toggle_skill_in(&skills_home(), name)
}

fn toggle_skill_in(home: &std::path::Path, name: &str) -> io::Result<bool> {
    let scope = rsi_common::input_assets::SkillScope::User {
        home: home.to_path_buf(),
    };
    Ok(rsi_common::input_assets::toggle_skill(&scope, name)?.enabled)
}

/// Delete a user skill directory (regardless of disabled state, the `d`
/// key) through the shared `rsi_common::input_assets::delete_skill` that
/// `rsid`'s `DeleteSkill` serves (#97): the whole tree, never following a link.
pub fn delete_skill(name: &str) -> io::Result<()> {
    delete_skill_in(&skills_home(), name)
}

pub(crate) fn delete_skill_in(home: &std::path::Path, name: &str) -> io::Result<()> {
    let scope = rsi_common::input_assets::SkillScope::User {
        home: home.to_path_buf(),
    };
    rsi_common::input_assets::delete_skill(&scope, name)?;
    Ok(())
}

/// Read the SKILL.md content for a given skill name (enabled or disabled)
/// through the shared `rsi_common::input_assets::read_skill` that `rsid`'s
/// `GetSkill` serves (hub #3062): contained in `~/.claude/skills`, capped at
/// `MAX_SKILL_BYTES`.
pub fn read_skill_content(name: &str) -> io::Result<String> {
    read_skill_content_in(&skills_home(), name)
}

fn read_skill_content_in(home: &std::path::Path, name: &str) -> io::Result<String> {
    use rsi_common::input_assets::{MAX_SKILL_BYTES, SkillScope, read_skill};
    let scope = SkillScope::User {
        home: home.to_path_buf(),
    };
    Ok(read_skill(&scope, name, MAX_SKILL_BYTES)?.content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Per-process unique tempdir under the system temp root. Avoids the
    /// `tempfile` dependency since `rsi` doesn't already pull it in.
    fn fresh_tmp(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rsi-claude-config-{}-{}-{}",
            label,
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

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

    fn touch_skill(root: &std::path::Path, name: &str, description: Option<&str>) {
        let skill_dir = root.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        let body = match description {
            Some(d) => format!("---\ndescription: {}\n---\n\nhello\n", d),
            None => "no frontmatter\n".to_string(),
        };
        fs::write(skill_dir.join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn list_user_skills_filters_files_and_strips_disabled_suffix() {
        let home = fresh_tmp("skills");
        let dir = home.join(".claude").join("skills");
        // alpha — enabled, with description
        touch_skill(&dir, "alpha", Some("Alpha skill"));
        // beta.disabled — disabled
        touch_skill(&dir, "beta.disabled", Some("Beta skill"));
        // gamma — enabled, no frontmatter
        touch_skill(&dir, "gamma", None);
        // a stray plain file
        fs::write(dir.join("README.txt"), "ignore me").unwrap();

        let skills = list_skills_in(&home).unwrap();
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);

        let alpha = skills.iter().find(|s| s.name == "alpha").unwrap();
        assert!(alpha.enabled);
        assert_eq!(alpha.description.as_deref(), Some("Alpha skill"));
        assert_eq!(alpha.path, dir.canonicalize().unwrap().join("alpha"));

        let beta = skills.iter().find(|s| s.name == "beta").unwrap();
        assert!(!beta.enabled);
        assert_eq!(beta.description.as_deref(), Some("Beta skill"));
        assert_eq!(beta.path, dir.canonicalize().unwrap().join("beta.disabled"));

        let gamma = skills.iter().find(|s| s.name == "gamma").unwrap();
        assert!(gamma.enabled);
        assert!(gamma.description.is_none());
    }

    #[test]
    fn toggle_skill_round_trips() {
        let home = fresh_tmp("toggle");
        let dir = home.join(".claude").join("skills");
        touch_skill(&dir, "demo", None);

        let now_enabled = toggle_skill_in(&home, "demo").unwrap();
        assert!(!now_enabled, "first toggle must disable");
        assert!(dir.join("demo.disabled").exists());
        assert!(!dir.join("demo").exists());

        let now_enabled = toggle_skill_in(&home, "demo").unwrap();
        assert!(now_enabled, "second toggle must re-enable");
        assert!(dir.join("demo").exists());
        assert!(!dir.join("demo.disabled").exists());

        let missing = toggle_skill_in(&home, "nope").unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn delete_skill_removes_directory() {
        let home = fresh_tmp("delete");
        let dir = home.join(".claude").join("skills");
        touch_skill(&dir, "tobedeleted", None);
        assert!(dir.join("tobedeleted").exists());
        delete_skill_in(&home, "tobedeleted").unwrap();
        assert!(!dir.join("tobedeleted").exists());
    }

    #[test]
    fn delete_skill_works_on_disabled_dir_too() {
        let home = fresh_tmp("delete-disabled");
        let dir = home.join(".claude").join("skills");
        touch_skill(&dir, "x.disabled", None);
        delete_skill_in(&home, "x").unwrap();
        assert!(!dir.join("x.disabled").exists());
        let escape = delete_skill_in(&home, "../x").unwrap_err();
        assert_eq!(escape.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn read_skill_content_returns_md_body() {
        let home = fresh_tmp("read-content");
        let dir = home.join(".claude").join("skills");
        touch_skill(&dir, "demo", Some("Demo"));
        touch_skill(&dir, "parked.disabled", Some("Parked"));
        let content = read_skill_content_in(&home, "demo").unwrap();
        assert_eq!(content, "---\ndescription: Demo\n---\n\nhello\n");
        let parked = read_skill_content_in(&home, "parked").unwrap();
        assert!(parked.contains("Parked"));
        let escape = read_skill_content_in(&home, "../demo").unwrap_err();
        assert_eq!(escape.kind(), io::ErrorKind::InvalidInput);
    }
}
