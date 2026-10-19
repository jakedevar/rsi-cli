//! #2767 (#2748 G16): deterministic skills and plugins delivery.
//!
//! The operator setting `provider_skill_delivery` ([`SkillDelivery`]) chooses how
//! a Claude or Codex launch gets the repository's agent skills:
//!
//! - `ambient` (default): today's behaviour, byte for byte. The CLI discovers
//!   skills from the sandbox working directory and the operator's home.
//! - `explicit`: the launch names its skills and suppresses every other source,
//!   so the loaded set is exactly the delivered one. Claude gets one
//!   `--plugin-dir` (a content-addressed plugin named [`PLUGIN_NAME`] built from
//!   the repository's `.claude/skills`) plus `--setting-sources ""`
//!   ([`CLAUDE_AMBIENT_SUPPRESS_ARGS`]), which stops the operator's
//!   `~/.claude/skills`, the working directory's `.claude/skills` and every
//!   settings-enabled plugin from loading (#2923). Codex gets
//!   `-c skills.bundled.enabled=false` and one `-c skills.config=[...]` override
//!   that enables exactly the repository's `.agents/skills` and disables every
//!   other skill a recursive, symlink-following scan of every root Codex reads
//!   finds (#2924). When that scan cannot prove it saw every skill, explicit
//!   delivery is refused ([`SkillDeliveryRefusal`]) rather than claimed.
//!
//! Codex's `exec` has no extra-root flag (`skills.extra_roots` and similar are
//! ignored by codex-cli 0.163.0; `skills/extraRoots/set` is an app-server
//! connection call), so its explicit delivery pins the set through the config
//! that does exist: `skills.config` entries enable or disable one skill by the
//! path of its `SKILL.md`.
//!
//! This module holds the pure parts (the setting, skill discovery, the plugin
//! manifest, the Codex override and the init check); `rsid::skill_delivery`
//! materializes the Claude plugin on disk.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// The persisted daemon runtime-config field.
pub const SETTING_FIELD: &str = "provider_skill_delivery";
/// Name of the plugin a Claude launch delivers. Its skills are namespaced
/// `rsi-skills:<skill>`; the init `plugins` array lists it by this name.
pub const PLUGIN_NAME: &str = "rsi-skills";
/// Skill directory (under the repository root) the Claude plugin is built from.
pub const CLAUDE_SKILLS_DIR: &str = ".claude/skills";
/// Skill directory (under the repository root) Codex discovers from the cwd.
pub const CODEX_SKILLS_DIR: &str = ".agents/skills";
/// The file that makes a directory a skill.
pub const SKILL_FILE: &str = "SKILL.md";
/// Most skills one delivery carries; a larger directory is refused whole, so a
/// pathological checkout never fills the plugin store or the argv.
pub const MAX_SKILLS: usize = 256;
/// The Claude arguments explicit delivery adds so no ambient skill loads:
/// an empty settings-source list drops the user (`~/.claude/skills`), project
/// and local sources, including their `enabledPlugins` (probed on Claude Code
/// 2.1.302: the init `skills` list then holds only the CLI's own bundled
/// skills and the `--plugin-dir` plugin). Passed after any isolation
/// `--setting-sources` so it wins.
pub const CLAUDE_AMBIENT_SUPPRESS_ARGS: [&str; 2] = ["--setting-sources", ""];
/// The Codex override turning off the skills the CLI bundles into its home.
pub const CODEX_BUNDLED_SKILLS_OFF: &str = "skills.bundled.enabled=false";
/// Most directories one Codex skill scan visits before it gives up.
pub const MAX_SCAN_DIRS: usize = 20_000;
/// Most skills one Codex override may disable; more is refused.
pub const MAX_DISABLED_SKILLS: usize = 512;
/// Longest `skills.config` override (bytes) one launch argument may carry.
pub const MAX_OVERRIDE_BYTES: usize = 96 * 1024;

/// Why explicit delivery cannot be made deterministic for a launch. The text of
/// a launch refusal begins with [`Self::code`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDeliveryRefusal {
    /// The skill scan hit its directory or skill bound, so a skill may stay
    /// enabled that the override does not name.
    ScanIncomplete,
    /// The override would not fit one launch argument.
    OverrideTooLarge,
}

