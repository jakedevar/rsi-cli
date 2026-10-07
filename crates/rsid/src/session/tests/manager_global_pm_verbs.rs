//! #1235 (hierarchy S1): the global manager seat holds the project-manager
//! verb set inside its granted projects. Each test asserts one invariant of
//! plan §2.3/§4 (I3, I4, I6, I7, I8, I9) through the daemon's own entry
//! points.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening,
    clippy::large_futures
)]

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
use super::*;
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
use rsi_common::agent_control_schema::AgentControlVerbV1 as Verb;
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
use rsi_common::global_manager::{
    ConfigureGlobalManagerRequestV1, GlobalManagerGrantV1, MANAGER_ANCESTOR_ALLOWANCE_EXCEEDED,
    MANAGER_ISSUE_WORKER_ALREADY_LIVE, MANAGER_PROJECT_NOT_IN_SCOPE,
    MANAGER_TARGET_OWNED_BY_ANCESTOR, RevokeGlobalManagerRequestV1,
};
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
use rsi_common::manager_issue_worker::{
    AgentManagerLaunchIssueWorkerRequestV1, ManagerIssueWorkerWatchV1,
};
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
use rsi_common::rpc::{
    AgentArchiveIssueRequestV1, AgentCreateIssueParams, AgentGetIssueRequestV1,
    AgentListIssuesRequestV1, AgentRestoreIssueRequestV1, AgentUpdateIssueRequestV1,
    AgentUpdateIssueStatusRequestV1,
};
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
use rsi_common::types::{Issue, IssueEventPageRequestV1, IssueStatus, NewIssue, WakeMode};

