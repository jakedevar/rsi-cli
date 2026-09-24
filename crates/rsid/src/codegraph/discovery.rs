//! Bounded exact-byte discovery. Watch events are only hints; this scan is the
//! authority for each complete staged inventory.

use std::path::{Component, Path};

use rsi_codegraph::{
    SourceFile,
    staged::{MAX_STAGED_FILE_BYTES, MAX_STAGED_FILES, MAX_STAGED_SOURCE_BYTES},
};
use walkdir::WalkDir;

use super::{IndexError, RegisteredWorkspace, Result};

const IGNORED_DIRS: &[&str] = &[".git", ".rsi", "target", "node_modules", "vendor", ".venv"];
const MAX_WALK_ENTRIES: usize = 100_000;

pub struct Discovery {
    pub files: Vec<SourceFile>,
    pub digest: String,
    pub bytes: usize,
}

fn include_file(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "Cargo.lock")
        || path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                matches!(
                    ext.to_ascii_lowercase().as_str(),
                    "rs" | "md" | "markdown" | "toml"
                )
            })
}

fn skip_entry(entry: &walkdir::DirEntry) -> bool {
    entry.file_type().is_dir()
        && entry
            .file_name()
            .to_str()
            .is_some_and(|name| IGNORED_DIRS.contains(&name))
}

/// Scan only regular source files inside the canonical registered root.
/// Symlinked source paths fail closed, including links that point back inside.
pub fn discover(workspace: &RegisteredWorkspace) -> Result<Discovery> {
    // A registered root can disappear or be replaced while the daemon runs.
    workspace.validate_current_root()?;
    let mut files = Vec::new();
    let mut total_bytes = 0usize;
    let mut entry_count = 0usize;
    let walker = WalkDir::new(&workspace.root)
        .follow_links(false)
        .sort_by_file_name();
    for entry in walker.into_iter().filter_entry(|entry| !skip_entry(entry)) {
        entry_count += 1;
        if entry_count > MAX_WALK_ENTRIES {
            return Err(IndexError::DiscoveryLimit("walk entries"));
        }
        let entry = entry.map_err(|error| IndexError::UnsafeWorkspace(error.to_string()))?;
        if !include_file(entry.path()) {
            continue;
        }
        if entry.file_type().is_symlink() {
            return Err(IndexError::UnsafeWorkspace(
                "source path is a symlink".into(),
            ));
        }
        if !entry.file_type().is_file() {
            continue;
        }
        if files.len() == MAX_STAGED_FILES {
            return Err(IndexError::DiscoveryLimit("file count"));
        }
        let relative = entry
            .path()
            .strip_prefix(&workspace.root)
            .map_err(|_| IndexError::UnsafeWorkspace("source escaped registered root".into()))?;
        if relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(IndexError::UnsafeWorkspace(
                "invalid relative source path".into(),
            ));
        }
        let relative_path = relative
            .to_str()
            .ok_or_else(|| IndexError::UnsafeWorkspace("non-UTF-8 source path".into()))?
            .replace(std::path::MAIN_SEPARATOR, "/");
        if relative_path.len() > rsi_codegraph::MAX_PATH_BYTES {
            return Err(IndexError::DiscoveryLimit("path length"));
        }
        let size = usize::try_from(
            entry
                .metadata()
                .map_err(|error| IndexError::UnsafeWorkspace(error.to_string()))?
                .len(),
        )
        .map_err(|_| IndexError::DiscoveryLimit("file size"))?;
        if size > MAX_STAGED_FILE_BYTES {
            return Err(IndexError::DiscoveryLimit("file size"));
        }
        total_bytes = total_bytes
            .checked_add(size)
            .ok_or(IndexError::DiscoveryLimit("source bytes"))?;
        if total_bytes > MAX_STAGED_SOURCE_BYTES {
            return Err(IndexError::DiscoveryLimit("source bytes"));
        }
        // Reject path substitution between traversal and read. This does not
        // close every TOCTOU race; a post-extraction rescan is the publish fence.
        let canonical = entry.path().canonicalize()?;
        if canonical != entry.path() || !canonical.starts_with(&workspace.root) {
            return Err(IndexError::UnsafeWorkspace(
                "source path changed during scan".into(),
            ));
        }
        let bytes = std::fs::read(entry.path())?;
        if bytes.len() > MAX_STAGED_FILE_BYTES {
            return Err(IndexError::DiscoveryLimit("file size"));
        }
        files.push(SourceFile {
            relative_path,
            bytes,
        });
    }
    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    let bytes = files.iter().map(|file| file.bytes.len()).sum();
    if bytes > MAX_STAGED_SOURCE_BYTES {
        return Err(IndexError::DiscoveryLimit("source bytes"));
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rsid-codegraph-discovery-v1");
    for file in &files {
        hasher.update(&(file.relative_path.len() as u64).to_le_bytes());
        hasher.update(file.relative_path.as_bytes());
        hasher.update(blake3::hash(&file.bytes).as_bytes());
    }
    Ok(Discovery {
        files,
        digest: hasher.finalize().to_hex().to_string(),
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn discovery_ignores_generated_files_and_changes_on_exact_bytes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("target")).unwrap();
        std::fs::write(root.path().join("target/generated.rs"), "bad").unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn one() {}\n").unwrap();
        let workspace = RegisteredWorkspace::primary(Uuid::new_v4(), root.path()).unwrap();
        let before = discover(&workspace).unwrap();
        assert_eq!(before.files.len(), 1);
        std::fs::write(root.path().join("lib.rs"), "pub fn two() {}\n").unwrap();
        let after = discover(&workspace).unwrap();
        assert_ne!(before.digest, after.digest);
        assert_eq!(after.files[0].relative_path, "lib.rs");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_source_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.rs"), "secret").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.rs"),
            root.path().join("link.rs"),
        )
        .unwrap();
        let workspace = RegisteredWorkspace::primary(Uuid::new_v4(), root.path()).unwrap();
        assert!(matches!(
            discover(&workspace),
            Err(IndexError::UnsafeWorkspace(_))
        ));
    }
}
