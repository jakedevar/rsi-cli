//! Read-only worktree content digests.
//!
//! The digest intentionally hashes filesystem content, not only Git status, so
//! a tracked file whose bytes change without changing the dirty-path set still
//! changes the digest.

use super::git_worktree::{git_command, run_bounded_records};
use crate::error::{DaemonError, Result};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Maximum number of files and symlinks included in one digest.
const WORKTREE_DIGEST_MAX_FILES: usize = 65_536;

/// Maximum total content bytes included in one digest.
const WORKTREE_DIGEST_MAX_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub enum WorktreeDigest {
    Digest([u8; 32]),
    Unbounded,
}

impl PartialEq for WorktreeDigest {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Digest(left), Self::Digest(right)) => left == right,
            _ => false,
        }
    }
}

impl Eq for WorktreeDigest {}

impl WorktreeDigest {
    #[must_use]
    pub fn event_value(self) -> Option<String> {
        match self {
            Self::Digest(bytes) => Some(format!("sha256:{}", hex::encode(bytes))),
            Self::Unbounded => None,
        }
    }
}

/// Compute a deterministic digest of cached and nonignored untracked files.
///
/// The digest is based on `(mode, path, SHA-256(file bytes or symlink target))`
/// records. A cached path whose file is missing contributes a deterministic
/// missing record; any other unreadable or non-regular/non-symlink path returns
/// `Unbounded`. `Unbounded` never equals another digest, including itself.
pub fn worktree_content_digest(root: &Path) -> Result<WorktreeDigest> {
    worktree_content_digest_with_limits(root, WORKTREE_DIGEST_MAX_FILES, WORKTREE_DIGEST_MAX_BYTES)
}

fn worktree_content_digest_with_limits(
    root: &Path,
    max_files: usize,
    max_bytes: usize,
) -> Result<WorktreeDigest> {
    let paths = list_worktree_paths(root)?;
    if paths.len() > max_files {
        return Ok(WorktreeDigest::Unbounded);
    }

    let mut records = Vec::with_capacity(paths.len());
    let mut total_bytes = 0_usize;
    for path in paths {
        let absolute = root.join(&path);
        let metadata = match std::fs::symlink_metadata(&absolute) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Ok(WorktreeDigest::Unbounded),
        };

        let (mode, content_hash, content_len) = match metadata {
            None => (0_u32, hash_bytes(b"missing"), 0_usize),
            Some(metadata) => {
                let mode = metadata.permissions().mode();
                if metadata.is_symlink() {
                    let content_len = usize::try_from(metadata.len()).map_err(|_| {
                        DaemonError::Process("worktree symlink target was too large".into())
                    })?;
                    if total_bytes.saturating_add(content_len) > max_bytes {
                        return Ok(WorktreeDigest::Unbounded);
                    }
                    let target = match std::fs::read_link(&absolute) {
                        Ok(target) => target,
                        Err(_) => return Ok(WorktreeDigest::Unbounded),
                    };
                    let target_bytes = target.as_os_str().as_bytes();
                    let content_len = target_bytes.len();
                    if total_bytes.saturating_add(content_len) > max_bytes {
                        return Ok(WorktreeDigest::Unbounded);
                    }
                    (mode, hash_bytes(target_bytes), content_len)
                } else if metadata.is_file() {
                    let content_len = usize::try_from(metadata.len())
                        .map_err(|_| DaemonError::Process("worktree file was too large".into()))?;
                    if total_bytes.saturating_add(content_len) > max_bytes {
                        return Ok(WorktreeDigest::Unbounded);
                    }
                    let mut file = match std::fs::File::open(&absolute) {
                        Ok(file) => file,
                        Err(_) => return Ok(WorktreeDigest::Unbounded),
                    };
                    let mut hasher = Sha256::new();
                    let mut buffer = [0_u8; 64 * 1024];
                    let mut read_len = 0_usize;
                    loop {
                        let read = match file.read(&mut buffer) {
                            Ok(read) => read,
                            Err(_) => return Ok(WorktreeDigest::Unbounded),
                        };
                        if read == 0 {
                            break;
                        }
                        read_len = read_len.saturating_add(read);
                        if total_bytes.saturating_add(read_len) > max_bytes {
                            return Ok(WorktreeDigest::Unbounded);
                        }
                        hasher.update(&buffer[..read]);
                    }
                    (mode, hasher.finalize().into(), read_len)
                } else {
                    return Ok(WorktreeDigest::Unbounded);
                }
            }
        };

        total_bytes = total_bytes.saturating_add(content_len);
        if total_bytes > max_bytes {
            return Ok(WorktreeDigest::Unbounded);
        }
        records.push((mode, path, content_hash));
    }

    records.sort();
    let mut digest = Sha256::new();
    digest.update(b"rsi-worktree-content-v1\0");
    for (mode, path, content_hash) in records {
        digest.update(mode.to_be_bytes());
        digest.update((path.as_os_str().len() as u64).to_be_bytes());
        digest.update(path.as_os_str().as_bytes());
        digest.update(content_hash);
    }
    Ok(WorktreeDigest::Digest(digest.finalize().into()))
}

