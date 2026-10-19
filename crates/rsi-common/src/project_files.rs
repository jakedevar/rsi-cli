//! Read-only, project-root-scoped file RPCs (hub #3012, slice 10A of #3000).
//!
//! Four operator-only methods serve the desktop file explorer, fuzzy file
//! picker and file viewer from the local daemon socket, so the desktop stays a
//! pure `rsid` frontend:
//!
//! - [`METHOD_LIST_DIRECTORY`]: one directory's entries, dirs first.
//! - [`METHOD_LIST_FILES`]: the picker feed (tracked plus untracked, not ignored).
//! - [`METHOD_READ_FILE`]: one file as UTF-8 text, capped, or `binary: true`.
//! - [`METHOD_GIT_GUTTER`]: per-line worktree-vs-`HEAD` markers for one file.
//!
//! Every request names its root with exactly one of `project_id` (the
//! project's `path`) or `session_id` (the session's `working_dir`), and a
//! `path` relative to that root. A path that leaves the root (a `..`
//! component, an absolute path, or a symlink resolving outside) is refused.
//! None of these methods is in the attributed verb registry (operator-only,
//! AGENTS.md rule 10) and none is on the rsi-remote gateway.
//!
//! hub #3103 adds the explorer and editor writes, operator-only on the same
//! root resolution: [`METHOD_WRITE_FILE`] (save against the observed
//! [`ReadProjectFileResponseV1::version`], or `force`), [`METHOD_CREATE_ENTRY`]
//! (add a file or mkdir), [`METHOD_MOVE_ENTRY`] (rename/move),
//! [`METHOD_COPY_ENTRY`], [`METHOD_TRASH_ENTRY`] (delete into a daemon-owned
//! trash under `~/.rsi`) and [`METHOD_UNDO`] (reverse the root's last
//! create, move, copy or trash). Reload is [`METHOD_READ_FILE`] again: it
//! answers the on-disk content and its current `version`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const METHOD_LIST_DIRECTORY: &str = "ListProjectDirectory";
pub const METHOD_LIST_FILES: &str = "ListProjectFiles";
pub const METHOD_READ_FILE: &str = "ReadProjectFile";
pub const METHOD_GIT_GUTTER: &str = "GetProjectFileGitGutter";

pub const METHOD_WRITE_FILE: &str = "WriteProjectFile";
pub const METHOD_CREATE_ENTRY: &str = "CreateProjectEntry";
pub const METHOD_MOVE_ENTRY: &str = "MoveProjectEntry";
pub const METHOD_COPY_ENTRY: &str = "CopyProjectEntry";
pub const METHOD_TRASH_ENTRY: &str = "TrashProjectEntry";
pub const METHOD_UNDO: &str = "UndoProjectFileOperation";

/// The project-file methods that change the filesystem (hub #3103).
pub const PROJECT_FILE_WRITE_METHODS: &[&str] = &[
    METHOD_WRITE_FILE,
    METHOD_CREATE_ENTRY,
    METHOD_MOVE_ENTRY,
    METHOD_COPY_ENTRY,
    METHOD_TRASH_ENTRY,
    METHOD_UNDO,
];

/// Every project-file method, for catalog tests.
pub const PROJECT_FILE_METHODS: &[&str] = &[
    METHOD_LIST_DIRECTORY,
    METHOD_LIST_FILES,
    METHOD_READ_FILE,
    METHOD_GIT_GUTTER,
    METHOD_WRITE_FILE,
    METHOD_CREATE_ENTRY,
    METHOD_MOVE_ENTRY,
    METHOD_COPY_ENTRY,
    METHOD_TRASH_ENTRY,
    METHOD_UNDO,
];

/// Default and ceiling of `ListProjectDirectory.max_entries`.
pub const DEFAULT_DIRECTORY_ENTRIES: usize = 2_000;
pub const MAX_DIRECTORY_ENTRIES: usize = 10_000;
/// Directory entries read before a listing stops scanning (then `truncated`).
pub const DIRECTORY_SCAN_LIMIT: usize = 50_000;
/// Default and ceiling of `ListProjectFiles.max_paths`.
pub const DEFAULT_PICKER_PATHS: usize = 20_000;
pub const MAX_PICKER_PATHS: usize = 100_000;
/// Default and ceiling of `ReadProjectFile.max_bytes`.
pub const DEFAULT_READ_BYTES: u64 = 1 << 20;
pub const MAX_READ_BYTES: u64 = 4 << 20;
/// Bytes sniffed for a NUL to call a file binary.
pub const BINARY_SNIFF_BYTES: usize = 8 << 10;
/// Largest file the git gutter diffs; a larger one answers `too_large`.
pub const MAX_GUTTER_FILE_BYTES: u64 = 4 << 20;
/// Largest file that carries a `version` (its bytes are hashed); a larger one
/// can only be saved with `force`.
pub const MAX_VERSIONED_FILE_BYTES: u64 = 64 << 20;
/// Largest `WriteProjectFile.content`, in UTF-8 bytes.
pub const MAX_WRITE_BYTES: usize = 8 << 20;
/// Undoable operations the daemon remembers per root (oldest dropped first).
pub const UNDO_DEPTH: usize = 64;

