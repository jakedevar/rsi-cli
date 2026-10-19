//! Workspace path safety: canonicalization and containment of session working directories.
//!
//! All session working directories pass through this module before subprocess spawn.
//! Per-file containment for provider tool execution (`resolve_sandboxed_path`)
//! lives in `rsid-harness::paths` (#2616).

use crate::error::{DaemonError, Result};
use std::path::{Component, Path, PathBuf};

/// Maximum symlink depth to prevent infinite symlink cycles.
/// Matches Linux kernel's MAXSYMLINKS (40).
const MAX_SYMLINK_DEPTH: usize = 40;

/// Canonicalize a session working directory.
///
/// Resolves symlinks, normalizes `.` and `..`, and verifies the directory exists.
/// Returns the canonical absolute path or an `InvalidParam` error with a
/// user-friendly message suitable for display in the TUI notification bar.
///
/// This is the primary validation entry point for `working_dir` at RPC ingestion.
pub fn canonicalize_working_dir(path: &Path) -> Result<PathBuf> {
    let canon = path.canonicalize().map_err(|e| {
        DaemonError::InvalidParam(format!(
            "working_dir '{}' is not accessible: {}",
            path.display(),
            e
        ))
    })?;

    // Verify it's a directory (canonicalize succeeds on files too)
    if !canon.is_dir() {
        return Err(DaemonError::InvalidParam(format!(
            "working_dir '{}' is not a directory",
            path.display()
        )));
    }

    Ok(canon)
}

/// Check that `path` is a descendant of at least one allowed root.
///
/// Both `path` and each root must be pre-canonicalized (no symlinks, no `..`).
/// Returns `Ok(())` if `path.starts_with(root)` for any root, or `InvalidParam` error.
///
/// When `allowed_roots` is empty, all paths are permitted (opt-in containment).
pub fn validate_containment(path: &Path, allowed_roots: &[PathBuf]) -> Result<()> {
    if allowed_roots.is_empty() {
        return Ok(());
    }

    for root in allowed_roots {
        if path.starts_with(root) {
            return Ok(());
        }
    }

    Err(DaemonError::InvalidParam(format!(
        "working_dir '{}' is outside all allowed workspace roots",
        path.display()
    )))
}

/// Canonicalize a path without requiring all components to exist.
///
/// Resolves symlinks for existing path segments, appends remaining segments
/// verbatim when a component doesn't exist. Protects against infinite symlink
/// cycles with a depth counter matching Linux's MAXSYMLINKS (40).
///
/// This is the Rust equivalent of Symphony's `PathSafety.canonicalize/1`.
/// Currently unused -- reserved for future use cases where paths must be
/// validated before their target directory is created.
#[allow(dead_code)]
pub fn canonicalize_non_strict(path: &Path) -> Result<PathBuf> {
    canonicalize_non_strict_inner(path, 0)
}

fn canonicalize_non_strict_inner(path: &Path, depth: usize) -> Result<PathBuf> {
    if depth > MAX_SYMLINK_DEPTH {
        return Err(DaemonError::InvalidParam(format!(
            "Symlink cycle detected resolving '{}'",
            path.display()
        )));
    }

    // Start with absolute path
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };

    let mut resolved = PathBuf::new();
    let mut hit_enoent = false;

    for component in abs.components() {
        match component {
            Component::RootDir => resolved.push("/"),
            Component::Normal(seg) => {
                resolved.push(seg);
                if hit_enoent {
                    // Past the point of no return -- append verbatim
                    continue;
                }
                match std::fs::symlink_metadata(&resolved) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        let target = std::fs::read_link(&resolved).map_err(|e| {
                            DaemonError::InvalidParam(format!(
                                "Cannot read symlink '{}': {}",
                                resolved.display(),
                                e
                            ))
                        })?;
                        let full_target = if target.is_absolute() {
                            target
                        } else {
                            resolved.parent().unwrap_or(Path::new("/")).join(&target)
                        };
                        // Recurse to resolve the symlink target
                        resolved = canonicalize_non_strict_inner(&full_target, depth + 1)?;
                    }
                    Ok(_) => {
                        // Regular file or directory -- keep going
                    }
                    Err(_) => {
                        // Path doesn't exist -- append remaining segments verbatim
                        hit_enoent = true;
                    }
                }
            }
            Component::ParentDir => {
                if !hit_enoent {
                    resolved.pop();
                } else {
                    resolved.push("..");
                }
            }
            Component::CurDir => {}
            Component::Prefix(_) => {} // Windows only
        }
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_canonicalize_working_dir_existing_dir() {
        let tmp = std::env::temp_dir();
        let result = canonicalize_working_dir(&tmp);
        assert!(result.is_ok());
        // Result should be absolute and have no symlinks
        let canon = result.unwrap();
        assert!(canon.is_absolute());
        assert!(canon.is_dir());
    }

    #[test]
    fn test_canonicalize_working_dir_nonexistent() {
        let path = Path::new("/nonexistent/directory/that/does/not/exist");
        let result = canonicalize_working_dir(path);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("working_dir"));
        assert!(err.contains("not accessible"));
    }

    #[test]
    fn test_canonicalize_working_dir_file_not_dir() {
        let tmp = std::env::temp_dir().join("rsi_test_path_safety_file");
        fs::write(&tmp, "test").ok();
        let result = canonicalize_working_dir(&tmp);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not a directory"));
        fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_validate_containment_empty_roots_allows_all() {
        let path = Path::new("/any/path/at/all");
        assert!(validate_containment(path, &[]).is_ok());
    }

    #[test]
    fn test_validate_containment_matching_root() {
        let roots = vec![PathBuf::from("/home/user/projects")];
        assert!(validate_containment(Path::new("/home/user/projects/flywheel"), &roots).is_ok());
    }

    #[test]
    fn test_validate_containment_no_match() {
        let roots = vec![PathBuf::from("/home/user/projects")];
        let result = validate_containment(Path::new("/etc/passwd"), &roots);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("outside all allowed workspace roots"));
    }

    #[test]
    fn test_validate_containment_multiple_roots() {
        let roots = vec![
            PathBuf::from("/home/user/work"),
            PathBuf::from("/home/user/personal"),
            PathBuf::from("/opt/tools"),
        ];
        assert!(validate_containment(Path::new("/home/user/work/proj"), &roots).is_ok());
        assert!(validate_containment(Path::new("/home/user/personal/notes"), &roots).is_ok());
        assert!(validate_containment(Path::new("/opt/tools/bin"), &roots).is_ok());
        assert!(validate_containment(Path::new("/tmp/scratch"), &roots).is_err());
    }

    #[test]
    fn test_canonicalize_non_strict_existing_path() {
        let tmp = std::env::temp_dir();
        let result = canonicalize_non_strict(&tmp);
        assert!(result.is_ok());
        assert!(result.unwrap().is_absolute());
    }

    #[test]
    fn test_canonicalize_non_strict_partially_existing() {
        // /tmp exists, /tmp/flywheel_nonexistent_subdir does not
        let path = std::env::temp_dir().join("flywheel_nonexistent_subdir/deep/path");
        let result = canonicalize_non_strict(&path);
        assert!(result.is_ok());
        let canon = result.unwrap();
        assert!(canon.is_absolute());
        // Should end with the non-existent segments appended verbatim
        assert!(canon.ends_with("flywheel_nonexistent_subdir/deep/path"));
    }
}
