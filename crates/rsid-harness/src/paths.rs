//! Sandboxed path resolution for the harness file tools.

use std::path::{Component, Path, PathBuf};

/// Resolve a tool-requested path relative to a working directory, rejecting
/// traversal outside it.
///
/// Extracted from `openai.rs` for cross-provider reuse. Used by provider tool
/// execution loops (read_file, write_file, list_dir, run_command).
///
/// - Absolute paths: accepted only if they fall within `working_dir`.
/// - Relative paths: joined with `working_dir`, then checked for escape.
/// - For existing files: `canonicalize` + prefix check.
/// - For non-existing files (write targets): manual component walk with `..` escape detection.
pub fn resolve_sandboxed_path(
    working_dir: &Path,
    relative: &str,
) -> std::result::Result<PathBuf, String> {
    let candidate = if Path::new(relative).is_absolute() {
        PathBuf::from(relative)
    } else {
        working_dir.join(relative)
    };

    // Canonicalize working_dir (it must exist)
    let canon_wd = working_dir
        .canonicalize()
        .map_err(|e| format!("Cannot resolve working directory: {e}"))?;

    // For existing files, canonicalize and check prefix
    if candidate.exists() {
        let canon = candidate
            .canonicalize()
            .map_err(|e| format!("Cannot resolve path: {e}"))?;
        if canon.starts_with(&canon_wd) {
            return Ok(canon);
        }
        return Err(format!(
            "Path escapes working directory: {}",
            canon.display()
        ));
    }

    // For new files (write_file), normalize manually and check prefix
    let mut normalized = canon_wd.clone();
    for component in Path::new(relative).components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(c) => {
                normalized.push(c);
            }
            Component::CurDir => {}
            Component::RootDir => {
                return Err("Absolute paths not allowed".to_string());
            }
            Component::Prefix(_) => {
                return Err("Prefix paths not allowed".to_string());
            }
        }
    }
    if normalized.starts_with(&canon_wd) {
        Ok(normalized)
    } else {
        Err(format!(
            "Path escapes working directory: {}",
            normalized.display()
        ))
    }
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
}
