//! Unit tests for session module.

mod harness_manager;
mod manager_actions;
mod terminal_watch_owner;

use super::types::{PIPELINE_PATH_RE, TerminalFinalizeDecision};
use super::*;
use crate::bus::{DaemonEvent, EventBus};
use crate::claude::StreamEvent;
use crate::config::{Config, RuntimeConfig};
use crate::memory::worker::{MemoryCommand, MemoryHandle};
use crate::model_control::call_control::{
    ModelCallControl, ModelCallKind, ModelCallSettlement, StoreBackedModelCallControl,
};
use crate::model_control::registry::RuntimeExecutionRoute;
use crate::model_control::{
    AdmissionDecision, ExpectedUsage, ModelAdmissionRequest, admit_invocation,
};
use crate::store::Store;
use rsi_common::model_control::{InvocationOwner, ModelInvocationPurpose, ModelInvocationStatus};
use rsi_common::types::{
    ContextUsageConfidence, ConversationEvent, Session, SessionKind, SessionProvider, SessionStatus,
};
use rsi_common::types::{EventType, Role};
use tempfile::TempDir;
use tokio::sync::mpsc;
use uuid::Uuid;

fn manager() -> (SessionManager, TempDir) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("rsi.db");
    let store = Store::open(&db_path).expect("open store");
    let config = Config::from_env();
    let runtime_config = RuntimeConfig::from_config(&config);
    let manager = SessionManager::new(
        std::sync::Arc::new(EventBus::new(16)),
        store,
        false,
        dir.path().join("daemon.sock"),
        None,
        Vec::new(),
        runtime_config,
        dir.path().join("sandboxes"),
    )
    .expect("manager");
    (manager, dir)
}

#[tokio::test]
async fn health_keeps_restart_evidence_visible_while_store_is_busy() {
    let directory = TempDir::new().unwrap();
    let store = Store::open(&directory.path().join("rsi.db")).unwrap();
    let observed_at = chrono::Utc::now();
    let restart = crate::watchdog::RestartRecord {
        version: 1,
        id: Uuid::new_v4(),
        observed_at,
        last_healthy_at: observed_at - chrono::Duration::seconds(30),
        failed_probes: vec!["store_probe_timeout".to_owned()],
    };
    store.persist_daemon_restart_record(&restart).unwrap();
    let runtime_config = RuntimeConfig::from_config(&Config::from_env());
    let manager = SessionManager::new(
        Arc::new(EventBus::new(16)),
        store,
        false,
        directory.path().join("daemon.sock"),
        None,
        Vec::new(),
        runtime_config,
        directory.path().join("sandboxes"),
    )
    .unwrap();

    let _busy_store = manager.store.lock().await;
    let health = manager.get_health_status().await;
    let visible = health.latest_daemon_restart.unwrap();
    assert_eq!(visible.id, restart.id);
    assert_eq!(visible.failed_probes, restart.failed_probes);
}

fn bare_session(id: Uuid) -> Session {
    let now = chrono::Utc::now();
    Session {
        context_fill_pct: None,
        id,
        status: SessionStatus::Completed,
        session_kind: SessionKind::Standard,
        provider: SessionProvider::default(),
        context_usage_confidence: ContextUsageConfidence::default(),
        rotation_depth: 0,
        retry_attempt: None,
        max_retries: None,
        created_at: now,
        updated_at: now,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        query: "test".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        working_dir: std::path::PathBuf::from("/tmp"),
        git_branch: None,
        model: None,
        claude_session_id: None,
        project_id: None,
        continued_from: None,
        tag: String::new(),
        tags: Vec::new(),
        parent_id: None,
        lead_session_id: None,
        is_eval: false,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        scheduled_job_id: None,
        stop_reason: None,
        cost_usd: None,
        duration_ms: None,
        num_turns: None,
        input_tokens: None,
        output_tokens: None,
        context_window: None,
        resolved_context_budget: None,
        total_input_tokens: None,
        total_output_tokens: None,
        total_cache_creation_tokens: None,
        total_cache_read_tokens: None,
        daemon_input_tokens: None,
        daemon_output_tokens: None,
        workflow_id: None,
        workflow_id_override: None,
        pipeline_artifact: None,
        pending_question: None,
        pending_archive: false,
        effort: None,
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
        rating: None,
        harness_version_hash: None,
        test_passed: None,
        clippy_passed: None,
        turn_count: None,
        retry_count: None,
        approval_wait_ms: Some(0),
        work_time_ms: None,
        approval_started_at: None,
        sandbox_kind: None,
        sandbox_root: None,
        sandbox_branch: None,
        sandbox_cleanup_state: None,
        capability_class: None,
        topology_node_id: None,
        topology_iteration: 0,
        provider_cli_version: None,
        provider_capabilities: Vec::new(),
        thinking_tokens: None,
        service_tier: None,
        cache_creation_1h_tokens: None,
        cache_creation_5m_tokens: None,
        permission_denial_count: None,
        subagent_stats_json: None,
        queued_turn_count: None,
        terminal_reason: None,
    }
}

async fn pending_provider_settlement(
    manager: &std::sync::Arc<SessionManager>,
) -> (Uuid, ModelCallSettlement) {
    let session_id = Uuid::new_v4();
    let request = ModelAdmissionRequest {
        purpose: ModelInvocationPurpose::SessionLaunchFresh,
        provider: Some("Harness".to_string()),
        model: Some("gpt-5.4".to_string()),
        backend: Some("Harness".to_string()),
        effort: Some("high".to_string()),
        trigger: "test_shutdown_late_provider_owner".to_string(),
        owner: InvocationOwner {
            session_id: Some(session_id),
            ..Default::default()
        },
        dedup_key: Some(format!("shutdown-root:{session_id}")),
        request_fingerprint: Some("sha256:shutdown-root".to_string()),
        parent_invocation_id: None,
        retry_of_invocation_id: None,
        expected_usage: Some(ExpectedUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            reasoning_tokens: 0,
            embedding_input_count: 0,
            wall_time_ms: 1_000,
        }),
        baseline_input_tokens: 0,
        baseline_output_tokens: 0,
        baseline_cache_creation_tokens: 0,
        baseline_cache_read_tokens: 0,
        baseline_reasoning_tokens: 0,
        baseline_embedding_input_count: 0,
        baseline_wall_time_ms: 0,
    };
    let permit = match admit_invocation(&manager.store, request, manager.event_bus())
        .await
        .expect("admission")
    {
        AdmissionDecision::Admitted(permit) => permit,
        AdmissionDecision::Duplicate { invocation_id } => {
            panic!("unexpected duplicate admission: {invocation_id}")
        }
    };
    let invocation_id = permit.invocation_id();
    let control = StoreBackedModelCallControl::new(
        std::sync::Arc::clone(&manager.store),
        std::sync::Arc::clone(manager.event_bus()),
        manager
            .model_call_settlements
            .handle()
            .expect("settlement producer"),
        InvocationOwner {
            session_id: Some(session_id),
            ..Default::default()
        },
        "Harness",
        Some("gpt-5.4".to_string()),
        "Harness",
        Some("high".to_string()),
        "test_shutdown_late_provider_owner",
        permit,
        ModelInvocationPurpose::SessionHarnessTurn,
        Some(ModelInvocationPurpose::SessionHarnessCompaction),
        RuntimeExecutionRoute::SessionHarnessOpenAiHttp,
    );
    let admitted = control
        .admit(ModelCallKind::Primary, "turn:0", "request", None)
        .await
        .expect("admitted call");
    let (settlement, execution) = admitted.into_parts();
    drop(execution);
    drop(control);
    (invocation_id, settlement)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_waits_for_a_late_provider_settlement_owner() {
    let (manager, _dir) = manager();
    let manager = std::sync::Arc::new(manager);
    let (invocation_id, settlement) = pending_provider_settlement(&manager).await;
    let shutdown = {
        let manager = std::sync::Arc::clone(&manager);
        tokio::spawn(async move { manager.shutdown().await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !shutdown.is_finished(),
        "SessionManager shutdown must wait for the provider-owned settlement"
    );
    let running = manager
        .store
        .lock()
        .await
        .load_model_invocation_record(invocation_id)
        .expect("load invocation")
        .expect("invocation row");
    assert_eq!(running.status, ModelInvocationStatus::Running);

    drop(settlement);
    tokio::time::timeout(std::time::Duration::from_secs(1), shutdown)
        .await
        .expect("shutdown completes after provider ownership is released")
        .expect("shutdown task")
        .expect("SessionManager shutdown");
    let terminal = manager
        .store
        .lock()
        .await
        .load_model_invocation_record(invocation_id)
        .expect("load invocation")
        .expect("invocation row");
    assert_eq!(terminal.status, ModelInvocationStatus::Failed);
    assert_eq!(
        terminal.error_class.as_deref(),
        Some("model_call_task_exited")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_retries_producer_timeout_until_the_provider_owner_settles() {
    let (manager, _dir) = manager();
    let manager = std::sync::Arc::new(manager);
    let (invocation_id, settlement) = pending_provider_settlement(&manager).await;
    let shutdown = {
        let manager = std::sync::Arc::clone(&manager);
        tokio::spawn(async move { manager.shutdown().await })
    };

    tokio::time::sleep(std::time::Duration::from_millis(325)).await;
    assert!(
        !shutdown.is_finished(),
        "production shutdown must stay in recovery after its first producer timeout"
    );
    let running = manager
        .store
        .lock()
        .await
        .load_model_invocation_record(invocation_id)
        .expect("load invocation")
        .expect("invocation row");
    assert_eq!(running.status, ModelInvocationStatus::Running);

    drop(settlement);
    tokio::time::timeout(std::time::Duration::from_secs(2), shutdown)
        .await
        .expect("shutdown recovery converges after producer ownership releases")
        .expect("shutdown task")
        .expect("SessionManager shutdown");
    let terminal = manager
        .store
        .lock()
        .await
        .load_model_invocation_record(invocation_id)
        .expect("load invocation")
        .expect("invocation row");
    assert_eq!(terminal.status, ModelInvocationStatus::Failed);
    assert_eq!(
        terminal.error_class.as_deref(),
        Some("model_call_task_exited")
    );
}

#[test]
fn test_convert_stream_event_message() {
    let stream = StreamEvent {
        event_type: "assistant".to_string(),
        data: serde_json::json!({
            "role": "assistant",
            "content": "Hello, world!"
        }),
    };
    let mut seq = 0;
    let events = SessionManager::convert_recognized_stream_event(&stream, Uuid::new_v4(), &mut seq)
        .expect("the fixture's event type must stay recognized by the converter");
    assert_eq!(events.len(), 1);
    let event = &events[0];

    assert_eq!(event.event_type, EventType::Message);
    assert_eq!(event.role, Some(Role::Assistant));
    assert_eq!(event.content, "Hello, world!");
    assert_eq!(event.sequence, 1);
}

#[test]
fn test_convert_stream_event_tool_use() {
    let stream = StreamEvent {
        event_type: "tool_use".to_string(),
        data: serde_json::json!({
            "name": "read_file",
            "input": {"path": "/tmp/test.txt"}
        }),
    };
    let mut seq = 5;
    let events = SessionManager::convert_recognized_stream_event(&stream, Uuid::new_v4(), &mut seq)
        .expect("the fixture's event type must stay recognized by the converter");
    assert_eq!(events.len(), 1);
    let event = &events[0];

    assert_eq!(event.event_type, EventType::ToolUse);
    assert_eq!(event.tool_name, Some("read_file".to_string()));
    assert!(event.tool_input.is_some());
    assert_eq!(event.sequence, 6);
}

#[test]
fn test_convert_stream_event_content_blocks() {
    let stream = StreamEvent {
        event_type: "assistant".to_string(),
        data: serde_json::json!({
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Line 1"},
                    {"type": "text", "text": "Line 2"}
                ]
            }
        }),
    };
    let mut seq = 0;
    let events = SessionManager::convert_recognized_stream_event(&stream, Uuid::new_v4(), &mut seq)
        .expect("the fixture's event type must stay recognized by the converter");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].content, "Line 1\nLine 2");
}

#[test]
fn test_convert_stream_event_content_block_tool_use() {
    // Real Claude Code CLI stream-json format: tool calls arrive as
    // {"type":"tool_use", ...} blocks embedded in the assistant message's
    // content array, not as their own top-level "tool_use" stream event.
    // Regression for the dropped-tool_use-block bug (session-detail
    // "N tool calls" undercount).
    let stream = StreamEvent {
        event_type: "assistant".to_string(),
        data: serde_json::json!({
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Let me check that file."},
                    {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"path": "/tmp/a.txt"}}
                ]
            }
        }),
    };
    let mut seq = 0;
    let events = SessionManager::convert_recognized_stream_event(&stream, Uuid::new_v4(), &mut seq)
        .expect("the fixture's event type must stay recognized by the converter");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, EventType::Message);
    assert_eq!(events[0].content, "Let me check that file.");
    assert_eq!(events[1].event_type, EventType::ToolUse);
    assert_eq!(events[1].tool_name, Some("Read".to_string()));
    assert_eq!(
        events[1].tool_input.as_deref(),
        Some(&serde_json::json!({"path": "/tmp/a.txt"}))
    );
}

#[test]
fn test_convert_stream_event_content_block_parallel_tool_use() {
    // Parallel tool calls: multiple tool_use blocks in one content array,
    // no interleaved text. All must convert, in array order.
    let stream = StreamEvent {
        event_type: "assistant".to_string(),
        data: serde_json::json!({
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"path": "/a"}},
                    {"type": "tool_use", "id": "toolu_2", "name": "Read", "input": {"path": "/b"}},
                    {"type": "tool_use", "id": "toolu_3", "name": "Read", "input": {"path": "/c"}}
                ]
            }
        }),
    };
    let mut seq = 0;
    let events = SessionManager::convert_recognized_stream_event(&stream, Uuid::new_v4(), &mut seq)
        .expect("the fixture's event type must stay recognized by the converter");
    assert_eq!(events.len(), 3);
    for (i, ev) in events.iter().enumerate() {
        assert_eq!(ev.event_type, EventType::ToolUse);
        assert_eq!(ev.tool_name, Some("Read".to_string()));
        assert!(ev.tool_input.is_some());
        assert_eq!(ev.sequence, (i as i32) + 1);
    }
}

#[test]
fn test_convert_stream_event_thinking_only() {
    let stream = StreamEvent {
        event_type: "assistant".to_string(),
        data: serde_json::json!({
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "Let me reason about this..."}
                ]
            }
        }),
    };
    let mut seq = 0;
    let events = SessionManager::convert_recognized_stream_event(&stream, Uuid::new_v4(), &mut seq)
        .expect("the fixture's event type must stay recognized by the converter");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, EventType::Thinking);
    assert_eq!(events[0].content, "Let me reason about this...");
}

#[test]
fn test_convert_stream_event_thinking_and_text() {
    let stream = StreamEvent {
        event_type: "assistant".to_string(),
        data: serde_json::json!({
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "Reasoning..."},
                    {"type": "text", "text": "Here is my answer"}
                ]
            }
        }),
    };
    let mut seq = 0;
    let events = SessionManager::convert_recognized_stream_event(&stream, Uuid::new_v4(), &mut seq)
        .expect("the fixture's event type must stay recognized by the converter");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, EventType::Thinking);
    assert_eq!(events[0].content, "Reasoning...");
    assert_eq!(events[0].sequence, 1);
    assert_eq!(events[1].event_type, EventType::Message);
    assert_eq!(events[1].content, "Here is my answer");
    assert_eq!(events[1].sequence, 2);
}

#[test]
fn test_convert_stream_event_empty_content_skipped() {
    let stream = StreamEvent {
        event_type: "assistant".to_string(),
        data: serde_json::json!({
            "role": "assistant",
            "content": ""
        }),
    };
    let mut seq = 0;
    let events = SessionManager::convert_recognized_stream_event(&stream, Uuid::new_v4(), &mut seq)
        .expect("the fixture's event type must stay recognized by the converter");
    assert!(
        events.is_empty(),
        "Empty string content should produce no events"
    );
}

#[test]
fn test_convert_stream_event_no_content_skipped() {
    let stream = StreamEvent {
        event_type: "assistant".to_string(),
        data: serde_json::json!({
            "role": "assistant"
        }),
    };
    let mut seq = 0;
    let events = SessionManager::convert_recognized_stream_event(&stream, Uuid::new_v4(), &mut seq)
        .expect("the fixture's event type must stay recognized by the converter");
    assert!(
        events.is_empty(),
        "Missing content should produce no events"
    );
}

#[test]
fn test_create_user_event() {
    let session_id = Uuid::new_v4();
    let event = SessionManager::create_user_event(session_id, 1, "fix the tests");

    assert_eq!(event.session_id, session_id);
    assert_eq!(event.sequence, 1);
    assert_eq!(event.event_type, EventType::Message);
    assert_eq!(event.role, Some(Role::User));
    assert_eq!(event.content, "fix the tests");
    assert!(event.tool_name.is_none());
    assert!(event.tool_input.is_none());
    assert_eq!(event.id, 0); // Placeholder before DB assignment
}

#[test]
fn test_convert_stream_event_result_returns_empty() {
    let stream = StreamEvent {
        event_type: "result".to_string(),
        data: serde_json::json!({"subtype": "success"}),
    };
    let mut seq = 0;
    let events = SessionManager::convert_recognized_stream_event(&stream, Uuid::new_v4(), &mut seq)
        .expect("the fixture's event type must stay recognized by the converter");

    assert!(events.is_empty());
}

#[test]
fn test_pipeline_path_regex() {
    // Research paths
    assert!(PIPELINE_PATH_RE.is_match("thoughts/shared/research/2026-03-04-foo.md"));
    assert!(PIPELINE_PATH_RE.is_match("some/prefix/thoughts/shared/research/bar.md"));

    // Plans paths
    assert!(PIPELINE_PATH_RE.is_match("thoughts/shared/plans/2026-03-04-write-tool.md"));

    // Handoffs paths
    assert!(PIPELINE_PATH_RE.is_match("thoughts/shared/handoffs/general/2026-03-04.md"));

    // Non-matching paths
    assert!(!PIPELINE_PATH_RE.is_match("thoughts/shared/other/foo.md"));
    assert!(!PIPELINE_PATH_RE.is_match("thoughts/shared/research/foo.txt"));
    assert!(!PIPELINE_PATH_RE.is_match("random/path/file.md"));

    // RSI-014: JSON sidecars are intentionally invisible to pipeline detection.
    // The daemon only auto-derives sessions from `.md` writes; `<doc>.json`
    // companions are an internal contract between writer and consumer skills.
    assert!(!PIPELINE_PATH_RE.is_match("thoughts/shared/research/2026-04-25-foo.json"));
    assert!(!PIPELINE_PATH_RE.is_match("thoughts/shared/plans/2026-04-25-foo.json"));
    assert!(!PIPELINE_PATH_RE.is_match("thoughts/shared/handoffs/general/2026-04-25-foo.json"));

    // Extract match from text
    let text = "I wrote the plan to `thoughts/shared/plans/2026-03-04-foo.md` successfully";
    let cap = PIPELINE_PATH_RE.find(text).unwrap();
    assert_eq!(cap.as_str(), "thoughts/shared/plans/2026-03-04-foo.md");
}

#[tokio::test]
async fn test_schedule_memory_sync_enqueues_sync_now() {
    let (tx, mut rx) = mpsc::channel(1);
    schedule_memory_sync(
        Some(MemoryHandle::new(tx)),
        "session_project_changed:deadbeef".to_string(),
    );

    let cmd = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("sync command should be sent")
        .expect("channel should stay open");

    match cmd {
        MemoryCommand::SyncNow { force, reason } => {
            assert!(!force);
            assert_eq!(reason, "session_project_changed:deadbeef");
        }
        other => panic!("expected SyncNow command, got {other:?}"),
    }
}

// ── Approval-wait accumulator tests ───────────────────────────────────────
// Verify the intra-session WaitingApproval duration accumulator accumulates
// across multiple open-close cycles (set-site in monitor.rs ↔ clear-site in
// lifecycle.rs::answer_question) and that the terminalization hook in
// finalize_session closes any still-open interval before snapshotting to
// Session.approval_wait_ms. These tests drive the struct fields directly
// rather than the full monitor loop — the arithmetic is independent of how
// the set/clear sites are wired up.

/// Simulate the set-site (monitor.rs AskUserQuestion branch).
fn open_approval_interval(start: &mut Option<std::time::Instant>) {
    if start.is_none() {
        *start = Some(std::time::Instant::now());
    }
}

/// Simulate the clear-site (lifecycle.rs answer_question / finalize snapshot).
fn close_approval_interval(start: &mut Option<std::time::Instant>, total_ms: &mut u64) {
    if let Some(started) = start.take() {
        let elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        *total_ms = total_ms.saturating_add(elapsed_ms);
    }
}

#[test]
fn approval_wait_accumulates_across_multiple_questions() {
    let mut start: Option<std::time::Instant> = None;
    let mut total_ms: u64 = 0;

    // First open-close cycle (~15 ms wait).
    open_approval_interval(&mut start);
    assert!(start.is_some(), "open set the timer");
    std::thread::sleep(std::time::Duration::from_millis(15));
    close_approval_interval(&mut start, &mut total_ms);
    assert!(start.is_none(), "close took the timer");
    let first_total = total_ms;
    assert!(
        first_total >= 15,
        "first interval should be ≥15 ms, got {}",
        first_total
    );

    // Second open-close cycle (~10 ms wait).
    open_approval_interval(&mut start);
    std::thread::sleep(std::time::Duration::from_millis(10));
    close_approval_interval(&mut start, &mut total_ms);

    // Total is cumulative — never resets between cycles.
    assert!(
        total_ms >= first_total + 10,
        "total after two cycles ({}) should be at least first ({}) + 10 ms",
        total_ms,
        first_total
    );
}

// ── TD1: work_time_ms fold accumulator tests ─────────────────────────────
// Verify `TrackedSession::recompute_work_time` (Running-interval MINUS
// approval-wait, D1) directly on the struct fields — no monitor loop, mirroring
// the approval-wait tests above. Covers: (a) work grows with no open approval;
// (b) work stays FLAT across an open→close approval interval; (c) two
// recomputes spaced by real wall-clock time on a plain Running session never
// decrease (running-monotonic, stands in for the DAEMON AUTONOMOUS "two
// GetSession reads ≥10s apart" check, hermetically); (d) monotonic after a
// `work_time_base_ms` bump (continue semantics — ADD to the reused floor,
// never reset).
#[test]
fn recompute_work_time_folds_running_minus_approval_wait() {
    let mut tracked = TrackedSession::new_for_test(bare_session(Uuid::new_v4()));

    // (a) Plain Running, no approval: work grows with wall-clock time.
    tracked.work_run_start = Some(std::time::Instant::now());
    tracked.work_time_base_ms = 0;
    std::thread::sleep(std::time::Duration::from_millis(15));
    tracked.recompute_work_time();
    let after_first_tick = tracked
        .session
        .work_time_ms
        .expect("work_time_ms set once Running");
    assert!(
        after_first_tick >= 15,
        "work should grow by at least the sleep duration, got {after_first_tick}"
    );

    // (c) Running-monotonic: a second recompute after further wall-clock time
    // must never decrease (hermetic stand-in for two live GetSession polls).
    std::thread::sleep(std::time::Duration::from_millis(10));
    tracked.recompute_work_time();
    let after_second_tick = tracked.session.work_time_ms.unwrap();
    assert!(
        after_second_tick >= after_first_tick + 10,
        "second recompute ({after_second_tick}) should be at least first ({after_first_tick}) + 10ms"
    );

    // (b) Open an approval interval: work must stay FLAT while it's open —
    // run_elapsed and approval_elapsed grow at the same rate, so the
    // subtraction cancels out.
    tracked.approval_wait_start = Some(std::time::Instant::now());
    std::thread::sleep(std::time::Duration::from_millis(15));
    tracked.recompute_work_time();
    let during_approval = tracked.session.work_time_ms.unwrap();
    assert!(
        during_approval >= after_second_tick,
        "work must never decrease, even mid-approval-wait"
    );
    assert!(
        during_approval < after_second_tick + 10,
        "work must stay ~flat while approval is open, got {during_approval} vs {after_second_tick}"
    );

    // Close the approval interval — mirrors the monitor.rs open-site /
    // lifecycle.rs close-site fold.
    let elapsed_ms = tracked
        .approval_wait_start
        .take()
        .unwrap()
        .elapsed()
        .as_millis() as u64;
    tracked.approval_wait_total_ms = tracked.approval_wait_total_ms.saturating_add(elapsed_ms);

    std::thread::sleep(std::time::Duration::from_millis(10));
    tracked.recompute_work_time();
    let after_close = tracked.session.work_time_ms.unwrap();
    assert!(
        after_close >= during_approval + 10,
        "work should resume growing once approval closes"
    );

    // (d) Continue semantics: a fresh Running interval with a bumped
    // `work_time_base_ms` (the persisted floor reused at monitor.rs Site 1)
    // must ADD to the prior total, never overwrite/reset it.
    let prior_total = after_close;
    tracked.work_run_start = Some(std::time::Instant::now());
    tracked.work_time_base_ms = prior_total;
    tracked.approval_wait_total_ms = 0;
    tracked.recompute_work_time();
    let after_continue = tracked.session.work_time_ms.unwrap();
    assert!(
        after_continue >= prior_total,
        "continue must ADD to the reused floor ({prior_total}), not reset it; got {after_continue}"
    );
}

#[tokio::test]
async fn continue_failure_reinserts_completed_session() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.provider = SessionProvider::Codex;
    session.claude_session_id = Some("resume-token".to_string());
    session.working_dir = std::path::PathBuf::from("/definitely/missing/rsi-session-dir");
    insert_row(&manager, &session).await;

    manager.completed.write().await.insert(
        session_id,
        CompletedSession {
            session,
            events: Vec::new(),
            turn_metrics: Vec::new(),
            retry_cancel: None,
            retry_fired_at: None,
            superseded_by_retry: None,
            events_hydrated: true,
        },
    );

    let err = manager
        .continue_session(session_id, "continue".to_string())
        .await
        .expect_err("continue should fail when launch cannot start");

    assert!(
        manager.completed.read().await.contains_key(&session_id),
        "failed continue must restore the completed session to memory"
    );
    assert!(
        matches!(
            err,
            crate::error::DaemonError::CodexBinaryNotFound | crate::error::DaemonError::Io(_)
        ),
        "expected a launch-time failure, got {err:?}"
    );
}

