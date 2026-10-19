use super::*;
use tokio::io::AsyncWriteExt;

/// #2285: wait for the detached turn's identity wait to park, then expire its
/// real 10 s deadline explicitly instead of sleeping through it. A turn whose
/// identity check fails at once (a malformed record) never parks: pass
/// `parks = false` and nothing is awaited.
fn expire_identity_wait_once_parked(
    process: &ClaudeProcess,
    parks: bool,
) -> impl std::future::Future<Output = ()> + use<> {
    let turn = process.detached.clone().expect("detached test process");
    async move {
        if parks {
            turn.identity_wait_parked_for_test().await;
            turn.expire_identity_deadline_for_test();
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn unpublished_shim_interrupt_refuses_and_force_kill_reaps_owned_child() {
    for malformed in [false, true] {
        let dir = tempfile::tempdir().unwrap(); // tmpfs-fixture-ok: spool identity only
        let lock = std::fs::File::create(dir.path().join("alive.lock")).unwrap();
        lock.lock().unwrap();
        if malformed {
            std::fs::write(dir.path().join("shim.json"), b"malformed").unwrap();
        }
        let child = Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap();
        let turn = DetachedTurn::new(Uuid::new_v4(), dir.path().to_path_buf());
        let mut process = ClaudeProcess::detached_for_test(child, turn);
        let interrupt = process.interrupt();
        // Exercise both custody parse failure and its bounded identity timeout.
        // The malformed record fails at once; without a record the identity
        // wait parks and the test expires its deadline explicitly.
        let expire = expire_identity_wait_once_parked(&process, !malformed);
        let killed = tokio::time::timeout(std::time::Duration::from_secs(12), async {
            let (killed, ()) = tokio::join!(process.kill(), expire);
            killed
        })
        .await;
        if !matches!(killed, Ok(Ok(()))) {
            let _ = crate::process_scope::kill_worker_child(
                process.child.as_mut().unwrap(),
                &process.stop,
            )
            .await;
        }
        assert!(
            interrupt
                .unwrap_err()
                .to_string()
                .contains("turn_shim_identity_pending")
        );
        killed.unwrap().unwrap();
        assert!(process.try_wait().unwrap().unwrap().code() != Some(0));
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn handoff_during_identity_wait_preserves_the_owned_child() {
    let dir = tempfile::tempdir().unwrap(); // tmpfs-fixture-ok: spool identity only
    let turn = DetachedTurn::new(Uuid::new_v4(), dir.path().to_path_buf());
    let child = Command::new("sleep")
        .arg("60")
        .process_group(0)
        .spawn()
        .unwrap();
    let mut process = ClaudeProcess::detached_for_test(child, turn.clone());
    let handoff = async {
        // Hand off while the identity wait is parked, then let its deadline
        // expire: the stop must leave the handed-off child running.
        turn.identity_wait_parked_for_test().await;
        turn.leave_running();
        turn.expire_identity_deadline_for_test();
    };
    let (stopped, ()) = tokio::join!(process.kill(), handoff);
    let alive = process.try_wait().unwrap().is_none();
    crate::process_scope::kill_worker_child(process.child.as_mut().unwrap(), &process.stop)
        .await
        .unwrap();
    stopped.unwrap();
    assert!(turn.is_handed_off());
    assert!(alive, "the handed-off child keeps running");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn complete_file_frames_match_pipe_events_and_offsets() {
    let input = b"{\"type\":\"system\",\"session_id\":\"cli-id\"}\r\n{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n{\"type\":\"result\",\"subtype\":\"success\"}\n";
    let mut lines = BoundedLines::new(input.as_slice(), PROVIDER_MAX_LINE_BYTES);
    let mut expected = Vec::new();
    while let Some(line) = lines.next_line().await.unwrap() {
        expected.push(serde_json::from_str::<StreamEvent>(&line).unwrap());
    }
    let mut tail = LineTail::default();
    let actual = input
        .chunks(7)
        .flat_map(|bytes| tail.push(bytes))
        .collect::<Vec<_>>();
    assert_eq!(actual.len(), expected.len());
    for (frame, event) in actual.iter().zip(expected) {
        assert_eq!(frame.event.event_type, event.event_type);
        assert_eq!(frame.event.data, event.data);
        assert_eq!(input[frame.end as usize - 1], b'\n');
    }
    assert_eq!(actual[0].start, 0);
    assert_eq!(actual.last().unwrap().end, input.len() as u64);
    assert!(!tail.has_partial());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[test]
fn partial_blank_malformed_and_oversized_lines_keep_newline_boundaries() {
    let mut tail = LineTail::default();
    assert!(tail.push(b"{\"type\":\"assistant\"").is_empty());
    assert!(tail.has_partial());
    let frames = tail.push(b",\"text\":\"hello\"}\n \nmalformed\n");
    assert_eq!(frames[0].event.event_type, "assistant");
    assert_eq!(frames[1].event.event_type, "turn_spool_checkpoint");
    assert_eq!(frames[2].event.event_type, "parse_error");
    assert_eq!(frames[2].event.data["raw_line"], "malformed");
    assert!(
        tail.push(&vec![b'x'; PROVIDER_MAX_LINE_BYTES + 10])
            .is_empty()
    );
    assert!(tail.buffer.len() <= PROVIDER_MAX_LINE_BYTES);
    let frames = tail.push(b"\n{\"type\":\"result\"}\n");
    assert_eq!(frames[0].event.event_type, "process_error");
    assert_eq!(frames[0].event.data["terminal"], false);
    assert_eq!(frames[1].event.event_type, "result");
    assert_eq!(frames[0].end, frames[1].start);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn live_tail_waits_for_newline_and_drains_before_completion() {
    let dir = tempfile::TempDir::new().unwrap();
    let lease = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.path().join("alive.lock"))
        .unwrap();
    lease.try_lock().unwrap();
    std::fs::write(dir.path().join("shim.json"), "{\"pid\":1234}\n").unwrap();
    std::fs::write(dir.path().join("stdout"), b"").unwrap();
    std::fs::write(dir.path().join("stderr"), b"a diagnostic\n").unwrap();
    std::fs::write(dir.path().join("stdin"), b"private prompt").unwrap();
    let turn = DetachedTurn::new(Uuid::new_v4(), dir.path().to_path_buf());
    let boot = Uuid::new_v4();
    let (tx, mut rx) = mpsc::channel(100);
    turn.spawn_reader(tx);
    turn.start(Ok(boot));
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(dir.path().join("stdout"))
        .await
        .unwrap();
    file.write_all(b"{\"type\":\"assistant\",\"text\":\"hel")
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(80), rx.recv())
            .await
            .is_err()
    );
    assert_eq!(turn.receipts.lock().unwrap().len(), 0);
    assert!(dir.path().join("stdin").exists(), "live prompt is retained");
    file.write_all(b"lo\"}\n{\"type\":\"result\"}\n")
        .await
        .unwrap();
    file.flush().await.unwrap();
    std::fs::write(
        dir.path().join("exit.json"),
        "{\"exit_code\":0,\"signal\":null,\"error\":null}\n",
    )
    .unwrap();
    drop(lease);
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.data["text"], "hello");
    let first = turn.next_receipt().unwrap();
    assert_eq!(first.expected_offset, 0);
    assert_eq!(first.boot_id, boot);
    assert_eq!(rx.recv().await.unwrap().event_type, "result");
    let last = turn.next_receipt().unwrap();
    assert_eq!(first.next_offset, last.expected_offset);
    assert_eq!(last.next_offset, file.metadata().await.unwrap().len());
    assert_eq!(rx.recv().await.unwrap().data["source"], "stderr");
    assert!(turn.next_receipt().is_none());
    assert!(rx.recv().await.is_none());
    assert!(turn.complete.load(std::sync::atomic::Ordering::Acquire));
    assert!(
        !dir.path().join("stdin").exists(),
        "turn end removes the prompt"
    );
    assert!(dir.path().exists(), "spool remains until Finished custody");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn final_partial_line_is_an_error_and_never_a_cursor_receipt() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("stdout"), "{\"type\":\"result\"}").unwrap();
    std::fs::write(
        dir.path().join("exit.json"),
        "{\"exit_code\":0,\"error\":null}",
    )
    .unwrap();
    let turn = DetachedTurn::new(Uuid::new_v4(), dir.path().to_path_buf());
    let (tx, mut rx) = mpsc::channel(100);
    turn.spawn_reader(tx);
    turn.start(Ok(Uuid::new_v4()));
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.event_type, "process_error");
    assert_eq!(event.data["terminal"], true);
    assert!(turn.next_receipt().is_none());
    assert!(!turn.complete.load(std::sync::atomic::Ordering::Acquire));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[test]
fn shim_wrapper_preserves_literal_arguments_cwd_and_environment() {
    let mut direct = Command::new("claude");
    direct
        .arg("--")
        .arg("a prompt with $HOME and `literal`")
        .current_dir("/workspace");
    direct.env("TEST_KEEP", "value").env_remove("TEST_REMOVE");
    let wrapped = wrap_command(
        &direct,
        Path::new("/installed/rsi-turn-shim"),
        Path::new("/state/turns/id"),
    );
    assert_eq!(wrapped.as_std().get_program(), "/installed/rsi-turn-shim");
    assert_eq!(
        wrapped.as_std().get_args().collect::<Vec<_>>(),
        vec![
            "--spool-dir",
            "/state/turns/id",
            "--",
            "claude",
            "--",
            "a prompt with $HOME and `literal`"
        ]
    );
    assert_eq!(
        wrapped.as_std().get_current_dir(),
        direct.as_std().get_current_dir()
    );
    assert_eq!(
        wrapped.as_std().get_envs().collect::<Vec<_>>(),
        direct.as_std().get_envs().collect::<Vec<_>>()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn durable_shim_setup_failure_is_a_completed_turn_with_a_terminal_diagnostic() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("stdout"), b"").unwrap();
    std::fs::write(dir.path().join("stderr"), b"").unwrap();
    std::fs::write(
        dir.path().join("exit.json"),
        "{\"exit_code\":null,\"signal\":null,\"error\":\"provider spawn failed\"}",
    )
    .unwrap();
    let turn = DetachedTurn::new(Uuid::new_v4(), dir.path().to_path_buf());
    let (tx, mut rx) = mpsc::channel(100);
    turn.spawn_reader(tx);
    turn.start(Ok(Uuid::new_v4()));
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.data["error"], "provider spawn failed");
    assert_eq!(event.data["terminal"], true);
    assert!(turn.next_receipt().is_none());
    assert!(rx.recv().await.is_none());
    assert!(turn.complete.load(std::sync::atomic::Ordering::Acquire));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn deploy_handoff_quiesces_reader_without_a_terminal_event() {
    let dir = tempfile::tempdir().unwrap();
    // A missing spool would be a terminal read error during ordinary operation.
    let turn = DetachedTurn::new(Uuid::new_v4(), dir.path().join("missing"));
    let (tx, mut rx) = mpsc::channel(10);
    turn.start(Ok(Uuid::new_v4()));
    turn.leave_running();
    turn.spawn_reader(tx);
    tokio::time::timeout(std::time::Duration::from_secs(1), turn.wait_for_handoff())
        .await
        .unwrap();
    assert!(turn.is_handed_off());
    turn.signal(nix::sys::signal::Signal::SIGQUIT).unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .is_none()
    );
    assert!(!turn.complete.load(std::sync::atomic::Ordering::Acquire));
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[test]
fn malformed_exit_record_settles_an_unlocked_adopted_handle_as_failed() {
    let dir = tempfile::tempdir().unwrap(); // tmpfs-fixture-ok: exit records only
    for record in [
        "malformed",
        "{\"exit_code\":999999999999}",
        "{\"signal\":0}",
    ] {
        std::fs::write(dir.path().join("exit.json"), record).unwrap();
        let mut process =
            ClaudeProcess::adopt(DetachedTurn::new(Uuid::new_v4(), dir.path().to_path_buf()));
        assert_eq!(process.try_wait().unwrap().unwrap().code(), Some(1));
    }
}

#[cfg(unix)]
async fn live_codex_spool() -> (
    tempfile::TempDir,
    tokio::process::Child,
    DetachedTurn,
    mpsc::Receiver<StreamEvent>,
) {
    let dir = tempfile::tempdir().unwrap(); // tmpfs-fixture-ok: spool only
    let script = r#"
import os, sys, fcntl, json, time
p = sys.argv[1]
f = open(p + '/alive.lock', 'w')
fcntl.flock(f, fcntl.LOCK_EX)
open(p + '/stdout', 'w').close()
open(p + '/stderr', 'w').close()
with open(p + '/shim.json', 'w') as out:
    json.dump({'pid': os.getpid()}, out)
while True: time.sleep(1)
"#;
    let child = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(dir.path())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut turn = DetachedTurn::new(Uuid::new_v4(), dir.path().to_path_buf());
    turn.output = TurnOutput::Codex {
        boundary: crate::codex::CodexTranscriptBoundary::ResumeUnavailable,
    };
    let boot = Uuid::new_v4();
    turn.custody(Uuid::new_v4(), boot).await.unwrap();
    let (tx, rx) = mpsc::channel(100);
    turn.spawn_reader(tx);
    turn.start(Ok(boot));
    (dir, child, turn, rx)
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn detached_codex_storage_full_is_detected_before_exit_and_only_once() {
    let (dir, mut child, turn, mut rx) = live_codex_spool().await;
    let mut stderr = tokio::fs::OpenOptions::new()
        .append(true)
        .open(dir.path().join("stderr"))
        .await
        .unwrap();
    let fatal = b"2026-08-19T21:48:22.123456Z ERROR codex_rollout::recorder: failed to persist rollout: No space left on device (os error 28); error_kind=StorageFull; raw_os_error=Some(28)";
    stderr.write_all(fatal).await.unwrap();
    stderr.flush().await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(80), rx.recv())
            .await
            .is_err()
    );
    stderr.write_all(b"\n").await.unwrap();
    stderr.flush().await.unwrap();
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.data["source"], "stderr");
    assert_eq!(event.data["terminal"], true);
    assert_eq!(
        event.data["error_class"],
        crate::codex::CODEX_STORAGE_FULL_ERROR_CLASS
    );
    assert_eq!(
        event.data["provider_event_type"],
        crate::codex::CODEX_STORAGE_FULL_PROVIDER_EVENT_TYPE
    );
    assert!(child.try_wait().unwrap().is_none());
    assert!(!dir.path().join("exit.json").exists());
    assert!(turn.next_receipt().is_none());
    stderr.write_all(fatal).await.unwrap();
    stderr.write_all(b"\n").await.unwrap();
    stderr.flush().await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(80), rx.recv())
            .await
            .is_err()
    );
    turn.leave_running();
    child.kill().await.unwrap();
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn detached_codex_stderr_bounds_signal_live_shim_before_exit() {
    for unterminated in [false, true] {
        let (dir, mut child, turn, mut rx) = live_codex_spool().await;
        let bytes = if unterminated {
            vec![b'x'; PROVIDER_MAX_LINE_BYTES + 1]
        } else {
            let mut bytes = Vec::new();
            while bytes.len() <= PROVIDER_MAX_STDERR_BYTES {
                bytes.extend_from_slice(&vec![b'x'; 1024]);
                bytes.push(b'\n');
            }
            bytes
        };
        tokio::fs::write(dir.path().join("stderr"), bytes)
            .await
            .unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.data["error_class"], "provider_output_overflow");
        assert_eq!(event.data["source"], "stderr");
        assert_eq!(event.data["terminal"], true);
        assert!(turn.next_receipt().is_none());
        assert!(!dir.path().join("exit.json").exists());
        let status = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
            .await
            .unwrap()
            .unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(15), "overflow signals the owned shim");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn detached_agy_nonzero_exit_matches_direct_result_and_removes_prompt() {
    let dir = tempfile::tempdir().unwrap(); // tmpfs-fixture-ok: spool only
    std::fs::write(dir.path().join("stdout"), b"answer\n").unwrap();
    std::fs::write(dir.path().join("stdin"), b"private prompt").unwrap();
    std::fs::write(
        dir.path().join("exit.json"),
        br#"{"exit_code":7,"signal":null,"error":null}"#,
    )
    .unwrap();
    let mut turn = DetachedTurn::new(Uuid::new_v4(), dir.path().to_path_buf());
    turn.output = TurnOutput::Antigravity {
        conversation_cache_path: dir.path().join("unused"),
        working_dir: None,
        pre_snapshot: None,
        capture: false,
    };
    let (tx, mut rx) = mpsc::channel(100);
    turn.spawn_reader(tx);
    turn.start(Ok(Uuid::new_v4()));
    assert_eq!(rx.recv().await.unwrap().event_type, "assistant");
    assert_eq!(rx.recv().await.unwrap().event_type, "result");
    assert!(rx.recv().await.is_none());
    assert_eq!(turn.exit_status().unwrap().unwrap().code(), Some(7));
    assert!(!dir.path().join("stdin").exists());
    assert!(turn.complete.load(std::sync::atomic::Ordering::Acquire));
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[test]
fn detached_handle_retains_exit_evidence_after_finished_spool_cleanup() {
    for code in [0, 7] {
        let dir = tempfile::tempdir().unwrap(); // tmpfs-fixture-ok: exit records only
        let spool = dir.path().join("turn");
        std::fs::create_dir(&spool).unwrap();
        std::fs::write(
            spool.join("exit.json"),
            format!(r#"{{"exit_code":{code},"signal":null,"error":null}}"#),
        )
        .unwrap();
        let turn = DetachedTurn::new(Uuid::new_v4(), spool.clone());
        let mut process = ClaudeProcess::adopt(turn.clone());
        assert_eq!(process.try_wait().unwrap().unwrap().code(), Some(code));
        std::fs::remove_dir_all(spool).unwrap();
        assert_eq!(turn.exit_status().unwrap().unwrap().code(), Some(code));
        assert_eq!(process.try_wait().unwrap().unwrap().code(), Some(code));
    }
}

/// A live shim holds the spool lock but has not published `shim.json` yet.
#[cfg(unix)]
async fn unpublished_codex_spool() -> (
    tempfile::TempDir,
    tokio::process::Child,
    DetachedTurn,
    mpsc::Receiver<StreamEvent>,
) {
    let dir = tempfile::tempdir().unwrap(); // tmpfs-fixture-ok: spool only
    let script = r#"
import sys, fcntl, time
p = sys.argv[1]
f = open(p + '/alive.lock', 'w')
fcntl.flock(f, fcntl.LOCK_EX)
open(p + '/stdout', 'w').close()
open(p + '/stderr', 'w').close()
open(p + '/ready', 'w').close()
while True: time.sleep(1)
"#;
    let child = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(dir.path())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    while !dir.path().join("ready").exists() {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let mut turn = DetachedTurn::new(Uuid::new_v4(), dir.path().to_path_buf());
    turn.output = TurnOutput::Codex {
        boundary: crate::codex::CodexTranscriptBoundary::ResumeUnavailable,
    };
    let boot = Uuid::new_v4();
    let (tx, rx) = mpsc::channel(100);
    turn.spawn_reader(tx);
    turn.start(Ok(boot));
    (dir, child, turn, rx)
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn adopted_codex_turn_resumes_stderr_after_the_delivered_offset() {
    let (dir, mut child, turn, mut rx) = live_codex_spool().await;
    let fatal = b"2026-08-19T21:48:22.123456Z ERROR codex_rollout::recorder: failed to persist rollout: No space left on device (os error 28); error_kind=StorageFull; raw_os_error=Some(28)\n";
    tokio::fs::write(dir.path().join("stderr"), fatal)
        .await
        .unwrap();
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.data["terminal"], true);
    let cursor = dir.path().join("stderr.cursor");
    for _ in 0..100 {
        if cursor.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap(),
        fatal.len().to_string()
    );
    // Daemon restart: a new handle adopts the same spool with a fresh decoder.
    turn.leave_running();
    let mut adopted = DetachedTurn::new(turn.invocation_id, dir.path().to_path_buf());
    adopted.output = turn.output.clone();
    adopted.adopted = Some(crate::store::provider_turn_custody::ProviderTurnCustody {
        worker_scope: None,
        invocation_id: turn.invocation_id,
        session_id: Uuid::new_v4(),
        spool_dir: dir.path().to_path_buf(),
        pid: child.id().unwrap(),
        start_time: None,
        boot_id: Uuid::new_v4(),
        stdout_offset: 0,
        state: crate::store::provider_turn_custody::ProviderTurnCustodyState::Adopted,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    });
    let (tx, mut adopted_rx) = mpsc::channel(100);
    adopted.spawn_reader(tx);
    adopted.start(Ok(Uuid::new_v4()));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), adopted_rx.recv())
            .await
            .is_err(),
        "an adopted turn must not replay delivered stderr"
    );
    adopted.leave_running();
    child.kill().await.unwrap();
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn stderr_overflow_signal_retries_until_the_shim_identity_is_proven() {
    let (dir, mut child, turn, mut rx) = unpublished_codex_spool().await;
    tokio::fs::write(
        dir.path().join("stderr"),
        vec![b'x'; PROVIDER_MAX_LINE_BYTES + 1],
    )
    .await
    .unwrap();
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.data["error_class"], "provider_output_overflow");
    // The identity is published only after the overflow was reported.
    let pid = child.id().unwrap();
    tokio::fs::write(dir.path().join("shim.json"), format!("{{\"pid\":{pid}}}"))
        .await
        .unwrap();
    *turn.identity.lock().unwrap() = Some((pid, pid_start_time(pid)));
    let status = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
        .await
        .unwrap()
        .unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(15),
        "the retried SIGTERM reaches the shim"
    );
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn stderr_overflow_with_unprovable_shim_fails_the_turn() {
    let (dir, mut child, _turn, mut rx) = unpublished_codex_spool().await;
    tokio::fs::write(
        dir.path().join("stderr"),
        vec![b'x'; PROVIDER_MAX_LINE_BYTES + 1],
    )
    .await
    .unwrap();
    let overflow = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(overflow.data["error_class"], "provider_output_overflow");
    let failure = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failure.data["terminal"], true);
    assert!(
        failure.data["error"]
            .as_str()
            .unwrap()
            .contains("could not be terminated"),
        "{failure:?}"
    );
    child.kill().await.unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn detached_force_stop_kills_only_the_recorded_scope() {
    // #2007: a stop that never reaches the shim (so the owned child group is
    // killed) must still clean up the scope the daemon recorded at launch.
    let dir = tempfile::tempdir().unwrap(); // tmpfs-fixture-ok: spool identity only
    let unit = format!("rsi-worker-2007-{}.scope", Uuid::new_v4());
    let other = format!("rsi-worker-2007-other-{}.scope", Uuid::new_v4());
    std::fs::write(dir.path().join("scope"), &other).unwrap();
    let lock = std::fs::File::create(dir.path().join("alive.lock")).unwrap();
    lock.lock().unwrap();
    let child = Command::new("sleep")
        .arg("60")
        .process_group(0)
        .spawn()
        .unwrap();
    let mut turn = DetachedTurn::new(Uuid::new_v4(), dir.path().to_path_buf());
    turn.scope = Some(unit.clone());
    let mut process = ClaudeProcess::detached_for_test(child, turn);
    let expire = expire_identity_wait_once_parked(&process, true);
    let killed = tokio::time::timeout(std::time::Duration::from_secs(12), async {
        let (killed, ()) = tokio::join!(process.kill(), expire);
        killed
    })
    .await;
    assert!(matches!(killed, Ok(Ok(()))));
    assert_eq!(
        rsid_core::process_control::recorded_scope_kills::killed(&unit),
        1
    );
    assert_eq!(
        rsid_core::process_control::recorded_scope_kills::killed(&other),
        0
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[test]
fn detached_turn_without_a_valid_scope_record_kills_no_scope() {
    let dir = tempfile::tempdir().unwrap(); // tmpfs-fixture-ok: spool identity only
    let turn = DetachedTurn::new(Uuid::new_v4(), dir.path().to_path_buf());
    assert_eq!(turn.scope, None);
    std::fs::write(dir.path().join("scope"), "rsi-worker-forged-valid.scope").unwrap();
    assert_eq!(turn.scope, None);
    turn.kill_recorded_scope();
    assert_eq!(
        rsid_core::process_control::recorded_scope_kills::killed("rsi-worker-forged-valid.scope"),
        0
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[test]
fn restart_restores_scope_from_store_and_ignores_forged_spool() {
    use crate::store::Store;
    let dir = tempfile::tempdir().unwrap(); // tmpfs-fixture-ok: database and spool only
    let db = dir.path().join("custody.db");
    let store = Store::open_test_fixture(&db).unwrap();
    let mut session = rsid_store::test_support::make_test_session();
    session.project_id = None;
    store.insert_session(&session).unwrap();
    let invocation = Uuid::new_v4();
    let unit = format!("rsi-worker-{invocation}-owned.scope");
    let forged = format!("rsi-worker-{invocation}-forged.scope");
    let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    store.conn.execute(
        "INSERT INTO model_invocations
         (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,session_id,created_at)
         VALUES(?1,'session.launch','session','foreground','paid','admitted','running','launch_session',?2,?3)",
        rusqlite::params![invocation.to_string(), session.id.to_string(), at],
    ).unwrap();
    store
        .insert_provider_turn_custody(&NewProviderTurnCustody {
            invocation_id: invocation,
            session_id: session.id,
            spool_dir: dir.path().to_path_buf(),
            worker_scope: Some(unit.clone()),
            pid: 12345,
            start_time: Some(1),
            boot_id: Uuid::new_v4(),
        })
        .unwrap();
    drop(store);
    std::fs::write(dir.path().join("scope"), &forged).unwrap();
    let restarted = Store::open(&db).unwrap();
    let row = restarted
        .get_provider_turn_custody(invocation)
        .unwrap()
        .unwrap();
    assert_eq!(row.session_id, session.id);
    let adopted = DetachedTurn::adopt(row);
    adopted.kill_recorded_scope();
    assert_eq!(
        rsid_core::process_control::recorded_scope_kills::killed(&unit),
        1
    );
    assert_eq!(
        rsid_core::process_control::recorded_scope_kills::killed(&forged),
        0
    );
}
