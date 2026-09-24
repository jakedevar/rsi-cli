use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use super::types::{MemoryFileEntry, MemorySource};
use crate::error::Result;

/// Compute the SHA-256 hex digest of a string.
///
/// Used for content-based deduplication of chunks and files.
/// The input is hashed as UTF-8 bytes.
pub fn hash_text(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let digest = hasher.finalize();
    hex::encode(digest)
}

/// Normalize a relative path: trim whitespace, strip leading `./` or `/`, replace `\` with `/`.
fn normalize_rel_path(value: &str) -> String {
    let trimmed = value.trim();
    let stripped = trimmed.trim_start_matches(['.', '/', ' ']);
    stripped.replace('\\', "/")
}

/// Check whether a relative path falls within the memory file scope.
///
/// Valid memory paths:
/// - `MEMORY.md` (root-level curated memory)
/// - `memory.md` (root-level alternative)
/// - `memory/...` (anything under the memory subdirectory)
///
/// The path is normalized: leading `./`, `/`, and trailing whitespace are stripped,
/// and backslashes are replaced with forward slashes.
pub fn is_memory_path(rel_path: &str) -> bool {
    let normalized = normalize_rel_path(rel_path);
    if normalized.is_empty() {
        return false;
    }
    if normalized == "MEMORY.md" || normalized == "memory.md" {
        return true;
    }
    normalized.starts_with("memory/")
}

/// Discover all memory-eligible Markdown files under the workspace directory.
///
/// Scans three locations in order:
/// 1. `{workspace_dir}/MEMORY.md` — root-level curated memory
/// 2. `{workspace_dir}/memory.md` — root-level alternative
/// 3. `{workspace_dir}/memory/` — recursively walk for all `.md` files
///
/// Symlinks are skipped at every level (files and directories).
/// Non-`.md` files are skipped.
/// Duplicate paths (by canonical path) are deduplicated.
///
/// Returns absolute paths to discovered files.
pub fn list_memory_files(workspace_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut result: Vec<PathBuf> = Vec::new();

    fn try_add_markdown_file(path: &Path, result: &mut Vec<PathBuf>) {
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(_) => return,
        };
        if meta.file_type().is_symlink() {
            return;
        }
        if !meta.is_file() {
            return;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            return;
        }
        result.push(path.to_path_buf());
    }

    try_add_markdown_file(&workspace_dir.join("MEMORY.md"), &mut result);
    try_add_markdown_file(&workspace_dir.join("memory.md"), &mut result);

    let memory_dir = workspace_dir.join("memory");
    if std::fs::symlink_metadata(&memory_dir)
        .is_ok_and(|meta| !meta.file_type().is_symlink() && meta.is_dir())
    {
        for entry in WalkDir::new(&memory_dir).follow_links(false) {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.file_type().is_symlink() {
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            result.push(path.to_path_buf());
        }
    }

    // Deduplicate by canonical path
    let mut seen = HashSet::new();
    result.retain(|p| {
        let canonical = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        seen.insert(canonical)
    });

    Ok(result)
}