impl SkillDeliveryRefusal {
    /// The stable machine-readable reason.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ScanIncomplete => "skill_delivery_scan_incomplete",
            Self::OverrideTooLarge => "skill_delivery_override_too_large",
        }
    }
}

/// How a launch gets the repository's skills.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SkillDelivery {
    /// The CLI discovers skills itself (default; argv unchanged).
    #[default]
    Ambient,
    /// The launch names its skills (`--plugin-dir`, `-c skills.config`).
    Explicit,
}

impl SkillDelivery {
    /// The canonical setting text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ambient => "ambient",
            Self::Explicit => "explicit",
        }
    }

    /// Parse the setting text (case-insensitive, trimmed).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "ambient" => Some(Self::Ambient),
            "explicit" => Some(Self::Explicit),
            _ => None,
        }
    }

    /// Parse the setting from its RPC value, refusing anything but a string
    /// naming a mode.
    pub fn parse_value(value: &Value) -> Result<Self, String> {
        let text = value.as_str().ok_or("expected string")?;
        Self::parse(text).ok_or_else(|| "expected one of: ambient, explicit".to_string())
    }

    /// Whether launches deliver their skills explicitly.
    #[must_use]
    pub const fn is_explicit(self) -> bool {
        matches!(self, Self::Explicit)
    }
}

/// Every value the TUI row cycles through, the default first.
#[must_use]
pub fn tui_options() -> Vec<String> {
    vec![
        SkillDelivery::Ambient.as_str().to_string(),
        SkillDelivery::Explicit.as_str().to_string(),
    ]
}

/// One skill: a directory holding a [`SKILL_FILE`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    /// The directory name.
    pub name: String,
    /// The skill directory.
    pub dir: PathBuf,
}

impl SkillEntry {
    /// Path of the skill's `SKILL.md`.
    #[must_use]
    pub fn skill_file(&self) -> PathBuf {
        self.dir.join(SKILL_FILE)
    }
}

/// Whether `name` is a skill directory name this delivery will carry: ASCII
/// letters, digits, `-`, `_` and `.`, not starting with `.`, at most 64 chars.
#[must_use]
pub fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// The skills directly under `root`: each child directory (not a symlink)
/// holding a regular `SKILL.md`, sorted by name. A missing or unreadable `root`
/// is no skills; more than [`MAX_SKILLS`] is none (refused whole).
#[must_use]
pub fn discover_skills(root: &Path) -> Vec<SkillEntry> {
    if !is_real_dir(root) {
        return Vec::new();
    }
    let Ok(read) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut skills: Vec<SkillEntry> = read
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            if !valid_skill_name(&name) {
                return None;
            }
            let file_type = entry.file_type().ok()?;
            if !file_type.is_dir() {
                return None;
            }
            let dir = entry.path();
            let skill_file = std::fs::symlink_metadata(dir.join(SKILL_FILE)).ok()?;
            skill_file.is_file().then_some(SkillEntry { name, dir })
        })
        .collect();
    if skills.len() > MAX_SKILLS {
        return Vec::new();
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_dir())
}

/// `<repo_root>/<rel>` when every component of `rel` below `repo_root` is a
/// real directory, never a symlink (#2925): a symlinked `.claude` or
/// `.claude/skills` would otherwise let a checkout deliver files from outside
/// the repository. `None` when any component is missing or a symlink.
#[must_use]
pub fn contained_dir(repo_root: &Path, rel: &str) -> Option<PathBuf> {
    let mut path = repo_root.to_path_buf();
    for part in rel.split('/').filter(|part| !part.is_empty()) {
        path.push(part);
        if !is_real_dir(&path) {
            return None;
        }
    }
    Some(path)
}

/// [`discover_skills`] for `<repo_root>/<rel>` through [`contained_dir`]: no
/// skills when the skills root or any ancestor below the repository is a symlink.
#[must_use]
pub fn discover_repo_skills(repo_root: &Path, rel: &str) -> Vec<SkillEntry> {
    contained_dir(repo_root, rel)
        .map(|root| discover_skills(&root))
        .unwrap_or_default()
}