#[tokio::test]
async fn continue_orphan_reap_failure_settles_admission_for_retry() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.provider = SessionProvider::Codex;
    session.claude_session_id = Some("resume-token".to_string());
    insert_row(&manager, &session).await;

    manager.completed.write().await.insert(
        session_id,
        CompletedSession {
            session,
            events: Vec::new(),
            turn_metrics: Vec::new(),
            retry_cancel: None,
            retry_fired_at: None,
            superseded_by_retry: None,
            events_hydrated: true,
        },
    );
    super::reaper::fail_runtime_orphan_reap_for_test(session_id);

    let error = manager
        .continue_session(session_id, "first continuation".to_string())
        .await
        .expect_err("the injected orphan proof failure must refuse provider spawn");
    assert!(
        error
            .to_string()
            .contains("injected runtime orphan reap failure"),
        "unexpected continuation failure: {error}"
    );

    let invocation = manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT status, error_class FROM model_invocations
             WHERE session_id = ?1 AND purpose = 'session.continue.resume'",
            rusqlite::params![session_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .expect("load refused resume admission");
    assert_eq!(invocation.0, "failed");
    assert_eq!(invocation.1.as_deref(), Some("orphan_reap_failed"));
    assert!(
        manager.completed.read().await.contains_key(&session_id),
        "failed pre-spawn proof must restore the completed session"
    );

    let _retry_error = manager
        .continue_session(session_id, "second continuation".to_string())
        .await
        .expect_err("the test environment must refuse the provider launch");
    let invocation_count = manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT COUNT(*) FROM model_invocations
             WHERE session_id = ?1 AND purpose = 'session.continue.resume'",
            rusqlite::params![session_id.to_string()],
            |row| row.get::<_, i64>(0),
        )
        .expect("count resume attempts");
    assert_eq!(invocation_count, 2, "retry must receive a new admission");
}

#[tokio::test]
async fn continue_recovers_completed_session_missing_from_memory() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.provider = SessionProvider::Codex;
    session.claude_session_id = Some("resume-token".to_string());
    session.working_dir = std::path::PathBuf::from("/definitely/missing/rsi-session-dir");

    // Simulate a session that is durably Completed in the store but absent
    // from both in-memory maps (e.g. a restart raced restoring it) -- the
    // exact scenario that used to make `continue_session` report a false
    // `SessionNotFound` for a row that plainly exists.
    manager
        .store()
        .lock()
        .await
        .insert_session(&session)
        .expect("insert session row directly into the store");
    assert!(
        !manager.completed.read().await.contains_key(&session_id),
        "precondition: session must be absent from the in-memory completed map"
    );
    assert!(
        !manager.active.read().await.contains_key(&session_id),
        "precondition: session must be absent from the in-memory active map"
    );

    let err = manager
        .continue_session(session_id, "continue".to_string())
        .await
        .expect_err("continue should still fail at launch in test env");

    assert!(
        !matches!(err, crate::error::DaemonError::SessionNotFound(_)),
        "a session durably Completed in the store must not be reported as \
         not found just because it's missing from the in-memory completed \
         map, got {err:?}"
    );
    assert!(
        matches!(
            err,
            crate::error::DaemonError::CodexBinaryNotFound | crate::error::DaemonError::Io(_)
        ),
        "expected a launch-time failure once the store fallback resolves the \
         session, got {err:?}"
    );
}

/// `AgentHalt` on a row that is already terminal and has no surviving
/// stamped orphan must report success, not `SessionNotFound`.
///
/// The requested end state -- "not running" -- already holds, so the halt
/// intent is satisfied. Reporting `SessionNotFound` here was a false answer:
/// the row exists and is readable through `AgentGetStatus`, and the ordinary
/// race (a master halting a child that just finished) was reported as a
/// missing session, which invites a pointless re-resolve or re-spawn loop.
/// This mirrors `continue_recovers_completed_session_missing_from_memory`,
/// where the same class of false `SessionNotFound` was already removed.
#[tokio::test]
async fn halt_of_terminal_session_without_surviving_orphan_reports_success() {
    let (manager, _dir) = manager();
    let target_id = Uuid::new_v4();
    let mut target = bare_session(target_id);
    target.status = SessionStatus::Interrupted;

    manager
        .store()
        .lock()
        .await
        .insert_session(&target)
        .expect("insert terminal target row");

    // Precondition: the false answer was produced because neither in-memory
    // ownership map held the row, forcing the terminal-orphan-reap fallback.
    assert!(
        !manager.active.read().await.contains_key(&target_id),
        "precondition: target must be absent from the active map"
    );
    assert!(
        !manager.completed.read().await.contains_key(&target_id),
        "precondition: target must be absent from the completed map"
    );

    // Self-target is the narrowest authorized scope
    // (`authorize_agent_target` admits the caller's own row), so the halt
    // reaches the terminal-orphan-reap fallback without needing a fixture
    // parent/Epic hierarchy for this assertion.
    manager
        .agent_control()
        .agent_halt(target_id, target_id)
        .await
        .expect("an already-terminal target whose intent is satisfied must halt successfully");
}

/// The `SessionNotFound` contract is unchanged for ids genuinely absent from
/// `sessions`: only the false answer for an existing terminal row is removed.
#[tokio::test]
async fn halt_of_absent_session_still_reports_not_found() {
    let (manager, _dir) = manager();
    let missing = Uuid::new_v4();

    let err = manager
        .agent_control()
        .agent_halt(missing, missing)
        .await
        .expect_err("an absent session id must still be reported as not found");
    assert!(
        matches!(err, crate::error::DaemonError::SessionNotFound(id) if id == missing),
        "expected SessionNotFound for an absent id, got {err:?}"
    );
}

/// C7 Phase 1: `continue_session` carries `completed_session.events` forward
/// verbatim into the resumed `TrackedSession` (initial sequence probe,
/// provider replay history). If it used a restored session's hydration
/// placeholder (`events: Vec::new()`, `events_hydrated: false` -- the exact
/// state `SessionManager::restore_sessions` now inserts, see
/// `session::launch::tests::restore_sessions_defers_event_load_and_hydrates_on_conversation_read`)
/// without hydrating first, a resumed session would silently lose its entire
/// prior transcript. The launch itself is forced to fail fast (missing Codex
/// binary / working dir), but hydration runs before that failure, and the
/// failure path reinserts the completed session -- so the map's post-failure
/// state proves whether hydration happened.
#[tokio::test]
async fn continue_hydrates_restored_placeholder_before_reinserting_on_failure() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.provider = SessionProvider::Codex;
    session.claude_session_id = Some("resume-token".to_string());
    session.working_dir = std::path::PathBuf::from("/definitely/missing/rsi-session-dir");

    manager
        .store()
        .lock()
        .await
        .insert_session(&session)
        .expect("insert session row directly into the store");
    let durable_events: Vec<ConversationEvent> = (0..2)
        .map(|i| ConversationEvent {
            id: 0,
            session_id,
            sequence: i,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            created_at: chrono::Utc::now(),
            content: format!("durable transcript event {i}"),
            tool_name: None,
            tool_input: None,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        })
        .collect();
    {
        let store = manager.store().lock().await;
        for event in &durable_events {
            store.insert_event(event).expect("insert durable event");
        }
    }

    // The exact placeholder shape `restore_sessions` inserts: no events in
    // memory yet, marked unhydrated.
    manager.completed.write().await.insert(
        session_id,
        CompletedSession {
            session,
            events: Vec::new(),
            turn_metrics: Vec::new(),
            retry_cancel: None,
            retry_fired_at: None,
            superseded_by_retry: None,
            events_hydrated: false,
        },
    );

    let err = manager
        .continue_session(session_id, "continue".to_string())
        .await
        .expect_err("continue should still fail at launch in test env");
    assert!(
        matches!(
            err,
            crate::error::DaemonError::CodexBinaryNotFound | crate::error::DaemonError::Io(_)
        ),
        "expected a launch-time failure, got {err:?}"
    );

    let completed = manager.completed.read().await;
    let reinserted = completed
        .get(&session_id)
        .expect("failed continue must restore the completed session to memory");
    assert!(
        reinserted.events_hydrated,
        "continue_session must hydrate the transcript before using it, even \
         though the launch itself fails afterward"
    );
    assert_eq!(
        reinserted.events.len(),
        2,
        "hydration must load the full durable transcript, not drop it"
    );
    for (hydrated, original) in reinserted.events.iter().zip(durable_events.iter()) {
        assert_eq!(hydrated.sequence, original.sequence);
        assert_eq!(hydrated.content, original.content);
    }
}

#[tokio::test]
async fn remint_revokes_prior_token_and_registers_new() {
    let (manager, _dir) = manager();
    let sid = Uuid::new_v4();
    let other_sid = Uuid::new_v4();

    // Seed a prior token for `sid` and an unrelated session's token that
    // must survive the re-mint untouched.
    manager
        .register_agent_token("token-a".to_string(), sid)
        .await;
    manager
        .register_agent_token("token-other".to_string(), other_sid)
        .await;

    let new_token = manager.remint_session_token(sid).await;
    assert!(
        manager.resolve_agent_token("token-a").await.is_none(),
        "re-mint must revoke the session's prior token"
    );
    assert_eq!(
        manager.resolve_agent_token(&new_token).await,
        Some(sid),
        "re-minted token must resolve to the session"
    );
    assert_eq!(
        manager.resolve_agent_token("token-other").await,
        Some(other_sid),
        "other sessions' tokens must be untouched"
    );

    // Fresh sid: the revoke half is a no-op — pure register.
    let fresh_sid = Uuid::new_v4();
    let fresh_token = manager.remint_session_token(fresh_sid).await;
    assert_eq!(
        manager.resolve_agent_token(&fresh_token).await,
        Some(fresh_sid)
    );

    // Re-minting again keeps at most one live token per session.
    let second_token = manager.remint_session_token(sid).await;
    assert!(manager.resolve_agent_token(&new_token).await.is_none());
    assert_eq!(manager.resolve_agent_token(&second_token).await, Some(sid));
    let live_for_sid = manager
        .agent_tokens
        .read()
        .await
        .values()
        .filter(|s| **s == sid)
        .count();
    assert_eq!(live_for_sid, 1, "at most one live token per session id");
}

#[test]
fn d05_agent_token_registry_remint_rebinding_collision_and_revoke_stay_bijective() {
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let mut registry = super::AgentTokenRegistry::default();

    assert_eq!(registry.insert("first-a".into(), first), None);
    assert_eq!(registry.insert("first-b".into(), first), None);
    assert_eq!(
        registry.get("first-a"),
        None,
        "same-session remint revokes forward mapping"
    );
    assert_eq!(registry.get("first-b"), Some(&first));
    assert_eq!(registry.token_for_session(first), Some("first-b"));

    assert_eq!(registry.insert("second".into(), second), None);
    assert_eq!(registry.insert("second".into(), first), Some(second));
    assert_eq!(
        registry.get("first-b"),
        None,
        "collision removes the old session token"
    );
    assert_eq!(registry.get("second"), Some(&first));
    assert_eq!(registry.token_for_session(first), Some("second"));
    assert_eq!(
        registry.token_for_session(second),
        None,
        "same-token rebinding removes the displaced reverse mapping"
    );
    assert_eq!(registry.by_token.len(), registry.by_session.len());
    for (token, session_id) in &registry.by_token {
        assert_eq!(registry.by_session.get(session_id), Some(token));
    }

    assert!(
        !registry.revoke_session_if_token(first, "first-b"),
        "stale cleanup must not revoke a newer incarnation's token"
    );
    assert_eq!(registry.token_for_session(first), Some("second"));
    assert!(registry.revoke_session_if_token(first, "second"));
    assert!(registry.is_empty());
    assert!(registry.by_session.is_empty());
    assert_eq!(registry.get("second"), None);
    assert_eq!(registry.token_for_session(first), None);
}

#[tokio::test]
async fn continue_registers_working_agent_token() {
    // G1 (A6): continue re-mints before guarded spawn, superseding stale
    // authority. If the provider never establishes, launch cleanup must also
    // revoke that prospective token (same missing-working_dir seam as
    // `continue_failure_reinserts_completed_session`).
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.provider = SessionProvider::Codex;
    session.claude_session_id = Some("resume-token".to_string());
    session.working_dir = std::path::PathBuf::from("/definitely/missing/rsi-session-dir");
    insert_row(&manager, &session).await;

    manager.completed.write().await.insert(
        session_id,
        CompletedSession {
            session,
            events: Vec::new(),
            turn_metrics: Vec::new(),
            retry_cancel: None,
            retry_fired_at: None,
            superseded_by_retry: None,
            events_hydrated: true,
        },
    );

    // Stale token from the session's previous process (pre-continue).
    manager
        .register_agent_token("stale-continue-token".to_string(), session_id)
        .await;

    manager
        .continue_session(session_id, "continue".to_string())
        .await
        .expect_err("continue should fail when launch cannot start");

    assert!(
        manager
            .resolve_agent_token("stale-continue-token")
            .await
            .is_none(),
        "continue must revoke the session's prior token (supersession)"
    );
    let registry = manager.agent_tokens.read().await;
    assert_eq!(
        registry.token_for_session(session_id),
        None,
        "failed continuation must revoke the prospective token"
    );
    assert!(
        registry.values().all(|mapped| *mapped != session_id),
        "failed continuation must leave neither stale nor prospective authority"
    );
    drop(registry);
}

#[tokio::test]
async fn continue_cancel_exhausts_retry_budget_in_store() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.status = SessionStatus::Failed;
    session.provider = SessionProvider::Codex;
    session.claude_session_id = Some("resume-token".to_string());
    session.working_dir = std::path::PathBuf::from("/definitely/missing/rsi-session-dir");
    session.retry_attempt = Some(0);
    session.max_retries = Some(2);
    insert_row(&manager, &session).await;

    let (cancel_tx, _cancel_rx) = tokio::sync::oneshot::channel();
    let mut completed = CompletedSession::for_test(session);
    completed.retry_cancel = Some(cancel_tx);
    manager
        .completed
        .write()
        .await
        .insert(session_id, completed);

    manager
        .continue_session(session_id, "continue".to_string())
        .await
        .expect_err("continue should reach launch and fail");

    let row = wait_for_retry_state(&manager, session_id, Some(2), Some(2)).await;
    assert_eq!(row.retry_attempt, Some(2));
    assert_eq!(row.max_retries, Some(2));
}

#[tokio::test]
async fn continue_interrupts_live_retry_child_before_proceeding() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let retry_child_id = Uuid::new_v4();
    let mut parent = bare_session(session_id);
    parent.status = SessionStatus::Failed;
    parent.provider = SessionProvider::Codex;
    parent.claude_session_id = Some("resume-token".to_string());
    parent.working_dir = std::path::PathBuf::from("/definitely/missing/rsi-session-dir");
    let mut completed = CompletedSession::for_test(parent);
    completed.superseded_by_retry = Some(retry_child_id);
    manager
        .completed
        .write()
        .await
        .insert(session_id, completed);

    let mut child_session = bare_session(retry_child_id);
    child_session.status = SessionStatus::Running;
    let mut child = TrackedSession::new_for_test(child_session);
    let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel(1);
    child.stop_tx = stop_tx;
    manager.active.write().await.insert(retry_child_id, child);

    let mut bus_rx = manager.event_bus.subscribe();
    manager
        .continue_session(session_id, "continue".to_string())
        .await
        .expect_err("continue should proceed to launch and fail");

    stop_rx
        .try_recv()
        .expect("continue must signal the live retry child to stop");
    assert!(
        manager
            .active
            .read()
            .await
            .get(&retry_child_id)
            .expect("retry child remains tracked until monitor finalizes")
            .interrupt_requested,
        "continue must mark the retry child as intentionally interrupted"
    );
    let mut saw_system_message = false;
    while let Ok(ev) = bus_rx.try_recv() {
        if let crate::bus::DaemonEvent::SystemMessage { message, .. } = ev.as_ref()
            && message.contains(&retry_child_id.to_string())
            && message.contains(&session_id.to_string())
        {
            saw_system_message = true;
        }
    }
    manager.event_bus.unsubscribe();
    assert!(
        saw_system_message,
        "continue must publish a visible message naming the interrupted retry child"
    );
}

#[tokio::test]
async fn cancel_retry_exhausts_retry_budget_in_store() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.status = SessionStatus::Failed;
    session.retry_attempt = Some(0);
    session.max_retries = Some(2);
    insert_row(&manager, &session).await;

    let (cancel_tx, _cancel_rx) = tokio::sync::oneshot::channel();
    let mut completed = CompletedSession::for_test(session);
    completed.retry_cancel = Some(cancel_tx);
    manager
        .completed
        .write()
        .await
        .insert(session_id, completed);

    assert!(
        manager
            .cancel_retry(session_id)
            .await
            .expect("cancel retry"),
        "pending retry should be cancelled"
    );
    let row = wait_for_retry_state(&manager, session_id, Some(2), Some(2)).await;
    assert_eq!(row.retry_attempt, Some(2));
    assert_eq!(row.max_retries, Some(2));
}

#[tokio::test]
async fn interrupt_completed_retry_exhausts_retry_budget_in_store() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.status = SessionStatus::Failed;
    session.retry_attempt = Some(0);
    session.max_retries = Some(2);
    insert_row(&manager, &session).await;

    let (cancel_tx, _cancel_rx) = tokio::sync::oneshot::channel();
    let mut completed = CompletedSession::for_test(session);
    completed.retry_cancel = Some(cancel_tx);
    manager
        .completed
        .write()
        .await
        .insert(session_id, completed);

    manager
        .interrupt_session(session_id)
        .await
        .expect("interrupt completed retry");
    let row = wait_for_retry_state(&manager, session_id, Some(2), Some(2)).await;
    assert_eq!(row.retry_attempt, Some(2));
    assert_eq!(row.max_retries, Some(2));
}

#[tokio::test]
async fn launch_retry_revalidates_durable_retry_budget() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.status = SessionStatus::Failed;
    session.retry_attempt = Some(2);
    session.max_retries = Some(2);
    insert_row(&manager, &session).await;

    let mut completed = CompletedSession::for_test(session);
    completed.retry_fired_at = Some(std::time::Instant::now());
    manager
        .completed
        .write()
        .await
        .insert(session_id, completed);

    manager
        .launch_retry(session_id)
        .await
        .expect("exhausted durable row should skip retry");

    assert!(
        manager.active.read().await.is_empty(),
        "durably exhausted retry must not launch a child"
    );
    let restored = manager
        .completed
        .read()
        .await
        .get(&session_id)
        .expect("original session reinserted")
        .session
        .clone();
    assert_eq!(restored.retry_attempt, Some(2));
    assert_eq!(restored.max_retries, Some(2));
}

#[tokio::test]
async fn stale_finalize_generation_does_not_clobber_active_session() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.status = SessionStatus::Running;
    insert_row(&manager, &session).await;

    let mut tracked = TrackedSession::new_for_test(session);
    tracked.spawn_generation = 2;
    manager.active.write().await.insert(session_id, tracked);

    SessionManager::finalize_session(
        session_id,
        1,
        TerminalFinalizeDecision::completed(),
        manager.active.clone(),
        manager.completed.clone(),
        manager.event_bus.clone(),
        manager.store.clone(),
        manager.persistence.clone(),
        None,
        manager.runtime_config.clone(),
    )
    .await;

    assert!(
        manager.active.read().await.contains_key(&session_id),
        "stale finalizer must leave newer active incarnation untouched"
    );
    assert!(
        !manager.completed.read().await.contains_key(&session_id),
        "stale finalizer must not insert completed state"
    );
    assert_eq!(
        manager
            .store
            .lock()
            .await
            .get_session(session_id)
            .expect("load session")
            .expect("session row")
            .status,
        SessionStatus::Running
    );

    SessionManager::finalize_session(
        session_id,
        2,
        TerminalFinalizeDecision::failed(
            crate::store::daemon_settings::AutofileCause::NoMeaningfulOutput,
        ),
        manager.active.clone(),
        manager.completed.clone(),
        manager.event_bus.clone(),
        manager.store.clone(),
        manager.persistence.clone(),
        None,
        manager.runtime_config.clone(),
    )
    .await;

    assert!(!manager.active.read().await.contains_key(&session_id));
    assert_eq!(
        manager
            .completed
            .read()
            .await
            .get(&session_id)
            .expect("matching generation finalizes")
            .session
            .status,
        SessionStatus::Failed
    );
}

#[tokio::test]
async fn newer_lifecycle_intent_replaces_stale_handoff_failure_reason() {
    for intent in ["interrupt", "stall"] {
        let (manager, _dir) = manager();
        let session_id = Uuid::new_v4();
        let mut session = bare_session(session_id);
        session.status = SessionStatus::Running;
        insert_row(&manager, &session).await;

        let mut tracked = TrackedSession::new_for_test(session);
        tracked.session.stop_reason = Some("terminal_handoff_superseded_by_tool".to_string());
        match intent {
            "interrupt" => tracked.interrupt_requested = true,
            "stall" => tracked.stall_interrupted = true,
            _ => unreachable!("fixed intent matrix"),
        }
        manager.active.write().await.insert(session_id, tracked);

        let finalized = SessionManager::finalize_session(
            session_id,
            0,
            TerminalFinalizeDecision {
                status: SessionStatus::Failed,
                c5_failure_cause: None,
            },
            manager.active.clone(),
            manager.completed.clone(),
            manager.event_bus.clone(),
            manager.store.clone(),
            manager.persistence.clone(),
            None,
            manager.runtime_config.clone(),
        )
        .await
        .expect("durable finalization");
        let completed = manager.completed.read().await;
        let row = &completed.get(&session_id).expect("completed row").session;
        match intent {
            "interrupt" => {
                assert_eq!(finalized.status, SessionStatus::Interrupted);
                assert_eq!(row.stop_reason, None);
            }
            "stall" => {
                assert_eq!(finalized.status, SessionStatus::Failed);
                assert_eq!(
                    row.stop_reason.as_deref(),
                    Some("terminal_failure:stall-timeout")
                );
            }
            _ => unreachable!("fixed intent matrix"),
        }
    }
}

#[tokio::test]
async fn terminal_status_event_is_published_after_completed_map_is_ready() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.status = SessionStatus::Running;
    insert_row(&manager, &session).await;
    manager
        .active
        .write()
        .await
        .insert(session_id, TrackedSession::new_for_test(session));
    let mut events = manager.event_bus.subscribe();

    let finalizer = tokio::spawn(SessionManager::finalize_session(
        session_id,
        0,
        TerminalFinalizeDecision::interrupted(),
        manager.active.clone(),
        manager.completed.clone(),
        manager.event_bus.clone(),
        manager.store.clone(),
        manager.persistence.clone(),
        None,
        manager.runtime_config.clone(),
    ));

    loop {
        let event = events.recv().await.expect("terminal status event");
        if matches!(
            event.as_ref(),
            DaemonEvent::SessionStatusChanged {
                session_id: changed,
                new_status: SessionStatus::Interrupted,
                ..
            } if *changed == session_id
        ) {
            break;
        }
    }

    assert!(
        manager.completed.read().await.contains_key(&session_id),
        "a terminal event must make the session resumable before consumers wake"
    );
    finalizer
        .await
        .expect("finalizer task")
        .expect("durable finalization");
}

/// B-002: archive, stall, and interrupt writers can queue after the monitor
/// captured settled evidence but before it obtains the final removal lock.
/// Tokio's fair `RwLock` grants the already-queued writer first; the finalizer
/// must then recompute precedence from that durable in-memory intent rather
/// than persist its stale Completed candidate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_lifecycle_intent_wins_at_atomic_finalization_boundary() {
    use crate::store::daemon_settings::AutofileCause;
    use std::sync::Arc;

    for intent in ["archive", "stall", "interrupt"] {
        let (manager, _dir) = manager();
        let session_id = Uuid::new_v4();
        let mut session = bare_session(session_id);
        session.status = SessionStatus::Running;
        insert_row(&manager, &session).await;
        let mut tracked = TrackedSession::new_for_test(session);
        tracked.spawn_generation = 7;
        manager.active.write().await.insert(session_id, tracked);

        let held = manager.active.write().await;
        let queued = Arc::new(tokio::sync::Notify::new());
        let queued_wait = queued.notified();
        let active_for_writer = Arc::clone(&manager.active);
        let queued_for_writer = Arc::clone(&queued);
        let writer = tokio::spawn(async move {
            queued_for_writer.notify_one();
            let mut active = active_for_writer.write().await;
            let tracked = active.get_mut(&session_id).expect("matching active row");
            match intent {
                "archive" => tracked.pending_archive = true,
                "stall" => tracked.stall_interrupted = true,
                "interrupt" => tracked.interrupt_requested = true,
                _ => unreachable!("fixed intent matrix"),
            }
        });
        queued_wait.await;

        let finalizer = tokio::spawn(SessionManager::finalize_session(
            session_id,
            7,
            TerminalFinalizeDecision::completed(),
            Arc::clone(&manager.active),
            Arc::clone(&manager.completed),
            Arc::clone(&manager.event_bus),
            Arc::clone(&manager.store),
            manager.persistence.clone(),
            None,
            Arc::clone(&manager.runtime_config),
        ));
        drop(held);
        writer.await.expect("queued lifecycle writer");
        let decision = finalizer
            .await
            .expect("finalizer task")
            .expect("durable finalization");

        let expected = match intent {
            "archive" => TerminalFinalizeDecision::completed(),
            "stall" => TerminalFinalizeDecision::failed(AutofileCause::StallTimeout),
            "interrupt" => TerminalFinalizeDecision::interrupted(),
            _ => unreachable!("fixed intent matrix"),
        };
        assert_eq!(decision, expected, "queued {intent} intent wins");
        assert!(
            !manager.active.read().await.contains_key(&session_id),
            "the winning decision and removal share the same lock boundary"
        );
    }
}

