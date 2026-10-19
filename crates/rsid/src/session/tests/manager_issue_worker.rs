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
use rsi_common::types::{Issue, IssueStatus, NewIssue, Project, WakeMode};

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
async fn new_issue_with_body(p: &Pilot, title: &str, body: String) -> Issue {
    p.manager
        .store
        .lock()
        .await
        .create_issue(&NewIssue {
            project_id: p.project,
            title: title.into(),
            body,
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
        launch: p.default_launch(),
        parent_epic_id: None,
        idempotency_key: key.into(),
        sandbox_source: None,
        continue_from: None,
        qa_lane: false,
        review_of: None,
        related: Vec::new(),
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

/// #2725: until the launch creates the worker row, the launcher's own
/// `on_terminal` on that id is covered by the launch (pending), and only for
/// the launcher; once the row exists the ordinary authorization applies.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_launch_covers_the_launchers_on_terminal_until_the_worker_exists() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let issue = new_issue(&p, "bound work").await;
    let control = p.manager.agent_control();
    let launched = control
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "pending-1", &p))
        .await
        .unwrap();
    let worker = launched.worker_session_id;
    assert_eq!(launched.watch, ManagerIssueWorkerWatchV1::PendingLaunch);

    assert!(
        control
            .issue_worker_launch_watch_pending(p.owner, worker)
            .await
    );
    // Another session, and an unrelated id, are not covered.
    assert!(
        !control
            .issue_worker_launch_watch_pending(p.epic, worker)
            .await
    );
    assert!(
        !control
            .issue_worker_launch_watch_pending(p.owner, Uuid::new_v4())
            .await
    );

    let _process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();
    assert!(
        !control
            .issue_worker_launch_watch_pending(p.owner, worker)
            .await
    );
    assert_eq!(watches_on(&p, worker).await, vec![p.owner]);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_call_yields_worker_note_in_progress_and_watch_and_replays() {
    let p = pilot().await;
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
    assert!(first.rolling_issue_commits.is_empty());
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn launch_receipt_and_brief_include_recent_rolling_commits_with_issue_boundary() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let mut issue = None;
    for n in 1..=12 {
        issue = Some(new_issue(&p, &format!("bound work {n}")).await);
    }
    let issue = issue.unwrap();

    // The Issue token must end at a non-digit: #123 is not #12.
    std::fs::write(p.repo.join("not-the-issue"), "123\n").unwrap();
    git(&p.repo, &["add", "not-the-issue"]);
    git(&p.repo, &["commit", "-qm", "fix #123 unrelated"]);
    std::fs::write(p.repo.join("the-issue"), "12\n").unwrap();
    git(&p.repo, &["add", "the-issue"]);
    git(&p.repo, &["commit", "-qm", "fix #12 already published"]);
    git(
        &p.repo,
        &["update-ref", "refs/remotes/origin/rolling", "HEAD"],
    );

    let result = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(
            p.owner,
            request(issue.display_number, "rolling-commits", &p),
        )
        .await
        .unwrap();
    assert_eq!(result.rolling_issue_commits.len(), 1);
    assert_eq!(
        result.rolling_issue_commits[0].subject,
        "fix #12 already published"
    );
    assert_eq!(
        result.rolling_issue_commits[0].sha,
        git(&p.repo, &["rev-parse", "HEAD"])
    );

    let process =
        crate::session::launch::install_controller_candidate_test_process(result.worker_session_id);
    p.execute().await.unwrap();
    let worker = p
        .manager
        .store
        .lock()
        .await
        .get_session(result.worker_session_id)
        .unwrap()
        .unwrap();
    assert!(
        worker.query.contains(&format!(
            "Commits already on rolling naming #{}:\n- {} fix #12 already published",
            issue.display_number, result.rolling_issue_commits[0].sha
        )),
        "{}",
        worker.query
    );
    drop(process);
}

/// Commit `file` with the given subject and committer date (Unix seconds).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn commit_dated(repo: &std::path::Path, file: &str, subject: &str, unix: i64) {
    std::fs::write(repo.join(file), subject).unwrap();
    git(repo, &["add", file]);
    let date = format!("{unix} +0000");
    let output = std::process::Command::new("git")
        .args(["commit", "-qm", subject])
        .env("GIT_COMMITTER_DATE", &date)
        .env("GIT_AUTHOR_DATE", &date)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

/// #2529: a project-local number is reused by older Issues, so a launch brief
/// carries only commits that can belong to the bound Issue.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn launch_brief_commit_hint_skips_commits_older_than_the_bound_issue() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let mut issue = None;
    for n in 1..=12 {
        issue = Some(new_issue(&p, &format!("bound work {n}")).await);
    }
    let issue = issue.unwrap();
    assert_eq!(issue.display_number, 12);

    // An unrelated historical Issue #12 left its commit long before this one.
    commit_dated(
        &p.repo,
        "old",
        "docs(#12): historical provider docs",
        1_577_836_800,
    );
    let now = chrono::Utc::now().timestamp() + 5;
    commit_dated(&p.repo, "new", "fix(#12): current work", now);
    git(
        &p.repo,
        &["update-ref", "refs/remotes/origin/rolling", "HEAD"],
    );

    let result = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "collision", &p))
        .await
        .unwrap();
    let subjects: Vec<_> = result
        .rolling_issue_commits
        .iter()
        .map(|commit| commit.subject.as_str())
        .collect();
    assert_eq!(subjects, ["fix(#12): current work"]);
}

/// #2529: a satellite mirror maps its hub number through the recorded origin:
/// the hub Issue's commits attach, the local number's older collision does not.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn launch_brief_for_a_satellite_mirror_follows_its_hub_origin() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let mut issue = None;
    for n in 1..=12 {
        let title = if n == 12 {
            "Reuse unchanged intent (#12 / hub #2494)".to_owned()
        } else {
            format!("bound work {n}")
        };
        issue = Some(new_issue(&p, &title).await);
    }
    let issue = issue.unwrap();
    assert_eq!(issue.display_number, 12);

    commit_dated(
        &p.repo,
        "a",
        "docs(#12): historical provider docs",
        1_577_836_800,
    );
    commit_dated(
        &p.repo,
        "b",
        "feat(#2494): hub work on the origin",
        1_577_836_900,
    );
    commit_dated(
        &p.repo,
        "c",
        "feat(#2495): another hub Issue",
        1_577_837_000,
    );
    git(
        &p.repo,
        &["update-ref", "refs/remotes/origin/rolling", "HEAD"],
    );

    let result = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "mirror", &p))
        .await
        .unwrap();
    let subjects: Vec<_> = result
        .rolling_issue_commits
        .iter()
        .map(|commit| commit.subject.as_str())
        .collect();
    assert_eq!(subjects, ["feat(#2494): hub work on the origin"]);

    let process =
        crate::session::launch::install_controller_candidate_test_process(result.worker_session_id);
    p.execute().await.unwrap();
    let worker = p
        .manager
        .store
        .lock()
        .await
        .get_session(result.worker_session_id)
        .unwrap()
        .unwrap();
    assert!(
        worker
            .query
            .contains("Commits already on rolling naming #2494 / #12:"),
        "{}",
        worker.query
    );
    assert!(!worker.query.contains("historical provider docs"));
    assert!(!worker.query.contains("#2495"));
    drop(process);
}

/// A launch note is useful context, but a full Issue body must not prevent
/// the worker binding and action journal from being created.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn near_limit_issue_body_does_not_prevent_worker_launch() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let body = "x".repeat(65_500);
    let issue = new_issue_with_body(&p, "large work", body.clone()).await;

    let launched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "large-1", &p))
        .await
        .unwrap();

    assert_eq!(launched.issue_id, issue.id);
    assert_eq!(launched.issue_status, IssueStatus::InProgress);
    let (status, _, stored_body) = issue_row(&p, issue.id).await;
    assert_eq!(status, "InProgress");
    assert_eq!(stored_body, body);
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

/// #315: add an Epic under the Pilot's Group. `in_scope` appends it to the
/// manager's stored scope; `created_at` orders the "newest first" listing.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn add_epic(
    p: &Pilot,
    title: &str,
    created_at: &str,
    status: &str,
    in_scope: bool,
    project: Option<Uuid>,
) -> Uuid {
    let id = Uuid::new_v4();
    let store = p.manager.store.lock().await;
    let mut row = bare_session(id);
    row.project_id = project.or(Some(p.project));
    row.working_dir = p.repo.clone();
    row.session_kind = SessionKind::Epic;
    row.parent_id = Some(p.group);
    row.title = Some(title.into());
    store.insert_session(&row).unwrap();
    store
        .conn
        .execute(
            "UPDATE sessions SET created_at=?2, status=?3 WHERE id=?1",
            rusqlite::params![id.to_string(), created_at, status],
        )
        .unwrap();
    if in_scope {
        let stored: String = store
            .conn
            .query_row(
                "SELECT epic_ids_json FROM harness_manager_scopes WHERE project_id=?1",
                [p.project.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        let mut ids: Vec<String> = serde_json::from_str(&stored).unwrap();
        ids.push(id.to_string());
        store
            .conn
            .execute(
                "UPDATE harness_manager_scopes SET epic_ids_json=?2 WHERE project_id=?1",
                rusqlite::params![p.project.to_string(), serde_json::to_string(&ids).unwrap()],
            )
            .unwrap();
    }
    id
}

/// #315: with several Epics in scope the refusal keeps its code, names the
/// field to set and lists the live scope Epics (id, kind, title) newest first,
/// capped at 8; an out-of-scope or foreign-project Epic is never listed; and
/// naming one of them launches.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn parent_required_refusal_lists_the_live_scope_epics_newest_first_capped() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let issue = new_issue(&p, "needs a parent").await;
    let control = p.manager.agent_control();
    let mut scope_epics = Vec::new();
    for n in 0..9 {
        let at = format!("2030-01-{:02}T00:00:00.000000000Z", n + 1);
        scope_epics
            .push(add_epic(&p, &format!("scope epic {n}"), &at, "Running", true, None).await);
    }
    add_epic(
        &p,
        "unscoped sibling",
        "2030-02-01T00:00:00.000000000Z",
        "Running",
        false,
        None,
    )
    .await;
    let foreign_project = Uuid::new_v4();
    p.manager
        .store
        .lock()
        .await
        .insert_project(&Project {
            id: foreign_project,
            name: "Foreign".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .unwrap();
    add_epic(
        &p,
        "foreign epic",
        "2030-03-01T00:00:00.000000000Z",
        "Running",
        false,
        Some(foreign_project),
    )
    .await;
    add_epic(
        &p,
        "archived epic",
        "2030-04-01T00:00:00.000000000Z",
        "Archived",
        true,
        None,
    )
    .await;
    let baseline = counts(&p).await;

    let error = control
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "k-parent", &p))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("manager_issue_worker_parent_required: set parent_epic_id"),
        "{error}"
    );
    // 9 added + the Pilot's own Epic are live in scope; 8 are shown.
    assert!(
        error.contains("(10 live, newest first, first 8 shown)"),
        "{error}"
    );
    // Exactly the eight newest scope Epics, newest first, each with its kind
    // and title; the oldest scope Epic and the Pilot's own Epic fall past the
    // cap. Unscoped, foreign-project and archived Epics are never candidates.
    let expected = scope_epics
        .iter()
        .enumerate()
        .rev()
        .take(8)
        .map(|(n, id)| format!("{id} Epic \"scope epic {n}\""))
        .collect::<Vec<_>>()
        .join("; ");
    assert!(
        error.ends_with(&format!("first 8 shown): {expected}")),
        "{error}"
    );
    assert_eq!(counts(&p).await, baseline);

    let mut named = request(issue.display_number, "k-parent-named", &p);
    named.parent_epic_id = scope_epics.last().copied();
    control
        .agent_manager_launch_issue_worker(p.owner, named)
        .await
        .unwrap();
}

