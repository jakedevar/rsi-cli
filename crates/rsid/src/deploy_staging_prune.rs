//! Staging-directory custody after a deploy (#2681).
//!
//! Every deploy is built into `~/.rsi/staging/bin-<sha>` (about 180 MB), and
//! nothing used to remove the old ones: a hub held 4.3 GB of them. A verified
//! deploy now prunes the others itself, on the hub and on a satellite alike
//! (a satellite runs the same deploy flow when the hub sends it a `peer_id`
//! deploy).
//!
//! Kept: the directory of the deployed commit, of the succeeded deploy before
//! it (the rollback build) and of any live (`staged` or `restarting`) deploy.
//! Also kept: a directory touched within [`STAGING_PRUNE_MIN_AGE`], because a
//! worker builds into `bin-<sha>` before it asks for the deploy that names it,
//! and the deploy row stores no directory. Only real directories named
//! `bin-<hex>` are ever removed; anything else under the root (the shared
//! `release-target`, the supervisor script, a symlink) is left alone.

use crate::store::Store;
use crate::store::agent_deploys::DeployRow;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A staging directory touched this recently is never pruned.
pub const STAGING_PRUNE_MIN_AGE: Duration = Duration::from_secs(60 * 60);

/// Succeeded deploys whose directories stay: the deployed one and the one
/// before it, for rollback.
const KEPT_SUCCEEDED_DEPLOYS: usize = 2;

/// Shortest `<hex>` after `bin-` that names a commit (a short sha).
const MIN_SHA_PREFIX: usize = 7;

/// What one prune pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Directories removed, with the bytes each held.
    pub removed: Vec<(PathBuf, u64)>,
    /// Directories that could not be removed, with the error.
    pub failed: Vec<(PathBuf, String)>,
    /// `bin-*` directories kept (named by the keep set, or too recent).
    pub kept: usize,
}

impl PruneReport {
    /// Bytes freed by the removed directories.
    #[must_use]
    pub fn bytes_freed(&self) -> u64 {
        self.removed.iter().map(|(_, bytes)| *bytes).sum()
    }
}

/// The commit a `bin-<hex>` directory name stands for, as the hex part.
fn bin_dir_sha_prefix(name: &str) -> Option<&str> {
    let hex = name.strip_prefix("bin-")?;
    let valid = hex.len() >= MIN_SHA_PREFIX
        && hex.len() <= 40
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    valid.then_some(hex)
}

/// Commits whose staging directory must stay after `deployed` succeeded.
///
/// # Errors
/// A persistence error.
pub fn keep_shas(store: &Store, deployed: &DeployRow) -> crate::error::Result<Vec<String>> {
    let mut keep = vec![deployed.sha.clone()];
    keep.extend(store.recent_succeeded_deploy_shas(KEPT_SUCCEEDED_DEPLOYS)?);
    if let Some(live) = store.live_agent_deploy()? {
        keep.push(live.sha);
    }
    keep.sort();
    keep.dedup();
    Ok(keep)
}

/// The bytes under `path`, never following a link. Unreadable entries count 0.
fn tree_bytes(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| tree_bytes(&entry.path()))
        .sum()
}

/// Remove the `bin-<hex>` directories of `root` that `keep` does not name and
/// that were not touched within `min_age` of `now`.
#[must_use]
pub fn prune_staging_bins(
    root: &Path,
    keep: &[String],
    now: SystemTime,
    min_age: Duration,
) -> PruneReport {
    let mut report = PruneReport::default();
    let Ok(entries) = fs::read_dir(root) else {
        return report;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(prefix) = name.to_str().and_then(bin_dir_sha_prefix) else {
            continue;
        };
        let path = entry.path();
        // A link or a file named like a staging directory is not ours to remove.
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let named = keep.iter().any(|sha| sha.starts_with(prefix));
        let recent = meta
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_none_or(|age| age < min_age);
        if named || recent {
            report.kept += 1;
            continue;
        }
        let bytes = tree_bytes(&path);
        match fs::remove_dir_all(&path) {
            Ok(()) => report.removed.push((path, bytes)),
            Err(error) => report.failed.push((path, error.to_string())),
        }
    }
    report.removed.sort();
    report
}

