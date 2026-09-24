//! Phase 0.3 — Store worker flush bench.
//!
//! `store_worker.rs` does NOT expose a "batch flush" API — the worker processes
//! commands one at a time off an mpsc channel (see `StoreWorker::run`). The
//! load-bearing throughput question is "how fast can N event inserts hit the
//! Store before the channel backs up?", so this bench measures that proxy
//! directly: drive `Store::insert_event` for N events against a tempfile-backed
//! SQLite (the same path the daemon uses, minus the channel hop).
//!
//! Hermetic: tempfile-backed `Store::open`, no daemon, no network. The proxy
//! captures the dominant cost (SQLite write) without requiring the worker
//! thread or the mpsc plumbing.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use rsi_common::types::{ConversationEvent, EventType, Role};
use rsid::store::Store;
use std::path::PathBuf;
use tempfile::TempDir;
use uuid::Uuid;

fn make_event(session_id: Uuid, sequence: i32) -> ConversationEvent {
    ConversationEvent {
        id: 0,
        session_id,
        sequence,
        event_type: if sequence % 2 == 0 {
            EventType::Message
        } else {
            EventType::ToolResult
        },
        role: Some(if sequence % 2 == 0 {
            Role::Assistant
        } else {
            Role::User
        }),
        content: format!(
            "synthetic event {sequence} body — paragraph one with some text \
             and a code reference `fn foo()` for realism"
        ),
        tool_name: if sequence % 2 == 0 {
            None
        } else {
            Some("Read".to_string())
        },
        tool_input: None,
        offload_id: None,
        tool_use_id: None,
        metadata: None,
        created_at: chrono::Utc::now(),
    }
}

fn make_session(session_id: Uuid) -> rsi_common::types::Session {
    rsi_common::types::Session {
        context_fill_pct: None,
        id: session_id,
        provider: rsi_common::types::SessionProvider::Claude,
        claude_session_id: None,
        query: "bench".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        pending_question: None,
        pending_archive: false,
        working_dir: PathBuf::from("/tmp/bench"),
        git_branch: None,
        status: rsi_common::types::SessionStatus::Running,
        project_id: None,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        session_kind: rsi_common::types::SessionKind::Standard,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        cost_usd: None,
        duration_ms: None,
        num_turns: None,
        model: None,
        input_tokens: None,
        output_tokens: None,
        context_window: None,
        resolved_context_budget: None,
        total_input_tokens: None,
        total_output_tokens: None,
        total_cache_creation_tokens: None,
        total_cache_read_tokens: None,
        stop_reason: None,
        continued_from: None,
        context_usage_confidence: rsi_common::types::ContextUsageConfidence::Missing,
        daemon_input_tokens: None,
        daemon_output_tokens: None,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        pipeline_artifact: None,
        workflow_id: None,
        workflow_id_override: None,
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

/// Open a fresh tempfile-backed Store with one parent session inserted, ready
/// for event inserts. Returned `TempDir` keeps the file alive for the bench.
fn fresh_store_with_session() -> (Store, TempDir, Uuid) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("bench.db");
    let store = Store::open(&db_path).expect("open store");
    let session_id = Uuid::new_v4();
    store
        .insert_session(&make_session(session_id))
        .expect("insert session");
    (store, dir, session_id)
}

fn bench_insert_events(c: &mut Criterion) {
    let mut group = c.benchmark_group("store_worker_flush/insert_events");
    // Reduce sample size — each iteration creates a fresh DB + N inserts, so the
    // total work scales fast. Criterion defaults are fine for N=10/100; bump
    // measurement targets only if needed.
    for n in [10usize, 100, 1000] {
        group.bench_function(format!("n_{n}"), |b| {
            b.iter_with_setup(fresh_store_with_session, |(store, _dir, session_id)| {
                for seq in 0..n {
                    let event = make_event(session_id, seq as i32);
                    store.insert_event(black_box(&event)).expect("insert event");
                }
                black_box(store);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_insert_events);
criterion_main!(benches);