/// #315: several Epics in scope but exactly one live one is an unambiguous
/// parent, so the launch needs no `parent_epic_id`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn one_live_scope_epic_among_archived_ones_is_the_default_parent() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let issue = new_issue(&p, "single live epic").await;
    add_epic(
        &p,
        "archived epic",
        "2030-04-01T00:00:00.000000000Z",
        "Archived",
        true,
        None,
    )
    .await;
    let launched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(
            p.owner,
            request(issue.display_number, "k-default-parent", &p),
        )
        .await
        .unwrap();
    let payload: String = p
        .manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT payload_json FROM harness_manager_v2_operations WHERE id=?1",
            [launched.action.operation_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(payload.contains(&p.epic.to_string()), "{payload}");
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
        display_number: Some(issue.display_number),
        body: Some(body),
        ..AgentUpdateIssueRequestV1::new(expected_row_version, key.into())
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
    // #1912: the brief leads with the `handoff` field, not a body copy.
    {
        let store = p.manager.store.lock().await;
        let row = store.get_session(worker).unwrap().unwrap();
        assert!(
            row.query.contains("publish your complete handoff")
                && row.query.contains("\"handoff\": \"<your handoff>\"")
                && !row.query.contains("<the current body, unchanged"),
            "{}",
            row.query
        );
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
                    note: None,
                    include_issue: false,
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

/// #1750: a bound worker publishes a complete handoff while the Issue body is
/// at its ceiling, as an append-only record; the body, other fields and other
/// Issues stay untouched, and `AgentGetIssue` shows the newest records in full.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bound_worker_publishes_a_handoff_when_the_body_is_at_its_ceiling() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let issue = new_issue_with_body(&p, "full body", "x".repeat(65_500)).await;
    let launched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "ceiling-1", &p))
        .await
        .unwrap();
    let worker = launched.worker_session_id;
    let process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();

    let store = p.manager.store.lock().await;
    let (status, version, body) = row_in(&store, issue.id);
    assert_eq!(body.len(), 65_500);
    let handoff = |text: String, key: &str| {
        let mut request = append_request(&issue, version, String::new(), key);
        request.body = None;
        request.handoff = Some(text);
        request
    };
    let code = |error: crate::error::DaemonError| error.to_string();

    // A body append past the ceiling cannot be sent at all.
    let over = store
        .agent_update_issue(
            worker,
            &append_request(
                &issue,
                version,
                format!("{body}{}", "y".repeat(2_000)),
                "c-over",
            ),
        )
        .unwrap_err();
    assert!(code(over).contains("invalid_request"));
    // A handoff may not ride along with another field.
    let mut titled = handoff("done".into(), "c-title");
    titled.title = Some("Renamed".into());
    assert!(
        code(
            store
                .agent_append_issue_handoff(worker, &titled)
                .unwrap_err()
        )
        .contains("invalid_request")
    );
    // Only the bound worker publishes one; the manager does not.
    assert!(
        code(
            store
                .agent_append_issue_handoff(p.owner, &handoff("done".into(), "c-mgr"))
                .unwrap_err()
        )
        .contains("authority_denied")
    );

    // The complete handoff lands and leaves the Issue row untouched.
    let text = format!("## Handoff\n{}\nFriction: none", "h".repeat(20_000));
    let receipt = store
        .agent_append_issue_handoff(worker, &handoff(text.clone(), "c-1"))
        .unwrap();
    assert_eq!(receipt.handoff_sequence, 1);
    assert!(!receipt.deduplicated);
    assert_eq!(receipt.handoff_bytes, text.len() as i64);
    assert_eq!(
        row_in(&store, issue.id),
        (status.clone(), version, body.clone())
    );

    // A replay applies nothing twice; the same key with other text conflicts.
    let replay = store
        .agent_append_issue_handoff(worker, &handoff(text.clone(), "c-1"))
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.handoff_sequence, 1);
    assert!(
        code(
            store
                .agent_append_issue_handoff(worker, &handoff("other".into(), "c-1"))
                .unwrap_err()
        )
        .contains("idempotency_conflict")
    );

    for n in 2..=4 {
        let receipt = store
            .agent_append_issue_handoff(worker, &handoff(format!("handoff {n}"), &format!("c-{n}")))
            .unwrap();
        assert_eq!(receipt.handoff_sequence, n);
    }
    let read = store
        .agent_get_issue(
            worker,
            &AgentGetIssueRequestV1 {
                project_id: None,
                issue_id: Some(issue.id),
                display_number: None,
            },
        )
        .unwrap();
    assert_eq!(read.handoffs_total, 4);
    let sequences: Vec<i64> = read.handoffs.iter().map(|h| h.sequence).collect();
    assert_eq!(sequences, vec![2, 3, 4]);
    assert_eq!(read.handoffs[2].body, "handoff 4");
    assert_eq!(read.handoffs[2].author_session_id, worker);
    assert_eq!(read.body_bytes_remaining, 36);
    assert_eq!(read.issue.body, body);
    // The oldest record is still stored in full and unchanged.
    let first: String = store
        .conn
        .query_row(
            "SELECT body FROM issue_handoffs WHERE issue_id=?1 AND sequence=1",
            [issue.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(first, text);
    // Records are append-only.
    assert!(
        store
            .conn
            .execute("DELETE FROM issue_handoffs", [])
            .is_err()
    );
    drop(store);

    // The AgentUpdateIssue transport shape returns the handoff receipt.
    let value = p
        .manager
        .agent_update_issue_value(worker, handoff("via value".into(), "c-5"))
        .await
        .unwrap();
    assert_eq!(value["handoff_sequence"], 5);

    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, worker)
        .await
        .unwrap();
    crate::session::launch::drop_controller_candidate_test_stream(worker);
    drop(process);
}

/// #1750 fix-forward: the public JSON entry points accept `handoff`. A bound
/// worker whose Issue body is exactly at the 65536-byte ceiling publishes its
/// handoff through the RPC/CLI decoder and the native Harness tool, not only
/// through an already typed request.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handoff_json_reaches_dispatch_through_rpc_decoder_and_harness_tool() {
    use crate::session::harness::tools::HarnessTool;
    use crate::session::harness::tools::rsi_control::{IssueControlToolKind, RsiControlIssueTool};

    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let issue = new_issue_with_body(&p, "exact ceiling", "x".repeat(65_536)).await;
    let launched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(
            p.owner,
            request(issue.display_number, "ceiling-json", &p),
        )
        .await
        .unwrap();
    let worker = launched.worker_session_id;
    let process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();
    let (status, version, body) = issue_row(&p, issue.id).await;
    assert_eq!(body.len(), 65_536);

    let json = |key: &str, text: &str| {
        serde_json::json!({
            "display_number": issue.display_number,
            "expected_row_version": version,
            "idempotency_key": key,
            "handoff": text,
        })
    };

    // RPC/CLI transport: the decoder yields a handoff request that dispatches
    // to a record.
    let decoded = crate::agent_issue_validation::decode_update(&json("rpc-1", "rpc handoff"))
        .expect("handoff is an allowed AgentUpdateIssue field");
    assert_eq!(decoded.handoff.as_deref(), Some("rpc handoff"));
    let value = p
        .manager
        .agent_update_issue_value(worker, decoded)
        .await
        .unwrap();
    assert_eq!(value["handoff_sequence"], 1);

    // Native Harness tool: the real tool, JSON in, receipt out.
    let tool = RsiControlIssueTool::new(
        p.manager.agent_control(),
        worker,
        IssueControlToolKind::Update,
    );
    let max_text = "m".repeat(65_536);
    let result = tool
        .execute(json("tool-1", &max_text), std::path::Path::new("/"))
        .await;
    assert!(result.success, "{}", result.output);
    let receipt: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(receipt["handoff_sequence"], 2);
    assert_eq!(receipt["handoff_bytes"], 65_536);

    // A handoff beside another field is still refused by the shared validator.
    let mut combined = json("tool-2", "x");
    combined["title"] = serde_json::json!("Renamed");
    let refused = tool.execute(combined, std::path::Path::new("/")).await;
    assert!(!refused.success);

    // The Issue row is untouched by every record.
    assert_eq!(issue_row(&p, issue.id).await, (status, version, body));

    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, worker)
        .await
        .unwrap();
    crate::session::launch::drop_controller_candidate_test_stream(worker);
    drop(process);
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

/// Record the real seed command's view of the allocated sandbox. No pool
/// builds or process-global environment changes are needed in this fixture.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn record_issue_worker_seed(p: &Pilot, worker: Uuid) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = p.repo.parent().unwrap().join(format!("seed-{worker}"));
    std::fs::write(
        &script,
        "#!/bin/sh\nset -eu\n\
         test \"$#\" = 6\n\
         test \"$1\" = seed\n\
         test \"$2\" = --sandbox\n\
         test \"$4\" = --target\n\
         test \"$6\" = --build-missing\n\
         git -C \"$3\" rev-parse HEAD > \"$5/seed-head\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let root = p
        .manager
        .sandbox_allocator
        .base_dir()
        .join(worker.to_string());
    crate::sandbox::execution_scratch::install_target_pool_script_for_test(root.clone(), script);
    root.join("target/seed-head")
}

/// #1809: the default worker must allocate and seed from published rolling
/// even when the operator's shared checkout is on main or detached.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn issue_worker_off_rolling_allocates_and_seeds_the_fetched_tip() {
    for (key, args) in [
        ("detached-1", &["checkout", "-q", "--detach"][..]),
        ("main-1", &["checkout", "-q", "-b", "main"][..]),
    ] {
        let p = pilot().await;
        std::fs::write(p.repo.join("Cargo.toml"), "[workspace]\n").unwrap();
        git(&p.repo, &["add", "Cargo.toml"]);
        git(&p.repo, &["commit", "-qm", "worker build fixture"]);
        grant_issue_coordinate(&p).await;
        let origin = rolling_checkout_with_origin(&p);
        let pinned = git(&p.repo, &["rev-parse", "HEAD"]);
        git(&p.repo, args);
        let branch = git(&p.repo, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let issue = new_issue(&p, key).await;
        let launched = p
            .manager
            .agent_control()
            .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, key, &p))
            .await
            .unwrap();
        let published = advance_origin_rolling(&p, &origin);
        assert_ne!(published, pinned);
        let seed_head = record_issue_worker_seed(&p, launched.worker_session_id);
        assert_eq!(
            establish_issue_worker(&p, &launched).await,
            published,
            "{key}"
        );
        assert_eq!(
            std::fs::read_to_string(seed_head).unwrap().trim(),
            published
        );
        assert_eq!(git(&p.repo, &["rev-parse", "HEAD"]), pinned);
        assert_eq!(git(&p.repo, &["rev-parse", "--abbrev-ref", "HEAD"]), branch);
        assert_eq!(git(&p.repo, &["rev-parse", "origin/rolling"]), pinned);

        // An explicit commit remains pinned even though rolling has advanced.
        let issue = new_issue(&p, "explicit seed source").await;
        let mut launch = request(issue.display_number, "explicit-seed", &p);
        launch.sandbox_source = Some(ManagerSandboxSourceV1::Commit(pinned.clone()));
        let explicit = p
            .manager
            .agent_control()
            .agent_manager_launch_issue_worker(p.owner, launch)
            .await
            .unwrap();
        let seed_head = record_issue_worker_seed(&p, explicit.worker_session_id);
        assert_eq!(establish_issue_worker(&p, &explicit).await, pinned);
        assert_eq!(std::fs::read_to_string(seed_head).unwrap().trim(), pinned);
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
/// #2427: a live topology execution holds its input Issue, so a worker launch is
/// refused naming the execution, and the Issue launches once it settles.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn issue_worker_launch_is_refused_while_a_topology_execution_holds_the_issue() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let issue = new_issue(&p, "held by an execution").await;
    let execution = Uuid::new_v4();
    {
        let store = p.manager.store.lock().await;
        let input = serde_json::json!({"issue": {
            "display_number": issue.display_number,
            "project_id": issue.project_id,
        }});
        store
            .conn
            .execute(
                "INSERT INTO topology_executions (id,topology_name_snapshot,workflow_id,\
                 definition_json,definition_digest,requested_by_kind,repo_root,base_commit,\
                 custody_plan_json,status,input_json,created_at,updated_at) \
                 VALUES (?1,'issue-pipeline',?2,'{}','d','manager','/tmp','0','{}','running',?3,\
                 '2026-01-01T00:00:00.000000000Z','2026-01-01T00:00:00.000000000Z')",
                rusqlite::params![
                    execution.to_string(),
                    Uuid::new_v4().to_string(),
                    input.to_string()
                ],
            )
            .unwrap();
    }
    let refused = launch_refusal(&p, request(issue.display_number, "held-1", &p)).await;
    assert!(
        refused.contains("manager_issue_worker_already_live"),
        "{refused}"
    );
    assert!(refused.contains(&execution.to_string()), "{refused}");

    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE topology_executions SET status='failed' WHERE id=?1",
            [execution.to_string()],
        )
        .unwrap();
    let launched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "held-2", &p))
        .await
        .unwrap();
    assert!(launched.worker_session_id != Uuid::nil());
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