/// [`prune_staging_bins`] with the production age floor, logging what it did.
#[must_use]
pub fn prune_and_log(root: &Path, keep: &[String]) -> PruneReport {
    let report = prune_staging_bins(root, keep, SystemTime::now(), STAGING_PRUNE_MIN_AGE);
    for (path, bytes) in &report.removed {
        tracing::info!(path = %path.display(), bytes, "pruned stale deploy staging directory");
    }
    for (path, error) in &report.failed {
        tracing::warn!(path = %path.display(), %error, "stale deploy staging directory not removed");
    }
    if !report.removed.is_empty() {
        tracing::info!(
            removed = report.removed.len(),
            kept = report.kept,
            bytes_freed = report.bytes_freed(),
            "deploy staging prune finished"
        );
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURRENT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PREVIOUS: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const PENDING: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const OLD: &str = "dddddddddddddddddddddddddddddddddddddddd";

    fn stage_dir(root: &Path, sha: &str, bytes: usize) -> PathBuf {
        let dir = root.join(format!("bin-{}", &sha[..9]));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("rsid"), vec![0_u8; bytes]).unwrap();
        dir
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_keep_set_stays_and_every_other_old_bin_directory_goes() {
        let root = tempfile::tempdir().unwrap();
        let current = stage_dir(root.path(), CURRENT, 10);
        let previous = stage_dir(root.path(), PREVIOUS, 10);
        let pending = stage_dir(root.path(), PENDING, 10);
        let old = stage_dir(root.path(), OLD, 100);
        let other = root.path().join("release-target");
        fs::create_dir_all(&other).unwrap();
        let keep = vec![
            CURRENT.to_string(),
            PREVIOUS.to_string(),
            PENDING.to_string(),
        ];

        let now = SystemTime::now() + Duration::from_secs(2 * 60 * 60);
        let report = prune_staging_bins(root.path(), &keep, now, STAGING_PRUNE_MIN_AGE);

        assert!(current.is_dir() && previous.is_dir() && pending.is_dir());
        assert!(!old.exists());
        assert!(other.is_dir(), "a directory that is not bin-<sha> stays");
        assert_eq!(report.removed, vec![(old, 100)]);
        assert_eq!(report.bytes_freed(), 100);
        assert_eq!(report.kept, 3);
        assert!(report.failed.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_directory_touched_within_the_age_floor_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let fresh = stage_dir(root.path(), OLD, 10);
        let report = prune_staging_bins(root.path(), &[], SystemTime::now(), STAGING_PRUNE_MIN_AGE);
        assert!(fresh.is_dir());
        assert_eq!(report.kept, 1);
        assert!(report.removed.is_empty());
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_link_named_like_a_staging_directory_is_not_followed_or_removed() {
        let root = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        fs::write(target.path().join("keep"), b"x").unwrap();
        let link = root.path().join(format!("bin-{}", &OLD[..9]));
        std::os::unix::fs::symlink(target.path(), &link).unwrap();
        let now = SystemTime::now() + Duration::from_secs(2 * 60 * 60);
        let report = prune_staging_bins(root.path(), &[], now, STAGING_PRUNE_MIN_AGE);
        assert!(report.removed.is_empty());
        assert!(target.path().join("keep").is_file());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn only_bin_hex_names_are_staging_directories() {
        assert_eq!(bin_dir_sha_prefix("bin-0123abcd9"), Some("0123abcd9"));
        for name in [
            "bin-",
            "bin-xyz1234",
            "bin-abc",
            "binary-0123abcd",
            "release-target",
        ] {
            assert_eq!(bin_dir_sha_prefix(name), None, "{name}");
        }
    }
}