/// `ListProjectDirectory` request. `path` `""` or `"."` is the root itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListProjectDirectoryRequestV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub path: String,
    /// Default [`DEFAULT_DIRECTORY_ENTRIES`], clamped to 1..=[`MAX_DIRECTORY_ENTRIES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entries: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectEntryKindV1 {
    File,
    Dir,
    Symlink,
}

/// One directory entry. `size` and `mtime` are those of the entry itself (a
/// symlink is not followed); `mtime` is RFC3339 UTC with nanoseconds, absent
/// when the platform cannot say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectDirectoryEntryV1 {
    pub name: String,
    /// Root-relative path with `/` separators.
    pub path: String,
    pub kind: ProjectEntryKindV1,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<String>,
    /// The name starts with `.`.
    pub hidden: bool,
    /// Git ignores it (always `false` outside a git work tree).
    pub ignored: bool,
}

/// `ListProjectDirectory` response: dirs first, then by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListProjectDirectoryResponseV1 {
    /// Canonical absolute root the request resolved to.
    pub root: String,
    /// Root-relative path of the listed directory (`""` for the root).
    pub path: String,
    pub entries: Vec<ProjectDirectoryEntryV1>,
    /// More entries exist than were returned.
    pub truncated: bool,
}

/// `ListProjectFiles` request: the picker feed under `path` (default the root).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListProjectFilesRequestV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub path: String,
    /// Default [`DEFAULT_PICKER_PATHS`], clamped to 1..=[`MAX_PICKER_PATHS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_paths: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectFilesSourceV1 {
    /// `git ls-files --cached --others --exclude-standard`.
    Git,
    /// Not a git work tree: a `.gitignore`-aware directory walk.
    Walk,
}

/// `ListProjectFiles` response: root-relative paths with `/` separators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListProjectFilesResponseV1 {
    pub root: String,
    pub paths: Vec<String>,
    pub truncated: bool,
    pub source: ProjectFilesSourceV1,
}

/// `ReadProjectFile` request. `start_line`/`end_line` are 1-based and
/// inclusive; either may be omitted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadProjectFileRequestV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    pub path: String,
    /// Default [`DEFAULT_READ_BYTES`], clamped to 1..=[`MAX_READ_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u64>,
}

/// `ReadProjectFile` response. A binary file has `binary: true` and no
/// `content`. `start_line`/`end_line` name the lines `content` holds (absent
/// when it holds none); `truncated` means the byte cap or the line range cut
/// the file short.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadProjectFileResponseV1 {
    pub root: String,
    pub path: String,
    pub size: u64,
    pub binary: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u64>,
    /// The whole file's version (hub #3103): an opaque token,
    /// `v1:<size>:<mtime unix nanos>:<sha256 hex>`, to pass back as
    /// `WriteProjectFile.expected_version`. Absent for a file larger than
    /// [`MAX_VERSIONED_FILE_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// `GetProjectFileGitGutter` request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetProjectFileGitGutterRequestV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitGutterStatusV1 {
    /// Diffed against `HEAD`; `markers` may be empty (unchanged).
    Tracked,
    /// Not in `HEAD` (untracked, newly added, or no `HEAD` yet): every line `added`.
    Untracked,
    /// Git ignores the file: no markers.
    Ignored,
    /// Git calls the change binary: no markers.
    Binary,
    /// Larger than [`MAX_GUTTER_FILE_BYTES`]: no markers.
    TooLarge,
    /// The root is not in a git work tree: no markers.
    NotARepo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitGutterMarkerKindV1 {
    Added,
    Modified,
    /// Lines were removed after `line` (`0`: before the first line).
    Removed,
}

/// One gutter marker on a 1-based worktree line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitGutterMarkerV1 {
    pub line: u64,
    pub kind: GitGutterMarkerKindV1,
}

