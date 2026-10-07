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
use rsi_common::rpc::{
    AgentGetIssueRequestV1, AgentUpdateIssueRequestV1, AgentUpdateIssueStatusRequestV1,
};
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
        project_id: None,
        issue,
        brief: "Implement the Issue and report.".into(),
        launch: p.policy.allowed_launches[0].clone(),
        parent_epic_id: None,
        idempotency_key: key.into(),
        sandbox_source: None,
        continue_from: None,
        qa_lane: false,
        review_of: None,
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
    let mut p = pilot().await;
    // The installed fake process is keyed by session, not by model name;
    // launch preflight still validates against the real model catalog.
    p.policy.allowed_launches[0].model = "claude-sonnet-5".into();
    grant_issue_coordinate(&p).await;
    let issue = new_issue(&p, "bound work").await;
    let other = new_issue(&p, "someone else's work").await;
    let control = p.manager.agent_control();
    let qa_request = || {
        let mut launch = request(issue.display_number, "launch-1", &p);
        launch.qa_lane = true;
        launch
    };

    let first = control
        .agent_manager_launch_issue_worker(p.owner, qa_request())
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

    // The manager-supplied grant persists in the established binding. Reusing
    // its launch key with a changed grant cannot silently alter authority.
    assert!(
        p.manager
            .store
            .lock()
            .await
            .live_qa_lane_binding(worker)
            .unwrap()
            .is_some()
    );
    let changed = control
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "launch-1", &p))
        .await
        .unwrap_err();
    assert!(
        changed
            .to_string()
            .contains("manager_v2_idempotency_conflict"),
        "{changed}"
    );

    // A replay returns the same worker and applies nothing twice.
    let before = counts(&p).await;
    let replay = control
        .agent_manager_launch_issue_worker(p.owner, qa_request())
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
                project_id: None,
                issue_id: None,
                display_number: Some(issue.display_number),
            },
        )
        .unwrap();
    assert_eq!(own.issue.id, issue.id);
    for target in [
        AgentGetIssueRequestV1 {
            project_id: None,
            issue_id: None,
            display_number: Some(other.display_number),
        },
        AgentGetIssueRequestV1 {
            project_id: None,
            issue_id: Some(other.id),
            display_number: None,
        },
        AgentGetIssueRequestV1 {
            project_id: None,
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

/// #1284: a body-only append to `issue` as `caller`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn append_request(
    issue: &Issue,
    expected_row_version: i64,
    body: String,
    key: &str,
) -> AgentUpdateIssueRequestV1 {
    AgentUpdateIssueRequestV1 {
        project_id: None,
        issue_id: None,
        display_number: Some(issue.display_number),
        expected_row_version,
        idempotency_key: key.into(),
        title: None,
        body: Some(body),
        labels: None,
        priority: None,
        clear_priority: false,
        assignee: None,
        clear_assignee: false,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn row_in(store: &crate::store::Store, id: Uuid) -> (String, i64, String) {
    store
        .conn
        .query_row(
            "SELECT status,row_version,body FROM issues WHERE id=?1",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn lists_update_issue(store: &crate::store::Store, session: Uuid) -> bool {
    store
        .agent_authority_projection(session)
        .unwrap()
        .verbs
        .contains(&rsi_common::agent_control_schema::AgentControlVerbV1::UpdateIssue)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bound_worker_appends_to_its_own_issue_and_nothing_else() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let issue = new_issue(&p, "bound handoff").await;
    let other = new_issue(&p, "another Issue").await;
    let launched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "append-1", &p))
        .await
        .unwrap();
    let worker = launched.worker_session_id;
    let process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();
    // The brief tells the worker it may append its handoff.
    {
        let store = p.manager.store.lock().await;
        let row = store.get_session(worker).unwrap().unwrap();
        assert!(row.query.contains("append your handoff"), "{}", row.query);
        // #1332: and to end it with the Friction field.
        assert!(
            row.query
                .contains("`Friction: none | #N[, #M] | <one line, not filed because ...>`"),
            "{}",
            row.query
        );
    }

    let store = p.manager.store.lock().await;
    assert!(lists_update_issue(&store, worker));
    let (status, version, body) = row_in(&store, issue.id);
    let denied = |error: crate::error::DaemonError| {
        assert!(error.to_string().contains("authority_denied"), "{error}");
    };
    // #1545: each bound-worker refusal names its own failed condition and the
    // field it concerns; the next action never sends a bound worker to a lead.
    let refused = |error: crate::error::DaemonError, code: &str, field: Option<&str>| {
        let message = error.to_string();
        assert!(
            message.contains(&format!("agent_issue_{code}")),
            "{message}"
        );
        let crate::error::DaemonError::StructuredRpc { data, .. } = error else {
            panic!("expected a structured Issue envelope: {message}");
        };
        assert_eq!(data["code"], code, "{data}");
        assert_eq!(
            data.pointer("/validation/field").and_then(|f| f.as_str()),
            field,
            "{data}"
        );
        let next_action = data["next_action"].as_str().unwrap();
        assert!(!next_action.contains("lead"), "{next_action}");
    };

    // Refused: another Issue, by number or by id, leaves it untouched.
    let other_row = row_in(&store, other.id);
    refused(
        store
            .agent_update_issue(
                worker,
                &append_request(
                    &other,
                    other.row_version,
                    format!("{}\nx", other.body),
                    "o-1",
                ),
            )
            .unwrap_err(),
        "bound_issue_wrong_issue",
        None,
    );
    let mut by_id = append_request(
        &other,
        other.row_version,
        format!("{}\nx", other.body),
        "o-2",
    );
    by_id.display_number = None;
    by_id.issue_id = Some(other.id);
    refused(
        store.agent_update_issue(worker, &by_id).unwrap_err(),
        "bound_issue_wrong_issue",
        None,
    );
    // So is naming another project.
    let mut foreign = append_request(&issue, version, format!("{body}\nmore"), "o-3");
    foreign.project_id = Some(Uuid::new_v4());
    refused(
        store.agent_update_issue(worker, &foreign).unwrap_err(),
        "bound_issue_wrong_issue",
        None,
    );
    assert_eq!(row_in(&store, other.id), other_row);

    // Refused on its own Issue: a rewrite, and an append that also edits
    // another field.
    refused(
        store
            .agent_update_issue(
                worker,
                &append_request(&issue, version, "Replaced.".into(), "w-1"),
            )
            .unwrap_err(),
        "bound_issue_not_append_only",
        None,
    );
    // A body that merely contains the current one (not as its prefix) is not
    // an append either.
    refused(
        store
            .agent_update_issue(
                worker,
                &append_request(&issue, version, format!("~{body}\nmore"), "w-1b"),
            )
            .unwrap_err(),
        "bound_issue_not_append_only",
        None,
    );
    let mut titled = append_request(&issue, version, format!("{body}\nmore"), "w-2");
    titled.title = Some("Renamed".into());
    refused(
        store.agent_update_issue(worker, &titled).unwrap_err(),
        "bound_issue_field_not_allowed",
        Some("title"),
    );
    let mut labelled = append_request(&issue, version, format!("{body}\nmore"), "w-3");
    labelled.labels = Some(vec!["mine".into()]);
    refused(
        store.agent_update_issue(worker, &labelled).unwrap_err(),
        "bound_issue_field_not_allowed",
        Some("labels"),
    );
    let mut bodiless = append_request(&issue, version, String::new(), "w-4");
    bodiless.body = None;
    bodiless.priority = Some(3);
    refused(
        store.agent_update_issue(worker, &bodiless).unwrap_err(),
        "bound_issue_field_not_allowed",
        Some("priority"),
    );
    // Refused: a status change on its own Issue.
    denied(
        store
            .agent_update_issue_status(
                worker,
                &AgentUpdateIssueStatusRequestV1 {
                    project_id: None,
                    issue_id: Some(issue.id),
                    display_number: None,
                    status: IssueStatus::Closed,
                    expected_row_version: version,
                    idempotency_key: "w-status".into(),
                },
            )
            .unwrap_err(),
    );
    assert_eq!(
        row_in(&store, issue.id),
        (status.clone(), version, body.clone())
    );

    // Allowed: an append to its own Issue, recorded as the worker itself; a
    // replay under the same key applies nothing twice.
    let appended = format!("{body}\n\n## Baton handoff\ndone: the store path");
    let append = append_request(&issue, version, appended.clone(), "w-append");
    let result = store.agent_update_issue(worker, &append).unwrap();
    assert!(!result.deduplicated);
    assert_eq!(result.issue.body, appended);
    assert_eq!(result.issue.row_version, version + 1);
    assert_eq!(result.issue.title, issue.title);
    assert_eq!(
        row_in(&store, issue.id),
        (status.clone(), version + 1, appended.clone())
    );
    assert_eq!(result.event.actor_session_id, Some(worker));
    // Recorded under the worker's own Epic, as the worker, not its lead.
    assert_eq!(result.event.owning_epic_id, Some(p.epic));
    assert!(
        store
            .agent_update_issue(worker, &append)
            .unwrap()
            .deduplicated
    );

    // A rotation successor of the worker holds no binding of its own.
    let rotated = Uuid::new_v4();
    let mut row = bare_session(rotated);
    row.project_id = Some(p.project);
    row.working_dir = p.repo.clone();
    row.session_kind = SessionKind::Task;
    row.parent_id = Some(p.epic);
    row.continued_from = Some(worker);
    row.status = SessionStatus::Running;
    store.insert_session(&row).unwrap();
    denied(
        store
            .agent_update_issue(
                rotated,
                &append_request(&issue, version + 1, format!("{appended}\nr"), "r-1"),
            )
            .unwrap_err(),
    );
    assert!(!lists_update_issue(&store, rotated));
    drop(store);

    // The binding ends with the worker: its append is refused at effect time
    // and the catalog stops listing the control. It still reads its Issue.
    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, worker)
        .await
        .unwrap();
    crate::session::launch::drop_controller_candidate_test_stream(worker);
    drop(process);
    let store = p.manager.store.lock().await;
    store
        .conn
        .execute(
            "UPDATE sessions SET status='Completed' WHERE id=?1",
            [worker.to_string()],
        )
        .unwrap();
    refused(
        store
            .agent_update_issue(
                worker,
                &append_request(&issue, version + 1, format!("{appended}\nlate"), "w-late"),
            )
            .unwrap_err(),
        "bound_issue_binding_not_live",
        None,
    );
    assert!(!lists_update_issue(&store, worker));
    assert!(
        store
            .agent_get_issue(
                worker,
                &AgentGetIssueRequestV1 {
                    project_id: None,
                    issue_id: Some(issue.id),
                    display_number: None,
                },
            )
            .is_ok()
    );
    assert_eq!(row_in(&store, issue.id), (status, version + 1, appended));
}

/// #1595: the manager closing the Issue while the worker is still live must
/// not cost the worker its final append-only handoff; nothing else widens.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bound_worker_appends_to_its_issue_after_the_manager_closed_it() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let issue = new_issue(&p, "closed under the worker").await;
    let launched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "closed-1", &p))
        .await
        .unwrap();
    let worker = launched.worker_session_id;
    let process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();

    let store = p.manager.store.lock().await;
    store
        .conn
        .execute(
            "UPDATE issues SET status='Closed', closed_at=updated_at, row_version=row_version+1
             WHERE id=?1",
            [issue.id.to_string()],
        )
        .unwrap();
    let (status, version, body) = row_in(&store, issue.id);
    assert_eq!(status, "Closed");

    // Another actor keeps the closed-Issue fence.
    let manager_edit = store
        .agent_update_issue(
            p.owner,
            &append_request(&issue, version, format!("{body}\nmanager"), "c-mgr"),
        )
        .unwrap_err();
    assert!(
        manager_edit
            .to_string()
            .contains("agent_issue_invalid_transition"),
        "{manager_edit}"
    );
    // The bound worker still may not rewrite or touch another field.
    let rewrite = store
        .agent_update_issue(
            worker,
            &append_request(&issue, version, "Replaced.".into(), "c-rewrite"),
        )
        .unwrap_err();
    assert!(
        rewrite
            .to_string()
            .contains("agent_issue_bound_issue_not_append_only"),
        "{rewrite}"
    );
    let mut titled = append_request(&issue, version, format!("{body}\nmore"), "c-title");
    titled.title = Some("Renamed".into());
    let titled = store.agent_update_issue(worker, &titled).unwrap_err();
    assert!(
        titled
            .to_string()
            .contains("agent_issue_bound_issue_field_not_allowed"),
        "{titled}"
    );
    assert_eq!(
        row_in(&store, issue.id),
        (status.clone(), version, body.clone())
    );

    // The append-only handoff lands and leaves the Issue closed.
    let appended = format!("{body}\n\n## Handoff\nRESULT after close");
    let result = store
        .agent_update_issue(
            worker,
            &append_request(&issue, version, appended.clone(), "c-append"),
        )
        .unwrap();
    assert_eq!(result.issue.body, appended);
    assert_eq!(row_in(&store, issue.id), (status, version + 1, appended));
    drop(store);

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
/// bare `origin`; returns the origin, the checkout HEAD at admission (also the
/// origin `rolling` tip then) and the queued operation.
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

