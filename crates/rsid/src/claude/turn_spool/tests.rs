use super::*;
use tokio::io::AsyncWriteExt;

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
        let killed = tokio::time::timeout(std::time::Duration::from_secs(12), process.kill()).await;
        if !matches!(killed, Ok(Ok(()))) {
            let _ = crate::process_scope::kill_worker_child(process.child.as_mut().unwrap()).await;
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
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        turn.leave_running();
    };
    let (stopped, ()) = tokio::join!(process.kill(), handoff);
    let alive = process.try_wait().unwrap().is_none();
    crate::process_scope::kill_worker_child(process.child.as_mut().unwrap())
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