/// The result of scanning skill roots the way Codex reads them (#2924): every
/// directory is searched recursively, symlinks are followed (a symlinked
/// directory is searched and a symlinked `SKILL.md` counts) and each skill is
/// recorded by its canonical `SKILL.md` path, which is how Codex keys
/// `skills.config`.
#[derive(Debug, Default)]
pub struct SkillScan {
    files: std::collections::BTreeSet<PathBuf>,
    seen: std::collections::HashSet<PathBuf>,
    dirs: usize,
    exceeded: bool,
}

impl SkillScan {
    /// An empty scan.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Search `root` (a missing root holds nothing).
    pub fn scan_root(&mut self, root: &Path) {
        let Ok(start) = std::fs::canonicalize(root) else {
            return;
        };
        let mut stack = vec![start];
        while let Some(dir) = stack.pop() {
            if !self.seen.insert(dir.clone()) {
                continue;
            }
            self.dirs += 1;
            if self.dirs > MAX_SCAN_DIRS {
                self.exceeded = true;
                return;
            }
            // An unreadable directory is unreadable to Codex too.
            let Ok(read) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in read.filter_map(Result::ok) {
                let path = entry.path();
                let Ok(meta) = std::fs::metadata(&path) else {
                    continue; // dangling symlink
                };
                let Ok(canonical) = std::fs::canonicalize(&path) else {
                    continue;
                };
                if meta.is_dir() {
                    stack.push(canonical);
                } else if meta.is_file() && entry.file_name() == SKILL_FILE {
                    self.files.insert(canonical);
                    if self.files.len() > MAX_DISABLED_SKILLS {
                        self.exceeded = true;
                        return;
                    }
                }
            }
        }
    }

    /// Whether the scan saw everything (no bound was hit).
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.exceeded
    }

    /// The canonical `SKILL.md` path of every skill found, sorted.
    #[must_use]
    pub fn into_files(self) -> Vec<PathBuf> {
        self.files.into_iter().collect()
    }
}

/// The `.claude-plugin/plugin.json` of the delivered plugin. `digest` is the
/// content digest of the skills, so a changed skill set is a new plugin version.
#[must_use]
pub fn claude_plugin_manifest(digest: &str) -> String {
    let mut text = serde_json::to_string_pretty(&json!({
        "name": PLUGIN_NAME,
        "version": format!("0.0.0+{digest}"),
        "description": "The repository's agent skills, delivered by rsid (provider_skill_delivery=explicit).",
    }))
    .unwrap_or_default();
    text.push('\n');
    text
}

fn toml_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
}

/// The Codex `-c` override for explicit delivery: `skills.config=[...]` with
/// one entry per skill, sorted by path, `enabled = true` for each of `enable`
/// (the repository's skills) and `enabled = false` for each `SKILL.md` path in
/// `disable` (every other skill a [`SkillScan`] found). `None` when both are
/// empty, so a repository with no skills passes nothing.
#[must_use]
pub fn codex_skills_config_override(enable: &[SkillEntry], disable: &[PathBuf]) -> Option<String> {
    let path_of = |path: &Path| path.to_string_lossy().into_owned();
    let enabled: std::collections::BTreeSet<String> = enable
        .iter()
        .map(|skill| path_of(&skill.skill_file()))
        .collect();
    // A skill both enabled and disabled (the repository is the home) stays on.
    let mut entries: Vec<(String, bool)> = enabled
        .iter()
        .map(|path| (path.clone(), true))
        .chain(
            disable
                .iter()
                .map(|path| path_of(path))
                .filter(|path| !enabled.contains(path))
                .map(|path| (path, false)),
        )
        .collect();
    if entries.is_empty() {
        return None;
    }
    entries.sort();
    entries.dedup();
    let rendered: Vec<String> = entries
        .iter()
        .map(|(path, enabled)| format!("{{path={},enabled={enabled}}}", toml_string(path.as_str())))
        .collect();
    Some(format!("skills.config=[{}]", rendered.join(",")))
}

/// What a Claude `system/init` event says about the delivered plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PluginInitVerdict {
    /// `plugins` lists [`PLUGIN_NAME`] and `plugin_errors` is empty.
    Delivered,
    /// `plugins` does not list the plugin (and nothing says it failed).
    #[default]
    Missing,
    /// `plugin_errors` is not empty; the plugin names it carries.
    Errors(Vec<String>),
}

