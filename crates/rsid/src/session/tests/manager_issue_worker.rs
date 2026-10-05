//! #1100: one manager verb launches one Issue-bound worker.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening,
    clippy::large_futures
)]

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
use super::*;
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
use rsi_common::manager_issue_worker::{
    AgentManagerLaunchIssueWorkerRequestV1, AgentManagerLaunchIssueWorkerResultV1,
    ManagerIssueWorkerWatchV1,
};
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
use rsi_common::rpc::AgentGetIssueRequestV1;
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
use rsi_common::types::{Issue, IssueStatus, NewIssue, WakeMode};

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn grant_issue_coordinate(p: &Pilot) {
    let mut policy = p.policy.clone();
    policy
        .capabilities
        .push(ManagerCapabilityV2::IssueCoordinate);
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: "issue-coordinate".into(),
            policy,
        })
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn new_issue(p: &Pilot, title: &str) -> Issue {
    p.manager
        .store
        .lock()
        .await
        .create_issue(&NewIssue {
            project_id: p.project,
            title: title.into(),
            body: "Build the thing.".into(),
            priority: None,
            labels: Vec::new(),
            created_by_session_id: None,
            assignee: None,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
        })
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn issue_row(p: &Pilot, id: Uuid) -> (String, i64, String) {
    p.manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT status,row_version,body FROM issues WHERE id=?1",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn counts(p: &Pilot) -> (i64, i64, i64) {
    let store = p.manager.store.lock().await;
    let one = |sql: &str| -> i64 { store.conn.query_row(sql, [], |row| row.get(0)).unwrap() };
    (
        one("SELECT count(*) FROM harness_manager_v2_operations WHERE kind='lifecycle_action'"),
        one("SELECT count(*) FROM issue_events"),
        one("SELECT count(*) FROM sessions"),
    )
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn request(issue: i64, key: &str, p: &Pilot) -> AgentManagerLaunchIssueWorkerRequestV1 {
    AgentManagerLaunchIssueWorkerRequestV1 {
        issue,
        brief: "Implement the Issue and report.".into(),
        launch: p.policy.allowed_launches[0].clone(),
        parent_epic_id: None,
        idempotency_key: key.into(),
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn watches_on(p: &Pilot, worker: Uuid) -> Vec<Uuid> {
    p.manager
        .store
        .lock()
        .await
        .list_enabled_terminal_watches()
        .unwrap()
        .into_iter()
        .filter(|job| job.wake_mode == WakeMode::OnTerminal(worker))
        .filter_map(|job| job.wake_session_id)
        .collect()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_call_yields_worker_note_in_progress_and_watch_and_replays() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let issue = new_issue(&p, "bound work").await;
    let other = new_issue(&p, "someone else's work").await;
    let control = p.manager.agent_control();

    let first = control
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "launch-1", &p))
        .await
        .unwrap();
    assert!(!first.deduplicated);
    assert_eq!(first.issue_id, issue.id);
    assert_eq!(first.issue_status, IssueStatus::InProgress);
    assert_eq!(first.watch, ManagerIssueWorkerWatchV1::PendingLaunch);
    let worker = first.worker_session_id;
    assert_eq!(first.action.target_session_id, Some(worker));
    let (status, version, body) = issue_row(&p, issue.id).await;
    assert_eq!(status, "InProgress");
    assert_eq!(version, first.issue_row_version);
    assert!(body.starts_with("Build the thing."));
    assert!(body.contains(&worker.to_string()), "{body}");

    // The launch runs through the ordinary create_session path.
    let process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();
    let established = p.receipt(first.action.operation_id).await;
    assert_eq!(established.state, ManagerActionStateV2::Succeeded);
    assert_eq!(established.outcome.as_deref(), Some("session_established"));
    let row = p
        .manager
        .store
        .lock()
        .await
        .get_session(worker)
        .unwrap()
        .unwrap();
    assert_eq!(row.parent_id, Some(p.epic));
    assert!(
        row.query
            .contains(&format!("Issue #{}", issue.display_number))
    );
    // #1115: the watch committed with the launch success itself (nothing
    // armed it afterwards), so no crash window leaves the worker unwatched.
    assert_eq!(watches_on(&p, worker).await, vec![p.owner]);

    // A replay returns the same worker and applies nothing twice.
    let before = counts(&p).await;
    let replay = control
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "launch-1", &p))
        .await
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.worker_session_id, worker);
    assert_eq!(replay.watch, ManagerIssueWorkerWatchV1::Armed);
    assert_eq!(counts(&p).await, before);
    assert_eq!(issue_row(&p, issue.id).await.1, version);
    assert_eq!(watches_on(&p, worker).await, vec![p.owner]);

    // The bound worker reads its own Issue and only that one.
    let store = p.manager.store.lock().await;
    let own = store
        .agent_get_issue(
            worker,
            &AgentGetIssueRequestV1 {
                issue_id: None,
                display_number: Some(issue.display_number),
            },
        )
        .unwrap();
    assert_eq!(own.issue.id, issue.id);
    for target in [
        AgentGetIssueRequestV1 {
            issue_id: None,
            display_number: Some(other.display_number),
        },
        AgentGetIssueRequestV1 {
            issue_id: Some(other.id),
            display_number: None,
        },
        AgentGetIssueRequestV1 {
            issue_id: None,
            display_number: Some(9_999),
        },
    ] {
        let error = store.agent_get_issue(worker, &target).unwrap_err();
        assert!(error.to_string().contains("authority_denied"), "{error}");
    }
    // The catalog advertises the read to the bound worker.
    let projection = store.agent_authority_projection(worker).unwrap();
    assert!(
        projection
            .verbs
            .contains(&rsi_common::agent_control_schema::AgentControlVerbV1::GetIssue)
    );
    drop(store);

    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, worker)
        .await
        .unwrap();
    crate::session::launch::drop_controller_candidate_test_stream(worker);
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn refusals_before_any_effect_leave_no_action_note_or_status_change() {
    let p = pilot().await;
    let issue = new_issue(&p, "refused work").await;
    let control = p.manager.agent_control();
    let baseline = counts(&p).await;
    let row = issue_row(&p, issue.id).await;

    // No IssueCoordinate grant yet.
    let error = control
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "k-grant", &p))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("authority_denied"), "{error}");
    assert_eq!(counts(&p).await, baseline);
    assert_eq!(issue_row(&p, issue.id).await, row);

    grant_issue_coordinate(&p).await;

    // A launch outside the allowlist.
    let mut off_list = request(issue.display_number, "k-allowlist", &p);
    off_list.launch.model = "not-allowed".into();
    let error = control
        .agent_manager_launch_issue_worker(p.owner, off_list)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("manager_v2"), "{error}");
    assert_eq!(counts(&p).await, baseline);
    assert_eq!(issue_row(&p, issue.id).await, row);

    // An unknown Issue.
    let error = control
        .agent_manager_launch_issue_worker(p.owner, request(9_999, "k-missing", &p))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("issue_unavailable"), "{error}");
    assert_eq!(counts(&p).await, baseline);

    // A Closed Issue rolls the admitted action back with it.
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE issues SET status='Closed' WHERE id=?1",
            [issue.id.to_string()],
        )
        .unwrap();
    let closed = issue_row(&p, issue.id).await;
    let error = control
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "k-closed", &p))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("issue_unavailable"), "{error}");
    assert_eq!(counts(&p).await, baseline);
    assert_eq!(issue_row(&p, issue.id).await, closed);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_watch_of_a_live_bound_worker_is_reconciled_idempotently() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let issue = new_issue(&p, "crash window").await;
    let launched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "crash-1", &p))
        .await
        .unwrap();
    let worker = launched.worker_session_id;
    let process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();
    assert_eq!(watches_on(&p, worker).await, vec![p.owner]);
    // Nothing is owed while the watch exists.
    assert_eq!(p.manager.reconcile_issue_worker_watches().await, 0);

    // Simulate the pre-fix crash window: the launch committed, the watch never
    // landed (or was lost).
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "DELETE FROM scheduled_jobs WHERE wake_mode=?1",
            [format!("on_terminal:{worker}")],
        )
        .unwrap();
    assert!(watches_on(&p, worker).await.is_empty());

    assert_eq!(p.manager.reconcile_issue_worker_watches().await, 1);
    assert_eq!(watches_on(&p, worker).await, vec![p.owner]);
    assert_eq!(p.manager.reconcile_issue_worker_watches().await, 0);
    assert_eq!(watches_on(&p, worker).await, vec![p.owner]);

    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, worker)
        .await
        .unwrap();
    crate::session::launch::drop_controller_candidate_test_stream(worker);
    drop(process);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn action_result_notices(p: &Pilot, operation: Uuid) -> i64 {
    p.manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_notices
             WHERE kind='action_result' AND subject_id=?1 AND retired_at IS NULL",
            [operation.to_string()],
            |row| row.get(0),
        )
        .unwrap()
}

