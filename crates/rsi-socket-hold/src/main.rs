//! Hold a user-only Unix front door while a supervisor replaces its daemons.
//! This process never accepts traffic. The kernel queues connections between
//! daemon lifetimes. Windows builds the explicit unsupported execution path.
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "Usage: rsi-socket-hold <socket-path> -- <supervisor> [arg...]";

#[cfg_attr(not(unix), allow(dead_code))]
struct Invocation {
    socket: PathBuf,
    argv: Vec<OsString>,
}

fn parse(args: impl IntoIterator<Item = OsString>) -> io::Result<Invocation> {
    let mut args = args.into_iter();
    let socket = args.next().filter(|arg| !arg.is_empty());
    let separator = args.next();
    let argv: Vec<_> = args.collect();
    if socket.is_none()
        || separator.as_deref() != Some(std::ffi::OsStr::new("--"))
        || argv.first().is_none_or(|arg| arg.is_empty())
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE));
    }
    Ok(Invocation {
        socket: socket.unwrap().into(),
        argv,
    })
}

#[cfg(unix)]
mod unix {
    use super::*;
    use nix::errno::Errno;
    use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, SigmaskHow, Signal};
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, connect, socket};
    use nix::unistd::Pid;
    use std::fs::{self, File, OpenOptions};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
    use std::os::unix::net::UnixListener;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::path::Path;
    use std::process::{Command, ExitStatus};

    struct HeldSocket {
        listener: UnixListener,
        _lease: File,
        path: PathBuf,
        identity: (u64, u64),
    }
    impl Drop for HeldSocket {
        fn drop(&mut self) {
            // Do not delete a replacement at our old pathname.
            if fs::symlink_metadata(&self.path)
                .is_ok_and(|meta| (meta.dev(), meta.ino()) == self.identity)
            {
                let _ = fs::remove_file(&self.path);
            }
        }
    }

    fn user_socket(path: &Path) -> io::Result<()> {
        let meta = fs::symlink_metadata(path)?;
        if !meta.file_type().is_socket()
            || meta.mode() & 0o7777 != 0o600
            || meta.uid() != unsafe { nix::libc::geteuid() }
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "socket holder requires a user-owned socket with mode 0600",
            ));
        }
        Ok(())
    }

    fn hold(path: &Path) -> io::Result<HeldSocket> {
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".holder.lock");
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(lock_path)?;
        let meta = lease.metadata()?;
        if !meta.is_file()
            || meta.mode() & 0o7777 != 0o600
            || meta.uid() != unsafe { nix::libc::geteuid() }
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe socket holder lock",
            ));
        }
        lease.try_lock().map_err(io::Error::other)?;
        match fs::symlink_metadata(path) {
            Ok(_) => {
                user_socket(path)?;
                // Nonblocking connect fails closed even when the live backlog
                // is full. Only missing/refused proves a stale front door.
                let probe = socket(
                    AddressFamily::Unix,
                    SockType::Stream,
                    SockFlag::empty(),
                    None,
                )?;
                if unsafe {
                    nix::libc::fcntl(probe.as_raw_fd(), nix::libc::F_SETFL, nix::libc::O_NONBLOCK)
                } < 0
                {
                    return Err(io::Error::last_os_error());
                }
                match connect(probe.as_raw_fd(), &UnixAddr::new(path)?) {
                    Err(Errno::ECONNREFUSED | Errno::ENOENT) => fs::remove_file(path)?,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            "socket is live or unproven; refusing to unlink it",
                        ));
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        // Single-threaded binary: set the mask before bind, so the socket is
        // never briefly world-accessible. No chmod of an existing socket.
        let original_mask = unsafe { nix::libc::umask(0o177) };
        let listener = UnixListener::bind(path);
        // The restrictive socket mask must not make the supervisor's new
        // directories unsearchable. Restore it on bind failure as well.
        unsafe { nix::libc::umask(original_mask) };
        let listener = listener?;
        let meta = fs::symlink_metadata(path)?;
        let held = HeldSocket {
            listener,
            _lease: lease,
            path: path.to_owned(),
            identity: (meta.dev(), meta.ino()),
        };
        user_socket(path)?;
        Ok(held)
    }

    fn spawn_and_wait(invocation: &Invocation, held: &HeldSocket) -> io::Result<ExitStatus> {
        let mut signals = SigSet::empty();
        for sig in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGCHLD] {
            signals.add(sig);
        }
        let mut child_mask = signals.thread_swap_mask(SigmaskHow::SIG_BLOCK)?;
        let default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
        for sig in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGCHLD] {
            unsafe { signal::sigaction(sig, &default)? };
            child_mask.remove(sig);
        }
        let fd = held.listener.as_raw_fd();
        let mut command = Command::new(&invocation.argv[0]);
        command
            .args(&invocation.argv[1..])
            .env("RSI_LISTEN_FD", fd.to_string())
            .env_remove("LISTEN_FDS")
            .env_remove("LISTEN_PID")
            .env_remove("LISTEN_FDNAMES");
        // SAFETY: single-threaded launcher; only fcntl and pthread_sigmask run
        // between fork and exec. The parent retains its close-on-exec listener.
        unsafe {
            command.pre_exec(move || {
                let flags = nix::libc::fcntl(fd, nix::libc::F_GETFD);
                if flags < 0
                    || nix::libc::fcntl(fd, nix::libc::F_SETFD, flags & !nix::libc::FD_CLOEXEC) < 0
                {
                    return Err(io::Error::last_os_error());
                }
                child_mask.thread_set_mask()?;
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let child_pid = Pid::from_raw(child.id() as i32);
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            let sig = signals.wait()?;
            if matches!(sig, Signal::SIGINT | Signal::SIGTERM) {
                // The supervisor forwards this to its current daemon; keep
                // the listener until it has finished draining and is reaped.
                signal::kill(child_pid, sig).or_else(|error| {
                    if error == Errno::ESRCH {
                        Ok(())
                    } else {
                        Err(error)
                    }
                })?;
            }
        }
    }

    pub(super) fn run(invocation: Invocation) -> io::Result<ExitCode> {
        let held = hold(&invocation.socket)?;
        let status = spawn_and_wait(&invocation, &held)?;
        Ok(ExitCode::from(
            status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
                .clamp(0, 255) as u8,
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        // hold() temporarily changes the single-threaded launcher's umask.
        // Keep the library test runner's filesystem fixtures serialized.
        static UMASK_FIXTURES: std::sync::Mutex<()> = std::sync::Mutex::new(());
        #[test]
        fn holder_cleanup_keeps_a_replacement_socket() {
            let _serial = UMASK_FIXTURES.lock().unwrap();
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("daemon.sock");
            let held = hold(&path).unwrap();
            fs::remove_file(&path).unwrap();
            let replacement = UnixListener::bind(&path).unwrap();
            let identity = fs::metadata(&path).unwrap().ino();
            drop(held);
            assert_eq!(fs::metadata(&path).unwrap().ino(), identity);
            let _client = std::os::unix::net::UnixStream::connect(&path).unwrap();
            let _accepted = replacement.accept().unwrap();
        }

        #[test]
        fn holder_refuses_live_and_unsafe_paths_and_reclaims_only_stale_user_sockets() {
            let _serial = UMASK_FIXTURES.lock().unwrap();
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("daemon.sock");
            let held = hold(&path).unwrap();
            assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o600);
            assert!(hold(&path).is_err());
            drop(held);
            let live = UnixListener::bind(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let inode = fs::metadata(&path).unwrap().ino();
            assert!(hold(&path).is_err());
            assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
            drop(live);
            let recovered = hold(&path).unwrap();
            drop(recovered);
            let stale = UnixListener::bind(&path).unwrap();
            drop(stale);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
            assert!(hold(&path).is_err());
            assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o666);
            fs::remove_file(&path).unwrap();
            fs::write(&path, "keep").unwrap();
            assert!(hold(&path).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), "keep");
        }
    }
}

fn main() -> ExitCode {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--help")
    {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let invocation = match parse(std::env::args_os().skip(1)) {
        Ok(invocation) => invocation,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    #[cfg(unix)]
    let result = unix::run(invocation);
    #[cfg(not(unix))]
    let _ = invocation;
    #[cfg(not(unix))]
    let result: io::Result<ExitCode> = Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "canary deploy socket holding is unsupported on this platform",
    ));
    result.unwrap_or_else(|error| {
        eprintln!("rsi-socket-hold: {error}");
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn argv_is_literal_and_requires_path_separator_and_program() {
        let parsed =
            parse(["/tmp/daemon.sock", "--", "supervisor", "a b", "$literal"].map(OsString::from))
                .unwrap();
        assert_eq!(parsed.socket, PathBuf::from("/tmp/daemon.sock"));
        assert_eq!(
            parsed.argv,
            ["supervisor", "a b", "$literal"].map(OsString::from)
        );
        for args in [
            vec![],
            vec!["path"],
            vec!["path", "--"],
            vec!["path", "cmd"],
            vec!["", "--", "cmd"],
        ] {
            assert!(parse(args.into_iter().map(OsString::from)).is_err());
        }
    }
}
