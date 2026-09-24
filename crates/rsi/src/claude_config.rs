//! `~/.claude/` configuration I/O for the rsi TUI.
//!
//! Round-trip preservation is mandatory: any top-level keys we don't model
//! must survive load -> save unchanged. The `hooks` block is the only
//! strongly-typed sub-tree.
//!
//! Concurrency model (Q7 in the plan): we keep the original raw bytes and
//! mtime of `~/.claude/settings.json` in memory. Before saving we re-stat the
//! file; if either the mtime or the bytes have changed since we loaded, the
//! caller surfaces a three-choice conflict prompt (Overwrite / Reload /
//! Cancel) instead of silently clobbering external edits.

use serde::de::{self, Deserializer, Visitor};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::SystemTime;

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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HookEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    pub hooks: Vec<HookCommand>,
}

/// On-disk shape of `~/.claude/settings.json`.
///
/// Only `hooks` is strongly typed. Everything else (env, permissions,
/// mcpServers, enabledPlugins, skipDangerousModePermissionPrompt, plus any
/// future top-level keys Claude Code introduces) flows through `other` via
/// `#[serde(flatten)]` so load + save preserves the file shape verbatim.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClaudeSettings {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hooks: BTreeMap<HookEvent, Vec<HookEntry>>,
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

/// Loaded settings + identity for conflict detection (Q7).
#[derive(Debug, Clone)]
pub struct LoadedSettings {
    pub data: ClaudeSettings,
    pub mtime: Option<SystemTime>,
    pub original_bytes: Vec<u8>,
}

/// In-memory snapshot of a pending save (used by the conflict prompt overlay).
#[derive(Debug, Clone)]
pub struct PendingHookSave {
    pub data: ClaudeSettings,
}

/// Absolute path to the user-tier settings file.
#[must_use]
pub fn path_user() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".claude").join("settings.json")
}

const fn default_timeout() -> u32 {
    60
}
const fn is_default_timeout(t: &u32) -> bool {
    *t == 60
}

/// Load `~/.claude/settings.json`. If the file doesn't exist, returns an
/// empty `ClaudeSettings` (the file will be created on first save).
pub fn load_user_settings() -> io::Result<LoadedSettings> {
    load_settings_at(&path_user())
}

fn load_settings_at(path: &std::path::Path) -> io::Result<LoadedSettings> {
    if !path.exists() {
        return Ok(LoadedSettings {
            data: ClaudeSettings::default(),
            mtime: None,
            original_bytes: Vec::new(),
        });
    }
    let bytes = fs::read(path)?;
    let mtime = fs::metadata(path).ok().and_then(|m| m.modified().ok());
    let data: ClaudeSettings = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(LoadedSettings {
        data,
        mtime,
        original_bytes: bytes,
    })
}

/// Atomic write: tmp file in same dir, fsync, rename. Survives torn reads
/// from the Claude CLI reading concurrently.
pub fn save_user_settings(settings: &ClaudeSettings) -> io::Result<()> {
    save_settings_at(&path_user(), settings)
}

