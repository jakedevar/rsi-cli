//! Sandboxed path resolution for the harness file tools.

use rsi_common::harness_tool_policy::FilePathScope;
use std::path::{Component, Path, PathBuf};

/// Resolve a tool-requested path relative to a working directory, rejecting
/// traversal outside it.
///
/// Extracted from `openai.rs` for cross-provider reuse. Used by provider tool
/// execution loops (read_file, write_file, list_dir, run_command).
///
/// - Absolute paths: accepted only if they fall within `working_dir`, whether
///   or not the target exists yet.
/// - Relative paths: joined with `working_dir`, then checked for escape.
/// - The deepest existing ancestor is canonicalized (so a symlink cannot lead
///   out) and the not-yet-existing remainder is appended after `..` folding.
///
/// # Errors
/// A message naming the working directory and suggesting a relative path.
pub fn resolve_sandboxed_path(
    working_dir: &Path,
    relative: &str,
) -> std::result::Result<PathBuf, String> {
    resolve_path_in_scope(working_dir, relative, FilePathScope::WorkingDir)
}

/// [`resolve_sandboxed_path`] under an explicit [`FilePathScope`] (#3172).
/// `TaskEnvironment` addresses any absolute path of the (disposable) task
/// environment and applies no containment; `WorkingDir` is the contained mode.
///
/// # Errors
/// A message naming the working directory and suggesting a relative path.
pub fn resolve_path_in_scope(
    working_dir: &Path,
    requested: &str,
    scope: FilePathScope,
) -> std::result::Result<PathBuf, String> {
    let candidate = if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        working_dir.join(requested)
    };

    if scope == FilePathScope::TaskEnvironment {
        return resolve_through_existing_ancestor(&candidate);
    }

    // Canonicalize working_dir (it must exist)
    let canon_wd = working_dir
        .canonicalize()
        .map_err(|e| format!("Cannot resolve working directory: {e}"))?;
    let resolved = resolve_through_existing_ancestor(&candidate)?;
    if resolved.starts_with(&canon_wd) {
        Ok(resolved)
    } else {
        Err(format!(
            "Path escapes working directory {}: {}. Use a path relative to the working \
             directory (for example `src/main.rs`).",
            canon_wd.display(),
            resolved.display()
        ))
    }
}

/// Fold `.` and `..` lexically, canonicalize the deepest ancestor that exists
/// and append the rest, so a write target that does not exist yet still gets a
/// symlink-free path.
fn resolve_through_existing_ancestor(path: &Path) -> std::result::Result<PathBuf, String> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    let mut existing = normalized.as_path();
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    while !existing.exists() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                existing = parent;
            }
            _ => break,
        }
    }
    let mut resolved = existing
        .canonicalize()
        .map_err(|e| format!("Cannot resolve path: {e}"))?;
    for name in tail.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_resolve_sandboxed_path_normal() {
        let wd = std::env::temp_dir();
        let result = resolve_sandboxed_path(&wd, "foo/bar.txt");
        assert!(result.is_ok());
        assert!(result.unwrap().starts_with(wd.canonicalize().unwrap()));
    }

    #[test]
    fn test_resolve_sandboxed_path_traversal_rejected() {
        let wd = std::env::temp_dir().join("rsi_test_sandbox");
        fs::create_dir_all(&wd).ok();
        let result = resolve_sandboxed_path(&wd, "../../etc/passwd");
        assert!(result.is_err());
        fs::remove_dir_all(&wd).ok();
    }

    #[test]
    fn test_resolve_sandboxed_path_absolute_outside_rejected() {
        let wd = std::env::temp_dir();
        let result = resolve_sandboxed_path(&wd, "/etc/passwd");
        // Absolute paths outside working_dir should be rejected
        if let Ok(path) = result {
            assert!(path.starts_with(wd.canonicalize().unwrap()));
        }
    }

    #[test]
    fn absolute_path_to_a_new_file_inside_working_dir_resolves() {
        let wd = tempfile::tempdir().unwrap();
        let canon = wd.path().canonicalize().unwrap();
        let target = canon.join("sub/new.py");
        let resolved = resolve_sandboxed_path(wd.path(), target.to_str().unwrap()).unwrap();
        assert_eq!(resolved, target);
        // The same holds when the working directory is spelled non-canonically.
        let dotted = wd.path().join("sub/../x.py");
        let resolved = resolve_sandboxed_path(wd.path(), dotted.to_str().unwrap()).unwrap();
        assert_eq!(resolved, canon.join("x.py"));
    }

    #[test]
    fn absolute_path_to_a_new_file_outside_working_dir_names_the_directory() {
        let wd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("new.py");
        let error = resolve_sandboxed_path(wd.path(), target.to_str().unwrap()).unwrap_err();
        let canon_wd = wd.path().canonicalize().unwrap();
        assert!(error.contains(&canon_wd.display().to_string()), "{error}");
        assert!(
            error.contains("relative to the working directory"),
            "{error}"
        );
        // `..` out of the directory is refused the same way for a new file.
        let error = resolve_sandboxed_path(wd.path(), "../escape.txt").unwrap_err();
        assert!(
            error.contains("relative to the working directory"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directory_cannot_lead_a_new_file_out() {
        let wd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), wd.path().join("link")).unwrap();
        assert!(resolve_sandboxed_path(wd.path(), "link/new.txt").is_err());
    }

    #[test]
    fn task_environment_scope_reaches_absolute_paths_outside_working_dir() {
        let wd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let canon = outside.path().canonicalize().unwrap();
        let target = canon.join("deep/new.conf");
        let resolved = resolve_path_in_scope(
            wd.path(),
            target.to_str().unwrap(),
            FilePathScope::TaskEnvironment,
        )
        .unwrap();
        assert_eq!(resolved, target);
        assert!(
            resolve_path_in_scope(
                wd.path(),
                target.to_str().unwrap(),
                FilePathScope::WorkingDir
            )
            .is_err()
        );
    }
}
