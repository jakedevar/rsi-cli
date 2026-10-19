//! Read-only, project-root-scoped file reads (hub #3012, slice 10A of #3000).
//!
//! The four operator-only RPCs in `rpc/project_files.rs` resolve a root (a
//! project's `path` or a session's `working_dir`) and call into this module.
//! Nothing in this file writes (the writes are in `write`, hub #3103): git
//! runs with `GIT_OPTIONAL_LOCKS=0` (no index refresh), no fsmonitor, no
//! external diff and no textconv, under a wall deadline and an output cap. Wire types are `rsi_common::project_files`.
//!
//! Path scope: the request path is relative to the canonical root. Any `..`
//! component, an absolute path (a Unix root, a Windows drive or UNC prefix),
//! and a path whose canonical form (symlinks resolved) leaves the root are
//! refused with a typed `project_file_*` code. Root and target are both
//! canonicalized by the platform, so macOS `/tmp` -> `/private/tmp` and
//! Windows `\\?\` verbatim paths compare like for like.

use crate::error::{DaemonError, Result};
use rsi_common::project_files::{
    BINARY_SNIFF_BYTES, DEFAULT_DIRECTORY_ENTRIES, DEFAULT_PICKER_PATHS, DEFAULT_READ_BYTES,
    DIRECTORY_SCAN_LIMIT, GetProjectFileGitGutterResponseV1, GitGutterMarkerKindV1,
    GitGutterMarkerV1, GitGutterStatusV1, ListProjectDirectoryResponseV1,
    ListProjectFilesResponseV1, MAX_DIRECTORY_ENTRIES, MAX_GUTTER_FILE_BYTES, MAX_PICKER_PATHS,
    MAX_READ_BYTES, ProjectDirectoryEntryV1, ProjectEntryKindV1, ProjectFilesSourceV1,
    ReadProjectFileResponseV1,
};
use std::collections::HashSet;
use std::io::{BufRead, Read};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// Wall deadline of one git read.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Most stdout one git read keeps; more is a truncated answer.
const GIT_MAX_OUTPUT: usize = 64 << 20;
/// Bytes a line-range read scans before it gives up (then `truncated`).
const READ_SCAN_LIMIT: u64 = 256 << 20;

pub(crate) const OUTSIDE_ROOT: &str = "project_file_outside_root";
pub(crate) const NOT_FOUND: &str = "project_file_not_found";
pub(crate) const NOT_A_DIRECTORY: &str = "project_file_not_a_directory";
pub(crate) const NOT_A_FILE: &str = "project_file_not_a_file";
pub(crate) const INVALID_REQUEST: &str = "project_file_invalid_request";
pub(crate) const UNKNOWN_ROOT: &str = "project_file_unknown_root";

pub(crate) fn refuse(code: &str, detail: impl std::fmt::Display) -> DaemonError {
    DaemonError::InvalidParam(format!("{code}: {detail}"))
}

/// Canonicalize a root directory (project `path` or session `working_dir`).
pub(crate) fn canonical_root(root: &Path) -> Result<PathBuf> {
    rsid_core::path_safety::canonicalize_working_dir(root).map_err(|e| refuse(UNKNOWN_ROOT, e))
}

/// A request path resolved inside `root` (already canonical).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopedPath {
    /// Canonical absolute target (symlinks resolved), inside `root`.
    pub(crate) target: PathBuf,
    /// The request path, lexically normalized, `/`-separated (`""` = root).
    pub(crate) display: String,
    /// The canonical target relative to `root`, `/`-separated (git's view).
    pub(crate) real_relative: String,
}