struct PersistedWatchLauncher {
    store: std::sync::Arc<tokio::sync::Mutex<crate::store::Store>>,
    watched: Uuid,
    wake_target: Uuid,
    deliveries: std::sync::atomic::AtomicUsize,
    delivery_notify: std::sync::Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl crate::issue_tracker::poller::SessionLauncher for PersistedWatchLauncher {
    async fn launch(&self, _config: crate::claude::LaunchConfig) -> crate::error::Result<Uuid> {
        Err(crate::error::DaemonError::Rpc(
            "A10 watch fixture only permits persisted terminal delivery".to_string(),
        ))
    }

    async fn fire_watch(
        &self,
        job: &rsi_common::types::ScheduledJob,
    ) -> crate::error::Result<crate::issue_tracker::poller::WatchFireOutcome> {
        let store = self.store.lock().await;
        let watched = store
            .get_session(self.watched)?
            .ok_or(crate::error::DaemonError::SessionNotFound(self.watched))?;
        if store.get_session(self.wake_target)?.is_none() {
            return Ok(crate::issue_tracker::poller::WatchFireOutcome::Abandon {
                reason: "missing persisted A10 wake target".to_string(),
            });
        }
        if !watched.status.is_terminal() {
            return Ok(crate::issue_tracker::poller::WatchFireOutcome::NotReady);
        }
        drop(store);
        self.deliveries
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.delivery_notify.notify_one();
        Ok(crate::issue_tracker::poller::WatchFireOutcome::Delivered {
            session_id: self.wake_target,
            delivered_job_ids: vec![job.id],
            observed_job_versions: vec![(job.id, job.updated_at)],
        })
    }
}

struct ScriptedMonitorFixture {
    manager: SessionManager,
    _dir: TempDir,
    session_id: Uuid,
    token: String,
    provider_tx: Option<mpsc::Sender<StreamEvent>>,
    process: ScriptedProcessControls,
    monitor: tokio::task::JoinHandle<()>,
    retry_rx: mpsc::Receiver<Uuid>,
    watch_job: rsi_common::types::ScheduledJob,
    wake_target: Uuid,
    watch_launcher: std::sync::Arc<PersistedWatchLauncher>,
    scheduler: crate::scheduler::SchedulerHandle,
    watch_task: tokio::task::JoinHandle<()>,
    bus_rx: tokio::sync::broadcast::Receiver<std::sync::Arc<crate::bus::DaemonEvent>>,
}

struct RecordingMultiTurnProvider {
    event_rx: mpsc::Receiver<StreamEvent>,
    started_turns: std::sync::Arc<tokio::sync::Mutex<Vec<crate::provider::TurnConfig>>>,
    turn_started: std::sync::Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl crate::provider::ProviderSession for RecordingMultiTurnProvider {
    async fn next_event(&mut self) -> Option<StreamEvent> {
        self.event_rx.recv().await
    }

    async fn start_turn(
        &mut self,
        config: &crate::provider::TurnConfig,
    ) -> crate::error::Result<crate::provider::TurnId> {
        self.started_turns.lock().await.push(config.clone());
        self.turn_started.notify_one();
        Ok(crate::provider::TurnId("memory-flush-turn".to_string()))
    }

    fn supports_multi_turn(&self) -> bool {
        true
    }
}

/// M-04 regression: drive the real monitor result boundary, not the predicate
/// in isolation. Removing the production call to `maybe_start_memory_flush_turn`
/// makes this test time out before the provider records a turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_flush_live_monitor_dispatches_once_from_canonical_budget() {
    use std::sync::atomic::Ordering;

    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let generation = 7;
    let budget = rsi_common::ResolvedContextBudget::new(
        100_000,
        rsi_common::ContextCapacity::default(),
        rsi_common::CapabilityEvidence {
            source: rsi_common::CapabilitySource::RuntimeTelemetry,
            source_version: None,
            source_digest: None,
            observed_at: Some(chrono::Utc::now()),
            confidence: rsi_common::CapabilityConfidence::Authoritative,
        },
    )
    .expect("valid canonical context budget");
    let mut session = bare_session(session_id);
    session.status = SessionStatus::Starting;
    session.provider = SessionProvider::CodexAppServer;
    session.model = Some("gpt-6-astra".to_string());
    session.context_window = Some(budget.active_tokens);
    session.resolved_context_budget = Some(budget);
    session.rotation_depth = 2;
    insert_row(&manager, &session).await;

    let (stop_tx, stop_rx) = mpsc::channel(4);
    let (mut tracked, process) =
        scripted_process_tracked(session_id, generation, true, 0, true, false, false);
    tracked.session = session;
    tracked.stop_tx = stop_tx;
    // 100k active - 20k reserve - 4k soft threshold = 76k. The monitor's
    // canonical current-window numerator therefore makes this flush eligible.
    tracked.codex_context_tokens = 80_000;
    tracked.live_usage_confidence = ContextUsageConfidence::Full;
    // A prior depth must not suppress the current depth, while the successful
    // dispatch below must suppress the flush result from recursively flushing.
    tracked.memory_flush_compaction_count = Some(1);
    manager.active.write().await.insert(session_id, tracked);

    let (event_tx, event_rx) = mpsc::channel(4);
    event_tx
        .send(StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({"subtype": "turn_completed"}),
        })
        .await
        .expect("queue successful primary result");
    let started_turns = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let turn_started = std::sync::Arc::new(tokio::sync::Notify::new());
    let provider = RecordingMultiTurnProvider {
        event_rx,
        started_turns: std::sync::Arc::clone(&started_turns),
        turn_started: std::sync::Arc::clone(&turn_started),
    };
    let (memory_tx, _memory_rx) = mpsc::channel(8);

    let monitor = tokio::spawn(SessionManager::monitor_session(
        session_id,
        generation,
        Box::new(provider),
        std::sync::Arc::clone(&manager.active),
        std::sync::Arc::clone(&manager.completed),
        std::sync::Arc::clone(&manager.event_bus),
        stop_rx,
        std::sync::Arc::clone(&manager.store),
        manager
            .model_call_settlements
            .handle()
            .expect("model settlement handle"),
        manager.persistence.clone(),
        0,
        false,
        manager.socket_path.clone(),
        std::sync::Arc::clone(&manager.token_counter),
        Some(MemoryHandle::new(memory_tx)),
        manager.retry_tx.clone(),
        std::sync::Arc::clone(&manager.tool_registry),
        crate::turn_controller::TurnController::new(
            crate::turn_controller::ContinuationPolicy::Single,
        ),
        std::sync::Arc::clone(&manager.runtime_config),
        std::sync::Arc::clone(&manager.spawn_coordinator),
        std::sync::Arc::clone(&manager.agent_tokens),
        std::sync::Arc::clone(&manager.spawn_epoch),
        std::sync::Arc::clone(&manager.agent_message_arbiter),
        manager.codegraph_handle.clone(),
        manager.custody_execution_runtime(),
    ));

    tokio::time::timeout(std::time::Duration::from_secs(2), turn_started.notified())
        .await
        .expect("live monitor should dispatch a memory-flush provider turn");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let recorded = manager
                .active
                .read()
                .await
                .get(&session_id)
                .is_some_and(|tracked| tracked.memory_flush_compaction_count == Some(2));
            if recorded {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("successful dispatch should record the current compaction count");

    {
        let turns = started_turns.lock().await;
        assert_eq!(turns.len(), 1);
        assert!(turns[0].input.contains("Pre-compaction memory flush turn"));
        assert!(turns[0].input.contains("Store durable memories now"));
        assert_eq!(
            turns[0].working_dir.as_deref(),
            Some(std::path::Path::new("/tmp"))
        );
    }
    assert_eq!(
        manager
            .active
            .read()
            .await
            .get(&session_id)
            .map(|tracked| tracked.session.status),
        Some(SessionStatus::Running),
        "the accepted flush turn keeps the session live"
    );

    event_tx
        .send(StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({"subtype": "turn_completed"}),
        })
        .await
        .expect("queue successful flush result");
    drop(event_tx);
    tokio::time::timeout(std::time::Duration::from_secs(5), monitor)
        .await
        .expect("monitor should settle after the flush result")
        .expect("monitor task should not panic");

    assert_eq!(
        started_turns.lock().await.len(),
        1,
        "the flush result must not recursively dispatch at the same depth"
    );
    assert!(
        process.interrupt_count.load(Ordering::SeqCst) >= 1,
        "terminal multi-turn settlement interrupts the scripted provider"
    );
    assert_eq!(
        manager
            .completed
            .read()
            .await
            .get(&session_id)
            .map(|completed| completed.session.status),
        Some(SessionStatus::Completed),
        "the original single-turn policy resumes after the flush"
    );
}

async fn start_scripted_monitor(
    prior_assistant: bool,
    queued_events: Vec<StreamEvent>,
    process_alive: bool,
    exit_code: i32,
) -> ScriptedMonitorFixture {
    start_scripted_monitor_for_provider(
        SessionProvider::Harness,
        prior_assistant,
        queued_events,
        process_alive,
        exit_code,
    )
    .await
}

/// `start_scripted_monitor` with the session's provider chosen by the caller.
///
/// The stream loop reads `tracked.session.provider` when it persists provider
/// telemetry (the rate-limit snapshot is keyed by it), so a test that asserts
/// on that row has to be able to say which provider is reporting.
async fn start_scripted_monitor_for_provider(
    provider: SessionProvider,
    prior_assistant: bool,
    queued_events: Vec<StreamEvent>,
    process_alive: bool,
    exit_code: i32,
) -> ScriptedMonitorFixture {
    use std::sync::atomic::Ordering;

    let (mut manager, dir) = manager();
    manager
        .runtime_config
        .retry_enabled
        .store(true, Ordering::Relaxed);
    manager
        .runtime_config
        .retry_max_backoff_ms
        .store(60_000, Ordering::Relaxed);
    let retry_rx = manager.take_retry_rx().expect("retry receiver");
    let session_id = Uuid::new_v4();
    let generation = 7;
    let mut session = bare_session(session_id);
    session.status = SessionStatus::Starting;
    session.session_kind = SessionKind::Bug;
    session.provider = provider;
    session.retry_attempt = Some(0);
    session.max_retries = Some(1);
    insert_row(&manager, &session).await;

    let mut history = Vec::new();
    if prior_assistant {
        let mut event = ConversationEvent {
            id: 0,
            session_id,
            sequence: 1,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            content: "historical useful assistant output".to_string(),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        event.id = manager
            .store
            .lock()
            .await
            .insert_event(&event)
            .expect("insert historical event");
        history.push(event);
    }

    let (stop_tx, stop_rx) = mpsc::channel(4);
    let (mut tracked, process) = scripted_process_tracked(
        session_id,
        generation,
        process_alive,
        exit_code,
        false,
        false,
        false,
    );
    tracked.session = session;
    tracked.events = history.clone();
    tracked.stop_tx = stop_tx;
    manager.active.write().await.insert(session_id, tracked);

    let token = format!("a10-token-{session_id}");
    manager
        .register_agent_token(token.clone(), session_id)
        .await;

    let wake_target = Uuid::new_v4();
    let mut wake_session = bare_session(wake_target);
    wake_session.status = SessionStatus::Completed;
    insert_row(&manager, &wake_session).await;
    let watch_job = mk_watch_job_for(session_id, wake_target, "a10 terminal watch");
    manager
        .store
        .lock()
        .await
        .insert_scheduled_job(&watch_job)
        .expect("insert watch");
    let watch_launcher = std::sync::Arc::new(PersistedWatchLauncher {
        store: std::sync::Arc::clone(&manager.store),
        watched: session_id,
        wake_target,
        deliveries: std::sync::atomic::AtomicUsize::new(0),
        delivery_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
    });
    let scheduler = crate::scheduler::spawn_scheduler(
        std::sync::Arc::clone(&manager.store),
        std::sync::Arc::clone(&manager.event_bus),
        watch_launcher.clone() as std::sync::Arc<dyn crate::issue_tracker::poller::SessionLauncher>,
        3600,
    );
    let watch_task = crate::watch_service::spawn_terminal_watch_service(
        std::sync::Arc::clone(&manager.event_bus),
        std::sync::Arc::clone(&manager.store),
        scheduler.clone(),
    );
    let bus_rx = manager.event_bus.subscribe();
    while manager.event_bus.subscriber_count() < 2 {
        tokio::task::yield_now().await;
    }

    let (provider_tx, provider_rx) = mpsc::channel(16);
    for event in queued_events {
        provider_tx.send(event).await.expect("queue provider event");
    }

    let monitor = tokio::spawn(SessionManager::monitor_session(
        session_id,
        generation,
        Box::new(crate::provider::CliProviderSession::new(provider_rx)),
        std::sync::Arc::clone(&manager.active),
        std::sync::Arc::clone(&manager.completed),
        std::sync::Arc::clone(&manager.event_bus),
        stop_rx,
        std::sync::Arc::clone(&manager.store),
        manager
            .model_call_settlements
            .handle()
            .expect("model settlement handle"),
        manager.persistence.clone(),
        history.len() as i32,
        false,
        manager.socket_path.clone(),
        std::sync::Arc::clone(&manager.token_counter),
        None,
        manager.retry_tx.clone(),
        std::sync::Arc::clone(&manager.tool_registry),
        crate::turn_controller::TurnController::new(
            crate::turn_controller::ContinuationPolicy::Single,
        ),
        std::sync::Arc::clone(&manager.runtime_config),
        std::sync::Arc::clone(&manager.spawn_coordinator),
        std::sync::Arc::clone(&manager.agent_tokens),
        std::sync::Arc::clone(&manager.spawn_epoch),
        std::sync::Arc::clone(&manager.agent_message_arbiter),
        manager.codegraph_handle.clone(),
        manager.custody_execution_runtime(),
    ));

    ScriptedMonitorFixture {
        manager,
        _dir: dir,
        session_id,
        token,
        provider_tx: Some(provider_tx),
        process,
        monitor,
        retry_rx,
        watch_job,
        wake_target,
        watch_launcher,
        scheduler,
        watch_task,
        bus_rx,
    }
}

async fn wait_for_scripted_result_boundary(
    fixture: &ScriptedMonitorFixture,
    required_content: &str,
) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let result_seen = {
                let active = fixture.manager.active.read().await;
                active.get(&fixture.session_id).is_some_and(|tracked| {
                    tracked.session.stop_reason.as_deref() == Some("success")
                })
            };
            let content_persisted = fixture
                .manager
                .store
                .lock()
                .await
                .load_events(fixture.session_id)
                .expect("load events")
                .iter()
                .any(|event| event.content == required_content);
            if result_seen && content_persisted {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("result boundary reached");
}

fn assert_no_retry_bus_event(
    rx: &mut tokio::sync::broadcast::Receiver<std::sync::Arc<crate::bus::DaemonEvent>>,
    session_id: Uuid,
) {
    while let Ok(event) = rx.try_recv() {
        assert!(
            !matches!(
                event.as_ref(),
                crate::bus::DaemonEvent::SessionRetrying {
                    session_id: retry_id,
                    ..
                } if *retry_id == session_id
            ),
            "successful/live terminal boundary must not publish SessionRetrying"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_result_drain_persists_queued_assistant_before_completion() {
    use std::sync::atomic::Ordering;
    use tokio::sync::mpsc::error::TryRecvError;

    for ordered_output in [false, true] {
        let (events, prior_assistant, required_content) = if ordered_output {
            (
                vec![
                    StreamEvent {
                        event_type: "assistant".to_string(),
                        data: serde_json::json!({
                            "role": "assistant",
                            "content": "queued assistant before result",
                        }),
                    },
                    StreamEvent {
                        event_type: "result".to_string(),
                        data: serde_json::json!({"subtype": "success"}),
                    },
                ],
                false,
                "queued assistant before result",
            )
        } else {
            (
                vec![
                    StreamEvent {
                        event_type: "tool_result".to_string(),
                        data: serde_json::json!({
                            "content": "quiet current tool output",
                            "name": "scripted",
                        }),
                    },
                    StreamEvent {
                        event_type: "system".to_string(),
                        data: serde_json::json!({"content": "quiet current system output"}),
                    },
                    StreamEvent {
                        event_type: "result".to_string(),
                        data: serde_json::json!({"subtype": "success"}),
                    },
                ],
                true,
                "quiet current system output",
            )
        };
        let mut fixture = start_scripted_monitor(prior_assistant, events, true, 0).await;

        wait_for_scripted_result_boundary(&fixture, required_content).await;

        let row = fixture
            .manager
            .store
            .lock()
            .await
            .get_session(fixture.session_id)
            .expect("load row")
            .expect("row");
        assert_eq!(row.status, SessionStatus::Running);
        assert_eq!(row.retry_attempt, Some(0));
        {
            let active = fixture.manager.active.read().await;
            assert_eq!(
                active
                    .get(&fixture.session_id)
                    .expect("live boundary remains active")
                    .spawn_generation,
                7
            );
        }
        assert!(
            !fixture
                .manager
                .completed
                .read()
                .await
                .contains_key(&fixture.session_id)
        );
        assert!(fixture.process.alive.load(Ordering::SeqCst));
        assert!(!fixture.monitor.is_finished());
        assert_eq!(
            fixture.manager.resolve_agent_token(&fixture.token).await,
            Some(fixture.session_id)
        );
        assert_eq!(
            fixture.watch_launcher.deliveries.load(Ordering::SeqCst),
            0,
            "A8 scheduler/fire path cannot deliver before durable terminal persistence"
        );
        assert!(matches!(
            fixture.retry_rx.try_recv(),
            Err(TryRecvError::Empty)
        ));
        assert_no_retry_bus_event(&mut fixture.bus_rx, fixture.session_id);
        assert_eq!(
            fixture
                .manager
                .store
                .lock()
                .await
                .load_sessions()
                .expect("load sessions")
                .len(),
            2,
            "no retry successor exists at the live boundary"
        );

        // Register the deterministic scheduler-delivery barrier before the
        // exact generation is allowed to die. `Notify::notify_one` retains a
        // permit if the fire path wins the race, so this is not a short timing
        // assertion.
        let delivery_wait = fixture.watch_launcher.delivery_notify.notified();
        tokio::pin!(delivery_wait);
        let (removed, release_finalizer) =
            super::lifecycle::pause_finalizer_after_active_removal(fixture.session_id);
        fixture.process.exit_code.store(0, Ordering::SeqCst);
        fixture.process.alive.store(false, Ordering::SeqCst);
        drop(fixture.provider_tx.take());
        tokio::time::timeout(std::time::Duration::from_secs(2), removed)
            .await
            .expect("finalizer removed active ownership")
            .expect("finalizer reached test barrier");
        assert!(
            !fixture
                .manager
                .active
                .read()
                .await
                .contains_key(&fixture.session_id)
        );
        crate::reconciliation::reconcile_store_consistency(
            &fixture.manager.active,
            &fixture.manager.store,
            &fixture.manager.event_bus,
            None,
        )
        .await;
        assert_eq!(
            fixture
                .manager
                .store
                .lock()
                .await
                .get_session(fixture.session_id)
                .expect("load row inside finalizer gap")
                .expect("row inside finalizer gap")
                .status,
            SessionStatus::Running,
            "reconciliation preserves the active durable row while the real finalizer settles"
        );
        release_finalizer.send(()).expect("release finalizer");
        tokio::time::timeout(std::time::Duration::from_secs(2), &mut fixture.monitor)
            .await
            .expect("monitor completes after clean quiescence")
            .expect("monitor task");

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let status = fixture
                    .manager
                    .store
                    .lock()
                    .await
                    .get_session(fixture.session_id)
                    .expect("load row")
                    .expect("row")
                    .status;
                if status == SessionStatus::Completed {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal status persisted");
        crate::reconciliation::reconcile_store_consistency(
            &fixture.manager.active,
            &fixture.manager.store,
            &fixture.manager.event_bus,
            None,
        )
        .await;
        let row = fixture
            .manager
            .store
            .lock()
            .await
            .get_session(fixture.session_id)
            .expect("load row")
            .expect("row");
        assert_eq!(row.status, SessionStatus::Completed);
        assert!(
            !fixture
                .manager
                .active
                .read()
                .await
                .contains_key(&fixture.session_id)
        );
        {
            let completed = fixture.manager.completed.read().await;
            let completed = completed
                .get(&fixture.session_id)
                .expect("completed ownership");
            assert!(completed.retry_cancel.is_none());
            assert!(completed.retry_fired_at.is_none());
        }
        assert!(!fixture.process.alive.load(Ordering::SeqCst));
        assert_eq!(
            fixture.manager.resolve_agent_token(&fixture.token).await,
            Some(fixture.session_id)
        );

        let persisted = fixture
            .manager
            .store
            .lock()
            .await
            .load_events(fixture.session_id)
            .expect("load events");
        assert_eq!(
            persisted
                .iter()
                .filter(|event| event.content == required_content)
                .count(),
            1,
            "queued event persists exactly once"
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), &mut delivery_wait)
            .await
            .expect("terminal watch dispatch observed");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if fixture
                    .manager
                    .store
                    .lock()
                    .await
                    .get_scheduled_job(&fixture.watch_job.id)
                    .expect("load persisted watch")
                    .expect("persisted watch")
                    .last_fired_at
                    .is_some()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("scheduler stamps accepted owner continuation");
        let watch = fixture
            .manager
            .store
            .lock()
            .await
            .get_scheduled_job(&fixture.watch_job.id)
            .expect("load persisted watch")
            .expect("persisted watch");
        assert!(
            watch.enabled,
            "dispatch keeps the persisted watch armed until provider output confirms consumption"
        );
        assert!(
            watch.last_fired_at.is_some(),
            "dispatch stamps the persisted watch for later consumption confirmation"
        );
        assert!(
            watch.next_fire_at > fixture.watch_job.next_fire_at,
            "dispatch defers the armed watch before any redelivery attempt"
        );
        assert_eq!(
            fixture.watch_launcher.deliveries.load(Ordering::SeqCst),
            1,
            "one persisted watch yields exactly one wake/Continue outcome"
        );
        assert!(
            fixture
                .manager
                .store
                .lock()
                .await
                .get_session(fixture.wake_target)
                .expect("load wake target")
                .is_some(),
            "watch delivery used a real persisted wake target"
        );
        assert!(matches!(
            fixture.retry_rx.try_recv(),
            Err(TryRecvError::Empty)
        ));
        assert_no_retry_bus_event(&mut fixture.bus_rx, fixture.session_id);

        let _ = fixture.scheduler.shutdown().await;
        fixture.watch_task.abort();
        fixture.manager.event_bus.unsubscribe();
    }
}

/// S-001: if a provider process is dead but a buggy producer retains its event
/// sender, the monitor does not fabricate normal terminal status. A subsequent
/// operator interrupt is the explicit recovery action: it waits for the exact
/// process settlement, releases the receiver, and persists Interrupted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settled_open_producer_recovers_on_later_interrupt() {
    use std::sync::atomic::Ordering;

    let events = vec![
        StreamEvent {
            event_type: "assistant".to_string(),
            data: serde_json::json!({
                "role": "assistant",
                "content": "result before leaked producer",
            }),
        },
        StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({"subtype": "success"}),
        },
    ];
    let mut fixture = start_scripted_monitor(false, events, true, 0).await;
    wait_for_scripted_result_boundary(&fixture, "result before leaked producer").await;

    // Leave `provider_tx` open deliberately: this is the leaked-producer
    // condition. The exact provider handle is nevertheless observed dead.
    fixture.process.exit_code.store(0, Ordering::SeqCst);
    fixture.process.alive.store(false, Ordering::SeqCst);
    fixture
        .manager
        .interrupt_session(fixture.session_id)
        .await
        .expect("later interrupt is accepted for recoverable active ownership");
    tokio::time::timeout(std::time::Duration::from_secs(2), &mut fixture.monitor)
        .await
        .expect("later interrupt advances leaked producer recovery")
        .expect("monitor task");

    let row = fixture
        .manager
        .store
        .lock()
        .await
        .get_session(fixture.session_id)
        .expect("load row")
        .expect("row");
    assert_eq!(row.status, SessionStatus::Interrupted);
    assert!(
        !fixture
            .manager
            .active
            .read()
            .await
            .contains_key(&fixture.session_id),
        "the explicit recovery cannot leave an indefinitely active Running row"
    );
    assert!(
        fixture.provider_tx.is_some(),
        "producer remained open until receiver cleanup"
    );

    drop(fixture.provider_tx.take());
    let _ = fixture.scheduler.shutdown().await;
    fixture.watch_task.abort();
    fixture.manager.event_bus.unsubscribe();
}

/// B1 diagnostic capture.
///
/// The monitor emits its unrecognized-type warning from its own tokio task, so
/// a thread-local `with_default` subscriber cannot observe it. A process-wide
/// capturing subscriber is installed once instead, filtered to WARN so the rest
/// of the suite keeps short-circuiting its `debug!`/`trace!` macros, and every
/// captured record carries the emitting session id so concurrent tests cannot
/// contaminate one another's assertions.
struct UnrecognizedEventCaptureLayer {
    records: UnrecognizedEventLog,
}

/// Shared `(session id, offending event type)` log the capturing layer fills.
type UnrecognizedEventLog = std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>;

#[derive(Default)]
struct UnrecognizedEventFields {
    message: Option<String>,
    session_id: Option<String>,
    event_type: Option<String>,
}

impl tracing::field::Visit for UnrecognizedEventFields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "session_id" => self.session_id = Some(value.to_string()),
            "event_type" => self.event_type = Some(value.to_string()),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        match field.name() {
            "message" => self.message = Some(rendered),
            "session_id" => self.session_id = Some(rendered),
            "event_type" => self.event_type = Some(rendered),
            _ => {}
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for UnrecognizedEventCaptureLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = UnrecognizedEventFields::default();
        event.record(&mut fields);
        let Some(message) = fields.message else {
            return;
        };
        if !message.starts_with("Provider stream: unrecognized event type") {
            return;
        }
        let (Some(session_id), Some(event_type)) = (fields.session_id, fields.event_type) else {
            return;
        };
        self.records.lock().unwrap().push((session_id, event_type));
    }
}

/// Install the capturing subscriber (once) and hand back the shared record log.
fn unrecognized_event_records() -> UnrecognizedEventLog {
    static RECORDS: std::sync::OnceLock<UnrecognizedEventLog> = std::sync::OnceLock::new();
    RECORDS
        .get_or_init(|| {
            use tracing_subscriber::Layer;
            use tracing_subscriber::layer::SubscriberExt;

            let records = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let layer = UnrecognizedEventCaptureLayer {
                records: std::sync::Arc::clone(&records),
            }
            .with_filter(tracing_subscriber::filter::LevelFilter::WARN);
            // `set_global_default` is fallible only because something else may
            // already own the global slot; nothing in this crate installs one.
            let _ =
                tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer));
            records
        })
        .clone()
}

/// Diagnostics this session emitted, in order, named by the offending type.
fn diagnosed_event_types(records: &UnrecognizedEventLog, session_id: Uuid) -> Vec<String> {
    let wanted = session_id.to_string();
    records
        .lock()
        .unwrap()
        .iter()
        .filter(|(sid, _)| *sid == wanted)
        .map(|(_, event_type)| event_type.clone())
        .collect()
}

