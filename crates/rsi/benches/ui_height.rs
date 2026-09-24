//! Phase 0.3 — UI height pre-compute bench.
//!
//! Benches the `update_event_heights(state, width)` pre-render pass — the hot
//! path that walks every event in a session and computes its rendered height
//! via `Paragraph::line_count`. This runs every frame the cache is stale.
//!
//! Hermetic: synthetic event lists, no daemon, no file I/O.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use rsi::types::SessionState;
use rsi::ui::height::{event_rendered_height, update_event_heights};
use rsi_common::types::{
    ContextUsageConfidence, ConversationEvent, EventType, Role, Session, SessionKind,
    SessionProvider, SessionStatus,
};
use std::path::PathBuf;
use uuid::Uuid;

fn make_session() -> Session {
    Session {
        context_fill_pct: None,
        id: Uuid::new_v4(),
        provider: SessionProvider::Claude,
        claude_session_id: None,
        query: "bench session".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        pending_question: None,
        pending_archive: false,
        working_dir: PathBuf::from("/tmp/bench"),
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
        resolved_context_budget: None,
        total_input_tokens: None,
        total_output_tokens: None,
        total_cache_creation_tokens: None,
        total_cache_read_tokens: None,
        stop_reason: None,
        continued_from: None,
        context_usage_confidence: ContextUsageConfidence::Missing,
        capability_class: None,
        topology_node_id: None,
        topology_iteration: 0,
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

fn make_events(n: usize) -> Vec<ConversationEvent> {
    let session_id = Uuid::new_v4();
    let mut events = Vec::with_capacity(n);
    for i in 0..n {
        // Cycle through event types so the bench exercises every render branch.
        let (event_type, role, content, tool_name, tool_input) = match i % 4 {
            0 => (
                EventType::Message,
                Some(Role::User),
                format!(
                    "User message {i}: please investigate **fn xyz** in `src/foo.rs` \
                     and propose a fix. See [docs](https://example.com/{i}).",
                ),
                None,
                None,
            ),
            1 => (
                EventType::Message,
                Some(Role::Assistant),
                format!(
                    "Assistant response {i}:\n\n# Plan\n\n- step one\n- step two\n\n\
                     ```rust\nfn main() {{ println!(\"hi {i}\"); }}\n```\n\nDone.",
                ),
                None,
                None,
            ),
            2 => (
                EventType::ToolUse,
                None,
                String::new(),
                Some("Read".to_string()),
                Some(Box::new(serde_json::json!({
                    "file_path": format!("/repo/src/file_{i}.rs"),
                }))),
            ),
            _ => (
                EventType::ToolResult,
                None,
                format!("file contents line A\nfile contents line B\n... ({i} lines)"),
                None,
                None,
            ),
        };

        events.push(ConversationEvent {
            id: i as i64,
            session_id,
            sequence: i as i32,
            event_type,
            role,
            content,
            tool_name,
            tool_input,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
            created_at: chrono::Utc::now(),
        });
    }
    events
}

fn bench_event_rendered_height(c: &mut Criterion) {
    let events = make_events(4); // one of each variant
    let mut group = c.benchmark_group("ui_height/event_rendered_height");
    for (label, idx) in [
        ("message_user", 0),
        ("message_assistant", 1),
        ("tool_use", 2),
        ("tool_result", 3),
    ] {
        let event = &events[idx];
        group.bench_function(label, |b| {
            b.iter(|| {
                let h = event_rendered_height(
                    black_box(event),
                    black_box(false),
                    black_box(false),
                    black_box(false),
                    black_box(true),
                    black_box(true),
                    black_box(true),
                    black_box(120),
                    black_box(idx),
                    black_box(4),
                );
                black_box(h);
            });
        });
    }
    group.finish();
}

fn bench_update_event_heights(c: &mut Criterion) {
    let mut group = c.benchmark_group("ui_height/update_event_heights");
    for n in [10usize, 100, 1000] {
        let events = make_events(n);
        group.bench_function(format!("n_{n}"), |b| {
            b.iter_with_setup(
                || {
                    let mut state = SessionState::new(make_session());
                    state.events = events.clone();
                    state.events_generation += 1;
                    state
                },
                |mut state| {
                    update_event_heights(black_box(&mut state), black_box(120));
                    black_box(&state.event_heights);
                },
            );
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_event_rendered_height,
    bench_update_event_heights
);
criterion_main!(benches);
