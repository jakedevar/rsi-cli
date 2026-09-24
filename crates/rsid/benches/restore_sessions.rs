//! Phase 0.3 — Restore-sessions startup bench.
//!
//! `SessionManager::restore_sessions` (crates/rsid/src/session/launch.rs:830)
//! requires full daemon wiring (Store + EventBus + persistence + completed
//! map + tokio runtime). That makes it impossible to bench hermetically. The
//! load-bearing cost on startup is the SQLite read path that hydrates the
//! session list, so this bench measures `Store::load_sessions()` directly as
//! a proxy. The wrapping `tokio::spawn_blocking` and per-session event load
//! are excluded — they scale linearly on top of this baseline.
//!
//! Hermetic: tempfile-backed `Store::open`, no daemon, no network, no real
//! `~/.rsi/rsi.db`. Inputs: pre-populated DB with mixed status sessions.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use rsi_common::types::{
    ContextUsageConfidence, Session, SessionKind, SessionProvider, SessionStatus,
};
use rsid::store::Store;
use std::path::PathBuf;
use tempfile::TempDir;
use uuid::Uuid;

fn make_session(status: SessionStatus) -> Session {
    Session {
        context_fill_pct: None,
        id: Uuid::new_v4(),
        provider: SessionProvider::Claude,
        claude_session_id: None,
        query: "synthetic restore-bench session".to_string(),
        title: Some("bench title".to_string()),
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        pending_question: None,
        pending_archive: false,
        working_dir: PathBuf::from("/tmp/bench"),
        git_branch: None,
        status,
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
        model: Some("claude-opus-4-7".to_string()),
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
        context_usage_confidence: ContextUsageConfidence::Missing,
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

/// Build a tempfile-backed Store pre-populated with N sessions in a mix of
/// terminal (Completed/Failed/Interrupted) and non-terminal (Running/Starting)
/// states. Mirrors the realistic distribution `restore_sessions` walks at
/// daemon startup.
fn populated_store(n: usize) -> (Store, TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("bench.db");
    let store = Store::open(&db_path).expect("open store");
    let statuses = [
        SessionStatus::Completed,
        SessionStatus::Running,
        SessionStatus::Failed,
        SessionStatus::Interrupted,
        SessionStatus::Starting,
    ];
    for i in 0..n {
        let status = statuses[i % statuses.len()];
        store
            .insert_session(&make_session(status))
            .expect("insert session");
    }
    (store, dir)
}

fn bench_load_sessions(c: &mut Criterion) {
    let mut group = c.benchmark_group("restore_sessions/load_sessions");
    for n in [10usize, 100, 500] {
        let (store, _dir) = populated_store(n);
        group.bench_function(format!("n_{n}"), |b| {
            b.iter(|| {
                let sessions = store.load_sessions().expect("load sessions");
                black_box(sessions);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_load_sessions);
criterion_main!(benches);
