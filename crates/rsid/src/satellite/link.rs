//! Custody checks for a hub-visible forwarded Unix socket.
//!
//! These checks are deliberately independent of the SSH direction. A socket
//! with the right mode is only a local transport endpoint; the peer identity
//! still has to be checked over that endpoint before its data is trusted.

use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path};
use std::time::Duration;
use tokio::net::UnixStream;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

/// Require a single socket directly below the private satellite directory.
/// Every path component is inspected without following symlinks.
fn inspect_socket(root: &Path, socket: &Path, expected_uid: u32) -> io::Result<SocketIdentity> {
    if !root.is_absolute()
        || socket.parent() != Some(root)
        || socket.file_name().is_none()
        || root
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid(
            "satellite socket must be directly inside an absolute satellite directory",
        ));
    }

    let mut component_path = std::path::PathBuf::new();
    for component in root.components() {
        component_path.push(component.as_os_str());
        if fs::symlink_metadata(&component_path)?
            .file_type()
            .is_symlink()
        {
            return Err(invalid("satellite socket path contains a symlink"));
        }
    }

    let directory = fs::symlink_metadata(root)?;
    if !directory.is_dir()
        || directory.uid() != expected_uid
        || directory.permissions().mode() & 0o7777 != 0o700
    {
        return Err(invalid(
            "satellite directory must be owner-owned and mode 0700",
        ));
    }

    let metadata = fs::symlink_metadata(socket)?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != expected_uid
        || metadata.permissions().mode() & 0o7777 != 0o600
    {
        return Err(invalid(
            "satellite endpoint must be an owner-owned mode 0600 socket",
        ));
    }
    Ok(SocketIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

/// Connect only to a socket whose owner, mode, type and inode remain stable.
/// The caller must still run the versioned identity handshake on this stream.
pub(crate) async fn connect_owned_socket(
    root: &Path,
    socket: &Path,
    deadline: Duration,
) -> io::Result<UnixStream> {
    // SAFETY: geteuid has no preconditions and does not mutate process state.
    let uid = unsafe { nix::libc::geteuid() };
    let before = inspect_socket(root, socket, uid)?;
    let stream = tokio::time::timeout(deadline, UnixStream::connect(socket))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "satellite socket connect timed out",
            )
        })??;
    if inspect_socket(root, socket, uid)? != before {
        return Err(invalid("satellite socket changed during connect"));
    }
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;

    fn fixture() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        UnixListener,
    ) {
        let temp = tempfile::Builder::new()
            .prefix("sat-link-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = temp.path().join("satellites");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = root.join("peer.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        (temp, root, socket, listener)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn private_socket_connects_and_rejects_insecure_custody() {
        let (_temp, root, socket, _listener) = fixture();
        connect_owned_socket(&root, &socket, Duration::from_millis(200))
            .await
            .unwrap();

        fs::set_permissions(&socket, fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(
            connect_owned_socket(&root, &socket, Duration::from_millis(200))
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            connect_owned_socket(&root, &socket, Duration::from_millis(200))
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn symlink_and_wrong_owner_are_refused() {
        let (temp, root, socket, _listener) = fixture();
        let alias = temp.path().join("alias");
        symlink(&root, &alias).unwrap();
        let socket_alias = root.join("alias.sock");
        symlink(&socket, &socket_alias).unwrap();
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { nix::libc::geteuid() };
        assert_eq!(
            inspect_socket(&alias, &alias.join("peer.sock"), uid)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            inspect_socket(&root, &socket, uid.wrapping_add(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            inspect_socket(&root, &socket_alias, uid)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            inspect_socket(&root, &temp.path().join("elsewhere.sock"), uid)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn two_independent_links_can_be_checked() {
        let (_temp, root, first, _first_listener) = fixture();
        let second = root.join("second.sock");
        let _second_listener = UnixListener::bind(&second).unwrap();
        fs::set_permissions(&second, fs::Permissions::from_mode(0o600)).unwrap();
        for socket in [&first, &second] {
            connect_owned_socket(&root, socket, Duration::from_millis(200))
                .await
                .unwrap();
        }
    }
}
