//! #2767 (#2748 G16): the daemon half of explicit skills delivery.
//!
//! With the operator setting `provider_skill_delivery = explicit`
//! (`rsi_common::provider_skill_delivery`), a Claude launch passes one
//! `--plugin-dir` and a Codex launch one `-c skills.config=[...]`. This module
//! resolves the repository a launch belongs to, builds the Claude plugin on disk
//! and derives the Codex override. Every failure is a skipped delivery (the
//! launch proceeds exactly as `ambient` would) and a warning, never a launch
//! error.
//!
//! The Claude plugin is content-addressed: `<data dir>/skill-delivery/<digest>`
//! is the plugin root (`.claude-plugin/plugin.json` plus `skills/<name>/...`),
//! so two sandboxes at the same commit (or any two checkouts with the same
//! skills) deliver the same directory and the same skill list, whatever their
//! cwd and home hold.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use rsi_common::provider_skill_delivery::{
    CLAUDE_SKILLS_DIR, CODEX_SKILLS_DIR, MAX_OVERRIDE_BYTES, SkillDeliveryRefusal, SkillEntry,
    SkillScan, claude_plugin_manifest, codex_skills_config_override, discover_repo_skills,
};
use sha2::{Digest, Sha256};

/// Most files one delivered plugin may hold.
const MAX_FILES: usize = 4096;
/// Most bytes one delivered plugin may hold.
const MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Deepest directory nesting inside one skill.
const MAX_DEPTH: usize = 8;
/// Most parent directories searched for the repository root.
const MAX_ROOT_DEPTH: usize = 64;
/// The manifest's place inside a plugin.
const MANIFEST_REL: &str = ".claude-plugin/plugin.json";

/// The directory holding the delivered Claude plugins.
pub(crate) fn plugin_store_dir() -> PathBuf {
    // A unit test never writes into the operator's home.
    #[cfg(test)]
    return std::env::temp_dir().join(format!("rsid-skill-delivery-test-{}", std::process::id()));
    #[cfg(not(test))]
    rsi_common::identity::data_path("skill-delivery", "skill-delivery")
}

/// The repository a launch in `working_dir` belongs to: the nearest ancestor
/// (or the directory itself) holding a `.git` entry, which is a file in a linked
/// worktree. `None` when there is none.
pub(crate) fn repo_root(working_dir: &Path) -> Option<PathBuf> {
    working_dir
        .ancestors()
        .take(MAX_ROOT_DEPTH)
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

struct PluginFile {
    /// Path inside the plugin, `/`-separated.
    rel: String,
    /// The bytes read once at collection: the digest, the written copy and the
    /// cache check all use these, so a source changed mid-build is not mixed in.
    bytes: Vec<u8>,
    executable: bool,
}

/// Read a regular file without following a symlink: the path must be a regular
/// file before the open and the opened file must be that same file (#2925).
fn read_regular(path: &Path, limit: u64) -> Result<(Vec<u8>, bool), String> {
    let before = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !before.file_type().is_file() {
        return Err("not a regular file".to_string());
    }
    if before.len() > limit {
        return Err("skills exceed the delivery size bounds".to_string());
    }
    let file = fs::File::open(path).map_err(|error| error.to_string())?;
    let after = file.metadata().map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Err("file changed while it was read".to_string());
        }
    }
    if !after.is_file() {
        return Err("not a regular file".to_string());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("skills exceed the delivery size bounds".to_string());
    }
    Ok((bytes, is_executable(&after)))
}

