//! Bounded exact-byte discovery. Watch events are only hints; this scan is the
//! authority for each complete staged inventory.

use std::path::{Component, Path, PathBuf};

use rsi_codegraph::{
    SourceFile,
    invalidate::{
        estimated_fact_heap_bytes, estimated_pass_heap_bytes, estimated_streamed_heap_bytes,
    },
    staged::{MAX_STAGED_FILE_BYTES, MAX_STAGED_FILES, MAX_STAGED_SOURCE_BYTES},
};
use walkdir::WalkDir;

use super::{IndexError, RegisteredWorkspace, Result};

const IGNORED_DIRS: &[&str] = &[".git", ".rsi", "target", "node_modules", "vendor", ".venv"];
const MAX_WALK_ENTRIES: usize = 100_000;

/// Estimated heap one index pass may hold (#3491). A corpus within the extraction
/// cache's retention budget keeps every file's facts alive while it resolves
/// references ([`estimated_fact_heap_bytes`] sizes it); a larger one is streamed
/// (`rsi_codegraph::stream`) and holds only the resolver's inputs
/// ([`estimated_streamed_heap_bytes`]). A hub start whose largest checkout
/// (193 MB, 90 MB of it Rust) reached 2.8 GB resident, then 1.5 GB with the
/// pass held whole; streamed, a 127 MB checkout peaks near 0.17 GB live. A corpus over this budget fails its
/// pass visibly (`DiscoveryLimit`) instead of starving the daemon.
pub const MAX_PASS_HEAP_BYTES: usize = 2 * 1024 * 1024 * 1024;

pub struct Discovery {
    pub files: Vec<SourceFile>,
    pub digest: String,
    pub bytes: usize,
    /// Symlinked source paths left out of the inventory, sorted by path.
    pub skipped_symlinks: Vec<SkippedSymlink>,
}

/// A symlinked path with an indexed extension. The link is never read or
/// followed: only the link itself is inspected, so a skip can neither leak a
/// target's bytes nor fail the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedSymlink {
    pub relative_path: String,
    /// Whether the recorded target stays under the registered root, judged
    /// lexically from the link text. An in-root alias such as
    /// `CLAUDE.md -> AGENTS.md` is expected; an outside target is worth a
    /// look. This classifies the skip and never authorizes a read.
    pub target_within_root: bool,
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

/// Resolve the link text without touching what it names: read the link
/// itself, join a relative target onto the link's directory and fold `.`/`..`
/// lexically. A target that climbs above the filesystem root is outside.
fn link_target_within_root(root: &Path, link: &Path) -> bool {
    let Ok(target) = std::fs::read_link(link) else {
        return false;
    };
    let joined = if target.is_absolute() {
        target
    } else {
        link.parent().unwrap_or(root).join(target)
    };
    let mut folded = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                if !folded.pop() {
                    return false;
                }
            }
            Component::CurDir => {}
            other => folded.push(other.as_os_str()),
        }
    }
    folded.starts_with(root)
}

/// Scan only regular source files inside the canonical registered root.
/// A symlinked source path is skipped and recorded, one path at a time, so a
/// repository that tracks `CLAUDE.md -> AGENTS.md` still indexes. A root or
/// directory swapped during the scan still fails closed.
pub fn discover(workspace: &RegisteredWorkspace) -> Result<Discovery> {
    scan(workspace, true, MAX_PASS_HEAP_BYTES)
}

/// The same scan and digest as [`discover`] without keeping any file's bytes
/// (`files` is empty): the publish fence only compares digests, and holding a
/// second copy of the source next to the staged facts is memory the pass does
/// not need (#3491).
pub fn discover_digest(workspace: &RegisteredWorkspace) -> Result<Discovery> {
    scan(workspace, false, MAX_PASS_HEAP_BYTES)
}