/// Establish the launched Issue worker and return its sandbox HEAD.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn establish_issue_worker(
    p: &Pilot,
    launched: &AgentManagerLaunchIssueWorkerResultV1,
) -> String {
    let worker = launched.worker_session_id;
    let process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();
    let receipt = p.receipt(launched.action.operation_id).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Succeeded);
    assert_eq!(receipt.outcome.as_deref(), Some("session_established"));
    assert_eq!(watches_on(p, worker).await, vec![p.owner]);
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
    let head = git(&root, &["rev-parse", "HEAD"]);
    assert!(git(&p.repo, &["for-each-ref", "refs/rsi"]).is_empty());
    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, worker)
        .await
        .unwrap();
    crate::session::launch::drop_controller_candidate_test_stream(worker);
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    head
}

/// #1195 (supersedes the #1144 pin of the checkout HEAD): an Issue worker
/// branches from the published `rolling` tip observed at launch, not from the
/// shared checkout, even after the operator pulled and origin moved again.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn issue_launch_branches_from_the_fresh_origin_tip_at_launch() {
    let p = pilot().await;
    let (origin, pinned, launched) = issue_launch_on_rolling(&p, "ff-1").await;

    let published = advance_origin_rolling(&p, &origin);
    git(&p.repo, &["fetch", "-q", "origin"]);
    git(&p.repo, &["merge", "-q", "--ff-only", "origin/rolling"]);
    assert_eq!(git(&p.repo, &["rev-parse", "HEAD"]), published);
    assert_ne!(published, pinned);
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

    assert_eq!(establish_issue_worker(&p, &launched).await, newest);
    // The shared checkout's refs are untouched by the observation.
    assert_eq!(
        git(&p.repo, &["rev-parse", "refs/heads/rolling"]),
        published
    );
}