/// Resolve `requested` inside the canonical `root`, refusing any escape.
pub(crate) fn resolve_in_root(root: &Path, requested: &str) -> Result<ScopedPath> {
    let mut clean = PathBuf::new();
    let mut parts: Vec<String> = Vec::new();
    for component in Path::new(requested).components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                return Err(refuse(
                    OUTSIDE_ROOT,
                    format!("'{requested}' is absolute; name a path relative to the root"),
                ));
            }
            Component::ParentDir => {
                return Err(refuse(
                    OUTSIDE_ROOT,
                    format!("'{requested}' has a '..' component"),
                ));
            }
            Component::CurDir => {}
            Component::Normal(part) => {
                clean.push(part);
                parts.push(part.to_string_lossy().into_owned());
            }
        }
    }
    let joined = root.join(&clean);
    let target = match joined.canonicalize() {
        Ok(target) => target,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(refuse(NOT_FOUND, format!("'{requested}' does not exist")));
        }
        Err(error) => return Err(refuse(NOT_FOUND, format!("'{requested}': {error}"))),
    };
    let Ok(inside) = target.strip_prefix(root) else {
        return Err(refuse(
            OUTSIDE_ROOT,
            format!("'{requested}' resolves outside the root"),
        ));
    };
    Ok(ScopedPath {
        display: parts.join("/"),
        real_relative: slash_path(inside),
        target,
    })
}