/// #1860: under one idempotency key, a replay with any semantic change (brief,
/// launch choice, sandbox source, Issue, `review_of`) is a conflict, not a
/// silent replay of the original worker.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn assert_changed_replays_conflict(
    p: &Pilot,
    original: &AgentManagerLaunchIssueWorkerRequestV1,
    other_issue: i64,
) {
    let mut changed = Vec::new();
    let mut brief = original.clone();
    brief.brief.push_str(" Also do something else.");
    changed.push(("brief", brief));
    let mut effort = original.clone();
    effort.launch.effort = Some("a-different-effort".into());
    changed.push(("effort", effort));
    let mut model = original.clone();
    model.launch.model = "a-different-model".into();
    changed.push(("model", model));
    let mut source = original.clone();
    source.sandbox_source = Some(ManagerSandboxSourceV1::Rolling {});
    changed.push(("sandbox_source", source));
    let mut issue = original.clone();
    issue.issue = other_issue;
    changed.push(("issue", issue));
    if original.review_of.is_some() {
        let mut review_of = original.clone();
        review_of.review_of = Some(other_issue);
        changed.push(("review_of", review_of));
    }
    for (field, request) in changed {
        let error = match p
            .manager
            .agent_control()
            .agent_manager_launch_issue_worker(p.owner, request)
            .await
        {
            Ok(replayed) => panic!("{field}: changed replay was accepted: {replayed:?}"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("idempotency_conflict"),
            "{field}: expected idempotency_conflict, got {error}"
        );
    }
}

/// #1590: `review_of` copies the implementer Issue's text and COMPLETE latest
/// handoff (landing filters included, even when oversized) into the bound
/// reviewer's brief; an unknown reviewed Issue is refused before any effect.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn review_of_copies_the_complete_implementer_handoff_into_the_reviewer_brief() {
    use rsi_common::manager_issue_worker::MANAGER_ISSUE_WORKER_REVIEWED_UNAVAILABLE;
    let p = pilot().await;
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
    let other = new_issue(&p, "another issue").await;
    assert_changed_replays_conflict(
        &p,
        &reviewing("review-1", implementer.display_number),
        other.display_number,
    )
    .await;
    drop(process);
}

/// #1750 fix-forward: a stored handoff record and a handoff in the body are
/// ordered by when each was written, so a later, smaller body handoff is not
/// hidden by an older record and a later record is not hidden by an older body
/// handoff, in both a reviewer's brief and a continuation's.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn review_brief_takes_the_newest_handoff_across_record_and_body() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let implementer = new_issue_with_body(&p, "implementer work", "x".repeat(60_000)).await;
    let reviews = [
        new_issue(&p, "review one").await,
        new_issue(&p, "review two").await,
    ];
    let record = |text: String, sequence: i64| {
        let p = &p;
        let implementer = &implementer;
        async move {
            p.manager
                .store
                .lock()
                .await
                .conn
                .execute(
                    "INSERT INTO issue_handoffs
                         (issue_id, sequence, project_id, author_session_id, idempotency_key, body, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        implementer.id.to_string(),
                        sequence,
                        p.project.to_string(),
                        Uuid::new_v4().to_string(),
                        format!("record-{sequence}"),
                        text,
                        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                    ],
                )
                .unwrap();
        }
    };
    let brief_of = |review: &Issue, key: &str| {
        let p = &p;
        let reviewed = implementer.display_number;
        let display = review.display_number;
        let key = key.to_string();
        async move {
            let mut launch = request(display, &key, p);
            launch.review_of = Some(reviewed);
            let launched = p
                .manager
                .agent_control()
                .agent_manager_launch_issue_worker(p.owner, launch)
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
            // End the reviewer so the next launch has capacity.
            crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, worker)
                .await
                .unwrap();
            crate::session::launch::drop_controller_candidate_test_stream(worker);
            drop(process);
            query
        }
    };

    // The 10000-byte handoff A cannot fit the full body, so it is a record.
    let big = format!("## Handoff\nA_RECORD_MARKER {}", "a".repeat(10_000));
    record(big, 1).await;
    let query = brief_of(&reviews[0], "review-record-only").await;
    assert!(query.contains("A_RECORD_MARKER"), "{query}");

    // A smaller handoff B written to the body afterwards is the latest.
    let (_, version, body) = issue_row(&p, implementer.id).await;
    p.manager
        .agent_control()
        .agent_update_issue(
            p.owner,
            append_request(
                &implementer,
                version,
                format!(
                    "{body}\n\n## Handoff\nB_BODY_MARKER scoped_test job j2 exit 0, landing filters"
                ),
                "append-b",
            ),
        )
        .await
        .unwrap();
    let query = brief_of(&reviews[1], "review-record-then-body").await;
    assert!(query.contains("B_BODY_MARKER"), "{query}");
    assert!(!query.contains("A_RECORD_MARKER"), "{query}");

    // A record C published after B is the latest again (body-then-record).
    record(
        "## Handoff\nC_RECORD_MARKER scoped_test job j3 exit 0, landing filters".into(),
        2,
    )
    .await;
    let after = new_issue(&p, "review three").await;
    let query = brief_of(&after, "review-body-then-record").await;
    assert!(query.contains("C_RECORD_MARKER"), "{query}");
    assert!(!query.contains("B_BODY_MARKER"), "{query}");

    // The whole body replaced by a short handoff D after record C: D is newer.
    let (_, version, _) = issue_row(&p, implementer.id).await;
    p.manager
        .agent_control()
        .agent_update_issue(
            p.owner,
            append_request(
                &implementer,
                version,
                "## Handoff\nD_SHORT_MARKER scoped_test job j4 exit 0, landing filters".into(),
                "replace-d",
            ),
        )
        .await
        .unwrap();
    let fourth = new_issue(&p, "review four").await;
    let query = brief_of(&fourth, "review-replaced-short").await;
    assert!(query.contains("D_SHORT_MARKER"), "{query}");
    assert!(!query.contains("C_RECORD_MARKER"), "{query}");

    // Record E after D wins; whitespace appended to the body afterwards does
    // not make D newer again.
    record(
        "## Handoff\nE_RECORD_MARKER scoped_test job j5 exit 0, landing filters".into(),
        3,
    )
    .await;
    let (_, version, body) = issue_row(&p, implementer.id).await;
    p.manager
        .agent_control()
        .agent_update_issue(
            p.owner,
            append_request(&implementer, version, format!("{body}\n \n\n"), "space"),
        )
        .await
        .unwrap();
    let fifth = new_issue(&p, "review five").await;
    let query = brief_of(&fifth, "review-whitespace").await;
    assert!(query.contains("E_RECORD_MARKER"), "{query}");
    assert!(!query.contains("D_SHORT_MARKER"), "{query}");
}