/// B1: an event type the converter does not map is now diagnosed exactly once,
/// by name, and still yields no conversation event.
///
/// Before this, the Claude stream path returned an empty vec on its catch-all
/// arm with no log and no metric, so `rate_limit_event` and `system/api_retry`
/// were invisible until a manual CLI probe found them. The Codex path warns;
/// this pins the same signal for every other provider, including the "second
/// occurrence stays quiet" property that keeps a chatty type off the log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unrecognized_stream_event_is_diagnosed_once_and_yields_no_conversation_event() {
    let records = unrecognized_event_records();

    let events = vec![
        StreamEvent {
            event_type: "api_retry".to_string(),
            data: serde_json::json!({
                "attempt": 1,
                "max_retries": 5,
                "retry_delay_ms": 1000,
                "error_status": 429,
                "error": "rate_limit",
            }),
        },
        // The SAME unknown type again: one diagnostic total, not two.
        StreamEvent {
            event_type: "api_retry".to_string(),
            data: serde_json::json!({
                "attempt": 2,
                "max_retries": 5,
                "retry_delay_ms": 2000,
                "error_status": 429,
                "error": "rate_limit",
            }),
        },
        StreamEvent {
            event_type: "assistant".to_string(),
            data: serde_json::json!({
                "role": "assistant",
                "content": "assistant text after the unrecognized events",
            }),
        },
        StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({"subtype": "success"}),
        },
    ];

    let mut fixture =
        start_scripted_monitor_for_provider(SessionProvider::Claude, false, events, true, 0).await;
    wait_for_scripted_result_boundary(&fixture, "assistant text after the unrecognized events")
        .await;
    drop(fixture.provider_tx.take());
    tokio::time::timeout(std::time::Duration::from_secs(5), &mut fixture.monitor)
        .await
        .expect("monitor drains the scripted stream")
        .expect("monitor task");

    assert_eq!(
        diagnosed_event_types(&records, fixture.session_id),
        vec!["api_retry".to_string()],
        "exactly one diagnostic, naming the offending type, for two occurrences"
    );

    // The unrecognized events carry no conversation content, so the persisted
    // transcript is exactly the one assistant message that followed them: the
    // loop kept running rather than converting or dropping real output.
    let persisted = fixture
        .manager
        .store
        .lock()
        .await
        .load_events(fixture.session_id)
        .expect("load events");
    let transcript: Vec<(EventType, String)> = persisted
        .iter()
        .map(|event| (event.event_type, event.content.clone()))
        .collect();
    assert_eq!(
        transcript,
        vec![(
            EventType::Message,
            "assistant text after the unrecognized events".to_string()
        )]
    );

    let _ = fixture.scheduler.shutdown().await;
    fixture.watch_task.abort();
    fixture.manager.event_bus.unsubscribe();
}

/// B2: `system/init` capture and `rate_limit_event` persistence were covered at
/// the parser level and at the store level, with nothing driving an event
/// through the monitor loop to prove the two are actually connected. This is
/// that wiring test: a real stream, the real loop, and the real rows.
///
/// Both payloads are the verbatim shapes observed from `claude 2.1.259`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_handshake_and_rate_limit_event_persist_through_the_monitor_loop() {
    let events = vec![
        StreamEvent {
            event_type: "system".to_string(),
            data: serde_json::json!({
                "subtype": "init",
                "model": "claude-opus-5[1m]",
                "session_id": "9d1c1f6a-0c1b-4a53-9d8f-1c2f1a3b4c5d",
                "claude_code_version": "2.1.259",
                "capabilities": [
                    "interrupt_receipt_v1",
                    "interrupt_cancel_queued_v1",
                    "msg_lifecycle_v1"
                ],
            }),
        },
        StreamEvent {
            event_type: "rate_limit_event".to_string(),
            data: serde_json::json!({
                "session_id": "9d1c1f6a-0c1b-4a53-9d8f-1c2f1a3b4c5d",
                "uuid": "1b2c3d4e-5f60-4712-8394-a5b6c7d8e9f0",
                "rate_limit_info": {
                    "status": "allowed",
                    "resetsAt": 1_788_402_000_i64,
                    "rateLimitType": "five_hour",
                    "overageStatus": "rejected",
                    "isUsingOverage": false,
                    "unifiedWindows": {
                        "five_hour": {"utilization": 0.27, "resetsAt": 1_788_402_000_i64},
                        "seven_day": {"utilization": 0.05, "resetsAt": 1_788_883_200_i64}
                    }
                }
            }),
        },
        StreamEvent {
            event_type: "assistant".to_string(),
            data: serde_json::json!({
                "role": "assistant",
                "content": "assistant text after the telemetry events",
            }),
        },
        StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({"subtype": "success"}),
        },
    ];

    let mut fixture =
        start_scripted_monitor_for_provider(SessionProvider::Claude, false, events, true, 0).await;
    wait_for_scripted_result_boundary(&fixture, "assistant text after the telemetry events").await;
    drop(fixture.provider_tx.take());
    tokio::time::timeout(std::time::Duration::from_secs(5), &mut fixture.monitor)
        .await
        .expect("monitor drains the scripted stream")
        .expect("monitor task");

    let store = fixture.manager.store.lock().await;
    let row = store
        .get_session(fixture.session_id)
        .expect("load row")
        .expect("row");
    assert_eq!(
        row.provider_cli_version.as_deref(),
        Some("2.1.259"),
        "the init handshake's advertised CLI version reaches the session row"
    );
    assert_eq!(
        row.provider_capabilities,
        vec![
            "interrupt_receipt_v1".to_string(),
            "interrupt_cancel_queued_v1".to_string(),
            "msg_lifecycle_v1".to_string(),
        ],
        "every advertised capability token is recorded, in order"
    );

    let snapshots = store
        .load_provider_rate_limit_snapshots()
        .expect("load rate-limit snapshots");
    let claude = snapshots
        .iter()
        .find(|snapshot| snapshot.provider == SessionProvider::Claude)
        .expect("the reporting session's provider owns the snapshot");
    assert_eq!(claude.status.as_deref(), Some("allowed"));
    assert_eq!(claude.rate_limit_type.as_deref(), Some("five_hour"));
    let mut windows: Vec<(String, f64)> = claude
        .windows
        .iter()
        .map(|window| (window.window_key.clone(), window.utilization))
        .collect();
    windows.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        windows,
        vec![
            ("five_hour".to_string(), 0.27),
            ("seven_day".to_string(), 0.05),
        ],
        "every reported plan window lands, not just the first"
    );
    drop(store);

    let _ = fixture.scheduler.shutdown().await;
    fixture.watch_task.abort();
    fixture.manager.event_bus.unsubscribe();
}