fn slash_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn join_display(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

fn root_string(root: &Path) -> String {
    root.to_string_lossy().into_owned()
}

fn rfc3339(time: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn clamp(requested: Option<usize>, default: usize, max: usize) -> usize {
    requested.unwrap_or(default).clamp(1, max)
}

// ---------------------------------------------------------------------------
// ListProjectDirectory
// ---------------------------------------------------------------------------

/// One directory's entries, dirs first then by name, capped. `ignored` is
/// filled in by [`list_directory`]; this blocking half never runs git.
#[cfg(test)]
pub(crate) fn list_directory_entries(
    root: &Path,
    requested: &str,
    max_entries: Option<usize>,
) -> Result<ListProjectDirectoryResponseV1> {
    let scope = listing::DirectoryScope::resolve(root, requested)?;
    directory_response(root, &scope, max_entries)
}

fn directory_response(
    root: &Path,
    scope: &listing::DirectoryScope,
    max_entries: Option<usize>,
) -> Result<ListProjectDirectoryResponseV1> {
    let cap = clamp(
        max_entries,
        DEFAULT_DIRECTORY_ENTRIES,
        MAX_DIRECTORY_ENTRIES,
    );
    let (mut entries, mut truncated) = scope.entries(DIRECTORY_SCAN_LIMIT)?;
    entries.sort_by(|a, b| {
        let a_dir = a.kind == ProjectEntryKindV1::Dir;
        let b_dir = b.kind == ProjectEntryKindV1::Dir;
        b_dir
            .cmp(&a_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    if entries.len() > cap {
        entries.truncate(cap);
        truncated = true;
    }
    Ok(ListProjectDirectoryResponseV1 {
        root: root_string(root),
        path: scope.scoped.display.clone(),
        entries,
        truncated,
    })
}

/// One directory's entries plus git's ignore verdict for each entry.
pub(crate) async fn list_directory(
    root: PathBuf,
    requested: String,
    max_entries: Option<usize>,
) -> Result<ListProjectDirectoryResponseV1> {
    let blocking_root = root.clone();
    #[cfg(test)]
    let hook = listing::take_resolve_hook();
    let (scope, mut listing) = tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        listing::install_resolve_hook(hook);
        let scope = listing::DirectoryScope::resolve(&blocking_root, &requested)?;
        let listing = directory_response(&blocking_root, &scope, max_entries)?;
        Ok::<_, DaemonError>((scope, listing))
    })
    .await
    .map_err(|e| DaemonError::Process(e.to_string()))??;
    if listing.entries.is_empty() {
        return Ok(listing);
    }
    // Directory entries are sent with a trailing `/` so a dir-only pattern
    // (`target/`) matches; the answer is mapped back to the entry.
    let mut stdin = Vec::new();
    for entry in &listing.entries {
        stdin.extend_from_slice(entry.path.as_bytes());
        if entry.kind == ProjectEntryKindV1::Dir {
            stdin.push(b'/');
        }
        stdin.push(0);
    }
    let ignored: HashSet<String> = match scope
        .git(&["check-ignore", "-z", "--stdin"], Some(stdin))
        .await?
    {
        Some(output) if output.code == Some(0) => nul_fields(&output.stdout)
            .map(|path| path.trim_end_matches('/').to_string())
            .collect(),
        _ => HashSet::new(),
    };
    for entry in &mut listing.entries {
        entry.ignored = ignored.contains(&entry.path);
    }
    Ok(listing)
}

// ---------------------------------------------------------------------------
// ListProjectFiles
// ---------------------------------------------------------------------------

/// The picker feed: tracked plus untracked-not-ignored files under
/// `requested` (git), or a `.gitignore`-aware walk outside a git work tree.
pub(crate) async fn list_files(
    root: PathBuf,
    requested: String,
    max_paths: Option<usize>,
) -> Result<ListProjectFilesResponseV1> {
    listing::list_files(root, requested, max_paths).await
}

mod listing;

// ---------------------------------------------------------------------------
// ReadProjectFile
// ---------------------------------------------------------------------------

/// One file as UTF-8 text (capped), or `binary: true` with no content.
pub(crate) fn read_file(
    root: &Path,
    requested: &str,
    max_bytes: Option<u64>,
    start_line: Option<u64>,
    end_line: Option<u64>,
) -> Result<ReadProjectFileResponseV1> {
    if start_line == Some(0) || end_line == Some(0) {
        return Err(refuse(INVALID_REQUEST, "line numbers are 1-based"));
    }
    if let (Some(start), Some(end)) = (start_line, end_line)
        && end < start
    {
        return Err(refuse(INVALID_REQUEST, "end_line is before start_line"));
    }
    let cap = max_bytes
        .unwrap_or(DEFAULT_READ_BYTES)
        .clamp(1, MAX_READ_BYTES);
    let scope = FileReadScope::resolve(root, requested)?;
    let mut file = scope.open()?;
    let metadata = file.metadata()?;
    let scoped = scope.scoped;
    let size = metadata.len();
    let mut response = ReadProjectFileResponseV1 {
        root: root_string(root),
        path: scoped.display.clone(),
        size,
        binary: false,
        content: None,
        truncated: false,
        start_line: None,
        end_line: None,
        version: None,
    };
    // hub #3103: the whole file's version, the token a save passes back.
    // hub #3146: it is dropped below if the file changed while it was read,
    // so a token always describes the bytes answered with it.
    response.version = write::file_version(&mut file)?;
    let mut response = read_content(file, response, cap, start_line, end_line)?;
    if !write::same_snapshot(&metadata, &response.1.metadata()?) {
        response.0.version = None;
    }
    Ok(response.0)
}

/// Keep the root pinned before pathname resolution. Canonicalization preserves
/// contained symlinks; the actual open walks the canonical relative path with
/// NOFOLLOW on every component. Only metadata from the opened file is trusted.
struct FileReadScope {
    root: rsi_common::asset_fs::AssetDir,
    root_path: PathBuf,
    scoped: ScopedPath,
}

impl FileReadScope {
    fn resolve(root: &Path, requested: &str) -> Result<Self> {
        let pinned = rsi_common::asset_fs::AssetDir::open(root)
            .map_err(|error| file_open_refusal(requested, error))?;
        Ok(Self {
            root: pinned,
            root_path: root.to_path_buf(),
            scoped: resolve_in_root(root, requested)?,
        })
    }

    fn open(&self) -> Result<std::fs::File> {
        let relative = self
            .scoped
            .target
            .strip_prefix(&self.root_path)
            .map_err(|_| refuse(OUTSIDE_ROOT, "file escaped its pinned root"))?;
        if relative.as_os_str().is_empty() {
            return Err(refuse(NOT_A_FILE, "the root is not a regular file"));
        }
        #[cfg(test)]
        read_safety_tests::before_open();
        self.root
            .read_file(relative)
            .map_err(|error| file_open_refusal(&self.scoped.display, error))
    }
}

fn file_open_refusal(requested: &str, error: std::io::Error) -> DaemonError {
    let code = match error.kind() {
        std::io::ErrorKind::Unsupported => write::UNSUPPORTED,
        std::io::ErrorKind::InvalidInput => NOT_A_FILE,
        std::io::ErrorKind::NotFound => NOT_FOUND,
        _ => write::IO_FAILED,
    };
    refuse(code, format!("'{requested}': {error}"))
}

/// The gutter's initial byte inspection uses exactly the same open as reads.
fn inspect_gutter_file(root: &Path, requested: &str) -> Result<(FileReadScope, u64, bool)> {
    let scope = FileReadScope::resolve(root, requested)?;
    let mut file = scope.open()?;
    let size = file.metadata()?.len();
    let binary = sniff_binary(&mut file)?;
    Ok((scope, size, binary))
}

/// The content part of [`read_file`]; answers the file back for its final
/// stat.
fn read_content(
    mut file: std::fs::File,
    mut response: ReadProjectFileResponseV1,
    cap: u64,
    start_line: Option<u64>,
    end_line: Option<u64>,
) -> Result<(ReadProjectFileResponseV1, std::fs::File)> {
    if sniff_binary(&mut file)? {
        response.binary = true;
        return Ok((response, file));
    }
    if start_line.is_none() && end_line.is_none() {
        let mut bytes = Vec::new();
        (&mut file).take(cap + 1).read_to_end(&mut bytes)?;
        let truncated = bytes.len() as u64 > cap;
        bytes.truncate(cap as usize);
        let Some(text) = utf8_prefix(bytes, truncated) else {
            response.binary = true;
            return Ok((response, file));
        };
        if !text.is_empty() {
            response.start_line = Some(1);
            response.end_line = Some(text.lines().count().max(1) as u64);
        }
        response.content = Some(text);
        response.truncated = truncated;
        return Ok((response, file));
    }
    let start = start_line.unwrap_or(1);
    let end = end_line.unwrap_or(u64::MAX);
    let mut reader = std::io::BufReader::new((&mut file).take(READ_SCAN_LIMIT));
    let mut content: Vec<u8> = Vec::new();
    let mut line_number = 0u64;
    let mut first = None;
    let mut last = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            if reader.get_ref().limit() == 0 {
                // The scan limit, not the end of the file, stopped the read.
                response.truncated = true;
            }
            break;
        }
        line_number += 1;
        if line_number < start {
            continue;
        }
        if line_number > end {
            break;
        }
        let room = cap - content.len() as u64;
        if line.len() as u64 > room {
            // A first line longer than the cap is returned partially; a
            // later one ends the content at the previous line boundary.
            if first.is_none() {
                content.extend_from_slice(&line[..room as usize]);
                first = Some(line_number);
                last = Some(line_number);
            }
            response.truncated = true;
            break;
        }
        content.extend_from_slice(&line);
        first.get_or_insert(line_number);
        last = Some(line_number);
    }
    drop(reader);
    let Some(text) = utf8_prefix(content, response.truncated) else {
        response.binary = true;
        response.truncated = false;
        return Ok((response, file));
    };
    response.content = Some(text);
    response.start_line = first;
    response.end_line = last;
    Ok((response, file))
}

/// A NUL byte in the first [`BINARY_SNIFF_BYTES`] calls the file binary. The
/// file position is rewound.
fn sniff_binary(file: &mut std::fs::File) -> std::io::Result<bool> {
    use std::io::Seek;
    let mut head = Vec::with_capacity(BINARY_SNIFF_BYTES);
    (&mut *file)
        .take(BINARY_SNIFF_BYTES as u64)
        .read_to_end(&mut head)?;
    file.seek(std::io::SeekFrom::Start(0))?;
    Ok(head.contains(&0))
}

/// The bytes as UTF-8; a cut-short read may end inside a multi-byte
/// character, which is dropped. Any other invalid byte means not text.
fn utf8_prefix(mut bytes: Vec<u8>, cut_short: bool) -> Option<String> {
    match std::str::from_utf8(&bytes) {
        Ok(_) => Some(String::from_utf8(bytes).ok()?),
        Err(error) if cut_short && error.error_len().is_none() => {
            bytes.truncate(error.valid_up_to());
            String::from_utf8(bytes).ok()
        }
        Err(_) => None,
    }
}

// ---------------------------------------------------------------------------
// GetProjectFileGitGutter
// ---------------------------------------------------------------------------

/// Per-line worktree-vs-`HEAD` markers for one file.
pub(crate) async fn git_gutter(
    root: PathBuf,
    requested: String,
) -> Result<GetProjectFileGitGutterResponseV1> {
    let scope_root = root.clone();
    let (scope, size, binary_head) =
        tokio::task::spawn_blocking(move || inspect_gutter_file(&scope_root, &requested))
            .await
            .map_err(|e| DaemonError::Process(e.to_string()))??;
    let scoped = &scope.scoped;
    let mut response = GetProjectFileGitGutterResponseV1 {
        root: root_string(&root),
        path: scoped.display.clone(),
        status: GitGutterStatusV1::NotARepo,
        markers: Vec::new(),
    };
    if size > MAX_GUTTER_FILE_BYTES {
        response.status = GitGutterStatusV1::TooLarge;
        return Ok(response);
    }
    if !inside_work_tree(&root).await {
        return Ok(response);
    }
    let relative = scoped.real_relative.clone();
    let in_head = run_git_by_path(
        &root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("HEAD:./{relative}"),
        ],
        None,
    )
    .await
    .is_some_and(|output| output.code == Some(0));
    if !in_head {
        let ignored = run_git_by_path(&root, &["check-ignore", "-q", "--", &relative], None)
            .await
            .is_some_and(|output| output.code == Some(0));
        if ignored {
            response.status = GitGutterStatusV1::Ignored;
            return Ok(response);
        }
        if binary_head {
            response.status = GitGutterStatusV1::Binary;
            return Ok(response);
        }
        let lines = tokio::task::spawn_blocking(move || count_lines(&scope))
            .await
            .map_err(|e| DaemonError::Process(e.to_string()))??;
        response.status = GitGutterStatusV1::Untracked;
        response.markers = (1..=lines)
            .map(|line| GitGutterMarkerV1 {
                line,
                kind: GitGutterMarkerKindV1::Added,
            })
            .collect();
        return Ok(response);
    }
    let pathspec = format!(":(literal){relative}");
    let Some(output) = run_git_by_path(
        &root,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "-U0",
            "HEAD",
            "--",
            &pathspec,
        ],
        None,
    )
    .await
    .filter(|output| output.code == Some(0) && !output.truncated) else {
        return Err(DaemonError::Process(format!(
            "git diff for '{}' did not complete",
            scoped.display
        )));
    };
    let diff = String::from_utf8_lossy(&output.stdout);
    let markers = parse_unified_zero(&diff);
    response.status = if markers.is_empty() && diff.lines().any(|l| l.starts_with("Binary files "))
    {
        GitGutterStatusV1::Binary
    } else {
        GitGutterStatusV1::Tracked
    };
    response.markers = markers;
    Ok(response)
}