/// #1845: a worker bound to Issue N may read (not update) the Issues a review
/// worker was launched on with `review_of` = N, and nothing else; a relaunch of
/// N carries the complete latest review into its brief, naming (never cutting)
/// a review too large to fit.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bound_worker_reads_its_review_issues_and_relaunch_brief_carries_them() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let implementer = new_issue(&p, "implementer work").await;
    let review = new_issue(&p, "review of implementer work").await;
    let huge = new_issue(&p, "oversized review").await;
    let other = new_issue(&p, "unrelated work").await;
    let set_body = |id: Uuid, body: String| {
        let p = &p;
        async move {
            p.manager
                .store
                .lock()
                .await
                .conn
                .execute(
                    "UPDATE issues SET body=?2 WHERE id=?1",
                    rusqlite::params![id.to_string(), body],
                )
                .unwrap();
        }
    };
    set_body(
        review.id,
        "Review it.\n\n## Review handoff\nREVIEW CHANGES\nF1 first_finding_marker\n\nF2 last_finding_marker".into(),
    )
    .await;
    set_body(
        huge.id,
        format!(
            "Review it.\n\n## Review handoff\nREVIEW CHANGES\n{}",
            "filler finding line\n".repeat(2_000)
        ),
    )
    .await;
    let control = p.manager.agent_control();
    let first = control
        .agent_manager_launch_issue_worker(
            p.owner,
            request(implementer.display_number, "impl-1", &p),
        )
        .await
        .unwrap();
    let worker = first.worker_session_id;
    let process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();
    let get = |worker: Uuid, number: i64| {
        let p = &p;
        async move {
            p.manager.store.lock().await.agent_get_issue(
                worker,
                &AgentGetIssueRequestV1 {
                    project_id: None,
                    issue_id: None,
                    display_number: Some(number),
                },
            )
        }
    };
    // No review launched yet: the review Issue is unreadable.
    let error = get(worker, review.display_number).await.unwrap_err();
    assert!(error.to_string().contains("authority_denied"), "{error}");

    let mut reviewing = request(review.display_number, "review-1", &p);
    reviewing.review_of = Some(implementer.display_number);
    let reviewer = control
        .agent_manager_launch_issue_worker(p.owner, reviewing)
        .await
        .unwrap()
        .worker_session_id;
    let reviewer_process =
        crate::session::launch::install_controller_candidate_test_process(reviewer);
    p.execute().await.unwrap();
    let mut reviewing = request(huge.display_number, "review-2", &p);
    reviewing.review_of = Some(implementer.display_number);
    let huge_reviewer = control
        .agent_manager_launch_issue_worker(p.owner, reviewing)
        .await
        .unwrap()
        .worker_session_id;
    let huge_process =
        crate::session::launch::install_controller_candidate_test_process(huge_reviewer);
    p.execute().await.unwrap();

    // The implementer's worker reads both review Issues and nothing else.
    let read = get(worker, review.display_number).await.unwrap();
    assert_eq!(read.issue.id, review.id);
    assert_eq!(
        get(worker, huge.display_number).await.unwrap().issue.id,
        huge.id
    );
    assert_eq!(
        get(worker, implementer.display_number)
            .await
            .unwrap()
            .issue
            .id,
        implementer.id
    );
    let error = get(worker, other.display_number).await.unwrap_err();
    assert!(error.to_string().contains("authority_denied"), "{error}");
    // The reviewer does not gain the reverse read, and the read is read-only.
    let error = get(reviewer, implementer.display_number).await.unwrap_err();
    assert!(error.to_string().contains("authority_denied"), "{error}");
    // #2001: a live review binding dedupes by title like an implementer.
    let page = p
        .manager
        .agent_list_issues(
            reviewer,
            rsi_common::rpc::AgentListIssuesRequestV1 {
                title_contains: Some(implementer.title.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(format!("{page:?}").contains(&implementer.title));
    let store = p.manager.store.lock().await;
    let error = store
        .agent_update_issue(
            worker,
            &rsi_common::rpc::AgentUpdateIssueRequestV1 {
                display_number: Some(review.display_number),
                body: Some(format!("{}\nedit", read.issue.body)),
                ..AgentUpdateIssueRequestV1::new(read.issue.row_version, "write-review".into())
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("bound_issue"), "{error}");
    store
        .conn
        .execute(
            "UPDATE sessions SET status='Completed' WHERE id IN (?1,?2,?3)",
            rusqlite::params![
                worker.to_string(),
                reviewer.to_string(),
                huge_reviewer.to_string()
            ],
        )
        .unwrap();
    drop(store);

    // The relaunch brief carries the complete latest review, and names the
    // oversized one instead of cutting it.
    let relaunch = control
        .agent_manager_launch_issue_worker(
            p.owner,
            request(implementer.display_number, "impl-2", &p),
        )
        .await
        .unwrap();
    let successor = relaunch.worker_session_id;
    let successor_process =
        crate::session::launch::install_controller_candidate_test_process(successor);
    p.execute().await.unwrap();
    let query = p
        .manager
        .store
        .lock()
        .await
        .get_session(successor)
        .unwrap()
        .unwrap()
        .query;
    assert!(query.contains("F1 first_finding_marker"), "{query}");
    assert!(query.contains("F2 last_finding_marker"), "{query}");
    assert!(query.contains(&format!(
        "read Issue #{} with AgentGetIssue",
        huge.display_number
    )));
    assert!(!query.contains("bytes omitted from the middle"));
    assert!(query.len() <= 32_768);
    // A replay reuses the journalled brief after a review grew.
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE issues SET body=body||'\nlater addition' WHERE id=?1",
            [review.id.to_string()],
        )
        .unwrap();
    let replay = control
        .agent_manager_launch_issue_worker(
            p.owner,
            request(implementer.display_number, "impl-2", &p),
        )
        .await
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.worker_session_id, successor);
    assert_changed_replays_conflict(
        &p,
        &request(implementer.display_number, "impl-2", &p),
        other.display_number,
    )
    .await;
    drop((process, reviewer_process, huge_process, successor_process));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn set_issue_body(p: &Pilot, id: Uuid, body: String) {
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE issues SET body=?2 WHERE id=?1",
            rusqlite::params![id.to_string(), body],
        )
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn read_issue(
    p: &Pilot,
    reader: Uuid,
    number: i64,
) -> crate::error::Result<rsi_common::types::AgentGetIssueResultV1> {
    p.manager.store.lock().await.agent_get_issue(
        reader,
        &AgentGetIssueRequestV1 {
            project_id: None,
            issue_id: None,
            display_number: Some(number),
        },
    )
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn assert_read_whole(p: &Pilot, reader: Uuid, issue: &Issue) {
    let read = read_issue(p, reader, issue.display_number).await.unwrap();
    assert_eq!(read.issue.id, issue.id);
    assert_eq!(read.issue.title, issue.title);
    assert!(
        read.issue
            .body
            .ends_with(&format!("END_{}", issue.display_number)),
        "{}",
        read.issue.body
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn assert_read_denied(p: &Pilot, reader: Uuid, number: i64) {
    let error = read_issue(p, reader, number).await.unwrap_err();
    assert!(
        error.to_string().contains("authority_denied"),
        "#{number}: {error}"
    );
}

/// #218: a bound worker reads, read-only, every same-project Issue its launch
/// attached or that a `#N` selector names in its Issue's body, its handoffs or
/// the manager's brief, with full end markers; the compiled brief carries the
/// attached texts whole (or names one too large), a launch snapshot that does
/// not follow later edits while the live read does; references do not chain;
/// writes on a related Issue stay refused; a revoked binding reads nothing.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bound_worker_reads_issues_its_body_handoffs_brief_and_launch_reference() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let bound = new_issue(&p, "bound work").await;
    let by_body = new_issue(&p, "named in the body").await;
    let by_handoff = new_issue(&p, "named in a handoff").await;
    let by_brief = new_issue(&p, "named in the brief").await;
    let sibling_a = new_issue(&p, "sibling finding a").await;
    let sibling_b = new_issue(&p, "sibling finding b").await;
    let sibling_c = new_issue(&p, "sibling finding c").await;
    let attached = new_issue(&p, "attached context").await;
    let huge = new_issue(&p, "oversized attached context").await;
    let chained = new_issue(&p, "named only by attached context").await;
    let unrelated = new_issue(&p, "named only in a publish sentence").await;
    let outsider = new_issue(&p, "nothing names this").await;
    let end = |issue: &Issue| {
        format!(
            "text of #{}\n\nEND_{}",
            issue.display_number, issue.display_number
        )
    };
    for issue in [
        &by_body,
        &by_handoff,
        &by_brief,
        &sibling_a,
        &sibling_b,
        &sibling_c,
        &chained,
        &unrelated,
        &outsider,
    ] {
        set_issue_body(&p, issue.id, end(issue)).await;
    }
    set_issue_body(
        &p,
        bound.id,
        format!(
            "Build it. The notice intent is #{}.",
            by_body.display_number
        ),
    )
    .await;
    set_issue_body(
        &p,
        attached.id,
        format!(
            "Context.\n\n## Handoff\nATTACHED_LAST_FINDING see #{}\nEND_{}",
            chained.display_number, attached.display_number
        ),
    )
    .await;
    set_issue_body(
        &p,
        huge.id,
        format!(
            "Context.\n\n## Handoff\n{}END_{}",
            "filler finding line\n".repeat(3_000),
            huge.display_number
        ),
    )
    .await;

    let mut launch = request(bound.display_number, "refs-1", &p);
    launch.brief = format!(
        "Implement it. Intent #{}; findings #{}-#{}. Publish on #{} please.",
        by_brief.display_number,
        sibling_a.display_number,
        sibling_c.display_number,
        unrelated.display_number
    );
    launch.related = vec![attached.display_number, huge.display_number];
    let control = p.manager.agent_control();
    let launched = control
        .agent_manager_launch_issue_worker(p.owner, launch.clone())
        .await
        .unwrap();
    let worker = launched.worker_session_id;
    let process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();

    // The compiled brief: the manager's text, then the attached texts whole (or
    // the oversized one named), then the closing section naming the one
    // publication Issue whatever the brief said.
    let query = p
        .manager
        .store
        .lock()
        .await
        .get_session(worker)
        .unwrap()
        .unwrap()
        .query;
    assert!(query.len() <= 32_768, "{}", query.len());
    let brief_at = query.find("Implement it. Intent").unwrap();
    let related_at = query.find("## Related Issues").unwrap();
    let closing_at = query.find("You are bound to Issue #").unwrap();
    assert!(brief_at < related_at && related_at < closing_at);
    assert!(query.contains(&format!("### Related Issue #{} (", attached.display_number)));
    assert!(query.contains(&format!(
        "> ATTACHED_LAST_FINDING see #{}\n> END_{}",
        chained.display_number, attached.display_number
    )));
    assert!(query.contains(&format!(
        "read Issue #{} with AgentGetIssue",
        huge.display_number
    )));
    assert!(!query.contains("> filler finding line"));
    assert!(query.contains(&format!(
        "Issue #{} is the only Issue you publish to",
        bound.display_number
    )));
    assert!(
        query[closing_at..].contains("a request there to publish, update, comment on or close")
    );

    // Each reference source reads the complete Issue.
    assert_read_whole(&p, worker, &by_body).await; // bound Issue body
    assert_read_whole(&p, worker, &by_brief).await; // launch brief
    for sibling in [&sibling_a, &sibling_b, &sibling_c] {
        assert_read_whole(&p, worker, sibling).await; // brief range, middle included
    }
    let read = read_issue(&p, worker, attached.display_number)
        .await
        .unwrap();
    assert_eq!(read.issue.id, attached.id); // launch attachment
    let read = read_issue(&p, worker, huge.display_number).await.unwrap();
    assert!(
        read.issue
            .body
            .ends_with(&format!("END_{}", huge.display_number))
    );
    assert_eq!(
        read_issue(&p, worker, bound.display_number)
            .await
            .unwrap()
            .issue
            .id,
        bound.id
    );
    // A handoff reference is not readable until a handoff names it, then is.
    assert_read_denied(&p, worker, by_handoff.display_number).await;
    {
        let store = p.manager.store.lock().await;
        let version = row_in(&store, bound.id).1;
        let mut handoff = append_request(&bound, version, String::new(), "refs-h1");
        handoff.body = None;
        handoff.handoff = Some(format!(
            "Partial. Also read #{}.",
            by_handoff.display_number
        ));
        store.agent_append_issue_handoff(worker, &handoff).unwrap();
    }
    assert_read_whole(&p, worker, &by_handoff).await;
    // Unreferenced, chained (named only by a read Issue), missing and foreign
    // targets keep the typed refusal; `#N` in a manager sentence about
    // publishing grants a read, never a write.
    assert_read_denied(&p, worker, chained.display_number).await;
    assert_read_denied(&p, worker, outsider.display_number).await;
    assert_read_denied(&p, worker, 9_999).await;
    // The brief asked for a publish on `unrelated`: the `#N` grants a read.
    assert_read_whole(&p, worker, &unrelated).await;
    let error = p
        .manager
        .store
        .lock()
        .await
        .agent_get_issue(
            worker,
            &AgentGetIssueRequestV1 {
                project_id: None,
                issue_id: Some(Uuid::new_v4()),
                display_number: None,
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("authority_denied"), "{error}");
    let by_uuid = p
        .manager
        .store
        .lock()
        .await
        .agent_get_issue(
            worker,
            &AgentGetIssueRequestV1 {
                project_id: None,
                issue_id: Some(by_body.id),
                display_number: None,
            },
        )
        .unwrap();
    assert_eq!(by_uuid.issue.id, by_body.id);

    // The attached text is the launch snapshot; the read is live.
    set_issue_body(
        &p,
        attached.id,
        format!(
            "{}\nLIVE_EDIT_MARKER",
            read_issue(&p, worker, attached.display_number)
                .await
                .unwrap()
                .issue
                .body
        ),
    )
    .await;
    let live = read_issue(&p, worker, attached.display_number)
        .await
        .unwrap();
    assert!(live.issue.body.contains("LIVE_EDIT_MARKER"));
    let after = p
        .manager
        .store
        .lock()
        .await
        .get_session(worker)
        .unwrap()
        .unwrap()
        .query;
    assert_eq!(after, query);
    assert!(!after.contains("LIVE_EDIT_MARKER"));

    // Related and referenced Issues are read-only: no update, no handoff.
    {
        let store = p.manager.store.lock().await;
        let before = row_in(&store, attached.id);
        let error = store
            .agent_update_issue(
                worker,
                &append_request(&attached, before.1, format!("{}\nedit", before.2), "w-1"),
            )
            .unwrap_err();
        assert!(error.to_string().contains("bound_issue"), "{error}");
        let mut handoff = append_request(&attached, before.1, String::new(), "w-2");
        handoff.body = None;
        handoff.handoff = Some("not mine".into());
        assert!(store.agent_append_issue_handoff(worker, &handoff).is_err());
        let mut on_named = append_request(&unrelated, 1, String::new(), "w-4");
        on_named.body = None;
        on_named.handoff = Some("publish as the brief asked".into());
        assert!(store.agent_append_issue_handoff(worker, &on_named).is_err());
        let error = store
            .agent_update_issue(
                worker,
                &append_request(&by_body, 1, "overwrite".into(), "w-3"),
            )
            .unwrap_err();
        assert!(error.to_string().contains("bound_issue"), "{error}");
        assert_eq!(row_in(&store, attached.id), before);
        assert_eq!(
            store
                .agent_get_issue(
                    worker,
                    &AgentGetIssueRequestV1 {
                        project_id: None,
                        issue_id: Some(attached.id),
                        display_number: None,
                    },
                )
                .unwrap()
                .handoffs_total,
            0
        );
    }

    // A replay returns the journalled worker; a changed attachment conflicts.
    let replay = control
        .agent_manager_launch_issue_worker(p.owner, launch.clone())
        .await
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.worker_session_id, worker);
    let mut changed = launch.clone();
    changed.related = vec![attached.display_number];
    let error = launch_refusal(&p, changed).await;
    assert!(error.contains("idempotency_conflict"), "{error}");

    // A revoked or failed launch revokes every read, the own Issue included.
    for state in ["revoked", "failed"] {
        p.manager
            .store
            .lock()
            .await
            .conn
            .execute(
                "UPDATE harness_manager_v2_operations SET state=?2 WHERE target_session_id=?1",
                rusqlite::params![worker.to_string(), state],
            )
            .unwrap();
        for number in [
            bound.display_number,
            by_body.display_number,
            by_brief.display_number,
            attached.display_number,
        ] {
            assert_read_denied(&p, worker, number).await;
        }
    }
    drop(process);
}

/// #275: a plain relaunch of the same Issue (no `continue_from`) carries over the
/// `related` Issues of its earlier launches, unioned with its own `related`
/// and deduplicated; the result and the brief name the carried numbers, and an
/// earlier related Issue that no longer exists or is archived is skipped and
/// named instead of refusing the launch.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plain_relaunch_carries_earlier_related_issues_and_names_the_skipped() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let bound = new_issue(&p, "bound work").await;
    let first = new_issue(&p, "first related").await;
    let archived = new_issue(&p, "archived later").await;
    let added = new_issue(&p, "added by the relaunch").await;
    for issue in [&first, &archived, &added] {
        set_issue_body(
            &p,
            issue.id,
            format!(
                "text of #{}\n\nEND_{}",
                issue.display_number, issue.display_number
            ),
        )
        .await;
    }
    let control = p.manager.agent_control();
    let finish = |worker: Uuid, process: Box<dyn std::any::Any>| {
        let p = &p;
        async move {
            // Mark the worker Completed and let go of its process: the next
            // launch of the Issue is refused while a worker reads live. A
            // late monitor Running write no longer reopens it (#277).
            p.manager
                .store
                .lock()
                .await
                .conn
                .execute(
                    "UPDATE sessions SET status='Completed' WHERE id=?1",
                    rusqlite::params![worker.to_string()],
                )
                .unwrap();
            drop(process);
        }
    };

    // Launch A attaches three Issues and reads the first.
    let mut a = request(bound.display_number, "carry-a", &p);
    a.related = vec![first.display_number, archived.display_number];
    let launched_a = control
        .agent_manager_launch_issue_worker(p.owner, a)
        .await
        .unwrap();
    assert!(launched_a.carried_related.is_empty());
    let process_a = crate::session::launch::install_controller_candidate_test_process(
        launched_a.worker_session_id,
    );
    p.execute().await.unwrap();
    assert_read_whole(&p, launched_a.worker_session_id, &first).await;
    finish(launched_a.worker_session_id, Box::new(process_a)).await;

    // One related Issue is archived before the relaunch.
    {
        let store = p.manager.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE issues SET archived_at=?2 WHERE id=?1",
                rusqlite::params![
                    archived.id.to_string(),
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
    }

    // Launch B is plain: no related, no continue_from. B reads what A attached.
    let launched_b = control
        .agent_manager_launch_issue_worker(p.owner, request(bound.display_number, "carry-b", &p))
        .await
        .unwrap();
    assert_eq!(launched_b.carried_related, vec![first.display_number]);
    assert_eq!(launched_b.skipped_related, vec![archived.display_number]);
    let worker_b = launched_b.worker_session_id;
    let process_b = crate::session::launch::install_controller_candidate_test_process(worker_b);
    p.execute().await.unwrap();
    assert_read_whole(&p, worker_b, &first).await;
    assert_read_denied(&p, worker_b, added.display_number).await;
    let query = p
        .manager
        .store
        .lock()
        .await
        .get_session(worker_b)
        .unwrap()
        .unwrap()
        .query;
    assert!(
        query.contains(&format!(
            "Carried over from earlier launches of Issue #{} (#275): #{}.",
            bound.display_number, first.display_number
        )),
        "{query}"
    );
    assert!(
        query.contains(&format!(
            "Not carried over from earlier launches (no longer exists, archived or over the bound): #{}.",
            archived.display_number
        )),
        "{query}"
    );
    assert!(query.contains(&format!("> END_{}", first.display_number)));
    finish(worker_b, Box::new(process_b)).await;

    // Launch C names its own related Issue: the union keeps B's carried Issue
    // once, even when C names it again, and adds the new one.
    let mut c = request(bound.display_number, "carry-c", &p);
    c.related = vec![added.display_number, first.display_number];
    let launched_c = control
        .agent_manager_launch_issue_worker(p.owner, c.clone())
        .await
        .unwrap();
    assert!(launched_c.carried_related.is_empty(), "named by C itself");
    let worker_c = launched_c.worker_session_id;
    let process_c = crate::session::launch::install_controller_candidate_test_process(worker_c);
    p.execute().await.unwrap();
    assert_read_whole(&p, worker_c, &first).await;
    assert_read_whole(&p, worker_c, &added).await;
    // A replay returns the journalled worker and the same carry record.
    let replay = control
        .agent_manager_launch_issue_worker(p.owner, c)
        .await
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.carried_related, launched_c.carried_related);
    finish(worker_c, Box::new(process_c)).await;

    // D is plain again and inherits A's and C's related Issues, deduplicated.
    let launched_d = control
        .agent_manager_launch_issue_worker(p.owner, request(bound.display_number, "carry-d", &p))
        .await
        .unwrap();
    assert_eq!(
        launched_d.carried_related,
        vec![added.display_number, first.display_number]
    );
    let worker_d = launched_d.worker_session_id;
    let _process_d = crate::session::launch::install_controller_candidate_test_process(worker_d);
    p.execute().await.unwrap();
    assert_read_whole(&p, worker_d, &first).await;
    assert_read_whole(&p, worker_d, &added).await;
}

/// #277: a worker ended through the Store while its monitor is still starting
/// stays Completed. The monitor's start-of-turn Running write is queued and
/// used to land after the end state (about 130 ms later), so the Issue looked
/// live and a relaunch was refused `manager_issue_worker_already_live`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_ended_while_its_monitor_starts_stays_completed_and_the_issue_relaunches() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let bound = new_issue(&p, "bound work").await;
    let control = p.manager.agent_control();
    let launched = control
        .agent_manager_launch_issue_worker(p.owner, request(bound.display_number, "end-a", &p))
        .await
        .unwrap();
    let worker = launched.worker_session_id;
    let _process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();
    p.manager
        .store
        .lock()
        .await
        .update_session_status(worker, SessionStatus::Completed)
        .unwrap();
    // The monitor marks its tracked session Running just before it queues
    // the durable write; wait for that, then for the queue to drain.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let tracked = p
            .manager
            .active
            .read()
            .await
            .get(&worker)
            .map(|tracked| tracked.session.status);
        if tracked == Some(SessionStatus::Running) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "monitor never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    p.manager.persistence.barrier().await.unwrap();
    let status = p
        .manager
        .store
        .lock()
        .await
        .get_session(worker)
        .unwrap()
        .unwrap()
        .status;
    assert_eq!(status, SessionStatus::Completed);

    let relaunched = control
        .agent_manager_launch_issue_worker(p.owner, request(bound.display_number, "end-b", &p))
        .await
        .unwrap();
    assert!(!relaunched.deduplicated);
    assert_ne!(relaunched.worker_session_id, worker);
}

/// #218 (hub #2408): a revision worker told to resolve N findings, keep a
/// parent's conditions and publish on the parent reads the parent and every
/// finding (a range's middle included) with AgentGetIssue, but its brief names
/// its own Issue as the only publication target and the parent write is refused.
/// A range wider than the bound selects only its endpoints.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revision_worker_reads_parent_and_every_sibling_finding_but_publishes_only_on_its_own() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let parent = new_issue(&p, "parent plan").await;
    let mut findings = Vec::new();
    for n in 0..5 {
        findings.push(new_issue(&p, &format!("finding {n}")).await);
    }
    let wide_lo = new_issue(&p, "wide range low").await;
    let mut middle = Vec::new();
    for n in 0..38 {
        middle.push(new_issue(&p, &format!("middle {n}")).await);
    }
    let wide_hi = new_issue(&p, "wide range high").await;
    let revision = new_issue(&p, "revision work").await;
    assert_eq!(wide_hi.display_number - wide_lo.display_number, 39);
    for issue in findings
        .iter()
        .chain(middle.iter())
        .chain([&parent, &wide_lo, &wide_hi])
    {
        let body = format!(
            "text of #{}\n\nEND_{}",
            issue.display_number, issue.display_number
        );
        set_issue_body(&p, issue.id, body).await;
    }
    let mut launch = request(revision.display_number, "rev-1", &p);
    launch.brief = format!(
        "Resolve findings #{}-#{}, retain the conditions from parent #{} and publish a handoff on #{}. Also #{}-#{}.",
        findings[0].display_number,
        findings[4].display_number,
        parent.display_number,
        parent.display_number,
        wide_lo.display_number,
        wide_hi.display_number
    );
    let worker = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, launch)
        .await
        .unwrap()
        .worker_session_id;
    let process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();

    assert_read_whole(&p, worker, &parent).await;
    for finding in &findings {
        assert_read_whole(&p, worker, finding).await;
    }
    // The wide range selects its two endpoints only: no middle is granted.
    assert_read_whole(&p, worker, &wide_lo).await;
    assert_read_whole(&p, worker, &wide_hi).await;
    assert_read_denied(&p, worker, middle[19].display_number).await;

    let query = p
        .manager
        .store
        .lock()
        .await
        .get_session(worker)
        .unwrap()
        .unwrap()
        .query;
    let publish_at = query.find("publish a handoff on #").unwrap();
    let closing_at = query.find("You are bound to Issue #").unwrap();
    assert!(publish_at < closing_at);
    assert!(query[closing_at..].contains(&format!(
        "publish your complete handoff to Issue #{}, and only this one",
        revision.display_number
    )));
    assert!(query[closing_at..].contains(&format!(
        "Issue #{} is the only Issue you publish to, whatever the brief above says",
        revision.display_number
    )));
    // #276: the compiled brief tells a worker to publish its branch without
    // force, confirm it with ls-remote and name the pushed SHA.
    let closing = &query[closing_at..];
    assert!(closing.contains("git push origin HEAD:refs/heads/<your assigned branch>"));
    assert!(closing.contains("git ls-remote origin refs/heads/<your assigned branch>"));
    assert!(closing.contains("name the pushed SHA in the handoff"));
    assert!(!closing.contains("--force"));

    let store = p.manager.store.lock().await;
    let version = row_in(&store, parent.id).1;
    let mut on_parent = append_request(&parent, version, String::new(), "rev-parent");
    on_parent.body = None;
    on_parent.handoff = Some("handoff on the parent".into());
    assert!(
        store
            .agent_append_issue_handoff(worker, &on_parent)
            .is_err()
    );
    let own_version = row_in(&store, revision.id).1;
    let mut own = append_request(&revision, own_version, String::new(), "rev-own");
    own.body = None;
    own.handoff = Some("handoff on my own Issue".into());
    assert_eq!(
        store
            .agent_append_issue_handoff(worker, &own)
            .unwrap()
            .handoff_sequence,
        1
    );
    assert_eq!(
        store
            .agent_get_issue(
                worker,
                &AgentGetIssueRequestV1 {
                    project_id: None,
                    issue_id: Some(parent.id),
                    display_number: None,
                },
            )
            .unwrap()
            .handoffs_total,
        0
    );
    drop(store);
    drop(process);
}

