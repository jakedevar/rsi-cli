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
//!
//! The archive stem always comes from the configured log path
//! ([`configured_log_path`]), never from the name fd 1 currently
//! resolves to. An fd inherited across a restart can still point at a renamed
//! archive; deriving the stem from that name grew it by one timestamp per
//! rotation until `rename` failed with `ENAMETOOLONG` (#2356). A live file
//! whose name is such a grown archive of the configured log is rotated back to
//! the configured path whatever its size, and the old chain is pruned by the
//! configured stem.

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
/// File name of the daemon log inside the data directory.
pub const LOG_FILENAME: &str = "daemon.log";

/// The configured daemon log path for `data_dir` (`~/.rsi/daemon.log`).
#[must_use]
pub fn configured_log_path(data_dir: &Path) -> PathBuf {
    data_dir.join(LOG_FILENAME)
}

/// One completed rotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rotation {
    pub archived: PathBuf,
    pub pruned: Vec<PathBuf>,
}

/// What `fd` refers to, from `fstat` (portable; it follows the open file, not
/// a path, so an unlinked or renamed file still reports itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FdFile {
    is_file: bool,
    len: u64,
    dev: u64,
    ino: u64,
}

impl FdFile {
    fn same_file(&self, metadata: &std::fs::Metadata) -> bool {
        self.ino == metadata.ino() && self.dev == metadata.dev()
    }
}

/// `None` when `fd` is not open. The `stat` field widths differ by platform
/// (`st_dev` is `i32` on macOS, `u64` on Linux), hence the casts.
#[allow(
    clippy::unnecessary_cast,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]
fn fd_file(fd: RawFd) -> Option<FdFile> {
    let stat = nix::sys::stat::fstat(fd).ok()?;
    let is_file = (stat.st_mode as nix::libc::mode_t & nix::libc::S_IFMT) == nix::libc::S_IFREG;
    Some(FdFile {
        is_file,
        len: u64::try_from(stat.st_size).unwrap_or(0),
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
    })
}

/// The path `fd` was opened at: `/proc/self/fd` on Linux, `F_GETPATH` on
/// macOS (#1229). Elsewhere the path is unknown and nothing is rotated.
#[cfg(target_os = "linux")]
fn fd_live_path(fd: RawFd) -> io::Result<Option<PathBuf>> {
    std::fs::read_link(format!("/proc/self/fd/{fd}")).map(Some)
}

