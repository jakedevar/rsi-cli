//! `harness_source_digest` (#3192, program #3135 M6 decision D4): a sha256 over the git tree or
//! blob ids of the code that interprets a benchmark run's effective inputs, taken at the daemon's
//! build commit (`RSI_BUILD_SHA`).
//!
//! The rsid build SHA alone is no identity for the harness: the hub daemon is redeployed often, and
//! most redeploys leave the harness code untouched. This digest changes exactly when a path in
//! the committed list ([`PATH_LIST`], `data/harness_source_paths.txt`, one reason per path)
//! changes, so a benchmark record stays comparable to an earlier one across a redeploy that did
//! not touch the harness (`bench_record::compare`).
//!
//! This file depends on `std` and `sha2` only: `crates/rsid/build.rs` includes it by path to embed
//! the digest of the commit it builds.

use std::path::Path;
use std::process::Command;

use sha2::{Digest as _, Sha256};

/// The committed path list, one `<path> | <reason>` line per path.
pub const PATH_LIST: &str = include_str!("../data/harness_source_paths.txt");

/// Layout version of the digested bytes; changing how the digest is taken bumps it.
const DIGEST_LAYOUT: &str = "rsi-harness-source-digest-v1";

/// Prefix of a digest that could not be taken (no git, the commit is not in the repository, a
/// listed path is missing or has uncommitted changes). It names the build SHA, never claims
/// comparability and is refused by the paired compare.
pub const UNAVAILABLE_PREFIX: &str = "unavailable:";

/// One line of the path list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePath {
    /// Repository-relative path; a trailing `/` marks a directory.
    pub path: String,
    /// Why this path decides whether two runs used the same harness.
    pub reason: String,
}

impl SourcePath {
    /// The path as git names it (no trailing slash).
    #[must_use]
    pub fn git_path(&self) -> &str {
        self.path.trim_end_matches('/')
    }
}

/// Parses a path list. Blank lines and `#` comments are skipped; every other line is
/// `<path> | <reason>` with a safe relative path and a nonempty reason, and no path repeats.
///
/// # Errors
/// A line that does not follow the format, with its 1-based number.
pub fn parse_path_list(text: &str) -> Result<Vec<SourcePath>, String> {
    let mut out: Vec<SourcePath> = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let number = index + 1;
        let (path, reason) = line
            .split_once('|')
            .map(|(path, reason)| (path.trim(), reason.trim()))
            .ok_or_else(|| format!("line {number}: expected `<path> | <reason>`"))?;
        if reason.is_empty() {
            return Err(format!("line {number}: `{path}` has no reason"));
        }
        let bare = path.trim_end_matches('/');
        let unsafe_path = bare.is_empty()
            || bare.starts_with('/')
            || bare.contains('\\')
            || bare.contains('\0')
            || bare
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..");
        if unsafe_path {
            return Err(format!(
                "line {number}: `{path}` is not a safe relative path"
            ));
        }
        if out.iter().any(|entry| entry.git_path() == bare) {
            return Err(format!("line {number}: `{path}` is listed twice"));
        }
        out.push(SourcePath {
            path: path.to_string(),
            reason: reason.to_string(),
        });
    }
    if out.is_empty() {
        return Err("the path list is empty".to_string());
    }
    Ok(out)
}

/// `sha256:<hex>` over `(path, object id)` pairs, sorted by path. The object id is a git tree or
/// blob id, so the pair changes exactly when the content under the path does.
#[must_use]
pub fn digest_from_entries(entries: &[(String, String)]) -> String {
    let mut sorted: Vec<&(String, String)> = entries.iter().collect();
    sorted.sort();
    let mut hasher = Sha256::new();
    hasher.update(DIGEST_LAYOUT.as_bytes());
    hasher.update(b"\n");
    for (path, object_id) in sorted {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(object_id.as_bytes());
        hasher.update(b"\n");
    }
    format!("sha256:{:x}", hasher.finalize())
}

/// The digest recorded when the real one cannot be taken ([`UNAVAILABLE_PREFIX`]).
#[must_use]
pub fn unavailable_digest(build_sha: &str) -> String {
    format!("{UNAVAILABLE_PREFIX}{build_sha}")
}

