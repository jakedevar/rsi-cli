//! Input-bar assets served by `rsid` (hub #3044): pasted images and slash
//! commands, so the desktop stays a pure `rsid` frontend that works off-host.
//!
//! Two operator-only methods:
//!
//! - [`METHOD_UPLOAD_PASTE_IMAGE`]: the client sends the image bytes (base64
//!   in JSON, at most [`MAX_PASTE_IMAGE_BYTES`] decoded) with a MIME type from
//!   [`PASTE_IMAGE_MIME_ALLOWLIST`]; the daemon checks the type against the
//!   magic bytes ([`sniff_image_mime`]) and stores the image under a
//!   content-addressed name (`<sha256>.<ext>`) in
//!   `~/.rsi/paste/`[`PASTE_UPLOAD_SUBDIR`], returning the path it will
//!   reference.
//! - [`METHOD_LIST_SLASH_COMMANDS`]: for a project or session root, the
//!   `.claude/commands/*.md` and `.claude/skills/*/SKILL.md` entries the TUI
//!   offers ([`discover_slash_commands`], which the TUI calls too). Names and
//!   descriptions only; an entry whose canonical path leaves the root (a
//!   symlink escape) is skipped and counted.
//!
//! - [`METHOD_LIST_SKILLS`] and [`METHOD_GET_SKILL`] (hub #3062): the skills
//!   of a project or session root (`.claude/skills`) or of the daemon user
//!   (`~/.claude/skills`, the TUI's Settings > Skills), and one skill's
//!   `SKILL.md` ([`list_skills`], [`read_skill`], which the TUI calls too).
//!   Read-only; every candidate must canonicalize inside its root (a symlink
//!   out of it is refused or skipped), and a `SKILL.md` is read to at most
//!   [`MAX_SKILL_BYTES`].
//!
//! - [`METHOD_SET_SKILL_ENABLED`] and [`METHOD_DELETE_SKILL`] (hub #3062
//!   follow-up, local #97): the TUI's Settings > Skills `e` and `d` on the
//!   daemon user's `~/.claude/skills` ([`set_skill_enabled`],
//!   [`toggle_skill`], [`delete_skill`], which the TUI calls too). Enable and
//!   disable rename `<name>/` and `<name>.disabled/`; delete moves the
//!   directory into the recoverable trash `<data dir>/`[`SKILL_TRASH_DIR`]
//!   (`<UTC %Y%m%dT%H%M%S>-<8 hex>/<entry>` with a `<batch>.origin` sidecar
//!   naming where it came from, the [`crate::trash_prune`] batch layout;
//!   local #91), so nothing is unlinked. The entry must be a real directory
//!   directly in the skills directory (a symlink, file or other non-directory
//!   entry is refused) and all I/O goes through pinned handles that never
//!   follow a link.
//!
//! None of these methods is in the attributed verb registry (operator-only,
//! AGENTS.md rule 10) and none is on the rsi-remote gateway.

use crate::asset_fs::AssetDir;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const METHOD_UPLOAD_PASTE_IMAGE: &str = "UploadPasteImage";
pub const METHOD_LIST_SLASH_COMMANDS: &str = "ListSlashCommands";
pub const METHOD_LIST_SKILLS: &str = "ListSkills";
pub const METHOD_GET_SKILL: &str = "GetSkill";
pub const METHOD_SET_SKILL_ENABLED: &str = "SetSkillEnabled";
pub const METHOD_DELETE_SKILL: &str = "DeleteSkill";

/// Every input-asset method, for the operator-only fence test.
pub const INPUT_ASSET_METHODS: &[&str] = &[
    METHOD_UPLOAD_PASTE_IMAGE,
    METHOD_LIST_SLASH_COMMANDS,
    METHOD_LIST_SKILLS,
    METHOD_GET_SKILL,
    METHOD_SET_SKILL_ENABLED,
    METHOD_DELETE_SKILL,
];

/// Largest decoded image `UploadPasteImage` stores (the desktop's G1 limit).
pub const MAX_PASTE_IMAGE_BYTES: usize = 32 << 20;
/// Subdirectory of `~/.rsi/paste` that holds uploads. The TUI deletes nothing
/// at exit (local #155); `rsid::paste_retention` removes both these uploads
/// and the TUI's own top-level `<uuid>.png` pastes once they are older than
/// seven days and nothing ever named them (a named one only under the opt-in
/// setting `paste_retention_referenced_days`, #86).
pub const PASTE_UPLOAD_SUBDIR: &str = "rsid";

/// MIME types `UploadPasteImage` accepts, with the file extension stored.
pub const PASTE_IMAGE_MIME_ALLOWLIST: &[(&str, &str)] = &[
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/gif", "gif"),
    ("image/webp", "webp"),
];

/// Default and ceiling of `ListSlashCommands.max_entries`.
pub const DEFAULT_SLASH_COMMANDS: usize = 500;
pub const MAX_SLASH_COMMANDS: usize = 2_000;
/// Directory entries scanned per directory before a listing stops (`truncated`).
pub const SLASH_SCAN_LIMIT: usize = 10_000;
/// Bytes read from a command or skill file to find its frontmatter description.
pub const SLASH_DESCRIPTION_READ_BYTES: u64 = 16 << 10;
/// Longest description returned, in characters.
pub const MAX_SLASH_DESCRIPTION_CHARS: usize = 300;

/// The TUI's descriptions for an entry without a frontmatter `description`.
pub const COMMAND_FALLBACK_DESCRIPTION: &str = "Custom command from .claude/commands/";
pub const SKILL_FALLBACK_DESCRIPTION: &str = "Skill from .claude/skills/";

/// Largest part of a `SKILL.md` `GetSkill` returns; a longer file is cut at
/// a character boundary and marked `truncated`.
pub const MAX_SKILL_BYTES: usize = 256 << 10;
/// Default and ceiling of `ListSkills.max_entries`.
pub const DEFAULT_SKILLS: usize = 500;
pub const MAX_SKILLS: usize = 2_000;
/// Longest skill name `GetSkill` accepts, in bytes.
pub const MAX_SKILL_NAME_BYTES: usize = 255;
/// The TUI disables a skill by renaming `<name>/` to `<name>.disabled/`.
pub const DISABLED_SKILL_SUFFIX: &str = ".disabled";
/// Where `DeleteSkill` keeps a deleted skill, below the rsi data directory.
pub const SKILL_TRASH_DIR: [&str; 2] = ["trash", "skills"];

/// The skill trash below `data_dir` (`<data dir>/trash/skills`).
pub fn skill_trash_root(data_dir: &Path) -> PathBuf {
    SKILL_TRASH_DIR
        .iter()
        .fold(data_dir.to_path_buf(), |path, part| path.join(part))
}

/// `UploadPasteImage` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadPasteImageRequestV1 {
    /// One of [`PASTE_IMAGE_MIME_ALLOWLIST`] (case-insensitive).
    pub mime: String,
    /// The image bytes, standard base64 with padding.
    pub data_base64: String,
}

/// `UploadPasteImage` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadPasteImageResponseV1 {
    /// Absolute path of the stored image: the `@path` a message references.
    pub path: String,
    /// `<sha256>.<ext>`.
    pub name: String,
    /// The verified MIME type (lowercase).
    pub mime: String,
    /// Decoded size in bytes.
    pub bytes: u64,
    /// Lowercase hex SHA-256 of the bytes.
    pub sha256: String,
    /// True when an identical image was already stored (nothing written).
    pub reused: bool,
}

/// `ListSlashCommands` request: exactly one of `project_id` (the project's
/// `path`) and `session_id` (the session's `working_dir`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListSlashCommandsRequestV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entries: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlashCommandSourceV1 {
    /// `.claude/commands/<name>.md`
    Command,
    /// `.claude/skills/<name>/SKILL.md`
    Skill,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlashCommandV1 {
    /// The command name without the leading `/`.
    pub name: String,
    /// The frontmatter `description`, else the TUI's generic text.
    pub description: String,
    pub source: SlashCommandSourceV1,
}