/// #1195: a clean local commit that was never pushed never becomes the
/// worker's base, and no longer blocks the launch either.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unpublished_local_commit_never_becomes_the_issue_worker_base() {
    let p = pilot().await;
    let (_origin, pinned, launched) = issue_launch_on_rolling(&p, "unpushed-1").await;
    std::fs::write(p.repo.join("source"), "another writer, never pushed\n").unwrap();
    git(&p.repo, &["add", "source"]);
    git(&p.repo, &["commit", "-qm", "unpublished local commit"]);
    let local = git(&p.repo, &["rev-parse", "HEAD"]);
    let head = establish_issue_worker(&p, &launched).await;
    assert_eq!(head, pinned);
    assert_ne!(head, local);
}

/// #1195: a checkout taken off `rolling` (detached or another branch) keeps
/// the existing fallback: the commit observed when the action was admitted.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checkout_off_rolling_falls_back_to_the_admitted_head() {
    for (key, args) in [
        (
            "detached-1",
            &["checkout", "-q", "--detach", "origin/rolling"][..],
        ),
        (
            "switched-1",
            &["checkout", "-q", "-b", "other", "origin/rolling"][..],
        ),
    ] {
        let p = pilot().await;
        let (origin, pinned, launched) = issue_launch_on_rolling(&p, key).await;
        let published = advance_origin_rolling(&p, &origin);
        git(&p.repo, &["fetch", "-q", "origin"]);
        git(&p.repo, args);
        assert_eq!(git(&p.repo, &["rev-parse", "HEAD"]), published);
        assert_eq!(establish_issue_worker(&p, &launched).await, pinned, "{key}");
    }
}