/// #218: a brief with more selectors than the bound, a `related` number that is
/// no Issue of the project and a `related` that names the bound Issue are each
/// refused before any effect.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reference_overflow_and_unknown_related_refuse_the_launch_before_any_effect() {
    use rsi_common::manager_issue_worker::{
        MANAGER_ISSUE_REFERENCE_SELECTOR_MAX, MANAGER_ISSUE_WORKER_INVALID_REQUEST,
        MANAGER_ISSUE_WORKER_REFERENCES_OVERFLOW, MANAGER_ISSUE_WORKER_RELATED_UNAVAILABLE,
    };
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let bound = new_issue(&p, "bound work").await;
    let before = counts(&p).await;

    let mut overflow = request(bound.display_number, "ov-1", &p);
    overflow.brief = (1..=MANAGER_ISSUE_REFERENCE_SELECTOR_MAX as i64 + 1)
        .map(|n| format!("#{n} "))
        .collect();
    let error = launch_refusal(&p, overflow).await;
    assert!(
        error.contains(MANAGER_ISSUE_WORKER_REFERENCES_OVERFLOW),
        "{error}"
    );

    let mut unknown = request(bound.display_number, "ov-2", &p);
    unknown.related = vec![9_999];
    let error = launch_refusal(&p, unknown).await;
    assert!(
        error.contains(MANAGER_ISSUE_WORKER_RELATED_UNAVAILABLE),
        "{error}"
    );

    let mut itself = request(bound.display_number, "ov-3", &p);
    itself.related = vec![bound.display_number];
    let error = launch_refusal(&p, itself).await;
    assert!(
        error.contains(MANAGER_ISSUE_WORKER_INVALID_REQUEST),
        "{error}"
    );
    assert_eq!(counts(&p).await, before);

    // Exactly the bound is accepted.
    let mut exact = request(bound.display_number, "ov-4", &p);
    exact.brief = (1..=MANAGER_ISSUE_REFERENCE_SELECTOR_MAX as i64)
        .map(|n| format!("#{n} "))
        .collect();
    let launched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, exact)
        .await
        .unwrap();
    assert!(launched.worker_session_id != Uuid::nil());
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
    let p = pilot().await;
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
        // #290: the instruction is the handoff-only request on this Issue.
        assert!(
            mail[0].1.contains(&format!(
                "`AgentUpdateIssue {{display_number: {number}, handoff: \"<complete handoff>\", expected_row_version: <any positive>, idempotency_key: \"<unique>\"}}` and no other field"
            )),
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

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
const CAPACITY_STOP: &str =
    "provider_error:codex:Selected model is at capacity. Please try a different model.";

/// Mark a launched worker `Failed` with `stop_reason`, ended two minutes ago.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn fail_worker(p: &Pilot, worker: Uuid, stop_reason: &str) {
    let ended = (chrono::Utc::now() - chrono::Duration::minutes(2))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET status='Failed', stop_reason=?2, updated_at=?3 WHERE id=?1",
            rusqlite::params![worker.to_string(), stop_reason, ended],
        )
        .unwrap();
}

/// Launch and establish one Issue-bound worker, then return it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn established_issue_worker(p: &mut Pilot, key: &str) -> (Issue, Uuid, Uuid) {
    grant_issue_coordinate(p).await;
    let issue = new_issue(p, "capacity work").await;
    let launched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, key, p))
        .await
        .unwrap();
    let worker = launched.worker_session_id;
    let _process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();
    assert_eq!(watches_on(p, worker).await, vec![p.owner]);
    // Stop the fake provider process and let its monitor settle, so no late
    // write races the failure the test then stamps on the row.
    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, worker)
        .await
        .unwrap();
    crate::session::launch::drop_controller_candidate_test_stream(worker);
    for _ in 0..100 {
        if !p.manager.active.read().await.contains_key(&worker) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // The monitor's final metadata write (status, `stop_reason = NULL`) may
    // still sit in the persistence queue; flush it before the test stamps its
    // own failure, or it overwrites the stamp under load (#2109).
    p.manager.persistence.barrier().await.unwrap();
    (issue, worker, launched.action.operation_id)
}

/// The scripted model of the policy's second allowed launch.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn second_launch_model() -> String {
    crate::session::provider_model_validation::SCRIPTED_TEST_SECOND_MODEL.into()
}

/// The other family the manager's policy also allows (#2109): a capacity
/// relaunch moves there, since resuming the same model already failed.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn allow_second_launch(p: &mut Pilot) {
    p.policy.allowed_launches.push(ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: second_launch_model(),
        effort: None,
    });
}

/// Record what a real Codex capacity failure leaves in the transcript (worker
/// 5ad7e7af events 36-37): the provider failure itself as two Assistant
/// messages. `tools` adds that many ToolUse/ToolResult pairs before it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn record_capacity_transcript(p: &Pilot, worker: Uuid, tools: i32) {
    let store = p.manager.store.lock().await;
    let mut sequence = 100;
    let mut add = |event_type: &str, role: Option<&str>, content: &str| {
        sequence += 1;
        store
            .conn
            .execute(
                "INSERT INTO conversation_events(session_id,sequence,event_type,role,content,created_at)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                rusqlite::params![
                    worker.to_string(),
                    sequence,
                    event_type,
                    role,
                    content,
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
    };
    for _ in 0..tools {
        add("ToolUse", None, "");
        add("ToolResult", None, "done");
    }
    for title in ["Provider diagnostic", "Process Error"] {
        add(
            "Message",
            Some("Assistant"),
            &format!(
                "**{title} (codex_event)**\n```\nSelected model is at capacity. Please try a different model.\n```"
            ),
        );
    }
}