/// `ListSlashCommands` response, sorted by name (then source).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSlashCommandsResponseV1 {
    /// The canonical root scanned.
    pub root: String,
    pub commands: Vec<SlashCommandV1>,
    /// True when `max_entries` or the scan limit cut the listing.
    pub truncated: bool,
    /// Entries skipped because their canonical path leaves the root.
    pub skipped_outside_root: u32,
}

/// `ListSkills` request: exactly one of `project_id` (the project's `path`),
/// `session_id` (the session's `working_dir`) and `user: true` (the daemon
/// user's `~/.claude/skills`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListSkillsRequestV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub user: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entries: Option<usize>,
}

/// One skill directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSummaryV1 {
    /// The directory name without the [`DISABLED_SKILL_SUFFIX`].
    pub name: String,
    /// False when the directory ends in [`DISABLED_SKILL_SUFFIX`].
    pub enabled: bool,
    /// The `SKILL.md` frontmatter `description`, on one line and capped.
    pub description: Option<String>,
    /// True when the directory holds a `SKILL.md` (inside the root).
    pub has_skill_md: bool,
}

/// `ListSkills` response, sorted by name (enabled first).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSkillsResponseV1 {
    /// The skills directory listed (canonical when it exists).
    pub skills_dir: String,
    pub skills: Vec<SkillSummaryV1>,
    /// True when `max_entries` or the scan limit cut the listing.
    pub truncated: bool,
    /// Entries skipped because their canonical path leaves the root.
    pub skipped_outside_root: u32,
}

/// `GetSkill` request: one scope as in [`ListSkillsRequestV1`] plus the
/// skill `name` (a single path component; `<name>.disabled` is read when
/// `<name>` has no `SKILL.md`, as the TUI does).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetSkillRequestV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub user: bool,
    pub name: String,
}

/// `GetSkill` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetSkillResponseV1 {
    pub name: String,
    pub enabled: bool,
    /// Canonical path of the `SKILL.md` read.
    pub path: String,
    /// The text (invalid UTF-8 replaced), at most [`MAX_SKILL_BYTES`].
    pub content: String,
    /// Size of the whole file in bytes.
    pub bytes: u64,
    /// True when the file is longer than [`MAX_SKILL_BYTES`].
    pub truncated: bool,
}

/// `SetSkillEnabled` request: the daemon user's skill `name` (one path
/// component, without the [`DISABLED_SKILL_SUFFIX`]) and the state wanted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSkillEnabledRequestV1 {
    pub name: String,
    pub enabled: bool,
}

/// `SetSkillEnabled` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSkillEnabledResponseV1 {
    pub name: String,
    pub enabled: bool,
    /// False when the skill was already in the requested state.
    pub changed: bool,
    /// The skill directory now (`<name>` or `<name>.disabled`).
    pub path: String,
}

/// `DeleteSkill` request: the daemon user's skill `name`; `<name>/` is
/// moved to the skill trash, else `<name>.disabled/`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteSkillRequestV1 {
    pub name: String,
}

/// `DeleteSkill` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteSkillResponseV1 {
    pub name: String,
    /// Whether the deleted directory was the enabled one.
    pub enabled: bool,
    /// The directory removed from the skills directory.
    pub path: String,
    /// Where the directory now is: `<trash>/<batch>/<entry>`.
    pub trash_path: String,
}

/// Where skills are read from. The containment root of a project or session
/// is its directory (its `.claude/skills` must stay inside it); the user
/// tier's root is the canonical `~/.claude/skills` itself, so a linked
/// `~/.claude` (a dotfiles checkout) still works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillScope {
    /// A project `path` or session `working_dir`.
    Root(PathBuf),
    /// A home directory: `<home>/.claude/skills`.
    User { home: PathBuf },
}

/// A refused skill read; [`SkillError::code`] is the wire refusal code.
#[derive(Debug)]
pub enum SkillError {
    InvalidName(String),
    NotFound(String),
    OutsideRoot(String),
    /// The skill entry is not a real directory (a symlink, file, FIFO...).
    NotDirectory(String),
    /// Both `<name>` and `<name>.disabled` exist, so a rename would clobber one.
    Conflict(String),
    Io(std::io::Error),
    /// An enable, disable or delete failed part-way or at its system call.
    WriteFailed(std::io::Error),
}

impl SkillError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidName(_) => "skill_invalid_name",
            Self::NotFound(_) => "skill_not_found",
            Self::OutsideRoot(_) => "skill_outside_root",
            Self::NotDirectory(_) => "skill_not_directory",
            Self::Conflict(_) => "skill_conflict",
            Self::Io(_) => "skill_read_failed",
            Self::WriteFailed(_) => "skill_write_failed",
        }
    }
}

impl std::fmt::Display for SkillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(d)
            | Self::NotFound(d)
            | Self::OutsideRoot(d)
            | Self::NotDirectory(d)
            | Self::Conflict(d) => f.write_str(d),
            Self::Io(e) | Self::WriteFailed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SkillError {}

impl From<std::io::Error> for SkillError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<SkillError> for std::io::Error {
    fn from(e: SkillError) -> Self {
        match e {
            SkillError::Io(e) | SkillError::WriteFailed(e) => e,
            SkillError::NotFound(d) => std::io::Error::new(std::io::ErrorKind::NotFound, d),
            SkillError::InvalidName(d) => std::io::Error::new(std::io::ErrorKind::InvalidInput, d),
            SkillError::OutsideRoot(d) | SkillError::NotDirectory(d) => {
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, d)
            }
            SkillError::Conflict(d) => std::io::Error::new(std::io::ErrorKind::AlreadyExists, d),
        }
    }
}

/// The extension stored for an allowlisted MIME type (case-insensitive).
pub fn paste_image_extension(mime: &str) -> Option<&'static str> {
    let mime = mime.trim().to_ascii_lowercase();
    PASTE_IMAGE_MIME_ALLOWLIST
        .iter()
        .find(|(allowed, _)| *allowed == mime)
        .map(|(_, ext)| *ext)
}

/// Calls `found` with each pasted image name `text` mentions: a
/// `<64 lowercase hex>.<ext>` upload name (`ext` in
/// [`PASTE_IMAGE_MIME_ALLOWLIST`], not followed by an alphanumeric) or a TUI
/// `<lowercase hyphenated uuid>.png` paste. The one matcher both
/// `rsid::paste_retention` (#86) and `Store::update_event_content`'s
/// reference fence (#179) use, so they agree on what a reference is.
pub fn paste_image_names_in(text: &str, found: &mut dyn FnMut(String)) {
    const SHA256_HEX_LEN: usize = 64;
    const UUID_LEN: usize = 36;
    fn is_lower_hex(b: u8) -> bool {
        b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
    }
    fn is_lower_uuid(bytes: &[u8]) -> bool {
        bytes.len() == UUID_LEN
            && bytes.iter().enumerate().all(|(i, b)| match i {
                8 | 13 | 18 | 23 => *b == b'-',
                _ => is_lower_hex(*b),
            })
    }
    let bytes = text.as_bytes();
    for (at, _) in text.match_indices(".png") {
        if at >= UUID_LEN && is_lower_uuid(&bytes[at - UUID_LEN..at]) {
            found(format!("{}.png", &text[at - UUID_LEN..at]));
        }
    }
    let mut start = 0;
    while start < bytes.len() {
        if !is_lower_hex(bytes[start]) {
            start += 1;
            continue;
        }
        let mut end = start;
        while end < bytes.len() && is_lower_hex(bytes[end]) {
            end += 1;
        }
        if end - start == SHA256_HEX_LEN && bytes.get(end) == Some(&b'.') {
            for (_, ext) in PASTE_IMAGE_MIME_ALLOWLIST {
                let tail = &bytes[end + 1..];
                if tail.starts_with(ext.as_bytes())
                    && !tail
                        .get(ext.len())
                        .is_some_and(|b| b.is_ascii_alphanumeric())
                {
                    found(format!("{}.{ext}", &text[start..end]));
                }
            }
        }
        start = end;
    }
}

