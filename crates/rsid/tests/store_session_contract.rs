//! Public Store API contracts for session persistence.

use std::path::PathBuf;

use rsi_common::types::{
    ContextUsageConfidence, Session, SessionKind, SessionProvider, SessionStatus,
};
use rsid::store::{Store, parse_timestamp};
use uuid::Uuid;

fn make_test_session() -> Session {
    Session {
        context_fill_pct: None,
        id: Uuid::new_v4(),
        provider: SessionProvider::Claude,
        claude_session_id: None,
        query: "Hello, Claude!".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        pending_question: None,
        pending_archive: false,
        working_dir: PathBuf::from("/tmp/test"),
        git_branch: None,
        status: SessionStatus::Starting,
        project_id: Some(
            Uuid::parse_str("00000000-0000-4000-8000-000000000077")
                .expect("fixed test project UUID"),
        ),
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

#[test]
fn parse_timestamp_accepts_legacy_comma_fraction_rfc3339() {
    let parsed = parse_timestamp("2026-08-02T16:15:11,769711900-07:00").unwrap();
    let expected = chrono::DateTime::parse_from_rfc3339("2026-08-02T16:15:11.769711900-07:00")
        .unwrap()
        .with_timezone(&chrono::Utc);

    assert_eq!(parsed, expected);
}

#[test]
fn insert_session_persists_testing_needed_and_rotation_disabled_at() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("test.db")).unwrap();
    let mut session = make_test_session();
    let testing_needed_at = chrono::Utc::now();
    let rotation_disabled_at = testing_needed_at + chrono::Duration::nanoseconds(1);
    session.testing_needed_at = Some(testing_needed_at);
    session.rotation_disabled_at = Some(rotation_disabled_at);

    store.insert_session(&session).unwrap();

    let loaded = store.get_session(session.id).unwrap().unwrap();
    assert_eq!(loaded.testing_needed_at, Some(testing_needed_at));
    assert_eq!(loaded.rotation_disabled_at, Some(rotation_disabled_at));
}

#[test]
fn set_session_rotation_disabled_at_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("test.db")).unwrap();
    let mut session = make_test_session();
    let stamp = chrono::Utc::now();
    session.rotation_disabled_at = Some(stamp);
    store.insert_session(&session).unwrap();

    for _ in 0..3 {
        store
            .set_session_rotation_disabled_at(session.id, Some(stamp))
            .unwrap();
        assert_eq!(
            store
                .get_session(session.id)
                .unwrap()
                .unwrap()
                .rotation_disabled_at,
            Some(stamp)
        );
    }

    store
        .set_session_rotation_disabled_at(session.id, None)
        .unwrap();
    assert!(
        store
            .get_session(session.id)
            .unwrap()
            .unwrap()
            .rotation_disabled_at
            .is_none()
    );
    assert!(
        store
            .set_session_rotation_disabled_at(Uuid::new_v4(), Some(stamp))
            .is_err()
    );
}

#[test]
fn fill_session_title_if_absent_supplies_only_a_missing_title() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("test.db")).unwrap();
    let session = make_test_session();
    store.insert_session(&session).unwrap();

    // Absent title: the generated fill is accepted.
    assert!(
        store
            .fill_session_title_if_absent(session.id, "Generated title")
            .unwrap()
    );
    assert_eq!(
        store.get_session(session.id).unwrap().unwrap().title,
        Some("Generated title".to_string())
    );

    // Present title: the next fill is refused and the title is unchanged.
    assert!(
        !store
            .fill_session_title_if_absent(session.id, "Generated overwrite")
            .unwrap()
    );
    assert_eq!(
        store.get_session(session.id).unwrap().unwrap().title,
        Some("Generated title".to_string())
    );

    // A missing session is an error, not a silent no-op.
    assert!(
        store
            .fill_session_title_if_absent(Uuid::new_v4(), "Orphan")
            .is_err()
    );
}

#[test]
fn explicit_title_survives_generated_fill_and_deliberate_rename_still_wins() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("test.db")).unwrap();
    let mut session = make_test_session();
    session.title = Some("PARITYMODALSESSION".to_string());
    store.insert_session(&session).unwrap();

    // Asynchronous enrichment must not overwrite the explicit title.
    assert!(
        !store
            .fill_session_title_if_absent(session.id, "Demiurge: Generated")
            .unwrap()
    );
    assert_eq!(
        store.get_session(session.id).unwrap().unwrap().title,
        Some("PARITYMODALSESSION".to_string())
    );

    // Deliberate rename is a distinct path and must still overwrite.
    store
        .update_session_title(session.id, "Operator rename")
        .unwrap();
    assert_eq!(
        store.get_session(session.id).unwrap().unwrap().title,
        Some("Operator rename".to_string())
    );
}

#[test]
fn load_archived_sessions_returns_only_the_previous_seven_days() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("test.db")).unwrap();
    let now = chrono::Utc::now();

    let mut recent = make_test_session();
    recent.status = SessionStatus::Archived;
    recent.updated_at = now - chrono::Duration::days(6);

    let mut expired = make_test_session();
    expired.status = SessionStatus::Archived;
    expired.updated_at = now - chrono::Duration::days(8);

    store.insert_session(&recent).unwrap();
    store.insert_session(&expired).unwrap();

    let archived = store.load_archived_sessions(None).unwrap();

    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].id, recent.id);

    let project_archived = store.load_archived_sessions(recent.project_id).unwrap();
    assert_eq!(project_archived.len(), 1);
    assert_eq!(project_archived[0].id, recent.id);
}