/// The latest launch of the Issue's binding: (worker, issue_binding JSON,
/// journalled query).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn latest_launch(p: &Pilot, after: Uuid) -> (Uuid, serde_json::Value, String) {
    let store = p.manager.store.lock().await;
    let (worker, binding, query): (String, String, String) = store
        .conn
        .query_row(
            "SELECT target_session_id, json_extract(payload_json,'$.issue_binding'),
                    json_extract(payload_json,'$.request.operation.query')
             FROM harness_manager_v2_operations
             WHERE kind='lifecycle_action' AND id<>?1 ORDER BY rowid DESC LIMIT 1",
            [after.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    (
        Uuid::parse_str(&worker).unwrap(),
        serde_json::from_str(&binding).unwrap(),
        query,
    )
}

/// #1930, #2109: a bound worker that fails on provider capacity at launch is
/// relaunched once per attempt on the same binding, on the next allowed launch:
/// the manager's watch stays quiet, the failed worker's watch is retired, the
/// Issue note and the journalled binding name the relaunch, and the relaunched
/// worker is watched by the same manager. A second scan admits nothing more.
/// The transcript is the real one: the failure text is two Assistant
/// messages, which #1930 mistook for a started turn.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capacity_failure_before_first_turn_relaunches_the_same_binding_once() {
    let mut p = pilot().await;
    allow_second_launch(&mut p);
    let (issue, worker, operation) = established_issue_worker(&mut p, "cap-1").await;
    fail_worker(&p, worker, CAPACITY_STOP).await;
    record_capacity_transcript(&p, worker, 0).await;

    // The failure is owed a relaunch: the manager's watch holds.
    let (decision, _) = p
        .manager
        .watch_decision_for(worker, true, p.owner)
        .await
        .unwrap();
    assert_eq!(decision, WatchDecision::NotReady);

    assert_eq!(p.manager.reconcile_issue_worker_capacity_retries().await, 1);
    // Exactly once: the admitted relaunch is the Issue's latest launch.
    assert_eq!(p.manager.reconcile_issue_worker_capacity_retries().await, 0);

    let (status, _, body) = issue_row(&p, issue.id).await;
    assert_eq!(status, "InProgress");
    assert!(
        body.contains("relaunched by the daemon")
            && body.contains("attempt 1/4")
            && body.contains("launch substituted")
            && body.contains("capacity: relaunched as")
            && body.contains(&second_launch_model())
            && body.contains("restarting from the original brief"),
        "{body}"
    );
    let (relaunch, binding, _) = latest_launch(&p, operation).await;
    assert_ne!(relaunch, worker);
    assert_eq!(
        binding["capacity_retry"]["predecessor_operation"],
        operation.to_string()
    );
    assert_eq!(binding["capacity_retry"]["continued"], false);
    assert!(binding["continue_from"].is_null(), "{binding}");
    // The failed worker's watch is retired (it must not report a failure the
    // daemon recovered); the relaunch is watched by the same manager.
    assert!(watches_on(&p, worker).await.is_empty());
    let _process = crate::session::launch::install_controller_candidate_test_process(relaunch);
    p.execute().await.unwrap();
    assert_eq!(watches_on(&p, relaunch).await, vec![p.owner]);
    assert_eq!(issue_row(&p, issue.id).await.0, "InProgress");
    // The manager reads what the daemon did on the relaunched worker's line.
    let store = p.manager.store.lock().await;
    let note = store
        .capacity_watch_note(&store.get_session(relaunch).unwrap().unwrap())
        .unwrap()
        .expect("the relaunch is annotated");
    assert!(
        note.contains("capacity: relaunched as") && note.contains(&second_launch_model()),
        "{note}"
    );
}

/// #2109: a capacity failure after hours of pushed work (worker 2e6901d6) is
/// recovered without a manager turn. The finalizer's heal resumes the SAME
/// session on the same model with capacity backoff while the manager's watch
/// stays quiet and no relaunch is attempted; once the bound is spent the
/// daemon relaunches the Issue on the next allowed launch with `continue_from`
/// the failed worker, so the successor branches from its committed HEAD.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capacity_failure_mid_work_resumes_in_place_then_relaunches_continuing_the_work() {
    let mut p = pilot().await;
    allow_second_launch(&mut p);
    let (issue, worker, operation) = established_issue_worker(&mut p, "cap-3").await;
    // The worker committed real work, ran tools, then the provider failed it.
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
    std::fs::write(root.join("mid-work"), "pushed\n").unwrap();
    git(&root, &["add", "mid-work"]);
    git(
        &root,
        &["commit", "-qm", "work before the capacity failure"],
    );
    fail_worker(&p, worker, CAPACITY_STOP).await;
    record_capacity_transcript(&p, worker, 3).await;

    let (store, bus) = (p.manager.store.clone(), p.manager.event_bus.clone());
    let heal = || crate::session::transient_heal::heal_failed_session(&store, &bus, worker);
    let fire = || async {
        store
            .lock()
            .await
            .conn
            .execute(
                "UPDATE scheduled_jobs SET enabled=0 WHERE wake_session_id=?1",
                [worker.to_string()],
            )
            .unwrap();
    };

    // Stage 1: the same session is resumed. The watch is quiet, no relaunch.
    heal().await;
    let state = p
        .manager
        .store
        .lock()
        .await
        .capacity_resume_state(worker)
        .unwrap()
        .expect("a same-session resume is armed");
    assert_eq!((state.attempts, state.exhausted), (1, false));
    assert!(state.pending_until.is_some());
    let (decision, _) = p
        .manager
        .watch_decision_for(worker, true, p.owner)
        .await
        .unwrap();
    assert_eq!(decision, WatchDecision::NotReady);
    assert_eq!(p.manager.reconcile_issue_worker_capacity_retries().await, 0);

    // Stage 2: the resumes keep hitting capacity until the bound is spent.
    for _ in 0..2 {
        fire().await;
        heal().await;
    }
    fire().await;
    heal().await;
    let state = p
        .manager
        .store
        .lock()
        .await
        .capacity_resume_state(worker)
        .unwrap()
        .unwrap();
    assert!(state.exhausted, "{state:?}");
    assert_eq!(state.pending_until, None);
    // The relaunch is owed now: the watch still holds, and the daemon admits it.
    let (decision, _) = p
        .manager
        .watch_decision_for(worker, true, p.owner)
        .await
        .unwrap();
    assert_eq!(decision, WatchDecision::NotReady);
    {
        let store = p.manager.store.lock().await;
        let failed = store.get_session(worker).unwrap().unwrap();
        let note = store.capacity_watch_note(&failed).unwrap().unwrap();
        assert!(
            note.contains("capacity: recovery exhausted (resumed 3/3"),
            "{note}"
        );
    }
    assert_eq!(p.manager.reconcile_issue_worker_capacity_retries().await, 1);
    assert_eq!(p.manager.reconcile_issue_worker_capacity_retries().await, 0);

    let (relaunch, binding, query) = latest_launch(&p, operation).await;
    assert_ne!(relaunch, worker);
    {
        let store = p.manager.store.lock().await;
        let failed = store.get_session(worker).unwrap().unwrap();
        let note = store.capacity_watch_note(&failed).unwrap().unwrap();
        assert!(
            note.contains(&format!(
                "capacity: relaunched as {} on Claude/{}",
                &relaunch.to_string()[..8],
                second_launch_model()
            )),
            "{note}"
        );
    }
    assert_eq!(binding["continue_from"], worker.to_string());
    assert_eq!(binding["capacity_retry"]["continued"], true);
    assert!(
        query.contains(&format!("You continue worker {worker}"))
            && query.contains("selected model at capacity")
            && query.contains("Implement the Issue and report."),
        "{query}"
    );
    let (_, _, body) = issue_row(&p, issue.id).await;
    assert!(
        body.contains("capacity: relaunched as")
            && body.contains("continuing the failed worker's committed work"),
        "{body}"
    );
    assert!(watches_on(&p, worker).await.is_empty());
    let _process = crate::session::launch::install_controller_candidate_test_process(relaunch);
    p.execute().await.unwrap();
    assert_eq!(watches_on(&p, relaunch).await, vec![p.owner]);
    // The successor's sandbox carries the failed worker's committed work.
    let successor_root = p
        .manager
        .store
        .lock()
        .await
        .get_session(relaunch)
        .unwrap()
        .unwrap()
        .sandbox_root
        .unwrap();
    assert!(successor_root.join("mid-work").is_file());
}

/// #2109: a manager that already relaunched the Issue by hand is not raced by
/// the daemon: the superseded worker's armed same-session resume is retired
/// so two sessions never run one Issue.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_manager_relaunch_retires_the_armed_capacity_resume() {
    let mut p = pilot().await;
    let (issue, worker, _operation) = established_issue_worker(&mut p, "cap-4").await;
    fail_worker(&p, worker, CAPACITY_STOP).await;
    record_capacity_transcript(&p, worker, 1).await;
    crate::session::transient_heal::heal_failed_session(
        &p.manager.store,
        &p.manager.event_bus,
        worker,
    )
    .await;
    assert!(
        p.manager
            .store
            .lock()
            .await
            .capacity_resume_state(worker)
            .unwrap()
            .unwrap()
            .pending_until
            .is_some()
    );
    p.manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "by-hand", &p))
        .await
        .unwrap();
    assert_eq!(p.manager.reconcile_issue_worker_capacity_retries().await, 0);
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .capacity_resume_state(worker)
            .unwrap()
            .unwrap()
            .pending_until,
        None
    );
}

/// Park `worker` as the daemon sees a worker that submitted its verification
/// job: its turn ended (`Completed`, no report) and it owns one `when` wake on
/// that job. Returns the wake id.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn park_worker_on_a_job_wake(p: &Pilot, worker: Uuid) -> Uuid {
    use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};
    let store = p.manager.store.lock().await;
    store
        .conn
        .execute(
            "UPDATE sessions SET status='Completed' WHERE id=?1",
            [worker.to_string()],
        )
        .unwrap();
    let now = chrono::Utc::now();
    let wake = ScheduledJob {
        id: Uuid::new_v4(),
        name: "scoped-test-wake".into(),
        message: "the job finished".into(),
        schedule: ScheduleSpec {
            recurrence: Recurrence::Once,
            anchor: now + chrono::Duration::hours(1),
        },
        last_fired_at: None,
        next_fire_at: now + chrono::Duration::hours(1),
        enabled: true,
        working_dir: None,
        provider: None,
        model: None,
        project_id: None,
        created_at: now,
        updated_at: now,
        wake_mode: WakeMode::Resume,
        wake_session_id: Some(worker),
    };
    store.insert_scheduled_job(&wake).unwrap();
    store
        .conn
        .execute(
            "UPDATE scheduled_jobs SET schedule_json=json_set(schedule_json,'$.wake_when',
                json('{\"predicate\":{\"jobs_terminal\":[]}}')) WHERE id=?1",
            [wake.id.to_string()],
        )
        .unwrap();
    wake.id
}

/// #2212: a worker whose turn ended while it waits on its own `when` wake
/// still holds the Issue. A second launch is refused `manager_issue_worker_
/// already_live` naming the worker and its wake (it used to supersede the
/// binding and refuse the first worker's handoff); the worker's final handoff
/// frees the Issue; an explicit `continue_from` of a parked worker still
/// replaces it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_launch_cannot_supersede_a_worker_parked_on_its_wake() {
    use rsi_common::types::{ConversationEvent, EventType, Role};
    let mut p = pilot().await;
    let (issue, worker, _operation) = established_issue_worker(&mut p, "park-1").await;
    let (other_issue, other_worker, _operation) = established_issue_worker(&mut p, "park-2").await;
    let wake = park_worker_on_a_job_wake(&p, worker).await;
    park_worker_on_a_job_wake(&p, other_worker).await;

    let before = counts(&p).await;
    let refused = launch_refusal(&p, request(issue.display_number, "park-3", &p)).await;
    assert!(
        refused.contains("manager_issue_worker_already_live"),
        "{refused}"
    );
    assert!(refused.contains(&worker.to_string()), "{refused}");
    assert!(refused.contains(&wake.to_string()), "{refused}");
    // The refusal changed nothing.
    assert_eq!(counts(&p).await, before);

    // An explicit continue_from replaces a parked worker.
    let mut continuing = request(other_issue.display_number, "park-4", &p);
    continuing.continue_from = Some(other_worker);
    let successor = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, continuing)
        .await
        .unwrap();
    assert_ne!(successor.worker_session_id, other_worker);

    // The parked worker's final handoff frees the Issue.
    {
        let store = p.manager.store.lock().await;
        store
            .insert_event(&ConversationEvent {
                id: 0,
                session_id: worker,
                sequence: 10_000,
                event_type: EventType::Message,
                role: Some(Role::Assistant),
                content: "PIPELINE HANDOFF — IMPLEMENTATION:\nFriction: none".into(),
                tool_name: None,
                tool_input: None,
                created_at: chrono::Utc::now(),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            })
            .unwrap();
    }
    let relaunched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "park-5", &p))
        .await
        .unwrap();
    assert_ne!(relaunched.worker_session_id, worker);
}

