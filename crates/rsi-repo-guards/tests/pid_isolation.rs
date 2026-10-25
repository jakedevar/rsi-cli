//! Executable contract tests. Linux namespace acceptance remains a Linux job;
//! non-Linux and explicit unavailable behavior are exercised on every host.
use serde_json::Value;
use std::process::{Command, Output};

fn cli(args: &[&str], disabled: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rsi-repo-guard"));
    command.arg("pid-isolate").args(args);
    if disabled {
        command.env("RSI_PID_ISOLATION", "off");
    }
    command.output().unwrap()
}
#[test]
fn status_and_require_preserve_unavailable_contract() {
    let done = cli(&["--status"], true);
    assert_eq!(done.status.code(), Some(3));
    assert_eq!(
        serde_json::from_slice::<Value>(&done.stdout).unwrap(),
        serde_json::json!({"mode":"unavailable","reason":"disabled"})
    );
    let done = cli(&["--require", "--", "must-not-start"], true);
    assert_eq!(done.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&done.stderr).contains("--require: refusing"));
    assert_eq!(cli(&[], true).status.code(), Some(2));
    assert_eq!(
        cli(&["--", "missing-command-for-pid-isolate-test"], true)
            .status
            .code(),
        Some(127)
    );
}
#[cfg(unix)]
#[test]
fn disabled_command_preserves_exact_exit_output_and_host_admission() {
    let done = Command::new(env!("CARGO_BIN_EXE_rsi-repo-guard"))
        .args([
            "pid-isolate",
            "--",
            "/bin/sh",
            "-c",
            "printf 'out:%s' \"$RSI_CARGO_SLOT_HELD\"; printf err >&2; exit 7",
        ])
        .env("RSI_PID_ISOLATION", "off")
        .env("RSI_CARGO_SLOT_HELD", "1")
        .output()
        .unwrap();
    assert_eq!(done.status.code(), Some(7));
    assert_eq!(done.stdout, b"out:1");
    let stderr = String::from_utf8_lossy(&done.stderr);
    assert!(stderr.contains("pid_isolation=unavailable reason=disabled"));
    assert!(stderr.ends_with("err"));
}
#[cfg(not(target_os = "linux"))]
#[test]
fn unsupported_host_reports_not_linux() {
    let done = Command::new(env!("CARGO_BIN_EXE_rsi-repo-guard"))
        .args(["pid-isolate", "--status"])
        .env_remove("RSI_PID_ISOLATION")
        .output()
        .unwrap();
    assert_eq!(done.status.code(), Some(3));
    assert_eq!(
        serde_json::from_slice::<Value>(&done.stdout).unwrap()["reason"],
        "not_linux"
    );
}