/// Every regular file under each skill, in a fixed order. Symlinks are never
/// followed or copied. `Err` when a bound is exceeded.
fn collect_files(skills: &[SkillEntry]) -> Result<Vec<PluginFile>, String> {
    let mut files = Vec::new();
    let mut total = 0u64;
    for skill in skills {
        for entry in walkdir::WalkDir::new(&skill.dir)
            .follow_links(false)
            .max_depth(MAX_DEPTH)
            .sort_by_file_name()
        {
            let entry = entry.map_err(|error| error.to_string())?;
            if !entry.file_type().is_file() {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&skill.dir)
                .map_err(|error| error.to_string())?;
            let mut rel = format!("skills/{}", skill.name);
            for part in relative.components() {
                let part = part.as_os_str().to_str().ok_or("non-UTF-8 skill path")?;
                rel.push('/');
                rel.push_str(part);
            }
            if files.len() >= MAX_FILES {
                return Err("skills exceed the delivery size bounds".to_string());
            }
            let (bytes, executable) = read_regular(entry.path(), MAX_BYTES - total.min(MAX_BYTES))?;
            total += bytes.len() as u64;
            files.push(PluginFile {
                rel,
                bytes,
                executable,
            });
        }
    }
    Ok(files)
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// The content digest (hex, 16 chars) of a skill file set.
fn digest(files: &[PluginFile]) -> String {
    let mut hasher = Sha256::new();
    for file in files {
        hasher.update(file.rel.as_bytes());
        hasher.update([0, u8::from(file.executable)]);
        hasher.update((file.bytes.len() as u64).to_le_bytes());
        hasher.update(&file.bytes);
    }
    hex::encode(hasher.finalize())[..16].to_string()
}

/// Whether `target` is exactly the plugin `files` and `digest` describe (#2926):
/// a real directory (not a symlink) holding the manifest and every skill file
/// with the same bytes and execute bit and nothing else, so a modified file, an
/// added `hooks/hooks.json` or `.mcp.json`, a symlink or a swapped directory is
/// never reused.
fn plugin_matches(target: &Path, files: &[PluginFile], digest: &str) -> bool {
    if !fs::symlink_metadata(target).is_ok_and(|meta| meta.file_type().is_dir()) {
        return false;
    }
    let manifest = claude_plugin_manifest(digest).into_bytes();
    let mut expected: BTreeMap<&str, (&[u8], bool)> = BTreeMap::new();
    expected.insert(MANIFEST_REL, (manifest.as_slice(), false));
    for file in files {
        expected.insert(file.rel.as_str(), (file.bytes.as_slice(), file.executable));
    }
    let mut expected_dirs: BTreeSet<&str> = BTreeSet::new();
    for rel in expected.keys() {
        let mut end = 0;
        while let Some(slash) = rel[end..].find('/') {
            end += slash;
            expected_dirs.insert(&rel[..end]);
            end += 1;
        }
    }
    let mut seen = 0usize;
    for entry in walkdir::WalkDir::new(target)
        .follow_links(false)
        .max_depth(MAX_DEPTH + 4)
        .min_depth(1)
    {
        let Ok(entry) = entry else {
            return false;
        };
        let Ok(relative) = entry.path().strip_prefix(target) else {
            return false;
        };
        let Some(rel) = relative
            .components()
            .map(|part| part.as_os_str().to_str())
            .collect::<Option<Vec<_>>>()
            .map(|parts| parts.join("/"))
        else {
            return false;
        };
        let file_type = entry.file_type();
        if file_type.is_dir() {
            if !expected_dirs.contains(rel.as_str()) {
                return false;
            }
        } else if file_type.is_file() {
            let Some((bytes, executable)) = expected.get(rel.as_str()) else {
                return false;
            };
            let Ok((found, found_executable)) = read_regular(entry.path(), MAX_BYTES) else {
                return false;
            };
            if found != *bytes || found_executable != *executable {
                return false;
            }
            seen += 1;
        } else {
            // A symlink or any other node is never part of a plugin.
            return false;
        }
    }
    seen == expected.len()
}

fn remove_any(path: &Path) {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => {
            let _ = fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = fs::remove_file(path);
        }
        Err(_) => {}
    }
}

