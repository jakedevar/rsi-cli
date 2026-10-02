//! Bounded daemon log when stdout and stderr are redirected to a file (#960).
//!
//! The legacy launcher runs `rsid >> ~/.rsi/daemon.log 2>&1`, and that file
//! reached 2 GB with no rotation. When fd 1 is a regular file larger than
//! [`MAX_LOG_BYTES`], the daemon renames it to `<name>.<UTC timestamp>`, opens
//! a fresh file at the same path and moves every standard fd that pointed at
//! the old file onto the new one with `dup2`. Nothing is lost: the old
//! content stays in the archive, the live file stays bounded, and the newest
//! [`KEEP_ARCHIVES`] archives are kept. Under the managed systemd service fd 1
//! is the journal socket, so this is a no-op and journald bounds its own
//! storage.

use chrono::{DateTime, Utc};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Size at which the live log is rotated.
pub const MAX_LOG_BYTES: u64 = 256 * 1024 * 1024;
/// Rotated archives kept next to the live log.
pub const KEEP_ARCHIVES: usize = 4;
/// How often the running daemon checks the size.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(600);

/// One completed rotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rotation {
    pub archived: PathBuf,
    pub pruned: Vec<PathBuf>,
}

fn fd_path(fd: RawFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{fd}"))
}

fn archive_suffix(now: DateTime<Utc>) -> String {
    now.format("%Y%m%dT%H%M%S%.9fZ").to_string()
}

fn is_archive_suffix(suffix: &str) -> bool {
    let bytes = suffix.as_bytes();
    // YYYYMMDD T HHMMSS . nnnnnnnnn Z
    bytes.len() == 26
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'T'
        && bytes[9..15].iter().all(u8::is_ascii_digit)
        && bytes[15] == b'.'
        && bytes[16..25].iter().all(u8::is_ascii_digit)
        && bytes[25] == b'Z'
}