/// The image MIME type the magic bytes declare, if it is an allowlisted one.
pub fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Longest base64 text that can decode to at most `max_bytes` (padded).
pub const fn base64_len_bound(max_bytes: usize) -> usize {
    max_bytes.div_ceil(3) * 4
}

/// List the slash commands under `root`, the way the TUI offers them.
///
/// `root` is canonicalized first; every candidate (`.claude`, the two
/// directories, each entry and each `SKILL.md`) must canonicalize inside it,
/// else it is skipped and counted in `skipped_outside_root`. Symlinks that
/// stay inside the root are followed. A missing directory lists nothing.
pub fn discover_slash_commands(
    root: &Path,
    max_entries: usize,
) -> std::io::Result<ListSlashCommandsResponseV1> {
    let root = root.canonicalize()?;
    let handle = AssetDir::open(&root)?;
    let mut scan = Scan {
        root: &root,
        handle: &handle,
        commands: Vec::new(),
        truncated: false,
        skipped_outside_root: 0,
    };
    if let Some(claude) = scan.inside(&root.join(".claude")) {
        if let Some(dir) = scan.inside(&claude.join("commands")) {
            scan.commands_dir(&dir);
        }
        if let Some(dir) = scan.inside(&claude.join("skills")) {
            scan.skills_dir(&dir);
        }
    }
    let Scan {
        mut commands,
        mut truncated,
        skipped_outside_root,
        ..
    } = scan;
    commands.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then((a.source as u8).cmp(&(b.source as u8)))
    });
    let max_entries = max_entries.clamp(1, MAX_SLASH_COMMANDS);
    if commands.len() > max_entries {
        commands.truncate(max_entries);
        truncated = true;
    }
    Ok(ListSlashCommandsResponseV1 {
        root: root.to_string_lossy().into_owned(),
        commands,
        truncated,
        skipped_outside_root,
    })
}

struct Scan<'a> {
    root: &'a Path,
    handle: &'a AssetDir,
    commands: Vec<SlashCommandV1>,
    truncated: bool,
    skipped_outside_root: u32,
}

impl Scan<'_> {
    /// The canonical form of an existing `path` inside the root; `None` when
    /// it does not exist or (counted) resolves outside the root.
    fn inside(&mut self, path: &Path) -> Option<PathBuf> {
        let canonical = path.canonicalize().ok()?;
        if canonical.starts_with(self.root) {
            Some(canonical)
        } else {
            self.skipped_outside_root = self.skipped_outside_root.saturating_add(1);
            None
        }
    }

    /// Entry names of `dir`, bounded by [`SLASH_SCAN_LIMIT`].
    fn entries(&mut self, dir: &Path) -> Vec<(String, PathBuf)> {
        let Ok(read) = dir
            .strip_prefix(self.root)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            .and_then(|relative| self.handle.open_dir(relative))
            .and_then(|dir| dir.entries(SLASH_SCAN_LIMIT))
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in read {
            if out.len() >= SLASH_SCAN_LIMIT {
                self.truncated = true;
                break;
            }
            if let Some(name) = entry.to_str() {
                out.push((name.to_string(), dir.join(&entry)));
            }
        }
        out
    }

    fn commands_dir(&mut self, dir: &Path) {
        for (name, path) in self.entries(dir) {
            let Some(stem) = name.strip_suffix(".md").filter(|stem| !stem.is_empty()) else {
                continue;
            };
            let Some(file) = self.inside(&path) else {
                continue;
            };
            if !file.is_file() {
                continue;
            }
            self.commands.push(SlashCommandV1 {
                name: stem.to_string(),
                description: description_of(
                    self.handle,
                    self.root,
                    &file,
                    COMMAND_FALLBACK_DESCRIPTION,
                ),
                source: SlashCommandSourceV1::Command,
            });
        }
    }

    fn skills_dir(&mut self, dir: &Path) {
        for (name, path) in self.entries(dir) {
            let Some(skill) = self.inside(&path) else {
                continue;
            };
            if !skill.is_dir() {
                continue;
            }
            let Some(file) = self.inside(&skill.join("SKILL.md")) else {
                continue;
            };
            if !file.is_file() {
                continue;
            }
            self.commands.push(SlashCommandV1 {
                name,
                description: description_of(
                    self.handle,
                    self.root,
                    &file,
                    SKILL_FALLBACK_DESCRIPTION,
                ),
                source: SlashCommandSourceV1::Skill,
            });
        }
    }
}

/// The frontmatter `description` in the first [`SLASH_DESCRIPTION_READ_BYTES`]
/// of `file`, on one line and capped, else `fallback`.
fn description_of(handle: &AssetDir, root: &Path, file: &Path, fallback: &str) -> String {
    frontmatter_description(handle, root, file).unwrap_or_else(|| fallback.to_string())
}

/// The frontmatter `description` in the first [`SLASH_DESCRIPTION_READ_BYTES`]
/// of `file`, on one line and capped at [`MAX_SLASH_DESCRIPTION_CHARS`].
fn frontmatter_description(handle: &AssetDir, root: &Path, file: &Path) -> Option<String> {
    let mut head = Vec::new();
    open_asset(handle, root, file)
        .and_then(|f| f.take(SLASH_DESCRIPTION_READ_BYTES).read_to_end(&mut head))
        .ok()?;
    let text = String::from_utf8_lossy(&head);
    let description = crate::command_meta::parse_command_frontmatter(&text)
        .and_then(|meta| meta.description)
        .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|d| !d.is_empty())?;
    if description.chars().count() > MAX_SLASH_DESCRIPTION_CHARS {
        let mut cut: String = description
            .chars()
            .take(MAX_SLASH_DESCRIPTION_CHARS - 1)
            .collect();
        cut.push('\u{2026}');
        Some(cut)
    } else {
        Some(description)
    }
}

/// The canonical containment root and skills directory of `scope`; `None`
/// when the skills directory (or a project's `.claude`) does not exist.
fn resolve_skills_dir(scope: &SkillScope) -> Result<Option<(PathBuf, PathBuf)>, SkillError> {
    let existing = |path: &Path| match path.canonicalize() {
        Ok(canonical) => Ok(Some(canonical)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SkillError::Io(e)),
    };
    match scope {
        SkillScope::User { home } => {
            Ok(existing(&home.join(".claude").join("skills"))?.map(|dir| (dir.clone(), dir)))
        }
        SkillScope::Root(root) => {
            let root = root.canonicalize()?;
            let inside = |path: PathBuf| {
                if path.starts_with(&root) {
                    Ok(path)
                } else {
                    Err(SkillError::OutsideRoot(format!(
                        "{} resolves outside {}",
                        path.display(),
                        root.display()
                    )))
                }
            };
            let Some(claude) = existing(&root.join(".claude"))? else {
                return Ok(None);
            };
            let claude = inside(claude)?;
            let Some(skills) = existing(&claude.join("skills"))? else {
                return Ok(None);
            };
            let skills = inside(skills)?;
            Ok(Some((root, skills)))
        }
    }
}

/// The skills directory `scope` names, before resolution (for a response
/// when it does not exist).
fn nominal_skills_dir(scope: &SkillScope) -> PathBuf {
    match scope {
        SkillScope::Root(base) | SkillScope::User { home: base } => {
            base.join(".claude").join("skills")
        }
    }
}