/// #1133: a launch whose parent source changed after admission (here the
/// container moved to another checkout) refuses at the source gate, and the
/// blocked Issue-bound launch leaves exactly one manager notice naming the
/// cause for the operation.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changed_parent_source_blocks_the_issue_launch_with_one_manager_notice() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let issue = new_issue(&p, "moved work").await;
    let launched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "rw-1", &p))
        .await
        .unwrap();
    let operation = launched.action.operation_id;

    let moved = p.repo.parent().unwrap().join("moved-checkout");
    git(
        p.repo.parent().unwrap(),
        &[
            "clone",
            "-q",
            p.repo.to_str().unwrap(),
            moved.to_str().unwrap(),
        ],
    );
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET working_dir=?1 WHERE id=?2",
            rusqlite::params![moved.to_str().unwrap(), p.epic.to_string()],
        )
        .unwrap();

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

/// #1553: a launch that ends without its worker tells its Issue and frees it:
/// the launch's own note said a worker was launched, so the Issue gets a
/// second note naming the end state, and the same Issue relaunches at once.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_issue_launch_notes_the_issue_and_frees_it_for_a_relaunch() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let issue = new_issue(&p, "blocked work").await;
    let control = p.manager.agent_control();
    let launched = control
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "bl-1", &p))
        .await
        .unwrap();
    let operation = launched.action.operation_id;
    // The live launch holds the Issue.
    let refused = launch_refusal(&p, request(issue.display_number, "bl-2", &p)).await;
    assert!(
        refused.contains("manager_issue_worker_already_live"),
        "{refused}"
    );

    let moved = p.repo.parent().unwrap().join("moved-checkout-1553");
    git(
        p.repo.parent().unwrap(),
        &[
            "clone",
            "-q",
            p.repo.to_str().unwrap(),
            moved.to_str().unwrap(),
        ],
    );
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET working_dir=?1 WHERE id=?2",
            rusqlite::params![moved.to_str().unwrap(), p.epic.to_string()],
        )
        .unwrap();
    p.manager.reconcile_manager_actions_once().await.unwrap();
    let receipt = p.receipt(operation).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Blocked);

    let (_, _, body) = issue_row(&p, issue.id).await;
    assert!(
        body.contains(&format!(
            "Launch (action {operation}) for this Issue ended blocked (manager_v2_source_changed)"
        )),
        "{body}"
    );
    // The binding is released: the relaunch is admitted.
    let relaunched = control
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "bl-3", &p))
        .await
        .unwrap();
    assert_ne!(relaunched.action.operation_id, operation);
}