#[cfg(target_os = "macos")]
fn fd_live_path(fd: RawFd) -> io::Result<Option<PathBuf>> {
    use std::os::unix::ffi::OsStrExt;
    let mut buffer = vec![0_u8; nix::libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes, NUL included, into the
    // buffer, which is PATH_MAX bytes long and outlives the call.
    if unsafe { nix::libc::fcntl(fd, nix::libc::F_GETPATH, buffer.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let len = buffer
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(buffer.len());
    Ok(Some(PathBuf::from(std::ffi::OsStr::from_bytes(
        &buffer[..len],
    ))))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn fd_live_path(_fd: RawFd) -> io::Result<Option<PathBuf>> {
    Ok(None)
}

/// `configured` with its directory resolved through symlinks, the form the
/// kernel reports for an open fd (macOS temp dirs live under `/var`, a link
/// to `/private/var`). `None` when the directory cannot be resolved.
fn resolved_configured(configured: &Path) -> Option<PathBuf> {
    let directory = std::fs::canonicalize(configured.parent()?).ok()?;
    Some(directory.join(configured.file_name()?))
}

fn archive_suffix(now: DateTime<Utc>) -> String {
    now.format("%Y%m%dT%H%M%S%.9fZ").to_string()
}

/// One `YYYYMMDDTHHMMSS.nnnnnnnnnZ` timestamp segment.
fn is_timestamp_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    bytes.len() == 26
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'T'
        && bytes[9..15].iter().all(u8::is_ascii_digit)
        && bytes[15] == b'.'
        && bytes[16..25].iter().all(u8::is_ascii_digit)
        && bytes[25] == b'Z'
}

/// One timestamp, or the grown chain `<ts>.<ts>...` that rotating an archive
/// by its own name used to leave behind (#2356).
fn is_archive_suffix(suffix: &str) -> bool {
    let mut rest = suffix;
    loop {
        let Some(head) = rest.get(..26) else {
            return false;
        };
        if !is_timestamp_segment(head) {
            return false;
        }
        rest = &rest[26..];
        if rest.is_empty() {
            return true;
        }
        let Some(tail) = rest.strip_prefix('.') else {
            return false;
        };
        rest = tail;
    }
}

/// Timestamp of the latest rotation recorded in an archive suffix.
fn latest_segment(suffix: &str) -> &str {
    suffix
        .get(suffix.len().saturating_sub(26)..)
        .unwrap_or(suffix)
}

/// Whether `live` is the configured log or a grown archive name of it.
fn is_stale_archive_name(live: &Path, configured: &Path) -> bool {
    let (Some(live_name), Some(name)) = (
        live.file_name().and_then(|name| name.to_str()),
        configured.file_name().and_then(|name| name.to_str()),
    ) else {
        return false;
    };
    live.parent() == configured.parent()
        && live_name
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(is_archive_suffix)
}

/// Rotate the regular file behind `fds[0]` once it exceeds `max_bytes`, or at
/// once when it is a renamed archive of `configured` (#2356).
///
/// The archive is `<configured>.<timestamp>` and the fresh file is created at
/// `configured`. When the live file is neither `configured` nor an archive of
/// it (an unrelated redirect), that file is its own configured path.
///
/// Every fd in `fds` that refers to that same file moves onto the fresh one.
/// Returns `None` when there is nothing to rotate (not a regular file, under
/// the cap, or already unlinked).
///
/// # Errors
/// Returns the I/O error of the rename, reopen, `dup2` or archive pruning.
pub fn rotate_fds_if_needed(
    fds: &[RawFd],
    configured: &Path,
    max_bytes: u64,
    keep: usize,
    now: DateTime<Utc>,
) -> io::Result<Option<Rotation>> {
    let Some(&primary) = fds.first() else {
        return Ok(None);
    };
    let Some(metadata) = fd_file(primary) else {
        return Ok(None);
    };
    if !metadata.is_file {
        return Ok(None);
    }
    let Some(live) = fd_live_path(primary)? else {
        return Ok(None);
    };
    let resolved = resolved_configured(configured);
    let is_configured = |path: &Path| path == configured || resolved.as_deref() == Some(path);
    let stale = is_stale_archive_name(&live, configured)
        || resolved
            .as_deref()
            .is_some_and(|resolved| is_stale_archive_name(&live, resolved));
    if metadata.len <= max_bytes && !stale {
        return Ok(None);
    }
    let path = live;
    // An unlinked target reads as "<path> (deleted)"; only rotate the file
    // that is still at its path.
    let current = match std::fs::metadata(&path) {
        Ok(current) => current,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.same_file(&current) {
        return Ok(None);
    }
    // The stem comes from the configured path, never from the live name.
    let target = if stale || is_configured(&path) {
        configured.to_path_buf()
    } else {
        path.clone()
    };
    let Some(name) = target.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    let archived = target.with_file_name(format!("{name}.{}", archive_suffix(now)));
    let targets = fds
        .iter()
        .copied()
        .filter(|fd| {
            fd_file(*fd).is_some_and(|file| file.dev == metadata.dev && file.ino == metadata.ino)
        })
        .collect::<Vec<_>>();
    std::fs::rename(&path, &archived)?;
    let fresh = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&target)?;
    for fd in targets {
        // SAFETY: dup2 atomically points `fd` at the fresh file; both
        // descriptors are open for the duration of the call.
        if unsafe { nix::libc::dup2(fresh.as_raw_fd(), fd) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    drop(fresh);
    let pruned = prune_archives(&target, keep)?;
    Ok(Some(Rotation { archived, pruned }))
}

/// Remove all but the newest `keep` rotation archives of `path`.
///
/// Only files named `<name>.<rotation timestamp>` (or the grown chain of
/// them, #2356) are considered, ordered by their latest rotation timestamp.
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
            is_archive_suffix(suffix).then(|| {
                (
                    latest_segment(suffix).to_owned(),
                    file_name.clone(),
                    entry.path(),
                )
            })
        })
        .collect::<Vec<_>>();
    // The timestamp format sorts chronologically.
    archives.sort();
    let archives = archives
        .into_iter()
        .map(|(_, _, path)| path)
        .collect::<Vec<_>>();
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
pub fn spawn_bounded_log(configured: PathBuf) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(CHECK_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let configured = configured.clone();
            let rotation = tokio::task::spawn_blocking(move || {
                rotate_fds_if_needed(
                    &[1, 2],
                    &configured,
                    MAX_LOG_BYTES,
                    KEEP_ARCHIVES,
                    Utc::now(),
                )
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

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
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

        let rotation = rotate_fds_if_needed(
            &[stdout.as_raw_fd(), stderr.as_raw_fd()],
            &path,
            10,
            4,
            at(1),
        )
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
            rotate_fds_if_needed(&[file.as_raw_fd()], &path, 1024, 4, at(2)).unwrap(),
            None
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "short\n");

        let (reader, writer) = nix::unistd::pipe().unwrap();
        assert_eq!(
            rotate_fds_if_needed(&[writer.as_raw_fd()], &path, 0, 4, at(3)).unwrap(),
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

    fn open_append(path: &Path) -> std::fs::File {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
    }

    fn sorted_names(dir: &Path) -> Vec<String> {
        let mut names = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn twelve_consecutive_rotations_keep_names_bounded_and_archives_at_keep() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILENAME);
        let mut stdout = open_append(&path);
        for second in 0..12 {
            stdout.write_all(b"enough bytes to rotate\n").unwrap();
            let rotation =
                rotate_fds_if_needed(&[stdout.as_raw_fd()], &path, 10, KEEP_ARCHIVES, at(second))
                    .unwrap()
                    .expect("rotates");
            let archived = rotation.archived.file_name().unwrap().to_str().unwrap();
            assert!(archived.len() <= 60, "{archived}");
        }
        let names = sorted_names(dir.path());
        assert!(names.iter().all(|name| name.len() <= 60), "{names:?}");
        assert_eq!(names.len(), KEEP_ARCHIVES + 1, "{names:?}");
        assert!(names.contains(&LOG_FILENAME.to_owned()));
        // The newest archives are the ones kept.
        assert!(names.contains(&format!("daemon.log.{}", archive_suffix(at(11)))));
        assert!(!names.contains(&format!("daemon.log.{}", archive_suffix(at(0)))));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn long_named_live_file_recovers_to_the_configured_path_and_prunes_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let configured = dir.path().join(LOG_FILENAME);
        // The chain the bug left: each rotation appended one more timestamp.
        let mut name = LOG_FILENAME.to_owned();
        let mut chain = Vec::new();
        for second in 1..=9 {
            name = format!("{name}.{}", archive_suffix(at(second)));
            chain.push(name.clone());
        }
        assert_eq!(name.len(), 253);
        for archive in &chain[..8] {
            std::fs::write(dir.path().join(archive), b"old archive").unwrap();
        }
        std::fs::write(dir.path().join("daemon.log.bak"), b"operator copy").unwrap();
        let live_path = dir.path().join(&name);
        let mut stdout = open_append(&live_path);
        let mut stderr = stdout.try_clone().unwrap();
        stdout.write_all(b"big live content\n").unwrap();

        // Far below the size cap: the stale name alone triggers recovery.
        let rotation = rotate_fds_if_needed(
            &[stdout.as_raw_fd(), stderr.as_raw_fd()],
            &configured,
            MAX_LOG_BYTES,
            KEEP_ARCHIVES,
            at(30),
        )
        .unwrap()
        .expect("a grown archive name is rotated back");

        let archived = dir
            .path()
            .join(format!("daemon.log.{}", archive_suffix(at(30))));
        assert_eq!(rotation.archived, archived);
        assert_eq!(
            std::fs::read_to_string(&archived).unwrap(),
            "big live content\n"
        );
        stdout.write_all(b"after stdout\n").unwrap();
        stderr.write_all(b"after stderr\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&configured).unwrap(),
            "after stdout\nafter stderr\n"
        );
        let names = sorted_names(dir.path());
        assert!(archived.file_name().unwrap().len() <= 60);
        // The kept archives are the newest by latest rotation: the fresh one
        // and the three newest old chain names (kept as they are).
        for second in 6..=8 {
            assert!(names.contains(&chain[second - 1]), "{names:?}");
        }
        assert!(names.contains(&"daemon.log.bak".to_owned()));
        let archives = names
            .iter()
            .filter(|name| name.starts_with("daemon.log.2"))
            .count();
        assert_eq!(archives, KEEP_ARCHIVES, "{names:?}");
        assert_eq!(rotation.pruned.len(), 5);

        // A second pass is a no-op: the live file is the configured path.
        assert_eq!(
            rotate_fds_if_needed(
                &[stdout.as_raw_fd()],
                &configured,
                MAX_LOG_BYTES,
                KEEP_ARCHIVES,
                at(31)
            )
            .unwrap(),
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn archive_suffix_accepts_timestamps_and_chains_only() {
        let one = archive_suffix(at(1));
        let two = archive_suffix(at(2));
        assert!(is_archive_suffix(&one));
        assert!(is_archive_suffix(&format!("{one}.{two}")));
        assert!(!is_archive_suffix(&format!("{one}.")));
        assert!(!is_archive_suffix(&format!("{one}.bak")));
        assert!(!is_archive_suffix("bak"));
        assert_eq!(latest_segment(&format!("{one}.{two}")), two);
    }
}