fn name_of(item: &Value) -> Option<String> {
    match item {
        Value::String(text) => Some(text.split(':').next().unwrap_or(text).trim().to_string()),
        Value::Object(map) => ["name", "plugin", "plugin_name", "id"]
            .iter()
            .find_map(|key| map.get(*key).and_then(Value::as_str))
            .map(str::to_string),
        _ => None,
    }
}

/// Judge a `system/init` event's `plugins` and `plugin_errors` for the plugin an
/// explicit Claude launch delivered. Only plugin names are read, never error
/// text.
#[must_use]
pub fn plugin_init_verdict(init: &Value) -> PluginInitVerdict {
    let names = |key: &str| -> Vec<String> {
        init.get(key)
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(name_of).collect())
            .unwrap_or_default()
    };
    let has_errors = init
        .get("plugin_errors")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty());
    if has_errors {
        return PluginInitVerdict::Errors(names("plugin_errors"));
    }
    if names("plugins").iter().any(|name| name == PLUGIN_NAME) {
        PluginInitVerdict::Delivered
    } else {
        PluginInitVerdict::Missing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(root: &Path, name: &str) -> SkillEntry {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SKILL_FILE), format!("---\nname: {name}\n---\n")).unwrap();
        SkillEntry {
            name: name.to_string(),
            dir,
        }
    }

    #[test]
    fn setting_parses_both_modes_and_refuses_the_rest() {
        assert_eq!(SkillDelivery::default(), SkillDelivery::Ambient);
        assert_eq!(
            SkillDelivery::parse_value(&json!(" Explicit ")),
            Ok(SkillDelivery::Explicit)
        );
        assert_eq!(
            SkillDelivery::parse_value(&json!("ambient")),
            Ok(SkillDelivery::Ambient)
        );
        for bad in [json!("bare"), json!(true), Value::Null, json!(["explicit"])] {
            assert!(SkillDelivery::parse_value(&bad).is_err(), "{bad}");
        }
        assert_eq!(tui_options(), ["ambient", "explicit"]);
        assert!(SkillDelivery::Explicit.is_explicit());
        assert!(!SkillDelivery::Ambient.is_explicit());
    }

    #[test]
    fn discovery_lists_real_skill_dirs_sorted_and_skips_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        skill(tmp.path(), "zeta");
        skill(tmp.path(), "alpha");
        skill(tmp.path(), ".hidden");
        std::fs::create_dir_all(tmp.path().join("no-skill-file")).unwrap();
        std::fs::write(tmp.path().join("loose.md"), "x").unwrap();
        skill(tmp.path(), "bad name");
        let names: Vec<String> = discover_skills(tmp.path())
            .into_iter()
            .map(|skill| skill.name)
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);
        assert!(discover_skills(&tmp.path().join("missing")).is_empty());
    }

    #[test]
    fn codex_override_is_sorted_toml_with_both_states() {
        let tmp = tempfile::tempdir().unwrap();
        let enable = [skill(tmp.path(), "b"), skill(tmp.path(), "a")];
        let off = tempfile::tempdir().unwrap();
        let disable = [skill(off.path(), "home").skill_file()];
        let text = codex_skills_config_override(&enable, &disable).unwrap();
        let (a, b, home) = (
            enable[1].skill_file(),
            enable[0].skill_file(),
            disable[0].clone(),
        );
        let entry = |path: &Path, on: bool| {
            let quoted = serde_json::to_string(path.to_str().unwrap()).unwrap();
            format!("{{path={quoted},enabled={on}}}")
        };
        let mut expected = vec![(a, true), (b, true), (home, false)];
        expected.sort();
        let rendered: Vec<String> = expected.iter().map(|(path, on)| entry(path, *on)).collect();
        assert_eq!(text, format!("skills.config=[{}]", rendered.join(",")));
        assert_eq!(codex_skills_config_override(&[], &[]), None);
    }

    #[test]
    fn init_verdict_reads_plugins_and_plugin_errors() {
        let ok = json!({"plugins":[{"name":"rsi-skills","path":"/p"}],"plugin_errors":[]});
        assert_eq!(plugin_init_verdict(&ok), PluginInitVerdict::Delivered);
        let strings = json!({"plugins":["rsi-skills"]});
        assert_eq!(plugin_init_verdict(&strings), PluginInitVerdict::Delivered);
        let missing = json!({"plugins":[{"name":"other"}]});
        assert_eq!(plugin_init_verdict(&missing), PluginInitVerdict::Missing);
        assert_eq!(plugin_init_verdict(&json!({})), PluginInitVerdict::Missing);
        let failed = json!({
            "plugins":[{"name":"rsi-skills"}],
            "plugin_errors":[{"plugin":"rsi-skills","message":"/home/u/secret boom"}]
        });
        assert_eq!(
            plugin_init_verdict(&failed),
            PluginInitVerdict::Errors(vec!["rsi-skills".to_string()])
        );
    }

    #[test]
    fn manifest_names_the_plugin_and_carries_the_digest() {
        let manifest: Value = serde_json::from_str(&claude_plugin_manifest("abc123")).unwrap();
        assert_eq!(manifest["name"], PLUGIN_NAME);
        assert_eq!(manifest["version"], "0.0.0+abc123");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_skills_root_or_ancestor_discovers_nothing() {
        use std::os::unix::fs::symlink;
        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        skill(&outside.path().join("skills"), "leak");
        // `.claude` itself is a symlink out of the repository.
        symlink(outside.path(), repo.path().join(".claude")).unwrap();
        assert_eq!(contained_dir(repo.path(), CLAUDE_SKILLS_DIR), None);
        assert!(discover_repo_skills(repo.path(), CLAUDE_SKILLS_DIR).is_empty());
        // `.agents` is real but `.agents/skills` is a symlink.
        std::fs::create_dir_all(repo.path().join(".agents")).unwrap();
        symlink(
            outside.path().join("skills"),
            repo.path().join(CODEX_SKILLS_DIR),
        )
        .unwrap();
        assert!(discover_repo_skills(repo.path(), CODEX_SKILLS_DIR).is_empty());
        assert!(discover_skills(&repo.path().join(CODEX_SKILLS_DIR)).is_empty());
        // A real tree is found.
        let real = tempfile::tempdir().unwrap();
        skill(&real.path().join(CLAUDE_SKILLS_DIR), "ok");
        let found = discover_repo_skills(real.path(), CLAUDE_SKILLS_DIR);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "ok");
    }

    #[cfg(unix)]
    #[test]
    fn scan_is_recursive_follows_symlinks_and_survives_cycles() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        skill(&root.path().join("skills/group"), "deep");
        skill(&root.path().join("skills"), ".system");
        skill(outside.path(), "ext");
        symlink(
            outside.path().join("ext"),
            root.path().join("skills/linked"),
        )
        .unwrap();
        // A cycle back to the root and a dangling link.
        symlink(
            root.path().join("skills"),
            root.path().join("skills/group/loop"),
        )
        .unwrap();
        symlink(
            root.path().join("nowhere"),
            root.path().join("skills/dangling"),
        )
        .unwrap();
        let mut scan = SkillScan::new();
        scan.scan_root(&root.path().join("skills"));
        scan.scan_root(&root.path().join("missing"));
        assert!(scan.is_complete());
        let canon = |path: PathBuf| std::fs::canonicalize(path).unwrap();
        assert_eq!(scan.into_files(), {
            let mut want = vec![
                canon(root.path().join("skills/group/deep").join(SKILL_FILE)),
                canon(root.path().join("skills/.system").join(SKILL_FILE)),
                canon(outside.path().join("ext").join(SKILL_FILE)),
            ];
            want.sort();
            want
        });
    }

    #[test]
    fn scan_past_the_skill_bound_is_incomplete() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..=MAX_DISABLED_SKILLS {
            skill(root.path(), &format!("s{index}"));
        }
        let mut scan = SkillScan::new();
        scan.scan_root(root.path());
        assert!(!scan.is_complete());
        assert_eq!(
            SkillDeliveryRefusal::ScanIncomplete.code(),
            "skill_delivery_scan_incomplete"
        );
    }

    #[test]
    fn suppress_args_pass_an_empty_setting_source_list() {
        assert_eq!(CLAUDE_AMBIENT_SUPPRESS_ARGS, ["--setting-sources", ""]);
    }
}