/// True for a digest produced by [`unavailable_digest`].
#[must_use]
pub fn is_unavailable(digest: &str) -> bool {
    digest.starts_with(UNAVAILABLE_PREFIX)
}

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|error| format!("git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {}: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The git object id of every listed path at `commit`, in list order.
///
/// # Errors
/// `commit` is not a full hex object id, git fails, or a listed path does not exist at `commit`.
pub fn object_ids_at(
    repo: &Path,
    commit: &str,
    paths: &[SourcePath],
) -> Result<Vec<(String, String)>, String> {
    let full = matches!(commit.len(), 40 | 64) && commit.bytes().all(|b| b.is_ascii_hexdigit());
    if !full {
        return Err(format!("`{commit}` is not a full commit id"));
    }
    let specs: Vec<String> = paths
        .iter()
        .map(|entry| format!("{commit}:{}", entry.git_path()))
        .collect();
    // `rev-parse --verify` takes one name; one call per path keeps every error attributable.
    let mut out = Vec::with_capacity(paths.len());
    for (entry, spec) in paths.iter().zip(&specs) {
        let text = git(repo, &["rev-parse", "--verify", "--end-of-options", spec])
            .map_err(|error| format!("{}: {error}", entry.path))?;
        out.push((entry.path.clone(), text.trim().to_string()));
    }
    Ok(out)
}

/// True when a listed path has uncommitted or untracked changes in `repo`'s working tree, so the
/// committed digest would not describe the bytes a build compiled.
///
/// # Errors
/// Git fails.
pub fn paths_dirty(repo: &Path, paths: &[SourcePath]) -> Result<bool, String> {
    let mut args: Vec<&str> = vec!["status", "--porcelain", "--untracked-files=all", "--"];
    args.extend(paths.iter().map(SourcePath::git_path));
    Ok(!git(repo, &args)?.trim().is_empty())
}

/// The `harness_source_digest` of `commit` over the paths of `list_text`.
///
/// # Errors
/// A malformed list or any [`object_ids_at`] error.
pub fn source_digest_from_list(
    repo: &Path,
    commit: &str,
    list_text: &str,
) -> Result<String, String> {
    let paths = parse_path_list(list_text)?;
    Ok(digest_from_entries(&object_ids_at(repo, commit, &paths)?))
}

/// The digest to record for a build of `commit` in `repo` with the committed [`PATH_LIST`]:
/// the real digest, or [`unavailable_digest`] when it cannot be taken or the working tree differs
/// from `commit` under a listed path. Never fails the caller.
#[must_use]
pub fn embedded_source_digest(repo: &Path, commit: &str) -> String {
    let dirty = parse_path_list(PATH_LIST)
        .and_then(|paths| paths_dirty(repo, &paths))
        .unwrap_or(true);
    if dirty {
        return unavailable_digest(commit);
    }
    source_digest_from_list(repo, commit, PATH_LIST).unwrap_or_else(|_| unavailable_digest(commit))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;

    const LIST: &str = "# list\n\nsrc/harness/ | the loop\nsrc/policy.rs | tool policy\n";

    fn run(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn write(dir: &Path, rel: &str, text: &str) {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// A fixture tree with a harness directory, a policy file and unrelated code.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q"]);
        write(dir.path(), "src/harness/loop.rs", "loop v1");
        write(dir.path(), "src/harness/tools/shell.rs", "shell v1");
        write(dir.path(), "src/policy.rs", "policy v1");
        write(dir.path(), "src/tui/view.rs", "tui v1");
        write(dir.path(), "README.md", "docs v1");
        commit(dir.path(), "base");
        dir
    }

    fn commit(dir: &Path, message: &str) -> String {
        run(dir, &["add", "-A"]);
        run(dir, &["commit", "-q", "--allow-empty", "-m", message]);
        run(dir, &["rev-parse", "HEAD"])
    }

    fn digest_at(dir: &Path, commit: &str) -> String {
        source_digest_from_list(dir, commit, LIST).unwrap()
    }

    #[test]
    fn a_change_outside_the_path_list_keeps_the_digest() {
        let dir = fixture();
        let before = run(dir.path(), &["rev-parse", "HEAD"]);
        write(dir.path(), "src/tui/view.rs", "tui v2");
        write(dir.path(), "README.md", "docs v2");
        write(dir.path(), "src/new_unrelated.rs", "x");
        let after = commit(dir.path(), "unrelated");
        assert_ne!(before, after);
        assert_eq!(
            digest_at(dir.path(), &before),
            digest_at(dir.path(), &after)
        );
    }

    #[test]
    fn a_change_inside_the_path_list_changes_the_digest() {
        let dir = fixture();
        let base_sha = run(dir.path(), &["rev-parse", "HEAD"]);
        let base = digest_at(dir.path(), &base_sha);
        assert!(base.starts_with("sha256:") && base.len() == 7 + 64);
        let mut seen = vec![base.clone()];
        for (rel, text) in [
            ("src/harness/loop.rs", "loop v2"),
            ("src/harness/tools/shell.rs", "shell v2"),
            (
                "src/harness/tools/added.rs",
                "new file in a listed directory",
            ),
            ("src/policy.rs", "policy v2"),
        ] {
            let case = fixture();
            write(case.path(), rel, text);
            let sha = commit(case.path(), "listed");
            let digest = digest_at(case.path(), &sha);
            assert_ne!(digest, base, "{rel}");
            assert!(
                !seen.contains(&digest),
                "{rel} collides with another change"
            );
            seen.push(digest);
        }
        // The digest is of the commit's content, not of its id or history.
        let again = commit(dir.path(), "empty commit");
        assert_ne!(base_sha, again);
        assert_eq!(digest_at(dir.path(), &again), base);
    }

    #[test]
    fn a_removed_or_missing_listed_path_is_refused_not_hashed() {
        let dir = fixture();
        fs::remove_file(dir.path().join("src/policy.rs")).unwrap();
        let sha = commit(dir.path(), "remove policy");
        let error = source_digest_from_list(dir.path(), &sha, LIST).unwrap_err();
        assert!(error.contains("src/policy.rs"), "{error}");
        let list_with_new_path =
            "src/harness/ | loop\nsrc/policy.rs | tool policy\nsrc/other | x\n";
        assert!(source_digest_from_list(dir.path(), &sha, list_with_new_path).is_err());
        assert!(source_digest_from_list(dir.path(), "main", LIST).is_err());
    }

    #[test]
    fn changing_the_list_changes_the_digest() {
        let dir = fixture();
        let sha = run(dir.path(), &["rev-parse", "HEAD"]);
        let fewer = "src/harness/ | the loop\n";
        assert_ne!(
            source_digest_from_list(dir.path(), &sha, fewer).unwrap(),
            digest_at(dir.path(), &sha)
        );
    }

    #[test]
    fn dirty_listed_paths_are_detected_and_unlisted_ones_are_not() {
        let dir = fixture();
        let paths = parse_path_list(LIST).unwrap();
        assert!(!paths_dirty(dir.path(), &paths).unwrap());
        write(dir.path(), "src/tui/view.rs", "edited");
        assert!(!paths_dirty(dir.path(), &paths).unwrap());
        write(dir.path(), "src/harness/loop.rs", "edited");
        assert!(paths_dirty(dir.path(), &paths).unwrap());
    }

    #[test]
    fn embedded_digest_falls_back_to_unavailable_without_git_state() {
        let dir = tempfile::tempdir().unwrap();
        let sha = "a".repeat(40);
        let digest = embedded_source_digest(dir.path(), &sha);
        assert_eq!(digest, unavailable_digest(&sha));
        assert!(is_unavailable(&digest));
        assert!(!is_unavailable("sha256:abc"));
    }

    #[test]
    fn the_committed_list_parses_and_every_path_exists() {
        let paths = parse_path_list(PATH_LIST).unwrap();
        assert!(paths.iter().any(|p| p.path == "crates/rsid-harness/"));
        let root = Path::new(crate::manifest_dir!()).join("../..");
        for entry in &paths {
            assert!(root.join(entry.git_path()).exists(), "{}", entry.path);
            assert!(!entry.reason.is_empty());
        }
    }

    #[test]
    fn list_format_errors_name_the_line() {
        for (text, needle) in [
            ("a/b", "line 1"),
            ("a/b |   ", "no reason"),
            ("../x | up", "safe relative"),
            ("/abs | root", "safe relative"),
            ("a//b | double", "safe relative"),
            ("a/ | one\na | two", "listed twice"),
            ("# only a comment\n", "empty"),
        ] {
            let error = parse_path_list(text).unwrap_err();
            assert!(error.contains(needle), "{text:?}: {error}");
        }
    }
}