async fn assert_failed_scripted_monitor(
    exit_code: i32,
    assistant_content: Option<&str>,
    expected_cause: crate::store::daemon_settings::AutofileCause,
) {
    use std::sync::atomic::Ordering;

    let events = assistant_content
        .map(|content| {
            vec![StreamEvent {
                event_type: "assistant".to_string(),
                data: serde_json::json!({
                    "role": "assistant",
                    "content": content,
                }),
            }]
        })
        .unwrap_or_default();
    let mut fixture = start_scripted_monitor(false, events, false, exit_code).await;
    drop(fixture.provider_tx.take());
    tokio::time::timeout(std::time::Duration::from_secs(2), &mut fixture.monitor)
        .await
        .expect("negative monitor completes")
        .expect("monitor task");

    let row = fixture
        .manager
        .store
        .lock()
        .await
        .get_session(fixture.session_id)
        .expect("load row")
        .expect("row");
    assert_eq!(row.status, SessionStatus::Failed);
    assert!(
        !fixture
            .manager
            .active
            .read()
            .await
            .contains_key(&fixture.session_id)
    );
    {
        let completed = fixture.manager.completed.read().await;
        let completed = completed
            .get(&fixture.session_id)
            .expect("completed failure");
        assert_eq!(completed.session.retry_attempt, Some(1));
        assert!(completed.retry_cancel.is_some());
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let persisted_attempt = fixture
                .manager
                .store
                .lock()
                .await
                .get_session(fixture.session_id)
                .expect("load row")
                .expect("row")
                .retry_attempt;
            if persisted_attempt == Some(1) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("retry attempt persisted");
    assert!(!fixture.process.alive.load(Ordering::SeqCst));
    assert_eq!(
        fixture.manager.resolve_agent_token(&fixture.token).await,
        Some(fixture.session_id)
    );

    let pending = fixture
        .manager
        .store
        .lock()
        .await
        .get_c5_autofile_pending(&crate::store::daemon_settings::c5_autofile_pending_key(
            fixture.session_id,
        ))
        .expect("load C5 marker")
        .expect("retry keeps C5 marker pending");
    assert_eq!(pending.cause, expected_cause);
    let mut retry_events = 0;
    while let Ok(event) = fixture.bus_rx.try_recv() {
        if matches!(
            event.as_ref(),
            crate::bus::DaemonEvent::SessionRetrying {
                session_id,
                ..
            } if *session_id == fixture.session_id
        ) {
            retry_events += 1;
        }
    }
    assert_eq!(retry_events, 1, "legitimate Failed arms A9 exactly once");

    let _ = fixture.scheduler.shutdown().await;
    fixture.watch_task.abort();
    fixture.manager.event_bus.unsubscribe();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_dead_zero_event_remains_failed() {
    assert_failed_scripted_monitor(
        0,
        None,
        crate::store::daemon_settings::AutofileCause::NoMeaningfulOutput,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_nonzero_exit_remains_failed() {
    assert_failed_scripted_monitor(
        23,
        Some("meaningful output before nonzero exit"),
        crate::store::daemon_settings::AutofileCause::NonZeroExit,
    )
    .await;
}

/// B-003: terminal normalized provider errors must override carried useful
/// history, while raw stderr remains a diagnostic and does not manufacture a
/// terminal failure. The table covers Codex CLI, Local, Harness, and the two
/// CodexAppServer `turn/completed` terminal shapes through the production
/// monitor/persistence/drain path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_provider_errors_with_prior_history_fail_after_drain() {
    let terminal_shapes = [
        (
            "codex failed turn",
            serde_json::json!({"error": "quota\n  exceeded", "source": "codex_event", "provider_event_type": "turn.failed"}),
            Some("provider_error:codex:quota exceeded"),
        ),
        (
            "codex usage limit",
            serde_json::json!({
                "error": "You've hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again later.",
                "source": "codex_event",
                "error_class": crate::codex::CODEX_USAGE_LIMIT_ERROR_CLASS,
                "provider_event_type": "turn.failed",
            }),
            Some(crate::codex::CODEX_USAGE_LIMIT_STOP_REASON),
        ),
        (
            "codex remote compaction usage limit",
            serde_json::json!({
                "error": "Error running remote compact task: You've hit your usage limit. Retry later.",
                "source": "codex_event",
                "error_class": crate::codex::CODEX_USAGE_LIMIT_ERROR_CLASS,
                "provider_event_type": "turn.failed",
            }),
            Some(crate::codex::CODEX_USAGE_LIMIT_STOP_REASON),
        ),
        (
            "local normalized error",
            serde_json::json!({"error": "local failed", "terminal": true, "source": "local"}),
            Some("provider_error:unclassified"),
        ),
        (
            "harness normalized error",
            serde_json::json!({"error": "harness failed", "terminal": true, "source": "harness"}),
            Some("provider_error:unclassified"),
        ),
        (
            "app server failed turn",
            serde_json::json!({"error": "turn failed", "error_class": "turn_failed", "provider_event_type": "turn/completed", "turn_status": "failed"}),
            Some("provider_error:unclassified"),
        ),
        (
            "app server interrupted turn",
            serde_json::json!({"error": "turn interrupted", "error_class": "interrupted", "provider_event_type": "turn/completed", "turn_status": "interrupted"}),
            Some("terminal_failure:other-terminal-failure"),
        ),
        (
            "codex storage full",
            serde_json::json!({
                "error": "recorder StorageFull",
                "source": "stderr",
                "terminal": true,
                "error_class": crate::codex::CODEX_STORAGE_FULL_ERROR_CLASS,
                "provider_event_type": crate::codex::CODEX_STORAGE_FULL_PROVIDER_EVENT_TYPE,
            }),
            Some(crate::codex::CODEX_STORAGE_FULL_STOP_REASON),
        ),
    ];

    for (name, data, expected_stop_reason) in terminal_shapes {
        let mut fixture = start_scripted_monitor(
            true,
            vec![StreamEvent {
                event_type: "process_error".to_string(),
                data,
            }],
            false,
            0,
        )
        .await;
        drop(fixture.provider_tx.take());
        tokio::time::timeout(std::time::Duration::from_secs(2), &mut fixture.monitor)
            .await
            .expect("terminal provider error monitor completes")
            .expect("terminal provider error monitor task");
        let row = fixture
            .manager
            .store
            .lock()
            .await
            .get_session(fixture.session_id)
            .expect("load terminal error row")
            .expect("terminal error row");
        assert_eq!(row.status, SessionStatus::Failed, "{name}");
        assert_eq!(
            row.stop_reason.as_deref(),
            expected_stop_reason,
            "{name} persists a daemon-owned stop reason"
        );
        let errors = fixture
            .manager
            .store
            .lock()
            .await
            .load_events(fixture.session_id)
            .expect("load terminal error evidence");
        assert!(
            errors
                .iter()
                .any(|event| event.content.contains("**Process Error (")),
            "{name} persists the terminal error evidence before finalization"
        );
        if name == "codex usage limit" {
            assert!(
                errors.iter().any(|event| event
                    .content
                    .contains("https://chatgpt.com/codex/settings/usage")),
                "usage-limit settlement preserves the complete provider diagnostic"
            );
        }
        let _ = fixture.scheduler.shutdown().await;
        fixture.watch_task.abort();
        fixture.manager.event_bus.unsubscribe();
    }

    let mut diagnostic = start_scripted_monitor(
        true,
        vec![StreamEvent {
            event_type: "process_error".to_string(),
            data: serde_json::json!({"error": "ordinary stderr", "source": "stderr"}),
        }],
        false,
        0,
    )
    .await;
    drop(diagnostic.provider_tx.take());
    tokio::time::timeout(std::time::Duration::from_secs(2), &mut diagnostic.monitor)
        .await
        .expect("stderr diagnostic monitor completes")
        .expect("stderr diagnostic monitor task");
    assert_eq!(
        diagnostic
            .manager
            .store
            .lock()
            .await
            .get_session(diagnostic.session_id)
            .expect("load stderr row")
            .expect("stderr row")
            .status,
        SessionStatus::Completed,
        "raw stderr stays nonterminal and preserved prior history completes cleanly"
    );
    let diagnostic_events = diagnostic
        .manager
        .store
        .lock()
        .await
        .load_events(diagnostic.session_id)
        .expect("load stderr diagnostic evidence");
    assert!(
        diagnostic_events.iter().any(|event| {
            event.content.contains("**Provider diagnostic (stderr)**")
                && event.content.contains("ordinary stderr")
        }),
        "raw stderr uses a non-failure diagnostic heading"
    );
    assert!(
        diagnostic_events
            .iter()
            .all(|event| !event.content.contains("**Process Error (stderr)**")),
        "raw stderr is not presented as a process error"
    );
    let _ = diagnostic.scheduler.shutdown().await;
    diagnostic.watch_task.abort();
    diagnostic.manager.event_bus.unsubscribe();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::expect_used,
    clippy::significant_drop_tightening,
    clippy::too_many_lines
)]
async fn codex_recoverable_error_events_do_not_end_the_turn() {
    use std::sync::atomic::Ordering;

    let mut fixture =
        start_scripted_monitor_for_provider(SessionProvider::Codex, false, vec![], true, 0).await;

    let provider_tx = fixture
        .provider_tx
        .as_ref()
        .expect("scripted provider stream sender");
    let mut thread_id = None;
    let reconnect_notice = crate::codex::map_codex_json_to_stream_event(
        &serde_json::json!({
            "type": "error",
            "message": "Reconnecting... 5/5 (SSE idle timeout)",
        }),
        &mut thread_id,
    )
    .expect("map Codex reconnect notice");
    provider_tx
        .send(reconnect_notice)
        .await
        .expect("send Codex reconnect notice");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let events = fixture
                .manager
                .store
                .lock()
                .await
                .load_events(fixture.session_id)
                .expect("load reconnect notice");
            if events.iter().any(|event| {
                event
                    .content
                    .contains("Reconnecting... 5/5 (SSE idle timeout)")
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("monitor should publish the reconnect notice");
    let interrupted = tokio::time::timeout(std::time::Duration::from_millis(200), async {
        loop {
            if fixture.process.interrupt_count.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        interrupted.is_err(),
        "an intermediate reconnect notice must not interrupt the live provider"
    );

    let warning = crate::codex::map_codex_json_to_stream_event(
        &serde_json::json!({
            "type": "error",
            "message": "Configured service tier `priority` is not advertised and will be omitted from requests.",
        }),
        &mut thread_id,
    )
    .expect("map Codex service-tier warning");
    provider_tx
        .send(warning)
        .await
        .expect("send Codex config warning");
    provider_tx
        .send(StreamEvent {
            event_type: "assistant".to_string(),
            data: serde_json::json!({
                "role": "assistant",
                "content": "Codex completed after reconnecting.",
            }),
        })
        .await
        .expect("send normal assistant output");
    provider_tx
        .send(StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({"subtype": "success"}),
        })
        .await
        .expect("send successful result");

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let events = fixture
                .manager
                .store
                .lock()
                .await
                .load_events(fixture.session_id)
                .expect("load in-flight Codex diagnostics");
            if events.iter().any(|event| {
                event
                    .content
                    .contains("Codex completed after reconnecting.")
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("normal provider events should be processed after both diagnostics");
    assert_eq!(
        fixture.process.interrupt_count.load(Ordering::SeqCst),
        0,
        "recoverable Codex error events must not interrupt the live provider"
    );

    // The scripted provider stays alive through all normal turn events. Let it
    // exit naturally after those events have been processed.
    fixture.process.alive.store(false, Ordering::SeqCst);
    drop(fixture.provider_tx.take());
    tokio::time::timeout(std::time::Duration::from_secs(2), &mut fixture.monitor)
        .await
        .expect("recoverable Codex diagnostics should allow the monitor to finish")
        .expect("monitor task should not panic");

    let row = fixture
        .manager
        .store
        .lock()
        .await
        .get_session(fixture.session_id)
        .expect("load completed Codex row")
        .expect("completed Codex row");
    assert_eq!(row.status, SessionStatus::Completed);
    assert_eq!(row.stop_reason.as_deref(), Some("success"));

    let events = fixture
        .manager
        .store
        .lock()
        .await
        .load_events(fixture.session_id)
        .expect("load Codex diagnostics and completion");
    assert!(events.iter().any(|event| {
        event
            .content
            .contains("**Provider diagnostic (codex_event)**")
            && event
                .content
                .contains("Reconnecting... 5/5 (SSE idle timeout)")
    }));
    assert!(events.iter().any(|event| {
        event
            .content
            .contains("**Provider diagnostic (codex_event)**")
            && event
                .content
                .contains("Configured service tier `priority` is not advertised")
    }));
    assert!(events.iter().any(|event| {
        event
            .content
            .contains("Codex completed after reconnecting.")
    }));

    assert_no_retry_bus_event(&mut fixture.bus_rx, fixture.session_id);
    let _ = fixture.scheduler.shutdown().await;
    fixture.watch_task.abort();
    fixture.manager.event_bus.unsubscribe();
}

/// Observed state of a replayed Codex CLI stream.
struct CodexReplay {
    /// Session row after the warning prelude was processed, before the turn.
    row_after_prelude: Session,
    /// Provider interrupts observed after the prelude, before the turn.
    interrupts_after_prelude: usize,
    /// Settled session row after the provider exited.
    row: Session,
    events: Vec<ConversationEvent>,
}

/// Replay a Codex CLI JSONL stream through the production mapper and monitor.
/// The `prelude` (thread start plus warning items) is delivered first and the
/// live provider state is sampled once every prelude diagnostic is persisted;
/// then `turn` is delivered and the provider exits cleanly.
#[allow(clippy::expect_used, clippy::significant_drop_tightening)]
async fn replay_codex_cli_stream(
    prelude: &[serde_json::Value],
    turn: &[serde_json::Value],
) -> CodexReplay {
    use std::sync::atomic::Ordering;

    let mut fixture =
        start_scripted_monitor_for_provider(SessionProvider::Codex, false, vec![], true, 0).await;
    let provider_tx = fixture
        .provider_tx
        .clone()
        .expect("scripted provider stream sender");
    let mut thread_id = None;
    let mut prelude_diagnostics = 0usize;
    for line in prelude {
        if let Some(event) = crate::codex::map_codex_json_to_stream_event(line, &mut thread_id) {
            if event.event_type == "process_error" {
                prelude_diagnostics += 1;
            }
            provider_tx
                .send(event)
                .await
                .expect("send Codex prelude event");
        }
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let persisted = fixture
                .manager
                .store
                .lock()
                .await
                .load_events(fixture.session_id)
                .expect("load prelude diagnostics")
                .iter()
                .filter(|event| event.content.contains("(codex_event)**"))
                .count();
            if persisted >= prelude_diagnostics {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("monitor persists every prelude diagnostic");
    // Give a (wrongly) terminal diagnostic time to start provider settlement.
    let _ = tokio::time::timeout(std::time::Duration::from_millis(200), async {
        while fixture.process.interrupt_count.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let interrupts_after_prelude = fixture.process.interrupt_count.load(Ordering::SeqCst);
    let row_after_prelude = fixture
        .manager
        .store
        .lock()
        .await
        .get_session(fixture.session_id)
        .expect("load Codex row after prelude")
        .expect("Codex row after prelude");
    for line in turn {
        if let Some(event) = crate::codex::map_codex_json_to_stream_event(line, &mut thread_id) {
            // A monitor that already settled may have dropped its receiver.
            let _ = provider_tx.send(event).await;
        }
    }
    drop(provider_tx);
    fixture.process.alive.store(false, Ordering::SeqCst);
    drop(fixture.provider_tx.take());
    tokio::time::timeout(std::time::Duration::from_secs(5), &mut fixture.monitor)
        .await
        .expect("replayed Codex stream should settle")
        .expect("monitor task should not panic");
    let row = fixture
        .manager
        .store
        .lock()
        .await
        .get_session(fixture.session_id)
        .expect("load replayed Codex row")
        .expect("replayed Codex row");
    let events = fixture
        .manager
        .store
        .lock()
        .await
        .load_events(fixture.session_id)
        .expect("load replayed Codex events");
    let _ = fixture.scheduler.shutdown().await;
    fixture.watch_task.abort();
    fixture.manager.event_bus.unsubscribe();
    CodexReplay {
        row_after_prelude,
        interrupts_after_prelude,
        row,
        events,
    }
}

const CODEX_0_156_CONFIG_WARNING: &str =
    "Codex is ignoring 1 unrecognized configuration setting: `model_verbosity_hint`.";
const CODEX_0_156_METADATA_WARNING: &str = "Model metadata for `z-ai/glm-5.3-flashx` not found. Defaulting to fallback metadata; this can degrade performance and cause issues.";

/// Issue #662: codex-cli 0.156.1 emits warnings as item-level `error` items
/// before `turn.started`. Replays the exact stream shape (warning item, then a
/// successful turn) for both the config and the model-metadata warning; the
/// session must complete and the warning must persist as a diagnostic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_0_156_item_error_warnings_before_turn_started_complete_session() {
    let thread = serde_json::json!({"type": "thread.started", "thread_id": "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"});
    let turn = [
        serde_json::json!({"type": "turn.started"}),
        serde_json::json!({"type": "item.completed", "item": {"id": "item_1", "type": "agent_message", "text": "Codex finished the turn after the warning."}}),
        serde_json::json!({"type": "turn.completed", "usage": {"input_tokens": 1200, "cached_input_tokens": 0, "output_tokens": 40}}),
    ];

    // Baseline: the same successful turn with no warning item.
    let baseline = replay_codex_cli_stream(std::slice::from_ref(&thread), &turn).await;
    assert_eq!(baseline.row.status, SessionStatus::Completed);

    for warning in [CODEX_0_156_CONFIG_WARNING, CODEX_0_156_METADATA_WARNING] {
        let replay = replay_codex_cli_stream(
            &[
                thread.clone(),
                serde_json::json!({"type": "item.completed", "item": {"id": "item_0", "type": "error", "message": warning}}),
            ],
            &turn,
        )
        .await;
        assert_eq!(
            replay.interrupts_after_prelude, 0,
            "a warning item never interrupts the live provider: {warning}"
        );
        assert_eq!(
            replay.row_after_prelude.status,
            SessionStatus::Running,
            "a warning item never settles the turn: {warning}"
        );
        assert_eq!(replay.row.status, SessionStatus::Completed, "{warning}");
        assert_eq!(
            replay.row.stop_reason, baseline.row.stop_reason,
            "a warning item settles exactly like a warning-free turn: {warning}"
        );
        assert!(
            replay.events.iter().any(|event| event
                .content
                .contains("**Provider diagnostic (codex_event)**")
                && event.content.contains(warning)),
            "warning item persists as a provider diagnostic: {warning}"
        );
        assert!(
            replay.events.iter().any(|event| event
                .content
                .contains("Codex finished the turn after the warning.")),
            "assistant output after the warning is processed: {warning}"
        );
    }
}

/// Issue #662: a genuine `turn.failed` after warning items still fails the
/// session, and the stop reason carries the real Codex error text instead of
/// `provider_error:unclassified`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_0_156_turn_failed_after_warning_items_fails_with_codex_text() {
    let replay = replay_codex_cli_stream(
        &[
            serde_json::json!({"type": "thread.started", "thread_id": "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5c"}),
            serde_json::json!({"type": "item.completed", "item": {"id": "item_0", "type": "error", "message": CODEX_0_156_METADATA_WARNING}}),
        ],
        &[
            serde_json::json!({"type": "turn.started"}),
            serde_json::json!({"type": "turn.failed", "error": {"message": "unexpected status 400 Bad Request: model `z-ai/glm-5.3-flashx`\nis not available"}}),
        ],
    )
    .await;
    assert_eq!(replay.row_after_prelude.status, SessionStatus::Running);
    assert_eq!(replay.row.status, SessionStatus::Failed);
    assert_eq!(
        replay.row.stop_reason.as_deref(),
        Some(
            "provider_error:codex:unexpected status 400 Bad Request: model `z-ai/glm-5.3-flashx` is not available"
        )
    );
    assert!(replay.events.iter().any(|event| {
        event
            .content
            .contains("**Provider diagnostic (codex_event)**")
            && event.content.contains(CODEX_0_156_METADATA_WARNING)
    }));
    assert!(replay.events.iter().any(|event| {
        event.content.contains("**Process Error (codex_event)**")
            && event.content.contains("400 Bad Request")
    }));
}

#[test]
fn codex_stop_reason_detail_is_single_line_and_bounded() {
    let long = format!("first\tline\r\n{}", "é".repeat(400));
    let detail = super::monitor::bounded_stop_reason_detail(&long).expect("non-empty detail");
    assert!(detail.starts_with("first line éé"));
    assert!(detail.ends_with('…'));
    assert_eq!(detail.chars().count(), 201);
    assert!(!detail.contains(['\n', '\r', '\t']));
    assert_eq!(super::monitor::bounded_stop_reason_detail(" \n\t "), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::expect_used, clippy::significant_drop_tightening)]
async fn codex_retry_exhaustion_error_waits_for_turn_failed() {
    use std::sync::atomic::Ordering;

    let mut fixture =
        start_scripted_monitor_for_provider(SessionProvider::Codex, false, vec![], true, 0).await;
    let provider_tx = fixture
        .provider_tx
        .as_ref()
        .expect("scripted provider stream sender");
    let mut thread_id = None;

    for value in [
        serde_json::json!({
            "type": "error",
            "message": "Reconnecting... 5/5 (SSE idle timeout)",
        }),
        serde_json::json!({
            "type": "error",
            "message": "stream disconnected before completion",
        }),
    ] {
        let event = crate::codex::map_codex_json_to_stream_event(&value, &mut thread_id)
            .expect("map Codex retry event");
        provider_tx
            .send(event)
            .await
            .expect("send Codex retry event");
    }

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let events = fixture
                .manager
                .store
                .lock()
                .await
                .load_events(fixture.session_id)
                .expect("load retry exhaustion diagnostics");
            if events.iter().any(|event| {
                event
                    .content
                    .contains("stream disconnected before completion")
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("monitor should persist the post-exhaustion error");

    let row = fixture
        .manager
        .store
        .lock()
        .await
        .get_session(fixture.session_id)
        .expect("load session before turn.failed")
        .expect("session before turn.failed");
    assert_eq!(row.status, SessionStatus::Running);
    assert_eq!(fixture.process.interrupt_count.load(Ordering::SeqCst), 0);

    let failed_turn = crate::codex::map_codex_json_to_stream_event(
        &serde_json::json!({
            "type": "turn.failed",
            "error": {"message": "stream disconnected before completion"},
        }),
        &mut thread_id,
    )
    .expect("map Codex failed turn");
    provider_tx
        .send(failed_turn)
        .await
        .expect("send Codex failed turn");
    fixture.process.alive.store(false, Ordering::SeqCst);
    drop(fixture.provider_tx.take());
    tokio::time::timeout(std::time::Duration::from_secs(2), &mut fixture.monitor)
        .await
        .expect("failed Codex turn should settle")
        .expect("monitor task should not panic");

    let row = fixture
        .manager
        .store
        .lock()
        .await
        .get_session(fixture.session_id)
        .expect("load failed Codex row")
        .expect("failed Codex row");
    assert_eq!(row.status, SessionStatus::Failed);

    let _ = fixture.scheduler.shutdown().await;
    fixture.watch_task.abort();
    fixture.manager.event_bus.unsubscribe();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_storage_full_interrupts_live_process_before_stream_eof() {
    use std::sync::atomic::Ordering;

    let mut fixture = start_scripted_monitor(
        false,
        vec![StreamEvent {
            event_type: "process_error".to_string(),
            data: serde_json::json!({
                "error": "recorder StorageFull",
                "source": "stderr",
                "terminal": true,
                "error_class": crate::codex::CODEX_STORAGE_FULL_ERROR_CLASS,
                "provider_event_type": crate::codex::CODEX_STORAGE_FULL_PROVIDER_EVENT_TYPE,
            }),
        }],
        true,
        0,
    )
    .await;

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while fixture.process.interrupt_count.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("terminal stderr interrupts before provider EOF");
    assert_eq!(fixture.process.kill_count.load(Ordering::SeqCst), 0);
    assert!(
        fixture.provider_tx.is_some(),
        "the simulated provider stream is still open at interruption"
    );

    // Model a provider honoring the interrupt, then allow the adapter stream
    // to close. The monitor must not need a natural EOF to initiate teardown.
    fixture.process.alive.store(false, Ordering::SeqCst);
    drop(fixture.provider_tx.take());
    tokio::time::timeout(std::time::Duration::from_secs(2), &mut fixture.monitor)
        .await
        .expect("storage-full monitor completes")
        .expect("storage-full monitor task");

    let row = fixture
        .manager
        .store
        .lock()
        .await
        .get_session(fixture.session_id)
        .expect("load storage-full row")
        .expect("storage-full row");
    assert_eq!(row.status, SessionStatus::Failed);
    assert_eq!(
        row.stop_reason.as_deref(),
        Some(crate::codex::CODEX_STORAGE_FULL_STOP_REASON)
    );
    assert_eq!(row.input_tokens, None);
    assert_eq!(row.output_tokens, None);
    assert_eq!(row.total_input_tokens, None);
    assert_eq!(row.total_output_tokens, None);
    assert_eq!(
        row.context_usage_confidence,
        ContextUsageConfidence::Missing,
        "a recorder failure without a terminal usage event stays explicitly unknown"
    );
    assert_eq!(fixture.process.interrupt_count.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.process.kill_count.load(Ordering::SeqCst), 0);

    let _ = fixture.scheduler.shutdown().await;
    fixture.watch_task.abort();
    fixture.manager.event_bus.unsubscribe();
}

#[tokio::test]
async fn get_session_falls_back_to_store() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let session = bare_session(session_id);
    let store = manager.store().clone();

    tokio::task::spawn_blocking(move || {
        let store = store.blocking_lock();
        store.insert_session(&session)
    })
    .await
    .unwrap()
    .expect("insert session");

    let loaded = manager
        .get_session(session_id)
        .await
        .expect("session should load from store fallback");
    assert_eq!(loaded.id, session_id);
}

#[tokio::test]
async fn get_session_stamps_context_fill_pct_for_idle_completed() {
    // An idle/Completed session that only exists in the store (no live
    // TrackedSession) must still come back with a daemon-computed
    // context_fill_pct so the TUI renders a bar without recomputing.
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.provider = SessionProvider::Claude;
    session.model = Some("claude-opus-4-7".to_string()); // 1M window
    session.total_input_tokens = Some(500_000);
    let store = manager.store().clone();
    let to_insert = session.clone();
    tokio::task::spawn_blocking(move || {
        let store = store.blocking_lock();
        store.insert_session(&to_insert)
    })
    .await
    .unwrap()
    .expect("insert session");

    let loaded = manager.get_session(session_id).await.expect("load");
    assert_eq!(
        loaded.context_fill_pct,
        Some(50.0),
        "idle Completed session must be stamped with the daemon-computed pct"
    );

    // And it appears on the list path too.
    let listed = manager.list_sessions().await;
    let row = listed
        .iter()
        .find(|s| s.id == session_id)
        .expect("session in list");
    assert_eq!(row.context_fill_pct, Some(50.0));
}

#[tokio::test]
async fn rpc_session_reads_rehydrate_version_fenced_context_capacity() {
    crate::provider_capabilities::provider_capabilities()
        .refresh_codex_catalog(
            crate::provider_capabilities::VALIDATED_CODEX_CLI_VERSION,
            include_bytes!("../../tests/fixtures/codex-models-0.155.1.json"),
            chrono::Utc::now(),
        )
        .expect("install exact catalog fixture");

    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.provider = SessionProvider::Codex;
    session.model = Some("gpt-6-astra".to_string());
    session.input_tokens = Some(34_000);
    session.context_window = Some(258_400);
    session.resolved_context_budget = Some(
        rsi_common::ResolvedContextBudget::new(
            258_400,
            rsi_common::ContextCapacity {
                runtime_effective_tokens: Some(258_400),
                ..rsi_common::ContextCapacity::default()
            },
            rsi_common::CapabilityEvidence {
                source: rsi_common::CapabilitySource::RuntimeTelemetry,
                source_version: Some(
                    crate::provider_capabilities::VALIDATED_CODEX_CLI_VERSION.to_string(),
                ),
                source_digest: Some(
                    crate::provider_capabilities::VALIDATED_CODEX_SEMANTIC_FIXTURE_DIGEST
                        .to_string(),
                ),
                observed_at: Some(chrono::Utc::now()),
                confidence: rsi_common::CapabilityConfidence::Authoritative,
            },
        )
        .expect("valid runtime budget"),
    );
    let store = manager.store().clone();
    tokio::task::spawn_blocking(move || {
        let store = store.blocking_lock();
        store.insert_session(&session)
    })
    .await
    .unwrap()
    .expect("persist capability tuple");

    for loaded in [
        manager.get_session(session_id).await.expect("get session"),
        manager
            .list_sessions()
            .await
            .into_iter()
            .find(|session| session.id == session_id)
            .expect("list session"),
    ] {
        let budget = loaded
            .resolved_context_budget
            .expect("RPC projection carries context budget");
        assert_eq!(budget.active_tokens, 258_400);
        assert_eq!(budget.capacity.runtime_effective_tokens, Some(258_400));
        assert_eq!(budget.capacity.provider_default_tokens, Some(272_000));
        assert_eq!(budget.capacity.provider_max_tokens, Some(872_000));
        assert_eq!(budget.capacity.effective_percent, Some(95));
        assert_eq!(budget.capacity.advertised_max_tokens, Some(1_050_000));
        assert_eq!(budget.capacity.max_output_tokens, Some(128_000));
        assert_eq!(
            budget.evidence.source,
            rsi_common::CapabilitySource::RuntimeTelemetry
        );
    }
}

#[tokio::test]
async fn archived_and_deleted_lists_stamp_context_fill_pct() {
    let (manager, _dir) = manager();
    let archived_id = Uuid::new_v4();
    let deleted_id = Uuid::new_v4();
    let mut archived = bare_session(archived_id);
    archived.status = SessionStatus::Archived;
    archived.provider = SessionProvider::Claude;
    archived.model = Some("claude-opus-4-7".to_string());
    archived.total_input_tokens = Some(500_000);

    let mut deleted = bare_session(deleted_id);
    deleted.status = SessionStatus::Deleted;
    deleted.provider = SessionProvider::Claude;
    deleted.model = Some("claude-opus-4-7".to_string());
    deleted.total_input_tokens = Some(250_000);

    let store = manager.store().clone();
    tokio::task::spawn_blocking(move || {
        let store = store.blocking_lock();
        store.insert_session(&archived)?;
        store.insert_session(&deleted)?;
        Ok::<_, crate::error::DaemonError>(())
    })
    .await
    .unwrap()
    .expect("insert sessions");

    let archived = manager
        .list_archived_sessions(None)
        .await
        .expect("archived list");
    let archived_row = archived
        .iter()
        .find(|s| s.id == archived_id)
        .expect("archived session");
    assert_eq!(archived_row.context_fill_pct, Some(50.0));

    let deleted = manager
        .list_deleted_sessions(None)
        .await
        .expect("deleted list");
    let deleted_row = deleted
        .iter()
        .find(|s| s.id == deleted_id)
        .expect("deleted session");
    assert_eq!(deleted_row.context_fill_pct, Some(25.0));
}

#[tokio::test]
async fn list_sessions_includes_store_rows_missing_from_memory() {
    let (manager, _dir) = manager();
    let in_memory_id = Uuid::new_v4();
    let store_only_id = Uuid::new_v4();

    manager.completed.write().await.insert(
        in_memory_id,
        CompletedSession {
            session: bare_session(in_memory_id),
            events: Vec::new(),
            turn_metrics: Vec::new(),
            retry_cancel: None,
            retry_fired_at: None,
            superseded_by_retry: None,
            events_hydrated: true,
        },
    );

    let store = manager.store().clone();
    tokio::task::spawn_blocking(move || {
        let store = store.blocking_lock();
        let session = bare_session(store_only_id);
        store.insert_session(&session)
    })
    .await
    .unwrap()
    .expect("insert store-only session");

    let sessions = manager.list_sessions().await;
    assert!(sessions.iter().any(|s| s.id == in_memory_id));
    assert!(sessions.iter().any(|s| s.id == store_only_id));
}

#[tokio::test]
async fn list_sessions_omits_hidden_terminal_statuses_from_memory() {
    let (manager, _dir) = manager();
    let visible_id = Uuid::new_v4();
    let deleted_id = Uuid::new_v4();
    let archived_id = Uuid::new_v4();

    let mut deleted = bare_session(deleted_id);
    deleted.status = SessionStatus::Deleted;
    let mut archived = bare_session(archived_id);
    archived.status = SessionStatus::Archived;

    let mut completed = manager.completed.write().await;
    completed.insert(
        visible_id,
        CompletedSession {
            session: bare_session(visible_id),
            events: Vec::new(),
            turn_metrics: Vec::new(),
            retry_cancel: None,
            retry_fired_at: None,
            superseded_by_retry: None,
            events_hydrated: true,
        },
    );
    completed.insert(
        deleted_id,
        CompletedSession {
            session: deleted,
            events: Vec::new(),
            turn_metrics: Vec::new(),
            retry_cancel: None,
            retry_fired_at: None,
            superseded_by_retry: None,
            events_hydrated: true,
        },
    );
    completed.insert(
        archived_id,
        CompletedSession {
            session: archived,
            events: Vec::new(),
            turn_metrics: Vec::new(),
            retry_cancel: None,
            retry_fired_at: None,
            superseded_by_retry: None,
            events_hydrated: true,
        },
    );
    drop(completed);

    let sessions = manager.list_sessions().await;
    assert!(sessions.iter().any(|s| s.id == visible_id));
    assert!(!sessions.iter().any(|s| s.id == deleted_id));
    assert!(!sessions.iter().any(|s| s.id == archived_id));
}

#[tokio::test]
async fn list_sessions_by_project_omits_hidden_terminal_statuses_from_memory() {
    let (manager, _dir) = manager();
    let project_id = Uuid::new_v4();
    let visible_id = Uuid::new_v4();
    let deleted_id = Uuid::new_v4();

    let mut visible = bare_session(visible_id);
    visible.project_id = Some(project_id);
    let mut deleted = bare_session(deleted_id);
    deleted.project_id = Some(project_id);
    deleted.status = SessionStatus::Deleted;

    let mut completed = manager.completed.write().await;
    completed.insert(
        visible_id,
        CompletedSession {
            session: visible,
            events: Vec::new(),
            turn_metrics: Vec::new(),
            retry_cancel: None,
            retry_fired_at: None,
            superseded_by_retry: None,
            events_hydrated: true,
        },
    );
    completed.insert(
        deleted_id,
        CompletedSession {
            session: deleted,
            events: Vec::new(),
            turn_metrics: Vec::new(),
            retry_cancel: None,
            retry_fired_at: None,
            superseded_by_retry: None,
            events_hydrated: true,
        },
    );
    drop(completed);

    let sessions = manager.list_sessions_by_project(Some(project_id)).await;
    assert!(sessions.iter().any(|s| s.id == visible_id));
    assert!(!sessions.iter().any(|s| s.id == deleted_id));
}

#[test]
fn approval_wait_idempotent_open_preserves_existing_start() {
    let mut start: Option<std::time::Instant> = None;
    let mut total_ms: u64 = 0;

    open_approval_interval(&mut start);
    let first_instant = start.expect("first open");
    // Defensive: a duplicate AskUserQuestion in the same session MUST NOT
    // reset the interval start — that would discard the already-elapsed time.
    open_approval_interval(&mut start);
    let second_instant = start.expect("second open");
    assert_eq!(
        first_instant, second_instant,
        "duplicate open must preserve existing start instant"
    );

    std::thread::sleep(std::time::Duration::from_millis(5));
    close_approval_interval(&mut start, &mut total_ms);
    assert!(total_ms >= 5);
}

#[test]
fn approval_wait_closes_open_interval_at_finalize() {
    let mut start: Option<std::time::Instant> = None;
    let mut total_ms: u64 = 0;

    // Open a question and never explicitly answer — mimics a session that
    // terminalizes while still in WaitingApproval state.
    open_approval_interval(&mut start);
    std::thread::sleep(std::time::Duration::from_millis(8));
    assert!(start.is_some(), "interval still open pre-finalize");

    // The finalize hook must close the still-open interval.
    close_approval_interval(&mut start, &mut total_ms);
    assert!(start.is_none(), "finalize consumed the open interval");
    assert!(total_ms >= 8, "finalize recorded elapsed ms");
}

#[test]
fn approval_wait_never_opened_returns_zero() {
    let mut start: Option<std::time::Instant> = None;
    let mut total_ms: u64 = 0;

    // A session that never hits an AskUserQuestion still runs through the
    // finalize snapshot. The close path is a no-op; total stays at 0.
    close_approval_interval(&mut start, &mut total_ms);
    assert_eq!(total_ms, 0, "unopened accumulator stays at zero");
    assert!(start.is_none());
}

// ---------------------------------------------------------------------------
// Single-flight session spawn guard (dual-resume split-brain dedup).
// ---------------------------------------------------------------------------

/// Build a `TrackedSession` whose Local process never finishes, so
/// `is_alive()` is deterministically `true` (an adoptable live child) without a
/// provider binary.
async fn fake_alive_tracked(id: Uuid) -> TrackedSession {
    let handle = tokio::spawn(async { std::future::pending::<()>().await });
    let mut session = bare_session(id);
    session.status = SessionStatus::Running;
    let mut tracked = TrackedSession::new_for_test(session);
    tracked.process = Some(super::types::ProviderProcess::Local(
        crate::openai::OpenAiProcess::test_task(handle),
    ));
    tracked
}

/// Build a `TrackedSession` whose Local process has already completed, so
/// `is_alive()` is deterministically `false` (a finalized/nulled-handle zombie
/// that must NOT be adopted).
async fn fake_dead_tracked(id: Uuid) -> TrackedSession {
    let handle = tokio::spawn(async {});
    // Deterministically drive the task to completion so is_finished() == true.
    while !handle.is_finished() {
        tokio::task::yield_now().await;
    }
    let mut session = bare_session(id);
    session.status = SessionStatus::Running;
    let mut tracked = TrackedSession::new_for_test(session);
    tracked.process = Some(super::types::ProviderProcess::Local(
        crate::openai::OpenAiProcess::test_task(handle),
    ));
    tracked
}

/// Build a `TrackedSession` whose Local task runs for `delay` then completes on
/// its own — `is_alive()` flips to `false` mid-grace with NO `kill()`. Models a
/// provider that honors SIGINT a beat after the grace poll begins.
async fn fake_dies_after(id: Uuid, delay: std::time::Duration) -> TrackedSession {
    let handle = tokio::spawn(async move {
        tokio::time::sleep(delay).await;
    });
    let mut session = bare_session(id);
    session.status = SessionStatus::Running;
    let mut tracked = TrackedSession::new_for_test(session);
    tracked.process = Some(super::types::ProviderProcess::Local(
        crate::openai::OpenAiProcess::test_task(handle),
    ));
    tracked
}

/// Standalone `active` map for `ensure_process_dead` state-machine tests — the
/// teardown kill-verify (A5 Change 1) only ever touches `active`, so no full
/// SessionManager is needed.
fn reaper_active_map()
-> std::sync::Arc<tokio::sync::RwLock<std::collections::HashMap<Uuid, TrackedSession>>> {
    std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()))
}

/// A5 Change 1: a provider that never dies on SIGINT is escalated to SIGKILL
/// after the grace window, and is dead afterward.
#[tokio::test]
async fn ensure_process_dead_escalates_when_never_dies() {
    let id = Uuid::new_v4();
    let map = reaper_active_map();
    map.write().await.insert(id, fake_alive_tracked(id).await);

    let outcome = super::reaper::ensure_process_dead(
        &map,
        id,
        std::time::Duration::from_millis(150),
        std::time::Duration::from_millis(15),
    )
    .await;
    assert_eq!(outcome, super::reaper::ReapOutcome::Escalated);

    // SIGKILL/abort completes asynchronously; poll (bounded) until dead.
    let mut dead = false;
    for _ in 0..200 {
        {
            let mut g = map.write().await;
            let p = g
                .get_mut(&id)
                .and_then(|t| t.process.as_mut())
                .expect("handle present");
            if !p.is_alive() {
                dead = true;
                break;
            }
        }
        tokio::task::yield_now().await;
    }
    assert!(dead, "escalated process must be dead after SIGKILL");
}

/// A5 Change 1: an already-dead handle returns `ExitedGracefully` with no
/// escalation — the normal, common teardown path.
#[tokio::test]
async fn ensure_process_dead_already_dead_exits_gracefully() {
    let id = Uuid::new_v4();
    let map = reaper_active_map();
    map.write().await.insert(id, fake_dead_tracked(id).await);

    let outcome = super::reaper::ensure_process_dead(
        &map,
        id,
        std::time::Duration::from_millis(150),
        std::time::Duration::from_millis(15),
    )
    .await;
    assert_eq!(outcome, super::reaper::ReapOutcome::ExitedGracefully);
}

/// A5 Change 1 (normal-path preservation): a provider that dies *within* the
/// grace window returns `ExitedGracefully`, proving NO SIGKILL is sent when the
/// process exits on its own before the deadline.
#[tokio::test]
async fn ensure_process_dead_dies_mid_grace_no_sigkill() {
    let id = Uuid::new_v4();
    let map = reaper_active_map();
    map.write().await.insert(
        id,
        fake_dies_after(id, std::time::Duration::from_millis(60)).await,
    );

    let outcome = super::reaper::ensure_process_dead(
        &map,
        id,
        std::time::Duration::from_secs(2),
        std::time::Duration::from_millis(15),
    )
    .await;
    assert_eq!(outcome, super::reaper::ReapOutcome::ExitedGracefully);
}

/// A5 Change 1: a session with no process handle (task provider / already
/// dropped) returns `NoHandle`.
#[tokio::test]
async fn ensure_process_dead_no_handle() {
    let id = Uuid::new_v4();
    let map = reaper_active_map();
    // new_for_test builds a TrackedSession with process: None.
    map.write()
        .await
        .insert(id, TrackedSession::new_for_test(bare_session(id)));

    let outcome = super::reaper::ensure_process_dead(
        &map,
        id,
        std::time::Duration::from_millis(150),
        std::time::Duration::from_millis(15),
    )
    .await;
    assert_eq!(outcome, super::reaper::ReapOutcome::NoHandle);
}

struct ScriptedProcessControls {
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
    exit_code: std::sync::Arc<std::sync::atomic::AtomicI32>,
    interrupt_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    kill_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

fn scripted_process_tracked(
    id: Uuid,
    generation: u64,
    alive_initially: bool,
    exit_code: i32,
    exit_on_interrupt: bool,
    interrupt_fails: bool,
    kill_fails: bool,
) -> (TrackedSession, ScriptedProcessControls) {
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize};

    let controls = ScriptedProcessControls {
        alive: std::sync::Arc::new(AtomicBool::new(alive_initially)),
        exit_code: std::sync::Arc::new(AtomicI32::new(exit_code)),
        interrupt_count: std::sync::Arc::new(AtomicUsize::new(0)),
        kill_count: std::sync::Arc::new(AtomicUsize::new(0)),
    };
    let mut tracked = TrackedSession::new_for_test(bare_session(id));
    tracked.spawn_generation = generation;
    tracked.process = Some(super::types::ProviderProcess::Scripted(
        super::types::ScriptedProcess {
            alive: std::sync::Arc::clone(&controls.alive),
            exit_code: std::sync::Arc::clone(&controls.exit_code),
            interrupt_count: std::sync::Arc::clone(&controls.interrupt_count),
            kill_count: std::sync::Arc::clone(&controls.kill_count),
            exit_on_interrupt,
            interrupt_fails,
            kill_fails,
        },
    ));
    (tracked, controls)
}

#[tokio::test]
async fn terminal_settlement_outcomes_are_exhaustive() {
    use super::types::{ProcessSettlementMode, ProcessSettlementOutcome};
    use std::sync::atomic::Ordering;

    let settle = |map: std::sync::Arc<
        tokio::sync::RwLock<std::collections::HashMap<Uuid, TrackedSession>>,
    >,
                  id,
                  mode,
                  natural_grace| async move {
        super::reaper::settle_process_ownership(
            &map,
            id,
            7,
            mode,
            natural_grace,
            std::time::Duration::from_millis(30),
            std::time::Duration::from_millis(2),
        )
        .await
    };

    let already_dead = Uuid::new_v4();
    let map = reaper_active_map();
    let (tracked, controls) =
        scripted_process_tracked(already_dead, 7, false, 0, false, false, false);
    map.write().await.insert(already_dead, tracked);
    assert_eq!(
        settle(
            map,
            already_dead,
            ProcessSettlementMode::AwaitNatural,
            std::time::Duration::from_millis(20),
        )
        .await,
        ProcessSettlementOutcome::AlreadyExited
    );
    assert_eq!(controls.interrupt_count.load(Ordering::SeqCst), 0);
    assert_eq!(controls.kill_count.load(Ordering::SeqCst), 0);

    let natural = Uuid::new_v4();
    let map = reaper_active_map();
    let (tracked, controls) = scripted_process_tracked(natural, 7, true, 0, false, false, false);
    map.write().await.insert(natural, tracked);
    let natural_alive = std::sync::Arc::clone(&controls.alive);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        natural_alive.store(false, Ordering::SeqCst);
    });
    assert_eq!(
        settle(
            map,
            natural,
            ProcessSettlementMode::AwaitNatural,
            std::time::Duration::from_millis(100),
        )
        .await,
        ProcessSettlementOutcome::ExitedNaturally
    );
    assert_eq!(controls.interrupt_count.load(Ordering::SeqCst), 0);

    let graceful = Uuid::new_v4();
    let map = reaper_active_map();
    let (tracked, controls) = scripted_process_tracked(graceful, 7, true, 0, true, false, false);
    map.write().await.insert(graceful, tracked);
    assert_eq!(
        settle(
            map,
            graceful,
            ProcessSettlementMode::InterruptNow,
            std::time::Duration::ZERO,
        )
        .await,
        ProcessSettlementOutcome::ExitedAfterInterrupt
    );
    assert_eq!(controls.interrupt_count.load(Ordering::SeqCst), 1);
    assert_eq!(controls.kill_count.load(Ordering::SeqCst), 0);

    let escalated = Uuid::new_v4();
    let map = reaper_active_map();
    let (tracked, controls) = scripted_process_tracked(escalated, 7, true, 0, false, false, false);
    map.write().await.insert(escalated, tracked);
    assert_eq!(
        settle(
            map,
            escalated,
            ProcessSettlementMode::InterruptNow,
            std::time::Duration::ZERO,
        )
        .await,
        ProcessSettlementOutcome::Escalated
    );
    assert_eq!(controls.interrupt_count.load(Ordering::SeqCst), 1);
    assert_eq!(controls.kill_count.load(Ordering::SeqCst), 1);
    assert!(!controls.alive.load(Ordering::SeqCst));

    let interrupt_failed = Uuid::new_v4();
    let map = reaper_active_map();
    let (tracked, controls) =
        scripted_process_tracked(interrupt_failed, 7, true, 0, false, true, false);
    map.write().await.insert(interrupt_failed, tracked);
    assert_eq!(
        settle(
            map,
            interrupt_failed,
            ProcessSettlementMode::InterruptNow,
            std::time::Duration::ZERO,
        )
        .await,
        ProcessSettlementOutcome::EscalatedAfterInterruptFailure
    );
    assert_eq!(controls.interrupt_count.load(Ordering::SeqCst), 1);
    assert_eq!(controls.kill_count.load(Ordering::SeqCst), 1);
    assert!(!controls.alive.load(Ordering::SeqCst));

    let failed = Uuid::new_v4();
    let map = reaper_active_map();
    let (tracked, controls) = scripted_process_tracked(failed, 7, true, 0, false, false, true);
    map.write().await.insert(failed, tracked);
    assert_eq!(
        settle(
            map,
            failed,
            ProcessSettlementMode::InterruptNow,
            std::time::Duration::ZERO,
        )
        .await,
        ProcessSettlementOutcome::EscalationFailed
    );
    assert!(controls.alive.load(Ordering::SeqCst));

    let no_handle = Uuid::new_v4();
    let map = reaper_active_map();
    let mut tracked = TrackedSession::new_for_test(bare_session(no_handle));
    tracked.spawn_generation = 7;
    map.write().await.insert(no_handle, tracked);
    assert_eq!(
        settle(
            map,
            no_handle,
            ProcessSettlementMode::InterruptNow,
            std::time::Duration::ZERO,
        )
        .await,
        ProcessSettlementOutcome::NoHandle
    );

    for outcome in [
        ProcessSettlementOutcome::NoHandle,
        ProcessSettlementOutcome::AlreadyExited,
        ProcessSettlementOutcome::ExitedNaturally,
        ProcessSettlementOutcome::ExitedAfterInterrupt,
        ProcessSettlementOutcome::Escalated,
        ProcessSettlementOutcome::EscalatedAfterInterruptFailure,
    ] {
        assert!(outcome.is_settled(), "{outcome:?}");
    }
    for outcome in [
        ProcessSettlementOutcome::GenerationChanged,
        ProcessSettlementOutcome::EscalationFailed,
    ] {
        assert!(!outcome.is_settled(), "{outcome:?}");
    }
}

#[tokio::test]
async fn terminal_settlement_generation_change_never_touches_newer_process() {
    use super::types::{ProcessSettlementMode, ProcessSettlementOutcome};
    use std::sync::atomic::Ordering;

    let id = Uuid::new_v4();
    let map = reaper_active_map();
    let (tracked, controls) = scripted_process_tracked(id, 8, true, 0, false, false, false);
    map.write().await.insert(id, tracked);

    let outcome = super::reaper::settle_process_ownership(
        &map,
        id,
        7,
        ProcessSettlementMode::InterruptNow,
        std::time::Duration::ZERO,
        std::time::Duration::from_millis(10),
        std::time::Duration::from_millis(1),
    )
    .await;

    assert_eq!(outcome, ProcessSettlementOutcome::GenerationChanged);
    assert!(controls.alive.load(Ordering::SeqCst));
    assert_eq!(controls.interrupt_count.load(Ordering::SeqCst), 0);
    assert_eq!(controls.kill_count.load(Ordering::SeqCst), 0);
}

/// B-001: `abort()` on a Local/Harness task is only a request.  A task already
/// running in a blocking section remains observable as live until its own
/// completion barrier releases, so escalation must retain ownership instead
/// of reporting success after the abort call returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_backed_local_and_harness_escalation_wait_for_observed_death() {
    use super::types::{HarnessProcess, ProcessSettlementMode, ProcessSettlementOutcome};
    use std::sync::{Arc, Barrier, mpsc as std_mpsc};

    for provider in ["local", "harness"] {
        let id = Uuid::new_v4();
        let map = reaper_active_map();
        let release = Arc::new(Barrier::new(2));
        let (started_tx, started_rx) = std_mpsc::sync_channel(1);
        let task_release = Arc::clone(&release);
        let handle = tokio::task::spawn_blocking(move || {
            started_tx
                .send(())
                .expect("test observes blocking task start");
            task_release.wait();
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("task reached barrier");

        let mut tracked = TrackedSession::new_for_test(bare_session(id));
        tracked.spawn_generation = 7;
        tracked.process = Some(match provider {
            "local" => super::types::ProviderProcess::Local(
                crate::openai::OpenAiProcess::test_task(handle),
            ),
            "harness" => super::types::ProviderProcess::Harness(HarnessProcess {
                task_handle: handle,
                cancel: tokio_util::sync::CancellationToken::new(),
            }),
            _ => unreachable!("fixed provider matrix"),
        });
        map.write().await.insert(id, tracked);

        let outcome = super::reaper::settle_process_ownership(
            &map,
            id,
            7,
            ProcessSettlementMode::InterruptNow,
            std::time::Duration::ZERO,
            std::time::Duration::from_millis(20),
            std::time::Duration::from_millis(2),
        )
        .await;
        assert_eq!(
            outcome,
            ProcessSettlementOutcome::EscalationFailed,
            "{provider} must not report escalation before its task is finished"
        );
        assert!(
            map.write()
                .await
                .get_mut(&id)
                .and_then(|tracked| tracked.process.as_mut())
                .expect("unsettled task remains recoverably owned")
                .is_alive(),
            "{provider} task remains live behind its completion barrier"
        );

        release.wait();
        let recovered = super::reaper::settle_process_ownership(
            &map,
            id,
            7,
            ProcessSettlementMode::AwaitNatural,
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(20),
            std::time::Duration::from_millis(2),
        )
        .await;
        assert!(
            recovered.is_settled(),
            "{provider} converges only after the real task releases"
        );
    }
}

/// B-004: an old monitor must reject a newer generation before it advances a
/// rotation deadline or signals its handle. The newer coordinator is set to an
/// already-expired kill-on-timeout phase to make the formerly-dangerous branch
/// deterministic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_monitor_rotation_deadline_neither_interrupts_nor_kills_new_generation() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.status = SessionStatus::Running;
    insert_row(&manager, &session).await;
    let (mut tracked, controls) =
        scripted_process_tracked(session_id, 8, true, 0, false, false, false);
    tracked.session = session;
    tracked.rotation.state = super::rotation_coordinator::RotationState::PendingInterrupt {
        deadline: tokio::time::Instant::now(),
    };
    manager.active.write().await.insert(session_id, tracked);
    let (_provider_tx, provider_rx) = mpsc::channel(1);
    let (_stop_tx, stop_rx) = mpsc::channel(1);

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        SessionManager::monitor_session(
            session_id,
            7,
            Box::new(crate::provider::CliProviderSession::new(provider_rx)),
            std::sync::Arc::clone(&manager.active),
            std::sync::Arc::clone(&manager.completed),
            std::sync::Arc::clone(&manager.event_bus),
            stop_rx,
            std::sync::Arc::clone(&manager.store),
            manager
                .model_call_settlements
                .handle()
                .expect("model settlement handle"),
            manager.persistence.clone(),
            0,
            false,
            manager.socket_path.clone(),
            std::sync::Arc::clone(&manager.token_counter),
            None,
            manager.retry_tx.clone(),
            std::sync::Arc::clone(&manager.tool_registry),
            crate::turn_controller::TurnController::new(
                crate::turn_controller::ContinuationPolicy::Single,
            ),
            std::sync::Arc::clone(&manager.runtime_config),
            std::sync::Arc::clone(&manager.spawn_coordinator),
            std::sync::Arc::clone(&manager.agent_tokens),
            std::sync::Arc::clone(&manager.spawn_epoch),
            std::sync::Arc::clone(&manager.agent_message_arbiter),
            manager.codegraph_handle.clone(),
            manager.custody_execution_runtime(),
        ),
    )
    .await
    .expect("stale monitor returns at generation-safe deadline gate");

    use std::sync::atomic::Ordering;
    assert!(controls.alive.load(Ordering::SeqCst));
    assert_eq!(controls.interrupt_count.load(Ordering::SeqCst), 0);
    assert_eq!(controls.kill_count.load(Ordering::SeqCst), 0);
    let active = manager.active.read().await;
    let newer = active.get(&session_id).expect("newer generation retained");
    assert_eq!(newer.spawn_generation, 8);
    assert!(matches!(
        newer.rotation.state(),
        super::rotation_coordinator::RotationState::PendingInterrupt { .. }
    ));
}

/// A closed producer and an expired rotation deadline must emit one durable timeout
/// while process settlement remains pending.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::expect_used, clippy::large_futures)]
async fn expired_rotation_deadline_with_closed_stream_records_one_timed_out_event() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.status = SessionStatus::Running;
    insert_row(&manager, &session).await;
    let (mut tracked, _controls) =
        scripted_process_tracked(session_id, 7, true, 0, false, true, true);
    tracked.session = session;
    tracked.rotation =
        super::rotation_coordinator::RotationCoordinator::new_writing_handoff(session_id, 0, true);
    tracked.rotation.state = super::rotation_coordinator::RotationState::WritingHandoff {
        handoff_filepath: None,
        deadline: tokio::time::Instant::now(),
    };
    let rotation_id = tracked
        .rotation
        .rotation_id()
        .expect("rotation id")
        .to_string();
    manager.active.write().await.insert(session_id, tracked);
    let (provider_tx, provider_rx) = mpsc::channel(1);
    drop(provider_tx);
    let (_stop_tx, stop_rx) = mpsc::channel(1);

    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        SessionManager::monitor_session(
            session_id,
            7,
            Box::new(crate::provider::CliProviderSession::new(provider_rx)),
            std::sync::Arc::clone(&manager.active),
            std::sync::Arc::clone(&manager.completed),
            std::sync::Arc::clone(&manager.event_bus),
            stop_rx,
            std::sync::Arc::clone(&manager.store),
            manager
                .model_call_settlements
                .handle()
                .expect("model settlement handle"),
            manager.persistence.clone(),
            0,
            false,
            manager.socket_path.clone(),
            std::sync::Arc::clone(&manager.token_counter),
            None,
            manager.retry_tx.clone(),
            std::sync::Arc::clone(&manager.tool_registry),
            crate::turn_controller::TurnController::new(
                crate::turn_controller::ContinuationPolicy::Single,
            ),
            std::sync::Arc::clone(&manager.runtime_config),
            std::sync::Arc::clone(&manager.spawn_coordinator),
            std::sync::Arc::clone(&manager.agent_tokens),
            std::sync::Arc::clone(&manager.spawn_epoch),
            std::sync::Arc::clone(&manager.agent_message_arbiter),
            manager.codegraph_handle.clone(),
            manager.custody_execution_runtime(),
        ),
    )
    .await;

    let count: i64 = manager.store.lock().await.conn.query_row(
        "SELECT count(*) FROM rotation_events WHERE session_id=?1 AND rotation_id=?2 AND phase='writing_handoff' AND event_type='timed_out'",
        rusqlite::params![session_id.to_string(), rotation_id],
        |row| row.get(0),
    ).expect("count durable timeouts");
    assert_eq!(count, 1, "one timeout event per rotation phase");
}

/// Test-only exercise of the production single-flight primitive. Acquires the
/// real `acquire_spawn_guard`; on the contended-live branch it adopts (emits a
/// dedup event and returns `false`); otherwise it runs the spawn branch
/// (holds the guard across `hold`, registers a fake-ALIVE child, bumps `count`,
/// returns `true`). Mirrors a spawn site's check -> launch -> active.insert span.
async fn guarded_fake_spawn(
    mgr: &SessionManager,
    id: Uuid,
    count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    hold: std::time::Duration,
) -> bool {
    use std::sync::atomic::Ordering;
    let spawn_guard = super::spawn_single_flight::acquire_spawn_guard(id).await;
    if spawn_guard.contended() && super::spawn_single_flight::adopt_if_live(&mgr.active, id).await {
        mgr.event_bus
            .publish(crate::bus::DaemonEvent::SessionSpawnDeduped {
                session_id: id,
                source: "guarded_fake_spawn".to_string(),
            });
        return false;
    }
    // Spawn branch: hold the guard across the "launch" window so a racing caller
    // blocks, then register the live child (as a real spawn site's insert does).
    tokio::time::sleep(hold).await;
    let tracked = fake_alive_tracked(id).await;
    mgr.active.write().await.insert(id, tracked);
    count.fetch_add(1, Ordering::SeqCst);
    true
}

/// Two racing resumes for one live session id resolve to exactly one tracked
/// child: the first spawns, the second blocks on the per-session guard and
/// adopts. Proves the split-brain dedup (F-004, F-005, F-012).
#[tokio::test]
async fn dual_resume_spawns_single_child() {
    use std::sync::atomic::Ordering;
    let (mgr, _dir) = manager();
    let id = Uuid::new_v4();
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut rx = mgr.event_bus.subscribe();

    // `join!` polls left-to-right: caller-1 acquires the guard non-contended and
    // parks on its hold; caller-2 then contends and, on acquiring, adopts.
    let (r1, r2) = tokio::join!(
        guarded_fake_spawn(
            &mgr,
            id,
            count.clone(),
            std::time::Duration::from_millis(150)
        ),
        guarded_fake_spawn(&mgr, id, count.clone(), std::time::Duration::from_millis(0)),
    );

    assert!(r1, "first (non-contended) caller must spawn");
    assert!(!r2, "second (contended) caller must adopt, not spawn");
    assert_eq!(count.load(Ordering::SeqCst), 1, "exactly one child spawned");
    assert_eq!(
        mgr.active.read().await.len(),
        1,
        "exactly one tracked child for the session id"
    );

    let mut dedup = 0usize;
    while let Ok(ev) = rx.try_recv() {
        if matches!(
            ev.as_ref(),
            crate::bus::DaemonEvent::SessionSpawnDeduped { .. }
        ) {
            dedup += 1;
        }
    }
    assert_eq!(dedup, 1, "exactly one session_spawn_deduped event emitted");
}

/// A reconciliation store/liveness pass running during an in-flight guarded
/// spawn must NOT produce a second child. Locks F-007's "reconcile is not a
/// spawner" invariant as a regression guard.
#[tokio::test]
async fn reconcile_during_guarded_spawn_no_double_spawn() {
    use std::sync::atomic::Ordering;
    let (mgr, _dir) = manager();
    let id = Uuid::new_v4();
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Represent an in-flight guarded spawn: hold the spawn guard AND register the
    // live child, as the spawn branch would, for the window where memory holds a
    // live child but the store row is not yet consistent.
    let spawn_guard = super::spawn_single_flight::acquire_spawn_guard(id).await;
    let tracked = fake_alive_tracked(id).await;
    mgr.active.write().await.insert(id, tracked);
    count.fetch_add(1, Ordering::SeqCst);

    // Drive the REAL reconciliation loop. Its store-consistency pass sees `id`
    // live in memory but absent from the store's active set (we never inserted a
    // store row), publishing a SystemMessage — a deterministic signal that a
    // full pass ran. Reconciliation must never spawn a provider.
    let mut rx = mgr.event_bus.subscribe();
    let handle = crate::reconciliation::spawn_reconciliation_loop(
        std::sync::Arc::clone(&mgr.active),
        mgr.store().clone(),
        std::sync::Arc::clone(&mgr.event_bus),
        crate::reconciliation::ReconciliationConfig {
            liveness_interval_secs: 1,
            consistency_interval_secs: 1,
            ..Default::default()
        },
    );

    // Await proof (bounded) that the store-consistency pass executed.
    let saw_pass = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if let crate::bus::DaemonEvent::SystemMessage { message, .. } = ev.as_ref()
                        && message.contains(&id.to_string())
                    {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(_) => continue,
            }
        }
    })
    .await;
    handle.abort();

    assert!(
        saw_pass.is_ok(),
        "reconciliation store-consistency pass did not run in time"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "reconciliation must not spawn a second child"
    );
    assert_eq!(
        mgr.active.read().await.len(),
        1,
        "reconciliation created no extra tracked child"
    );
    drop(spawn_guard);
}

/// A finalized/dead tracked child (nulled or completed handle) must NOT be
/// adopted: a contended caller that finds a non-live entry falls through and
/// spawns fresh, emitting no dedup event (F-002, F-012, F-003).
#[tokio::test]
async fn stale_process_is_not_adopted_and_respawns() {
    use std::sync::atomic::Ordering;
    let (mgr, _dir) = manager();
    let id = Uuid::new_v4();

    // Pre-insert a dead child so the adopt check sees a non-live entry.
    let dead = fake_dead_tracked(id).await;
    mgr.active.write().await.insert(id, dead);

    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut rx = mgr.event_bus.subscribe();

    // A pure holder forces the guarded spawn to run its *contended* path against
    // the dead child. `join!` polls the holder first, so it wins the guard.
    let (_, spawned) = tokio::join!(
        async {
            let g = super::spawn_single_flight::acquire_spawn_guard(id).await;
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            drop(g);
        },
        guarded_fake_spawn(&mgr, id, count.clone(), std::time::Duration::from_millis(0)),
    );

    assert!(
        spawned,
        "a dead/nulled child must NOT be adopted -> respawn fresh"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "contended-but-not-live caller runs the spawn branch"
    );

    let mut dedup = 0usize;
    while let Ok(ev) = rx.try_recv() {
        if matches!(
            ev.as_ref(),
            crate::bus::DaemonEvent::SessionSpawnDeduped { .. }
        ) {
            dedup += 1;
        }
    }
    assert_eq!(
        dedup, 0,
        "no dedup event when the stale child is not adopted"
    );
}

/// B1 regression: the spawn guard MUST be released before a spawn site's inline
/// `monitor_session` await, so a same-task, same-id re-acquire *after* the
/// check -> launch -> insert window does NOT block. The two rotation sites
/// (`spawn_rotation_child`, `resume_for_handoff_write`) run `monitor_session`
/// inline on the same task; when that session rotates again the monitor
/// re-enters `acquire_spawn_guard` for the SAME id. If the guard were still held
/// (the B1 bug) that re-acquire would deadlock on the non-reentrant
/// `tokio::Mutex`.
///
/// This drives the REAL `acquire_spawn_guard` primitive: acquire -> insert a
/// fake-ALIVE child -> drop the guard (the fix) -> re-acquire on the SAME task
/// under a bounded timeout. With the drop, the re-acquire returns immediately and
/// is NOT contended. Were the drop missing, the re-acquire would park forever and
/// the timeout would fire -> a clean failure, not a hung suite.
#[tokio::test]
async fn guard_released_before_monitor_allows_same_task_reacquire() {
    let (mgr, _dir) = manager();
    let id = Uuid::new_v4();

    // Spawn site: hold the guard across check -> launch -> active.insert.
    let spawn_guard = super::spawn_single_flight::acquire_spawn_guard(id).await;
    assert!(
        !spawn_guard.contended(),
        "first acquire on a fresh id is the sole spawner (not contended)"
    );
    mgr.active
        .write()
        .await
        .insert(id, fake_alive_tracked(id).await);

    // The B1 fix: drop the guard BEFORE the inline monitor await — exactly the
    // `drop(spawn_guard)` the two rotation sites now perform after their insert.
    drop(spawn_guard);

    // A 2nd-generation rotation re-enters the guard for the SAME id on the SAME
    // task (the all-inline monitor chain). Bounded so a reintroduced B1
    // self-deadlock surfaces as a timeout, never as a hang.
    let reacquire = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        super::spawn_single_flight::acquire_spawn_guard(id),
    )
    .await;

    let guard = reacquire.expect(
        "same-task re-acquire after the guard was dropped must not block \
         (B1 self-deadlock regression)",
    );
    assert!(
        !guard.contended(),
        "guard was released before re-acquire, so the re-acquire is uncontended"
    );
}