/// `GetProjectFileGitGutter` response: markers sorted by line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetProjectFileGitGutterResponseV1 {
    pub root: String,
    pub path: String,
    pub status: GitGutterStatusV1,
    pub markers: Vec<GitGutterMarkerV1>,
}

/// `WriteProjectFile` request (save). The file is replaced atomically (a
/// temporary file in the same directory, then a rename) and keeps its mode.
/// An existing file is saved only while its version equals
/// `expected_version`; otherwise the save is refused `project_file_stale`
/// unless `force` is true. A missing file is created only with `force` (its
/// directory must exist): `CreateProjectEntry` is the way to add one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteProjectFileRequestV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    pub path: String,
    /// The new content, at most [`MAX_WRITE_BYTES`].
    pub content: String,
    /// `ReadProjectFileResponseV1.version` the edit started from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<String>,
    /// Save even though the file changed (or vanished) on disk.
    #[serde(default)]
    pub force: bool,
}

/// `WriteProjectFile` response: the saved file's new version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteProjectFileResponseV1 {
    pub root: String,
    pub path: String,
    pub size: u64,
    pub version: String,
    /// The save replaced content whose version was not `expected_version`
    /// (only with `force`).
    pub forced: bool,
    /// The file did not exist before (only with `force`).
    pub created: bool,
}

/// `CreateProjectEntry` request: add an empty file or a directory, creating
/// missing parents. An existing entry is never overwritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateProjectEntryRequestV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    pub path: String,
    /// `file` or `dir` (`symlink` is refused).
    pub kind: ProjectEntryKindV1,
}

/// `MoveProjectEntry` / `CopyProjectEntry` request. `from` names the entry
/// itself (a symlink is moved or copied as a symlink, never followed); `to`
/// must not exist (never overwritten) and its missing parents are created.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferProjectEntryRequestV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    pub from: String,
    pub to: String,
}

/// `TrashProjectEntry` request: move one entry (not followed) into the
/// daemon-owned trash.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrashProjectEntryRequestV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    pub path: String,
}

/// `UndoProjectFileOperation` request: reverse the root's most recent
/// undoable operation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UndoProjectFileOperationRequestV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
}

/// An undoable explorer operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectFileOperationKindV1 {
    /// `path` (the top-most entry the add made) was created.
    Created,
    /// `path` moved to `to`.
    Moved,
    /// `path` was copied to `to`.
    Copied,
    /// `path` moved into the trash.
    Trashed,
}

/// Response of `CreateProjectEntry`, `MoveProjectEntry`, `CopyProjectEntry`
/// and `TrashProjectEntry`. Paths are root-relative with `/` separators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectFileOperationResponseV1 {
    pub root: String,
    pub op: ProjectFileOperationKindV1,
    /// Created: the requested path; moved/copied: the source; trashed: the
    /// original path.
    pub path: String,
    /// Moved/copied: the destination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Created: the top-most entry that did not exist before (what undo
    /// removes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_top: Option<String>,
    /// Undoable operations now remembered for this root.
    pub undo_depth: usize,
}

/// What undoing an operation did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectFileUndoEffectV1 {
    /// A trashed entry is back at its original path.
    Restored,
    /// A moved entry is back at its source path.
    MovedBack,
    /// An empty created or copied entry was removed.
    Removed,
    /// A non-empty created or copied entry moved into the trash (undo never
    /// destroys content).
    Trashed,
}

/// `UndoProjectFileOperation` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoProjectFileOperationResponseV1 {
    pub root: String,
    /// The operation that was reversed.
    pub undone: ProjectFileOperationKindV1,
    pub effect: ProjectFileUndoEffectV1,
    /// The path the undo acted on (restored or moved-back path, or the
    /// removed/trashed created entry).
    pub path: String,
    /// Moved: the path the entry moved back from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub undo_depth: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_refuse_unknown_fields_and_name_snake_case_kinds() {
        let bad = serde_json::json!({"project_id": Uuid::nil(), "path": "a", "extra": 1});
        assert!(serde_json::from_value::<ReadProjectFileRequestV1>(bad).is_err());
        assert_eq!(
            serde_json::to_value(GitGutterStatusV1::NotARepo).unwrap(),
            serde_json::json!("not_a_repo")
        );
        assert_eq!(
            serde_json::to_value(ProjectEntryKindV1::Symlink).unwrap(),
            serde_json::json!("symlink")
        );
    }
}