fn scan(
    workspace: &RegisteredWorkspace,
    keep_bytes: bool,
    heap_budget: usize,
) -> Result<Discovery> {
    // A registered root can disappear or be replaced while the daemon runs.
    workspace.validate_current_root()?;
    let mut files = Vec::new();
    let mut digests: Vec<(String, blake3::Hash)> = Vec::new();
    let mut skipped_symlinks = Vec::new();
    let mut total_bytes = 0usize;
    let mut read_bytes = 0usize;
    let mut estimated_heap = 0usize;
    let mut streamed_heap = 0usize;
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
            skipped_symlinks.push(SkippedSymlink {
                relative_path: entry
                    .path()
                    .strip_prefix(&workspace.root)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/"),
                target_within_root: link_target_within_root(&workspace.root, entry.path()),
            });
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        if digests.len() == MAX_STAGED_FILES {
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
        estimated_heap =
            estimated_heap.saturating_add(estimated_fact_heap_bytes(&relative_path, size));
        streamed_heap =
            streamed_heap.saturating_add(estimated_streamed_heap_bytes(&relative_path, size));
        if estimated_pass_heap_bytes(estimated_heap, streamed_heap) > heap_budget {
            return Err(IndexError::DiscoveryLimit("pass heap budget"));
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
        read_bytes = read_bytes.saturating_add(bytes.len());
        digests.push((relative_path.clone(), blake3::hash(&bytes)));
        if keep_bytes {
            files.push(SourceFile {
                relative_path,
                bytes,
            });
        }
    }
    digests.sort_by(|a, b| a.0.cmp(&b.0));
    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    skipped_symlinks.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    let bytes = read_bytes;
    if bytes > MAX_STAGED_SOURCE_BYTES {
        return Err(IndexError::DiscoveryLimit("source bytes"));
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rsid-codegraph-discovery-v1");
    for (relative_path, digest) in &digests {
        hasher.update(&(relative_path.len() as u64).to_le_bytes());
        hasher.update(relative_path.as_bytes());
        hasher.update(digest.as_bytes());
    }
    // A tree with no links hashes exactly as before. Mixing the skips in
    // otherwise lets a new link reach the publish fence and refresh the count.
    for link in &skipped_symlinks {
        hasher.update(b"skipped-symlink");
        hasher.update(&(link.relative_path.len() as u64).to_le_bytes());
        hasher.update(link.relative_path.as_bytes());
        hasher.update(&[u8::from(link.target_within_root)]);
    }
    Ok(Discovery {
        files,
        digest: hasher.finalize().to_hex().to_string(),
        bytes,
        skipped_symlinks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
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

    /// #3491: the publish fence needs the digest only, so it must not hold a
    /// second copy of the source next to the staged facts; and a corpus whose
    /// estimated pass heap exceeds the budget fails visibly before any read.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_fence_scan_keeps_no_bytes_and_a_pass_over_the_heap_budget_is_refused() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn one() {}\n".repeat(8)).unwrap();
        std::fs::write(root.path().join("notes.md"), "# notes\n".repeat(8)).unwrap();
        let workspace = RegisteredWorkspace::primary(Uuid::new_v4(), root.path()).unwrap();
        let full = discover(&workspace).unwrap();
        let fence = discover_digest(&workspace).unwrap();
        assert_eq!(full.files.len(), 2);
        assert!(fence.files.is_empty());
        assert_eq!(fence.digest, full.digest);
        assert_eq!(fence.bytes, full.bytes);

        let estimate = full
            .files
            .iter()
            .map(|file| estimated_fact_heap_bytes(&file.relative_path, file.bytes.len()))
            .sum::<usize>();
        scan(&workspace, true, estimate).expect("a pass exactly at the budget is admitted");
        assert!(matches!(
            scan(&workspace, true, estimate - 1),
            Err(IndexError::DiscoveryLimit("pass heap budget"))
        ));
        // The shipped budget admits this pass.
        assert!(estimate < MAX_PASS_HEAP_BYTES);
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn symlinked_sources_are_skipped_and_recorded_without_following() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "# agents\n").unwrap();
        std::fs::create_dir_all(root.path().join("crates/rsid")).unwrap();
        std::fs::write(root.path().join("crates/rsid/AGENTS.md"), "# rsid\n").unwrap();
        std::fs::write(outside.path().join("secret.md"), "SECRET-OUTSIDE-BYTES").unwrap();
        std::fs::create_dir(outside.path().join("docs")).unwrap();
        std::fs::write(outside.path().join("docs/leak.md"), "LEAK").unwrap();
        // In-root alias, nested in-root alias, out-of-root file, dangling link
        // and a linked directory: none may be read, followed or descended.
        std::os::unix::fs::symlink("AGENTS.md", root.path().join("CLAUDE.md")).unwrap();
        std::os::unix::fs::symlink("AGENTS.md", root.path().join("crates/rsid/CLAUDE.md")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.md"), root.path().join("x.md"))
            .unwrap();
        std::os::unix::fs::symlink("missing-target.md", root.path().join("dangling.md")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("docs"), root.path().join("linked-docs"))
            .unwrap();

        let workspace = RegisteredWorkspace::primary(Uuid::new_v4(), root.path()).unwrap();
        let found = discover(&workspace).unwrap();

        let paths = found
            .files
            .iter()
            .map(|file| file.relative_path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, ["AGENTS.md", "crates/rsid/AGENTS.md"]);
        assert!(
            found
                .files
                .iter()
                .all(|file| !file.bytes.windows(4).any(|w| w == b"LEAK" || w == b"SECR")),
            "a skipped link's target bytes entered the inventory"
        );
        assert_eq!(
            found.skipped_symlinks,
            [
                SkippedSymlink {
                    relative_path: "CLAUDE.md".into(),
                    target_within_root: true,
                },
                SkippedSymlink {
                    relative_path: "crates/rsid/CLAUDE.md".into(),
                    target_within_root: true,
                },
                SkippedSymlink {
                    relative_path: "dangling.md".into(),
                    target_within_root: true,
                },
                SkippedSymlink {
                    relative_path: "x.md".into(),
                    target_within_root: false,
                },
            ]
        );
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn skipped_symlinks_change_the_digest_only_when_present() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "# agents\n").unwrap();
        let workspace = RegisteredWorkspace::primary(Uuid::new_v4(), root.path()).unwrap();
        let plain = discover(&workspace).unwrap();
        assert!(plain.skipped_symlinks.is_empty());
        assert_eq!(plain.digest, discover(&workspace).unwrap().digest);

        std::os::unix::fs::symlink("AGENTS.md", root.path().join("CLAUDE.md")).unwrap();
        let linked = discover(&workspace).unwrap();
        assert_eq!(linked.files.len(), plain.files.len());
        assert_eq!(linked.skipped_symlinks.len(), 1);
        assert_ne!(linked.digest, plain.digest);
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn registered_root_swapped_for_a_symlink_still_fails_closed() {
        let base = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let root = base.path().join("checkout");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("lib.rs"), "pub fn one() {}\n").unwrap();
        std::fs::write(other.path().join("lib.rs"), "pub fn two() {}\n").unwrap();
        let workspace = RegisteredWorkspace::primary(Uuid::new_v4(), &root).unwrap();
        discover(&workspace).unwrap();

        std::fs::rename(&root, base.path().join("moved")).unwrap();
        std::os::unix::fs::symlink(other.path(), &root).unwrap();
        assert!(matches!(
            discover(&workspace),
            Err(IndexError::UnsafeWorkspace(_))
        ));
    }
}
