//! End-to-end shim contract: real children, files, locks and Unix signals.
#![cfg(unix)]

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde_json::Value;
use std::fs::{self, File};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

struct Turn(Child);

impl Turn {
    fn start(dir: &Path, stdin: Option<&Path>, argv: &[&str]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rsi-turn-shim"));
        command.arg("--spool-dir").arg(dir);
        if let Some(path) = stdin {
            command.arg("--stdin-file").arg(path);
        }
        // A vanished launcher cannot break the provider's stdio.
        Self(
            command
                .arg("--")
                .args(argv)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }

    fn signal(&self, signal: Signal) {
        kill(Pid::from_raw(self.0.id() as i32), signal).unwrap();
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "shim did not finish within 8s");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            // Forward termination first; kill only this shim if it is stuck.
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

fn wait_for_output(dir: &Path, expected: &[&str]) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let output = fs::read_to_string(dir.join("stdout")).unwrap_or_default();
        if expected.iter().all(|text| output.contains(text)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "provider never became ready: {output}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn exit_record(dir: &Path) -> Value {
    serde_json::from_slice(&fs::read(dir.join("exit.json")).unwrap()).unwrap()
}

fn assert_unlocked(dir: &Path) {
    let lock = File::open(dir.join("alive.lock")).unwrap();
    lock.try_lock().unwrap();
}

#[test]
fn spools_append_with_file_stdin_and_atomic_nonzero_exit() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("turn");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("stdout"), "prior-out\n").unwrap();
    fs::write(dir.join("stderr"), "prior-err\n").unwrap();
    let input = temp.path().join("prompt");
    fs::write(&input, "prompt\n").unwrap();
    let mut turn = Turn::start(
        &dir,
        Some(&input),
        &["sh", "-c", "cat; printf 'new-err\\n' >&2; exit 23"],
    );
    assert_eq!(turn.wait().code(), Some(23));
    assert_eq!(
        fs::read_to_string(dir.join("stdout")).unwrap(),
        "prior-out\nprompt\n"
    );
    assert_eq!(
        fs::read_to_string(dir.join("stderr")).unwrap(),
        "prior-err\nnew-err\n"
    );
    assert_eq!(
        exit_record(&dir),
        serde_json::json!({"exit_code":23,"signal":null,"error":null})
    );
    assert!(fs::symlink_metadata(dir.join(".exit.json.tmp")).is_err());
    assert_unlocked(&dir);
}

#[test]
fn null_stdin_and_literal_argv_survive_invocation_reuse() {
    let temp = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut turn = Turn::start(
            temp.path(),
            None,
            &[
                "sh",
                "-c",
                "cat; printf '%s\\n' \"$1\"",
                "sh",
                "--stdin-file",
            ],
        );
        assert!(turn.wait().success());
        assert_eq!(exit_record(temp.path())["exit_code"], 0);
        assert_unlocked(temp.path());
    }
    assert_eq!(
        fs::read_to_string(temp.path().join("stdout")).unwrap(),
        "--stdin-file\n--stdin-file\n"
    );
}

#[test]
fn lifetime_lock_refuses_duplicate_and_publishes_exit_before_release() {
    let temp = tempfile::tempdir().unwrap();
    let mut turn = Turn::start(temp.path(), None, &["sh", "-c", "echo ready; exec sleep 5"]);
    wait_for_output(temp.path(), &["ready"]);
    let lock = File::open(temp.path().join("alive.lock")).unwrap();
    assert!(matches!(
        lock.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    assert!(fs::symlink_metadata(temp.path().join("exit.json")).is_err());
    let mut duplicate = Turn::start(temp.path(), None, &["sh", "-c", "echo duplicate"]);
    assert!(!duplicate.wait().success());
    assert_eq!(
        fs::read_to_string(temp.path().join("stdout")).unwrap(),
        "ready\n"
    );
    turn.signal(Signal::SIGTERM);
    assert_eq!(turn.wait().code(), Some(143));
    assert_eq!(exit_record(temp.path())["signal"], 15);
    assert_unlocked(temp.path());
}

#[test]
fn sigterm_reaches_provider_and_its_descendant_group() {
    let temp = tempfile::tempdir().unwrap();
    let mut turn = Turn::start(
        temp.path(),
        None,
        &[
            "sh",
            "-c",
            "trap 'echo provider-term; wait; exit 42' TERM; \
             sh -c 'trap \"echo descendant-term; exit 0\" TERM; echo descendant-ready; sleep 5' & \
             echo provider-ready; wait",
        ],
    );
    wait_for_output(temp.path(), &["provider-ready", "descendant-ready"]);
    turn.signal(Signal::SIGTERM);
    assert_eq!(turn.wait().code(), Some(42));
    let output = fs::read_to_string(temp.path().join("stdout")).unwrap();
    assert!(output.contains("provider-term"));
    assert!(output.contains("descendant-term"));
    assert_eq!(exit_record(temp.path())["exit_code"], 42);
    assert_unlocked(temp.path());
}

#[test]
fn sigint_reaches_provider_with_signal_exit_record() {
    let temp = tempfile::tempdir().unwrap();
    let mut turn = Turn::start(temp.path(), None, &["sh", "-c", "echo ready; exec sleep 5"]);
    wait_for_output(temp.path(), &["ready"]);
    turn.signal(Signal::SIGINT);
    assert_eq!(turn.wait().code(), Some(130));
    assert_eq!(
        exit_record(temp.path()),
        serde_json::json!({"exit_code":null,"signal":2,"error":null})
    );
    assert_unlocked(temp.path());
}

#[test]
fn spawn_failure_is_durable_and_releases_lock() {
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("missing-provider");
    let mut turn = Turn::start(temp.path(), None, &[missing.to_str().unwrap()]);
    assert_eq!(turn.wait().code(), Some(1));
    let record = exit_record(temp.path());
    assert!(record["exit_code"].is_null());
    assert!(record["signal"].is_null());
    assert!(record["error"].as_str().unwrap().contains("No such file"));
    assert_unlocked(temp.path());
}

#[test]
fn identity_names_shim_and_force_stop_reaps_a_term_resistant_provider() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("turn");
    let mut turn = Turn::start(
        &dir,
        None,
        &[
            "sh",
            "-c",
            "trap '' TERM INT; echo ready; while :; do sleep 1; done",
        ],
    );
    wait_for_output(&dir, &["ready"]);
    let identity: Value =
        serde_json::from_slice(&fs::read(dir.join("shim.json")).unwrap()).unwrap();
    assert_eq!(identity["pid"], turn.0.id());
    turn.signal(Signal::SIGQUIT);
    assert_eq!(turn.wait().code(), Some(137));
    assert_eq!(exit_record(&dir)["signal"], 9);
    assert_unlocked(&dir);
}