fn count_lines(scope: &FileReadScope) -> Result<u64> {
    let file = scope.open()?;
    let mut reader = std::io::BufReader::new(file.take(MAX_GUTTER_FILE_BYTES));
    let mut count = 0u64;
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? > 0 {
        count += 1;
        line.clear();
    }
    Ok(count)
}

/// Gutter markers from a `git diff -U0` body. Each hunk `@@ -a,b +c,d @@`:
/// `b == 0` adds lines `c..c+d`; `d == 0` removes after line `c`; otherwise
/// the overlap is modified, extra new lines are added and extra old lines are
/// a removal after the last modified line.
pub(crate) fn parse_unified_zero(diff: &str) -> Vec<GitGutterMarkerV1> {
    let mut markers = Vec::new();
    for line in diff.lines() {
        let Some(header) = line.strip_prefix("@@ -") else {
            continue;
        };
        let Some((old, rest)) = header.split_once(" +") else {
            continue;
        };
        let Some((new, _)) = rest.split_once(" @@") else {
            continue;
        };
        let (Some((_, old_count)), Some((new_start, new_count))) = (range(old), range(new)) else {
            continue;
        };
        let push = |markers: &mut Vec<GitGutterMarkerV1>, line, kind| {
            markers.push(GitGutterMarkerV1 { line, kind });
        };
        if old_count == 0 {
            for line in new_start..new_start + new_count {
                push(&mut markers, line, GitGutterMarkerKindV1::Added);
            }
        } else if new_count == 0 {
            push(&mut markers, new_start, GitGutterMarkerKindV1::Removed);
        } else {
            let overlap = old_count.min(new_count);
            for line in new_start..new_start + overlap {
                push(&mut markers, line, GitGutterMarkerKindV1::Modified);
            }
            for line in new_start + overlap..new_start + new_count {
                push(&mut markers, line, GitGutterMarkerKindV1::Added);
            }
            if old_count > new_count {
                push(
                    &mut markers,
                    new_start + new_count - 1,
                    GitGutterMarkerKindV1::Removed,
                );
            }
        }
    }
    markers
}

