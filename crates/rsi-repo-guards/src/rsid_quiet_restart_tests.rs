//! Case map for `rsid-quiet-restart` (#465), ported from the Python-backed section of
//! `scripts/tests/test_rsid_quiet_restart.sh` (the shell cases run the same
//! scenarios through `request_quiet_restart`):
//!
//! * `pending_reply_exits_zero_and_sends_the_documented_params`: a staged request
//!   (exit 0, status on stdout, `RequestOperatorRestart` with `binaries_dir` and
//!   `max_wait_secs`).
//! * `not_under_the_supervisor_exits_ten`: the `deploy_needs_supervisor` refusal.
//! * `empty_malformed_and_non_rpc_replies_are_ambiguous`: exit 1 naming `NOW=1`
//!   (negative: never a restart).
//! * `missing_client_and_hung_client_are_ambiguous`: negative cases, one with the
//!   timeout shortened through `RSID_QUIET_RESTART_RPC_TIMEOUT_SECS`.
//! * `other_refusals_stop_and_name_now`: any other `error` (negative).
//! * `absent_and_stale_sockets_are_down_evidence`: exit 11 and the RPC is never run.
//! * `a_non_numeric_wait_is_refused`: negative (the old heredoc died on `int()`).
//! * `usage_error`: missing arguments.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;

use super::main;
use crate::Ctx;
use crate::testutil;

