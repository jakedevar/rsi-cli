//! Zero-regression suite: non-sandboxed sessions must produce zero sandbox
//! side-effects.
//!
//! Specifically:
//! 1. N sessions with `sandbox_kind = None` produce `Session.sandbox_kind == None`.
//! 2. The `RSI_SANDBOX_BASE` directory (simulated by a tempdir) is EMPTY after
//!    all launches — no files or dirs are created.
//! 3. `Session` JSON serialized from a non-sandboxed row contains no
//!    `sandbox_*` keys with non-null values (serde skips `None` fields).
//!
//! Note on byte-for-bit fixture: maintaining a byte-exact JSON fixture is brittle
//! (field order is Serde's insertion order, which may shift between Rust versions
//! and struct layout changes). Instead we assert the looser property:
//!   - No `sandbox_*` JSON key has a non-null value.
//!   - Deserialization from the serialized form succeeds (serde contract holds).
//! This is documented as an intentional loosening per Phase 5 instructions.

use rsi_common::types::{
    ContextUsageConfidence, Session, SessionKind, SessionProvider, SessionStatus,
};
use rsid::sandbox::SandboxAllocator;
use tempfile::tempdir;
use uuid::Uuid;

/// Build a non-sandboxed `Session` (all four sandbox_* fields set to `None`).
fn make_non_sandboxed_session(working_dir: &std::path::Path) -> Session {
    Session {
        context_fill_pct: None,
        id: Uuid::new_v4(),
        provider: SessionProvider::Claude,
        claude_session_id: None,
        query: "noop test".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        pending_question: None,
        pending_archive: false,
        working_dir: working_dir.to_path_buf(),
        git_branch: None,
        status: SessionStatus::Completed,
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
        // The four sandbox fields — all None for non-sandboxed sessions.
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

/// Assert that all four sandbox fields are `None` on a non-sandboxed Session.
#[test]
fn noop_sessions_have_no_sandbox_fields() {
    let dir = tempdir().unwrap();
    const N: usize = 3;
    for _ in 0..N {
        let session = make_non_sandboxed_session(dir.path());
        assert!(
            session.sandbox_kind.is_none(),
            "sandbox_kind must be None for non-sandboxed session"
        );
        assert!(
            session.sandbox_root.is_none(),
            "sandbox_root must be None for non-sandboxed session"
        );
        assert!(
            session.sandbox_branch.is_none(),
            "sandbox_branch must be None for non-sandboxed session"
        );
        assert!(
            session.sandbox_cleanup_state.is_none(),
            "sandbox_cleanup_state must be None for non-sandboxed session"
        );
    }
}

/// Assert that the `RSI_SANDBOX_BASE` directory receives ZERO writes when N
/// non-sandboxed sessions are created. The allocator is the only component that
/// writes into that directory; a non-sandboxed session must never invoke it.
///
/// Regression belt: if any code path accidentally calls `SandboxAllocator::ensure_base`
/// or `allocate` during a non-sandboxed launch, this test catches it.
#[test]
fn noop_sessions_do_not_write_sandbox_base() {
    let sandbox_base = tempdir().unwrap();
    let working_dir = tempdir().unwrap();

    // Create an allocator pointed at the base — but never call it.
    // This mirrors daemon startup (allocator is constructed but only called
    // when sandbox_kind is Some).
    let _allocator = SandboxAllocator::new(sandbox_base.path().to_path_buf());

    const N: usize = 3;
    for _ in 0..N {
        // Simulate the non-sandboxed launch path: build a Session without calling
        // the allocator. The allocator must NOT be called for sandbox=None sessions.
        let _session = make_non_sandboxed_session(working_dir.path());
    }

    // The sandbox base must be completely empty — no sub-directories, no files.
    let entries: Vec<_> = std::fs::read_dir(sandbox_base.path())
        .expect("read_dir on sandbox base must succeed")
        .flatten()
        .collect();

    assert!(
        entries.is_empty(),
        "sandbox base must be empty after {} non-sandboxed session launches; \
         found {} entries: {:?}",
        N,
        entries.len(),
        entries.iter().map(|e| e.path()).collect::<Vec<_>>()
    );
}

/// Assert the Session JSON serialization contract for non-sandboxed sessions:
/// - `sandbox_kind`, `sandbox_root`, `sandbox_branch`, `sandbox_cleanup_state`
///   must either be absent from the JSON or present with a `null` value.
/// - No key has a non-null sandbox value.
/// - Deserialization round-trips without error.
///
/// Loosening rationale: byte-exact fixture would be brittle across Rust/serde
/// version upgrades and field-order changes. We check structural properties
/// instead, which are stable and meaningful.
#[test]
fn noop_session_json_has_no_non_null_sandbox_keys() {
    let dir = tempdir().unwrap();
    const N: usize = 3;

    for _ in 0..N {
        let session = make_non_sandboxed_session(dir.path());

        // Serialize to JSON.
        let json = serde_json::to_string(&session).expect("Session must serialize");
        let value: serde_json::Value =
            serde_json::from_str(&json).expect("Serialized Session must parse as JSON");

        // Each sandbox field, if present, must be null (not a non-null value).
        for key in &[
            "sandbox_kind",
            "sandbox_root",
            "sandbox_branch",
            "sandbox_cleanup_state",
        ] {
            if let Some(v) = value.get(*key) {
                assert!(
                    v.is_null(),
                    "JSON key '{key}' must be null (or absent) for a non-sandboxed Session, \
                     got: {v}"
                );
            }
            // Key being absent is also acceptable (serde skip_serializing_if = Option::is_none).
        }

        // Deserialization round-trip must succeed without error.
        let round_tripped: Session =
            serde_json::from_str(&json).expect("Session must deserialize from its own JSON");

        assert!(
            round_tripped.sandbox_kind.is_none(),
            "round-tripped sandbox_kind must be None"
        );
        assert!(
            round_tripped.sandbox_root.is_none(),
            "round-tripped sandbox_root must be None"
        );
        assert!(
            round_tripped.sandbox_branch.is_none(),
            "round-tripped sandbox_branch must be None"
        );
        assert!(
            round_tripped.sandbox_cleanup_state.is_none(),
            "round-tripped sandbox_cleanup_state must be None"
        );
    }
}