/// `start[,count]` with git's default count of 1.
fn range(text: &str) -> Option<(u64, u64)> {
    match text.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((text.parse().ok()?, 1)),
    }
}

// ---------------------------------------------------------------------------
// Bounded read-only git
// ---------------------------------------------------------------------------

struct GitOutput {
    code: Option<i32>,
    stdout: Vec<u8>,
    /// Stdout passed [`GIT_MAX_OUTPUT`]; the process was killed.
    truncated: bool,
}

fn nul_fields(bytes: &[u8]) -> impl Iterator<Item = &str> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .filter_map(|field| std::str::from_utf8(field).ok())
}

/// Whether `root` is in a git work tree that tracks it. A root inside a
/// directory its enclosing repository ignores (a scratch dir under `target/`)
/// is not: git would answer for files the user never versioned there.
async fn inside_work_tree(root: &Path) -> bool {
    let Some(output) = run_git_by_path(root, &["rev-parse", "--show-toplevel"], None).await else {
        return false;
    };
    if output.code != Some(0) {
        return false;
    }
    let top = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let Ok(top) = Path::new(&top).canonicalize() else {
        return false;
    };
    let Ok(inside) = root.strip_prefix(&top) else {
        return false;
    };
    if inside.as_os_str().is_empty() {
        return true;
    }
    let directory = format!("{}/", slash_path(inside));
    !run_git_by_path(&top, &["check-ignore", "-q", "--", &directory], None)
        .await
        .is_some_and(|output| output.code == Some(0))
}

