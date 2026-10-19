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
        total_prompt_tokens: None,
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

/// #257: each session-metadata writer returns the `updated_at` it stored and
/// the value it replaced, so the daemon can mirror the stamp into a terminal
/// session's runtime copy. No changed row, no stamp.
#[test]
fn session_metadata_writes_return_the_updated_at_they_replaced_and_stored_257() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("test.db")).unwrap();
    let session = make_test_session();
    store.insert_session(&session).unwrap();
    let stored_updated_at =
        |store: &Store| store.get_session(session.id).unwrap().unwrap().updated_at;

    let before = stored_updated_at(&store);
    let filled = store
        .fill_session_title_if_absent_stamped(session.id, "Generated title")
        .unwrap()
        .expect("an absent title is filled");
    assert_eq!(filled.previous_updated_at, Some(before));
    assert_eq!(filled.stamped_at, stored_updated_at(&store));

    // Each later write chains from the stamp the previous one stored.
    let mut previous = filled.stamped_at;
    let writes: [&dyn Fn() -> Option<rsid::store::SessionWriteStamp>; 5] = [
        &|| {
            store
                .update_session_description(session.id, "Refined")
                .unwrap()
        },
        &|| store.update_session_rating(session.id, Some(7)).unwrap(),
        &|| {
            store
                .update_session_title(session.id, "Operator rename")
                .unwrap()
        },
        &|| {
            store
                .update_session_active_task(session.id, Some("task"))
                .unwrap()
        },
        &|| {
            store
                .update_session_project(session.id, session.project_id)
                .unwrap()
        },
    ];
    for write in writes {
        let stamp = write().expect("the row was written");
        assert_eq!(stamp.previous_updated_at, Some(previous));
        assert_eq!(stamp.stamped_at, stored_updated_at(&store));
        previous = stamp.stamped_at;
    }

    // A refused fill and a missing row write nothing and return no stamp.
    assert_eq!(
        store
            .fill_session_title_if_absent_stamped(session.id, "Generated overwrite")
            .unwrap(),
        None
    );
    assert_eq!(stored_updated_at(&store), previous);
    assert_eq!(
        store
            .update_session_description(Uuid::new_v4(), "Orphan")
            .unwrap(),
        None
    );
}

/// #258: the parent move and the lead pin return the stamp they stored, chained
/// from the `updated_at` they replaced; a pin of an already pinned session and a
/// missing row return no stamp, and the legacy pin entry point is unchanged.
#[test]
fn parent_move_and_lead_pin_return_their_stored_stamp_258() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("test.db")).unwrap();
    let session = make_test_session();
    store.insert_session(&session).unwrap();
    let stored = |store: &Store| store.get_session(session.id).unwrap().unwrap();

    let before = stored(&store).updated_at;
    let moved = store
        .update_session_parent(session.id, None)
        .unwrap()
        .expect("the row was written");
    assert_eq!(moved.previous_updated_at, Some(before));
    assert_eq!(moved.stamped_at, stored(&store).updated_at);

    let (pinned_at, stamp) = store.pin_session_if_unpinned_stamped(session.id).unwrap();
    let stamp = stamp.expect("an unpinned session is pinned");
    assert_eq!(stamp.previous_updated_at, Some(moved.stamped_at));
    let row = stored(&store);
    assert_eq!(stamp.stamped_at, row.updated_at);
    assert_eq!(row.pinned_at, Some(stamp.stamped_at));
    assert_eq!(pinned_at, Some(stamp.stamped_at.to_rfc3339()));

    let (again, none) = store.pin_session_if_unpinned_stamped(session.id).unwrap();
    assert_eq!(none, None);
    assert_eq!(again, pinned_at);
    assert_eq!(stored(&store).updated_at, stamp.stamped_at);
    assert_eq!(
        store.pin_session_if_unpinned(session.id).unwrap(),
        pinned_at
    );
    assert!(
        store
            .pin_session_if_unpinned_stamped(Uuid::new_v4())
            .is_err()
    );
    assert_eq!(
        store.update_session_parent(Uuid::new_v4(), None).unwrap(),
        None
    );
}

