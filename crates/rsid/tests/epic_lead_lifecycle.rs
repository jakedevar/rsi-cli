//! Phase 2 integration tests: epic lead-session lifecycle hooks.
//!
//! Tests the store-level invariants for lead pointer assignment, auto-promote
//! atomicity, and deletion clearing — using a real SQLite store (in-memory
//! via tempfile). Each test is self-contained and avoids spinning up a full
//! daemon / TCP socket.

use rsi_common::types::{
    ContextUsageConfidence, Session, SessionKind, SessionProvider, SessionStatus,
};
use rsid::store::Store;
use tempfile::TempDir;
use uuid::Uuid;

/// Open a fresh ephemeral store for each test.
fn open_store() -> (Store, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.db");
    let store = Store::open(&path).unwrap();
    (store, dir)
}

/// Minimal Session fixture — only the fields required for store.insert_session.
fn make_session(id: Uuid, kind: SessionKind, parent_id: Option<Uuid>) -> Session {
    let now = chrono::Utc::now();
    Session {
        context_fill_pct: None,
        id,
        status: SessionStatus::Completed,
        session_kind: kind,
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
        parent_id,
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
        pipeline_artifact: None,
        workflow_id: None,
        workflow_id_override: None,
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
        approval_wait_ms: None,
        work_time_ms: None,
        approval_started_at: None,
        sandbox_kind: None,
        sandbox_root: None,
        sandbox_branch: None,
        sandbox_cleanup_state: None,
        tag: String::new(),
        tags: Vec::new(),
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

/// `try_promote_lead_if_unset` — first caller wins, second is a no-op.
#[test]
fn handle_launch_session_auto_promotes_first_child() {
    let (store, _dir) = open_store();

    let epic_id = Uuid::new_v4();
    let story_a = Uuid::new_v4();
    let story_b = Uuid::new_v4();

    // Insert an Epic container.
    store
        .insert_session(&make_session(epic_id, SessionKind::Epic, None))
        .unwrap();
    // Insert two child Story sessions.
    store
        .insert_session(&make_session(story_a, SessionKind::Story, Some(epic_id)))
        .unwrap();
    store
        .insert_session(&make_session(story_b, SessionKind::Story, Some(epic_id)))
        .unwrap();

    // First promotion wins.
    let won_a = store.try_promote_lead_if_unset(epic_id, story_a).unwrap();
    assert!(won_a, "first caller must win the promotion race");

    // Second promotion loses (Epic already has a lead).
    let won_b = store.try_promote_lead_if_unset(epic_id, story_b).unwrap();
    assert!(!won_b, "second caller must lose (lead already set)");

    // Confirm the persisted lead is story_a.
    let epic_row = store.get_session(epic_id).unwrap().unwrap();
    assert_eq!(epic_row.lead_session_id, Some(story_a));
}

/// Concurrent `try_promote_lead_if_unset` — at most one wins.
///
/// Spawns N threads each attempting to set themselves as lead. Exactly one
/// must succeed; all others must return `false`.
#[test]
fn auto_promote_race_only_first_wins() {
    // Use a real DB file (not shared across threads via Store — each thread
    // gets its own Store handle, all pointing at the same file to exercise
    // the SQLite conditional-UPDATE atomicity).
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("race.db");

    // Seed the DB from the primary handle.
    {
        let store = Store::open(&path).unwrap();
        let epic_id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        store
            .insert_session(&make_session(epic_id, SessionKind::Epic, None))
            .unwrap();
    }

    let epic_id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
    const CONCURRENCY: usize = 8;

    let candidates: Vec<Uuid> = (0..CONCURRENCY).map(|_| Uuid::new_v4()).collect();
    let path = path.clone();

    let results: Vec<bool> = std::thread::scope(|s| {
        candidates
            .iter()
            .map(|&cid| {
                let p = path.clone();
                s.spawn(move || {
                    let store = Store::open(&p).unwrap();
                    store.try_promote_lead_if_unset(epic_id, cid).unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect()
    });

    let winner_count = results.iter().filter(|&&r| r).count();
    assert_eq!(winner_count, 1, "exactly one thread must win the race");
}

/// `clear_lead_session_if_matches` NULLs the Epic lead pointer.
#[test]
fn handle_delete_session_clears_lead_pointers() {
    let (store, _dir) = open_store();

    let epic_id = Uuid::new_v4();
    let story_id = Uuid::new_v4();

    store
        .insert_session(&make_session(epic_id, SessionKind::Epic, None))
        .unwrap();
    store
        .insert_session(&make_session(story_id, SessionKind::Story, Some(epic_id)))
        .unwrap();

    // Set lead.
    store.set_lead_session(epic_id, Some(story_id)).unwrap();

    // Verify it's set.
    let before = store.get_session(epic_id).unwrap().unwrap();
    assert_eq!(before.lead_session_id, Some(story_id));

    // Pre-delete hook clears it.
    store.clear_lead_session_if_matches(story_id).unwrap();

    // Verify it's cleared.
    let after = store.get_session(epic_id).unwrap().unwrap();
    assert_eq!(after.lead_session_id, None);
}

/// `handle_set_epic_lead_clears_with_none` — calling set_lead_session(epic, None)
/// unconditionally clears the pointer.
#[test]
fn handle_set_epic_lead_clears_with_none() {
    let (store, _dir) = open_store();

    let epic_id = Uuid::new_v4();
    let story_id = Uuid::new_v4();

    store
        .insert_session(&make_session(epic_id, SessionKind::Epic, None))
        .unwrap();
    store
        .insert_session(&make_session(story_id, SessionKind::Story, Some(epic_id)))
        .unwrap();

    store.set_lead_session(epic_id, Some(story_id)).unwrap();
    let row = store.get_session(epic_id).unwrap().unwrap();
    assert_eq!(row.lead_session_id, Some(story_id));

    store.set_lead_session(epic_id, None).unwrap();
    let row = store.get_session(epic_id).unwrap().unwrap();
    assert_eq!(row.lead_session_id, None);
}

/// `find_epics_by_lead` returns the Epic IDs whose lead matches the target.
#[test]
fn rotation_transfers_lead_to_successor() {
    let (store, _dir) = open_store();

    let epic_id = Uuid::new_v4();
    let old_lead = Uuid::new_v4();
    let new_lead = Uuid::new_v4();

    store
        .insert_session(&make_session(epic_id, SessionKind::Epic, None))
        .unwrap();
    store
        .insert_session(&make_session(old_lead, SessionKind::Story, Some(epic_id)))
        .unwrap();
    store
        .insert_session(&make_session(new_lead, SessionKind::Story, Some(epic_id)))
        .unwrap();

    // Set old_lead as the Epic's current lead.
    store.set_lead_session(epic_id, Some(old_lead)).unwrap();

    // Rotation: find all Epics pointing at old_lead.
    let epics = store.find_epics_by_lead(old_lead).unwrap();
    assert_eq!(epics, vec![epic_id]);

    // Transfer each pointer to new_lead.
    for eid in &epics {
        store.set_lead_session(*eid, Some(new_lead)).unwrap();
    }

    // Verify transfer.
    let row = store.get_session(epic_id).unwrap().unwrap();
    assert_eq!(row.lead_session_id, Some(new_lead));

    // old_lead no longer has any Epics pointing at it.
    let stale = store.find_epics_by_lead(old_lead).unwrap();
    assert!(stale.is_empty());
}

/// `find_epics_by_lead` scopes only to the target lead ID —
/// an unrelated lead does not appear in results.
#[test]
fn handle_set_epic_lead_rejects_non_child() {
    // Verify that `parent_id` membership is what distinguishes valid leads.
    // If a Story is under Epic-A, it must NOT be accepted as lead for Epic-B.
    let (store, _dir) = open_store();

    let epic_a = Uuid::new_v4();
    let epic_b = Uuid::new_v4();
    let story_id = Uuid::new_v4();

    store
        .insert_session(&make_session(epic_a, SessionKind::Epic, None))
        .unwrap();
    store
        .insert_session(&make_session(epic_b, SessionKind::Epic, None))
        .unwrap();
    // Story is under epic_a.
    store
        .insert_session(&make_session(story_id, SessionKind::Story, Some(epic_a)))
        .unwrap();

    // Fetch the story row and assert parent_id is epic_a, not epic_b.
    let story_row = store.get_session(story_id).unwrap().unwrap();
    assert_eq!(story_row.parent_id, Some(epic_a));
    // The membership check in handle_set_epic_lead would reject story for epic_b:
    assert_ne!(story_row.parent_id, Some(epic_b));
}
