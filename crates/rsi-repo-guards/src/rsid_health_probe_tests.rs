//! Case map for `rsid-health-probe` (#465). It replaces the inline interpreter heredoc of
//! `scripts/rsid-health.sh rsid_rpc_ready`; the shell cases that exercise it
//! end to end through a real fake daemon are in
//! `scripts/tests/test_install_release_start.sh` (identity, foreign daemon,
//! launcher exit, "answered exited right after replying" = exit 3).
//!
//! * `reply_predicate_*`: the JSON-RPC readiness predicate (good reply; wrong
//!   `jsonrpc`; wrong id; an `error`; a missing or non-integer field).
//! * `launcher_*`: the socket probe against a real listener whose sender is this
//!   process: identified reply (Ready); reported PID that is not the sender or
//!   not in the launcher tree (NotReady, negative); startup time without a timezone
//!   (NotReady, negative); legacy reply with no identity (Ready via the kernel
//!   identity); a reply cut off before its newline (NotReady); a launcher that
//!   exited or is PID 1 (ProcessGone).
//! * `rpc_*`: the no-launcher path through a scripted `rsi-rpc`: complete reply
//!   (Ready), incomplete reply, failing exit and a hung client (NotReady within the
//!   budget).
//! * `main_*`: argument handling and exit codes.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::time::{Duration, Instant};

use super::{Outcome, main, probe, reply_ready};
use crate::Ctx;
use crate::testutil;

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap()
}

#[test]
fn reply_predicate_accepts_a_complete_reply() {
    assert!(reply_ready(&json(
        r#"{"jsonrpc":"2.0","id":1,"result":{"persistence_queue_capacity":1,"project_cache_size":0}}"#
    )));
}

#[test]
fn reply_predicate_refuses_incomplete_replies() {
    for bad in [
        r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
        r#"{"jsonrpc":"1.0","id":1,"result":{"persistence_queue_capacity":1,"project_cache_size":0}}"#,
        r#"{"jsonrpc":"2.0","id":2,"result":{"persistence_queue_capacity":1,"project_cache_size":0}}"#,
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":1},"result":{"persistence_queue_capacity":1,"project_cache_size":0}}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{"persistence_queue_capacity":1.5,"project_cache_size":0}}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{"persistence_queue_capacity":"1","project_cache_size":0}}"#,
        r"[]",
        r#""x""#,
    ] {
        assert!(!reply_ready(&json(bad)), "{bad}");
    }
}

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

/// Serve one connection: read the request line, write `reply` and close.
fn serve(dir: &Path, reply: String) -> (String, std::thread::JoinHandle<String>) {
    let socket = dir.join("s.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let handle = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while conn.read(&mut byte).unwrap_or(0) == 1 {
            request.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        conn.write_all(reply.as_bytes()).unwrap();
        // Stay connected like a daemon: the client closes first (LOCAL_PEERPID needs a live peer).
        while conn.read(&mut byte).unwrap_or(0) > 0 {}
        String::from_utf8(request).unwrap()
    });
    (socket.to_string_lossy().into_owned(), handle)
}

fn health(pid: i64, started: &str) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"persistence_queue_capacity\":1,\"project_cache_size\":0,\"daemon_pid\":{pid},\"daemon_started_at\":\"{started}\"}}}}\n"
    )
}

const STARTED: &str = "2026-10-22T10:00:00.123456789Z";

fn me() -> i64 {
    i64::from(std::process::id())
}

#[test]
fn launcher_identified_reply_from_the_launcher_is_ready() {
    let dir = testutil::short_tempdir();
    let (socket, server) = serve(dir.path(), health(me(), STARTED));
    assert_eq!(probe("unused", &socket, Some(me()), soon()), Outcome::Ready);
    assert_eq!(
        server.join().unwrap(),
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"GetHealthStatus\",\"params\":null}\n"
    );
}

#[test]
fn launcher_foreign_reported_pid_is_not_ready() {
    // The reported PID (our parent) is not the sender here, and is not in the launcher
    // tree of this process either: a foreign daemon never reads as healthy.
    let dir = testutil::short_tempdir();
    let parent = i64::from(std::os::unix::process::parent_id());
    let (socket, server) = serve(dir.path(), health(parent, STARTED));
    assert_eq!(
        probe("unused", &socket, Some(me()), soon()),
        Outcome::NotReady
    );
    let _ = server.join();
}

#[test]
fn launcher_other_tree_is_not_ready() {
    // The sender is this process, the launcher is its parent's parent chain's init
    // sibling: use PID 2 style "elsewhere" by naming our own parent as launcher while
    // the reply claims us: we are a child of the launcher, so this is Ready; naming a
    // live unrelated process (a sleeping child) as launcher must not be.
    let dir = testutil::short_tempdir();
    let mut other = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let (socket, server) = serve(dir.path(), health(me(), STARTED));
    let outcome = probe("unused", &socket, Some(i64::from(other.id())), soon());
    let _ = other.kill();
    let _ = other.wait();
    let _ = server.join();
    assert_eq!(outcome, Outcome::NotReady);
}