/// Build a MemoryFileEntry for a file on disk.
///
/// Reads the file, computes its SHA-256 hash, and collects filesystem metadata.
/// Returns `None` if the file doesn't exist (race between discovery and read).
/// Propagates other I/O errors.
pub fn build_file_entry(abs_path: &Path, workspace_dir: &Path) -> Result<Option<MemoryFileEntry>> {
    let metadata = match std::fs::metadata(abs_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    let content = match std::fs::read_to_string(abs_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    let hash = hash_text(&content);

    let rel_path = abs_path
        .strip_prefix(workspace_dir)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| {
            abs_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        });

    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let size = metadata.len() as i64;

    Ok(Some(MemoryFileEntry {
        path: rel_path,
        abs_path: abs_path.to_path_buf(),
        mtime_ms,
        size,
        hash,
        source: MemorySource::Memory,
        // Memory files are global by design — see plan §Scope Semantics:
        // global memory files carry NULL project_id and are excluded from
        // project-scoped agent injection.
        project_id: None,
        // On-disk memory files use mtime/size for change detection; the event
        // watermark applies only to session transcripts.
        watermark: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // --- hash_text tests ---

    #[test]
    fn test_hash_text_empty() {
        assert_eq!(
            hash_text(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_hash_text_hello() {
        // SHA-256 of "hello"
        assert_eq!(
            hash_text("hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_hash_text_unicode() {
        let hash = hash_text("こんにちは🌍");
        assert_eq!(hash.len(), 64); // valid hex digest
        // Deterministic: same input -> same output
        assert_eq!(hash, hash_text("こんにちは🌍"));
    }

    #[test]
    fn test_hash_text_deterministic() {
        let a = hash_text("test content");
        let b = hash_text("test content");
        assert_eq!(a, b);
    }

    // --- is_memory_path tests ---

    #[test]
    fn test_is_memory_path_root_memory() {
        assert!(is_memory_path("MEMORY.md"));
    }

    #[test]
    fn test_is_memory_path_alt_memory() {
        assert!(is_memory_path("memory.md"));
    }

    #[test]
    fn test_is_memory_path_subdir() {
        assert!(is_memory_path("memory/2026-02-28.md"));
    }

    #[test]
    fn test_is_memory_path_nested_subdir() {
        assert!(is_memory_path("memory/topic/notes.md"));
    }

    #[test]
    fn test_is_memory_path_leading_dot_slash() {
        assert!(is_memory_path("./MEMORY.md"));
    }

    #[test]
    fn test_is_memory_path_leading_slash() {
        assert!(is_memory_path("/MEMORY.md"));
    }

    #[test]
    fn test_is_memory_path_wrong_case() {
        assert!(!is_memory_path("Memory.md"));
    }

    #[test]
    fn test_is_memory_path_empty() {
        assert!(!is_memory_path(""));
    }

    #[test]
    fn test_is_memory_path_random_file() {
        assert!(!is_memory_path("src/main.rs"));
    }

    #[test]
    fn test_is_memory_path_memory_no_slash() {
        assert!(!is_memory_path("memory"));
    }

    #[test]
    fn test_is_memory_path_backslash() {
        assert!(is_memory_path("memory\\notes.md"));
    }

    #[test]
    fn test_normalize_rel_path_various() {
        assert_eq!(normalize_rel_path("./foo.md"), "foo.md");
        assert_eq!(normalize_rel_path("/foo.md"), "foo.md");
        assert_eq!(normalize_rel_path("  ./foo.md  "), "foo.md");
        assert_eq!(normalize_rel_path("foo\\bar.md"), "foo/bar.md");
        assert_eq!(normalize_rel_path("///foo.md"), "foo.md");
        assert_eq!(normalize_rel_path(""), "");
    }

    // --- list_memory_files tests ---

    #[test]
    fn test_list_memory_files_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let files = list_memory_files(dir.path()).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_list_memory_files_memory_md() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("MEMORY.md"), "# Memory").unwrap();
        let files = list_memory_files(dir.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("MEMORY.md"));
    }

    #[test]
    fn test_list_memory_files_both_roots() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("MEMORY.md"), "# Memory").unwrap();
        fs::write(dir.path().join("memory.md"), "# Alt").unwrap();
        let files = list_memory_files(dir.path()).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_list_memory_files_subdir() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        fs::write(dir.path().join("memory/notes.md"), "# Notes").unwrap();
        let files = list_memory_files(dir.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("notes.md"));
    }

    #[test]
    fn test_list_memory_files_nested() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("memory/topic")).unwrap();
        fs::write(dir.path().join("memory/topic/deep.md"), "# Deep").unwrap();
        let files = list_memory_files(dir.path()).unwrap();
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn test_list_memory_files_non_md_skipped() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        fs::write(dir.path().join("memory/notes.txt"), "text").unwrap();
        let files = list_memory_files(dir.path()).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_list_memory_files_dedup() {
        let dir = tempfile::tempdir().unwrap();
        // MEMORY.md at root and in memory/ — only root is added by try_add,
        // memory/ walk would add the nested one. Different canonical paths = both kept.
        fs::write(dir.path().join("MEMORY.md"), "# Root").unwrap();
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        fs::write(dir.path().join("memory/MEMORY.md"), "# Nested").unwrap();
        let files = list_memory_files(dir.path()).unwrap();
        // Both should be present since they're different files
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_list_memory_files_nonexistent_dir() {
        let files = list_memory_files(Path::new("/nonexistent/path/12345")).unwrap();
        assert!(files.is_empty());
    }

    // --- build_file_entry tests ---

    #[test]
    fn test_build_file_entry_basic() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("MEMORY.md");
        fs::write(&file_path, "hello world").unwrap();

        let entry = build_file_entry(&file_path, dir.path()).unwrap().unwrap();
        assert_eq!(entry.path, "MEMORY.md");
        assert_eq!(entry.hash, hash_text("hello world"));
        assert_eq!(entry.size, 11);
        assert!(entry.mtime_ms > 0);
        assert_eq!(entry.source, MemorySource::Memory);
    }

    #[test]
    fn test_build_file_entry_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let result = build_file_entry(&dir.path().join("nope.md"), dir.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_build_file_entry_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("memory/sub")).unwrap();
        let file_path = dir.path().join("memory/sub/notes.md");
        fs::write(&file_path, "content").unwrap();

        let entry = build_file_entry(&file_path, dir.path()).unwrap().unwrap();
        assert_eq!(entry.path, "memory/sub/notes.md");
    }
}