fn mock_rpc(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    let args = dir.join(format!("{name}.args"));
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\necho \"$@\" > '{}'\ncat <<'EOF'\n{body}\nEOF\n",
            args.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

fn live_socket(dir: &Path) -> (String, UnixListener) {
    let path = dir.join("live.sock");
    let listener = UnixListener::bind(&path).unwrap();
    (path.to_string_lossy().into_owned(), listener)
}

fn run(args: &[&str], vars: &[(&str, &str)]) -> (i32, String, String) {
    testutil::run(main, args, &Ctx::new(".", vars))
}

#[test]
fn pending_reply_exits_zero_and_sends_the_documented_params() {
    let dir = testutil::short_tempdir();
    let (socket, _listener) = live_socket(dir.path());
    let rpc = mock_rpc(
        dir.path(),
        "pending",
        r#"{"jsonrpc":"2.0","id":1,"result":{"pending":true,"sha":"abc","release_by":"2026-10-03T00:00:00Z"}}"#,
    );
    let (code, out, _) = run(&[&rpc, &socket, "/build", "600"], &[]);
    assert_eq!(code, 0);
    assert!(out.contains("Restart pending"), "{out}");
    assert!(
        out.contains("sha abc, release by 2026-10-03T00:00:00Z"),
        "{out}"
    );
    let args = std::fs::read_to_string(dir.path().join("pending.args")).unwrap();
    assert!(args.contains("RequestOperatorRestart"), "{args}");
    assert!(
        args.contains(r#"{"binaries_dir": "/build", "max_wait_secs": 600}"#),
        "{args}"
    );
    assert!(args.starts_with(&format!("--socket {socket} ")), "{args}");
    // Without a wait the params carry only the directory.
    let (code, _, _) = run(&[&rpc, &socket, "/build"], &[]);
    assert_eq!(code, 0);
    let args = std::fs::read_to_string(dir.path().join("pending.args")).unwrap();
    assert!(args.contains(r#"{"binaries_dir": "/build"}"#), "{args}");
    assert!(!args.contains("max_wait_secs"), "{args}");
}

#[test]
fn not_under_the_supervisor_exits_ten() {
    let dir = testutil::short_tempdir();
    let (socket, _listener) = live_socket(dir.path());
    let rpc = mock_rpc(
        dir.path(),
        "unsupervised",
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"deploy_needs_supervisor"}}"#,
    );
    let (code, _, err) = run(&[&rpc, &socket, "/build"], &[]);
    assert_eq!(code, 10);
    assert!(err.contains("restarting it directly"), "{err}");
}

#[test]
fn empty_malformed_and_non_rpc_replies_are_ambiguous() {
    let dir = testutil::short_tempdir();
    let (socket, _listener) = live_socket(dir.path());
    for (name, body) in [
        ("empty", ""),
        ("malformed", "not json at all"),
        ("notrpc", r#"{"hello":"world"}"#),
        ("array", "[1,2]"),
    ] {
        let rpc = mock_rpc(dir.path(), name, body);
        let (code, out, err) = run(&[&rpc, &socket, "/build"], &[]);
        assert_eq!(code, 1, "{name}");
        assert!(out.is_empty(), "{name}: {out}");
        assert!(err.contains("NOW=1"), "{name}: {err}");
    }
}

#[test]
fn missing_client_and_hung_client_are_ambiguous() {
    let dir = testutil::short_tempdir();
    let (socket, _listener) = live_socket(dir.path());
    let missing = dir
        .path()
        .join("missing-rpc")
        .to_string_lossy()
        .into_owned();
    let (code, _, err) = run(&[&missing, &socket, "/build"], &[]);
    assert_eq!(code, 1);
    assert!(err.contains("cannot run"), "{err}");
    assert!(err.contains("NOW=1"), "{err}");

    let hang = dir.path().join("hang");
    std::fs::write(&hang, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&hang, std::fs::Permissions::from_mode(0o755)).unwrap();
    let started = std::time::Instant::now();
    let (code, _, err) = run(
        &[&hang.to_string_lossy(), &socket, "/build"],
        &[("RSID_QUIET_RESTART_RPC_TIMEOUT_SECS", "1")],
    );
    assert_eq!(code, 1);
    assert!(err.contains("did not answer"), "{err}");
    assert!(err.contains("NOW=1"), "{err}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[test]
fn other_refusals_stop_and_name_now() {
    let dir = testutil::short_tempdir();
    let (socket, _listener) = live_socket(dir.path());
    let rpc = mock_rpc(
        dir.path(),
        "busy",
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"deploy_already_in_progress"}}"#,
    );
    let (code, _, err) = run(&[&rpc, &socket, "/build"], &[]);
    assert_eq!(code, 1);
    assert!(err.contains("deploy_already_in_progress"), "{err}");
    assert!(err.contains("NOW=1"), "{err}");
}

#[test]
fn absent_and_stale_sockets_are_down_evidence() {
    let dir = testutil::short_tempdir();
    let rpc = mock_rpc(
        dir.path(),
        "never",
        r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
    );
    let absent = dir
        .path()
        .join("absent.sock")
        .to_string_lossy()
        .into_owned();
    let (code, out, err) = run(&[&rpc, &absent, "/build"], &[]);
    assert_eq!((code, out.as_str(), err.as_str()), (11, "", ""));
    // A socket file nobody listens on refuses the connection.
    let stale = dir.path().join("stale.sock");
    drop(UnixListener::bind(&stale).unwrap());
    // A test thread that forks while the listener is open keeps a copy of its fd until
    // that child execs; wait until the connection is really refused.
    for _ in 0..400 {
        if std::os::unix::net::UnixStream::connect(&stale)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::ConnectionRefused)
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let (code, _, _) = run(&[&rpc, &stale.to_string_lossy(), "/build"], &[]);
    assert_eq!(code, 11);
    // The RPC is never attempted against a daemon that is down.
    assert!(!dir.path().join("never.args").exists());
}

#[test]
fn a_non_numeric_wait_is_refused() {
    let dir = testutil::short_tempdir();
    let (socket, _listener) = live_socket(dir.path());
    let rpc = mock_rpc(
        dir.path(),
        "never",
        r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
    );
    let (code, _, err) = run(&[&rpc, &socket, "/build", "soon"], &[]);
    assert_eq!(code, 1);
    assert!(err.contains("integer"), "{err}");
    assert!(!dir.path().join("never.args").exists());
}

#[test]
fn usage_error() {
    let (code, _, err) = run(&["only-rpc"], &[]);
    assert_eq!(code, 64);
    assert!(err.contains("usage:"), "{err}");
}