/// The pilot project A (PM `owner`, Epic `epic`) plus a granted project B
/// with no PM and an ungranted project C. The global seat is a leaf under
/// A's Epic, so only rule (c) keeps the PM's own scope off it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
struct Portfolio {
    p: Pilot,
    seat: Uuid,
    b: Uuid,
    b_epic: Uuid,
    c: Uuid,
    c_epic: Uuid,
    grant: GlobalManagerGrantV1,
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn global_policy(p: &Pilot, mode: ManagerOperatingModeV2) -> ManagerPolicyV2 {
    ManagerPolicyV2 {
        mode,
        capabilities: vec![
            ManagerCapabilityV2::IssueCoordinate,
            ManagerCapabilityV2::SessionCreate,
            ManagerCapabilityV2::SessionControl,
            ManagerCapabilityV2::LeadControl,
            ManagerCapabilityV2::WorkPlan,
            ManagerCapabilityV2::Topology,
            ManagerCapabilityV2::OperatorDelegation,
        ],
        max_created_containers: 8,
        max_created_sessions: 12,
        max_active_sessions: 8,
        allowed_launches: p.policy.allowed_launches.clone(),
        max_recovery_attempts: 3,
        retry_delay_seconds: 1,
        ..Default::default()
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn add_project_with_epic(p: &Pilot, name: &str) -> (Uuid, Uuid) {
    let project = Uuid::new_v4();
    let group = Uuid::new_v4();
    let epic = Uuid::new_v4();
    let store = p.manager.store.lock().await;
    store
        .insert_project(&Project {
            id: project,
            name: format!("{name} {project}"),
            path: Some(p.repo.clone()),
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .unwrap();
    for (id, kind, parent) in [
        (group, SessionKind::Group, None),
        (epic, SessionKind::Epic, Some(group)),
    ] {
        let mut row = bare_session(id);
        row.project_id = Some(project);
        row.working_dir = p.repo.clone();
        row.session_kind = kind;
        row.parent_id = parent;
        store.insert_session(&row).unwrap();
    }
    (project, epic)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn grant_pm(p: &Pilot, capabilities: &[ManagerCapabilityV2], max_created_sessions: u16) {
    let mut policy = p.policy.clone();
    policy.capabilities.extend_from_slice(capabilities);
    policy.max_created_sessions = max_created_sessions;
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: "pm-s1".into(),
            policy,
        })
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn configure_grant(
    p: &Pilot,
    seat: Uuid,
    projects: Vec<Uuid>,
    policy: ManagerPolicyV2,
    expected: i64,
    key: &str,
) -> GlobalManagerGrantV1 {
    p.manager
        .store
        .lock()
        .await
        // The operator has reviewed the cap preview (#1398): these tests
        // exercise cap enforcement, not the confirmation step.
        .configure_global_manager_confirmed(
            &ConfigureGlobalManagerRequestV1 {
                session_id: seat,
                project_ids: projects,
                allowed_launches: p.policy.allowed_launches.clone(),
                project_policy: policy,
                expected_grant_version: expected,
                idempotency_key: key.into(),
            },
            "operator:test",
            true,
        )
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn portfolio_with(mode: ManagerOperatingModeV2) -> Portfolio {
    let p = pilot().await;
    grant_pm(
        &p,
        &[
            ManagerCapabilityV2::IssueCoordinate,
            ManagerCapabilityV2::SessionControl,
        ],
        12,
    )
    .await;
    let (b, b_epic) = add_project_with_epic(&p, "B").await;
    let (c, c_epic) = add_project_with_epic(&p, "C").await;
    let seat = Uuid::new_v4();
    {
        let store = p.manager.store.lock().await;
        let mut row = bare_session(seat);
        row.project_id = Some(p.project);
        row.working_dir = p.repo.clone();
        row.session_kind = SessionKind::Task;
        row.parent_id = Some(p.epic);
        row.status = SessionStatus::Running;
        store.insert_session(&row).unwrap();
    }
    let grant = configure_grant(
        &p,
        seat,
        vec![p.project, b],
        global_policy(&p, mode),
        0,
        "grant-1",
    )
    .await;
    Portfolio {
        p,
        seat,
        b,
        b_epic,
        c,
        c_epic,
        grant,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn portfolio() -> Portfolio {
    portfolio_with(ManagerOperatingModeV2::Execute).await
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn new_issue(p: &Pilot, project: Uuid, title: &str) -> Issue {
    p.manager
        .store
        .lock()
        .await
        .create_issue(&NewIssue {
            project_id: project,
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
fn launch_request(
    p: &Pilot,
    project: Option<Uuid>,
    epic: Uuid,
    issue: i64,
    key: &str,
) -> AgentManagerLaunchIssueWorkerRequestV1 {
    AgentManagerLaunchIssueWorkerRequestV1 {
        project_id: project,
        issue,
        brief: "Implement the Issue and report.".into(),
        launch: p.policy.allowed_launches[0].clone(),
        parent_epic_id: Some(epic),
        idempotency_key: key.into(),
        sandbox_source: None,
        continue_from: None,
        qa_lane: false,
        review_of: None,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn create_session(p: &Pilot, epic: Uuid) -> ManagerActionV2 {
    ManagerActionV2::CreateSession {
        parent_id: epic,
        kind: SessionKind::Task,
        query: "do the work".into(),
        launch: p.policy.allowed_launches[0].clone(),
        sandbox_source: None,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn global_control(
    g: &Portfolio,
    project: Option<Uuid>,
    key: &str,
    operation: ManagerActionV2,
) -> AgentManagerControlRequestV2 {
    AgentManagerControlRequestV2 {
        project_id: project,
        fence: ManagerFenceV2 {
            scope_version: g.grant.grant_version,
            policy_version: g.grant.grant_version,
        },
        idempotency_key: key.into(),
        operation,
    }
}

/// A PM control request at the PM's current policy version.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn pm_control(
    p: &Pilot,
    policy_version: i64,
    key: &str,
    operation: ManagerActionV2,
) -> AgentManagerControlRequestV2 {
    let mut request = p.request(key, operation);
    request.fence.policy_version = policy_version;
    request
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn code(error: &DaemonError) -> String {
    error.to_string()
}

/// Insert a live leaf under `epic` in `project`, as a launch would.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn live_leaf(p: &Pilot, id: Uuid, project: Uuid, epic: Uuid) {
    let mut row = bare_session(id);
    row.project_id = Some(project);
    row.working_dir = p.repo.clone();
    row.session_kind = SessionKind::Task;
    row.parent_id = Some(epic);
    row.status = SessionStatus::Running;
    p.manager.store.lock().await.insert_session(&row).unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn global_launches_an_issue_worker_in_a_project_without_a_pm() {
    let g = portfolio().await;
    let p = &g.p;
    let issue = new_issue(p, g.b, "work in B").await;
    let control = p.manager.agent_control();
    let launched = control
        .agent_manager_launch_issue_worker(
            g.seat,
            launch_request(p, Some(g.b), g.b_epic, issue.display_number, "g-b-1"),
        )
        .await
        .unwrap();
    assert!(!launched.deduplicated);
    assert_eq!(launched.issue_id, issue.id);
    assert_eq!(launched.issue_status, IssueStatus::InProgress);
    let worker = launched.worker_session_id;
    {
        let store = p.manager.store.lock().await;
        let body: String = store
            .conn
            .query_row(
                "SELECT body FROM issues WHERE id=?1",
                [issue.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(body.contains(&worker.to_string()), "{body}");
        // The ledger principal is (B, the global lineage root, the epoch).
        let principal: (String, String, i64) = store
            .conn
            .query_row(
                "SELECT project_id,manager_session_id,scope_version FROM harness_manager_v2_operations WHERE id=?1",
                [launched.action.operation_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            principal,
            (g.b.to_string(), g.seat.to_string(), g.grant.grant_version)
        );
    }
    let process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();
    let established = p.receipt(launched.action.operation_id).await;
    assert_eq!(established.state, ManagerActionStateV2::Succeeded);
    let row = p
        .manager
        .store
        .lock()
        .await
        .get_session(worker)
        .unwrap()
        .unwrap();
    assert_eq!(row.project_id, Some(g.b));
    assert_eq!(row.parent_id, Some(g.b_epic));
    // The on_terminal watch is armed on the global seat.
    let watchers: Vec<Uuid> = p
        .manager
        .store
        .lock()
        .await
        .list_enabled_terminal_watches()
        .unwrap()
        .into_iter()
        .filter(|job| job.wake_mode == WakeMode::OnTerminal(worker))
        .filter_map(|job| job.wake_session_id)
        .collect();
    assert_eq!(watchers, vec![g.seat]);
    let replay = control
        .agent_manager_launch_issue_worker(
            g.seat,
            launch_request(p, Some(g.b), g.b_epic, issue.display_number, "g-b-1"),
        )
        .await
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.watch, ManagerIssueWorkerWatchV1::Armed);
    // The global reads its worker's events through the target-session path.
    control
        .agent_get_status(g.seat, worker)
        .await
        .expect("the global reads a session in a granted project");
    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, worker)
        .await
        .unwrap();
    crate::session::launch::drop_controller_candidate_test_stream(worker);
    drop(process);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn every_project_bound_verb_serves_a_granted_project_and_refuses_one_outside_the_grant() {
    let g = portfolio().await;
    let p = &g.p;
    let control = p.manager.agent_control();
    let issue_b = new_issue(p, g.b, "B issue").await;
    let issue_c = new_issue(p, g.c, "C issue").await;
    let not_in_scope = |error: DaemonError| {
        assert!(
            code(&error).contains(MANAGER_PROJECT_NOT_IN_SCOPE),
            "expected {MANAGER_PROJECT_NOT_IN_SCOPE}, got {error}"
        );
    };
    // A granted project serves every verb outright in this fixture.
    let served = |result: std::result::Result<(), DaemonError>| {
        if let Err(error) = result {
            panic!("a granted project must be served: {error}");
        }
    };
    for (project, issue, in_grant) in [(g.b, &issue_b, true), (g.c, &issue_c, false)] {
        let check = |result: std::result::Result<(), DaemonError>| {
            if in_grant {
                served(result);
            } else {
                not_in_scope(result.unwrap_err());
            }
        };
        let target = Some(project);
        check(
            control
                .agent_create_issue(
                    g.seat,
                    AgentCreateIssueParams {
                        project_id: target,
                        title: "created by the global".into(),
                        body: String::new(),
                        priority: None,
                        labels: Vec::new(),
                        assignee: None,
                        idempotency_key: format!("create-{project}"),
                        harness: false,
                        source_issue: None,
                    },
                )
                .await
                .map(drop),
        );
        check(
            control
                .agent_list_issues(
                    g.seat,
                    AgentListIssuesRequestV1 {
                        project_id: target,
                        ..Default::default()
                    },
                )
                .await
                .map(drop),
        );
        check(
            control
                .agent_get_issue(
                    g.seat,
                    AgentGetIssueRequestV1 {
                        project_id: target,
                        issue_id: None,
                        display_number: Some(issue.display_number),
                    },
                )
                .await
                .map(drop),
        );
        check(
            control
                .agent_list_issue_events(
                    g.seat,
                    IssueEventPageRequestV1 {
                        project_id: target,
                        issue_id: issue.id,
                        after_sequence: 0,
                        limit: None,
                    },
                )
                .await
                .map(drop),
        );
        check(
            control
                .agent_update_issue(
                    g.seat,
                    AgentUpdateIssueRequestV1 {
                        project_id: target,
                        issue_id: Some(issue.id),
                        display_number: None,
                        expected_row_version: issue.row_version,
                        idempotency_key: format!("title-{project}"),
                        title: Some("retitled by the global".into()),
                        body: None,
                        labels: None,
                        priority: None,
                        clear_priority: false,
                        assignee: None,
                        clear_assignee: false,
                    },
                )
                .await
                .map(drop),
        );
        let current = p
            .manager
            .store
            .lock()
            .await
            .get_issue(issue.id)
            .unwrap()
            .unwrap();
        check(
            control
                .agent_update_issue_status(
                    g.seat,
                    AgentUpdateIssueStatusRequestV1 {
                        project_id: target,
                        issue_id: Some(issue.id),
                        display_number: None,
                        status: IssueStatus::Closed,
                        expected_row_version: current.row_version,
                        idempotency_key: format!("status-{project}"),
                    },
                )
                .await
                .map(drop),
        );
        let current = p
            .manager
            .store
            .lock()
            .await
            .get_issue(issue.id)
            .unwrap()
            .unwrap();
        check(
            control
                .agent_archive_issue(
                    g.seat,
                    AgentArchiveIssueRequestV1 {
                        project_id: target,
                        issue_id: issue.id,
                        expected_row_version: current.row_version,
                        idempotency_key: format!("archive-{project}"),
                    },
                )
                .await
                .map(drop),
        );
        let current = p
            .manager
            .store
            .lock()
            .await
            .get_issue(issue.id)
            .unwrap()
            .unwrap();
        check(
            control
                .agent_restore_issue(
                    g.seat,
                    AgentRestoreIssueRequestV1 {
                        project_id: target,
                        issue_id: issue.id,
                        expected_row_version: current.row_version,
                        idempotency_key: format!("restore-{project}"),
                    },
                )
                .await
                .map(drop),
        );
        let epic = if in_grant { g.b_epic } else { g.c_epic };
        check(
            control
                .agent_manager_progress(
                    g.seat,
                    rsi_common::harness_manager::AgentManagerProgressRequestV1 {
                        project_id: target,
                        ..Default::default()
                    },
                )
                .await
                .map(drop),
        );
        check(
            control
                .agent_manager_inspect(
                    g.seat,
                    AgentManagerInspectRequestV2 {
                        project_id: target,
                        ..Default::default()
                    },
                )
                .await
                .map(drop),
        );
        check(
            control
                .agent_manager_update(
                    g.seat,
                    AgentManagerUpdateRequestV2 {
                        project_id: target,
                        fence: ManagerFenceV2 {
                            scope_version: g.grant.grant_version,
                            policy_version: g.grant.grant_version,
                        },
                        idempotency_key: format!("handoff-{project}"),
                        change: ManagerUpdateV2::Handoff {
                            summary: "global handoff".into(),
                            next_actions: Vec::new(),
                        },
                    },
                )
                .await
                .map(drop),
        );
        check(
            control
                .agent_manager_control(
                    g.seat,
                    global_control(
                        &g,
                        target,
                        &format!("control-{project}"),
                        create_session(p, epic),
                    ),
                )
                .await
                .map(drop),
        );
        let prepared = control
            .agent_manager_prepare_control(
                g.seat,
                AgentManagerPrepareControlRequestV2 {
                    project_id: target,
                    operation: PreparedManagerActionV2::CreateSession {
                        parent_id: epic,
                        kind: SessionKind::Task,
                        query: "prepared work".into(),
                        launch: p.policy.allowed_launches[0].clone(),
                        sandbox_source: None,
                    },
                },
            )
            .await;
        if in_grant {
            let prepared = prepared.unwrap();
            served(
                control
                    .agent_manager_commit_prepared_control(
                        g.seat,
                        AgentManagerCommitPreparedControlRequestV2 {
                            project_id: target,
                            prepared_id: prepared.prepared_id,
                            target_digest: prepared.target_digest,
                            idempotency_key: format!("commit-{project}"),
                        },
                    )
                    .await
                    .map(drop),
            );
        } else {
            not_in_scope(prepared.unwrap_err());
            not_in_scope(
                control
                    .agent_manager_commit_prepared_control(
                        g.seat,
                        AgentManagerCommitPreparedControlRequestV2 {
                            project_id: target,
                            prepared_id: Uuid::new_v4(),
                            target_digest: format!("sha256:{}", "0".repeat(64)),
                            idempotency_key: format!("commit-{project}"),
                        },
                    )
                    .await
                    .unwrap_err(),
            );
        }
        check(
            control
                .agent_manager_launch_issue_worker(
                    g.seat,
                    launch_request(
                        p,
                        target,
                        epic,
                        new_issue(p, project, "worker issue").await.display_number,
                        &format!("w-{project}"),
                    ),
                )
                .await
                .map(drop),
        );
    }
    // GetAction reads a granted project's receipt and refuses C.
    let receipt = control
        .agent_manager_control(
            g.seat,
            global_control(
                &g,
                Some(g.b),
                "control-receipt",
                create_session(p, g.b_epic),
            ),
        )
        .await
        .unwrap();
    let read = control
        .agent_manager_get_action(
            g.seat,
            AgentManagerGetActionRequestV2 {
                project_id: Some(g.b),
                operation_id: receipt.operation_id,
            },
        )
        .await
        .unwrap();
    assert_eq!(read.operation_id, receipt.operation_id);
    not_in_scope(
        control
            .agent_manager_get_action(
                g.seat,
                AgentManagerGetActionRequestV2 {
                    project_id: Some(g.c),
                    operation_id: receipt.operation_id,
                },
            )
            .await
            .unwrap_err(),
    );
    // Progress returns the fence for the target project.
    let progress = control
        .agent_manager_progress(
            g.seat,
            rsi_common::harness_manager::AgentManagerProgressRequestV1 {
                project_id: Some(g.b),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(progress.config.project_id, g.b);
    let fence = progress.fence.expect("progress names the fence");
    assert_eq!(
        (fence.scope_version, fence.policy_version),
        (g.grant.grant_version, g.grant.grant_version)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn the_pm_is_unchanged_and_cannot_name_another_project() {
    let g = portfolio().await;
    let p = &g.p;
    let control = p.manager.agent_control();
    // Omitted and own project behave exactly as before.
    for project in [None, Some(p.project)] {
        control
            .agent_list_issues(
                p.owner,
                AgentListIssuesRequestV1 {
                    project_id: project,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let progress = control
            .agent_manager_progress(
                p.owner,
                rsi_common::harness_manager::AgentManagerProgressRequestV1 {
                    project_id: project,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(progress.config.manager_session_id, p.owner);
    }
    // The PM of A passing project B is refused.
    let error = control
        .agent_list_issues(
            p.owner,
            AgentListIssuesRequestV1 {
                project_id: Some(g.b),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        code(&error).contains(MANAGER_PROJECT_NOT_IN_SCOPE),
        "{error}"
    );
    let error = control
        .agent_manager_control(
            p.owner,
            AgentManagerControlRequestV2 {
                project_id: Some(g.b),
                ..p.request("pm-b", create_session(p, g.b_epic))
            },
        )
        .await
        .unwrap_err();
    assert!(
        code(&error).contains(MANAGER_PROJECT_NOT_IN_SCOPE),
        "{error}"
    );
    let error = control
        .agent_manager_progress(
            p.owner,
            rsi_common::harness_manager::AgentManagerProgressRequestV1 {
                project_id: Some(g.b),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        code(&error).contains(MANAGER_PROJECT_NOT_IN_SCOPE),
        "{error}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn one_live_issue_worker_per_issue_across_the_chain() {
    let g = portfolio().await;
    let p = &g.p;
    let issue = new_issue(p, p.project, "shared work").await;
    let control = p.manager.agent_control();
    let first = control
        .agent_manager_launch_issue_worker(
            p.owner,
            launch_request(p, None, p.epic, issue.display_number, "pm-1"),
        )
        .await
        .unwrap();
    let error = control
        .agent_manager_launch_issue_worker(
            g.seat,
            launch_request(p, Some(p.project), p.epic, issue.display_number, "g-1"),
        )
        .await
        .unwrap_err();
    assert!(
        code(&error).contains(MANAGER_ISSUE_WORKER_ALREADY_LIVE),
        "{error}"
    );
    // The PM's own second key is refused too; its exact replay deduplicates.
    let error = control
        .agent_manager_launch_issue_worker(
            p.owner,
            launch_request(p, None, p.epic, issue.display_number, "pm-2"),
        )
        .await
        .unwrap_err();
    assert!(
        code(&error).contains(MANAGER_ISSUE_WORKER_ALREADY_LIVE),
        "{error}"
    );
    let replay = control
        .agent_manager_launch_issue_worker(
            p.owner,
            launch_request(p, None, p.epic, issue.display_number, "pm-1"),
        )
        .await
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.worker_session_id, first.worker_session_id);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn mutations_flow_down_and_reads_go_by_coverage() {
    let g = portfolio().await;
    let p = &g.p;
    let control = p.manager.agent_control();
    let by_global = control
        .agent_manager_control(
            g.seat,
            global_control(&g, Some(p.project), "g-a", create_session(p, p.epic)),
        )
        .await
        .unwrap()
        .target_session_id
        .unwrap();
    let by_pm = control
        .agent_manager_control(p.owner, pm_control(p, 2, "pm-a", create_session(p, p.epic)))
        .await
        .unwrap()
        .target_session_id
        .unwrap();
    live_leaf(p, by_global, p.project, p.epic).await;
    live_leaf(p, by_pm, p.project, p.epic).await;
    let store = p.manager.store.lock().await;
    // PM halt of a global-launched worker: refused.
    let error = store
        .manager_session_control_scope(p.owner, by_global, true)
        .unwrap_err();
    assert!(
        code(&error).contains(MANAGER_TARGET_OWNED_BY_ANCESTOR),
        "{error}"
    );
    // Global halt of a PM-launched worker: accepted.
    let scope = store
        .manager_session_control_scope(g.seat, by_pm, true)
        .unwrap()
        .expect("the global reaches the PM's worker");
    assert_eq!(scope.target.id, by_pm);
    assert_eq!(scope.config.manager_session_id, g.seat);
    // PM halt of the global seat: refused, though the seat sits in its Epic.
    let error = store
        .manager_session_control_scope(p.owner, g.seat, true)
        .unwrap_err();
    assert!(
        code(&error).contains(MANAGER_TARGET_OWNED_BY_ANCESTOR),
        "{error}"
    );
    // Reads go by coverage: the PM still reads the global's worker.
    assert!(
        store
            .manager_session_control_scope(p.owner, by_global, false)
            .unwrap()
            .is_some()
    );
    // A project outside the grant is not reached at all.
    drop(store);
    let foreign = Uuid::new_v4();
    live_leaf(p, foreign, g.c, g.c_epic).await;
    assert!(
        p.manager
            .store
            .lock()
            .await
            .manager_session_control_scope(g.seat, foreign, false)
            .unwrap()
            .is_none()
    );
    // The daemon verb surfaces the same refusal.
    let error = control.agent_halt(p.owner, by_global).await.unwrap_err();
    assert!(
        code(&error).contains(MANAGER_TARGET_OWNED_BY_ANCESTOR),
        "{error}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn budgets_are_charged_up_the_chain() {
    let g = portfolio().await;
    let p = &g.p;
    grant_pm_again(p, 3).await;
    let mut policy = global_policy(p, ManagerOperatingModeV2::Execute);
    policy.max_created_sessions = 3;
    let grant = configure_grant(
        p,
        g.seat,
        vec![p.project, g.b],
        policy,
        g.grant.grant_version,
        "grant-cap-3",
    )
    .await;
    let control = p.manager.agent_control();
    for key in ["pm-1", "pm-2"] {
        control
            .agent_manager_control(p.owner, pm_control(p, 3, key, create_session(p, p.epic)))
            .await
            .unwrap();
    }
    control
        .agent_manager_control(
            g.seat,
            AgentManagerControlRequestV2 {
                project_id: Some(p.project),
                fence: ManagerFenceV2 {
                    scope_version: grant.grant_version,
                    policy_version: grant.grant_version,
                },
                idempotency_key: "g-1".into(),
                operation: create_session(p, p.epic),
            },
        )
        .await
        .unwrap();
    let error = control
        .agent_manager_control(p.owner, pm_control(p, 3, "pm-3", create_session(p, p.epic)))
        .await
        .unwrap_err();
    assert!(
        code(&error).contains(MANAGER_ANCESTOR_ALLOWANCE_EXCEEDED),
        "{error}"
    );
}

/// The PM's policy at version 2 (after `grant_pm`) with a session cap.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn grant_pm_again(p: &Pilot, max_created_sessions: u16) {
    let mut policy = p.policy.clone();
    policy.capabilities.extend_from_slice(&[
        ManagerCapabilityV2::IssueCoordinate,
        ManagerCapabilityV2::SessionControl,
    ]);
    policy.max_created_sessions = max_created_sessions;
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: 2,
            idempotency_key: "pm-cap".into(),
            policy,
        })
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn prepare_in_b(g: &Portfolio) -> ManagerPreparedActionReceiptV2 {
    g.p.manager
        .agent_control()
        .agent_manager_prepare_control(
            g.seat,
            AgentManagerPrepareControlRequestV2 {
                project_id: Some(g.b),
                operation: PreparedManagerActionV2::CreateSession {
                    parent_id: g.b_epic,
                    kind: SessionKind::Task,
                    query: "prepared work".into(),
                    launch: g.p.policy.allowed_launches[0].clone(),
                    sandbox_source: None,
                },
            },
        )
        .await
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn commit_in_b(
    g: &Portfolio,
    caller: Uuid,
    prepared: &ManagerPreparedActionReceiptV2,
) -> Result<ManagerPreparedActionCommitResultV2> {
    g.p.manager
        .agent_control()
        .agent_manager_commit_prepared_control(
            caller,
            AgentManagerCommitPreparedControlRequestV2 {
                project_id: Some(g.b),
                prepared_id: prepared.prepared_id,
                target_digest: prepared.target_digest.clone(),
                idempotency_key: "commit".into(),
            },
        )
        .await
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_replaced_or_revoked_grant_refuses_the_prepared_commit() {
    // Replacement: the operator re-grants the same seat (a new epoch).
    let g = portfolio().await;
    let prepared = prepare_in_b(&g).await;
    configure_grant(
        &g.p,
        g.seat,
        vec![g.p.project, g.b],
        global_policy(&g.p, ManagerOperatingModeV2::Execute),
        g.grant.grant_version,
        "grant-2",
    )
    .await;
    let error = commit_in_b(&g, g.seat, &prepared).await.unwrap_err();
    assert!(
        code(&error).contains("manager_v2_prepared_authority_changed"),
        "{error}"
    );

    // Revocation: the seat no longer holds any grant.
    let g = portfolio().await;
    let prepared = prepare_in_b(&g).await;
    g.p.manager
        .store
        .lock()
        .await
        .revoke_global_manager(&RevokeGlobalManagerRequestV1 {
            expected_grant_version: g.grant.grant_version,
            idempotency_key: "revoke".into(),
        })
        .unwrap();
    let error = commit_in_b(&g, g.seat, &prepared).await.unwrap_err();
    assert!(
        code(&error).contains(MANAGER_PROJECT_NOT_IN_SCOPE),
        "{error}"
    );

    // A stale fence after re-grant: the old epoch is refused at admission.
    let g = portfolio().await;
    configure_grant(
        &g.p,
        g.seat,
        vec![g.p.project, g.b],
        global_policy(&g.p, ManagerOperatingModeV2::Execute),
        g.grant.grant_version,
        "grant-3",
    )
    .await;
    let error =
        g.p.manager
            .agent_control()
            .agent_manager_control(
                g.seat,
                global_control(&g, Some(g.b), "stale", create_session(&g.p, g.b_epic)),
            )
            .await
            .unwrap_err();
    assert!(
        code(&error).contains("manager_node_authority_changed"),
        "{error}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_context_cap_transfer_keeps_the_ledger_and_retires_the_predecessor_token() {
    let g = portfolio().await;
    let p = &g.p;
    let control = p.manager.agent_control();
    let worker = control
        .agent_manager_control(
            g.seat,
            global_control(&g, Some(g.b), "before", create_session(p, g.b_epic)),
        )
        .await
        .unwrap()
        .target_session_id
        .unwrap();
    live_leaf(p, worker, g.b, g.b_epic).await;
    let successor = Uuid::new_v4();
    {
        let store = p.manager.store.lock().await;
        let mut row = bare_session(successor);
        row.project_id = Some(p.project);
        row.working_dir = p.repo.clone();
        row.session_kind = SessionKind::Task;
        row.parent_id = Some(p.epic);
        row.continued_from = Some(g.seat);
        row.status = SessionStatus::Running;
        store.insert_session(&row).unwrap();
        assert!(store.transfer_global_seat(g.seat, successor).unwrap());
        // The successor's principal is the predecessor's: same root, epoch.
        let caller = store.resolve_manager_caller(successor, Some(g.b)).unwrap();
        let crate::store::harness_manager_v2::ManagerCallerV1::Global(authority) = caller else {
            panic!("the successor resolves the global arm");
        };
        assert_eq!(authority.config.manager_session_id, g.seat);
        assert_eq!(authority.config.row_version, g.grant.grant_version);
        // It controls its predecessor's worker.
        assert!(
            store
                .manager_session_control_scope(successor, worker, true)
                .unwrap()
                .is_some()
        );
        // The predecessor's token is refused.
        let error = store.resolve_manager_caller(g.seat, Some(g.b)).unwrap_err();
        assert!(
            code(&error).contains("manager_node_custody_changed"),
            "{error}"
        );
        assert!(
            store
                .manager_session_control_scope(g.seat, worker, true)
                .unwrap()
                .is_none()
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn the_catalog_lists_the_pm_verbs_the_project_policy_grants() {
    let g = portfolio().await;
    let projection =
        g.p.manager
            .store
            .lock()
            .await
            .agent_authority_projection(g.seat)
            .unwrap();
    assert!(projection.guidance_ids.contains(&"portfolio_manager"));
    for verb in [
        Verb::ManagerLaunchIssueWorker,
        Verb::ManagerControl,
        Verb::ManagerPrepareControl,
        Verb::ManagerCommitPreparedControl,
        Verb::ManagerGetAction,
        Verb::ManagerProgress,
        Verb::ManagerInspect,
        Verb::ManagerUpdate,
        Verb::ListIssues,
        Verb::GetIssue,
        Verb::UpdateIssue,
        Verb::UpdateIssueStatus,
        Verb::ArchiveIssue,
        Verb::RestoreIssue,
        Verb::ListIssueEvents,
        Verb::GlobalOverview,
    ] {
        assert!(projection.verbs.contains(&verb), "execute lists {verb:?}");
    }
    // Approval-answer and other operator methods stay default-denied even
    // with OperatorDelegation in the project policy.
    assert_eq!(projection.delegated_operator_methods, Vec::<&str>::new());
    assert!(projection.control_actions.iter().all(|action| !matches!(
        action,
        ManagerActionKindV2::OperatorCall
            | ManagerActionKindV2::SucceedManager
            | ManagerActionKindV2::Integrate
    )));
    let rendered =
        crate::session::preamble::render_authority_catalog(g.seat, &projection, None).unwrap();
    assert!(rendered.roles.contains(&"global_manager".to_string()));

    let status = portfolio_with(ManagerOperatingModeV2::Status).await;
    let projection = status
        .p
        .manager
        .store
        .lock()
        .await
        .agent_authority_projection(status.seat)
        .unwrap();
    for verb in [
        Verb::ListIssues,
        Verb::GetIssue,
        Verb::ListIssueEvents,
        Verb::ManagerProgress,
        Verb::ManagerInspect,
        Verb::ManagerGetAction,
    ] {
        assert!(projection.verbs.contains(&verb), "status lists {verb:?}");
    }
    let reads_only = [
        Verb::ManagerLaunchIssueWorker,
        Verb::ManagerControl,
        Verb::ManagerPrepareControl,
        Verb::ManagerUpdate,
        Verb::UpdateIssue,
        Verb::UpdateIssueStatus,
        Verb::ArchiveIssue,
    ];
    assert_eq!(
        reads_only
            .iter()
            .filter(|verb| projection.verbs.contains(verb))
            .count(),
        0,
        "a Status-mode policy lists reads only: {:?}",
        projection.verbs
    );
    // The call-time guard matches the catalog: a Status-mode global reads but
    // never mutates an Issue.
    let issue = new_issue(&status.p, status.b, "status").await;
    let control = status.p.manager.agent_control();
    control
        .agent_get_issue(
            status.seat,
            AgentGetIssueRequestV1 {
                project_id: Some(status.b),
                issue_id: Some(issue.id),
                display_number: None,
            },
        )
        .await
        .unwrap();
    let error = control
        .agent_update_issue_status(
            status.seat,
            AgentUpdateIssueStatusRequestV1 {
                project_id: Some(status.b),
                issue_id: Some(issue.id),
                display_number: None,
                status: IssueStatus::InProgress,
                expected_row_version: issue.row_version,
                idempotency_key: "status-mutation".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(code(&error).contains("authority_denied"), "{error}");
}

// ---- S1.b: deploy, landing and sandbox jobs --------------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
const DEPLOY_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn deploy_service(dir: &TempDir) -> (crate::deploy::DeployService, std::path::PathBuf) {
    let source = dir.path().join("build");
    let install = dir.path().join("install");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&install).unwrap();
    std::fs::write(source.join("rsid"), b"new-rsid").unwrap();
    std::fs::write(install.join("rsid"), b"old-rsid").unwrap();
    let service = crate::deploy::DeployService::new(
        install,
        vec![dir.path().to_path_buf()],
        Box::new(|| true),
        std::sync::Arc::new(|_: &std::path::Path| Ok((DEPLOY_SHA.to_string(), 999))),
    );
    (service, source)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn deploy_needs_deploy_in_the_global_project_policy() {
    let g = portfolio().await;
    let p = &g.p;
    let dir = TempDir::new().unwrap();
    let (service, source) = deploy_service(&dir);
    let request =
        |project: Uuid, key: &str| rsi_common::agent_deploy::AgentRequestDeployRequestV1 {
            project_id: Some(project),
            sha: DEPLOY_SHA.into(),
            binaries_dir: Some(source.to_string_lossy().into_owned()),
            build: None,
            idempotency_key: key.into(),
            max_wait_secs: None,
            peer_id: None,
            cancel: None,
            interrupt_workers: None,
        };
    let control = p.manager.agent_control();
    let error = control
        .agent_request_deploy_with(
            g.seat,
            request(g.b, "no-deploy"),
            &service,
            chrono::Utc::now(),
        )
        .await
        .unwrap_err();
    assert!(
        code(&error).contains("deploy_capability_required"),
        "{error}"
    );

    let mut policy = global_policy(p, ManagerOperatingModeV2::Execute);
    policy.capabilities.push(ManagerCapabilityV2::Deploy);
    configure_grant(
        p,
        g.seat,
        vec![p.project, g.b],
        policy,
        g.grant.grant_version,
        "grant-deploy",
    )
    .await;
    let projection = p
        .manager
        .store
        .lock()
        .await
        .agent_authority_projection(g.seat)
        .unwrap();
    assert!(projection.verbs.contains(&Verb::RequestDeploy));
    let error = control
        .agent_request_deploy_with(
            g.seat,
            request(g.c, "outside"),
            &service,
            chrono::Utc::now(),
        )
        .await
        .unwrap_err();
    assert!(
        code(&error).contains(MANAGER_PROJECT_NOT_IN_SCOPE),
        "{error}"
    );
    let receipt = control
        .agent_request_deploy_with(g.seat, request(g.b, "deploy"), &service, chrono::Utc::now())
        .await
        .unwrap();
    assert_eq!(receipt.sha, DEPLOY_SHA);
    assert!(!receipt.replayed);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn landing_uses_the_in_reach_source_sandbox_and_refuses_one_out_of_reach() {
    let g = portfolio().await;
    let p = &g.p;
    let worker = Uuid::new_v4();
    live_leaf(p, worker, g.b, g.b_epic).await;
    let outside = Uuid::new_v4();
    live_leaf(p, outside, g.c, g.c_epic).await;
    let head = git(&p.repo, &["rev-parse", "HEAD"]);
    let request = |project: Option<Uuid>, source: Uuid, key: &str| {
        rsi_common::rolling_queue::AgentEnqueueLandingSourceRequestV1 {
            project_id: project,
            source_session_id: Some(source),
            source_commit: head.clone(),
            test_filters: Vec::new(),
            idempotency_key: key.into(),
        }
    };
    let control = p.manager.agent_control();
    let receipt = control
        .agent_enqueue_landing_source(g.seat, request(Some(g.b), worker, "land-b"), true)
        .await
        .unwrap();
    assert_eq!(receipt.entry.source_commit, head);
    let (project, repo): (Option<String>, String) = p
        .manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT project_id,repo_path FROM rolling_queue_entries WHERE id=?1",
            [receipt.entry.id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(project, Some(g.b.to_string()));
    assert_eq!(repo, p.repo.display().to_string());
    // A source outside the named project, and one out of reach, are refused.
    let error = control
        .agent_enqueue_landing_source(g.seat, request(Some(p.project), worker, "land-a"), true)
        .await
        .unwrap_err();
    assert!(
        code(&error).contains(MANAGER_PROJECT_NOT_IN_SCOPE),
        "{error}"
    );
    let error = control
        .agent_enqueue_landing_source(g.seat, request(None, outside, "land-c"), true)
        .await
        .unwrap_err();
    assert!(code(&error).contains("queue_not_authorized"), "{error}");
    // The catalog advertises the landing verb to an executing global seat.
    let projection = p
        .manager
        .store
        .lock()
        .await
        .agent_authority_projection(g.seat)
        .unwrap();
    assert!(projection.verbs.contains(&Verb::EnqueueLandingSource));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_sandbox_job_runs_only_in_a_terminal_in_reach_session() {
    use crate::agent_jobs::{JobRuntime, JobTools, LaunchSpec};
    use rsi_common::agent_jobs::{AgentSubmitJobRequestV1, JobKind};

    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<LaunchSpec>>);
    impl JobRuntime for Recorder {
        fn launch(&self, spec: &LaunchSpec) -> std::result::Result<(), String> {
            self.0.lock().unwrap().push(spec.clone());
            Ok(())
        }
        fn unit_active(&self, _unit: &str) -> bool {
            true
        }
    }
    let g = portfolio().await;
    let p = &g.p;
    let sandbox = TempDir::new().unwrap();
    let (live, done) = (Uuid::new_v4(), Uuid::new_v4());
    live_leaf(p, live, g.b, g.b_epic).await;
    live_leaf(p, done, g.b, g.b_epic).await;
    {
        let store = p.manager.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Completed',sandbox_root=?2 WHERE id=?1",
                rusqlite::params![done.to_string(), sandbox.path().display().to_string()],
            )
            .unwrap();
    }
    let tools = || JobTools {
        cargo_slot: "/x/cargo-slot".into(),
        lander: "/x/rsi-rolling-land".into(),
    };
    let request = |target: Uuid| AgentSubmitJobRequestV1 {
        project_id: Some(g.b),
        sandbox_session_id: Some(target),
        kind: JobKind::Landing,
        params: serde_json::json!({"accepted": DEPLOY_SHA}),
        name: Some("land".into()),
        idempotency_key: Some(format!("land-{target}")),
        worktree: None,
        wake: None,
    };
    let control = p.manager.agent_control();
    let recorder = std::sync::Arc::new(Recorder::default());
    let error = control
        .agent_submit_job(g.seat, request(live), recorder.clone(), tools())
        .await
        .unwrap_err();
    assert!(code(&error).contains("job_sandbox_session_live"), "{error}");
    assert!(recorder.0.lock().unwrap().is_empty());
    let receipt = control
        .agent_submit_job(g.seat, request(done), recorder.clone(), tools())
        .await;
    if cfg!(target_os = "linux") {
        let receipt = receipt.unwrap();
        assert_eq!(receipt.job.owner_session_id, g.seat);
        let launched = recorder.0.lock().unwrap();
        assert_eq!(launched.len(), 1);
        assert!(
            launched[0].cwd.starts_with(sandbox.path()),
            "{:?}",
            launched[0].cwd
        );
    } else {
        // Landing jobs are Linux-only; the authority check still ran first.
        assert!(code(&receipt.unwrap_err()).contains("job_platform_unsupported"));
    }
}

/// #1236 (S2): a second root over [C] next to the global over [A, B]. Each
/// seat's Issue verbs reach only its own coverage, a root overlapping A is
/// refused, and the global's own verbs keep working.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_disjoint_roots_each_reach_only_their_own_coverage() {
    use rsi_common::portfolio_nodes::{ConfigurePortfolioNodeRequestV1, MANAGER_SCOPE_OVERLAP};
    let g = portfolio().await;
    let p = &g.p;
    let second = Uuid::new_v4();
    let third = Uuid::new_v4();
    {
        let store = p.manager.store.lock().await;
        for (id, project, epic) in [(second, g.c, g.c_epic), (third, g.c, g.c_epic)] {
            let mut row = bare_session(id);
            row.project_id = Some(project);
            row.working_dir = p.repo.clone();
            row.session_kind = SessionKind::Task;
            row.parent_id = Some(epic);
            row.status = SessionStatus::Running;
            store.insert_session(&row).unwrap();
        }
    }
    let root = |seat: Uuid, projects: Vec<Uuid>, key: &str| ConfigurePortfolioNodeRequestV1 {
        node_id: None,
        parent_node_id: None,
        adopt_node_ids: Vec::new(),
        expected_parent_grant_version: None,
        tier_label: "global".into(),
        seat_session_id: seat,
        project_ids: projects,
        allowed_launches: p.policy.allowed_launches.clone(),
        policy: global_policy(p, ManagerOperatingModeV2::Execute),
        child_policy: None,
        max_direct_reports: 5,
        expected_node_grant_version: 0,
        expected_authority_epoch: 0,
        idempotency_key: key.into(),
    };
    {
        let store = p.manager.store.lock().await;
        store
            .configure_portfolio_node(
                &root(second, vec![g.c], "root-c"),
                crate::store::portfolio_nodes::PortfolioGrantor::Operator,
                "operator:test",
            )
            .unwrap();
        let overlap = store
            .configure_portfolio_node(
                &root(third, vec![p.project], "root-overlap"),
                crate::store::portfolio_nodes::PortfolioGrantor::Operator,
                "operator:test",
            )
            .unwrap_err();
        assert!(code(&overlap).contains(MANAGER_SCOPE_OVERLAP), "{overlap}");
    }
    let control = p.manager.agent_control();
    let create = |seat: Uuid, project: Uuid, key: &str| {
        let control = control.clone();
        let key = key.to_string();
        async move {
            control
                .agent_create_issue(
                    seat,
                    AgentCreateIssueParams {
                        project_id: Some(project),
                        title: "portfolio root work".into(),
                        body: String::new(),
                        priority: None,
                        labels: Vec::new(),
                        assignee: None,
                        idempotency_key: key,
                        harness: false,
                        source_issue: None,
                    },
                )
                .await
        }
    };
    create(second, g.c, "second-c").await.unwrap();
    create(g.seat, g.b, "global-b").await.unwrap();
    for (seat, project) in [
        (second, p.project),
        (second, g.b),
        (g.seat, g.c),
        (third, p.project),
    ] {
        let refused = create(seat, project, &format!("{seat}-{project}"))
            .await
            .unwrap_err();
        assert!(
            code(&refused).contains(MANAGER_PROJECT_NOT_IN_SCOPE),
            "{seat} in {project}: {refused}"
        );
    }
    let listed = control
        .agent_list_issues(
            second,
            AgentListIssuesRequestV1 {
                project_id: Some(g.b),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        code(&listed).contains(MANAGER_PROJECT_NOT_IN_SCOPE),
        "{listed}"
    );
}

/// #1256: disjoint roots whose seats are hosted in the same project. The
/// global over [A, B] never halts or continues the seat of the root over
/// [C] (nor that root's rotation successor) through the shared control gate
/// or the daemon verbs; its own leaf stays in reach. The targets are stored
/// sessions only, so a refusal happens before any process effect.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_never_controls_a_sibling_roots_seat_hosted_in_its_project() {
    use rsi_common::agent_coordination::AgentContinueChildRequestV1;
    use rsi_common::portfolio_nodes::ConfigurePortfolioNodeRequestV1;
    let g = portfolio().await;
    let p = &g.p;
    let (sibling, successor, own_leaf) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    live_leaf(p, sibling, p.project, p.epic).await;
    live_leaf(p, own_leaf, p.project, p.epic).await;
    {
        let store = p.manager.store.lock().await;
        let mut row = bare_session(successor);
        row.project_id = Some(p.project);
        row.working_dir = p.repo.clone();
        row.session_kind = SessionKind::Task;
        row.parent_id = Some(p.epic);
        row.status = SessionStatus::Running;
        row.continued_from = Some(sibling);
        store.insert_session(&row).unwrap();
        store
            .configure_portfolio_node(
                &ConfigurePortfolioNodeRequestV1 {
                    node_id: None,
                    parent_node_id: None,
                    adopt_node_ids: Vec::new(),
                    expected_parent_grant_version: None,
                    tier_label: "global".into(),
                    seat_session_id: sibling,
                    project_ids: vec![g.c],
                    allowed_launches: p.policy.allowed_launches.clone(),
                    policy: global_policy(p, ManagerOperatingModeV2::Execute),
                    child_policy: None,
                    max_direct_reports: 5,
                    expected_node_grant_version: 0,
                    expected_authority_epoch: 0,
                    idempotency_key: "root-c".into(),
                },
                crate::store::portfolio_nodes::PortfolioGrantor::Operator,
                "operator:test",
            )
            .unwrap();
        for target in [sibling, successor] {
            for mutation in [true, false] {
                assert!(
                    store
                        .manager_session_control_scope(g.seat, target, mutation)
                        .unwrap()
                        .is_none(),
                    "{target} mutation={mutation}"
                );
            }
        }
        assert!(
            store
                .manager_session_control_scope(g.seat, own_leaf, true)
                .unwrap()
                .is_some()
        );
    }
    let control = p.manager.agent_control();
    for target in [sibling, successor] {
        assert!(
            control.agent_halt(g.seat, target).await.is_err(),
            "halt {target}"
        );
        let continued = p
            .manager
            .agent_continue_child(
                g.seat,
                AgentContinueChildRequestV1 {
                    target_session_id: target,
                    query: "keep going".into(),
                    expected_tip_session_id: target,
                    expected_event_sequence: 0,
                    expected_custody_generation: None,
                    idempotency_key: Some(format!("continue-{target}")),
                },
            )
            .await;
        assert!(continued.is_err(), "continue {target}: {continued:?}");
    }
    // Nothing ran: both seats keep their stored status.
    let store = p.manager.store.lock().await;
    for target in [sibling, successor] {
        assert_eq!(
            store.get_session(target).unwrap().unwrap().status,
            SessionStatus::Running
        );
    }
}

/// #1237 (hierarchy S3): levels above global, on this file's fixtures.
#[path = "manager_portfolio_levels.rs"]
mod portfolio_levels;

/// Post-land review #1272: granted resource limits and lead-action ownership.
#[path = "manager_review_1272.rs"]
mod review_1272;

// ---- #1277-#1279: authority is rechecked at the effect ---------------------

/// How the grant changes between admission and effect.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[derive(Clone, Copy, Debug)]
enum GrantChange {
    Revoke,
    Replace,
}

/// A hook that changes the seat's grant exactly at the await point.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn change_grant(
    g: &Portfolio,
    change: GrantChange,
    policy: ManagerPolicyV2,
) -> impl FnOnce(&crate::store::Store) + Send + 'static {
    let (seat, version, allowed, projects) = (
        g.seat,
        g.grant.grant_version,
        g.p.policy.allowed_launches.clone(),
        vec![g.p.project, g.b],
    );
    move |store| match change {
        GrantChange::Revoke => {
            store
                .revoke_global_manager(&RevokeGlobalManagerRequestV1 {
                    expected_grant_version: version,
                    idempotency_key: "revoke-at-effect".into(),
                })
                .unwrap();
        }
        GrantChange::Replace => {
            store
                .configure_global_manager(
                    &ConfigureGlobalManagerRequestV1 {
                        session_id: seat,
                        project_ids: projects,
                        allowed_launches: allowed,
                        project_policy: policy,
                        expected_grant_version: version,
                        idempotency_key: "replace-at-effect".into(),
                    },
                    "operator:test",
                )
                .unwrap();
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn row_count(g: &Portfolio, table: &str) -> i64 {
    let sql = match table {
        "rolling_queue_entries" => "SELECT count(*) FROM rolling_queue_entries",
        "agent_jobs" => "SELECT count(*) FROM agent_jobs",
        _ => "SELECT count(*) FROM agent_deploys",
    };
    g.p.manager
        .store
        .lock()
        .await
        .conn
        .query_row(sql, [], |row| row.get(0))
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_landing_enqueue_refuses_a_grant_changed_during_the_source_probe() {
    for change in [GrantChange::Revoke, GrantChange::Replace] {
        let g = portfolio().await;
        let worker = Uuid::new_v4();
        live_leaf(&g.p, worker, g.b, g.b_epic).await;
        let head = git(&g.p.repo, &["rev-parse", "HEAD"]);
        let request = |key: &str| rsi_common::rolling_queue::AgentEnqueueLandingSourceRequestV1 {
            project_id: Some(g.b),
            source_session_id: Some(worker),
            source_commit: head.clone(),
            test_filters: Vec::new(),
            idempotency_key: key.into(),
        };
        crate::session::effect_fence::seam::install(
            g.seat,
            "enqueue",
            change_grant(
                &g,
                change,
                global_policy(&g.p, ManagerOperatingModeV2::Execute),
            ),
        );
        let error =
            g.p.manager
                .agent_control()
                .agent_enqueue_landing_source(g.seat, request("raced"), true)
                .await
                .unwrap_err();
        assert!(
            code(&error).contains("queue_not_authorized"),
            "{change:?}: {error}"
        );
        assert_eq!(
            row_count(&g, "rolling_queue_entries").await,
            0,
            "{change:?}"
        );
    }
    // An unchanged grant still enqueues across the recheck.
    let g = portfolio().await;
    let worker = Uuid::new_v4();
    live_leaf(&g.p, worker, g.b, g.b_epic).await;
    let head = git(&g.p.repo, &["rev-parse", "HEAD"]);
    g.p.manager
        .agent_control()
        .agent_enqueue_landing_source(
            g.seat,
            rsi_common::rolling_queue::AgentEnqueueLandingSourceRequestV1 {
                project_id: Some(g.b),
                source_session_id: Some(worker),
                source_commit: head,
                test_filters: Vec::new(),
                idempotency_key: "steady".into(),
            },
            true,
        )
        .await
        .unwrap();
    assert_eq!(row_count(&g, "rolling_queue_entries").await, 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_sandbox_job_refuses_a_grant_changed_during_the_directory_probe() {
    use crate::agent_jobs::{JobRuntime, JobTools, LaunchSpec};
    use rsi_common::agent_jobs::{AgentSubmitJobRequestV1, JobKind};

    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<LaunchSpec>>);
    impl JobRuntime for Recorder {
        fn launch(&self, spec: &LaunchSpec) -> std::result::Result<(), String> {
            self.0.lock().unwrap().push(spec.clone());
            Ok(())
        }
        fn unit_active(&self, _unit: &str) -> bool {
            true
        }
    }
    for change in [GrantChange::Revoke, GrantChange::Replace] {
        let g = portfolio().await;
        let sandbox = TempDir::new().unwrap(); // tmpfs-fixture-ok: no sandbox is allocated
        let done = Uuid::new_v4();
        live_leaf(&g.p, done, g.b, g.b_epic).await;
        g.p.manager
            .store
            .lock()
            .await
            .conn
            .execute(
                "UPDATE sessions SET status='Completed',sandbox_root=?2 WHERE id=?1",
                rusqlite::params![done.to_string(), sandbox.path().display().to_string()],
            )
            .unwrap();
        crate::session::effect_fence::seam::install(
            g.seat,
            "submit_job",
            change_grant(
                &g,
                change,
                global_policy(&g.p, ManagerOperatingModeV2::Execute),
            ),
        );
        let recorder = std::sync::Arc::new(Recorder::default());
        let error =
            g.p.manager
                .agent_control()
                .agent_submit_job(
                    g.seat,
                    AgentSubmitJobRequestV1 {
                        project_id: Some(g.b),
                        sandbox_session_id: Some(done),
                        kind: JobKind::Landing,
                        params: serde_json::json!({"accepted": DEPLOY_SHA}),
                        name: Some("land".into()),
                        idempotency_key: Some("raced".into()),
                        worktree: None,
                        wake: None,
                    },
                    recorder.clone(),
                    JobTools {
                        cargo_slot: "/x/cargo-slot".into(),
                        lander: "/x/rsi-rolling-land".into(),
                    },
                )
                .await
                .unwrap_err();
        assert!(
            code(&error).contains("job_kind_not_authorized"),
            "{change:?}: {error}"
        );
        assert!(recorder.0.lock().unwrap().is_empty(), "{change:?}");
        assert_eq!(row_count(&g, "agent_jobs").await, 0, "{change:?}");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_deploy_refuses_a_grant_changed_while_the_binaries_stage() {
    for change in [GrantChange::Revoke, GrantChange::Replace] {
        let g = portfolio().await;
        let dir = TempDir::new().unwrap(); // tmpfs-fixture-ok: no sandbox is allocated
        let (service, source) = deploy_service(&dir);
        let mut policy = global_policy(&g.p, ManagerOperatingModeV2::Execute);
        policy.capabilities.push(ManagerCapabilityV2::Deploy);
        let grant = configure_grant(
            &g.p,
            g.seat,
            vec![g.p.project, g.b],
            policy.clone(),
            g.grant.grant_version,
            "grant-deploy",
        )
        .await;
        let g = Portfolio { grant, ..g };
        crate::session::effect_fence::seam::install(
            g.seat,
            "stage_deploy",
            change_grant(&g, change, policy),
        );
        let error =
            g.p.manager
                .agent_control()
                .agent_request_deploy_with(
                    g.seat,
                    rsi_common::agent_deploy::AgentRequestDeployRequestV1 {
                        project_id: Some(g.b),
                        sha: DEPLOY_SHA.into(),
                        binaries_dir: Some(source.to_string_lossy().into_owned()),
                        build: None,
                        idempotency_key: "raced".into(),
                        max_wait_secs: None,
                        peer_id: None,
                        cancel: None,
                        interrupt_workers: None,
                    },
                    &service,
                    chrono::Utc::now(),
                )
                .await
                .unwrap_err();
        let text = code(&error);
        assert!(
            text.contains("deploy_not_authorized") || text.contains(MANAGER_PROJECT_NOT_IN_SCOPE),
            "{change:?}: {error}"
        );
        assert_eq!(row_count(&g, "agent_deploys").await, 0, "{change:?}");
        // The staged copy is gone: only the installed binary remains.
        assert_eq!(
            std::fs::read_dir(dir.path().join("install"))
                .unwrap()
                .count(),
            1,
            "{change:?}"
        );
    }
}

/// #1415: the portfolio seat settles the project manager's non-gate decision
/// record through `AgentManagerUpdate`, the audit names the seat, the launch
/// block clears, and a gate record still refuses the seat.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_portfolio_seat_rules_a_pms_non_gate_decision_and_refuses_a_gate() {
    let g = portfolio().await;
    let p = &g.p;
    let control = p.manager.agent_control();
    let pm_fence = ManagerFenceV2 {
        scope_version: 1,
        policy_version: 2,
    };
    for (key, question, gate) in [
        ("census-access", "Who may read the census extract?", None),
        (
            "ship",
            "Merge the ingest branch to main?",
            Some(ManagerDecisionGateV2::MainOrRelease),
        ),
    ] {
        control
            .agent_manager_update(
                p.owner,
                AgentManagerUpdateRequestV2 {
                    project_id: None,
                    fence: pm_fence.clone(),
                    idempotency_key: format!("ask-{key}"),
                    change: ManagerUpdateV2::Decision {
                        key: key.into(),
                        expected_row_version: 0,
                        epic_id: p.epic,
                        question: question.into(),
                        request_id: None,
                        work_key: None,
                        gate,
                        options: vec![],
                    },
                },
            )
            .await
            .unwrap();
    }
    let rulings = control
        .agent_manager_inspect(
            g.seat,
            AgentManagerInspectRequestV2 {
                project_id: Some(p.project),
                section: ManagerInspectSectionV2::Rulings,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(rulings.rows.len(), 1, "the gate is not on offer");
    let row = rulings.rows[0].clone();
    assert_eq!(row["key"], "census-access");
    let ruling =
        |key: &str, row: &serde_json::Value, idempotency: &str| AgentManagerUpdateRequestV2 {
            project_id: Some(p.project),
            fence: ManagerFenceV2 {
                scope_version: g.grant.grant_version,
                policy_version: g.grant.grant_version,
            },
            idempotency_key: idempotency.into(),
            change: ManagerUpdateV2::DecisionRuling {
                key: key.into(),
                expected_row_version: row["row_version"].as_i64().unwrap(),
                target_digest: row["target_digest"].as_str().unwrap().into(),
                answer: "Read access for the census team only".into(),
                owner_manager_session_id: row["owner_manager_session_id"]
                    .as_str()
                    .map(|id| Uuid::parse_str(id).unwrap()),
            },
        };
    let (config, ship) = {
        let store = p.manager.store.lock().await;
        let config = store.get_harness_manager(p.project).unwrap().unwrap();
        assert!(
            store.manager_v2_decision_gate(&config, p.epic).is_err(),
            "a pending record blocks launches"
        );
        let ship = store
            .manager_v2_record(&config, "decision", "ship")
            .unwrap()
            .unwrap();
        (config, ship)
    };
    let refused = control
        .agent_manager_update(
            g.seat,
            ruling(
                "ship",
                &serde_json::json!({
                    "row_version": ship.row_version,
                    "target_digest": ship.payload["target_digest"],
                    "owner_manager_session_id": config.manager_session_id,
                }),
                "rule-ship",
            ),
        )
        .await;
    assert!(
        code(&refused.unwrap_err()).contains("manager_v2_decision_operator_gate"),
        "a real gate stays with the operator"
    );
    let epic = p.epic;
    let receipt = control
        .agent_manager_update(g.seat, ruling("census-access", &row, "rule-census"))
        .await
        .unwrap();
    assert!(!receipt.deduplicated);
    let store = p.manager.store.lock().await;
    // The pending-decision launch block clears for the settled record only.
    let still = store.manager_v2_decision_gate(&config, epic).unwrap_err();
    assert!(code(&still).contains("manager_v2_pending_operator_decision"));
    let settled = store
        .manager_v2_record(&config, "decision", "census-access")
        .unwrap()
        .unwrap();
    assert_eq!(settled.payload["status"], "answered");
    assert_eq!(settled.payload["answered_by"]["kind"], "portfolio_manager");
    assert_eq!(
        settled.payload["answered_by"]["session_id"],
        serde_json::json!(g.seat)
    );
    assert_eq!(
        store
            .manager_v2_record(&config, "decision", "ship")
            .unwrap()
            .unwrap()
            .payload["status"],
        "pending"
    );
}