/// #1195: an Issue launch with a source that is not a worktree of the project
/// refuses before any effect: no action, Issue note or status change.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn invalid_sandbox_source_refuses_the_issue_launch_before_any_effect() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let issue = new_issue(&p, "sourced work").await;
    let baseline = counts(&p).await;
    let row = issue_row(&p, issue.id).await;
    let elsewhere = p.repo.parent().unwrap().join("not-a-worktree");
    std::fs::create_dir(&elsewhere).unwrap();
    let mut sourced = request(issue.display_number, "k-source", &p);
    sourced.sandbox_source = Some(ManagerSandboxSourceV1::Path(
        elsewhere.to_str().unwrap().into(),
    ));
    let error = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, sourced)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains(MANAGER_SANDBOX_SOURCE_NOT_WORKTREE),
        "{error}"
    );
    assert_eq!(counts(&p).await, baseline);
    assert_eq!(issue_row(&p, issue.id).await, row);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn launch_refusal(p: &Pilot, request: AgentManagerLaunchIssueWorkerRequestV1) -> String {
    p.manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, request)
        .await
        .unwrap_err()
        .to_string()
}

/// #1590: `review_of` copies the implementer Issue's text and COMPLETE latest
/// handoff (landing filters included, even when oversized) into the bound
/// reviewer's brief; an unknown reviewed Issue is refused before any effect.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn review_of_copies_the_complete_implementer_handoff_into_the_reviewer_brief() {
    use rsi_common::manager_issue_worker::MANAGER_ISSUE_WORKER_REVIEWED_UNAVAILABLE;
    let mut p = pilot().await;
    p.policy.allowed_launches[0].model = "claude-sonnet-5".into();
    grant_issue_coordinate(&p).await;
    let implementer = new_issue(&p, "implementer work").await;
    let review = new_issue(&p, "review of implementer work").await;
    // A handoff far beyond the old 7000-char paste limit, ending in the
    // landing filters a reviewer needs.
    let handoff = format!(
        "Build the thing. Acceptance: it works.\n\n## Handoff\nRESULT abc\n{}\nLANDING FILTERS: rsid=shard:store-01:test(final_filter_marker)",
        "filler line of the handoff body\n".repeat(1_200)
    );
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE issues SET body=?2 WHERE id=?1",
            rusqlite::params![implementer.id.to_string(), handoff],
        )
        .unwrap();
    let reviewing = |key: &str, reviewed: i64| {
        let mut launch = request(review.display_number, key, &p);
        launch.review_of = Some(reviewed);
        launch
    };
    let baseline = counts(&p).await;
    let refused = launch_refusal(&p, reviewing("review-missing", 9_999)).await;
    assert!(
        refused.contains(MANAGER_ISSUE_WORKER_REVIEWED_UNAVAILABLE),
        "{refused}"
    );
    assert_eq!(counts(&p).await, baseline);

    let control = p.manager.agent_control();
    let launched = control
        .agent_manager_launch_issue_worker(
            p.owner,
            reviewing("review-1", implementer.display_number),
        )
        .await
        .unwrap();
    let worker = launched.worker_session_id;
    let process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();
    let query = p
        .manager
        .store
        .lock()
        .await
        .get_session(worker)
        .unwrap()
        .unwrap()
        .query;
    assert!(query.contains("Acceptance: it works."), "{query}");
    assert!(query.contains("LANDING FILTERS: rsid=shard:store-01:test(final_filter_marker)"));
    assert!(query.contains("bytes omitted from the middle"));
    assert!(query.contains(&format!("Issue #{}", review.display_number)));
    assert!(query.len() <= 32_768);
    // A replay reuses the journalled brief even after the reviewed Issue grew.
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE issues SET body=body||'\nlater addition' WHERE id=?1",
            [implementer.id.to_string()],
        )
        .unwrap();
    let replay = control
        .agent_manager_launch_issue_worker(
            p.owner,
            reviewing("review-1", implementer.display_number),
        )
        .await
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.worker_session_id, worker);
    drop(process);
}