/// #1930: auth, invalid-model, quota and unclassified provider failures keep
/// failing fast: no relaunch, and the manager's watch fires.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn other_provider_failures_are_not_relaunched() {
    let mut p = pilot().await;
    let (_issue, worker, _operation) = established_issue_worker(&mut p, "cap-2").await;
    for reason in [
        "provider_error:codex:invalid model",
        "provider_error:provider_auth_invalid",
        "provider_error:codex_usage_limit",
        "provider_error:unclassified",
    ] {
        fail_worker(&p, worker, reason).await;
        assert_eq!(
            p.manager.reconcile_issue_worker_capacity_retries().await,
            0,
            "{reason}"
        );
        let (decision, _) = p
            .manager
            .watch_decision_for(worker, true, p.owner)
            .await
            .unwrap();
        assert!(matches!(decision, WatchDecision::Fire { .. }), "{reason}");
    }
}

/// #1878: live bound workers can dedupe within their project without bodies;
/// both public transports use the same projection and managers keep full rows.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bound_worker_lists_project_summaries_through_rpc_and_native_tool() {
    use crate::session::harness::tools::HarnessTool;
    use crate::session::harness::tools::rsi_control::{IssueControlToolKind, RsiControlIssueTool};
    use rsi_common::rpc::AgentListIssuesRequestV1;

    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let issue = new_issue(&p, "dedupe worker task").await;
    // Two matches with distinct titles and identical labels, plus a title-only
    // match and a foreign project match, exercise composed filters and paging.
    let create = |store: &Store, project_id, title: &str, labels: Vec<String>| {
        store
            .create_issue(&NewIssue {
                project_id,
                title: title.into(),
                body: "Private investigation and handoff".into(),
                priority: Some(2),
                labels,
                created_by_session_id: None,
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            })
            .unwrap()
    };
    let labels = vec!["kaizen".into(), "dedupe".into()];
    let matches = {
        let store = p.manager.store.lock().await;
        let first = create(&store, p.project, "Catalog dedupe alpha", labels.clone());
        let second = create(&store, p.project, "Catalog dedupe beta", labels.clone());
        create(&store, p.project, "Catalog without labels", Vec::new());
        create(
            &store,
            p.project,
            "Catalog partial labels",
            vec!["kaizen".into()],
        );
        create(&store, p.project, "Other title with labels", labels.clone());
        let mut foreign = store.get_project(p.project).unwrap().unwrap();
        foreign.id = Uuid::new_v4();
        foreign.name = "Foreign dedupe project".into();
        store.insert_project(&foreign).unwrap();
        create(&store, foreign.id, "Catalog foreign", labels.clone());
        (first, second, foreign.id)
    };
    let launched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(
            p.owner,
            request(issue.display_number, "dedupe-summary", &p),
        )
        .await
        .unwrap();
    let worker = launched.worker_session_id;
    let process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();

    let catalog = p
        .manager
        .agent_control()
        .agent_get_authority_catalog(worker, Default::default())
        .await
        .unwrap();
    assert!(
        catalog
            .controls
            .iter()
            .any(|control| control.method == "AgentListIssues")
    );

    let args = serde_json::json!({
        "title_contains": "CaTaLoG", "labels": labels,
        "status": "Open", "limit": 1, "order": "desc"
    });
    // RPC handler uses this decoder and serializes this same typed result.
    let rpc_params = crate::agent_issue_validation::decode_list(&args).unwrap();
    let rpc_value = serde_json::to_value(
        p.manager
            .agent_list_issues(worker, rpc_params)
            .await
            .unwrap(),
    )
    .unwrap();
    let expected_summary = |row: &Issue| {
        serde_json::json!({
            "display_number": row.display_number, "id": row.id,
            "title": row.title, "labels": row.labels, "status": row.status,
            "priority": row.priority, "created_at": row.created_at,
            "updated_at": row.updated_at
        })
    };
    // Exact row equality pins the allowlist, excluding body and all extra fields.
    assert_eq!(
        rpc_value["issues"],
        serde_json::json!([expected_summary(&matches.1)])
    );
    let tool = RsiControlIssueTool::new(
        p.manager.agent_control(),
        worker,
        IssueControlToolKind::List,
    );
    let native = tool.execute(args.clone(), std::path::Path::new("/")).await;
    assert!(native.success, "{}", native.output);
    let native_value: serde_json::Value = serde_json::from_str(&native.output).unwrap();
    assert_eq!(native_value, rpc_value);
    let mut next = args.clone();
    next["cursor"] = rpc_value["next_cursor"].clone();
    let native = tool.execute(next, std::path::Path::new("/")).await;
    assert!(native.success, "{}", native.output);
    let value: serde_json::Value = serde_json::from_str(&native.output).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "issues": [expected_summary(&matches.0)], "next_cursor": null
        })
    );
    let mut closed = args.clone();
    closed["status"] = serde_json::json!("Closed");
    let native = tool.execute(closed, std::path::Path::new("/")).await;
    assert!(native.success, "{}", native.output);
    let value: serde_json::Value = serde_json::from_str(&native.output).unwrap();
    assert_eq!(value["issues"], serde_json::json!([]));

    for project_id in [p.project, matches.2] {
        let mut forbidden = args.clone();
        forbidden["project_id"] = serde_json::json!(project_id);
        let request = crate::agent_issue_validation::decode_list(&forbidden).unwrap();
        let error = p
            .manager
            .agent_list_issues(worker, request)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("authority_denied"), "{error}");
        let native = tool.execute(forbidden, std::path::Path::new("/")).await;
        assert!(!native.success);
    }
    for title in [None, Some("   ")] {
        let error = p
            .manager
            .agent_list_issues(
                worker,
                AgentListIssuesRequestV1 {
                    title_contains: title.map(str::to_string),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("invalid_request"), "{error}");
    }
    // The manager has the identical full-record JSON and may omit title_contains.
    let manager_page = p
        .manager
        .agent_list_issues(p.owner, AgentListIssuesRequestV1::default())
        .await
        .unwrap()
        .into_full()
        .expect("manager retains full records");
    let manager_row = manager_page
        .issues
        .iter()
        .find(|row| row.id == matches.0.id)
        .unwrap();
    assert_eq!(
        serde_json::to_value(manager_row).unwrap(),
        serde_json::to_value(&matches.0).unwrap()
    );
    let manager_tool = RsiControlIssueTool::new(
        p.manager.agent_control(),
        p.owner,
        IssueControlToolKind::List,
    );
    let native = manager_tool
        .execute(serde_json::json!({}), std::path::Path::new("/"))
        .await;
    assert!(native.success, "{}", native.output);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&native.output).unwrap(),
        serde_json::to_value(manager_page).unwrap()
    );

    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, worker)
        .await
        .unwrap();
    p.manager
        .store
        .lock()
        .await
        .update_session_status(worker, SessionStatus::Completed)
        .unwrap();
    let error = p
        .manager
        .agent_list_issues(
            worker,
            crate::agent_issue_validation::decode_list(&args).unwrap(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("authority_denied"), "{error}");
    let catalog = p
        .manager
        .agent_control()
        .agent_get_authority_catalog(worker, Default::default())
        .await
        .unwrap();
    assert!(
        catalog
            .controls
            .iter()
            .all(|control| control.method != "AgentListIssues")
    );
    crate::session::launch::drop_controller_candidate_test_stream(worker);
    drop(process);
}

/// #2091: PM self-succession and context-cap rotation both publish the
/// guarded rotation receipt. Repair and fresh arm follow each committed hop.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pm_watch_rearm_follows_published_self_succession_and_context_cap() {
    let mut p = pilot().await;
    let (_, worker, _) = established_issue_worker(&mut p, "pm-watch-succession").await;
    let control = p.manager.agent_control();
    let mut predecessor = p.owner;
    for _ in 0..2 {
        let successor = Uuid::new_v4();
        {
            let store = p.manager.store.lock().await;
            // The previous watch was consumed before succession, leaving no
            // enabled row to move. Repair must consult the published seat.
            store
                .conn
                .execute(
                    "UPDATE scheduled_jobs SET enabled=0 WHERE wake_mode=?1",
                    [format!("on_terminal:{worker}")],
                )
                .unwrap();
            let mut row = store.get_session(predecessor).unwrap().unwrap();
            row.id = successor;
            row.continued_from = Some(predecessor);
            row.rotation_depth += 1;
            row.status = SessionStatus::Running;
            store.insert_session(&row).unwrap();
            store
                .update_session_status(predecessor, SessionStatus::Archived)
                .unwrap();
            assert!(
                store
                    .record_harness_manager_rotation(predecessor, successor)
                    .unwrap()
            );
            store
                .update_session_status(worker, SessionStatus::Running)
                .unwrap();
        }
        assert_eq!(p.manager.reconcile_issue_worker_watches().await, 1);
        assert_eq!(watches_on(&p, worker).await, vec![successor]);
        assert_eq!(p.manager.reconcile_issue_worker_watches().await, 0);
        let candidate = control
            .build_automatic_child_watch_candidate(p.owner, worker)
            .await
            .unwrap();
        assert_eq!(candidate.wake_session_id, Some(successor));
        assert!(
            control
                .rearm_child_watch_after_continue(p.owner, worker)
                .await
        );
        assert_eq!(watches_on(&p, worker).await, vec![successor]);
        predecessor = successor;
    }
    // A worker turn ending after both publications wakes the final PM seat.
    let candidate = control
        .build_automatic_child_watch_candidate(p.owner, worker)
        .await
        .unwrap();
    {
        let store = p.manager.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET enabled=0 WHERE wake_mode=?1",
                [format!("on_terminal:{worker}")],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Completed',updated_at=?2 WHERE id=?1",
                rusqlite::params![
                    worker.to_string(),
                    (candidate.created_at + chrono::Duration::seconds(1))
                        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
    }
    let job = match control
        .arm_automatic_child_watch(p.owner, worker)
        .await
        .unwrap()
    {
        crate::session::agent_verbs::ArmWatchOutcome::Armed(job) => job,
        other => panic!("fresh terminal watch expected: {other:?}"),
    };
    assert_eq!(job.wake_session_id, Some(predecessor));
    match p.manager.plan_terminal_watch_fire(&job).await.unwrap() {
        crate::session::WatchFirePlan::Deliver { tip, .. } => assert_eq!(tip, predecessor),
        other => panic!("terminal worker must wake the final PM seat: {other:?}"),
    }
}

/// #2114: the Issue-worker launch commit resolves the watch owner in the same
/// store-lock hold as the row insert. A PM self-succession published while the
/// launch is parked before its commit leaves exactly one enabled watch, and it
/// wakes the new seat, not the retired one.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn launch_commit_watch_wakes_the_seat_published_before_the_commit() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let issue = new_issue(&p, "commit-lock watch").await;
    let launched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(
            p.owner,
            request(issue.display_number, "commit-lock", &p),
        )
        .await
        .unwrap();
    let worker = launched.worker_session_id;
    let _process = crate::session::launch::install_controller_candidate_test_process(worker);
    let (reached, resume) = crate::session::launch::install_controller_candidate_test_pause(
        worker,
        crate::session::launch::ControllerCandidateTestPhase::ManagerActionConfirming,
    );
    let predecessor = p.owner;
    let successor = Uuid::new_v4();
    let succeed = async {
        reached.await.unwrap();
        {
            let store = p.manager.store.lock().await;
            let mut row = store.get_session(predecessor).unwrap().unwrap();
            row.id = successor;
            row.continued_from = Some(predecessor);
            row.rotation_depth += 1;
            row.status = SessionStatus::Running;
            store.insert_session(&row).unwrap();
            store
                .update_session_status(predecessor, SessionStatus::Archived)
                .unwrap();
            assert!(
                store
                    .record_harness_manager_rotation(predecessor, successor)
                    .unwrap()
            );
        }
        resume.send(()).unwrap();
    };
    let (result, ()) = tokio::join!(p.execute(), succeed);
    result.unwrap();
    assert_eq!(watches_on(&p, worker).await, vec![successor]);
    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, worker)
        .await
        .unwrap();
    crate::session::launch::drop_controller_candidate_test_stream(worker);
}

