//! Adopt an externally held front door without changing its path or permissions.
use std::ffi::OsStr;
use std::io;
use std::os::fd::BorrowedFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn number(value: &OsStr) -> io::Result<u32> {
    let value = value
        .to_str()
        .ok_or_else(|| invalid("listener environment must be ASCII"))?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid(
            "listener environment must be an unsigned decimal integer",
        ));
    }
    value
        .parse()
        .map_err(|_| invalid("listener environment integer overflow"))
}

fn listener_fd(
    explicit: Option<&OsStr>,
    count: Option<&OsStr>,
    owner: Option<&OsStr>,
    pid: u32,
) -> io::Result<Option<i32>> {
    if let Some(value) = explicit {
        let fd = number(value)?;
        if fd < 3 || fd > i32::MAX as u32 {
            return Err(invalid("RSI_LISTEN_FD must name a descriptor >= 3"));
        }
        return Ok(Some(fd as i32));
    }
    let Some(count) = count else { return Ok(None) };
    // systemd activation is process-addressed. Descendants must not adopt it.
    if owner.map(number).transpose()? != Some(pid) {
        return Ok(None);
    }
    let count = number(count)?;
    if count == 0 {
        return Ok(None);
    }
    #[cfg(not(target_os = "linux"))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "systemd activation requires Linux; use RSI_LISTEN_FD",
    ));
    #[cfg(target_os = "linux")]
    if count == 1 {
        Ok(Some(3))
    } else {
        Err(invalid(
            "rsid requires exactly one systemd listening descriptor",
        ))
    }
}

#[derive(Debug)]
pub struct InheritedListener {
    pub listener: UnixListener,
    // An ENOPROTOOPT accept probe may dequeue a client. Preserve it for rsid.
    pub pending: Option<UnixStream>,
}

impl std::ops::Deref for InheritedListener {
    type Target = UnixListener;
    fn deref(&self) -> &UnixListener {
        &self.listener
    }
}

/// Return a validated listener, or None for the ordinary daemon-owned socket.
/// Invalid activation always fails before store open; it never falls back to bind.
///
/// # Safety
/// Call only during single-threaded startup, before any environment readers or
/// subprocesses exist: successful adoption removes process environment entries.
pub unsafe fn inherited_daemon_listener(path: &Path) -> io::Result<Option<InheritedListener>> {
    let explicit = std::env::var_os("RSI_LISTEN_FD");
    let count = std::env::var_os("LISTEN_FDS");
    let owner = std::env::var_os("LISTEN_PID");
    let fd = listener_fd(
        explicit.as_deref(),
        count.as_deref(),
        owner.as_deref(),
        std::process::id(),
    )?;
    let listener = fd.map(|fd| validated_listener(fd, path)).transpose()?;
    // Consume activation metadata even when it addresses a different process.
    // The supervisor keeps its own environment for the next daemon lifetime.
    for key in [
        "RSI_LISTEN_FD",
        "LISTEN_FDS",
        "LISTEN_PID",
        "LISTEN_FDNAMES",
    ] {
        unsafe { std::env::remove_var(key) };
    }
    Ok(listener)
}