/// Build (or reuse) the Claude plugin for the skills under
/// `<repo_root>/.claude/skills` below `store_dir` and return its root. `Ok(None)`
/// when the repository has no skills (a symlinked skills root or ancestor counts
/// as none, #2925). The directory is written aside and renamed into place, so a
/// reader never sees a half-written plugin. An existing directory of the same
/// digest is reused only when it matches the expected bytes exactly; otherwise
/// it is moved aside and rebuilt (#2926).
pub(crate) fn build_claude_plugin(
    store_dir: &Path,
    repo_root: &Path,
) -> Result<Option<PathBuf>, String> {
    let skills = discover_repo_skills(repo_root, CLAUDE_SKILLS_DIR);
    if skills.is_empty() {
        return Ok(None);
    }
    let files = collect_files(&skills)?;
    let digest = digest(&files);
    let target = store_dir.join(&digest);
    if plugin_matches(&target, &files, &digest) {
        return Ok(Some(target));
    }
    fs::create_dir_all(store_dir).map_err(|error| error.to_string())?;
    let unique = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4().simple());
    let staging = store_dir.join(format!(".staging-{unique}"));
    let written = write_plugin(&staging, &files, &digest);
    if let Err(error) = written {
        remove_any(&staging);
        return Err(error);
    }
    if fs::symlink_metadata(&target).is_ok() {
        // The entry failed the check: move it (a symlink moves as a link) out of
        // the way before the publish.
        let stale = store_dir.join(format!(".stale-{unique}"));
        if let Err(error) = fs::rename(&target, &stale) {
            remove_any(&staging);
            return Err(error.to_string());
        }
        remove_any(&stale);
    }
    match fs::rename(&staging, &target) {
        Ok(()) => Ok(Some(target)),
        Err(_) if plugin_matches(&target, &files, &digest) => {
            // A concurrent launch published the same content first.
            remove_any(&staging);
            Ok(Some(target))
        }
        Err(error) => {
            remove_any(&staging);
            Err(error.to_string())
        }
    }
}

fn write_plugin(root: &Path, files: &[PluginFile], digest: &str) -> Result<(), String> {
    let manifest_dir = root.join(".claude-plugin");
    fs::create_dir_all(&manifest_dir).map_err(|error| error.to_string())?;
    fs::write(
        manifest_dir.join("plugin.json"),
        claude_plugin_manifest(digest),
    )
    .map_err(|error| error.to_string())?;
    for file in files {
        let destination = root.join(&file.rel);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::write(&destination, &file.bytes).map_err(|error| error.to_string())?;
        if file.executable {
            set_executable(&destination).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// The plugin directory a Claude launch in `working_dir` passes as
/// `--plugin-dir`, or `None` (nothing to deliver, or delivery failed). Under
/// explicit delivery the launch still suppresses every ambient skill source
/// either way, so a failed build means no skills, never ambient ones.
pub(crate) fn claude_plugin_dir_for_launch(working_dir: Option<&Path>) -> Option<PathBuf> {
    claude_plugin_dir_in(&plugin_store_dir(), working_dir?)
}

pub(crate) fn claude_plugin_dir_in(store_dir: &Path, working_dir: &Path) -> Option<PathBuf> {
    let root = repo_root(working_dir)?;
    match build_claude_plugin(store_dir, &root) {
        Ok(dir) => dir,
        Err(error) => {
            tracing::warn!(
                repo = %root.display(),
                %error,
                "skill delivery: could not build the Claude plugin; launching with no repository skills"
            );
            None
        }
    }
}

/// Whether an explicit Claude launch in `working_dir` has skills to deliver, so
/// its init is expected to list the `rsi-skills` plugin. Cheap: no plugin is
/// built.
pub(crate) fn claude_skills_expected(working_dir: &Path) -> bool {
    repo_root(working_dir)
        .is_some_and(|root| !discover_repo_skills(&root, CLAUDE_SKILLS_DIR).is_empty())
}

/// The user and admin skill roots Codex reads outside any repository.
pub(crate) fn codex_ambient_roots() -> Vec<PathBuf> {
    // A unit test's argv does not depend on the machine's own skills.
    if cfg!(test) {
        return Vec::new();
    }
    let home = dirs::home_dir();
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".codex")));
    let mut roots: Vec<PathBuf> = codex_home
        .map(|dir| dir.join("skills"))
        .into_iter()
        .chain(home.map(|home| home.join(".agents").join("skills")))
        .collect();
    #[cfg(unix)]
    roots.push(PathBuf::from("/etc/codex/skills"));
    roots
}

