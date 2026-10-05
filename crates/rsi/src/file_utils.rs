use std::path::{Path, PathBuf};

const MAX_FILES: usize = 100_000;

/// Walk a directory tree, returning relative file paths.
/// Respects .gitignore, optionally shows hidden files.
/// Capped at MAX_FILES entries, sorted alphabetically.
pub fn walk_files(root: &Path, show_hidden: bool) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(!show_hidden)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();
    for entry in walker.flatten() {
        if entry.file_type().is_none_or(|ft| !ft.is_file()) {
            continue;
        }
        if let Ok(rel) = entry.path().strip_prefix(root) {
            paths.push(rel.to_path_buf());
        }
        if paths.len() >= MAX_FILES {
            break;
        }
    }
    paths.sort();
    paths
}

/// Like `walk_files` but limits traversal depth. `max_depth = Some(n)` includes
/// files where the relative path has at most `n` directory components (0 = root,
/// 1 = one level deep). `None` behaves identically to `walk_files`.
pub fn walk_files_scoped(root: &Path, show_hidden: bool, max_depth: Option<usize>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(!show_hidden)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();
    for entry in walker.flatten() {
        if entry.file_type().is_none_or(|ft| !ft.is_file()) {
            continue;
        }
        if let Ok(rel) = entry.path().strip_prefix(root) {
            if let Some(max) = max_depth {
                // components() for a file path: the file name is the last component,
                // preceding components are directory segments. depth = parent component count.
                let dir_depth = rel.components().count().saturating_sub(1);
                if dir_depth > max {
                    continue;
                }
            }
            paths.push(rel.to_path_buf());
        }
        if paths.len() >= MAX_FILES {
            break;
        }
    }
    paths.sort();
    paths
}

/// Returns true if `target` resolves (following symlinks) to a path inside
/// `scope_root`, at any depth. Canonicalizes both paths, so a symlink inside
/// the project that points outside it is out of scope.
///
/// The explorer tree expands to any depth, so files it shows must open; the
/// earlier two-component cap made every nested file (`src/app/mod.rs`)
/// unopenable from the explorer, its finder and telescope.
pub fn is_within_project_scope(scope_root: &Path, target: &Path) -> bool {
    let Ok(canon_root) = scope_root.canonicalize() else {
        return false;
    };
    let Ok(canon_target) = target.canonicalize() else {
        return false;
    };
    canon_target.starts_with(&canon_root)
}

/// Search a project directory for a file matching a partial path suffix.
///
/// When an AI returns a path like `src/event.rs` or `event.rs` instead of the
/// full absolute path, this function walks the `working_dir` tree and finds files
/// whose relative path ends with the given `partial_path`, ensuring the match is
/// at a path component boundary (not a substring within a filename).
///
/// Returns the absolute path of the best match (shortest relative path = most
/// specific). Returns `None` if no match or if the partial path is empty.
pub fn find_file_by_suffix(working_dir: &Path, partial_path: &str) -> Option<PathBuf> {
    if partial_path.is_empty() {
        return None;
    }

    let needle = partial_path.replace('\\', "/");
    let walker = ignore::WalkBuilder::new(working_dir)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    let mut matches: Vec<PathBuf> = Vec::new();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let abs = entry.into_path();
        let rel = match abs.strip_prefix(working_dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let rel_str = rel.to_string_lossy();
        // Match at a path-component boundary: the relative path must either
        // equal the needle exactly, or the character before the suffix must be '/'.
        if rel_str.ends_with(&needle)
            && (rel_str.len() == needle.len()
                || rel_str.as_bytes()[rel_str.len() - needle.len() - 1] == b'/')
        {
            matches.push(abs);
        }
        // Bail early if we find too many matches (avoids full traversal for ambiguous queries)
        if matches.len() > 50 {
            break;
        }
    }

    // Prefer the shortest relative path (most specific match).
    matches.sort_by_key(|p| p.components().count());
    matches.into_iter().next()
}

#[cfg(test)]
mod scope_tests {
    use super::*;

    #[test]
    #[allow(clippy::unwrap_used)]
    fn nested_project_files_are_in_scope() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("crates/rsi/src");
        std::fs::create_dir_all(&nested).unwrap();
        let file = nested.join("main.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        assert!(is_within_project_scope(root.path(), &file));
        assert!(is_within_project_scope(
            root.path(),
            &root.path().join("crates/./rsi/../rsi/src/main.rs")
        ));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn paths_outside_the_project_are_out_of_scope() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let outside = other.path().join("secret.txt");
        std::fs::write(&outside, "x").unwrap();

        assert!(!is_within_project_scope(root.path(), &outside));
        assert!(!is_within_project_scope(
            root.path(),
            &root.path().join("missing.rs")
        ));
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn symlinks_escaping_the_project_are_out_of_scope() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let outside = other.path().join("secret.txt");
        std::fs::write(&outside, "x").unwrap();
        let link = root.path().join("link.txt");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        assert!(!is_within_project_scope(root.path(), &link));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn unbounded_walk_finds_deep_files() {
        let root = tempfile::tempdir().unwrap();
        let deep = root.path().join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("deep.rs"), "").unwrap();
        std::fs::write(root.path().join("top.rs"), "").unwrap();

        let files = walk_files_scoped(root.path(), false, None);
        assert!(files.contains(&PathBuf::from("a/b/c/deep.rs")));
        assert!(files.contains(&PathBuf::from("top.rs")));
    }
}