// ---------------------------------------------------------------------------
// A8 terminal watch — predicate, planning, lineage, delivery policy.
// ---------------------------------------------------------------------------

fn mk_watch_job_for(watched: Uuid, master: Uuid, message: &str) -> rsi_common::types::ScheduledJob {
    let now = chrono::Utc::now();
    rsi_common::types::ScheduledJob {
        id: Uuid::new_v4(),
        name: "rsi-watch".to_string(),
        message: message.to_string(),
        schedule: rsi_common::types::ScheduleSpec {
            recurrence: rsi_common::types::Recurrence::EverySeconds(60),
            anchor: now,
        },
        last_fired_at: None,
        next_fire_at: now,
        enabled: true,
        working_dir: None,
        provider: None,
        model: None,
        project_id: None,
        created_at: now,
        updated_at: now,
        wake_mode: rsi_common::types::WakeMode::OnTerminal(watched),
        wake_session_id: Some(master),
    }
}

#[allow(clippy::expect_used)]
async fn insert_row(manager: &SessionManager, session: &Session) {
    manager
        .store
        .lock()
        .await
        .insert_session(session)
        .expect("insert session row");
}

async fn wait_for_retry_state(
    manager: &SessionManager,
    session_id: Uuid,
    retry_attempt: Option<u8>,
    max_retries: Option<u8>,
) -> Session {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
    loop {
        let row = manager
            .store
            .lock()
            .await
            .get_session(session_id)
            .expect("load session")
            .expect("session row");
        if row.retry_attempt == retry_attempt && row.max_retries == max_retries {
            return row;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for retry state {:?}/{:?}; last row had {:?}/{:?}",
            retry_attempt,
            max_retries,
            row.retry_attempt,
            row.max_retries
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// T-4 / T-5: the pure predicate decision table (D4/D5 policy).
#[test]
fn watch_fire_decision_table() {
    use super::{WatchDecision, watch_fire_decision};

    // Plain terminal states fire with no annotations.
    for status in [
        SessionStatus::Completed,
        SessionStatus::Interrupted,
        SessionStatus::Archived,
        SessionStatus::Deleted,
    ] {
        assert_eq!(
            watch_fire_decision(Some(status), false, false),
            WatchDecision::Fire {
                retry_eligible: None,
                question_pending: false
            },
            "{status:?} must fire plain"
        );
    }

    // T-5 (D4): Failed + retry-eligible + LIVE timer -> suppress.
    assert_eq!(
        watch_fire_decision(Some(SessionStatus::Failed), true, true),
        WatchDecision::NotReady
    );
    // Failed + eligible but NO live timer (post-restart) -> fire, annotated.
    assert_eq!(
        watch_fire_decision(Some(SessionStatus::Failed), true, false),
        WatchDecision::Fire {
            retry_eligible: Some(true),
            question_pending: false
        }
    );
    // Failed exhausted -> fire regardless of any stale timer marker.
    assert_eq!(
        watch_fire_decision(Some(SessionStatus::Failed), false, true),
        WatchDecision::Fire {
            retry_eligible: Some(false),
            question_pending: false
        }
    );
    assert_eq!(
        watch_fire_decision(Some(SessionStatus::Failed), false, false),
        WatchDecision::Fire {
            retry_eligible: Some(false),
            question_pending: false
        }
    );

    // T-4 (D5): WaitingApproval is notify-worthy, annotated.
    assert_eq!(
        watch_fire_decision(Some(SessionStatus::WaitingApproval), false, false),
        WatchDecision::Fire {
            retry_eligible: None,
            question_pending: true
        }
    );

    // Live states wait; a missing row abandons.
    assert_eq!(
        watch_fire_decision(Some(SessionStatus::Starting), false, false),
        WatchDecision::NotReady
    );
    assert_eq!(
        watch_fire_decision(Some(SessionStatus::Running), false, false),
        WatchDecision::NotReady
    );
    assert_eq!(
        watch_fire_decision(None, false, false),
        WatchDecision::Abandon
    );
}

/// T-1 (session half): an already-terminal-at-arm child plans a delivery on
/// the FIRST attempt — no waiting on further transitions (arm-time race).
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn arm_on_already_terminal_child_fires_immediately() {
    let (manager, _dir) = manager();
    let child = Uuid::new_v4();
    let master = Uuid::new_v4();
    insert_row(&manager, &bare_session(child)).await; // bare_session = Completed
    insert_row(&manager, &bare_session(master)).await;
    push_event(
        &manager,
        master,
        1,
        EventType::Message,
        Some(Role::Assistant),
        chrono::Utc::now() - chrono::Duration::seconds(5),
    )
    .await;

    let job = mk_watch_job_for(child, master, "watching build");
    manager
        .store
        .lock()
        .await
        .insert_scheduled_job(&job)
        .expect("insert job");

    match manager.plan_terminal_watch_fire(&job).await.expect("plan") {
        super::WatchFirePlan::Deliver {
            tip,
            message,
            job_ids,
            ..
        } => {
            assert_eq!(tip, master);
            assert_eq!(job_ids, vec![job.id]);
            assert!(message.contains("[rsid-watch]"));
            assert!(message.contains(&child.to_string()[..8]));
            assert!(message.contains("Completed"));
            assert!(message.contains("watching build"));
            assert!(
                !message.contains("retry-eligible"),
                "plain terminal must not carry retry annotation"
            );
        }
        other => panic!("expected Deliver, got {other:?}"),
    }
}

/// T-5 (session half, D4): a live in-memory retry timer suppresses the fire;
/// without it (post-restart shape) the fire is annotated with eligibility.
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn failed_with_live_retry_timer_suppresses_exhausted_or_timerless_fires_annotated() {
    let (manager, _dir) = manager();
    let child = Uuid::new_v4();
    let master = Uuid::new_v4();
    let mut failed = bare_session(child);
    failed.status = SessionStatus::Failed;
    failed.retry_attempt = Some(0);
    failed.max_retries = Some(2);
    insert_row(&manager, &failed).await;
    insert_row(&manager, &bare_session(master)).await;

    // Live retry timer marker in the completed map.
    let (cancel_tx, _cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let mut cs = CompletedSession::for_test(failed.clone());
    cs.retry_cancel = Some(cancel_tx);
    manager.completed.write().await.insert(child, cs);

    let job = mk_watch_job_for(child, master, "");
    assert_eq!(
        manager.plan_terminal_watch_fire(&job).await.expect("plan"),
        super::WatchFirePlan::NotReady,
        "live retry timer must suppress the fire"
    );

    {
        let mut completed = manager.completed.write().await;
        let cs = completed.get_mut(&child).expect("completed retry marker");
        cs.retry_cancel = None;
        cs.retry_fired_at = Some(std::time::Instant::now());
    }
    assert_eq!(
        manager.plan_terminal_watch_fire(&job).await.expect("plan"),
        super::WatchFirePlan::NotReady,
        "fired-but-unconsumed retry must suppress the fire"
    );

    // Restart shape: eligibility persisted, timer gone.
    manager.completed.write().await.remove(&child);
    match manager.plan_terminal_watch_fire(&job).await.expect("plan") {
        super::WatchFirePlan::Deliver { message, .. } => {
            assert!(message.contains("retry-eligible: true"));
        }
        other => panic!("expected Deliver, got {other:?}"),
    }

    // Exhausted retries: fire annotated false.
    let mut exhausted = failed.clone();
    exhausted.retry_attempt = Some(2);
    manager
        .store
        .lock()
        .await
        .update_retry_state(child, Some(2), Some(2))
        .expect("update retry state");
    let _ = exhausted;
    match manager.plan_terminal_watch_fire(&job).await.expect("plan") {
        super::WatchFirePlan::Deliver { message, .. } => {
            assert!(message.contains("retry-eligible: false"));
        }
        other => panic!("expected Deliver, got {other:?}"),
    }
}

/// D5: a `WaitingApproval` child plans an annotated delivery.
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn waiting_approval_fires_watch() {
    let (manager, _dir) = manager();
    let child = Uuid::new_v4();
    let master = Uuid::new_v4();
    let mut waiting = bare_session(child);
    waiting.status = SessionStatus::WaitingApproval;
    insert_row(&manager, &waiting).await;
    insert_row(&manager, &bare_session(master)).await;

    let job = mk_watch_job_for(child, master, "");
    match manager.plan_terminal_watch_fire(&job).await.expect("plan") {
        super::WatchFirePlan::Deliver { message, .. } => {
            assert!(message.contains("question-pending: true"));
            assert!(message.contains("WaitingApproval"));
        }
        other => panic!("expected Deliver, got {other:?}"),
    }
}

/// Insert one conversation event for a session at an exact instant, so the
/// delivery-confirmation gate can be driven deterministically.
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn push_event(
    manager: &SessionManager,
    session_id: Uuid,
    sequence: i32,
    event_type: EventType,
    role: Option<Role>,
    created_at: chrono::DateTime<chrono::Utc>,
) {
    let event = ConversationEvent {
        id: 0,
        session_id,
        sequence,
        event_type,
        role,
        content: String::new(),
        tool_name: None,
        tool_input: None,
        created_at,
        offload_id: None,
        tool_use_id: None,
        metadata: None,
    };
    manager
        .store
        .lock()
        .await
        .insert_event(&event)
        .expect("insert event");
}

/// Issue #12: the delivery-confirmation gate.
///
/// A watch row is retired only on positive evidence that the resumed tip
/// actually ran. This pins the three cases that used to be indistinguishable
/// once the row was disabled at spawn time:
///
/// 1. A turn that wrote only `System` noise plus the injected prompt — exactly
///    what a turn that dies before emitting anything leaves behind — must NOT
///    confirm. That was the case that permanently destroyed the notification.
/// 2. Provider output that PREDATES the attempt must not confirm either, or any
///    session with prior history would self-confirm forever.
/// 3. Provider output dated after the attempt confirms and retires the row.
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn watch_delivery_confirms_only_on_post_dispatch_provider_output() {
    let (manager, _dir) = manager();
    let master = Uuid::new_v4();
    let child = Uuid::new_v4();
    insert_row(&manager, &bare_session(master)).await;
    insert_row(&manager, &bare_session(child)).await;

    let mut job = mk_watch_job_for(child, master, "note");
    let fired_at = chrono::Utc::now();
    job.last_fired_at = Some(fired_at);
    let at = |ms: i64| fired_at + chrono::Duration::milliseconds(ms);

    // Case 2 first: assistant output from BEFORE the dispatch is already on the
    // record. A stale row like this must not be mistaken for a fresh response.
    push_event(
        &manager,
        master,
        1,
        EventType::Message,
        Some(Role::Assistant),
        at(-5_000),
    )
    .await;
    // Case 1: the dispatched turn produced only transport noise and the
    // daemon-injected prompt.
    push_event(&manager, master, 2, EventType::System, None, at(100)).await;
    push_event(
        &manager,
        master,
        3,
        EventType::Message,
        Some(Role::User),
        at(200),
    )
    .await;

    match manager.plan_terminal_watch_fire(&job).await.expect("plan") {
        super::WatchFirePlan::Deliver { .. } => {}
        other => panic!("a turn that produced no provider output must re-deliver, got {other:?}"),
    }

    // Case 3: the tip finally emits provider-authored output after the attempt.
    push_event(
        &manager,
        master,
        4,
        EventType::Message,
        Some(Role::Assistant),
        at(300),
    )
    .await;

    assert_eq!(
        manager.plan_terminal_watch_fire(&job).await.expect("plan"),
        super::WatchFirePlan::Confirmed,
        "post-dispatch provider output must retire the watch"
    );
}

/// Issue #12: an un-consumable notification must fail loudly rather than
/// re-billing a provider turn forever. Past the give-up window the gate
/// abandons, which disables the row AND surfaces a `SystemMessage`.
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn watch_delivery_gives_up_loudly_after_the_bound() {
    let (manager, _dir) = manager();
    let master = Uuid::new_v4();
    let child = Uuid::new_v4();
    insert_row(&manager, &bare_session(master)).await;

    // The watched child's terminal row is the give-up anchor; age it past the
    // bound so retries cannot slide the deadline forward.
    let mut stale_child = bare_session(child);
    stale_child.updated_at =
        chrono::Utc::now() - super::WATCH_DELIVERY_GIVE_UP_AFTER - chrono::Duration::minutes(1);
    insert_row(&manager, &stale_child).await;

    let mut job = mk_watch_job_for(child, master, "note");
    job.last_fired_at = Some(chrono::Utc::now());

    match manager.plan_terminal_watch_fire(&job).await.expect("plan") {
        super::WatchFirePlan::AbandonUnconsumed(delivery) => {
            let reason = delivery.reason();
            assert!(
                reason.contains("never consumed"),
                "give-up must name the real failure; got: {reason}"
            );
        }
        other => panic!("expected Abandon past the give-up bound, got {other:?}"),
    }
}

/// T-6 (session half): two fire-ready children of one master coalesce into
/// ONE delivery listing both, with both job ids satisfied.
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn two_terminal_children_one_master_single_delivery() {
    let (manager, _dir) = manager();
    let master = Uuid::new_v4();
    let child_a = Uuid::new_v4();
    let child_b = Uuid::new_v4();
    insert_row(&manager, &bare_session(master)).await;
    insert_row(&manager, &bare_session(child_a)).await;
    insert_row(&manager, &bare_session(child_b)).await;

    let job_a = mk_watch_job_for(child_a, master, "note-a");
    let job_b = mk_watch_job_for(child_b, master, "note-b");
    {
        let guard = manager.store.lock().await;
        guard.insert_scheduled_job(&job_a).expect("insert a");
        guard.insert_scheduled_job(&job_b).expect("insert b");
    }

    match manager
        .plan_terminal_watch_fire(&job_a)
        .await
        .expect("plan")
    {
        super::WatchFirePlan::Deliver {
            tip,
            message,
            job_ids,
            ..
        } => {
            assert_eq!(tip, master);
            assert_eq!(job_ids.len(), 2);
            assert!(job_ids.contains(&job_a.id) && job_ids.contains(&job_b.id));
            assert_eq!(message.lines().count(), 2, "one line per child");
            assert!(message.contains(&child_a.to_string()[..8]));
            assert!(message.contains(&child_b.to_string()[..8]));
            assert!(message.contains("note-a") && message.contains("note-b"));
        }
        other => panic!("expected Deliver, got {other:?}"),
    }

    // A still-running sibling must NOT join the batch.
    let child_c = Uuid::new_v4();
    let mut running = bare_session(child_c);
    running.status = SessionStatus::Running;
    insert_row(&manager, &running).await;
    let job_c = mk_watch_job_for(child_c, master, "");
    manager
        .store
        .lock()
        .await
        .insert_scheduled_job(&job_c)
        .expect("insert c");
    match manager
        .plan_terminal_watch_fire(&job_a)
        .await
        .expect("plan")
    {
        super::WatchFirePlan::Deliver { job_ids, .. } => {
            assert!(
                !job_ids.contains(&job_c.id),
                "running child must stay armed"
            );
        }
        other => panic!("expected Deliver, got {other:?}"),
    }
}

#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn new_child_delivery_coalesces_only_unconsumed_siblings() {
    let (manager, _dir) = manager();
    let master = Uuid::new_v4();
    let children: Vec<_> = (0..3).map(|_| Uuid::new_v4()).collect();
    insert_row(&manager, &bare_session(master)).await;
    for child in &children {
        insert_row(&manager, &bare_session(*child)).await;
    }
    let jobs: Vec<_> = children
        .iter()
        .map(|child| mk_watch_job_for(*child, master, "completion"))
        .collect();
    let delivered_at = chrono::Utc::now() - chrono::Duration::seconds(5);
    {
        let store = manager.store.lock().await;
        for job in &jobs {
            store.insert_scheduled_job(job).unwrap();
        }
        store
            .update_scheduled_job_fired(&jobs[1].id, &delivered_at, None, true)
            .unwrap();
    }
    push_event(
        &manager,
        master,
        1,
        EventType::Message,
        Some(Role::Assistant),
        chrono::Utc::now(),
    )
    .await;

    let super::WatchFirePlan::Deliver { job_ids, .. } =
        manager.plan_terminal_watch_fire(&jobs[2]).await.unwrap()
    else {
        panic!("new child completion must still reach its owner");
    };
    assert_eq!(job_ids.len(), 2);
    assert!(job_ids.contains(&jobs[0].id));
    assert!(job_ids.contains(&jobs[2].id));
}

/// T-8: delivery targets the rotation-lineage TIP of the wake target, with
/// the chase depth-capped.
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn fire_targets_rotation_lineage_tip() {
    let (manager, _dir) = manager();
    let child = Uuid::new_v4();
    insert_row(&manager, &bare_session(child)).await;

    // Chain m0 <- m1 <- ... <- m9 (continued_from), strictly increasing
    // created_at so successor picks the next hop deterministically.
    let base = chrono::Utc::now();
    let mut chain: Vec<Uuid> = Vec::new();
    for i in 0..10u32 {
        let id = Uuid::new_v4();
        let mut s = bare_session(id);
        s.created_at = base + chrono::Duration::seconds(i64::from(i));
        s.continued_from = chain.last().copied();
        insert_row(&manager, &s).await;
        chain.push(id);
    }

    let job = mk_watch_job_for(child, chain[0], "");
    match manager.plan_terminal_watch_fire(&job).await.expect("plan") {
        super::WatchFirePlan::Deliver { tip, .. } => {
            assert_eq!(
                tip, chain[8],
                "lineage chase must stop at the depth cap (8 hops)"
            );
        }
        other => panic!("expected Deliver, got {other:?}"),
    }

    // Short chain resolves the true tip.
    let job_tail = mk_watch_job_for(child, chain[7], "");
    match manager
        .plan_terminal_watch_fire(&job_tail)
        .await
        .expect("plan")
    {
        super::WatchFirePlan::Deliver { tip, .. } => assert_eq!(tip, chain[9]),
        other => panic!("expected Deliver, got {other:?}"),
    }
}

#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn terminal_watch_follows_rotation_tip() {
    let (manager, _dir) = manager();
    let origin = Uuid::new_v4();
    let tip = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let mut predecessor = bare_session(origin);
    predecessor.status = SessionStatus::Archived;
    let mut successor = bare_session(tip);
    successor.continued_from = Some(origin);
    successor.status = SessionStatus::Running;
    successor.handoff_filepath = Some("thoughts/shared/handoffs/own.md".into());
    insert_row(&manager, &predecessor).await;
    insert_row(&manager, &successor).await;
    insert_row(&manager, &bare_session(owner)).await;
    let job = mk_watch_job_for(origin, owner, "completion");
    manager
        .store
        .lock()
        .await
        .insert_scheduled_job(&job)
        .unwrap();
    assert_eq!(
        manager.plan_terminal_watch_fire(&job).await.unwrap(),
        super::WatchFirePlan::NotReady
    );
    assert!(
        manager
            .store
            .lock()
            .await
            .get_scheduled_job(&job.id)
            .unwrap()
            .unwrap()
            .enabled
    );
    successor.status = SessionStatus::Completed;
    manager
        .store
        .lock()
        .await
        .update_session_status(tip, successor.status)
        .unwrap();
    match manager.plan_terminal_watch_fire(&job).await.unwrap() {
        super::WatchFirePlan::Deliver { message, .. } => {
            assert!(message.contains(&format!("{origin}→{tip}")));
            assert!(message.contains("thoughts/shared/handoffs/own.md"));
        }
        other => panic!("expected delivery from completed tip: {other:?}"),
    }
}

/// §9 Q1 (Jake: INCLUDE): plain `Resume`-mode wakes chase rotation lineage
/// too — `resume_scheduled` on a rotated-away id acts on the live tip.
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn resume_scheduled_targets_rotation_lineage_tip() {
    use crate::issue_tracker::poller::SessionLauncher;

    let (manager, _dir) = manager();
    let old_master = Uuid::new_v4();
    let successor = Uuid::new_v4();
    insert_row(&manager, &bare_session(old_master)).await;
    let mut succ_row = bare_session(successor);
    succ_row.created_at = chrono::Utc::now() + chrono::Duration::seconds(1);
    succ_row.continued_from = Some(old_master);
    succ_row.status = SessionStatus::Running;
    insert_row(&manager, &succ_row).await;

    // Make the TIP active so the busy-check names it — proving the chase
    // remapped old_master -> successor before any continue attempt.
    let mut tracked_session = bare_session(successor);
    tracked_session.status = SessionStatus::Running;
    let tracked = TrackedSession::new_for_test(tracked_session);
    manager.active.write().await.insert(successor, tracked);

    let err = manager
        .resume_scheduled(old_master, "wake".to_string())
        .await
        .expect_err("active tip must reject the resume");
    let msg = err.to_string();
    assert!(
        msg.contains(&successor.to_string()),
        "busy-check must name the TIP, got: {msg}"
    );
    assert!(!msg.contains(&old_master.to_string()));

    // Capacity-only Resume carries due-slot identity, but it uses the same
    // rotation-tip resolver and reaches the same busy fence before admission.
    let err = manager
        .resume_capacity_scheduled(
            old_master,
            "capacity wake".to_string(),
            Uuid::new_v4(),
            chrono::Utc::now(),
        )
        .await
        .expect_err("active capacity tip must reject the resume");
    let msg = err.to_string();
    assert!(msg.contains(&successor.to_string()));
    assert!(!msg.contains(&old_master.to_string()));

    // Missing origin row -> SessionNotFound, not a fresh fallthrough.
    let missing = Uuid::new_v4();
    let err = manager
        .resume_scheduled(missing, "wake".to_string())
        .await
        .expect_err("missing origin must error");
    assert!(matches!(
        err,
        crate::error::DaemonError::SessionNotFound(id) if id == missing
    ));
}

/// T-3 (session half, D3): a busy master defers delivery — the fire outcome
/// is `NotReady` (requeue), never an error or a consumed wake.
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn busy_master_delivery_defers_to_not_ready() {
    let (manager, _dir) = manager();
    let child = Uuid::new_v4();
    let master = Uuid::new_v4();
    insert_row(&manager, &bare_session(child)).await;
    let mut master_row = bare_session(master);
    master_row.status = SessionStatus::Running;
    insert_row(&manager, &master_row).await;

    let mut tracked_session = bare_session(master);
    tracked_session.status = SessionStatus::Running;
    manager
        .active
        .write()
        .await
        .insert(master, TrackedSession::new_for_test(tracked_session));

    let job = mk_watch_job_for(child, master, "");
    let outcome = manager.fire_terminal_watch(&job).await.expect("fire");
    assert_eq!(
        outcome,
        crate::issue_tracker::poller::WatchFireOutcome::NotReady,
        "busy master must map to NotReady (row stays armed)"
    );
}

/// K2 design test 6 (child watch): a retryable fence refusal (busy tip)
/// keeps the watch armed and records durable retry state; at the bound the
/// watch is abandoned with a typed `continuation_retry_exhausted` naming the
/// tip, never retried forever.
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::large_futures)]
async fn child_watch_fence_refusal_records_retry_and_abandons_typed_at_bound() {
    use crate::store::manager_actions::fence::{
        CONTINUATION_RETRY_EXHAUSTED, CONTINUATION_TARGET_BUSY,
    };
    let (manager, _dir) = manager();
    let child = Uuid::new_v4();
    let master = Uuid::new_v4();
    insert_row(&manager, &bare_session(child)).await;
    let mut master_row = bare_session(master);
    master_row.status = SessionStatus::Running;
    insert_row(&manager, &master_row).await;
    let mut tracked_session = bare_session(master);
    tracked_session.status = SessionStatus::Running;
    manager
        .active
        .write()
        .await
        .insert(master, TrackedSession::new_for_test(tracked_session));
    let job = mk_watch_job_for(child, master, "");
    manager
        .store
        .lock()
        .await
        .insert_scheduled_job(&job)
        .unwrap();

    assert_eq!(
        manager.fire_terminal_watch(&job).await.expect("fire"),
        crate::issue_tracker::poller::WatchFireOutcome::NotReady
    );
    let retry = manager
        .store
        .lock()
        .await
        .continuation_retry(job.id)
        .unwrap()
        .expect("retry recorded");
    assert_eq!(
        (retry.attempts, retry.last_code.as_str()),
        (1, CONTINUATION_TARGET_BUSY)
    );
    assert_eq!(retry.last_tip, Some(master));
    for _ in 0..6 {
        assert_eq!(
            manager.fire_terminal_watch(&job).await.expect("fire"),
            crate::issue_tracker::poller::WatchFireOutcome::NotReady
        );
    }
    match manager.fire_terminal_watch(&job).await.expect("fire") {
        crate::issue_tracker::poller::WatchFireOutcome::Abandon { reason } => {
            assert!(reason.starts_with(CONTINUATION_RETRY_EXHAUSTED), "{reason}");
            assert!(reason.contains(&master.to_string()), "{reason}");
        }
        other => panic!("expected typed Abandon, got {other:?}"),
    }
}