fn validated_listener(fd: i32, path: &Path) -> io::Result<InheritedListener> {
    // Check the raw descriptor before constructing a borrowed Rust descriptor.
    let flags = unsafe { nix::libc::fcntl(fd, nix::libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut accepting: nix::libc::c_int = 0;
    let mut len = std::mem::size_of_val(&accepting) as nix::libc::socklen_t;
    let result = unsafe {
        nix::libc::getsockopt(
            fd,
            nix::libc::SOL_SOCKET,
            nix::libc::SO_ACCEPTCONN,
            (&mut accepting as *mut nix::libc::c_int).cast(),
            &mut len,
        )
    };
    let accepting = if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(accepting)
    };
    let mut address: nix::libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut address_len = std::mem::size_of_val(&address) as nix::libc::socklen_t;
    if unsafe {
        nix::libc::getsockname(
            fd,
            (&mut address as *mut nix::libc::sockaddr_storage).cast(),
            &mut address_len,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    if i32::from(address.ss_family) != nix::libc::AF_UNIX {
        return Err(invalid("inherited listener must use AF_UNIX"));
    }
    let mut socket_type: nix::libc::c_int = 0;
    let mut type_len = std::mem::size_of_val(&socket_type) as nix::libc::socklen_t;
    if unsafe {
        nix::libc::getsockopt(
            fd,
            nix::libc::SOL_SOCKET,
            nix::libc::SO_TYPE,
            (&mut socket_type as *mut nix::libc::c_int).cast(),
            &mut type_len,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    if socket_type != nix::libc::SOCK_STREAM {
        return Err(invalid("inherited Unix listener must use SOCK_STREAM"));
    }
    // Clone rather than take ownership of a raw caller descriptor. Both copies
    // are close-on-exec so provider children cannot hold the daemon front door.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let listener = UnixListener::from(borrowed.try_clone_to_owned()?);
    if listener.local_addr()?.as_pathname() != Some(path) {
        return Err(invalid(
            "inherited Unix listener must be bound at the exact configured socket path",
        ));
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket()
        || metadata.mode() & 0o7777 != 0o600
        || metadata.uid() != unsafe { nix::libc::geteuid() }
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "inherited socket must be a user-owned socket with mode 0600",
        ));
    }
    if unsafe { nix::libc::fcntl(fd, nix::libc::F_SETFD, flags | nix::libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    listener.set_nonblocking(true)?;
    let pending = verify_accepting(&listener, accepting)?;
    Ok(InheritedListener { listener, pending })
}

fn verify_accepting(
    listener: &UnixListener,
    accepting: io::Result<i32>,
) -> io::Result<Option<UnixStream>> {
    match accepting {
        Ok(value) if value != 0 => Ok(None),
        Ok(_) => Err(invalid("inherited descriptor must be a listening socket")),
        Err(error) if error.raw_os_error() == Some(nix::libc::ENOPROTOOPT) => {
            // BSD kernels may not expose SO_ACCEPTCONN. On a validated Unix
            // stream, nonblocking accept distinguishes listening from bound or
            // connected sockets. Keep an accepted client instead of dropping it.
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true)?;
                    Ok(Some(stream))
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    #[ignore = "isolated activation environment fixture"]
    fn activation_child_environment_fixture() {
        let path = std::path::PathBuf::from(std::env::var_os("RSI_TEST_LISTENER_PATH").unwrap());
        // Only this test runs in the child; no runtime or other env readers.
        let listener = unsafe { inherited_daemon_listener(&path) }
            .unwrap()
            .unwrap();
        let status = std::process::Command::new("sh")
            .args([
                "-c",
                "test -z \"${RSI_LISTEN_FD+x}${LISTEN_FDS+x}${LISTEN_PID+x}${LISTEN_FDNAMES+x}\"",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        assert!(listener.local_addr().unwrap().as_pathname() == Some(path.as_path()));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn adopted_activation_is_consumed_before_spawning_children() {
        use std::os::unix::process::CommandExt;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("daemon.sock");
        let holder = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let fd = holder.as_raw_fd();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "instance_guard::inherited_listener::tests::activation_child_environment_fixture",
                "--test-threads=1",
            ])
            .env("RSI_TEST_LISTENER_PATH", &path)
            .env("RSI_LISTEN_FD", fd.to_string())
            .env("LISTEN_FDS", "1")
            .env("LISTEN_PID", "1")
            .env("LISTEN_FDNAMES", "daemon");
        unsafe {
            command.pre_exec(move || {
                if nix::libc::fcntl(fd, nix::libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        assert!(command.status().unwrap().success());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn bsd_acceptconn_values_and_probe_preserve_queued_clients() {
        use std::io::{Read, Write};
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("daemon.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        assert!(verify_accepting(&listener, Ok(2)).unwrap().is_none());
        assert!(verify_accepting(&listener, Ok(0)).is_err());
        let unsupported = || Err(io::Error::from_raw_os_error(nix::libc::ENOPROTOOPT));
        assert!(
            verify_accepting(&listener, unsupported())
                .unwrap()
                .is_none()
        );
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"queued").unwrap();
        let mut pending = verify_accepting(&listener, unsupported()).unwrap().unwrap();
        let mut bytes = [0; 6];
        pending.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"queued");
        assert!(
            verify_accepting(
                &listener,
                Err(io::Error::from_raw_os_error(nix::libc::EBADF))
            )
            .is_err()
        );
        // A stream that never called listen must fail the probe. Use libc,
        // like production, so this test needs no extra nix socket feature.
        use std::os::fd::{FromRawFd, OwnedFd};
        let fd = unsafe { nix::libc::socket(nix::libc::AF_UNIX, nix::libc::SOCK_STREAM, 0) };
        assert!(fd >= 0);
        let raw = unsafe { OwnedFd::from_raw_fd(fd) };
        let unbound = UnixListener::from(raw);
        unbound.set_nonblocking(true).unwrap();
        assert!(verify_accepting(&unbound, unsupported()).is_err());
    }

    fn os(value: &str) -> Option<&OsStr> {
        Some(OsStr::new(value))
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn activation_environment_is_strict_and_process_addressed() {
        assert_eq!(listener_fd(None, None, None, 42).unwrap(), None);
        assert_eq!(
            listener_fd(os("9"), os("2"), os("42"), 42).unwrap(),
            Some(9)
        );
        for value in ["", "-1", "0", "2", "+3", "3 ", "2147483648", "99999999999"] {
            assert!(listener_fd(os(value), None, None, 42).is_err(), "{value}");
        }
        assert_eq!(listener_fd(None, os("1"), os("41"), 42).unwrap(), None);
        assert_eq!(listener_fd(None, os("1"), None, 42).unwrap(), None);
        assert_eq!(listener_fd(None, os("0"), os("42"), 42).unwrap(), None);
        assert!(listener_fd(None, os("2"), os("42"), 42).is_err());
        assert!(listener_fd(None, os("bad"), os("42"), 42).is_err());
        #[cfg(target_os = "linux")]
        assert_eq!(listener_fd(None, os("1"), os("42"), 42).unwrap(), Some(3));
        #[cfg(not(target_os = "linux"))]
        assert_eq!(
            listener_fd(None, os("1"), os("42"), 42).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn listener_validation_preserves_front_door_across_daemon_lifetimes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("daemon.sock");
        let holder = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let before = std::fs::metadata(&path).unwrap();
        let first = validated_listener(holder.as_raw_fd(), &path).unwrap();
        drop(first);
        let client = std::os::unix::net::UnixStream::connect(&path).unwrap();
        let second = validated_listener(holder.as_raw_fd(), &path).unwrap();
        let (_accepted, _) = second.accept().unwrap();
        drop(client);
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(
            (after.ino(), after.uid(), after.mode()),
            (before.ino(), before.uid(), before.mode())
        );
        assert_ne!(
            unsafe { nix::libc::fcntl(second.as_raw_fd(), nix::libc::F_GETFD) }
                & nix::libc::FD_CLOEXEC,
            0
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn listener_validation_refuses_wrong_path_type_state_and_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("daemon.sock");
        let holder = UnixListener::bind(&path).unwrap();
        for mode in [0o666, 0o660, 0o400, 0o1600] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                validated_listener(holder.as_raw_fd(), &path)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(validated_listener(holder.as_raw_fd(), &temp.path().join("wrong.sock")).is_err());
        let file = std::fs::File::open(path.parent().unwrap()).unwrap();
        assert!(validated_listener(file.as_raw_fd(), &path).is_err());
        assert!(validated_listener(-1, &path).is_err());
        let datagram = std::os::unix::net::UnixDatagram::unbound().unwrap();
        assert!(validated_listener(datagram.as_raw_fd(), &path).is_err());
        let (connected, _) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(validated_listener(connected.as_raw_fd(), &path).is_err());
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(validated_listener(tcp.as_raw_fd(), &path).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "replacement").unwrap();
        assert!(validated_listener(holder.as_raw_fd(), &path).is_err());
    }
}