#[test]
fn launcher_startup_time_without_timezone_is_not_ready() {
    let dir = testutil::short_tempdir();
    let (socket, server) = serve(dir.path(), health(me(), "2026-10-22T10:00:00"));
    assert_eq!(
        probe("unused", &socket, Some(me()), soon()),
        Outcome::NotReady
    );
    let _ = server.join();
}

#[test]
fn launcher_invalid_identity_is_not_ready() {
    for (pid, started) in [
        ("1", "\"2026-10-22T10:00:00Z\""),
        ("\"7\"", "\"2026-10-22T10:00:00Z\""),
        ("7", "null"),
    ] {
        let dir = testutil::short_tempdir();
        let reply = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"persistence_queue_capacity\":1,\"project_cache_size\":0,\"daemon_pid\":{pid},\"daemon_started_at\":{started}}}}}\n"
        );
        let (socket, server) = serve(dir.path(), reply);
        assert_eq!(
            probe("unused", &socket, Some(me()), soon()),
            Outcome::NotReady,
            "{pid} {started}"
        );
        let _ = server.join();
    }
}

#[test]
fn launcher_legacy_reply_without_identity_uses_the_kernel_identity() {
    let dir = testutil::short_tempdir();
    let reply = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"persistence_queue_capacity\":1,\"project_cache_size\":0}}\n";
    let (socket, server) = serve(dir.path(), reply.to_string());
    assert_eq!(probe("unused", &socket, Some(me()), soon()), Outcome::Ready);
    let _ = server.join();
}

#[test]
fn launcher_reply_cut_off_before_its_newline_is_not_ready() {
    let dir = testutil::short_tempdir();
    let (socket, server) = serve(dir.path(), "{\"jsonrpc\":\"2.0\",\"id\":1".to_string());
    assert_eq!(
        probe(
            "unused",
            &socket,
            Some(me()),
            Instant::now() + Duration::from_millis(500)
        ),
        Outcome::NotReady
    );
    let _ = server.join();
}

#[test]
fn launcher_that_exited_is_process_gone() {
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    let pid = i64::from(gone.id());
    gone.wait().unwrap();
    let dir = testutil::short_tempdir();
    let socket = dir
        .path()
        .join("nobody.sock")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        probe("unused", &socket, Some(pid), soon()),
        Outcome::ProcessGone
    );
    assert_eq!(
        probe("unused", &socket, Some(1), soon()),
        Outcome::ProcessGone
    );
    assert_eq!(
        probe("unused", &socket, Some(0), soon()),
        Outcome::ProcessGone
    );
}

#[test]
fn launcher_missing_socket_is_not_ready() {
    let dir = testutil::short_tempdir();
    let socket = dir
        .path()
        .join("nobody.sock")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        probe("unused", &socket, Some(me()), soon()),
        Outcome::NotReady
    );
}

fn script(dir: &Path, body: &str) -> String {
    let path = dir.join("rpc");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn rpc_complete_reply_is_ready_and_gets_the_documented_arguments() {
    let dir = testutil::short_tempdir();
    let log = dir.path().join("args");
    let rpc = script(
        dir.path(),
        &format!(
            "echo \"$@\" > '{}'\necho '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"persistence_queue_capacity\":1,\"project_cache_size\":0}}}}'",
            log.display()
        ),
    );
    assert_eq!(probe(&rpc, "/tmp/some.sock", None, soon()), Outcome::Ready);
    assert_eq!(
        std::fs::read_to_string(log).unwrap(),
        "--socket /tmp/some.sock GetHealthStatus\n"
    );
}

#[test]
fn rpc_incomplete_failing_and_hung_clients_are_not_ready() {
    let dir = testutil::short_tempdir();
    let incomplete = script(
        dir.path(),
        "echo '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}'",
    );
    assert_eq!(probe(&incomplete, "s", None, soon()), Outcome::NotReady);
    let failing = script(
        dir.path(),
        "echo '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"persistence_queue_capacity\":1,\"project_cache_size\":0}}'\nexit 3",
    );
    assert_eq!(probe(&failing, "s", None, soon()), Outcome::NotReady);
    let hung = script(dir.path(), "sleep 20");
    let started = Instant::now();
    let deadline = started + Duration::from_millis(300);
    assert_eq!(probe(&hung, "s", None, deadline), Outcome::NotReady);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        probe("/nonexistent/rsi-rpc", "s", None, soon()),
        Outcome::NotReady
    );
}

fn run_main(args: &[&str]) -> (i32, String) {
    let args: Vec<String> = args.iter().map(ToString::to_string).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = main(&args, &Ctx::from_process(), &mut out, &mut err);
    assert!(out.is_empty());
    (code, String::from_utf8(err).unwrap())
}

#[test]
fn main_usage_and_launcher_argument() {
    let (code, err) = run_main(&["only-rpc"]);
    assert_eq!(code, 64);
    assert!(err.contains("usage:"), "{err}");
    // A launcher argument that is not a number is not ready (never a pass).
    assert_eq!(run_main(&["rpc", "sock", "abc"]).0, 1);
    // An empty launcher means the no-launcher path; the rpc is missing, so not ready.
    assert_eq!(run_main(&["/nonexistent/rsi-rpc", "sock", ""]).0, 1);
    assert_eq!(run_main(&["/nonexistent/rsi-rpc", "sock", "0"]).0, 2);
}