/// The project skill roots Codex reads for a launch in `working_dir` (probed on
/// codex-cli 0.163.0): `.agents/skills` and `.codex/skills` of every directory
/// from the working directory up to the repository root, so a nested one is
/// read too. Without a repository only the working directory counts.
fn codex_project_roots(working_dir: &Path, repo: Option<&Path>) -> Vec<PathBuf> {
    let dirs: Vec<&Path> = match repo {
        Some(repo) => working_dir
            .ancestors()
            .take(MAX_ROOT_DEPTH)
            .take_while(|dir| dir.starts_with(repo))
            .collect(),
        None => vec![working_dir],
    };
    dirs.into_iter()
        .flat_map(|dir| {
            [
                dir.join(CODEX_SKILLS_DIR),
                dir.join(".codex").join("skills"),
            ]
        })
        .collect()
}

/// The `-c skills.config=[...]` override a Codex launch in `working_dir` passes:
/// the repository's own `.agents/skills` (real directories only) enabled, every
/// other skill Codex would read disabled. That is every skill under a
/// recursive, symlink-following scan of the project roots (including nested
/// ones) and `ambient_roots`, keyed by canonical path (#2924). `Ok(None)` when
/// both sets are empty. `Err` when the scan cannot prove it saw every skill or
/// the override would not fit one argument: explicit delivery is then refused
/// rather than claimed.
pub(crate) fn codex_skills_override_in(
    working_dir: &Path,
    ambient_roots: &[PathBuf],
) -> Result<Option<String>, SkillDeliveryRefusal> {
    let repo = repo_root(working_dir);
    let enable: Vec<SkillEntry> = repo
        .as_deref()
        .map(|root| discover_repo_skills(root, CODEX_SKILLS_DIR))
        .unwrap_or_default()
        .into_iter()
        .map(|skill| SkillEntry {
            dir: fs::canonicalize(&skill.dir).unwrap_or(skill.dir),
            name: skill.name,
        })
        .collect();
    let mut scan = SkillScan::new();
    for root in codex_project_roots(working_dir, repo.as_deref())
        .iter()
        .chain(ambient_roots)
    {
        scan.scan_root(root);
    }
    if !scan.is_complete() {
        return Err(SkillDeliveryRefusal::ScanIncomplete);
    }
    let override_text = codex_skills_config_override(&enable, &scan.into_files());
    if override_text
        .as_ref()
        .is_some_and(|text| text.len() > MAX_OVERRIDE_BYTES)
    {
        return Err(SkillDeliveryRefusal::OverrideTooLarge);
    }
    Ok(override_text)
}