/// Launch an Issue worker from the fixture checkout put on `rolling` with a
/// bare `origin`; returns the origin, the commit pinned at prepare time and the
/// queued operation.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn issue_launch_on_rolling(
    p: &Pilot,
    key: &str,
) -> (
    std::path::PathBuf,
    String,
    AgentManagerLaunchIssueWorkerResultV1,
) {
    grant_issue_coordinate(p).await;
    let origin = rolling_checkout_with_origin(p);
    let issue = new_issue(p, key).await;
    let pinned = git(&p.repo, &["rev-parse", "HEAD"]);
    let launched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, key, p))
        .await
        .unwrap();
    (origin, pinned, launched)
}

/// #1144: assert the moved parent source blocks the launch at the source gate.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn assert_moved_source_refused(p: &Pilot, operation: Uuid) {
    p.manager.reconcile_manager_actions_once().await.unwrap();
    let receipt = p.receipt(operation).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Blocked);
    assert_eq!(
        receipt.outcome.as_deref(),
        Some("manager_v2_source_changed")
    );
}

/// #1133/#1144: a parent working_dir that moved by a verified published
/// fast-forward on the same branch between prepare and establishment (the
/// operator's `git pull` of `origin/rolling`) still establishes the launch.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn operator_pulled_fast_forward_parent_source_still_establishes_the_issue_launch() {
    let p = pilot().await;
    let (origin, pinned, launched) = issue_launch_on_rolling(&p, "ff-1").await;
    let worker = launched.worker_session_id;

    let published = advance_origin_rolling(&p, &origin);
    git(&p.repo, &["fetch", "-q", "origin"]);
    git(&p.repo, &["merge", "-q", "--ff-only", "origin/rolling"]);
    assert_eq!(git(&p.repo, &["rev-parse", "HEAD"]), published);
    assert_ne!(published, pinned);
    // Origin moves again after the pull: a newer fetched tip must not become
    // the child's base either.
    let scratch = p.repo.parent().unwrap().join("scratch-newer");
    git(
        p.repo.parent().unwrap(),
        &[
            "clone",
            "-q",
            "-b",
            "rolling",
            origin.to_str().unwrap(),
            scratch.to_str().unwrap(),
        ],
    );
    git(&scratch, &["config", "user.name", "Newer origin"]);
    git(&scratch, &["config", "user.email", "newer@example.invalid"]);
    std::fs::write(scratch.join("newer"), "newer than the pull\n").unwrap();
    git(&scratch, &["add", "newer"]);
    git(&scratch, &["commit", "-qm", "origin newer than the pull"]);
    git(
        &scratch,
        &["push", "-q", "origin", "HEAD:refs/heads/rolling"],
    );
    let newest = git(&scratch, &["rev-parse", "HEAD"]);
    assert_ne!(newest, published);

    let process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();
    let receipt = p.receipt(launched.action.operation_id).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Succeeded);
    assert_eq!(receipt.outcome.as_deref(), Some("session_established"));
    assert_eq!(watches_on(&p, worker).await, vec![p.owner]);

    // #1144: the child sandbox is allocated from the EXACT commit frozen at
    // prepare time, not the moved HEAD and not the newer fetched origin tip.
    let root = p
        .manager
        .store
        .lock()
        .await
        .get_session(worker)
        .unwrap()
        .unwrap()
        .sandbox_root
        .unwrap();
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), pinned);
    assert_ne!(git(&root, &["rev-parse", "HEAD"]), published);
    assert_ne!(git(&root, &["rev-parse", "HEAD"]), newest);
    assert!(git(&p.repo, &["for-each-ref", "refs/rsi"]).is_empty());

    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, worker)
        .await
        .unwrap();
    crate::session::launch::drop_controller_candidate_test_stream(worker);
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
}