/// #1254: an Issue worker that crosses its cap puts exactly one typed
/// `worker_context_cap` notice in its manager's inbox (and gets one baton
/// mail from it); once it has ended its turn, `continue_from` relaunches the
/// Issue from its committed HEAD with its lineage, display identity, final
/// message and the Issue's latest handoff. A running predecessor, another
/// Issue and an out-of-scope session are refused before any effect.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_cap_notifies_the_manager_and_continue_from_relaunches_from_its_head() {
    use crate::store::worker_baton::WorkerBatonOutcome;
    use rsi_common::manager_issue_worker::{
        MANAGER_ISSUE_WORKER_PREDECESSOR_LIVE, MANAGER_ISSUE_WORKER_PREDECESSOR_OTHER_ISSUE,
        MANAGER_ISSUE_WORKER_PREDECESSOR_OUT_OF_SCOPE,
    };
    let mut p = pilot().await;
    // The installed fake process is keyed by session, not by model name;
    // launch preflight still validates against the real model catalog.
    p.policy.allowed_launches[0].model = "claude-sonnet-5".into();
    let (_origin, _pinned, launched) = issue_launch_on_rolling(&p, "baton-1").await;
    p.manager.store.lock().await.conn.execute(
        "UPDATE harness_manager_v2_operations SET payload_json=json_set(payload_json,'$.issue_binding.qa_lane',json('true')) WHERE id=?1",
        [launched.action.operation_id.to_string()]).unwrap();
    let worker = launched.worker_session_id;
    let number = launched.issue_display_number;
    let continuing = |key: &str, issue: i64, from: Uuid| {
        let mut next = request(issue, key, &p);
        next.continue_from = Some(from);
        next
    };
    // Not launched yet: no bound worker to continue.
    let refused = launch_refusal(&p, continuing("baton-early", number, worker)).await;
    assert!(
        refused.contains(MANAGER_ISSUE_WORKER_PREDECESSOR_OUT_OF_SCOPE),
        "{refused}"
    );
    let process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(launched.action.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    assert!(
        p.manager
            .store
            .lock()
            .await
            .live_qa_lane_binding(worker)
            .unwrap()
            .is_some()
    );
    // A running predecessor is refused; the worker ends its own turn.
    let refused = launch_refusal(&p, continuing("baton-live", number, worker)).await;
    assert!(
        refused.contains(MANAGER_ISSUE_WORKER_PREDECESSOR_LIVE),
        "{refused}"
    );

    // The crossing: one notice in the manager's inbox, one baton mail.
    {
        let store = p.manager.store.lock().await;
        let now = chrono::Utc::now();
        assert_eq!(
            store
                .record_worker_context_cap(worker, 650_000, 600_000, now)
                .unwrap(),
            WorkerBatonOutcome::Sent
        );
        assert_eq!(
            store
                .record_worker_context_cap(worker, 700_000, 600_000, now)
                .unwrap(),
            WorkerBatonOutcome::AlreadyRecorded
        );
        let inbox = store.manager_inbox(p.owner, &Default::default()).unwrap();
        let caps: Vec<_> = inbox
            .notices
            .iter()
            .filter(|notice| notice.state["record_kind"] == "worker_context_cap")
            .collect();
        assert_eq!(caps.len(), 1, "{:?}", inbox.notices);
        assert_eq!(caps[0].state["worker"], worker.to_string());
        assert_eq!(caps[0].state["issue"], launched.issue_id.to_string());
        assert_eq!(caps[0].state["measured_tokens"], 650_000);
        assert_eq!(caps[0].state["cap"], 600_000);
        let mail: Vec<(String, String)> = {
            let mut statement = store
                .conn
                .prepare(
                    "SELECT owner_session_id, payload FROM agent_messages
                     WHERE target_session_id=?1",
                )
                .unwrap();
            statement
                .query_map([worker.to_string()], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap()
        };
        assert_eq!(mail.len(), 1);
        assert_eq!(mail[0].0, p.owner.to_string());
        assert!(
            mail[0].1.contains(&format!("Issue #{number}")),
            "{}",
            mail[0].1
        );
        assert!(mail[0].1.contains("PIPELINE HANDOFF — BATON <sha>"));
        assert!(
            mail[0]
                .1
                .contains("the next step and your `Friction:` line"),
            "{}",
            mail[0].1
        );
    }

    // The worker commits WIP, leaves one uncommitted edit, writes its final
    // message and a handoff on the Issue, and ends its turn.
    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, worker)
        .await
        .unwrap();
    crate::session::launch::drop_controller_candidate_test_stream(worker);
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    let predecessor = p
        .manager
        .store
        .lock()
        .await
        .get_session(worker)
        .unwrap()
        .unwrap();
    let root = predecessor.sandbox_root.clone().unwrap();
    std::fs::write(root.join("baton-work"), "half done\n").unwrap();
    git(&root, &["add", "baton-work"]);
    git(&root, &["commit", "-qm", "wip: half done"]);
    let head = git(&root, &["rev-parse", "HEAD"]);
    std::fs::write(root.join("baton-work"), "half done\nnot committed\n").unwrap();
    {
        let store = p.manager.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Completed' WHERE id=?1",
                [worker.to_string()],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO conversation_events (session_id, sequence, event_type, role, content, created_at)
                 VALUES (?1, 9001, 'Message', 'Assistant', ?2, ?3)",
                rusqlite::params![
                    worker.to_string(),
                    "Final words: half done.\nPIPELINE HANDOFF — BATON abc1234",
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE issues SET body = body || ?2 WHERE id=?1",
                rusqlite::params![
                    launched.issue_id.to_string(),
                    "\n\n## Baton handoff\nleft: wire the TUI row"
                ],
            )
            .unwrap();
    }

    // Another Issue and a session that is no bound worker are refused.
    let other = new_issue(&p, "other work").await;
    let refused = launch_refusal(&p, continuing("baton-other", other.display_number, worker)).await;
    assert!(
        refused.contains(MANAGER_ISSUE_WORKER_PREDECESSOR_OTHER_ISSUE),
        "{refused}"
    );
    for stranger in [Uuid::new_v4(), p.owner] {
        let refused = launch_refusal(&p, continuing("baton-stranger", number, stranger)).await;
        assert!(
            refused.contains(MANAGER_ISSUE_WORKER_PREDECESSOR_OUT_OF_SCOPE),
            "{refused}"
        );
    }

    // One call relaunches from the predecessor's HEAD; a replay is the same.
    p.manager
        .host_load()
        .set_source(Arc::new(|| crate::host_load::LoadReading::Load1(95.0)));
    let control = p.manager.agent_control();
    let next = continuing("baton-2", number, worker);
    let relaunched = control
        .agent_manager_launch_issue_worker(p.owner, next.clone())
        .await
        .unwrap();
    let successor = relaunched.worker_session_id;
    assert_ne!(successor, worker);
    let source = relaunched.action.sandbox_source.clone().expect("source");
    assert_eq!(source.commit.as_deref(), Some(head.as_str()));
    assert!(source.source_dirty);
    let replay = control
        .agent_manager_launch_issue_worker(p.owner, next)
        .await
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.worker_session_id, successor);

    let process = crate::session::launch::install_controller_candidate_test_process(successor);
    let view = control
        .agent_manager_get_action_view(
            p.owner,
            AgentManagerGetActionRequestV2 {
                project_id: None,
                operation_id: relaunched.action.operation_id,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        view.held, None,
        "a continuation never carries a host-load hold"
    );
    p.manager.reconcile_manager_actions_once().await.unwrap();
    let receipt = p.receipt(relaunched.action.operation_id).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Succeeded);
    let row = p
        .manager
        .store
        .lock()
        .await
        .get_session(successor)
        .unwrap()
        .unwrap();
    // Continuation defaults to no grant; the predecessor's grant is not inherited.
    assert!(
        p.manager
            .store
            .lock()
            .await
            .live_qa_lane_binding(successor)
            .unwrap()
            .is_none()
    );
    let delegated: i64 = p.manager.store.lock().await.conn.query_row(
        "SELECT coalesce(json_extract(payload_json,'$.issue_binding.qa_lane'),0) FROM harness_manager_v2_operations WHERE id=?1",
        [relaunched.action.operation_id.to_string()], |row| row.get(0)).unwrap();
    assert_eq!(delegated, 0);
    assert_eq!(row.continued_from, Some(worker));
    // Display identity is inherited across the lineage.
    assert_eq!(row.agent_role, predecessor.agent_role);
    assert_eq!(row.epic_spawn_ordinal, predecessor.epic_spawn_ordinal);
    assert_eq!(row.parent_id, Some(p.epic));
    for expected in [
        worker.to_string().as_str(),
        head.as_str(),
        "> Final words: half done.",
        "> left: wire the TUI row",
        "NOT copied",
        "Implement the Issue and report.",
        &format!("Issue #{number}"),
    ] {
        assert!(row.query.contains(expected), "{expected}: {}", row.query);
    }
    let new_root = row.sandbox_root.unwrap();
    assert_eq!(git(&new_root, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        std::fs::read_to_string(new_root.join("baton-work")).unwrap(),
        "half done\n"
    );
    // #1284: the successor holds the binding now. Even if the predecessor
    // runs again, its superseded binding no longer lets it append; the
    // successor's append lands.
    {
        let store = p.manager.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Running' WHERE id=?1",
                [worker.to_string()],
            )
            .unwrap();
        let current = store
            .agent_get_issue(
                successor,
                &AgentGetIssueRequestV1 {
                    project_id: None,
                    issue_id: Some(launched.issue_id),
                    display_number: None,
                },
            )
            .unwrap()
            .issue;
        let error = store
            .agent_update_issue(
                worker,
                &append_request(
                    &current,
                    current.row_version,
                    format!("{}\nstale", current.body),
                    "baton-stale",
                ),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("bound_issue_binding_not_live"),
            "{error}"
        );
        let appended = store
            .agent_update_issue(
                successor,
                &append_request(
                    &current,
                    current.row_version,
                    format!("{}\n\n## Handoff 2\nfresh", current.body),
                    "baton-fresh",
                ),
            )
            .unwrap();
        assert!(appended.issue.body.ends_with("## Handoff 2\nfresh"));
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Completed' WHERE id=?1",
                [worker.to_string()],
            )
            .unwrap();
    }
    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, successor)
        .await
        .unwrap();
    crate::session::launch::drop_controller_candidate_test_stream(successor);
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
}