/// Review round 2 `retry_exhaustion_crash_reset` (child watch): the eighth
/// retryable refusal returns the typed Abandon while the recorded attempts
/// stay durable until the scheduler's retirement commits. A daemon that dies
/// in between reopens to the same exhausted budget, re-abandons on the next
/// refusal, and the retirement then clears the state in its own write.
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::large_futures)]
async fn child_watch_retry_exhaustion_survives_crash_before_abandon_settlement() {
    use crate::store::manager_actions::fence::{
        CONTINUATION_RETRY_EXHAUSTED, CONTINUATION_TARGET_BUSY,
    };
    let (manager, dir) = manager();
    let child = Uuid::new_v4();
    let master = Uuid::new_v4();
    insert_row(&manager, &bare_session(child)).await;
    let mut master_row = bare_session(master);
    master_row.status = SessionStatus::Running;
    insert_row(&manager, &master_row).await;
    let mut tracked_session = bare_session(master);
    tracked_session.status = SessionStatus::Running;
    manager
        .active
        .write()
        .await
        .insert(master, TrackedSession::new_for_test(tracked_session));
    let job = mk_watch_job_for(child, master, "");
    manager
        .store
        .lock()
        .await
        .insert_scheduled_job(&job)
        .unwrap();
    for _ in 0..7 {
        assert_eq!(
            manager.fire_terminal_watch(&job).await.expect("fire"),
            crate::issue_tracker::poller::WatchFireOutcome::NotReady
        );
    }
    let exhausted = |outcome| match outcome {
        crate::issue_tracker::poller::WatchFireOutcome::Abandon { reason } => {
            assert!(reason.starts_with(CONTINUATION_RETRY_EXHAUSTED), "{reason}");
        }
        other => panic!("expected typed Abandon, got {other:?}"),
    };
    exhausted(manager.fire_terminal_watch(&job).await.expect("fire"));

    // Crash point: the scheduler's Abandon retirement never ran. Reopen.
    let reopened = Store::open(&dir.path().join("rsi.db")).expect("reopen store");
    let row = reopened
        .get_scheduled_job(&job.id)
        .unwrap()
        .expect("watch row kept");
    assert!(row.enabled);
    let retry = reopened
        .continuation_retry(job.id)
        .unwrap()
        .expect("attempts stay durable until the terminal write");
    assert_eq!(
        (retry.attempts, retry.last_code.as_str()),
        (7, CONTINUATION_TARGET_BUSY)
    );

    // No fresh budget: the next refusal re-exhausts, and the retirement
    // clears the retry state together with disabling the row.
    exhausted(manager.fire_terminal_watch(&job).await.expect("fire"));
    assert!(
        reopened
            .retire_unchanged_child_watch(&row, "abandoned")
            .unwrap()
    );
    let settled = reopened
        .get_scheduled_job(&job.id)
        .unwrap()
        .expect("watch row kept");
    assert!(!settled.enabled);
    assert!(settled.last_fired_at.is_some());
    assert_eq!(reopened.continuation_retry(job.id).unwrap(), None);
}

/// T-10 (session half): missing watched session or missing wake target maps
/// to Abandon — never NotReady-forever, never a fresh launch.
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn missing_watched_or_master_abandons() {
    let (manager, _dir) = manager();
    let master = Uuid::new_v4();
    insert_row(&manager, &bare_session(master)).await;

    // Watched row missing.
    let job = mk_watch_job_for(Uuid::new_v4(), master, "");
    match manager.fire_terminal_watch(&job).await.expect("fire") {
        crate::issue_tracker::poller::WatchFireOutcome::Abandon { reason } => {
            assert!(reason.contains("watched session"));
        }
        other => panic!("expected Abandon, got {other:?}"),
    }

    // Wake target row missing.
    let child = Uuid::new_v4();
    insert_row(&manager, &bare_session(child)).await;
    let job = mk_watch_job_for(child, Uuid::new_v4(), "");
    match manager.fire_terminal_watch(&job).await.expect("fire") {
        crate::issue_tracker::poller::WatchFireOutcome::Abandon { reason } => {
            assert!(reason.contains("wake target"));
        }
        other => panic!("expected Abandon, got {other:?}"),
    }
}

/// A9.1 (D7 / F-004): the bounded retry dispatcher must (a) never run more than
/// `concurrency` handlers at once and (b) let quick retries finish even while a
/// hung handler is still blocked — the liveness property the old serial loop
/// lacked.
#[tokio::test]
async fn bounded_retry_dispatch_caps_concurrency_and_survives_hung_handler() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    let (tx, rx) = mpsc::channel::<Uuid>(64);
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    // The first id "hangs" for far longer than the quick ones. A serial handler
    // would block every quick retry behind it; bounded concurrency must not.
    let hung_id = Uuid::from_u128(1);

    let in_flight_h = Arc::clone(&in_flight);
    let max_h = Arc::clone(&max_in_flight);
    let done_h = Arc::clone(&completed);
    let dispatcher = tokio::spawn(run_bounded_retry_dispatch(rx, 4, move |id| {
        let in_flight = Arc::clone(&in_flight_h);
        let max = Arc::clone(&max_h);
        let done = Arc::clone(&done_h);
        async move {
            let cur = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            max.fetch_max(cur, Ordering::SeqCst);
            let hang = id == hung_id;
            tokio::time::sleep(Duration::from_millis(if hang { 400 } else { 10 })).await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
            done.fetch_add(1, Ordering::SeqCst);
        }
    }));

    tx.send(hung_id).await.unwrap();
    for i in 2..=6u128 {
        tx.send(Uuid::from_u128(i)).await.unwrap();
    }
    drop(tx); // close the channel so the dispatch loop ends after draining

    // While the hung handler is still sleeping, the 5 quick ones must complete.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        completed.load(Ordering::SeqCst) >= 5,
        "quick retries must not be blocked behind a hung retry (got {})",
        completed.load(Ordering::SeqCst)
    );

    dispatcher.await.unwrap();
    // Drain the remaining (hung) task.
    for _ in 0..50 {
        if completed.load(Ordering::SeqCst) == 6 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(completed.load(Ordering::SeqCst), 6, "all retries must run");
    assert!(
        max_in_flight.load(Ordering::SeqCst) <= 4,
        "in-flight concurrency must stay bounded (peaked at {})",
        max_in_flight.load(Ordering::SeqCst)
    );
}

/// Seed `active` with a Running session so `continue_session` takes the
/// interrupt-then-wait branch (`is_active == true`), which no other test in
/// this file exercises — every other `continue_session` test seeds `completed`
/// and therefore skips the wait entirely.
async fn seed_active_running(manager: &SessionManager, session_id: Uuid) {
    let mut session = bare_session(session_id);
    session.status = SessionStatus::Running;
    manager
        .active
        .write()
        .await
        .insert(session_id, TrackedSession::new_for_test(session));
}

/// Publish the terminal transition the interrupt-wait branch is blocking on,
/// and stage the matching `completed` entry the branch consumes right after.
async fn finalize_active_session(manager: &SessionManager, session_id: Uuid) {
    manager.active.write().await.remove(&session_id);
    let mut session = bare_session(session_id);
    session.status = SessionStatus::Interrupted;
    manager
        .completed
        .write()
        .await
        .insert(session_id, CompletedSession::for_test(session));
    manager
        .event_bus
        .publish(DaemonEvent::SessionStatusChanged {
            session_id,
            old_status: SessionStatus::Running,
            new_status: SessionStatus::Interrupted,
        });
}

/// Regression: a *correct but slow* teardown must not be reported as a failure.
///
/// The teardown worst case is `TEARDOWN_TERMINAL_WORST_CASE` (~8.4s: two
/// settlement attempts, each spending `TEARDOWN_KILL_GRACE` twice). The old
/// hardcoded 10s deadline sat barely above that, so jitter on a loaded machine
/// tipped a legitimate teardown into a spurious "Timeout waiting for active
/// session to finalize" error. Here the terminal event lands past the OLD
/// deadline but inside the new one; `continue_session` must proceed to launch
/// (and fail there, for want of a provider binary) rather than time out.
///
/// `start_paused` means tokio auto-advances virtual time whenever every task is
/// parked on a timer, so this runs instantly despite the multi-second waits.
#[tokio::test(start_paused = true)]
async fn continue_tolerates_teardown_slower_than_the_old_ten_second_deadline() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    seed_active_running(&manager, session_id).await;

    let late = super::reaper::TEARDOWN_TERMINAL_WORST_CASE + std::time::Duration::from_secs(2);
    assert!(
        late > std::time::Duration::from_secs(10),
        "fixture must land past the old 10s deadline to be a regression test (was {late:?})"
    );
    assert!(
        late < SessionManager::CONTINUE_INTERRUPT_WAIT,
        "fixture must land inside the new deadline"
    );

    let (result, ()) = tokio::join!(
        manager.continue_session(session_id, "keep my typed prompt".to_string()),
        async {
            tokio::time::sleep(late).await;
            finalize_active_session(&manager, session_id).await;
        }
    );

    let err = result.expect_err("launch fails without a provider binary");
    let msg = err.to_string();
    assert!(
        !msg.contains("did not finalize"),
        "a teardown inside the deadline must not surface as a timeout; got: {msg}"
    );
}

/// The deadline still fires when the terminal transition never arrives — the
/// dominant real cause being `ProcessSettlementOutcome::EscalationFailed`, which
/// publishes no terminal status at all, leaving this timeout as the only signal
/// the caller ever receives. It must stay loud and must not be swallowed by the
/// wider wait.
#[tokio::test(start_paused = true)]
async fn continue_still_times_out_when_no_terminal_status_ever_arrives() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    seed_active_running(&manager, session_id).await;

    let started = tokio::time::Instant::now();
    let err = manager
        .continue_session(session_id, "query".to_string())
        .await
        .expect_err("no terminal status ever published -> must time out");

    let waited = started.elapsed();
    assert!(
        waited >= SessionManager::CONTINUE_INTERRUPT_WAIT,
        "must wait the full deadline before giving up (waited {waited:?})"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("did not finalize") && msg.contains("wedged"),
        "timeout must name unsettled provider ownership, not read as a transient wait; got: {msg}"
    );
}