/// Rotate the regular file behind `fds[0]` once it exceeds `max_bytes`.
///
/// Every fd in `fds` that refers to that same file moves onto the fresh one.
/// Returns `None` when there is nothing to rotate (not a regular file, under
/// the cap, or already unlinked).
///
/// # Errors
/// Returns the I/O error of the rename, reopen, `dup2` or archive pruning.
pub fn rotate_fds_if_needed(
    fds: &[RawFd],
    max_bytes: u64,
    keep: usize,
    now: DateTime<Utc>,
) -> io::Result<Option<Rotation>> {
    let Some(&primary) = fds.first() else {
        return Ok(None);
    };
    let Ok(metadata) = std::fs::metadata(fd_path(primary)) else {
        return Ok(None);
    };
    if !metadata.is_file() || metadata.len() <= max_bytes {
        return Ok(None);
    }
    let path = std::fs::read_link(fd_path(primary))?;
    // An unlinked target reads as "<path> (deleted)"; only rotate the file
    // that is still at its path.
    let current = match std::fs::metadata(&path) {
        Ok(current) => current,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if current.ino() != metadata.ino() || current.dev() != metadata.dev() {
        return Ok(None);
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    let archived = path.with_file_name(format!("{name}.{}", archive_suffix(now)));
    let targets = fds
        .iter()
        .copied()
        .filter(|fd| {
            std::fs::metadata(fd_path(*fd))
                .is_ok_and(|meta| meta.ino() == metadata.ino() && meta.dev() == metadata.dev())
        })
        .collect::<Vec<_>>();
    std::fs::rename(&path, &archived)?;
    let fresh = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)?;
    for fd in targets {
        // SAFETY: dup2 atomically points `fd` at the fresh file; both
        // descriptors are open for the duration of the call.
        if unsafe { nix::libc::dup2(fresh.as_raw_fd(), fd) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    drop(fresh);
    let pruned = prune_archives(&path, keep)?;
    Ok(Some(Rotation { archived, pruned }))
}

/// Remove all but the newest `keep` rotation archives of `path`.
///
/// Only files named `<name>.<rotation timestamp>` are considered.
///
/// # Errors
/// Returns the I/O error of listing the directory or removing an archive.
pub fn prune_archives(path: &Path, keep: usize) -> io::Result<Vec<PathBuf>> {
    let (Some(directory), Some(name)) = (
        path.parent(),
        path.file_name().and_then(|name| name.to_str()),
    ) else {
        return Ok(Vec::new());
    };
    let prefix = format!("{name}.");
    let mut archives = std::fs::read_dir(directory)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_name = entry.file_name().into_string().ok()?;
            let suffix = file_name.strip_prefix(&prefix)?;
            is_archive_suffix(suffix).then(|| entry.path())
        })
        .collect::<Vec<_>>();
    // The timestamp format sorts chronologically.
    archives.sort();
    let excess = archives.len().saturating_sub(keep);
    let mut pruned = Vec::new();
    for archive in archives.into_iter().take(excess) {
        std::fs::remove_file(&archive)?;
        pruned.push(archive);
    }
    Ok(pruned)
}

/// Check the daemon's own stdout/stderr now and every [`CHECK_INTERVAL`].
/// Needs a Tokio runtime.
pub fn spawn_bounded_log() {
    tokio::spawn(async {
        let mut ticker = tokio::time::interval(CHECK_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let rotation = tokio::task::spawn_blocking(|| {
                rotate_fds_if_needed(&[1, 2], MAX_LOG_BYTES, KEEP_ARCHIVES, Utc::now())
            })
            .await;
            match rotation {
                Ok(Ok(Some(rotation))) => tracing::info!(
                    archived = %rotation.archived.display(),
                    pruned = rotation.pruned.len(),
                    max_bytes = MAX_LOG_BYTES,
                    "Daemon log rotated"
                ),
                Ok(Ok(None)) => {}
                Ok(Err(error)) => tracing::warn!(%error, "Daemon log rotation failed"),
                Err(error) => tracing::warn!(%error, "Daemon log rotation task failed"),
            }
        }
    });
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::Write;

    fn at(second: u32) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("2026-09-28T08:00:{second:02}.000000001Z"))
            .expect("timestamp")
            .with_timezone(&Utc)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn oversized_log_moves_to_an_archive_and_both_fds_write_the_fresh_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let mut stdout = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let mut stderr = stdout.try_clone().unwrap();
        stdout.write_all(b"old line one\nold line two\n").unwrap();

        let rotation =
            rotate_fds_if_needed(&[stdout.as_raw_fd(), stderr.as_raw_fd()], 10, 4, at(1))
                .unwrap()
                .expect("oversized log rotates");

        assert_eq!(
            rotation.archived,
            dir.path().join("daemon.log.20260928T080001.000000001Z")
        );
        assert_eq!(
            std::fs::read_to_string(&rotation.archived).unwrap(),
            "old line one\nold line two\n"
        );
        stdout.write_all(b"new stdout\n").unwrap();
        stderr.write_all(b"new stderr\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "new stdout\nnew stderr\n"
        );
        assert!(rotation.pruned.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn small_logs_and_pipes_stay_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"short\n").unwrap();
        assert_eq!(
            rotate_fds_if_needed(&[file.as_raw_fd()], 1024, 4, at(2)).unwrap(),
            None
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "short\n");

        let (reader, writer) = nix::unistd::pipe().unwrap();
        assert_eq!(
            rotate_fds_if_needed(&[writer.as_raw_fd()], 0, 4, at(3)).unwrap(),
            None
        );
        drop(reader);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn pruning_keeps_the_newest_archives_and_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        for second in 10..16 {
            std::fs::write(
                dir.path()
                    .join(format!("daemon.log.{}", archive_suffix(at(second)))),
                b"archive",
            )
            .unwrap();
        }
        std::fs::write(dir.path().join("daemon.log.bak"), b"operator copy").unwrap();

        let pruned = prune_archives(&path, 4).unwrap();

        assert_eq!(
            pruned,
            vec![
                dir.path().join("daemon.log.20260928T080010.000000001Z"),
                dir.path().join("daemon.log.20260928T080011.000000001Z"),
            ]
        );
        let mut remaining = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        remaining.sort();
        assert_eq!(
            remaining,
            vec![
                "daemon.log.20260928T080012.000000001Z",
                "daemon.log.20260928T080013.000000001Z",
                "daemon.log.20260928T080014.000000001Z",
                "daemon.log.20260928T080015.000000001Z",
                "daemon.log.bak",
            ]
        );
    }
}