/// List the skill directories of `scope`, the way the TUI's Settings >
/// Skills shows them.
///
/// Every directory entry must canonicalize inside the containment root (see
/// [`SkillScope`]), and so must its `SKILL.md`; an entry that leaves it is
/// skipped and counted in `skipped_outside_root`, as is a project
/// `.claude/skills` linked out of the project. A missing directory lists
/// nothing. At most [`SLASH_SCAN_LIMIT`] entries are scanned.
pub fn list_skills(
    scope: &SkillScope,
    max_entries: usize,
) -> std::io::Result<ListSkillsResponseV1> {
    let mut listing = ListSkillsResponseV1 {
        skills_dir: nominal_skills_dir(scope).to_string_lossy().into_owned(),
        skills: Vec::new(),
        truncated: false,
        skipped_outside_root: 0,
    };
    let (root, dir) = match resolve_skills_dir(scope) {
        Ok(Some(found)) => found,
        Ok(None) => return Ok(listing),
        Err(SkillError::OutsideRoot(_)) => {
            listing.skipped_outside_root = 1;
            return Ok(listing);
        }
        Err(e) => return Err(e.into()),
    };
    listing.skills_dir = dir.to_string_lossy().into_owned();
    let handle = AssetDir::open(&root)?;
    let relative = dir
        .strip_prefix(&root)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::PermissionDenied))?;
    for (index, entry) in handle
        .open_dir(relative)?
        .entries(SLASH_SCAN_LIMIT)?
        .into_iter()
        .enumerate()
    {
        if index >= SLASH_SCAN_LIMIT {
            listing.truncated = true;
            break;
        }
        let Some(raw) = entry.to_str().map(str::to_string) else {
            continue;
        };
        let Ok(skill) = dir.join(&entry).canonicalize() else {
            continue;
        };
        if !skill.starts_with(&root) {
            listing.skipped_outside_root = listing.skipped_outside_root.saturating_add(1);
            continue;
        }
        if !skill.is_dir() {
            continue;
        }
        let md = match skill.join("SKILL.md").canonicalize() {
            Ok(md) if !md.starts_with(&root) => {
                listing.skipped_outside_root = listing.skipped_outside_root.saturating_add(1);
                continue;
            }
            Ok(md) if md.is_file() => Some(md),
            _ => None,
        };
        let (name, enabled) = match raw.strip_suffix(DISABLED_SKILL_SUFFIX) {
            Some(stem) if !stem.is_empty() => (stem.to_string(), false),
            _ => (raw, true),
        };
        listing.skills.push(SkillSummaryV1 {
            name,
            enabled,
            description: md
                .as_deref()
                .and_then(|md| frontmatter_description(&handle, &root, md)),
            has_skill_md: md.is_some(),
        });
    }
    listing
        .skills
        .sort_by(|a, b| a.name.cmp(&b.name).then(b.enabled.cmp(&a.enabled)));
    let max_entries = max_entries.clamp(1, MAX_SKILLS);
    if listing.skills.len() > max_entries {
        listing.skills.truncate(max_entries);
        listing.truncated = true;
    }
    Ok(listing)
}

/// Refuse a name that is not one plain path component.
fn check_skill_name(name: &str) -> Result<(), SkillError> {
    let plain = !name.is_empty()
        && name.len() <= MAX_SKILL_NAME_BYTES
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0']);
    if plain {
        Ok(())
    } else {
        Err(SkillError::InvalidName(format!(
            "'{}' is not a skill directory name",
            name.escape_debug()
        )))
    }
}

/// Read skill `name`'s `SKILL.md` in `scope` (`<name>/`, else
/// `<name>.disabled/`), at most `max_bytes` (capped at [`MAX_SKILL_BYTES`]).
///
/// The skill directory and its `SKILL.md` must canonicalize inside the
/// containment root, else [`SkillError::OutsideRoot`]. The root is pinned and
/// the canonical relative path is opened without following replacement links
/// in any component. Nothing is written.
pub fn read_skill(
    scope: &SkillScope,
    name: &str,
    max_bytes: usize,
) -> Result<GetSkillResponseV1, SkillError> {
    check_skill_name(name)?;
    let not_found = || SkillError::NotFound(format!("SKILL.md not found for: {name}"));
    let (root, dir) = resolve_skills_dir(scope)?.ok_or_else(not_found)?;
    let handle = AssetDir::open(&root)?;
    let inside = |path: PathBuf| {
        if path.starts_with(&root) {
            Ok(path)
        } else {
            Err(SkillError::OutsideRoot(format!(
                "skill '{name}' resolves outside {}",
                root.display()
            )))
        }
    };
    for (dir_name, enabled) in [
        (name.to_string(), true),
        (format!("{name}{DISABLED_SKILL_SUFFIX}"), false),
    ] {
        let candidate = dir.join(&dir_name);
        if std::fs::symlink_metadata(&candidate).is_err() {
            continue;
        }
        let skill = inside(candidate.canonicalize()?)?;
        let md = skill.join("SKILL.md");
        if std::fs::symlink_metadata(&md).is_err() {
            continue;
        }
        let md = inside(md.canonicalize()?)?;
        if !md.is_file() {
            return Err(not_found());
        }
        let file = open_asset(&handle, &root, &md)?;
        let bytes = file.metadata()?.len();
        let cap = max_bytes.min(MAX_SKILL_BYTES);
        let mut buf = Vec::new();
        file.take(cap as u64 + 1).read_to_end(&mut buf)?;
        let mut truncated = buf.len() > cap;
        if truncated {
            buf.truncate(cap);
            // Never split a UTF-8 sequence at the cut.
            if let Err(e) = std::str::from_utf8(&buf) {
                if e.error_len().is_none() {
                    buf.truncate(e.valid_up_to());
                }
            }
        }
        // Lossy conversion may expand each invalid byte to three UTF-8 bytes.
        // Enforce the wire content cap after conversion as well.
        let mut content = String::from_utf8_lossy(&buf).into_owned();
        if content.len() > cap {
            let mut end = cap;
            while !content.is_char_boundary(end) {
                end -= 1;
            }
            content.truncate(end);
            truncated = true;
        }
        return Ok(GetSkillResponseV1 {
            name: name.to_string(),
            enabled,
            path: md.to_string_lossy().into_owned(),
            content,
            bytes,
            truncated,
        });
    }
    Err(not_found())
}

/// The pinned skills directory of `scope` for an enable, disable or delete,
/// with its canonical path. A missing skills directory is `NotFound`.
fn skills_dir_for_write(scope: &SkillScope, name: &str) -> Result<(AssetDir, PathBuf), SkillError> {
    check_skill_name(name)?;
    let (root, dir) = resolve_skills_dir(scope)?.ok_or_else(|| skill_missing(name))?;
    let relative = dir
        .strip_prefix(&root)
        .map_err(|_| SkillError::OutsideRoot(format!("{} leaves its root", dir.display())))?;
    let handle = AssetDir::open(&root)?.open_dir(relative)?;
    Ok((handle, dir))
}

fn skill_missing(name: &str) -> SkillError {
    SkillError::NotFound(format!("skill not found: {name}"))
}

/// Whether `entry` exists in the pinned skills directory as a real directory;
/// a symlink or any other non-directory entry is refused, never followed.
fn real_skill_dir(dir: &AssetDir, entry: &str) -> Result<bool, SkillError> {
    match dir.entry_is_dir(Path::new(entry))? {
        None => Ok(false),
        Some(true) => Ok(true),
        Some(false) => Err(SkillError::NotDirectory(format!(
            "'{}' is not a skill directory (a link or other non-directory entry)",
            entry.escape_debug()
        ))),
    }
}

