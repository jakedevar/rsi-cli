//! #630: infrastructure deaths relaunch the same DB-native review at most twice.
#![allow(clippy::unwrap_used, clippy::significant_drop_tightening)]

use super::*;

fn successor(store: &Store, assignment_id: Uuid) -> Uuid {
    let next: String = store
        .conn
        .query_row(
            "SELECT superseded_by_assignment_id FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    Uuid::parse_str(&next).unwrap()
}

fn review_row(store: &Store, assignment_id: Uuid) -> (String, Option<String>) {
    store
        .conn
        .query_row(
            "SELECT state,failure_code FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

fn assignment_count(store: &Store) -> i64 {
    store
        .conn
        .query_row(
            "SELECT count(*) FROM manager_review_assignments",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

fn fail_allocation_action(store: &Store, assignment_id: Uuid) {
    let action: String = store
        .conn
        .query_row(
            "SELECT action_operation_id FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE harness_manager_v2_operations
                SET state='failed',row_version=row_version+1,updated_at=?2
              WHERE id=?1",
            params![
                action,
                Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            ],
        )
        .unwrap();
}

#[tokio::test]
async fn retry_uses_commit_bound_seal_after_notes_only_commit() {
    let f = fixture().await;
    let assignment = request_and_activate_db_review(&f, "retry-sealed-notes").await;
    let note = f.source_root.join("thoughts/shared/notes/retry.md");
    std::fs::create_dir_all(note.parent().unwrap()).unwrap();
    std::fs::write(&note, "review handoff\n").unwrap();
    command(&f.source_root, &["add", "thoughts/shared/notes/retry.md"]);
    command(&f.source_root, &["commit", "-m", "notes: retry handoff"]);
    assert_ne!(
        command(&f.source_root, &["rev-parse", "HEAD"]),
        f.source_head
    );

    let store = f.handle.store.lock().await;
    fail_allocation_action(&store, assignment);
    assert!(store.refresh_manager_review_assignment(assignment).unwrap());
    let next = successor(&store, assignment);
    assert_eq!(review_row(&store, next).0, "reserved");
    assert_same_review(&store, assignment, next, &f.source_head);
    assert!(store.allocate_manager_review_assignment(next).unwrap());
    assert_eq!(review_row(&store, next).0, "allocating");
}

#[tokio::test]
async fn retry_uses_rotation_tip_as_source_holder() {
    let f = fixture().await;
    let assignment = request_and_activate_db_review(&f, "retry-rotated-source").await;
    let mut store = f.handle.store.lock().await;
    let custody = store.live_custody_for_session(f.source).unwrap();
    let mut tip = store.get_session(f.source).unwrap().unwrap();
    tip.id = Uuid::new_v4();
    tip.continued_from = Some(f.source);
    tip.status = SessionStatus::Starting;
    store.insert_session(&tip).unwrap();
    store
        .bind_reserved_session_custody(
            tip.id,
            SessionCustodyBinding::Transfer {
                custody_id: custody.custody_id,
                from_session_id: f.source,
                generation: custody.generation,
                cause: CustodyCause::Rotation,
                origin_session_id: Some(f.source),
                scheduled_job_id: None,
            },
        )
        .unwrap();
    store
        .update_session_status(f.source, SessionStatus::Archived)
        .unwrap();
    fail_allocation_action(&store, assignment);
    assert!(store.refresh_manager_review_assignment(assignment).unwrap());
    let next = successor(&store, assignment);
    assert!(store.allocate_manager_review_assignment(next).unwrap());
    let context: String = store
        .conn
        .query_row(
            "SELECT c.payload_json FROM manager_review_assignments a
               JOIN harness_manager_v2_records c
                 ON c.kind='lifecycle_context' AND c.record_key=a.action_operation_id
              WHERE a.assignment_id=?1",
            [next.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let context: Value = serde_json::from_str(&context).unwrap();
    assert_eq!(context["source"]["session_id"], json!(tip.id));
    assert_eq!(context["source"]["commit"], json!(f.source_head));
}

#[tokio::test]
async fn rotated_relaunch_crosses_the_pending_acceptance_gate() {
    let f = fixture().await;
    let assignment = request_and_activate_db_review(&f, "retry-rotated-gate").await;
    let mut store = f.handle.store.lock().await;
    let custody = store.live_custody_for_session(f.source).unwrap();
    let mut tip = store.get_session(f.source).unwrap().unwrap();
    tip.id = Uuid::new_v4();
    tip.continued_from = Some(f.source);
    tip.status = SessionStatus::Starting;
    store.insert_session(&tip).unwrap();
    store
        .bind_reserved_session_custody(
            tip.id,
            SessionCustodyBinding::Transfer {
                custody_id: custody.custody_id,
                from_session_id: f.source,
                generation: custody.generation,
                cause: CustodyCause::Rotation,
                origin_session_id: Some(f.source),
                scheduled_job_id: None,
            },
        )
        .unwrap();
    store
        .update_session_status(f.source, SessionStatus::Archived)
        .unwrap();
    fail_allocation_action(&store, assignment);
    assert!(store.refresh_manager_review_assignment(assignment).unwrap());
    let next = successor(&store, assignment);
    assert!(store.allocate_manager_review_assignment(next).unwrap());
    super::review_source::hold_for_acceptance(&store, &f);
    super::review_source::claim_and_cross_launch_gate(&store, tip.id);
}

fn assert_same_review(store: &Store, old: Uuid, next: Uuid, source: &str) {
    let (old_source, old_launch, old_revision, old_work): (String, String, i64, String) = store
        .conn
        .query_row(
            "SELECT source_sha,json_extract(request_json,'$.launch'),spec_revision,work_key
               FROM manager_review_assignments WHERE assignment_id=?1",
            [old.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    let (next_source, next_launch, next_revision, next_work, retry_of): (
        String,
        String,
        i64,
        String,
        String,
    ) = store
        .conn
        .query_row(
            "SELECT source_sha,json_extract(request_json,'$.launch'),spec_revision,work_key,
                    json_extract(request_json,'$.infra_retry_of')
               FROM manager_review_assignments WHERE assignment_id=?1",
            [next.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(old_source, source);
    assert_eq!(
        (next_source, next_launch, next_revision, next_work),
        (old_source, old_launch, old_revision, old_work)
    );
    assert_eq!(retry_of, old.to_string());
}

#[tokio::test]
async fn each_infrastructure_cause_launches_same_source_and_family_successor() {
    for cause in ["interrupted", "provider_failed", "allocation_failed"] {
        let f = fixture().await;
        let assignment = request_and_activate_db_review(&f, cause).await;
        let store = f.handle.store.lock().await;
        if cause == "allocation_failed" {
            fail_allocation_action(&store, assignment);
        } else {
            super::review_end::bind_review_invocation(&store, &f, assignment);
            let (status, error_class) = if cause == "interrupted" {
                (SessionStatus::Interrupted, "cancelled_after_restart")
            } else {
                (SessionStatus::Failed, "provider_unavailable")
            };
            store.update_session_status(f.reviewer, status).unwrap();
            store
                .conn
                .execute(
                    "UPDATE model_invocations SET status='failed',error_class=?2 WHERE id=?1",
                    params![f.invocation.to_string(), error_class],
                )
                .unwrap();
        }
        assert!(store.refresh_manager_review_assignment(assignment).unwrap());
        assert_eq!(review_row(&store, assignment).0, "superseded");
        let next = successor(&store, assignment);
        assert_eq!(review_row(&store, next).0, "reserved");
        assert_same_review(&store, assignment, next, &f.source_head);
        assert_eq!(
            store.reconcile_manager_review_assignments_once().unwrap().0,
            1
        );
        assert_eq!(review_row(&store, next).0, "allocating");
        assert_eq!(assignment_count(&store), 2);
    }
}

#[tokio::test]
async fn infrastructure_retry_chain_stops_after_two_relaunches() {
    let f = fixture().await;
    let first = request_and_activate_db_review(&f, "retry-bound").await;
    let store = f.handle.store.lock().await;
    let mut current = first;
    for attempt in 0..=2 {
        if attempt != 0 {
            assert!(store.allocate_manager_review_assignment(current).unwrap());
        }
        fail_allocation_action(&store, current);
        assert!(store.refresh_manager_review_assignment(current).unwrap());
        if attempt < 2 {
            let next = successor(&store, current);
            assert_same_review(&store, current, next, &f.source_head);
            current = next;
        }
    }
    assert_eq!(assignment_count(&store), 3);
    assert_eq!(
        review_row(&store, current),
        (
            "failed".into(),
            Some("manager_review_infra_retry_exhausted".into())
        )
    );
    assert!(!store.refresh_manager_review_assignment(current).unwrap());
}

#[tokio::test]
async fn final_tool_denial_and_budget_endings_do_not_relaunch() {
    for (status, error_class, tool_denials, code) in [
        (
            "completed",
            None,
            None,
            "manager_review_receipt_missing_final_without_receipt",
        ),
        (
            "completed",
            None,
            Some(2),
            "manager_review_receipt_missing_final_without_receipt",
        ),
        (
            "failed",
            Some("over_budget_actual_exceeded"),
            None,
            "manager_review_receipt_missing_budget_exceeded",
        ),
    ] {
        let f = fixture().await;
        let assignment = request_and_activate_db_review(&f, status).await;
        let store = f.handle.store.lock().await;
        super::review_end::bind_review_invocation(&store, &f, assignment);
        store
            .update_session_status(f.reviewer, SessionStatus::Completed)
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET permission_denial_count=?2 WHERE id=?1",
                params![f.reviewer.to_string(), tool_denials],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE model_invocations SET status=?2,error_class=?3 WHERE id=?1",
                params![f.invocation.to_string(), status, error_class],
            )
            .unwrap();
        assert!(store.refresh_manager_review_assignment(assignment).unwrap());
        assert_eq!(
            review_row(&store, assignment),
            ("failed".into(), Some(code.into()))
        );
        assert_eq!(assignment_count(&store), 1);
    }
}

#[tokio::test]
async fn later_code_commit_reserves_retry_at_the_exact_sealed_sha() {
    let f = fixture().await;
    let assignment = request_and_activate_db_review(&f, "retry-source-moved").await;
    std::fs::write(f.source_root.join("code.txt"), "newer source\n").unwrap();
    command(&f.source_root, &["commit", "-am", "newer source"]);
    assert_ne!(
        command(&f.source_root, &["rev-parse", "HEAD"]),
        f.source_head
    );
    let store = f.handle.store.lock().await;
    fail_allocation_action(&store, assignment);
    assert!(store.refresh_manager_review_assignment(assignment).unwrap());
    assert_eq!(review_row(&store, assignment).0, "superseded");
    let next = successor(&store, assignment);
    assert_eq!(review_row(&store, next).0, "reserved");
    assert_same_review(&store, assignment, next, &f.source_head);
    assert_eq!(assignment_count(&store), 2);
}

#[tokio::test]
async fn non_ancestor_seal_still_refuses_infrastructure_retry() {
    let f = fixture().await;
    let assignment = request_and_activate_db_review(&f, "retry-non-ancestor").await;
    // This is a disposable fixture worktree: replace its history with a
    // sibling of the sealed commit, then confirm the seal no longer holds.
    command(&f.source_root, &["reset", "--hard", &f.allocation_commit]);
    std::fs::write(f.source_root.join("code.txt"), "sibling source\n").unwrap();
    command(&f.source_root, &["commit", "-am", "sibling source"]);
    let store = f.handle.store.lock().await;
    fail_allocation_action(&store, assignment);
    assert!(store.refresh_manager_review_assignment(assignment).unwrap());
    assert_eq!(
        review_row(&store, assignment),
        (
            "failed".into(),
            Some("manager_review_source_changed".into())
        )
    );
    assert_eq!(assignment_count(&store), 1);
}

/// A relaunch the policy refuses fails typed and does not loop. The refusal is
/// a revoked SessionCreate capability: since #674 K15a the lifetime creation
/// limit no longer charges DB-native review launches, so it cannot refuse one.
#[tokio::test]
async fn refused_relaunch_fails_typed_without_loop() {
    let f = fixture().await;
    let assignment = request_and_activate_db_review(&f, "retry-limit").await;
    let store = f.handle.store.lock().await;
    fail_allocation_action(&store, assignment);
    assert!(store.refresh_manager_review_assignment(assignment).unwrap());
    let next = successor(&store, assignment);
    store.conn.execute(
        "UPDATE harness_manager_v2_policies
            SET policy_json=json_set(policy_json,'$.capabilities',json('[]'))
          WHERE project_id=(SELECT project_id FROM manager_review_assignments WHERE assignment_id=?1)",
        [next.to_string()],
    ).unwrap();
    assert_eq!(
        store.reconcile_manager_review_assignments_once().unwrap().0,
        1
    );
    assert_eq!(
        review_row(&store, next),
        (
            "failed".into(),
            Some("manager_review_infra_relaunch_refused".into())
        )
    );
    assert_eq!(assignment_count(&store), 2);
    assert_eq!(
        store.reconcile_manager_review_assignments_once().unwrap().0,
        0
    );
}

/// #674 K15a: an infrastructure relaunch is a DB-native review launch, so an
/// exhausted lifetime creation budget does not refuse it; it is journaled and
/// linked like the first allocation.
#[tokio::test]
async fn creation_limit_does_not_refuse_review_relaunch() {
    let f = fixture().await;
    let assignment = request_and_activate_db_review(&f, "retry-limit-uncharged").await;
    let store = f.handle.store.lock().await;
    fail_allocation_action(&store, assignment);
    assert!(store.refresh_manager_review_assignment(assignment).unwrap());
    let next = successor(&store, assignment);
    store.conn.execute(
        "UPDATE harness_manager_v2_policies
            SET policy_json=json_set(policy_json,'$.max_created_sessions',1)
          WHERE project_id=(SELECT project_id FROM manager_review_assignments WHERE assignment_id=?1)",
        [next.to_string()],
    ).unwrap();
    store.reconcile_manager_review_assignments_once().unwrap();
    assert_eq!(review_row(&store, next), ("allocating".into(), None));
    let linked: bool = store
        .conn
        .query_row(
            "SELECT action_operation_id IS NOT NULL FROM manager_review_assignments WHERE assignment_id=?1",
            [next.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(linked, "the relaunch must be journaled and linked");
    assert_eq!(assignment_count(&store), 2);
}