/// Run read-only Git using the pathname cwd for existing gutter callers.
async fn run_git_by_path(root: &Path, args: &[&str], stdin: Option<Vec<u8>>) -> Option<GitOutput> {
    run_git(root, args, stdin, None).await
}

/// Registered read-only Git spawn, optionally using a pinned directory cwd.
/// Returns `None` if startup fails or [`GIT_TIMEOUT`] expires.
async fn run_git(
    root: &Path,
    args: &[&str],
    stdin: Option<Vec<u8>>,
    directory: Option<std::fs::File>,
) -> Option<GitOutput> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(if directory.is_some() {
            Path::new(".")
        } else {
            root
        })
        .args(["-c", "core.fsmonitor=false", "-c", "core.quotepath=off"])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(directory) = directory {
        #[cfg(unix)]
        // SAFETY: fchdir is async-signal-safe; the owned descriptor stays live
        // in the closure until exec. No allocation or locking in the child.
        unsafe {
            command.pre_exec(move || {
                rustix::process::fchdir(&directory).map_err(std::io::Error::from)
            });
        }
        #[cfg(not(unix))]
        {
            let _ = directory;
            return None;
        }
    }
    let run = async move {
        let mut child = command.spawn().ok()?;
        let writer = match (stdin, child.stdin.take()) {
            (Some(bytes), Some(mut pipe)) => Some(tokio::spawn(async move {
                let _ = pipe.write_all(&bytes).await;
                drop(pipe);
            })),
            _ => None,
        };
        let stdout = child.stdout.take()?;
        let mut bytes = Vec::new();
        stdout
            .take(GIT_MAX_OUTPUT as u64 + 1)
            .read_to_end(&mut bytes)
            .await
            .ok()?;
        let truncated = bytes.len() > GIT_MAX_OUTPUT;
        if truncated {
            bytes.truncate(GIT_MAX_OUTPUT);
            let _ = child.start_kill();
        }
        let status = child.wait().await.ok()?;
        if let Some(writer) = writer {
            let _ = writer.await;
        }
        Some(GitOutput {
            code: status.code(),
            stdout: bytes,
            truncated,
        })
    };
    tokio::time::timeout(GIT_TIMEOUT, run).await.ok().flatten()
}

pub(crate) mod write;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod read_safety_tests;
