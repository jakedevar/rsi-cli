//! Exercise a real held socket through the shipped supervisor and Unix signals.
#![cfg(unix)]
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Holder(Child);
impl Holder {
    fn start(path: &Path, argv: &[&Path]) -> Self {
        Self(
            Command::new(env!("CARGO_BIN_EXE_rsi-socket-hold"))
                // Durable jobs inherit the daemon's supervisor environment.
                // A fixture must refresh its private script and use a fresh
                // restart budget, regardless of how the live daemon launched.
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .arg(path)
                .arg("--")
                .args(argv)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(fs::File::create(path.with_extension("holder.log")).unwrap())
                .spawn()
                .unwrap(),
        )
    }
    fn wait(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "holder did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Holder {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = kill(Pid::from_raw(self.0.id() as i32), Signal::SIGTERM);
            for _ in 0..100 {
                if self.0.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
fn wait_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "missing fixture marker {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn executable(path: &Path, code: &str) {
    fs::write(path, format!("#!/usr/bin/env python3\n{code}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn reply(path: &Path) -> String {
    let mut stream = UnixStream::connect(path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(b"request\n").unwrap();
    let mut result = String::new();
    stream.read_to_string(&mut result).unwrap();
    result
}

#[test]
fn supervisor_restarts_and_refreshes_with_one_user_only_front_door() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("daemon.sock");
    let daemon = temp.path().join("rsid");
    let supervisor = temp.path().join("rsid-supervisor.sh");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/rsid-supervisor.sh");
    fs::copy(source, &supervisor).unwrap();
    fs::set_permissions(&supervisor, fs::Permissions::from_mode(0o700)).unwrap();
    executable(
        &daemon,
        r#"import fcntl, os, pathlib, socket, sys
root = pathlib.Path(__file__).parent
counter = root / 'starts'
turn = int(counter.read_text()) + 1 if counter.exists() else 1
counter.write_text(str(turn))
fd = int(os.environ['RSI_LISTEN_FD'])
fcntl.fcntl(fd, fcntl.F_SETFD, fcntl.FD_CLOEXEC)
listener = socket.socket(fileno=fd)
(root / ('ready' + str(turn))).touch()
stream, _ = listener.accept()
stream.recv(1024)
stream.sendall(str(turn).encode())
stream.close()
listener.close()
if turn == 1:
    script = root / 'rsid-supervisor.sh'
    script.write_text(script.read_text() + '\n# fixture refresh\n')
    (root / 'gap').touch()
    sys.exit(75)
sys.exit(23)
"#,
    );
    let mut holder = Holder::start(&path, &[&supervisor, &daemon]);
    wait_file(&temp.path().join("ready1"));
    let before = fs::metadata(&path).unwrap();
    assert_eq!(before.mode() & 0o7777, 0o600);
    assert_eq!(before.uid(), unsafe { nix::libc::geteuid() });
    assert_eq!(reply(&path), "1");
    wait_file(&temp.path().join("gap"));
    // The old daemon has closed its listener. This client queues during the
    // supervisor's restart backoff, then the next daemon reads it once.
    let mut queued = UnixStream::connect(&path).unwrap();
    let between = fs::metadata(&path).unwrap();
    assert_eq!(
        (between.dev(), between.ino(), between.uid(), between.mode()),
        (before.dev(), before.ino(), before.uid(), before.mode())
    );
    queued
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    queued.write_all(b"request\n").unwrap();
    let mut second_reply = String::new();
    queued.read_to_string(&mut second_reply).unwrap();
    assert_eq!(second_reply, "2");
    assert_eq!(holder.wait().code(), Some(23));
    assert_eq!(fs::read_to_string(temp.path().join("starts")).unwrap(), "2");
    assert!(
        temp.path().join("rsid-supervisor.sh.last-good").is_file(),
        "supervisor log: {}",
        fs::read_to_string(path.with_extension("holder.log")).unwrap()
    );
    let lock = fs::File::open(temp.path().join("daemon.sock.holder.lock")).unwrap();
    lock.try_lock().unwrap();
    // Holder cleanup permits a subsequent launch at the same path.
    let next = std::os::unix::net::UnixListener::bind(&path).unwrap();
    drop(next);
}

#[test]
fn holder_forwards_stop_signals_and_reaps_before_releasing_socket() {
    for signal in [Signal::SIGTERM, Signal::SIGINT] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("daemon.sock");
        let child = temp.path().join("child");
        executable(
            &child,
            r#"import os, pathlib, signal, sys, time
root = pathlib.Path(__file__).parent
def stopped(signum, frame):
    (root / 'stopped').write_text(str(signum))
    sys.exit(23)
signal.signal(signal.SIGTERM, stopped)
signal.signal(signal.SIGINT, stopped)
(root / 'ready').touch()
while True: time.sleep(1)
"#,
        );
        let mut holder = Holder::start(&path, &[&child]);
        wait_file(&temp.path().join("ready"));
        let before = fs::metadata(&path).unwrap();
        let lock = fs::File::open(temp.path().join("daemon.sock.holder.lock")).unwrap();
        assert!(lock.try_lock().is_err());
        kill(Pid::from_raw(holder.0.id() as i32), signal).unwrap();
        assert_eq!(holder.wait().code(), Some(23));
        assert_eq!(
            fs::read_to_string(temp.path().join("stopped")).unwrap(),
            (signal as i32).to_string()
        );
        lock.try_lock().unwrap();
        assert_eq!(before.mode() & 0o7777, 0o600);
    }
}

#[test]
fn spawn_failure_releases_the_front_door_and_lease() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("daemon.sock");
    let mut holder = Holder::start(&path, &[&temp.path().join("missing")]);
    assert_eq!(holder.wait().code(), Some(1));
    let lock = fs::File::open(temp.path().join("daemon.sock.holder.lock")).unwrap();
    lock.try_lock().unwrap();
    let next = std::os::unix::net::UnixListener::bind(&path).unwrap();
    drop(next);
}
