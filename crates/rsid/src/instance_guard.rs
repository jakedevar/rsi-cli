//! Process-wide custody for one `rsid` database.
//!
//! The daemon's crash reconciliation assumes that no other daemon still owns
//! live provider processes for the same Store. Keep that assumption structural:
//! acquire this guard before opening or migrating the database and retain it
//! until socket cleanup at shutdown.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

const INCUMBENT_METADATA_MAX_BYTES: u64 = 4 * 1024;
const SOCKET_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// An exclusive kernel lease for one daemon database.
///
/// The lock file deliberately persists after shutdown. Authority is the lock
/// held by this open descriptor, never file existence, so a crash releases the
/// lease without an unlink/recreate race.
#[derive(Debug)]
#[must_use = "dropping the guard releases the daemon's single-instance lease"]
pub struct DaemonInstanceGuard {
    _file: File,
    path: PathBuf,
}

impl DaemonInstanceGuard {
    /// Acquire the database-adjacent lease and publish bounded diagnostics.
    ///
    /// This must run before `Store::open`: Store open may migrate or reconcile
    /// shared state, which is precisely what the lease serializes.
    pub fn acquire(database_path: &Path, socket_path: &Path) -> io::Result<Self> {
        let path = lock_path_for_database(database_path);
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW);
        let mut file = options.open(&path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot open rsid instance lock {} for database {}: {error}",
                    path.display(),
                    database_path.display()
                ),
            )
        })?;

        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let incumbent = read_incumbent_metadata(&mut file);
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!(
                        "another rsid instance already owns database {} via lock {}{}",
                        database_path.display(),
                        path.display(),
                        incumbent
                            .filter(|value| !value.is_empty())
                            .map_or_else(String::new, |value| format!("; incumbent={value}"))
                    ),
                ));
            }
            Err(TryLockError::Error(error)) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "cannot acquire rsid instance lock {} for database {}: {error}",
                        path.display(),
                        database_path.display()
                    ),
                ));
            }
        }

        // Existing lock files can predate the owner-only mode. Correct the mode
        // only after acquiring authority, so two contenders never race writes.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.set_len(0)?;
        file.rewind()?;
        let metadata = serde_json::json!({
            "pid": std::process::id(),
            "acquired_at": chrono::Utc::now().to_rfc3339_opts(
                chrono::SecondsFormat::Nanos,
                true,
            ),
            "database": database_path.to_string_lossy(),
            "socket": socket_path.to_string_lossy(),
        });
        serde_json::to_writer(&mut file, &metadata).map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        file.sync_data()?;

        Ok(Self { _file: file, path })
    }

    /// Path used for diagnostics. The file itself carries no authority.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Resolve `rsi.db` to `rsi.db.lock` without inventing a second state root.
pub fn lock_path_for_database(database_path: &Path) -> PathBuf {
    let mut lock_path = database_path.as_os_str().to_os_string();
    lock_path.push(".lock");
    PathBuf::from(lock_path)
}

/// Refuse to unlink a socket that still accepts local connections.
///
/// This is the first-rollout bridge for an incumbent binary that predates the
/// advisory lock. `NotFound` and `ConnectionRefused` are the only stale states;
/// permissions, type errors, and all other ambiguity fail closed.
pub async fn refuse_live_daemon_socket(socket_path: &Path) -> io::Result<()> {
    match tokio::time::timeout(
        SOCKET_PROBE_TIMEOUT,
        tokio::net::UnixStream::connect(socket_path),
    )
    .await
    {
        Ok(Ok(_)) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "another rsid instance accepts connections at {}; refusing to unlink its socket",
                socket_path.display()
            ),
        )),
        Ok(Err(error))
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(())
        }
        Ok(Err(error)) => Err(io::Error::new(
            error.kind(),
            format!(
                "cannot prove daemon socket {} is stale; refusing to unlink it: {error}",
                socket_path.display()
            ),
        )),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "daemon socket probe for {} exceeded {} ms; refusing to unlink an unproven socket",
                socket_path.display(),
                SOCKET_PROBE_TIMEOUT.as_millis()
            ),
        )),
    }
}

fn read_incumbent_metadata(file: &mut File) -> Option<String> {
    file.rewind().ok()?;
    let mut bytes = Vec::new();
    file.take(INCUMBENT_METADATA_MAX_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn exclusive_database_lease_reports_incumbent_and_releases_on_drop() {
        let temp = tempfile::tempdir().expect("tempdir");
        let database = temp.path().join("rsi.db");
        let socket = temp.path().join("daemon.sock");

        let first = DaemonInstanceGuard::acquire(&database, &socket).expect("first lease");
        assert_eq!(first.path(), temp.path().join("rsi.db.lock"));

        let error = DaemonInstanceGuard::acquire(&database, &socket)
            .expect_err("second lease must be refused");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(error.to_string().contains(&std::process::id().to_string()));
        assert!(error.to_string().contains("incumbent="));

        drop(first);
        let _reacquired =
            DaemonInstanceGuard::acquire(&database, &socket).expect("lease after release");
    }

    #[test]
    fn database_directories_have_independent_leases() {
        let first_temp = tempfile::tempdir().expect("first tempdir");
        let second_temp = tempfile::tempdir().expect("second tempdir");
        let first = DaemonInstanceGuard::acquire(
            &first_temp.path().join("rsi.db"),
            &first_temp.path().join("daemon.sock"),
        )
        .expect("first lease");
        let second = DaemonInstanceGuard::acquire(
            &second_temp.path().join("rsi.db"),
            &second_temp.path().join("daemon.sock"),
        )
        .expect("independent lease");
        assert_ne!(first.path(), second.path());
    }

    #[tokio::test]
    async fn live_socket_is_refused_and_stale_socket_is_recoverable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket = temp.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("bind listener");

        let error = refuse_live_daemon_socket(&socket)
            .await
            .expect_err("live socket refused");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(socket.exists(), "probe must not unlink the live socket");

        drop(listener);
        refuse_live_daemon_socket(&socket)
            .await
            .expect("dropped listener is stale");
        assert!(
            socket.exists(),
            "caller remains responsible for stale cleanup"
        );
    }

    #[tokio::test]
    async fn absent_socket_is_recoverable() {
        let temp = tempfile::tempdir().expect("tempdir");
        refuse_live_daemon_socket(&temp.path().join("missing.sock"))
            .await
            .expect("missing socket is stale");
    }
}