#[cfg(target_os = "linux")]
fn linux_available() -> bool {
    let done = Command::new(env!("CARGO_BIN_EXE_rsi-repo-guard"))
        .args(["pid-isolate", "--status"])
        .env_remove("RSI_PID_ISOLATION")
        .output()
        .unwrap();
    let status: Value = serde_json::from_slice(&done.stdout).unwrap();
    if status["mode"] == "namespaced" {
        assert!(done.status.success());
        true
    } else {
        assert_eq!(done.status.code(), Some(3));
        assert!(!status["reason"].as_str().unwrap().is_empty());
        eprintln!("SKIP real PID namespace integration: production fallback status {status}");
        false
    }
}
#[cfg(target_os = "linux")]
#[test]
fn real_namespace_hides_host_decoy_and_preserves_exit_output() {
    if !linux_available() {
        return;
    }
    // This exact owned child is cleaned up even if the assertion panics.
    struct Decoy(std::process::Child);
    impl Drop for Decoy {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut decoy = Decoy(Command::new("sleep").arg("30").spawn().unwrap());
    // The wrapped command runs as a child of the reaping init (hub #3667), which
    // is PID 1; the command is never PID 1 itself, so the PID-1 `--canary`
    // entry point cannot be the wrapped command (it exits 11, silently). Check
    // the contract from inside the namespace instead: the fresh /proc shows no
    // host decoy or host test process, and the command's parent is the init.
    let check = r#"[ "$PPID" = 1 ] || exit 11
for host_pid in "$@"; do
  [ ! -e "/proc/$host_pid" ] || exit 12
done"#;
    let done = cli(
        &[
            "--require",
            "--",
            "/bin/sh",
            "-c",
            check,
            "pid-isolate-check",
            &decoy.0.id().to_string(),
            &std::process::id().to_string(),
        ],
        false,
    );
    assert!(
        done.status.success(),
        "{}",
        String::from_utf8_lossy(&done.stderr)
    );
    assert!(decoy.0.try_wait().unwrap().is_none());
    let done = cli(
        &[
            "--require",
            "--",
            "/bin/sh",
            "-c",
            "echo out; echo err >&2; exit 7",
        ],
        false,
    );
    assert_eq!(done.status.code(), Some(7));
    assert_eq!(done.stdout, b"out\n");
    assert!(String::from_utf8_lossy(&done.stderr).contains("pid_isolation=namespaced"));
    assert!(String::from_utf8_lossy(&done.stderr).ends_with("err\n"));
}

// The reaping init (hub #3667): inside the PID namespace the command's parent is PID 1, which
// must wait on orphans. Run in the real binary because the init calls waitpid(-1).
#[cfg(unix)]
fn init(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rsi-repo-guard"))
        .args(["pid-isolate", "--init", "--"])
        .args(args)
        .output()
        .unwrap()
}
#[cfg(unix)]
#[test]
fn init_preserves_exit_status_output_signals_and_spawn_failure() {
    let done = init(&["/bin/sh", "-c", "echo out; echo err >&2; exit 7"]);
    assert_eq!(done.status.code(), Some(7));
    assert_eq!(done.stdout, b"out\n");
    assert_eq!(done.stderr, b"err\n");
    assert_eq!(
        init(&["/bin/sh", "-c", "kill -9 $$"]).status.code(),
        Some(137)
    );
    let missing = init(&["/nonexistent/pid-init-command"]);
    assert_eq!(missing.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("cannot run"));
    let none = Command::new(env!("CARGO_BIN_EXE_rsi-repo-guard"))
        .args(["pid-isolate", "--init"])
        .output()
        .unwrap();
    assert_eq!(none.status.code(), Some(2));
}
#[cfg(unix)]
#[test]
fn init_forwards_termination_to_the_command() {
    // The command ignores nothing: SIGTERM sent to the init must stop it (143 = 128+15).
    let mut child = Command::new(env!("CARGO_BIN_EXE_rsi-repo-guard"))
        .args([
            "pid-isolate",
            "--init",
            "--",
            "/bin/sh",
            "-c",
            "sleep 1; kill -TERM $PPID; sleep 30",
        ])
        .spawn()
        .unwrap();
    let done = child.wait().unwrap();
    assert_eq!(done.code(), Some(143));
}

/// Spawns an orphaned grandchild, kills it, and requires `kill(pid, 0)` to report ESRCH
/// (the shell's `kill -0` fails) within five seconds. An unreaped zombie keeps answering.
#[cfg(target_os = "linux")]
const ORPHAN_SCRIPT: &str = r#"
f="$1"
sh -c 'sleep 300 >/dev/null 2>&1 </dev/null & echo $! > "$0"' "$f"
pid=$(cat "$f")
kill "$pid"
i=0
while [ "$i" -lt 50 ]; do
  kill -0 "$pid" 2>/dev/null || exit 0
  sleep 0.1
  i=$((i + 1))
done
echo "orphan $pid still answers kill(pid, 0)" >&2
exit 1
"#;
#[cfg(target_os = "linux")]
#[test]
fn init_reaps_an_orphaned_grandchild_so_kill_zero_reports_esrch() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("orphan.pid");
    // Subreaper mode: the init adopts the orphan even without a PID namespace.
    let done = init(&[
        "/bin/sh",
        "-c",
        ORPHAN_SCRIPT,
        "sh",
        pid_file.to_str().unwrap(),
    ]);
    assert!(
        done.status.success(),
        "{}",
        String::from_utf8_lossy(&done.stderr)
    );
}
#[cfg(target_os = "linux")]
#[test]
fn namespaced_command_orphans_are_reaped_by_the_init() {
    if !linux_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("orphan.pid");
    let done = cli(
        &[
            "--require",
            "--",
            "/bin/sh",
            "-c",
            ORPHAN_SCRIPT,
            "sh",
            pid_file.to_str().unwrap(),
        ],
        false,
    );
    assert!(
        done.status.success(),
        "{}",
        String::from_utf8_lossy(&done.stderr)
    );
}
