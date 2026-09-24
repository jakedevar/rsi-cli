//! Phase 0.4 — dhat heap baseline test.
//!
//! Run with: cargo test --features dhat-heap -p rsid --test dhat_baseline -- --ignored
//! Output:   target/dhat/dhat-heap-baseline.json
//!
//! # Proxy choice
//! Full `SessionManager` bring-up requires daemon-level async wiring (tokio runtime,
//! provider subprocess spawning, Unix socket). For a hermetic, synchronous heap
//! measurement we instead exercise the two heaviest store-write paths directly:
//!
//!   - `StoreHandle::insert_session` — allocates per-session row + channel message
//!   - `StoreHandle::insert_event`   — allocates per-event row + content string
//!
//! This captures the hot allocation path for the store worker (the largest
//! sustained heap consumer in the daemon) without any I/O or subprocess overhead.
//! The proxy gap: provider-side parsing allocations (claude.rs / codex.rs stream
//! deserialization) are NOT measured here; those belong in a future bench target.
//!
//! Load: 10 sessions × 500 events = 5 000 insert_event calls, all flushed
//! synchronously through the real `StoreWorker` running on its own OS thread.

#![cfg(feature = "dhat-heap")]

use std::path::PathBuf;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[test]
#[ignore]
fn dhat_heap_baseline() {
    // Create output directory before starting the profiler; dhat errors if the
    // directory does not exist at profile-drop time.
    std::fs::create_dir_all("target/dhat").expect("failed to create target/dhat/");

    let _profiler = dhat::Profiler::builder()
        .file_name("target/dhat/dhat-heap-baseline.json")
        .build();

    // ── Store + worker setup ──────────────────────────────────────────────────
    // Use a tempfile-backed SQLite database so the test is hermetic (no
    // ~/.rsi/rsi.db access) and each run starts from a clean schema.
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("dhat-test.db");
    let store = rsid::store::Store::open(&db_path).expect("store open");

    // Capacity 6000 > N_SESSIONS + N_SESSIONS*M_EVENTS (5010 total cmds) so
    // try_send never returns TrySendError::Full before the worker drains.
    let handle = rsid::store_worker::spawn_store_worker(store, 6000);

    // ── Session fixture ───────────────────────────────────────────────────────
    use rsi_common::types::{
        ContextUsageConfidence, ConversationEvent, EventType, Role, Session, SessionKind,
        SessionProvider, SessionStatus,
    };
    use uuid::Uuid;

    const N_SESSIONS: usize = 10;
    const M_EVENTS: i32 = 500;

    let mut session_ids: Vec<Uuid> = Vec::with_capacity(N_SESSIONS);

    for i in 0..N_SESSIONS {
        let id = Uuid::new_v4();
        session_ids.push(id);

        let session = Session {
            id,
            provider: SessionProvider::Claude,
            claude_session_id: None,
            query: format!("dhat baseline session {}", i),
            title: None,
            description: None,
            short_summary: None,
            pending_question: None,
            pending_archive: false,
            working_dir: PathBuf::from("/tmp/dhat-test"),
            git_branch: None,
            status: SessionStatus::Running,
            project_id: None,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            session_kind: SessionKind::Standard,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            continued_from: None,
            context_usage_confidence: ContextUsageConfidence::Missing,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            pipeline_artifact: None,
            workflow_id: None,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            scheduled_job_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: None,
            work_time_ms: None,
            approval_started_at: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: None,
            lead_session_id: None,
            is_eval: false,
        };

        handle
            .insert_session(session)
            .expect("insert_session failed");
    }

    // ── Event stream ──────────────────────────────────────────────────────────
    for &session_id in &session_ids {
        for seq in 0..M_EVENTS {
            let event = ConversationEvent {
                id: 0, // DB assigns real ID
                session_id,
                sequence: seq,
                event_type: EventType::Message,
                role: Some(Role::Assistant),
                content: format!("dhat baseline event {} for session {}", seq, session_id),
                tool_name: None,
                tool_input: None,
                created_at: chrono::Utc::now(),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            };

            handle
                .insert_event(session_id, event)
                .expect("insert_event failed");
        }
    }

    // ── Flush via shutdown ────────────────────────────────────────────────────
    // Sending Shutdown drains all pending commands before the worker thread exits.
    handle.shutdown().expect("shutdown failed");

    // The profiler snapshot is written automatically when `_profiler` drops at
    // the end of this function (dhat::Profiler::Drop impl).
}