/// Enable or disable (`want`), or flip when `want` is `None`, as the TUI's
/// `e` does: `<name>/` is enabled and renamed to `<name>.disabled/` to
/// disable; an existing `<name>/` wins over `<name>.disabled/`.
fn change_skill_state(
    scope: &SkillScope,
    name: &str,
    want: Option<bool>,
) -> Result<SetSkillEnabledResponseV1, SkillError> {
    let (handle, dir) = skills_dir_for_write(scope, name)?;
    let disabled = format!("{name}{DISABLED_SKILL_SUFFIX}");
    let enabled_dir = real_skill_dir(&handle, name)?;
    let disabled_dir = real_skill_dir(&handle, &disabled)?;
    let current = match (enabled_dir, disabled_dir) {
        (true, _) => true,
        (false, true) => false,
        (false, false) => return Err(skill_missing(name)),
    };
    let target = want.unwrap_or(!current);
    let entry = |enabled: bool| if enabled { name } else { disabled.as_str() };
    if target != current {
        if enabled_dir && disabled_dir {
            return Err(SkillError::Conflict(format!(
                "both '{name}' and '{disabled}' exist; remove one first"
            )));
        }
        handle
            .rename(Path::new(entry(current)), Path::new(entry(target)))
            .map_err(SkillError::WriteFailed)?;
    }
    Ok(SetSkillEnabledResponseV1 {
        name: name.to_string(),
        enabled: target,
        changed: target != current,
        path: dir.join(entry(target)).to_string_lossy().into_owned(),
    })
}

/// Enable or disable skill `name` in `scope` (`SetSkillEnabled`): a rename
/// between `<name>/` and `<name>.disabled/` inside the pinned skills
/// directory. Already in that state is `changed: false`; both directories
/// present is [`SkillError::Conflict`]; a link or non-directory entry is
/// [`SkillError::NotDirectory`].
pub fn set_skill_enabled(
    scope: &SkillScope,
    name: &str,
    enabled: bool,
) -> Result<SetSkillEnabledResponseV1, SkillError> {
    change_skill_state(scope, name, Some(enabled))
}

/// Flip skill `name` in `scope` (the TUI's `e`); returns the new state.
pub fn toggle_skill(
    scope: &SkillScope,
    name: &str,
) -> Result<SetSkillEnabledResponseV1, SkillError> {
    change_skill_state(scope, name, None)
}

/// Delete skill `name` in `scope` (`DeleteSkill`, the TUI's `d`): `<name>/`,
/// else `<name>.disabled/`, is moved whole into a fresh batch of the skill
/// trash `trash_root` (created when missing) with one `renameat`, so it can be
/// restored. The entry must be a real directory in the pinned skills
/// directory and is never followed. A trash on another filesystem is refused
/// and nothing moves.
pub fn delete_skill(
    scope: &SkillScope,
    name: &str,
    trash_root: &Path,
) -> Result<DeleteSkillResponseV1, SkillError> {
    let (handle, dir) = skills_dir_for_write(scope, name)?;
    for (entry, enabled) in [
        (name.to_string(), true),
        (format!("{name}{DISABLED_SKILL_SUFFIX}"), false),
    ] {
        if !real_skill_dir(&handle, &entry)? {
            continue;
        }
        let path = dir.join(&entry);
        let trash_path = trash_skill_entry(&handle, &path, &entry, trash_root)?;
        return Ok(DeleteSkillResponseV1 {
            name: name.to_string(),
            enabled,
            path: path.to_string_lossy().into_owned(),
            trash_path: trash_path.to_string_lossy().into_owned(),
        });
    }
    Err(skill_missing(name))
}

/// Move `entry` of the pinned skills directory into a new trash batch. The
/// sidecar is created first and exclusively, so it claims the batch name; a
/// failed move removes the empty batch and its sidecar again.
fn trash_skill_entry(
    skills: &AssetDir,
    origin: &Path,
    entry: &str,
    trash_root: &Path,
) -> Result<PathBuf, SkillError> {
    use std::io::Write as _;
    let trash = AssetDir::create_data_root(trash_root).map_err(SkillError::WriteFailed)?;
    let id = Uuid::new_v4().simple().to_string();
    let batch = format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S"),
        &id[..8]
    );
    let sidecar = PathBuf::from(format!("{batch}.{}", crate::trash_prune::ORIGIN_EXTENSION));
    let discard = |batch_made: bool| {
        if batch_made {
            let _ = trash.remove_tree(Path::new(&batch));
        }
        let _ = trash.remove_file(&sidecar);
    };
    let mut origin_file = trash
        .create_file(&sidecar)
        .map_err(SkillError::WriteFailed)?;
    if let Err(e) = origin_file
        .write_all(origin.to_string_lossy().as_bytes())
        .and_then(|()| origin_file.sync_all())
    {
        discard(false);
        return Err(SkillError::WriteFailed(e));
    }
    let batch_dir = match trash.create_dir(Path::new(&batch)) {
        Ok(dir) => dir,
        Err(e) => {
            discard(false);
            return Err(SkillError::WriteFailed(e));
        }
    };
    match skills.rename_into(Path::new(entry), &batch_dir, Path::new(entry)) {
        Ok(()) => Ok(trash_root.join(&batch).join(entry)),
        Err(e) => {
            discard(true);
            if e.kind() == std::io::ErrorKind::CrossesDevices {
                return Err(SkillError::WriteFailed(std::io::Error::new(
                    std::io::ErrorKind::CrossesDevices,
                    format!(
                        "the skill trash {} is on another filesystem than {}; nothing was deleted",
                        trash_root.display(),
                        origin.display()
                    ),
                )));
            }
            Err(SkillError::WriteFailed(e))
        }
    }
}

/// The only content-opening boundary for skill and slash-command reads.
fn open_asset(handle: &AssetDir, root: &Path, path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(test)]
    BEFORE_ASSET_OPEN.with(|hook| {
        let hook = hook.borrow_mut().take();
        if let Some(hook) = hook {
            hook(path);
        }
    });
    let relative = path
        .strip_prefix(root)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::PermissionDenied))?;
    handle.read_file(relative)
}