/// #1144: a clean local commit that was never pushed is no published
/// fast-forward, even though it descends from the pin.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unpublished_local_commit_blocks_the_issue_launch() {
    let p = pilot().await;
    let (_origin, _pinned, launched) = issue_launch_on_rolling(&p, "unpushed-1").await;
    std::fs::write(p.repo.join("source"), "another writer, never pushed\n").unwrap();
    git(&p.repo, &["add", "source"]);
    git(&p.repo, &["commit", "-qm", "unpublished local commit"]);
    assert_moved_source_refused(&p, launched.action.operation_id).await;
}

/// #1144: a detached HEAD at a published descendant is not the same symbolic
/// branch the source was frozen on.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detached_head_at_a_published_descendant_blocks_the_issue_launch() {
    let p = pilot().await;
    let (origin, _pinned, launched) = issue_launch_on_rolling(&p, "detached-1").await;
    advance_origin_rolling(&p, &origin);
    git(&p.repo, &["fetch", "-q", "origin"]);
    git(&p.repo, &["checkout", "-q", "--detach", "origin/rolling"]);
    assert_moved_source_refused(&p, launched.action.operation_id).await;
}

/// #1144: a source switched to another clean branch at a published
/// descendant is not the same symbolic branch.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switched_branch_at_a_published_descendant_blocks_the_issue_launch() {
    let p = pilot().await;
    let (origin, _pinned, launched) = issue_launch_on_rolling(&p, "switched-1").await;
    advance_origin_rolling(&p, &origin);
    git(&p.repo, &["fetch", "-q", "origin"]);
    git(
        &p.repo,
        &["checkout", "-q", "-b", "other", "origin/rolling"],
    );
    assert_moved_source_refused(&p, launched.action.operation_id).await;
}

/// #1133: a rewrite (the pinned commit is no ancestor of the new HEAD) still
/// refuses, and the blocked Issue-bound launch leaves exactly one manager
/// notice naming the cause for the operation.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rewritten_parent_source_blocks_the_issue_launch_with_one_manager_notice() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let issue = new_issue(&p, "rewrite work").await;
    let launched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "rw-1", &p))
        .await
        .unwrap();
    let operation = launched.action.operation_id;

    git(
        &p.repo,
        &["commit", "--amend", "-qm", "rewritten after prepare"],
    );

    p.manager.reconcile_manager_actions_once().await.unwrap();
    let receipt = p.receipt(operation).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Blocked);
    assert_eq!(
        receipt.outcome.as_deref(),
        Some("manager_v2_source_changed")
    );
    assert_eq!(action_result_notices(&p, operation).await, 1);
    // A second reconcile pass does not queue a second notice.
    p.manager.reconcile_manager_actions_once().await.unwrap();
    assert_eq!(action_result_notices(&p, operation).await, 1);
}