fn list_worktree_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let mut command = git_command();
    command
        .args([
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS");
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(key);
    }

    let mut paths = Vec::new();
    run_bounded_records(
        command,
        None,
        b'\0',
        true,
        "list worktree content paths",
        &mut |record| {
            let path = record.strip_suffix(b"\0").ok_or_else(|| {
                DaemonError::Process("Git worktree content path record was unterminated".into())
            })?;
            if path.is_empty() {
                return Err(DaemonError::Process(
                    "Git worktree content path record was empty".into(),
                ));
            }
            paths.push(PathBuf::from(std::ffi::OsString::from_vec(path.to_vec())));
            Ok(())
        },
    )?;
    Ok(paths)
}

fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repository() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("temp repository");
        git(directory.path(), &["init", "-q", "-b", "main"]);
        git(
            directory.path(),
            &["config", "user.email", "digest@example.test"],
        );
        git(directory.path(), &["config", "user.name", "Digest Test"]);
        directory
    }

    fn commit(root: &Path, path: &str, contents: &[u8]) {
        std::fs::write(root.join(path), contents).expect("write file");
        git(root, &["add", path]);
        git(root, &["commit", "-qm", "fixture"]);
    }

    fn digest(root: &Path) -> WorktreeDigest {
        worktree_content_digest(root).expect("digest worktree")
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn digest_is_deterministic_order_independent_and_ignores_excluded_files() {
        let left = repository();
        let right = repository();
        std::fs::write(left.path().join(".gitignore"), "ignored\n").expect("write left ignore");
        std::fs::write(right.path().join(".gitignore"), "ignored\n").expect("write right ignore");
        std::fs::write(left.path().join("alpha"), b"alpha").expect("write alpha");
        std::fs::write(left.path().join("beta"), b"beta").expect("write beta");
        std::fs::write(right.path().join("beta"), b"beta").expect("write beta");
        std::fs::write(right.path().join("alpha"), b"alpha").expect("write alpha");
        assert_eq!(digest(left.path()), digest(right.path()));

        std::fs::write(left.path().join("ignored"), b"ignored").expect("write ignored file");
        assert_eq!(digest(left.path()), digest(right.path()));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn digest_tracks_content_untracked_mode_symlink_and_missing_changes() {
        let repository = repository();
        commit(repository.path(), "tracked", b"base");
        let baseline = digest(repository.path());

        std::fs::write(repository.path().join("tracked"), b"changed").expect("change content");
        let changed = digest(repository.path());
        assert_ne!(baseline, changed);

        std::fs::write(repository.path().join("tracked"), b"base").expect("restore content");
        assert_eq!(baseline, digest(repository.path()));

        std::fs::write(repository.path().join("untracked"), b"new").expect("write untracked");
        let untracked = digest(repository.path());
        assert_ne!(baseline, untracked);

        std::os::unix::fs::symlink("tracked", repository.path().join("link"))
            .expect("create symlink");
        let symlink = digest(repository.path());
        assert_ne!(untracked, symlink);

        std::fs::remove_file(repository.path().join("link")).expect("remove symlink");
        std::os::unix::fs::symlink("untracked", repository.path().join("link"))
            .expect("retarget symlink");
        assert_ne!(symlink, digest(repository.path()));

        std::fs::remove_file(repository.path().join("link")).expect("remove second symlink");
        let mode = std::fs::metadata(repository.path().join("tracked"))
            .expect("tracked metadata")
            .permissions()
            .mode();
        std::fs::set_permissions(
            repository.path().join("tracked"),
            std::fs::Permissions::from_mode(mode | 0o111),
        )
        .expect("make executable");
        let executable = digest(repository.path());
        assert_ne!(untracked, executable);

        std::fs::remove_file(repository.path().join("tracked")).expect("delete tracked file");
        let missing = digest(repository.path());
        assert_ne!(executable, missing);
        assert_eq!(missing, digest(repository.path()));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn digest_is_unbounded_past_caps_and_never_equals_itself() {
        let repository = repository();
        std::fs::write(repository.path().join("file"), b"abc").expect("write file");
        assert!(matches!(
            worktree_content_digest_with_limits(repository.path(), 0, 100)
                .expect("count-capped digest"),
            WorktreeDigest::Unbounded
        ));
        assert!(matches!(
            worktree_content_digest_with_limits(repository.path(), 1, 2)
                .expect("byte-capped digest"),
            WorktreeDigest::Unbounded
        ));
        let sparse = tempfile::tempdir().expect("sparse repository");
        git(sparse.path(), &["init", "-q", "-b", "main"]);
        let file = std::fs::File::create(sparse.path().join("sparse")).expect("create sparse file");
        file.set_len(1024 * 1024 * 1024)
            .expect("extend sparse file");
        drop(file);
        assert!(matches!(
            worktree_content_digest_with_limits(sparse.path(), 10, 1024)
                .expect("sparse-file digest"),
            WorktreeDigest::Unbounded
        ));
        assert_ne!(WorktreeDigest::Unbounded, WorktreeDigest::Unbounded);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn digest_supports_repositories_without_commits() {
        let repository = repository();
        assert_eq!(digest(repository.path()), digest(repository.path()));
    }
}
