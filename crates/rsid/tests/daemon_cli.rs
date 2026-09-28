#![allow(clippy::expect_used, clippy::unwrap_used)] // Process fixture assertions.

use std::{path::Path, process::Command};

fn run_flag(flag: &str, socket: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rsid"))
        .arg(flag)
        .env("RSI_DAEMON_SOCKET_PATH", socket)
        .output()
        .expect("rsid flag should exit")
}

#[test]
fn version_and_help_exit_without_starting_a_daemon() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("daemon.sock");
    for flag in ["--version", "--help"] {
        let output = run_flag(flag, &socket);
        assert!(output.status.success(), "{flag}: {output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("rsid "), "{flag}: {stdout}");
        assert!(!socket.exists(), "{flag} started a daemon");
    }
    assert!(
        String::from_utf8(run_flag("--help", &socket).stdout)
            .unwrap()
            .contains("Usage: rsid")
    );
}

#[test]
fn unknown_flag_exits_with_usage() {
    let temp = tempfile::tempdir().unwrap();
    let output = run_flag("--unknown", &temp.path().join("daemon.sock"));
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("Usage: rsid")
    );
}