/// #265 (hub #2818): each stamped session write carries its Store commit
/// order (`write_seq`, assigned inside its transaction) and the unbroken run
/// of stamped writes it ends. Writes that each replaced the value the one
/// before stored share a run that starts from the row the first replaced; an
/// unstamped write of the row starts a new run; the session's last stamped
/// write, and the last of each field, are readable without the Store, and a
/// write that changed nothing is not recorded.
#[test]
fn session_writes_carry_store_commit_order_and_run_265() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("test.db")).unwrap();
    let session = make_test_session();
    store.insert_session(&session).unwrap();
    let before = store.get_session(session.id).unwrap().unwrap().updated_at;
    assert_eq!(rsid::store::last_session_write(session.id), None);
    assert_eq!(rsid::store::last_session_write_seq(session.id), 0);

    let title = store
        .update_session_title(session.id, "First")
        .unwrap()
        .expect("the row was written");
    let rating = store
        .update_session_rating(session.id, Some(3))
        .unwrap()
        .expect("the row was written");
    assert!(rating.write_seq > title.write_seq);
    assert_eq!(title.run_seq, title.write_seq);
    assert_eq!(title.run_base, Some(before));
    assert_eq!(rating.run_seq, title.run_seq);
    assert_eq!(rating.run_base, Some(before));
    assert_eq!(rsid::store::last_session_write(session.id), Some(rating));
    // Each field keeps its own last write.
    let writes = rsid::store::session_writes(session.id).expect("recorded writes");
    assert_eq!(
        writes.field(rsid::store::SessionWriteField::Title),
        title.write_seq
    );
    assert_eq!(
        writes.field(rsid::store::SessionWriteField::Rating),
        rating.write_seq
    );
    assert_eq!(writes.field(rsid::store::SessionWriteField::Parent), 0);

    // An unstamped write moves `updated_at` outside any run.
    let unstamped = rating.stamped_at + chrono::Duration::seconds(5);
    store
        .conn
        .execute(
            "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
            rusqlite::params![unstamped.to_rfc3339(), session.id.to_string()],
        )
        .unwrap();
    let after_gap = store
        .update_session_title(session.id, "Second")
        .unwrap()
        .expect("the row was written");
    assert!(after_gap.write_seq > rating.write_seq);
    assert_eq!(after_gap.previous_updated_at, Some(unstamped));
    assert_eq!(after_gap.run_seq, after_gap.write_seq);
    assert_eq!(after_gap.run_base, Some(unstamped));

    // A write that changed no row is neither stamped nor recorded.
    assert_eq!(
        store
            .fill_session_title_if_absent_stamped(session.id, "Generated")
            .unwrap(),
        None
    );
    assert_eq!(
        rsid::store::last_session_write_seq(session.id),
        after_gap.write_seq
    );
}

/// #2876: the process-global write-order registry follows the Store's session
/// set. Across repeated create / write / purge cycles no record outlives its
/// row, while a retained (here archived) session keeps its record and a
/// session written again after a retirement of another is unaffected.
#[test]
fn purged_sessions_release_their_write_order_records_2876() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("test.db")).unwrap();
    let kept = make_test_session();
    store.insert_session(&kept).unwrap();
    let kept_stamp = store
        .update_session_title(kept.id, "kept")
        .unwrap()
        .expect("the row was written");

    let mut purged = Vec::new();
    for round in 0..25 {
        let session = make_test_session();
        store.insert_session(&session).unwrap();
        store
            .update_session_title(session.id, &format!("title {round}"))
            .unwrap()
            .expect("title stamped");
        store
            .update_session_rating(session.id, Some(1))
            .unwrap()
            .expect("rating stamped");
        assert!(rsid::store::session_writes(session.id).is_some());
        store.purge_session(session.id).unwrap();
        purged.push(session.id);
    }

    for id in purged {
        assert!(store.get_session(id).unwrap().is_none());
        assert_eq!(rsid::store::session_writes(id), None, "record of {id}");
        assert_eq!(rsid::store::last_session_write_seq(id), 0);
    }
    assert_eq!(rsid::store::last_session_write(kept.id), Some(kept_stamp));
    let next = store
        .update_session_title(kept.id, "kept again")
        .unwrap()
        .expect("the row was written");
    assert!(next.write_seq > kept_stamp.write_seq);
    assert_eq!(rsid::store::last_session_write(kept.id), Some(next));
}