/// Regression (issue #13): a `continue_session` arriving while the per-session
/// spawn guard is held must NOT adopt the live child and discard the caller's
/// query.
///
/// Pre-fix, `contended() && adopt_if_live(..)` returned `Ok(())` here without
/// ever using `query` — no conversation event, no provider turn, and a success
/// reply to the caller. Both signals are satisfied by ordinary traffic:
/// `contended()` only means the lock was held or queued, and a live child in
/// `active` is the normal state of every `Running` session. So the shortcut fired
/// on a plain concurrent continue, not just on a genuine twin spawn.
///
/// The fixture pins the *observable* difference: adopting reports success,
/// whereas proceeding reaches the interrupt-then-respawn path, whose only
/// outcome with no monitor to publish a terminal status is the interrupt-wait
/// deadline. Asserting on that error proves the query was not short-circuited
/// away.
#[tokio::test(start_paused = true)]
async fn contended_continue_does_not_silently_drop_the_query() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();

    // A LIVE tracked child: exactly what `adopt_if_live` keys on.
    manager
        .active
        .write()
        .await
        .insert(session_id, fake_alive_tracked(session_id).await);

    // A pure holder forces the contended branch without spawning anything.
    // `join!` polls left-to-right, so the holder wins the guard first.
    let ((), result) = tokio::join!(
        async {
            let g = super::spawn_single_flight::acquire_spawn_guard(session_id).await;
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            drop(g);
        },
        manager.continue_session(session_id, "keep my typed prompt".to_string()),
    );

    let err = result.expect_err("a contended continue must not report success by adopting");
    assert!(
        err.to_string().contains("did not finalize"),
        "must have proceeded into the interrupt-and-wait path rather than adopting; got: {err}"
    );
}

// Deliberately NOT tested here: "the caller's query survives the interrupt wait".
//
// Once the adopt shortcut is gone (see the test directly above, which covers the
// one path that really did drop it), the daemon owns the query as a plain
// `String` parameter for the rest of the function, so it cannot lose it across
// the wait — and there is no observable trace of it when the subsequent spawn
// fails: a failed launch persists neither a conversation turn nor an updated
// `Session.query` (verified empirically; both readbacks come back as the
// pre-continue values). Asserting on either would be a test of the rollback,
// dressed up as a test of query preservation.
//
// The real loss was TUI-side: the input surface was cleared before the RPC and
// never restored when it failed. That is covered where it actually happens, by
// the `restores_*_on_failed_continue` tests in `crates/rsi`.

#[test]
fn terminal_capacity_monitor_skips_c5_only_for_committed_owned_or_absent_markers() {
    use crate::session::agent_verbs::{MasterNoIdleOutcome, MasterNoIdleRecoveryDisposition};
    use crate::store::capacity_recovery::{
        CapacityC5Resolution, CapacityCommitKind, CapacityFailureSettlement,
        CapacityIssueDisposition, CapacityTerminalSettlement,
    };

    let incident = Uuid::new_v4();
    let wake = Uuid::new_v4();
    let due = chrono::Utc::now();
    for resolution in [
        CapacityC5Resolution::ResolvedExact,
        CapacityC5Resolution::AlreadyAbsent,
        CapacityC5Resolution::Unrelated,
    ] {
        let failure = MasterNoIdleOutcome::CapacityRecovered {
            settlement: CapacityFailureSettlement {
                commit_kind: CapacityCommitKind::New,
                incident_id: incident,
                wake_job_id: wake,
                outage_epoch: 1,
                backoff_bucket: 1,
                due_slot: due,
                issue_disposition: CapacityIssueDisposition::Projectless,
                c5_resolution: resolution,
            },
        };
        let terminal = MasterNoIdleOutcome::TerminalAllowed {
            capacity: Some(CapacityTerminalSettlement {
                commit_kind: CapacityCommitKind::Replay,
                incident_id: incident,
                wake_job_id: wake,
                issue_disposition: CapacityIssueDisposition::Projectless,
                c5_resolution: resolution,
            }),
        };
        let expected = resolution != CapacityC5Resolution::Unrelated;
        assert_eq!(
            super::monitor::no_idle_outcome_owns_capacity_c5(Some(&failure)),
            expected
        );
        assert_eq!(
            super::monitor::no_idle_outcome_owns_capacity_c5(Some(&terminal)),
            expected
        );
    }

    for ordinary in [
        MasterNoIdleOutcome::NotApplicable,
        MasterNoIdleOutcome::OrdinaryGuardPresent,
        MasterNoIdleOutcome::GenericRecovered {
            wake_job_id: wake,
            disposition: MasterNoIdleRecoveryDisposition::ProjectlessWakeOnly,
        },
        MasterNoIdleOutcome::TerminalAllowed { capacity: None },
    ] {
        assert!(!super::monitor::no_idle_outcome_owns_capacity_c5(Some(
            &ordinary
        )));
    }
    assert!(!super::monitor::no_idle_outcome_owns_capacity_c5(None));
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn due_capacity_resume_manager() -> (
    std::sync::Arc<SessionManager>,
    TempDir,
    Uuid,
    crate::store::capacity_recovery::CapacityFailureSettlement,
    rsi_common::types::ScheduledJob,
) {
    let (manager, directory) = manager();
    let manager = std::sync::Arc::new(manager);
    let controller = Uuid::new_v4();
    let mut session = bare_session(controller);
    session.provider = SessionProvider::Codex;
    session.model = Some("gpt-5.4".into());
    session.claude_session_id = Some("capacity-failure-thread".into());
    session.status = SessionStatus::Failed;
    session.stop_reason = Some("provider_error:codex_usage_limit".into());
    session.working_dir = directory.path().to_path_buf();
    insert_row(&manager, &session).await;
    manager
        .completed
        .write()
        .await
        .insert(controller, CompletedSession::for_test(session.clone()));

    let guard_job = crate::session::harness::tools::schedule_wake::build_agent_scheduled_job(
        crate::session::harness::tools::schedule_wake::ScheduleWakeRequest {
            message: "guard".into(),
            in_seconds: None,
            at: None,
            name: None,
            every_seconds: None,
            mode: Some("program_guard".into()),
            working_dir: session.working_dir.clone(),
            provider: Some(SessionProvider::Codex),
            model: session.model.clone(),
            project_id: None,
            origin_session_id: Some(controller),
            watch_session_id: None,
        },
    )
    .unwrap();
    let invocation = Uuid::new_v4();
    let terminal_at = chrono::Utc::now() - chrono::Duration::seconds(70);
    let timestamp = terminal_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let recovery = {
        let store = manager.store.lock().await;
        store.insert_scheduled_job(&guard_job).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO model_invocations(
                    id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                    provider,model,backend,trigger_source,session_id,policy_snapshot_json,
                    usage_confidence,created_at,completed_at
                 ) VALUES(?1,'session_continue','session_lifecycle','foreground','paid_capable',
                          'admitted','failed','Codex','gpt-5.4','Codex','capacity-failure-fixture',
                          ?2,'{}','unavailable',?3,?3)",
                rusqlite::params![invocation.to_string(), controller.to_string(), timestamp],
            )
            .unwrap();
        store
            .set_session_model_invocation(controller, Some(invocation))
            .unwrap();
        store
            .settle_capacity_failure(
                controller,
                controller,
                guard_job.id,
                invocation,
                1,
                terminal_at,
            )
            .unwrap()
    };
    let due_job = manager
        .store
        .lock()
        .await
        .get_scheduled_job(&recovery.wake_job_id)
        .unwrap()
        .unwrap();
    assert!(recovery.due_slot <= chrono::Utc::now());
    (manager, directory, controller, recovery, due_job)
}

#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn capacity_resume_checked_reaper_failure_keeps_admission_and_authorizes_zero_launches() {
    use crate::issue_tracker::poller::SessionLauncher;
    use std::sync::atomic::Ordering;

    let (manager, _directory, controller, recovery, due_job) = due_capacity_resume_manager().await;
    let scripted = super::launch::install_controller_candidate_test_process(controller);
    super::reaper::fail_next_capacity_orphan_reap();
    let launcher: std::sync::Arc<dyn SessionLauncher> = manager.clone();
    crate::scheduler::fire_job_for_test(&manager.store, manager.event_bus(), &launcher, &due_job)
        .await;

    assert_eq!(scripted.productive_start_count.load(Ordering::SeqCst), 0);
    assert!(!manager.active.read().await.contains_key(&controller));
    assert!(manager.completed.read().await.contains_key(&controller));
    let store = manager.store.lock().await;
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT state FROM master_no_idle_capacity_attempts
                 WHERE delivery_wake_job_id=?1 AND delivery_due_slot=?2",
                rusqlite::params![
                    recovery.wake_job_id.to_string(),
                    recovery
                        .due_slot
                        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "delivery_admitted"
    );
    assert!(
        store
            .get_scheduled_job(&recovery.wake_job_id)
            .unwrap()
            .unwrap()
            .enabled
    );
    drop(store);
    super::launch::drop_controller_candidate_test_process(controller);
}

#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn capacity_resume_provider_spawn_failure_keeps_admission_and_authorizes_zero_launches() {
    use crate::issue_tracker::poller::SessionLauncher;
    use std::sync::atomic::Ordering;

    let (manager, _directory, controller, recovery, due_job) = due_capacity_resume_manager().await;
    let scripted = super::launch::install_controller_candidate_test_process(controller);
    super::launch::fail_next_capacity_provider_spawn(controller);
    let launcher: std::sync::Arc<dyn SessionLauncher> = manager.clone();
    crate::scheduler::fire_job_for_test(&manager.store, manager.event_bus(), &launcher, &due_job)
        .await;

    assert_eq!(scripted.productive_start_count.load(Ordering::SeqCst), 0);
    assert!(!manager.active.read().await.contains_key(&controller));
    assert!(manager.completed.read().await.contains_key(&controller));
    let store = manager.store.lock().await;
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT state FROM master_no_idle_capacity_attempts
                 WHERE delivery_wake_job_id=?1 AND delivery_due_slot=?2",
                rusqlite::params![
                    recovery.wake_job_id.to_string(),
                    recovery
                        .due_slot
                        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "delivery_admitted"
    );
    assert!(
        store
            .get_scheduled_job(&recovery.wake_job_id)
            .unwrap()
            .unwrap()
            .enabled
    );
    drop(store);
    super::launch::drop_controller_candidate_test_process(controller);
}

#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn capacity_resume_confirmation_failure_kills_unconfirmed_process_and_keeps_due_slot() {
    use crate::issue_tracker::poller::SessionLauncher;
    use std::sync::atomic::Ordering;

    let (manager, _directory, controller, recovery, due_job) = due_capacity_resume_manager().await;
    let scripted = super::launch::install_controller_candidate_test_process(controller);
    crate::store::capacity_recovery::test_fail_next_launch_confirmation();
    let launcher: std::sync::Arc<dyn SessionLauncher> = manager.clone();
    crate::scheduler::fire_job_for_test(&manager.store, manager.event_bus(), &launcher, &due_job)
        .await;

    assert_eq!(scripted.productive_start_count.load(Ordering::SeqCst), 1);
    assert!(!scripted.alive.load(Ordering::SeqCst));
    assert!(!manager.active.read().await.contains_key(&controller));
    assert!(manager.completed.read().await.contains_key(&controller));
    let store = manager.store.lock().await;
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT state FROM master_no_idle_capacity_attempts
                 WHERE delivery_wake_job_id=?1 AND delivery_due_slot=?2",
                rusqlite::params![
                    recovery.wake_job_id.to_string(),
                    recovery
                        .due_slot
                        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "delivery_admitted"
    );
    assert!(
        store
            .get_scheduled_job(&recovery.wake_job_id)
            .unwrap()
            .unwrap()
            .enabled
    );
    drop(store);
    super::launch::drop_controller_candidate_test_stream(controller);
}

#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn capacity_resume_crash_after_admission_before_spawn_reopens_to_one_productive_launch() {
    use crate::issue_tracker::poller::SessionLauncher;
    use std::sync::atomic::Ordering;

    let directory = TempDir::new().unwrap();
    let database = directory.path().join("capacity-crash-reopen.sqlite");
    let build_manager = || {
        let config = Config::from_env();
        SessionManager::new(
            std::sync::Arc::new(EventBus::new(16)),
            Store::open(&database).expect("open file-backed capacity Store"),
            false,
            directory.path().join("daemon.sock"),
            None,
            Vec::new(),
            RuntimeConfig::from_config(&config),
            directory.path().join("sandboxes"),
        )
        .expect("construct file-backed SessionManager")
    };

    let manager = std::sync::Arc::new(build_manager());
    let controller = Uuid::new_v4();
    let mut session = bare_session(controller);
    session.provider = SessionProvider::Codex;
    session.model = Some("gpt-5.4".into());
    session.claude_session_id = Some("capacity-crash-thread".into());
    session.status = SessionStatus::Failed;
    session.stop_reason = Some("provider_error:codex_usage_limit".into());
    session.working_dir = directory.path().to_path_buf();
    insert_row(&manager, &session).await;
    manager
        .completed
        .write()
        .await
        .insert(controller, CompletedSession::for_test(session.clone()));

    let guard_job = crate::session::harness::tools::schedule_wake::build_agent_scheduled_job(
        crate::session::harness::tools::schedule_wake::ScheduleWakeRequest {
            message: "guard".into(),
            in_seconds: None,
            at: None,
            name: None,
            every_seconds: None,
            mode: Some("program_guard".into()),
            working_dir: session.working_dir.clone(),
            provider: Some(SessionProvider::Codex),
            model: session.model.clone(),
            project_id: None,
            origin_session_id: Some(controller),
            watch_session_id: None,
        },
    )
    .expect("build program sentinel");
    let terminal_invocation = Uuid::new_v4();
    let terminal_at = chrono::Utc::now() - chrono::Duration::seconds(70);
    let timestamp = terminal_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let recovery = {
        let store = manager.store.lock().await;
        store.insert_scheduled_job(&guard_job).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO model_invocations(
                    id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                    provider,model,backend,trigger_source,session_id,policy_snapshot_json,
                    usage_confidence,created_at,completed_at
                 ) VALUES(?1,'session_continue','session_lifecycle','foreground','paid_capable',
                          'admitted','failed','Codex','gpt-5.4','Codex','capacity-crash-fixture',
                          ?2,'{}','unavailable',?3,?3)",
                rusqlite::params![
                    terminal_invocation.to_string(),
                    controller.to_string(),
                    timestamp
                ],
            )
            .unwrap();
        store
            .set_session_model_invocation(controller, Some(terminal_invocation))
            .unwrap();
        store
            .settle_capacity_failure(
                controller,
                controller,
                guard_job.id,
                terminal_invocation,
                1,
                terminal_at,
            )
            .unwrap()
    };
    let due_job = manager
        .store
        .lock()
        .await
        .get_scheduled_job(&recovery.wake_job_id)
        .unwrap()
        .unwrap();
    assert!(recovery.due_slot <= chrono::Utc::now());

    let scripted = super::launch::install_controller_candidate_test_process(controller);
    super::launch::fail_after_capacity_admission_before_provider_spawn(controller);
    let launcher: std::sync::Arc<dyn SessionLauncher> = manager.clone();
    crate::scheduler::fire_job_for_test(&manager.store, manager.event_bus(), &launcher, &due_job)
        .await;
    assert_eq!(scripted.productive_start_count.load(Ordering::SeqCst), 0);
    let (admitted_invocation, admitted_state): (String, String) = manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT model_invocation_id,state FROM master_no_idle_capacity_attempts
             WHERE delivery_wake_job_id=?1 AND delivery_due_slot=?2",
            rusqlite::params![
                recovery.wake_job_id.to_string(),
                recovery
                    .due_slot
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(admitted_state, "delivery_admitted");
    assert!(
        manager
            .store
            .lock()
            .await
            .get_scheduled_job(&recovery.wake_job_id)
            .unwrap()
            .unwrap()
            .enabled
    );
    drop(launcher);
    drop(manager);

    let reopened = std::sync::Arc::new(build_manager());
    reopened.restore_sessions().await.unwrap();
    let launcher: std::sync::Arc<dyn SessionLauncher> = reopened.clone();
    let persisted = reopened
        .store
        .lock()
        .await
        .get_scheduled_job(&recovery.wake_job_id)
        .unwrap()
        .unwrap();
    let mut replay_events = reopened.event_bus().subscribe();
    crate::scheduler::fire_job_for_test(
        &reopened.store,
        reopened.event_bus(),
        &launcher,
        &persisted,
    )
    .await;
    let mut replay_diagnostics = Vec::new();
    while let Ok(event) = replay_events.try_recv() {
        replay_diagnostics.push(format!("{event:?}"));
    }
    assert_eq!(
        scripted.productive_start_count.load(Ordering::SeqCst),
        1,
        "reopen fire diagnostics: {replay_diagnostics:?}"
    );
    assert!(reopened.active.read().await.contains_key(&controller));
    let (confirmed_invocation, confirmed_state): (String, String) = reopened
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT model_invocation_id,state FROM master_no_idle_capacity_attempts
             WHERE delivery_wake_job_id=?1 AND delivery_due_slot=?2",
            rusqlite::params![
                recovery.wake_job_id.to_string(),
                recovery
                    .due_slot
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(confirmed_invocation, admitted_invocation);
    assert_eq!(confirmed_state, "delivery_launch_confirmed");
    assert!(
        !reopened
            .store
            .lock()
            .await
            .get_scheduled_job(&recovery.wake_job_id)
            .unwrap()
            .unwrap()
            .enabled
    );

    crate::scheduler::fire_job_for_test(
        &reopened.store,
        reopened.event_bus(),
        &launcher,
        &persisted,
    )
    .await;
    assert_eq!(scripted.productive_start_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        reopened
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT count(*) FROM model_invocations WHERE id=?1",
                [admitted_invocation],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );

    super::launch::drop_controller_candidate_test_stream(controller);
    scripted.alive.store(false, Ordering::SeqCst);
}

#[test]
fn test_convert_stream_event_tool_result_array_content() {
    // Regression: the Claude Code CLI echoes tool results back as a "user"
    // event whose tool_result block has an ARRAY of content blocks. Ingest
    // used to require a string here and silently dropped every such result.
    let stream = StreamEvent {
        event_type: "user".to_string(),
        data: serde_json::json!({
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_1",
                        "content": [
                            {"type": "text", "text": "line one"},
                            {"type": "text", "text": "line two"}
                        ]
                    }
                ]
            }
        }),
    };
    let mut seq = 0;
    let events = SessionManager::convert_recognized_stream_event(&stream, Uuid::new_v4(), &mut seq)
        .expect("the fixture's event type must stay recognized by the converter");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, EventType::ToolResult);
    assert_eq!(events[0].content, "line one\nline two");
    assert_eq!(events[0].tool_use_id, Some("toolu_1".to_string()));
}

#[test]
fn test_convert_stream_event_tool_result_string_content_keeps_id() {
    // The pre-existing string shape still works, and now carries the join key.
    let stream = StreamEvent {
        event_type: "user".to_string(),
        data: serde_json::json!({
            "message": {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_9", "content": "ok"}
                ]
            }
        }),
    };
    let mut seq = 0;
    let events = SessionManager::convert_recognized_stream_event(&stream, Uuid::new_v4(), &mut seq)
        .expect("the fixture's event type must stay recognized by the converter");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, EventType::ToolResult);
    assert_eq!(events[0].content, "ok");
    assert_eq!(events[0].tool_use_id, Some("toolu_9".to_string()));
}

#[test]
fn test_convert_stream_event_tool_result_preserves_provider_error_flag() {
    for is_error in [true, false] {
        let claude = StreamEvent {
            event_type: "user".to_string(),
            data: serde_json::json!({
                "message": {"content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": "tool output",
                    "is_error": is_error,
                }]}
            }),
        };
        let top_level = StreamEvent {
            event_type: "tool_result".to_string(),
            data: serde_json::json!({"content": "tool output", "is_error": is_error}),
        };
        for stream in [&claude, &top_level] {
            let mut seq = 0;
            let events =
                SessionManager::convert_recognized_stream_event(stream, Uuid::new_v4(), &mut seq)
                    .expect("tool result stream is recognized");
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].event_type, EventType::ToolResult);
            assert_eq!(events[0].content, "tool output");
            assert_eq!(
                events[0].metadata.as_deref(),
                Some(&serde_json::json!({"is_error": is_error}))
            );
        }
    }

    let ordinary = StreamEvent {
        event_type: "tool_result".to_string(),
        data: serde_json::json!({"content": "ordinary output"}),
    };
    let mut seq = 0;
    let events =
        SessionManager::convert_recognized_stream_event(&ordinary, Uuid::new_v4(), &mut seq)
            .expect("ordinary tool result stream is recognized");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, EventType::ToolResult);
    assert_eq!(events[0].content, "ordinary output");
    assert_eq!(events[0].metadata, None);
}

#[test]
fn test_codex_command_execution_result_error_metadata() {
    for (exit_code, expected_metadata) in [
        (Some(0), Some(serde_json::json!({"is_error": false}))),
        (Some(7), Some(serde_json::json!({"is_error": true}))),
        (None, None),
    ] {
        let mut item = serde_json::json!({
            "type": "command_execution",
            "aggregated_output": "command output",
        });
        if let Some(exit_code) = exit_code {
            item["exit_code"] = serde_json::json!(exit_code);
        }
        let raw = serde_json::json!({"type": "item.completed", "item": item});
        let stream = crate::codex::map_codex_json_to_stream_event(&raw, &mut None)
            .expect("Codex command result is recognized");
        let mut seq = 0;
        let events =
            SessionManager::convert_recognized_stream_event(&stream, Uuid::new_v4(), &mut seq)
                .expect("top-level tool result is recognized");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::ToolResult);
        assert_eq!(events[0].content, "command output");
        assert_eq!(events[0].metadata.as_deref(), expected_metadata.as_ref());
    }
}

#[test]
fn test_convert_stream_event_tool_result_unparseable_is_loud() {
    // A shape we cannot model must produce a visible System event carrying the
    // raw block, never a silent drop.
    let stream = StreamEvent {
        event_type: "user".to_string(),
        data: serde_json::json!({
            "message": {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_5", "content": 42},
                    {"type": "tool_result", "tool_use_id": "toolu_6"}
                ]
            }
        }),
    };
    let mut seq = 0;
    let events = SessionManager::convert_recognized_stream_event(&stream, Uuid::new_v4(), &mut seq)
        .expect("the fixture's event type must stay recognized by the converter");
    assert_eq!(events.len(), 2);
    for ev in &events {
        assert_eq!(ev.event_type, EventType::System);
        assert!(ev.content.contains("[ingest]"), "content: {}", ev.content);
        assert!(ev.metadata.is_some());
    }
    assert_eq!(events[0].tool_use_id, Some("toolu_5".to_string()));
    assert!(events[0].content.contains("42"));
    assert_eq!(events[1].tool_use_id, Some("toolu_6".to_string()));
}

#[test]
fn test_convert_stream_event_tool_result_nontext_blocks_are_loud() {
    // Text blocks are captured AND the non-text block is reported, so nothing
    // in the payload vanishes.
    let stream = StreamEvent {
        event_type: "user".to_string(),
        data: serde_json::json!({
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_7",
                        "content": [
                            {"type": "text", "text": "see image"},
                            {"type": "image", "source": {"type": "base64", "data": "AAAA"}}
                        ]
                    }
                ]
            }
        }),
    };
    let mut seq = 0;
    let events = SessionManager::convert_recognized_stream_event(&stream, Uuid::new_v4(), &mut seq)
        .expect("the fixture's event type must stay recognized by the converter");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, EventType::ToolResult);
    assert_eq!(events[0].content, "see image");
    assert_eq!(events[0].tool_use_id, Some("toolu_7".to_string()));
    assert_eq!(events[1].event_type, EventType::System);
    assert!(events[1].content.contains("image"));
    assert_eq!(events[1].tool_use_id, Some("toolu_7".to_string()));
}

#[test]
fn test_convert_stream_event_tool_use_id_roundtrips() {
    let stream = StreamEvent {
        event_type: "assistant".to_string(),
        data: serde_json::json!({
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "toolu_abc", "name": "Read", "input": {"path": "/a"}}
                ]
            }
        }),
    };
    let mut seq = 0;
    let events = SessionManager::convert_recognized_stream_event(&stream, Uuid::new_v4(), &mut seq)
        .expect("the fixture's event type must stay recognized by the converter");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].tool_use_id, Some("toolu_abc".to_string()));
}

#[test]
fn test_convert_stream_event_parallel_tool_use_distinct_ids() {
    let stream = StreamEvent {
        event_type: "assistant".to_string(),
        data: serde_json::json!({
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"path": "/a"}},
                    {"type": "tool_use", "id": "toolu_2", "name": "Read", "input": {"path": "/b"}},
                    {"type": "tool_use", "id": "toolu_3", "name": "Read", "input": {"path": "/c"}}
                ]
            }
        }),
    };
    let mut seq = 0;
    let events = SessionManager::convert_recognized_stream_event(&stream, Uuid::new_v4(), &mut seq)
        .expect("the fixture's event type must stay recognized by the converter");
    let ids: Vec<_> = events.iter().map(|e| e.tool_use_id.clone()).collect();
    assert_eq!(
        ids,
        vec![
            Some("toolu_1".to_string()),
            Some("toolu_2".to_string()),
            Some("toolu_3".to_string()),
        ]
    );
}

#[test]
fn test_convert_stream_event_top_level_tool_use_id() {
    // Harness agent-loop synthetic events carry the id at the top level.
    let use_stream = StreamEvent {
        event_type: "tool_use".to_string(),
        data: serde_json::json!({"name": "Read", "input": {"path": "/a"}, "id": "call_1"}),
    };
    let mut seq = 0;
    let events =
        SessionManager::convert_recognized_stream_event(&use_stream, Uuid::new_v4(), &mut seq)
            .expect("the fixture's event type must stay recognized by the converter");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].tool_use_id, Some("call_1".to_string()));

    let result_stream = StreamEvent {
        event_type: "tool_result".to_string(),
        data: serde_json::json!({"name": "Read", "content": "done", "tool_use_id": "call_1"}),
    };
    let mut seq = 0;
    let events =
        SessionManager::convert_recognized_stream_event(&result_stream, Uuid::new_v4(), &mut seq)
            .expect("the fixture's event type must stay recognized by the converter");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].tool_use_id, Some("call_1".to_string()));
}