#[cfg(test)]
type BeforeAssetOpen = Box<dyn FnOnce(&Path)>;
#[cfg(test)]
std::thread_local! {
    static BEFORE_ASSET_OPEN: std::cell::RefCell<Option<BeforeAssetOpen>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(listing: &ListSlashCommandsResponseV1) -> Vec<(&str, SlashCommandSourceV1)> {
        listing
            .commands
            .iter()
            .map(|c| (c.name.as_str(), c.source))
            .collect()
    }

    #[test]
    fn commands_and_skills_are_listed_with_frontmatter_descriptions() {
        let dir = tempfile::TempDir::new().unwrap();
        let commands = dir.path().join(".claude/commands");
        std::fs::create_dir_all(&commands).unwrap();
        std::fs::write(
            commands.join("plan.md"),
            "---\ndescription: Make a plan\n---\nbody",
        )
        .unwrap();
        std::fs::write(commands.join("bare.md"), "no frontmatter").unwrap();
        std::fs::write(commands.join("notes.txt"), "x").unwrap();
        std::fs::create_dir_all(commands.join("dir.md")).unwrap();
        let skills = dir.path().join(".claude/skills");
        std::fs::create_dir_all(skills.join("debug")).unwrap();
        std::fs::write(
            skills.join("debug/SKILL.md"),
            "---\nname: debug\ndescription: Investigate\n  a problem\n---\n",
        )
        .unwrap();
        std::fs::create_dir_all(skills.join("empty")).unwrap();

        let listing = discover_slash_commands(dir.path(), DEFAULT_SLASH_COMMANDS).unwrap();
        assert_eq!(
            names(&listing),
            vec![
                ("bare", SlashCommandSourceV1::Command),
                ("debug", SlashCommandSourceV1::Skill),
                ("plan", SlashCommandSourceV1::Command),
            ]
        );
        assert_eq!(
            listing.commands[0].description,
            COMMAND_FALLBACK_DESCRIPTION
        );
        assert_eq!(listing.commands[1].description, "Investigate a problem");
        assert_eq!(listing.commands[2].description, "Make a plan");
        assert_eq!(
            listing.root,
            dir.path().canonicalize().unwrap().to_string_lossy()
        );
        assert!(!listing.truncated);
        assert_eq!(listing.skipped_outside_root, 0);
    }

    #[test]
    fn a_root_without_claude_lists_nothing_and_max_entries_truncates() {
        let dir = tempfile::TempDir::new().unwrap();
        let listing = discover_slash_commands(dir.path(), 10).unwrap();
        assert!(listing.commands.is_empty());
        let commands = dir.path().join(".claude/commands");
        std::fs::create_dir_all(&commands).unwrap();
        for name in ["a", "b", "c"] {
            std::fs::write(commands.join(format!("{name}.md")), "").unwrap();
        }
        let listing = discover_slash_commands(dir.path(), 2).unwrap();
        assert_eq!(listing.commands.len(), 2);
        assert!(listing.truncated);
        assert!(discover_slash_commands(&dir.path().join("missing"), 2).is_err());
    }

    #[test]
    fn a_long_description_is_capped() {
        let dir = tempfile::TempDir::new().unwrap();
        let commands = dir.path().join(".claude/commands");
        std::fs::create_dir_all(&commands).unwrap();
        let long = "word ".repeat(200);
        std::fs::write(
            commands.join("long.md"),
            format!("---\ndescription: {long}\n---\n"),
        )
        .unwrap();
        let listing = discover_slash_commands(dir.path(), 10).unwrap();
        let description = &listing.commands[0].description;
        assert_eq!(description.chars().count(), MAX_SLASH_DESCRIPTION_CHARS);
        assert!(description.ends_with('\u{2026}'));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escapes_are_skipped_and_inside_links_followed() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(
            outside.path().join("secret.md"),
            "---\ndescription: outside\n---\n",
        )
        .unwrap();
        std::fs::create_dir_all(outside.path().join("skill")).unwrap();
        std::fs::write(outside.path().join("skill/SKILL.md"), "").unwrap();

        let dir = tempfile::TempDir::new().unwrap();
        let commands = dir.path().join(".claude/commands");
        std::fs::create_dir_all(&commands).unwrap();
        std::fs::write(commands.join("real.md"), "").unwrap();
        symlink(outside.path().join("secret.md"), commands.join("leak.md")).unwrap();
        symlink(commands.join("real.md"), commands.join("alias.md")).unwrap();
        let skills = dir.path().join(".claude/skills");
        std::fs::create_dir_all(skills.join("inner")).unwrap();
        symlink(
            outside.path().join("skill/SKILL.md"),
            skills.join("inner/SKILL.md"),
        )
        .unwrap();
        symlink(outside.path().join("skill"), skills.join("away")).unwrap();

        let listing = discover_slash_commands(dir.path(), 10).unwrap();
        assert_eq!(
            names(&listing),
            vec![
                ("alias", SlashCommandSourceV1::Command),
                ("real", SlashCommandSourceV1::Command),
            ]
        );
        assert_eq!(listing.skipped_outside_root, 3);

        // A `.claude` that is itself a link out of the root lists nothing.
        let linked = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(outside.path().join(".claude/commands")).unwrap();
        std::fs::write(outside.path().join(".claude/commands/x.md"), "").unwrap();
        symlink(
            outside.path().join(".claude"),
            linked.path().join(".claude"),
        )
        .unwrap();
        let listing = discover_slash_commands(linked.path(), 10).unwrap();
        assert!(listing.commands.is_empty());
        assert_eq!(listing.skipped_outside_root, 1);
    }

    #[test]
    fn mime_allowlist_and_magic_bytes_agree() {
        assert_eq!(paste_image_extension(" Image/PNG "), Some("png"));
        assert_eq!(paste_image_extension("image/jpeg"), Some("jpg"));
        assert_eq!(paste_image_extension("image/svg+xml"), None);
        assert_eq!(paste_image_extension("../png"), None);
        assert_eq!(
            sniff_image_mime(b"\x89PNG\r\n\x1a\nrest"),
            Some("image/png")
        );
        assert_eq!(
            sniff_image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        assert_eq!(sniff_image_mime(b"GIF89a..."), Some("image/gif"));
        assert_eq!(
            sniff_image_mime(b"RIFF\0\0\0\0WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(sniff_image_mime(b"RIFF\0\0\0\0WAVE"), None);
        assert_eq!(sniff_image_mime(b"<svg"), None);
        assert_eq!(base64_len_bound(3), 4);
        assert_eq!(base64_len_bound(4), 8);
    }

    #[test]
    fn requests_refuse_unknown_fields() {
        assert!(
            serde_json::from_value::<UploadPasteImageRequestV1>(
                serde_json::json!({"mime": "image/png", "data_base64": "", "path": "/etc/x"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<ListSlashCommandsRequestV1>(
                serde_json::json!({"working_dir": "/"})
            )
            .is_err()
        );
    }

    fn skill(dir: &Path, dir_name: &str, body: &str) {
        let skill = dir.join(dir_name);
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn user_skills_list_strips_the_disabled_suffix_and_reads_descriptions() {
        let home = tempfile::TempDir::new().unwrap();
        let skills = home.path().join(".claude/skills");
        skill(
            &skills,
            "alpha",
            "---\ndescription: Alpha skill\n---\n\nhello\n",
        );
        skill(
            &skills,
            "beta.disabled",
            "---\ndescription: Beta skill\n---\n",
        );
        skill(&skills, "gamma", "no frontmatter\n");
        std::fs::create_dir_all(skills.join("empty")).unwrap();
        std::fs::write(skills.join("README.txt"), "ignore me").unwrap();
        let scope = SkillScope::User {
            home: home.path().to_path_buf(),
        };

        let listing = list_skills(&scope, DEFAULT_SKILLS).unwrap();
        let rows: Vec<_> = listing
            .skills
            .iter()
            .map(|s| {
                (
                    s.name.as_str(),
                    s.enabled,
                    s.description.as_deref(),
                    s.has_skill_md,
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("alpha", true, Some("Alpha skill"), true),
                ("beta", false, Some("Beta skill"), true),
                ("empty", true, None, false),
                ("gamma", true, None, true),
            ]
        );
        assert_eq!(
            listing.skills_dir,
            skills.canonicalize().unwrap().to_string_lossy()
        );
        assert!(!listing.truncated);
        assert_eq!(listing.skipped_outside_root, 0);
        let one = list_skills(&scope, 1).unwrap();
        assert_eq!(one.skills.len(), 1);
        assert!(one.truncated);

        let missing = SkillScope::User {
            home: home.path().join("nobody"),
        };
        assert!(
            list_skills(&missing, DEFAULT_SKILLS)
                .unwrap()
                .skills
                .is_empty()
        );
    }

    #[test]
    fn read_skill_returns_the_enabled_or_disabled_skill_md_within_the_cap() {
        let dir = tempfile::TempDir::new().unwrap();
        let skills = dir.path().join(".claude/skills");
        skill(&skills, "demo", "---\ndescription: Demo\n---\n\nhello\n");
        skill(&skills, "off.disabled", "parked\n");
        skill(&skills, "wide", &"\u{e9}".repeat(10));
        let scope = SkillScope::Root(dir.path().to_path_buf());

        let demo = read_skill(&scope, "demo", MAX_SKILL_BYTES).unwrap();
        assert_eq!(demo.content, "---\ndescription: Demo\n---\n\nhello\n");
        assert!(demo.enabled);
        assert!(!demo.truncated);
        assert_eq!(demo.bytes, demo.content.len() as u64);
        let expected = skills.canonicalize().unwrap().join("demo/SKILL.md");
        assert_eq!(demo.path, expected.to_string_lossy());

        let off = read_skill(&scope, "off", MAX_SKILL_BYTES).unwrap();
        assert_eq!((off.content.as_str(), off.enabled), ("parked\n", false));

        // 20 bytes of two-byte characters cut at 5 bytes keeps 2 whole ones.
        let wide = read_skill(&scope, "wide", 5).unwrap();
        assert_eq!(wide.content, "\u{e9}\u{e9}");
        assert!(wide.truncated);
        assert_eq!(wide.bytes, 20);

        for name in ["", ".", "..", "../demo", "a/b", "a\\b", "nul\0"] {
            let err = read_skill(&scope, name, MAX_SKILL_BYTES).unwrap_err();
            assert_eq!(err.code(), "skill_invalid_name", "{name:?}");
        }
        let err = read_skill(&scope, "absent", MAX_SKILL_BYTES).unwrap_err();
        assert_eq!(err.code(), "skill_not_found");
        let bare = tempfile::TempDir::new().unwrap();
        let err = read_skill(&SkillScope::Root(bare.path().to_path_buf()), "demo", 10).unwrap_err();
        assert_eq!(err.code(), "skill_not_found");
    }

    #[cfg(unix)]
    #[test]
    fn read_skill_caps_lossy_utf8_content_and_preserves_file_size() {
        let dir = tempfile::TempDir::new().unwrap();
        let skills = dir.path().join(".claude/skills");
        skill(&skills, "invalid", "");
        let scope = SkillScope::Root(dir.path().to_path_buf());
        for len in [MAX_SKILL_BYTES, MAX_SKILL_BYTES + 1] {
            std::fs::write(skills.join("invalid/SKILL.md"), vec![0xff; len]).unwrap();
            let response = read_skill(&scope, "invalid", MAX_SKILL_BYTES).unwrap();
            assert_eq!(response.content, "\u{fffd}".repeat(MAX_SKILL_BYTES / 3));
            assert!(response.content.len() <= MAX_SKILL_BYTES);
            assert!(response.truncated);
            assert_eq!(response.bytes, len as u64);
        }
        std::fs::write(skills.join("invalid/SKILL.md"), [b'a', 0xff, b'b']).unwrap();
        let response = read_skill(&scope, "invalid", 4).unwrap();
        assert_eq!(response.content, "a\u{fffd}");
        assert!(response.truncated);
        assert_eq!(response.bytes, 3);
        let response = read_skill(&scope, "invalid", 0).unwrap();
        assert_eq!(response.content, "");
        assert!(response.truncated);
    }

    #[cfg(unix)]
    fn swap_at_open(outside: PathBuf, ancestor: bool) {
        BEFORE_ASSET_OPEN.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |md| {
                if ancestor {
                    let parent = md.parent().unwrap();
                    std::fs::rename(parent, parent.with_file_name("saved")).unwrap();
                    std::os::unix::fs::symlink(&outside, parent).unwrap();
                } else {
                    std::fs::remove_file(md).unwrap();
                    std::os::unix::fs::symlink(outside.join(md.file_name().unwrap()), md).unwrap();
                }
            }));
        });
    }

    #[cfg(unix)]
    #[test]
    fn skill_reads_refuse_file_and_ancestor_swaps_after_validation() {
        for ancestor in [false, true] {
            let dir = tempfile::TempDir::new().unwrap();
            let outside = tempfile::TempDir::new().unwrap();
            let skills = dir.path().join(".claude/skills");
            skill(&skills, "demo", "inside\n");
            std::fs::write(outside.path().join("SKILL.md"), "outside fixture\n").unwrap();
            swap_at_open(outside.path().to_path_buf(), ancestor);
            let result = read_skill(
                &SkillScope::Root(dir.path().to_path_buf()),
                "demo",
                MAX_SKILL_BYTES,
            );
            assert_eq!(result.unwrap_err().code(), "skill_read_failed");
            assert_eq!(
                std::fs::read_to_string(outside.path().join("SKILL.md")).unwrap(),
                "outside fixture\n"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn descriptions_refuse_file_and_ancestor_swaps_after_validation() {
        for source in ["command", "slash_skill", "skill"] {
            for ancestor in [false, true] {
                let dir = tempfile::TempDir::new().unwrap();
                let outside = tempfile::TempDir::new().unwrap();
                let md_name = if source == "command" {
                    "demo.md"
                } else {
                    "SKILL.md"
                };
                let md = if source == "command" {
                    dir.path().join(".claude/commands/demo.md")
                } else {
                    dir.path().join(".claude/skills/demo/SKILL.md")
                };
                std::fs::create_dir_all(md.parent().unwrap()).unwrap();
                std::fs::write(&md, "---\ndescription: inside\n---\n").unwrap();
                std::fs::write(
                    outside.path().join(md_name),
                    "---\ndescription: outside fixture\n---\n",
                )
                .unwrap();
                swap_at_open(outside.path().to_path_buf(), ancestor);
                if source == "skill" {
                    let result =
                        list_skills(&SkillScope::Root(dir.path().to_path_buf()), DEFAULT_SKILLS)
                            .unwrap();
                    assert_eq!(result.skills.len(), 1);
                    assert_eq!(result.skills[0].description, None);
                } else {
                    let result =
                        discover_slash_commands(dir.path(), DEFAULT_SLASH_COMMANDS).unwrap();
                    assert_eq!(result.commands.len(), 1);
                    assert_eq!(
                        result.commands[0].description,
                        if source == "command" {
                            COMMAND_FALLBACK_DESCRIPTION
                        } else {
                            SKILL_FALLBACK_DESCRIPTION
                        }
                    );
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn skill_read_keeps_the_pinned_root_after_root_replacement() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        skill(&root.join(".claude/skills"), "demo", "inside\n");
        skill(&outside.join(".claude/skills"), "demo", "outside fixture\n");
        let saved = dir.path().join("saved");
        let old_root = root.clone();
        BEFORE_ASSET_OPEN.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |_| {
                std::fs::rename(&old_root, &saved).unwrap();
                std::os::unix::fs::symlink(&outside, &old_root).unwrap();
            }));
        });
        let response = read_skill(&SkillScope::Root(root), "demo", MAX_SKILL_BYTES).unwrap();
        assert_eq!(response.content, "inside\n");
    }

    #[cfg(unix)]
    #[test]
    fn skills_linked_out_of_the_root_are_refused_and_skipped() {
        let outside = tempfile::TempDir::new().unwrap();
        skill(outside.path(), "secret", "---\ndescription: leaked\n---\n");
        let dir = tempfile::TempDir::new().unwrap();
        let skills = dir.path().join(".claude/skills");
        skill(&skills, "ok", "fine\n");
        std::os::unix::fs::symlink(outside.path().join("secret"), skills.join("dirlink")).unwrap();
        std::fs::create_dir_all(skills.join("filelink")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret/SKILL.md"),
            skills.join("filelink/SKILL.md"),
        )
        .unwrap();
        let scope = SkillScope::Root(dir.path().to_path_buf());

        for name in ["dirlink", "filelink"] {
            let err = read_skill(&scope, name, MAX_SKILL_BYTES).unwrap_err();
            assert_eq!(err.code(), "skill_outside_root", "{name}");
        }
        let listing = list_skills(&scope, DEFAULT_SKILLS).unwrap();
        let names: Vec<_> = listing.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["ok"]);
        assert_eq!(listing.skipped_outside_root, 2);

        // A project whose whole .claude/skills is a link out of it.
        let linked = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(linked.path().join(".claude")).unwrap();
        std::os::unix::fs::symlink(outside.path(), linked.path().join(".claude/skills")).unwrap();
        let scope = SkillScope::Root(linked.path().to_path_buf());
        let err = read_skill(&scope, "secret", MAX_SKILL_BYTES).unwrap_err();
        assert_eq!(err.code(), "skill_outside_root");
        let listing = list_skills(&scope, DEFAULT_SKILLS).unwrap();
        assert!(listing.skills.is_empty());
        assert_eq!(listing.skipped_outside_root, 1);

        // The user tier's root is ~/.claude/skills itself, so a linked
        // ~/.claude (a dotfiles checkout) reads, and a skill linked out of
        // that directory is still refused.
        let home = tempfile::TempDir::new().unwrap();
        let dotfiles = tempfile::TempDir::new().unwrap();
        skill(&dotfiles.path().join("skills"), "mine", "mine\n");
        std::os::unix::fs::symlink(
            outside.path().join("secret"),
            dotfiles.path().join("skills/away"),
        )
        .unwrap();
        std::os::unix::fs::symlink(dotfiles.path(), home.path().join(".claude")).unwrap();
        let scope = SkillScope::User {
            home: home.path().to_path_buf(),
        };
        assert_eq!(read_skill(&scope, "mine", 100).unwrap().content, "mine\n");
        let err = read_skill(&scope, "away", MAX_SKILL_BYTES).unwrap_err();
        assert_eq!(err.code(), "skill_outside_root");
    }

    /// `e` semantics: a rename between `<name>/` and `<name>.disabled/`,
    /// idempotent for an explicit state, refused when both exist.
    #[cfg(unix)]
    #[test]
    fn set_skill_enabled_renames_like_the_tui_toggle() {
        let home = tempfile::TempDir::new().unwrap();
        let skills = home.path().join(".claude/skills");
        skill(&skills, "demo", "demo\n");
        let scope = SkillScope::User {
            home: home.path().to_path_buf(),
        };
        let off = set_skill_enabled(&scope, "demo", false).unwrap();
        assert!(!off.enabled && off.changed);
        assert!(skills.join("demo.disabled/SKILL.md").is_file());
        assert!(off.path.ends_with("demo.disabled"));
        let again = set_skill_enabled(&scope, "demo", false).unwrap();
        assert!(!again.enabled && !again.changed);
        let flipped = toggle_skill(&scope, "demo").unwrap();
        assert!(flipped.enabled && flipped.changed);
        assert!(skills.join("demo/SKILL.md").is_file());

        skill(&skills, "demo.disabled", "twin\n");
        let err = set_skill_enabled(&scope, "demo", false).unwrap_err();
        assert_eq!(err.code(), "skill_conflict");
        assert!(skills.join("demo/SKILL.md").is_file());
        assert_eq!(
            std::fs::read_to_string(skills.join("demo.disabled/SKILL.md")).unwrap(),
            "twin\n"
        );
        assert_eq!(
            set_skill_enabled(&scope, "nope", true).unwrap_err().code(),
            "skill_not_found"
        );
        for bad in ["", "..", "a/b", "../demo"] {
            assert_eq!(
                set_skill_enabled(&scope, bad, true).unwrap_err().code(),
                "skill_invalid_name",
                "{bad:?}"
            );
        }
    }

    /// `d` semantics (local #91): the enabled directory, else the disabled
    /// one, moved whole into a fresh batch of the skill trash with an origin
    /// sidecar; links inside move as links and their targets are kept.
    #[cfg(unix)]
    #[test]
    fn delete_skill_moves_the_tree_to_the_trash_without_following_links() {
        let home = tempfile::TempDir::new().unwrap();
        let data = tempfile::TempDir::new().unwrap();
        let trash = skill_trash_root(data.path());
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("keep.txt"), "keep\n").unwrap();
        let skills = home.path().join(".claude/skills");
        skill(&skills, "demo", "demo\n");
        std::fs::create_dir_all(skills.join("demo/nested/deeper")).unwrap();
        std::fs::write(skills.join("demo/nested/deeper/x.md"), "x").unwrap();
        std::os::unix::fs::symlink(outside.path(), skills.join("demo/nested/out")).unwrap();
        skill(&skills, "parked.disabled", "parked\n");
        let scope = SkillScope::User {
            home: home.path().to_path_buf(),
        };
        let gone = delete_skill(&scope, "demo", &trash).unwrap();
        assert!(gone.enabled);
        assert!(gone.path.ends_with("demo"));
        assert!(std::fs::symlink_metadata(skills.join("demo")).is_err());
        let kept = PathBuf::from(&gone.trash_path);
        assert_eq!(kept.file_name().unwrap(), "demo");
        let batch = kept.parent().unwrap();
        assert_eq!(batch.parent().unwrap(), trash);
        assert_eq!(
            std::fs::read_to_string(kept.join("SKILL.md")).unwrap(),
            "demo\n"
        );
        assert_eq!(
            std::fs::read_to_string(kept.join("nested/deeper/x.md")).unwrap(),
            "x"
        );
        assert_eq!(
            std::fs::read_link(kept.join("nested/out")).unwrap(),
            outside.path()
        );
        assert_eq!(
            std::fs::read_to_string(batch.with_extension("origin")).unwrap(),
            gone.path
        );
        assert_eq!(
            std::fs::read_to_string(outside.path().join("keep.txt")).unwrap(),
            "keep\n"
        );

        let parked = delete_skill(&scope, "parked", &trash).unwrap();
        assert!(!parked.enabled);
        assert!(std::fs::symlink_metadata(skills.join("parked.disabled")).is_err());
        assert!(PathBuf::from(&parked.trash_path).join("SKILL.md").is_file());
        assert!(parked.trash_path.ends_with("parked.disabled"));
        assert_eq!(
            delete_skill(&scope, "parked", &trash).unwrap_err().code(),
            "skill_not_found"
        );
        // Two batches and their two sidecars, nothing else.
        assert_eq!(std::fs::read_dir(&trash).unwrap().count(), 4);
    }

    /// A skill entry that is a link (in or out of the root), a file or a FIFO
    /// is refused by enable, disable and delete; nothing is touched.
    #[cfg(unix)]
    #[test]
    fn skill_writes_refuse_links_and_non_directory_entries() {
        let home = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        skill(outside.path(), "secret", "secret\n");
        let skills = home.path().join(".claude/skills");
        skill(&skills, "real", "real\n");
        std::os::unix::fs::symlink(outside.path().join("secret"), skills.join("away")).unwrap();
        std::os::unix::fs::symlink(skills.join("real"), skills.join("inroot")).unwrap();
        std::fs::write(skills.join("plain"), "file").unwrap();
        let mkfifo = std::process::Command::new("mkfifo")
            .arg(skills.join("pipe"))
            .status()
            .unwrap();
        assert!(mkfifo.success());
        let scope = SkillScope::User {
            home: home.path().to_path_buf(),
        };
        for name in ["away", "inroot", "plain", "pipe"] {
            for err in [
                set_skill_enabled(&scope, name, false).unwrap_err(),
                toggle_skill(&scope, name).unwrap_err(),
                delete_skill(&scope, name, &home.path().join("trash")).unwrap_err(),
            ] {
                assert_eq!(err.code(), "skill_not_directory", "{name}");
            }
            assert!(
                std::fs::symlink_metadata(skills.join(name)).is_ok(),
                "{name}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(outside.path().join("secret/SKILL.md")).unwrap(),
            "secret\n"
        );
        assert!(skills.join("real/SKILL.md").is_file());

        // A project's .claude/skills linked out of the project is refused.
        let linked = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(linked.path().join(".claude")).unwrap();
        std::os::unix::fs::symlink(outside.path(), linked.path().join(".claude/skills")).unwrap();
        let scope = SkillScope::Root(linked.path().to_path_buf());
        assert_eq!(
            delete_skill(&scope, "secret", &home.path().join("trash"))
                .unwrap_err()
                .code(),
            "skill_outside_root"
        );
        assert!(outside.path().join("secret/SKILL.md").is_file());
    }
}