/// [`codex_skills_override_in`] against the real operator roots. `Ok(None)` with
/// no working directory.
pub(crate) fn codex_skills_override_for_launch(
    working_dir: Option<&Path>,
) -> Result<Option<String>, SkillDeliveryRefusal> {
    match working_dir {
        Some(dir) => codex_skills_override_in(dir, &codex_ambient_roots()),
        None => Ok(None),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rsi_common::provider_skill_delivery::{MAX_DISABLED_SKILLS, SKILL_FILE};

    /// A repository root (a `.git` marker) with the named skills under `rel`.
    #[cfg(any(
        not(feature = "test-shard-mode"),
        feature = "test-shard-other-03",
        feature = "test-shard-other-05"
    ))]
    pub(crate) fn repo_with_skills(rel: &str, names: &[&str]) -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        fs::write(repo.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
        for name in names {
            let dir = repo.path().join(rel).join(name);
            fs::create_dir_all(dir.join("scripts")).unwrap();
            fs::write(
                dir.join(SKILL_FILE),
                format!("---\nname: {name}\n---\nbody\n"),
            )
            .unwrap();
            fs::write(dir.join("scripts/run.sh"), "#!/bin/sh\n").unwrap();
        }
        repo
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn repo_root_is_the_nearest_git_ancestor() {
        let repo = repo_with_skills(CLAUDE_SKILLS_DIR, &["a"]);
        let nested = repo.path().join("crates/x");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(repo_root(&nested).as_deref(), Some(repo.path()));
        assert_eq!(repo_root(repo.path()).as_deref(), Some(repo.path()));
    }

    /// The plugin holds the manifest and every skill file, is reused for equal
    /// content (two sandboxes deliver one directory) and is a new directory
    /// when a skill changes.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn plugin_is_content_addressed_and_identical_across_checkouts() {
        let store = tempfile::tempdir().unwrap();
        let first = repo_with_skills(CLAUDE_SKILLS_DIR, &["alpha", "beta"]);
        let second = repo_with_skills(CLAUDE_SKILLS_DIR, &["beta", "alpha"]);
        let one = build_claude_plugin(store.path(), first.path())
            .unwrap()
            .expect("plugin");
        let two = build_claude_plugin(store.path(), second.path())
            .unwrap()
            .expect("plugin");
        assert_eq!(one, two, "same skills, same plugin directory");
        assert!(one.starts_with(store.path()));
        let manifest: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(one.join(".claude-plugin/plugin.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["name"], "rsi-skills");
        for name in ["alpha", "beta"] {
            assert!(one.join("skills").join(name).join(SKILL_FILE).is_file());
            assert!(
                one.join("skills")
                    .join(name)
                    .join("scripts/run.sh")
                    .is_file()
            );
        }
        fs::write(
            first
                .path()
                .join(CLAUDE_SKILLS_DIR)
                .join("alpha")
                .join(SKILL_FILE),
            "changed",
        )
        .unwrap();
        let changed = build_claude_plugin(store.path(), first.path())
            .unwrap()
            .expect("plugin");
        assert_ne!(changed, one);
        let leftovers: Vec<_> = fs::read_dir(store.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".staging"))
            .collect();
        assert!(leftovers.is_empty(), "no staging directory is left behind");
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn plugin_never_follows_symlinks_and_keeps_the_exec_bit() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let store = tempfile::tempdir().unwrap();
        let repo = repo_with_skills(CLAUDE_SKILLS_DIR, &["alpha"]);
        let skill = repo.path().join(CLAUDE_SKILLS_DIR).join("alpha");
        fs::set_permissions(
            skill.join("scripts/run.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "s").unwrap();
        symlink(outside.path().join("secret"), skill.join("leak")).unwrap();
        symlink(outside.path(), skill.join("leakdir")).unwrap();
        let plugin = build_claude_plugin(store.path(), repo.path())
            .unwrap()
            .expect("plugin");
        let copied = plugin.join("skills/alpha");
        assert!(!copied.join("leak").exists());
        assert!(!copied.join("leakdir").exists());
        let mode = fs::metadata(copied.join("scripts/run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "exec bit kept");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn no_skills_or_no_repository_deliver_nothing() {
        let store = tempfile::tempdir().unwrap();
        let empty = repo_with_skills(CLAUDE_SKILLS_DIR, &[]);
        assert_eq!(build_claude_plugin(store.path(), empty.path()), Ok(None));
        let bare = tempfile::tempdir().unwrap();
        // Job TMPDIR can itself live inside a repository (#2612). Keep the
        // whole bounded ancestor search inside our empty fixture; neither
        // an enclosing checkout nor its skills can decide this test.
        let working_dir =
            (0..MAX_ROOT_DEPTH).fold(bare.path().to_path_buf(), |path, _| path.join("d"));
        fs::create_dir_all(&working_dir).unwrap();
        assert_eq!(repo_root(&working_dir), None);
        assert_eq!(claude_plugin_dir_in(store.path(), &working_dir), None);
        assert_eq!(codex_skills_override_in(&working_dir, &[]), Ok(None));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn codex_override_enables_the_repo_and_disables_the_ambient_user_skills() {
        let repo = repo_with_skills(CODEX_SKILLS_DIR, &["plan", "debug"]);
        let home = tempfile::tempdir().unwrap();
        let ambient = home.path().join("skills");
        for name in ["mine", "theirs"] {
            fs::create_dir_all(ambient.join(name)).unwrap();
            fs::write(ambient.join(name).join(SKILL_FILE), "x").unwrap();
        }
        let nested = repo.path().join("src");
        fs::create_dir_all(&nested).unwrap();
        let text = codex_skills_override_in(&nested, std::slice::from_ref(&ambient))
            .unwrap()
            .unwrap();
        let table: toml::Table = toml::from_str(&text).expect("valid TOML override");
        let entries = table["skills"]["config"].as_array().unwrap();
        let state = |suffix: &str| {
            entries
                .iter()
                .find(|entry| entry["path"].as_str().unwrap().ends_with(suffix))
                .map(|entry| entry["enabled"].as_bool().unwrap())
        };
        assert_eq!(state("plan/SKILL.md"), Some(true));
        assert_eq!(state("debug/SKILL.md"), Some(true));
        assert_eq!(state("mine/SKILL.md"), Some(false));
        assert_eq!(state("theirs/SKILL.md"), Some(false));
        assert_eq!(entries.len(), 4);
        // Another checkout of the same skills, another home: same enabled set.
        let other = repo_with_skills(CODEX_SKILLS_DIR, &["debug", "plan"]);
        let other_text = codex_skills_override_in(other.path(), &[])
            .unwrap()
            .unwrap();
        let other_table: toml::Table = toml::from_str(&other_text).unwrap();
        let other_names: Vec<String> = other_table["skills"]["config"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                let path = std::path::Path::new(entry["path"].as_str().unwrap());
                path.parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(other_names, ["debug", "plan"]);
    }

    /// The plugin directory for `repo`, asserted to match the repo's skills byte
    /// for byte.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    fn built(store: &Path, repo: &Path) -> PathBuf {
        let plugin = build_claude_plugin(store, repo).unwrap().expect("plugin");
        let skills = discover_repo_skills(repo, CLAUDE_SKILLS_DIR);
        let files = collect_files(&skills).unwrap();
        assert!(plugin_matches(&plugin, &files, &digest(&files)));
        plugin
    }

    /// #2925: a symlinked `.claude` or `.claude/skills` is never read, so no
    /// file from outside the repository reaches the plugin store.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn symlinked_skills_ancestor_is_never_copied() {
        use std::os::unix::fs::symlink;
        let outside = repo_with_skills(CLAUDE_SKILLS_DIR, &["leak"]);
        for (link_at, target) in [
            (".claude", outside.path().join(".claude")),
            (".claude/skills", outside.path().join(CLAUDE_SKILLS_DIR)),
        ] {
            let store = tempfile::tempdir().unwrap();
            let repo = tempfile::tempdir().unwrap();
            fs::write(repo.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
            if link_at == ".claude/skills" {
                fs::create_dir_all(repo.path().join(".claude")).unwrap();
            }
            symlink(&target, repo.path().join(link_at)).unwrap();
            assert_eq!(
                build_claude_plugin(store.path(), repo.path()),
                Ok(None),
                "{link_at}"
            );
            assert!(!claude_skills_expected(repo.path()), "{link_at}");
            assert_eq!(
                fs::read_dir(store.path()).unwrap().count(),
                0,
                "nothing was copied for a symlinked {link_at}"
            );
        }
    }

    /// #2926: a cache entry is reused only when it is exactly the expected
    /// plugin; a tampered one is rebuilt, a symlinked one replaced.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn tampered_or_symlinked_cache_entry_is_rebuilt() {
        use std::os::unix::fs::symlink;
        let store = tempfile::tempdir().unwrap();
        let repo = repo_with_skills(CLAUDE_SKILLS_DIR, &["alpha"]);
        let plugin = built(store.path(), repo.path());
        let skill_file = plugin.join("skills/alpha").join(SKILL_FILE);
        let original = fs::read(&skill_file).unwrap();

        // Modified bytes.
        fs::write(&skill_file, "tampered").unwrap();
        assert_eq!(built(store.path(), repo.path()), plugin);
        assert_eq!(fs::read(&skill_file).unwrap(), original);

        // An added hooks file and an added MCP file.
        fs::create_dir_all(plugin.join("hooks")).unwrap();
        fs::write(plugin.join("hooks/hooks.json"), "{}").unwrap();
        fs::write(plugin.join(".mcp.json"), "{}").unwrap();
        assert_eq!(built(store.path(), repo.path()), plugin);
        assert!(!plugin.join("hooks").exists());
        assert!(!plugin.join(".mcp.json").exists());

        // A missing or symlinked manifest.
        fs::remove_file(plugin.join(".claude-plugin/plugin.json")).unwrap();
        assert_eq!(built(store.path(), repo.path()), plugin);
        let manifest = plugin.join(".claude-plugin/plugin.json");
        let good = fs::read(&manifest).unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("m.json"), &good).unwrap();
        fs::remove_file(&manifest).unwrap();
        symlink(outside.path().join("m.json"), &manifest).unwrap();
        built(store.path(), repo.path());
        assert!(
            !fs::symlink_metadata(&manifest)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        // A symlink in a skill.
        symlink(outside.path(), plugin.join("skills/alpha/extra")).unwrap();
        built(store.path(), repo.path());
        assert!(!plugin.join("skills/alpha/extra").exists());

        // The digest directory itself is a symlink to a tree with the same bytes.
        fs::rename(&plugin, outside.path().join("moved")).unwrap();
        symlink(outside.path().join("moved"), &plugin).unwrap();
        let rebuilt = built(store.path(), repo.path());
        assert_eq!(rebuilt, plugin);
        assert!(
            fs::symlink_metadata(&plugin).unwrap().file_type().is_dir(),
            "the symlinked digest directory became a real directory"
        );
        assert!(
            outside.path().join("moved").exists(),
            "the symlink target is left alone"
        );
        assert!(
            fs::read_dir(store.path())
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().starts_with('.')),
            "no staging or stale directory is left behind"
        );
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    fn override_states(text: &str) -> BTreeMap<String, bool> {
        let table: toml::Table = toml::from_str(text).expect("valid TOML override");
        table["skills"]["config"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["path"].as_str().unwrap().to_string(),
                    entry["enabled"].as_bool().unwrap(),
                )
            })
            .collect()
    }

    /// #2924: the loaded Codex set is exactly the repository's real
    /// `.agents/skills` directories. Nested skills, symlinked skills, the
    /// `.codex/skills` project root, a nested working directory's own roots and
    /// the user roots (searched recursively, through symlinks) are all disabled.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn codex_override_disables_every_other_skill_codex_would_read() {
        use std::os::unix::fs::symlink;
        let repo = repo_with_skills(CODEX_SKILLS_DIR, &["plan"]);
        let canon = |path: PathBuf| fs::canonicalize(path).unwrap();
        let skill_of = |dir: PathBuf| canon(dir).join(SKILL_FILE).to_string_lossy().into_owned();
        let outside = repo_with_skills("skills", &["ext"]);
        let agents = repo.path().join(CODEX_SKILLS_DIR);
        // A skill nested inside a delivered one and inside a group directory.
        let nested_in_skill = agents.join("plan/inner");
        fs::create_dir_all(&nested_in_skill).unwrap();
        fs::write(nested_in_skill.join(SKILL_FILE), "x").unwrap();
        let grouped = agents.join("group/deep");
        fs::create_dir_all(&grouped).unwrap();
        fs::write(grouped.join(SKILL_FILE), "x").unwrap();
        // A symlinked skill directory (not a delivered skill).
        symlink(outside.path().join("skills/ext"), agents.join("linked")).unwrap();
        // The project `.codex/skills` root and a nested working directory's roots.
        let codex_dir = repo.path().join(".codex/skills/legacy");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(codex_dir.join(SKILL_FILE), "x").unwrap();
        let cwd = repo.path().join("crates/x");
        let sub = cwd.join(CODEX_SKILLS_DIR).join("subskill");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join(SKILL_FILE), "x").unwrap();
        // A user root with a nested skill and a symlinked skill.
        let home = tempfile::tempdir().unwrap();
        let user = home.path().join("skills");
        let user_nested = user.join("grp/deep");
        fs::create_dir_all(&user_nested).unwrap();
        fs::write(user_nested.join(SKILL_FILE), "x").unwrap();
        fs::create_dir_all(user.join(".system/bundled")).unwrap();
        fs::write(user.join(".system/bundled").join(SKILL_FILE), "x").unwrap();
        symlink(outside.path().join("skills/ext"), user.join("link")).unwrap();

        let text = codex_skills_override_in(&cwd, std::slice::from_ref(&user))
            .unwrap()
            .unwrap();
        let states = override_states(&text);
        let expected: BTreeMap<String, bool> = [
            (skill_of(agents.join("plan")), true),
            (skill_of(nested_in_skill), false),
            (skill_of(grouped), false),
            (skill_of(outside.path().join("skills/ext")), false),
            (skill_of(codex_dir), false),
            (skill_of(sub), false),
            (skill_of(user_nested), false),
            (skill_of(user.join(".system/bundled")), false),
        ]
        .into_iter()
        .collect();
        assert_eq!(states, expected);
        let enabled: Vec<_> = states.iter().filter(|(_, on)| **on).collect();
        assert_eq!(enabled.len(), 1, "only the delivered skill stays enabled");
    }

    /// #2924/#2925: a symlinked `.agents/skills` is not a delivered root, so its
    /// skills are disabled rather than enabled.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn codex_symlinked_skills_root_is_disabled_not_enabled() {
        use std::os::unix::fs::symlink;
        let outside = repo_with_skills("skills", &["ext"]);
        let repo = tempfile::tempdir().unwrap();
        fs::write(repo.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
        fs::create_dir_all(repo.path().join(".agents")).unwrap();
        symlink(
            outside.path().join("skills"),
            repo.path().join(CODEX_SKILLS_DIR),
        )
        .unwrap();
        let text = codex_skills_override_in(repo.path(), &[]).unwrap().unwrap();
        let ext = fs::canonicalize(outside.path().join("skills/ext"))
            .unwrap()
            .join(SKILL_FILE)
            .to_string_lossy()
            .into_owned();
        assert_eq!(override_states(&text), BTreeMap::from([(ext, false)]));
    }

    /// A scan that cannot prove it saw every skill refuses explicit delivery
    /// with a typed reason instead of claiming a deterministic set.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn codex_override_refuses_when_the_scan_is_incomplete() {
        let repo = repo_with_skills(CODEX_SKILLS_DIR, &["plan"]);
        let user = tempfile::tempdir().unwrap();
        for index in 0..=MAX_DISABLED_SKILLS {
            let dir = user.path().join(format!("s{index}"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(SKILL_FILE), "x").unwrap();
        }
        assert_eq!(
            codex_skills_override_in(repo.path(), &[user.path().to_path_buf()]),
            Err(SkillDeliveryRefusal::ScanIncomplete)
        );
    }
}