fn save_settings_at(path: &std::path::Path, settings: &ClaudeSettings) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // tmp file in the SAME directory as the destination so the rename is atomic
    // (rename across filesystems is not atomic on Linux).
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let tmp_name = format!(
        ".{}.rsi-tmp.{}",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("settings.json"),
        std::process::id()
    );
    let tmp = parent.join(tmp_name);
    {
        let mut f = fs::File::create(&tmp)?;
        let pretty = serde_json::to_vec_pretty(settings)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        f.write_all(&pretty)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Cheap restat for conflict detection at save time. Returns mtime + raw
/// bytes (caller compares bytes against the snapshot). No crypto hash.
pub fn stat_user_settings() -> io::Result<(Option<SystemTime>, Vec<u8>)> {
    let path = path_user();
    if !path.exists() {
        return Ok((None, Vec::new()));
    }
    let bytes = fs::read(&path)?;
    let mtime = fs::metadata(&path).ok().and_then(|m| m.modified().ok());
    Ok((mtime, bytes))
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
pub fn flatten_hooks(settings: &ClaudeSettings) -> Vec<HookRow> {
    let mut rows = Vec::new();
    for (event, entries) in &settings.hooks {
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

/// Absolute path to the user-tier skills directory.
#[must_use]
pub fn skills_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".claude").join("skills")
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
    /// First non-empty `description:` line from the optional SKILL.md
    /// frontmatter, if any.
    pub description: Option<String>,
}

/// List immediate child directories of `~/.claude/skills/`. Files and
/// symlinks are skipped. Order is alphabetical by stripped name.
pub fn list_user_skills() -> io::Result<Vec<SkillEntry>> {
    list_skills_in(&skills_dir())
}

fn list_skills_in(root: &std::path::Path) -> io::Result<Vec<SkillEntry>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        // Use file_type() instead of metadata() to avoid following symlinks.
        let ft = entry.file_type()?;
        if !ft.is_dir() {
            continue;
        }
        let path = entry.path();
        let raw_name = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue, // skip non-UTF8 names
        };
        let (name, enabled) = match raw_name.strip_suffix(".disabled") {
            Some(stem) => (stem.to_string(), false),
            None => (raw_name.clone(), true),
        };
        let description = read_skill_description(&path);
        out.push(SkillEntry {
            name,
            path,
            enabled,
            description,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Read the `description:` line from `SKILL.md` frontmatter if present.
/// Returns the first non-empty value seen. Best-effort: any I/O or parse
/// error yields `None`.
fn read_skill_description(skill_dir: &std::path::Path) -> Option<String> {
    let md = skill_dir.join("SKILL.md");
    let body = fs::read_to_string(&md).ok()?;
    let mut lines = body.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        if line.trim() == "---" {
            return None;
        }
        if let Some(rest) = line.strip_prefix("description:") {
            let value = rest.trim().trim_matches('"').trim_matches('\'').to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// Toggle a user skill's enabled state via directory rename
/// (`<name>/` ↔ `<name>.disabled/`). Returns the new state.
pub fn toggle_skill(name: &str) -> io::Result<bool> {
    toggle_skill_in(&skills_dir(), name)
}

fn toggle_skill_in(root: &std::path::Path, name: &str) -> io::Result<bool> {
    let enabled_path = root.join(name);
    let disabled_path = root.join(format!("{}.disabled", name));
    if enabled_path.exists() {
        fs::rename(&enabled_path, &disabled_path)?;
        Ok(false)
    } else if disabled_path.exists() {
        fs::rename(&disabled_path, &enabled_path)?;
        Ok(true)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("skill not found: {}", name),
        ))
    }
}

/// Delete a user skill directory (regardless of disabled state).
pub fn delete_skill(name: &str) -> io::Result<()> {
    delete_skill_in(&skills_dir(), name)
}

fn delete_skill_in(root: &std::path::Path, name: &str) -> io::Result<()> {
    let enabled_path = root.join(name);
    let disabled_path = root.join(format!("{}.disabled", name));
    if enabled_path.exists() {
        fs::remove_dir_all(&enabled_path)
    } else if disabled_path.exists() {
        fs::remove_dir_all(&disabled_path)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("skill not found: {}", name),
        ))
    }
}

/// Read the SKILL.md content for a given skill name (enabled or disabled).
pub fn read_skill_content(name: &str) -> io::Result<String> {
    read_skill_content_in(&skills_dir(), name)
}

fn read_skill_content_in(root: &std::path::Path, name: &str) -> io::Result<String> {
    let enabled = root.join(name).join("SKILL.md");
    let disabled = root.join(format!("{}.disabled", name)).join("SKILL.md");
    if enabled.exists() {
        fs::read_to_string(&enabled)
    } else if disabled.exists() {
        fs::read_to_string(&disabled)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("SKILL.md not found for: {}", name),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    const SAMPLE: &str = r#"{
      "env": {"CLAUDE_FOO": "1"},
      "permissions": {"allow": ["bash:ls", "bash:rg"]},
      "hooks": {
        "Stop": [{"hooks": [{"type": "command", "command": "echo done", "timeout": 10}]}],
        "PreToolUse": [{"matcher": "Bash.*", "hooks": [{"type": "command", "command": "true"}]}]
      },
      "enabledPlugins": {},
      "mcpServers": {},
      "skipDangerousModePermissionPrompt": true,
      "futureKey": {"nested": "preserved"}
    }"#;

    #[test]
    fn settings_roundtrip_preserves_unknown_keys() {
        let parsed: ClaudeSettings = serde_json::from_str(SAMPLE).unwrap();
        let reser = serde_json::to_string(&parsed).unwrap();
        let reparsed: ClaudeSettings = serde_json::from_str(&reser).unwrap();

        assert!(
            reparsed
                .hooks
                .contains_key(&HookEvent::Known(KnownHookEvent::Stop))
        );
        assert!(
            reparsed
                .hooks
                .contains_key(&HookEvent::Known(KnownHookEvent::PreToolUse))
        );

        assert!(reparsed.other.contains_key("env"));
        assert!(reparsed.other.contains_key("permissions"));
        assert!(reparsed.other.contains_key("enabledPlugins"));
        assert!(reparsed.other.contains_key("mcpServers"));
        assert!(
            reparsed
                .other
                .contains_key("skipDangerousModePermissionPrompt")
        );
        assert_eq!(
            reparsed.other.get("futureKey").unwrap()["nested"],
            "preserved"
        );
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

    #[test]
    fn invalid_json_returns_error() {
        let result: Result<ClaudeSettings, _> = serde_json::from_str("{not json");
        assert!(result.is_err());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = fresh_tmp("roundtrip");
        let path = dir.join("settings.json");

        let mut settings: ClaudeSettings = serde_json::from_str(SAMPLE).unwrap();
        settings.hooks.insert(
            HookEvent::Known(KnownHookEvent::SessionStart),
            vec![HookEntry {
                matcher: None,
                hooks: vec![HookCommand {
                    kind: "command".to_string(),
                    command: "echo start".to_string(),
                    timeout: 60,
                }],
            }],
        );

        save_settings_at(&path, &settings).unwrap();
        let loaded = load_settings_at(&path).unwrap();

        assert!(
            loaded
                .data
                .hooks
                .contains_key(&HookEvent::Known(KnownHookEvent::SessionStart))
        );
        assert!(loaded.data.other.contains_key("env"));
        assert!(loaded.data.other.contains_key("futureKey"));
        assert!(!loaded.original_bytes.is_empty());
    }

    #[test]
    fn save_uses_atomic_rename_no_partial_file() {
        let dir = fresh_tmp("atomic");
        let path = dir.join("settings.json");
        // Pre-existing file content
        fs::write(&path, br#"{"env":{}}"#).unwrap();
        let original = fs::read(&path).unwrap();

        let settings = ClaudeSettings::default();
        save_settings_at(&path, &settings).unwrap();

        // After save, the only files in the dir should be settings.json.
        // No tmp leftovers.
        let files: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert_eq!(
            files.len(),
            1,
            "tmp file must not survive a successful save"
        );

        // And the contents have changed.
        let new_bytes = fs::read(&path).unwrap();
        assert_ne!(original, new_bytes);
    }

    #[test]
    fn flatten_hooks_orders_deterministically() {
        let mut settings = ClaudeSettings::default();
        settings.hooks.insert(
            HookEvent::Known(KnownHookEvent::Stop),
            vec![HookEntry {
                matcher: None,
                hooks: vec![HookCommand {
                    kind: "command".to_string(),
                    command: "a".to_string(),
                    timeout: 60,
                }],
            }],
        );
        settings.hooks.insert(
            HookEvent::Known(KnownHookEvent::PreToolUse),
            vec![HookEntry {
                matcher: Some("Bash.*".to_string()),
                hooks: vec![HookCommand {
                    kind: "command".to_string(),
                    command: "b".to_string(),
                    timeout: 60,
                }],
            }],
        );

        let rows = flatten_hooks(&settings);
        assert_eq!(rows.len(), 2);
        // KnownHookEvent ordering: PreToolUse < Stop.
        assert_eq!(rows[0].command, "b");
        assert_eq!(rows[1].command, "a");
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
        let dir = fresh_tmp("skills");
        // alpha — enabled, with description
        touch_skill(&dir, "alpha", Some("Alpha skill"));
        // beta.disabled — disabled
        touch_skill(&dir, "beta.disabled", Some("Beta skill"));
        // gamma — enabled, no frontmatter
        touch_skill(&dir, "gamma", None);
        // a stray plain file
        fs::write(dir.join("README.txt"), "ignore me").unwrap();

        let skills = list_skills_in(&dir).unwrap();
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);

        let alpha = skills.iter().find(|s| s.name == "alpha").unwrap();
        assert!(alpha.enabled);
        assert_eq!(alpha.description.as_deref(), Some("Alpha skill"));

        let beta = skills.iter().find(|s| s.name == "beta").unwrap();
        assert!(!beta.enabled);
        assert_eq!(beta.description.as_deref(), Some("Beta skill"));

        let gamma = skills.iter().find(|s| s.name == "gamma").unwrap();
        assert!(gamma.enabled);
        assert!(gamma.description.is_none());
    }

    #[test]
    fn toggle_skill_round_trips() {
        let dir = fresh_tmp("toggle");
        touch_skill(&dir, "demo", None);

        let now_enabled = toggle_skill_in(&dir, "demo").unwrap();
        assert!(!now_enabled, "first toggle must disable");
        assert!(dir.join("demo.disabled").exists());
        assert!(!dir.join("demo").exists());

        let now_enabled = toggle_skill_in(&dir, "demo").unwrap();
        assert!(now_enabled, "second toggle must re-enable");
        assert!(dir.join("demo").exists());
        assert!(!dir.join("demo.disabled").exists());
    }

    #[test]
    fn delete_skill_removes_directory() {
        let dir = fresh_tmp("delete");
        touch_skill(&dir, "tobedeleted", None);
        assert!(dir.join("tobedeleted").exists());
        delete_skill_in(&dir, "tobedeleted").unwrap();
        assert!(!dir.join("tobedeleted").exists());
    }

    #[test]
    fn delete_skill_works_on_disabled_dir_too() {
        let dir = fresh_tmp("delete-disabled");
        touch_skill(&dir, "x.disabled", None);
        delete_skill_in(&dir, "x").unwrap();
        assert!(!dir.join("x.disabled").exists());
    }

    #[test]
    fn read_skill_content_returns_md_body() {
        let dir = fresh_tmp("read-content");
        touch_skill(&dir, "demo", Some("Demo"));
        let content = read_skill_content_in(&dir, "demo").unwrap();
        assert!(content.contains("hello"));
    }
}
