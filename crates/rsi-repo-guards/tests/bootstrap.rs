//! Real staged shell wrapper, compiled Rust client and Unix governor stub.
#![cfg(unix)]
use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    dir: tempfile::TempDir,
    requests: std::thread::JoinHandle<Vec<Value>>,
    socket: PathBuf,
}
impl Fixture {
    fn new(queued: bool) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("rsi-bootstrap-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = dir.path().join("s");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(8);
            let mut requests = Vec::new();
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut line = String::new();
                        std::io::BufReader::new(stream.try_clone().unwrap())
                            .read_line(&mut line)
                            .unwrap();
                        let request: Value = serde_json::from_str(&line).unwrap();
                        let release = request["method"] == "ReleaseAdmission";
                        let result = if release {
                            json!({"released":true})
                        } else if queued {
                            json!({"status":"queued","ticket_id":"ticket","position":0,"message":"memory wait"})
                        } else {
                            json!({"status":"granted","lease_id":"lease"})
                        };
                        writeln!(
                            stream,
                            "{}",
                            json!({"jsonrpc":"2.0","id":1,"result":result})
                        )
                        .unwrap();
                        requests.push(request);
                        if release {
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("{error}"),
                }
            }
            requests
        });
        Self {
            dir,
            requests,
            socket,
        }
    }
    fn command(&self, script: &str) -> Command {
        let root = PathBuf::from(rsi_common::manifest_dir!())
            .join("../..")
            .canonicalize()
            .unwrap();
        let binary = PathBuf::from(env!("CARGO_BIN_EXE_rsi-repo-guard"));
        let target = binary.parent().unwrap().parent().unwrap();
        let mut command = Command::new("/bin/bash");
        command
            .arg(root.join("scripts/cargo-slot-rust"))
            .args(["/bin/sh", "-c", script])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.dir.path())
            .env("CARGO_TARGET_DIR", target)
            .env("RSI_DAEMON_SOCKET_PATH", &self.socket)
            .env("RSI_CARGO_ADMISSION_MAX_SEC", "3")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

#[test]
fn governed_wrapper_preserves_exit_and_releases_lease() {
    let fixture = Fixture::new(false);
    let result = fixture
        .command("printf command-ran; exit 7")
        .output()
        .unwrap();
    assert_eq!(
        result.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"command-ran");
    let requests = fixture.requests.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["params"]["class"], "build");
    assert_eq!(requests[1]["params"]["lease_id"], "lease");
}
#[test]
fn queued_timeout_cancels_ticket_without_starting_command() {
    let fixture = Fixture::new(true);
    let start = Instant::now();
    let result = fixture
        .command("printf command-ran")
        .env("RSI_CARGO_ADMISSION_MAX_SEC", "1")
        .output()
        .unwrap();
    assert_eq!(
        result.status.code(),
        Some(124),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(result.stdout.is_empty());
    assert!(start.elapsed() < Duration::from_secs(5));
    let requests = fixture.requests.join().unwrap();
    assert_eq!(requests.last().unwrap()["params"]["lease_id"], "ticket");
    assert_eq!(requests.last().unwrap()["method"], "ReleaseAdmission");
}
#[test]
fn termination_stops_owned_descendants_and_releases_lease() {
    let fixture = Fixture::new(false);
    let mut child = fixture.command("printf ready; sleep 30").spawn().unwrap();
    let mut ready = [0; 5];
    if let Err(error) = child.stdout.as_mut().unwrap().read_exact(&mut ready) {
        let result = child.wait_with_output().unwrap();
        panic!(
            "wrapper closed before readiness: {error}; status={:?}; stderr={}",
            result.status.code(),
            String::from_utf8_lossy(&result.stderr)
        );
    }
    assert_eq!(&ready, b"ready");
    let start = Instant::now();
    // SAFETY: this is precisely the wrapper process created and owned above.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let result = child.wait_with_output().unwrap();
    assert_eq!(
        result.status.code(),
        Some(143),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(start.elapsed() < Duration::from_secs(3));
    let requests = fixture.requests.join().unwrap();
    assert_eq!(requests.last().unwrap()["params"]["lease_id"], "lease");
}
