//! Shared plumbing for the native navigation tools: `grep`, `glob` and
//! `list_files` (#2254).
//!
//! All three walk the working tree the same way: gitignore-aware (`.gitignore`,
//! `.ignore` and `.git/info/exclude`, never the operator's global ignore file,
//! so results do not depend on whose machine runs the session), hidden files
//! included, the `.git` directory skipped, symlinks never followed, entries in
//! path order. The walk is blocking, so it runs on a blocking thread under a
//! time budget that also stops when the tool call is cancelled or dropped.

use super::is_system_blocked;
use crate::paths::resolve_sandboxed_path;
use crate::types::ToolResult;
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Wall-clock cap for one search or listing.
pub(super) const SEARCH_TIME_LIMIT: Duration = Duration::from_secs(20);

pub(super) fn failure(message: impl Into<String>) -> ToolResult {
    ToolResult {
        success: false,
        output: String::new(),
        error_msg: Some(message.into()),
    }
}

/// A search root resolved inside the sandbox.
#[derive(Debug)]
pub(super) struct SearchScope {
    /// Canonical path of the file or directory to walk.
    pub root: PathBuf,
    /// Canonical working directory; result paths are shown relative to it.
    pub base: PathBuf,
}

impl SearchScope {
    /// Resolve `path_arg` (empty or `.` means the working directory) inside
    /// `working_dir`, refusing escapes, system paths and missing targets.
    pub(super) fn resolve(working_dir: &Path, path_arg: &str) -> Result<Self, ToolResult> {
        let base = working_dir
            .canonicalize()
            .map_err(|error| failure(format!("Cannot resolve working directory: {error}")))?;
        let root = if path_arg.is_empty() || path_arg == "." {
            base.clone()
        } else {
            resolve_sandboxed_path(working_dir, path_arg)
                .map_err(|error| failure(format!("Invalid path: {error}")))?
        };
        if is_system_blocked(&root) {
            return Err(failure(format!(
                "Access to {} is restricted by system blocklist.",
                root.display()
            )));
        }
        if !root.exists() {
            return Err(failure(format!("Path not found: {path_arg}")));
        }
        Ok(Self { root, base })
    }

    /// `path` relative to the working directory, `/`-separated.
    pub(super) fn display(&self, path: &Path) -> String {
        let relative = path.strip_prefix(&self.base).unwrap_or(path);
        relative
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/")
    }
}

/// The walker every navigation tool shares; see the module comment.
pub(super) fn walker(root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_global(false)
        .follow_links(false)
        .sort_by_file_path(|left, right| left.cmp(right))
        .filter_entry(|entry| entry.file_name() != ".git");
    builder
}

/// Why a blocking walk stopped before it finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StopReason {
    TimedOut,
    Cancelled,
}

/// Time and cancellation budget handed to a blocking walk.
pub(super) struct Budget {
    started: Instant,
    limit: Duration,
    dropped: Arc<AtomicBool>,
    cancel: CancellationToken,
}

impl Budget {
    /// `Some` once the walk should stop.
    pub(super) fn stop_reason(&self) -> Option<StopReason> {
        if self.dropped.load(Ordering::Relaxed) || self.cancel.is_cancelled() {
            Some(StopReason::Cancelled)
        } else if self.started.elapsed() >= self.limit {
            Some(StopReason::TimedOut)
        } else {
            None
        }
    }
}

/// Marks the budget dropped when the awaiting tool future goes away, so a
/// cancelled call does not leave a walk running.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Run a blocking walk under [`SEARCH_TIME_LIMIT`] and the call's cancel token.
pub(super) async fn run_blocking<T, F>(cancel: &CancellationToken, work: F) -> Result<T, ToolResult>
where
    T: Send + 'static,
    F: FnOnce(&Budget) -> T + Send + 'static,
{
    let dropped = Arc::new(AtomicBool::new(false));
    let _guard = StopOnDrop(Arc::clone(&dropped));
    let budget = Budget {
        started: Instant::now(),
        limit: SEARCH_TIME_LIMIT,
        dropped,
        cancel: cancel.clone(),
    };
    tokio::task::spawn_blocking(move || work(&budget))
        .await
        .map_err(|error| failure(format!("search task failed: {error}")))
}

/// A file glob: a pattern without `/` matches the file name at any depth
/// (`*.rs`), one with `/` matches the path relative to the search root
/// (`src/**/*.ts`), the way `rg --glob` does.
pub(super) struct FileGlob {
    matcher: globset::GlobMatcher,
    by_name: bool,
}