/// #2462: an Issue-bound launch that ends without establishing its worker
/// (here: blocked while the worker row already failed within seconds) still
/// arms the launcher's terminal watch, which fires at once on the terminal
/// worker, and the worker's own log names the cause.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn ended_launch_of_an_already_failed_worker_arms_a_watch_and_names_the_cause() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let issue = new_issue(&p, "fails in seconds").await;
    let launched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "fast-fail", &p))
        .await
        .unwrap();
    let worker = launched.worker_session_id;
    let operation = p
        .manager
        .store
        .lock()
        .await
        .manager_action_operation(launched.action.operation_id)
        .unwrap()
        .unwrap();
    {
        let store = p.manager.store.lock().await;
        if store.get_session(worker).unwrap().is_none() {
            store.insert_session(&bare_session(worker)).unwrap();
        }
        store
            .update_session_status(worker, SessionStatus::Failed)
            .unwrap();
    }
    assert!(watches_on(&p, worker).await.is_empty());

    p.manager
        .settle_ended_issue_worker_launch(&operation, "manager_v2_lifecycle_unconfirmed")
        .await;

    let store = p.manager.store.lock().await;
    let session = store.get_session(worker).unwrap().unwrap();
    assert_eq!(
        session.terminal_reason.as_deref(),
        Some("launch_failed:manager_v2_lifecycle_unconfirmed")
    );
    let events = store.load_events(worker).unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.event_type == EventType::System
                && event.content.contains("manager_v2_lifecycle_unconfirmed")),
        "{events:?}"
    );
    let job = store
        .list_enabled_terminal_watches()
        .unwrap()
        .into_iter()
        .find(|job| job.wake_mode == WakeMode::OnTerminal(worker))
        .expect("the launcher's terminal watch is armed");
    drop(store);
    match p.manager.plan_terminal_watch_fire(&job).await.unwrap() {
        super::WatchFirePlan::Deliver { job_ids, .. } => assert_eq!(job_ids, vec![job.id]),
        other => panic!("an already-terminal worker must fire at once, got {other:?}"),
    }
}

/// #2478: a manager settling an uncertain Issue-worker launch as proven absent
/// (`settle_uncertain_action`) repairs the same silence the daemon reconcile
/// does: the already-failed worker records a non-empty terminal cause and the
/// launcher's terminal watch is armed and fires at once.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settling_an_uncertain_issue_worker_launch_failed_names_the_cause_and_arms_the_watch() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let issue = new_issue(&p, "uncertain launch").await;
    let launched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "uncertain", &p))
        .await
        .unwrap();
    let worker = launched.worker_session_id;
    let launch_op = launched.action.operation_id;
    let claim = p.claim().await;
    assert_eq!(claim.id(), launch_op);
    let version = {
        let store = p.manager.store.lock().await;
        let uncertain = store
            .finish_manager_action(&claim, ManagerActionStateV2::Uncertain, "crash")
            .unwrap();
        if store.get_session(worker).unwrap().is_none() {
            store.insert_session(&bare_session(worker)).unwrap();
        }
        store
            .update_session_status(worker, SessionStatus::Failed)
            .unwrap();
        uncertain.row_version
    };
    assert!(watches_on(&p, worker).await.is_empty());

    // The coordinate grant bumped the policy version past `Pilot::request`'s.
    let settle = p
        .manager
        .agent_control()
        .agent_manager_control(
            p.owner,
            AgentManagerControlRequestV2::new(
                ManagerFenceV2 {
                    scope_version: 1,
                    policy_version: 2,
                },
                "settle-uncertain-launch".into(),
                ManagerActionV2::SettleUncertainAction {
                    operation_id: launch_op,
                    expected_row_version: version,
                },
            ),
        )
        .await
        .unwrap();
    p.execute().await.unwrap();

    assert_eq!(
        p.receipt(launch_op).await.state,
        ManagerActionStateV2::Failed
    );
    assert_eq!(
        p.receipt(settle.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    let store = p.manager.store.lock().await;
    let session = store.get_session(worker).unwrap().unwrap();
    assert_eq!(
        session.terminal_reason.as_deref(),
        Some("launch_failed:manager_v2_recovered_effect_absent")
    );
    let job = store
        .list_enabled_terminal_watches()
        .unwrap()
        .into_iter()
        .find(|job| job.wake_mode == WakeMode::OnTerminal(worker))
        .expect("the launcher's terminal watch is armed");
    drop(store);
    match p.manager.plan_terminal_watch_fire(&job).await.unwrap() {
        super::WatchFirePlan::Deliver { job_ids, .. } => assert_eq!(job_ids, vec![job.id]),
        other => panic!("an already-terminal worker must fire at once, got {other:?}"),
    }
}

/// #2391: a worker bound to Issue N may read (not update) the Issues N cites
/// as `#M` in its body or its handoffs, one level deep, and nothing else.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bound_worker_reads_the_issues_its_own_issue_cites() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let in_body = new_issue(&p, "context cited in the body").await;
    let in_handoff = new_issue(&p, "context cited in a handoff").await;
    let near_miss = new_issue(&p, "context only a longer number resembles").await;
    let second_level = new_issue(&p, "context cited only by the context").await;
    let uncited = new_issue(&p, "unrelated work").await;
    let implementer = new_issue_with_body(
        &p,
        "implementer work",
        format!(
            "Design decision 6 says failures raise a notice (#{}), see also a#{} and #{}0.",
            in_body.display_number, uncited.display_number, near_miss.display_number
        ),
    )
    .await;
    {
        let store = p.manager.store.lock().await;
        // The cited context cites another Issue itself: not readable.
        store
            .conn
            .execute(
                "UPDATE issues SET body=?2 WHERE id=?1",
                rusqlite::params![
                    in_body.id.to_string(),
                    format!("See #{}.", second_level.display_number)
                ],
            )
            .unwrap();
    }
    let control = p.manager.agent_control();
    let worker = control
        .agent_manager_launch_issue_worker(
            p.owner,
            request(implementer.display_number, "cite-1", &p),
        )
        .await
        .unwrap()
        .worker_session_id;
    let process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();
    let get = |number: i64| {
        let p = &p;
        async move {
            p.manager.store.lock().await.agent_get_issue(
                worker,
                &AgentGetIssueRequestV1 {
                    project_id: None,
                    issue_id: None,
                    display_number: Some(number),
                },
            )
        }
    };
    // A body citation grants the read of the title, status and body.
    let read = get(in_body.display_number).await.unwrap();
    assert_eq!(read.issue.id, in_body.id);
    assert_eq!(read.issue.title, in_body.title);
    assert_eq!(read.issue.status, in_body.status);
    // Neither a glued `a#N`, a longer number, an unrelated Issue, nor the
    // context's own citation (second level) is readable.
    for denied in [&uncited, &near_miss, &in_handoff, &second_level] {
        let error = get(denied.display_number).await.unwrap_err();
        assert!(error.to_string().contains("authority_denied"), "{error}");
    }
    // A handoff citation grants the read too.
    {
        let store = p.manager.store.lock().await;
        store
            .conn
            .execute(
                "INSERT INTO issue_handoffs
                   (issue_id, sequence, project_id, author_session_id, idempotency_key, body, created_at)
                 VALUES (?1, 1, ?2, ?3, 'cite-handoff', ?4, '2026-10-14T00:00:00.000000000Z')",
                rusqlite::params![
                    implementer.id.to_string(),
                    p.project.to_string(),
                    worker.to_string(),
                    format!("Checked the context in (#{}).", in_handoff.display_number)
                ],
            )
            .unwrap();
    }
    assert_eq!(
        get(in_handoff.display_number).await.unwrap().issue.id,
        in_handoff.id
    );
    // Read-only: the bound worker cannot update a cited Issue.
    let store = p.manager.store.lock().await;
    let error = store
        .agent_update_issue(
            worker,
            &rsi_common::rpc::AgentUpdateIssueRequestV1 {
                display_number: Some(in_body.display_number),
                body: Some(format!("{}\nedit", read.issue.body)),
                ..AgentUpdateIssueRequestV1::new(read.issue.row_version, "write-cited".into())
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("bound_issue"), "{error}");
    drop(store);
    drop(process);
}

/// #2547: a citation in a handoff older than the newest 16 still grants the
/// read; an uncited Issue stays denied.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bound_worker_reads_an_issue_cited_in_an_old_handoff() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let cited = new_issue(&p, "context cited in the oldest handoff").await;
    let uncited = new_issue(&p, "unrelated work").await;
    let implementer = new_issue(&p, "implementer work").await;
    let control = p.manager.agent_control();
    let worker = control
        .agent_manager_launch_issue_worker(
            p.owner,
            request(implementer.display_number, "old-cite-1", &p),
        )
        .await
        .unwrap()
        .worker_session_id;
    let process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();
    {
        let store = p.manager.store.lock().await;
        for sequence in 1..=17_i64 {
            let body = if sequence == 1 {
                format!("Checked the context in (#{}).", cited.display_number)
            } else {
                format!("Progress note {sequence}, nothing cited.")
            };
            store
                .conn
                .execute(
                    "INSERT INTO issue_handoffs
                       (issue_id, sequence, project_id, author_session_id, idempotency_key, body, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, '2026-10-14T00:00:00.000000000Z')",
                    rusqlite::params![
                        implementer.id.to_string(),
                        sequence,
                        p.project.to_string(),
                        worker.to_string(),
                        format!("old-cite-handoff-{sequence}"),
                        body
                    ],
                )
                .unwrap();
        }
    }
    let get = |number: i64| {
        let p = &p;
        async move {
            p.manager.store.lock().await.agent_get_issue(
                worker,
                &AgentGetIssueRequestV1 {
                    project_id: None,
                    issue_id: None,
                    display_number: Some(number),
                },
            )
        }
    };
    let read = get(cited.display_number).await.unwrap();
    assert_eq!(read.issue.id, cited.id);
    let error = get(uncited.display_number).await.unwrap_err();
    assert!(error.to_string().contains("authority_denied"), "{error}");
    drop(process);
}

/// #2537: success fixtures launch the Pilot's own default grant, never a
/// hard-coded provider model. Reseeding that default changes the model and
/// effort every launch fixture uses; the resolver still accepts the pair and
/// the launch reaches its positive end state (a watched, established worker
/// on the seeded model).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn success_fixture_launch_follows_the_seeded_default_and_its_resolver() {
    let seeded = pilot_seed_launch();
    let reseeded = ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: crate::session::provider_model_validation::SCRIPTED_TEST_RESEEDED_MODEL.into(),
        effort: Some("high".into()),
    };
    assert_ne!(reseeded, seeded);
    let p = pilot_with_default_launch(reseeded.clone()).await;
    assert_eq!(p.default_launch(), reseeded);
    p.manager
        .validate_manager_launch_models(std::slice::from_ref(&p.default_launch()))
        .await
        .unwrap();
    p.manager
        .preflight_manager_action_model(reseeded.provider, &reseeded.model)
        .await
        .unwrap();

    grant_issue_coordinate(&p).await;
    let issue = new_issue(&p, "reseeded default").await;
    let launch_request = request(issue.display_number, "reseeded-1", &p);
    assert_eq!(launch_request.launch, reseeded);
    let launched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, launch_request)
        .await
        .unwrap();
    let worker = launched.worker_session_id;
    let _process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();

    let established = p.receipt(launched.action.operation_id).await;
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
    assert_eq!(row.model.as_deref(), Some(reseeded.model.as_str()));
    assert_eq!(watches_on(&p, worker).await, vec![p.owner]);
}

/// #212: a commit that names the local number only in its body is an incidental
/// mention, not published work on the bound Issue; the subject match is kept and
/// the brief labels the hint advisory.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn launch_brief_commit_hint_ignores_body_only_mentions_and_stays_advisory() {
    let p = pilot().await;
    grant_issue_coordinate(&p).await;
    let mut issue = None;
    for n in 1..=12 {
        issue = Some(new_issue(&p, &format!("bound work {n}")).await);
    }
    let issue = issue.unwrap();
    assert_eq!(issue.display_number, 12);

    std::fs::write(p.repo.join("incidental"), "x\n").unwrap();
    git(&p.repo, &["add", "incidental"]);
    git(
        &p.repo,
        &[
            "commit",
            "-qm",
            "docs(provider): residual triage",
            "-m",
            "Mentions historical provider exposure #12 in passing.",
        ],
    );
    std::fs::write(p.repo.join("genuine"), "y\n").unwrap();
    git(&p.repo, &["add", "genuine"]);
    git(
        &p.repo,
        &["commit", "-qm", "fix(#12): genuine published work"],
    );
    git(
        &p.repo,
        &["update-ref", "refs/remotes/origin/rolling", "HEAD"],
    );

    let result = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(p.owner, request(issue.display_number, "incidental", &p))
        .await
        .unwrap();
    let subjects: Vec<_> = result
        .rolling_issue_commits
        .iter()
        .map(|commit| commit.subject.as_str())
        .collect();
    assert_eq!(subjects, ["fix(#12): genuine published work"]);

    let process =
        crate::session::launch::install_controller_candidate_test_process(result.worker_session_id);
    p.execute().await.unwrap();
    let worker = p
        .manager
        .store
        .lock()
        .await
        .get_session(result.worker_session_id)
        .unwrap()
        .unwrap();
    assert!(
        worker
            .query
            .contains("fix(#12): genuine published work\nAdvisory only:"),
        "{}",
        worker.query
    );
    assert!(worker.query.contains("never closure evidence"));
    drop(process);
}