impl FileGlob {
    pub(super) fn new(pattern: &str) -> Result<Self, String> {
        let pattern = pattern.trim().trim_start_matches("./");
        if pattern.is_empty() {
            return Err("glob must not be empty".into());
        }
        let matcher = globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|error| format!("invalid glob '{pattern}': {error}"))?
            .compile_matcher();
        Ok(Self {
            matcher,
            by_name: !pattern.contains('/'),
        })
    }

    /// Match a file by its name and its path relative to the search root.
    pub(super) fn is_match(&self, relative: &Path) -> bool {
        if self.by_name {
            relative
                .file_name()
                .is_some_and(|name| self.matcher.is_match(Path::new(name)))
        } else {
            self.matcher.is_match(relative)
        }
    }
}

/// Parse an optional non-negative integer argument.
pub(super) fn optional_uint(args: &serde_json::Value, key: &str) -> Result<Option<usize>, String> {
    let Some(value) = args.get(key).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let parsed = value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|number| number.fract() == 0.0 && *number >= 0.0)
            .map(|number| number as u64)
    });
    parsed
        .map(|number| usize::try_from(number).unwrap_or(usize::MAX))
        .map(Some)
        .ok_or_else(|| format!("invalid_arguments: {key} must be a non-negative integer"))
}

/// Cut `text` to at most `max_bytes` at a character boundary. Returns the kept
/// text and the number of bytes dropped.
pub(super) fn clip_at_char_boundary(text: &str, max_bytes: usize) -> (&str, usize) {
    if text.len() <= max_bytes {
        return (text, 0);
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], text.len() - end)
}

/// A throwaway repository for the navigation tool tests: a `.git` directory so
/// the walk applies `.gitignore`, an ignored `target/` and `*.log`, a hidden
/// directory, a binary file and sources that mention `needle`.
#[cfg(test)]
pub(super) fn fixture_repo() -> tempfile::TempDir {
    let dir = crate::test_support::disk_backed_tempdir("session-harness-navigation-");
    let root = dir.path();
    let write = |path: &str, content: &[u8]| {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    };
    write(".git/config", b"needle in git\n");
    write(".gitignore", b"target/\n*.log\n");
    write(
        "src/main.rs",
        b"fn main() {\n    println!(\"needle in main\");\n}\n",
    );
    write("src/lib.rs", b"pub fn helper() {}\n// NEEDLE upper\n");
    write("target/debug/out.rs", b"needle in target\n");
    write("build.log", b"needle in log\n");
    write(".github/workflows/ci.yml", b"name: needle ci\n");
    write("bin.dat", b"\0\x01needle\n");
    write("README.md", b"no match here\n");
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_glob_matches_names_at_any_depth_and_paths_from_the_root() {
        let by_name = FileGlob::new("*.rs").unwrap();
        assert!(by_name.is_match(Path::new("lib.rs")));
        assert!(by_name.is_match(Path::new("src/deep/mod.rs")));
        assert!(!by_name.is_match(Path::new("src/readme.md")));

        let by_path = FileGlob::new("src/**/*.ts").unwrap();
        assert!(by_path.is_match(Path::new("src/a.ts")));
        assert!(by_path.is_match(Path::new("src/x/y/b.ts")));
        assert!(!by_path.is_match(Path::new("lib/a.ts")));

        let braces = FileGlob::new("*.{ts,tsx}").unwrap();
        assert!(braces.is_match(Path::new("ui/app.tsx")));
        assert!(!braces.is_match(Path::new("ui/app.js")));

        assert!(FileGlob::new("[").is_err());
        assert!(FileGlob::new("  ").is_err());
    }

    #[test]
    fn optional_uint_accepts_integers_and_names_the_bad_field() {
        let args = serde_json::json!({"a": 5, "b": 7.0, "c": -1, "d": "x", "e": null});
        assert_eq!(optional_uint(&args, "a"), Ok(Some(5)));
        assert_eq!(optional_uint(&args, "b"), Ok(Some(7)));
        assert_eq!(optional_uint(&args, "e"), Ok(None));
        assert_eq!(optional_uint(&args, "missing"), Ok(None));
        for key in ["c", "d"] {
            let error = optional_uint(&args, key).unwrap_err();
            assert!(error.starts_with("invalid_arguments:"), "{error}");
            assert!(error.contains(key), "{error}");
        }
    }

    #[test]
    fn clip_keeps_a_character_boundary() {
        assert_eq!(clip_at_char_boundary("abcdef", 3), ("abc", 3));
        assert_eq!(clip_at_char_boundary("abc", 10), ("abc", 0));
        // `é` is two bytes: a cut inside it backs up to the boundary.
        assert_eq!(clip_at_char_boundary("aé", 2), ("a", 2));
    }
}
