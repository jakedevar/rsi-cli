use super::*;
use crate::{
    bus::EventBus,
    config::{Config, RuntimeConfig},
    session::SessionManager,
    store::{
        Store,
        sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding},
    },
};
use chrono::Utc;
use rsi_common::{
    harness_manager::ConfigureHarnessManagerRequestV1,
    types::{
        Project, SandboxCleanupState, SandboxKind, SessionKind, SessionProvider, SessionStatus,
    },
};
use std::{path::PathBuf, process::Command, sync::Arc};
use tempfile::TempDir;

mod bookkeeping_cas;
mod review_end;
mod review_infra_retry;

fn command(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CEILING_DIRECTORIES", "/var/tmp")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
struct Fixture {
    dir: TempDir,
    sessions: SessionManager,
    handle: AgentControlHandle,
    manager: Uuid,
    epic: Uuid,
    source: Uuid,
    reviewer: Uuid,
    invocation: Uuid,
    source_root: PathBuf,
    review_root: PathBuf,
    allocation_commit: String,
    source_head: String,
}
async fn fixture() -> Fixture {
    fixture_with_merged_source(false).await
}

async fn fixture_with_merged_source(merge_rolling: bool) -> Fixture {
    let dir = tempfile::Builder::new()
        .prefix("evidence-")
        .tempdir_in("/var/tmp/ham-v2-fd23a414")
        .unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    command(&root, &["init", "-b", "rolling"]);
    command(&root, &["config", "user.name", "Test"]);
    command(&root, &["config", "user.email", "test@example.invalid"]);
    command(
        &root,
        &[
            "config",
            "--local",
            "rsi.managerIntegrationTarget",
            "local-only",
        ],
    );
    std::fs::write(root.join("code.txt"), "baseline\n").unwrap();
    command(&root, &["add", "code.txt"]);
    command(&root, &["commit", "-m", "base"]);
    let source = Uuid::new_v4();
    let reviewer = Uuid::new_v4();
    let invocation = Uuid::new_v4();
    let sandbox_base = dir.path().join("sandboxes");
    std::fs::create_dir(&sandbox_base).unwrap();
    let source_root = sandbox_base.join(source.to_string());
    command(
        &root,
        &[
            "worktree",
            "add",
            "-b",
            "source",
            source_root.to_str().unwrap(),
        ],
    );
    std::fs::write(source_root.join("code.txt"), "implemented\n").unwrap();
    command(&source_root, &["commit", "-am", "source"]);
    let allocation_commit = command(&root, &["rev-parse", "HEAD"]);
    if merge_rolling {
        std::fs::write(root.join("shared"), "top\nmiddle\nbottom\n").unwrap();
        command(&root, &["add", "shared"]);
        command(&root, &["commit", "-m", "rolling shared content"]);
        command(&source_root, &["merge", "--no-edit", "rolling"]);
        std::fs::write(
            source_root.join("shared"),
            "top\nmiddle\nbottom\nsource addition\n",
        )
        .unwrap();
        command(&source_root, &["commit", "-am", "source shared addition"]);
    }
    let source_head = command(&source_root, &["rev-parse", "HEAD"]);
    let review_root = sandbox_base.join(reviewer.to_string());
    command(
        &root,
        &[
            "worktree",
            "add",
            "-b",
            "review",
            review_root.to_str().unwrap(),
            &source_head,
        ],
    );
    let mut store = Store::open(&dir.path().join("rsi.db")).unwrap();
    let project = Uuid::new_v4();
    store
        .insert_project(&Project {
            id: project,
            name: "Evidence feature".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .unwrap();
    let mut manager =
        crate::session::agent_verbs::tests::test_session(Uuid::new_v4(), root.clone());
    manager.session_kind = SessionKind::Standard;
    manager.status = SessionStatus::Completed;
    manager.project_id = Some(project);
    store.insert_session(&manager).unwrap();
    let mut group = manager.clone();
    group.id = Uuid::new_v4();
    group.session_kind = SessionKind::Group;
    store.insert_session(&group).unwrap();
    let mut epic = manager.clone();
    epic.id = Uuid::new_v4();
    epic.session_kind = SessionKind::Epic;
    epic.parent_id = Some(group.id);
    store.insert_session(&epic).unwrap();
    for (id, inv, path, branch, base) in [
        (
            source,
            Uuid::new_v4(),
            &source_root,
            "source",
            &allocation_commit,
        ),
        (reviewer, invocation, &review_root, "review", &source_head),
    ] {
        let mut session = manager.clone();
        session.id = id;
        session.session_kind = SessionKind::Feature;
        session.parent_id = Some(epic.id);
        session.status = SessionStatus::Starting;
        session.sandbox_kind = Some(SandboxKind::GitWorktree);
        session.sandbox_root = Some(path.clone());
        session.sandbox_branch = Some(branch.into());
        session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        session.model = Some(
            if id == source {
                "gpt-6-sol"
            } else {
                "claude-sonnet-5"
            }
            .into(),
        );
        store.conn.execute("INSERT INTO model_invocations(id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,provider,model,trigger_source,session_id,policy_snapshot_json,usage_confidence,created_at) VALUES(?1,'session_launch_fresh','session_lifecycle','foreground','paid_capable','admitted','running','Claude','test','manager_evidence_test',?2,'{}','unavailable',?3)",params![inv.to_string(),id.to_string(),Utc::now().to_rfc3339()]).unwrap();
        store
            .insert_direct_session_with_custody_and_invocation(
                &session,
                SessionCustodyBinding::New(NewCustodyRoot {
                    custody_id: Uuid::new_v4(),
                    canonical_repo_dir: root.display().to_string(),
                    sandbox_root: path.display().to_string(),
                    sandbox_branch: branch.into(),
                    repository_identity: std::fs::canonicalize(root.join(".git"))
                        .unwrap()
                        .display()
                        .to_string(),
                    source_commit: base.clone(),
                    cause: CustodyCause::FreshLaunch,
                }),
                inv,
            )
            .unwrap();
        store
            .update_session_status(id, SessionStatus::Completed)
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE model_invocations SET status='completed',completed_at=?2 WHERE id=?1",
                params![inv.to_string(), Utc::now().to_rfc3339()],
            )
            .unwrap();
    }
    store.set_lead_session(epic.id, Some(source)).unwrap();
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: project,
            session_id: manager.id,
            epic_ids: Some(vec![epic.id]),
            expected_row_version: 0,
        })
        .unwrap();
    store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: project,
            expected_scope_version: 1,
            expected_policy_version: 0,
            idempotency_key: "grant".into(),
            policy: ManagerPolicyV2 {
                capabilities: vec![
                    ManagerCapabilityV2::WorkPlan,
                    ManagerCapabilityV2::Integration,
                    ManagerCapabilityV2::SessionCreate,
                ],
                max_created_sessions: 8,
                ..Default::default()
            },
        })
        .unwrap();
    let runtime = RuntimeConfig::from_config(&Config::from_env());
    let sessions = SessionManager::new(
        Arc::new(EventBus::new(16)),
        store,
        false,
        dir.path().join("daemon.sock"),
        None,
        Vec::new(),
        runtime,
        sandbox_base,
    )
    .unwrap();
    let handle = sessions.agent_control();
    handle
        .agent_manager_update(
            manager.id,
            req(
                ManagerUpdateV2::Work {
                    key: "product".into(),
                    expected_row_version: 0,
                    epic_id: epic.id,
                    title: "Product".into(),
                    kind: ManagerWorkKindV2::Product,
                    priority: 1,
                    weight: 1,
                    required_gates: vec![
                        ManagerWorkStageV2::Implementation,
                        ManagerWorkStageV2::Review,
                        ManagerWorkStageV2::Verification,
                    ],
                },
                "work",
            ),
        )
        .await
        .unwrap();
    Fixture {
        dir,
        sessions,
        handle,
        manager: manager.id,
        epic: epic.id,
        source,
        reviewer,
        invocation,
        source_root,
        review_root,
        allocation_commit,
        source_head,
    }
}

// Queue a legitimate operator scope edit immediately behind the inspector's
// first Store acquisition. Pages with no external observation are one snapshot;
// Work must still reject a changed scope across its validation boundary.
async fn inspect_with_queued_scope_edit(
    section: ManagerInspectSectionV2,
    operator: bool,
) -> Result<ManagerInspectionV2> {
    use std::future::Future;
    use std::task::{Context, Waker};

    let f = fixture().await;
    let guard = f.handle.store.lock().await;
    let project = guard
        .get_session(f.manager)
        .unwrap()
        .unwrap()
        .project_id
        .unwrap();
    let query = AgentManagerInspectRequestV2 {
        section,
        ..Default::default()
    };
    let mut inspection = Box::pin(async {
        if operator {
            f.handle
                .get_harness_manager_state(GetHarnessManagerStateRequestV2 {
                    project_id: project,
                    query,
                })
                .await
        } else {
            f.handle.agent_manager_inspect(f.manager, query).await
        }
    });
    let mut scope_edit = Box::pin(async {
        let store = f.handle.store.lock().await;
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: vec![],
                project_id: project,
                session_id: f.manager,
                epic_ids: Some(vec![]),
                expected_row_version: 1,
            })
            .unwrap();
    });
    // Explicit polling establishes FIFO order without sleeps or test-only
    // production callbacks. The held guard makes both futures wait.
    let mut cx = Context::from_waker(Waker::noop());
    assert!(inspection.as_mut().poll(&mut cx).is_pending());
    assert!(scope_edit.as_mut().poll(&mut cx).is_pending());
    drop(guard);
    let (result, ()) = tokio::join!(inspection, scope_edit);
    assert_eq!(
        f.handle
            .store
            .lock()
            .await
            .get_harness_manager(project)
            .unwrap()
            .unwrap()
            .row_version,
        2
    );
    result
}

#[tokio::test]
async fn manager_overview_returns_one_authorized_snapshot_before_queued_scope_edit() {
    for operator in [false, true] {
        let snapshot = inspect_with_queued_scope_edit(ManagerInspectSectionV2::Overview, operator)
            .await
            .unwrap();
        assert_eq!(snapshot.scope_version, 1);
        assert!(snapshot.rows.iter().any(|row| row["type"] == "overview"));
        let control = snapshot
            .rows
            .iter()
            .find(|row| row["type"] == "manager_control")
            .unwrap();
        assert_eq!(control["eligibility"], "eligible");
        assert!(control["expected"]["authority_epoch"].as_i64().unwrap() > 0);
        assert!(control["current_session_id"].is_string());
    }
}

#[tokio::test]
async fn manager_decisions_return_one_authorized_snapshot_before_queued_scope_edit() {
    for operator in [false, true] {
        let snapshot = inspect_with_queued_scope_edit(ManagerInspectSectionV2::Decisions, operator)
            .await
            .unwrap();
        assert_eq!(snapshot.scope_version, 1);
    }
}

#[tokio::test]
async fn manager_work_rechecks_scope_after_external_observation_boundary() {
    for operator in [false, true] {
        assert!(
            inspect_with_queued_scope_edit(ManagerInspectSectionV2::Work, operator)
                .await
                .is_err()
        );
    }
}

async fn reopen_mail_fixture(f: &mut Fixture) {
    let sessions = SessionManager::new(
        Arc::new(EventBus::new(16)),
        Store::open(&f.dir.path().join("rsi.db")).unwrap(),
        false,
        f.dir.path().join("unused.sock"),
        None,
        vec![],
        RuntimeConfig::from_config(&Config::from_env()),
        f.dir.path().join("sandboxes"),
    )
    .unwrap();
    f.handle = sessions.agent_control();
    f.sessions = sessions;
}

async fn rotate_mail_lead(f: &Fixture) -> Uuid {
    let store = f.handle.store.lock().await;
    let mut successor = store.get_session(f.source).unwrap().unwrap();
    successor.id = Uuid::new_v4();
    successor.continued_from = Some(f.source);
    successor.rotation_depth += 1;
    // Receipt consumer fixture, not a provider/custody establishment test.
    successor.sandbox_root = None;
    successor.sandbox_branch = None;
    successor.sandbox_kind = None;
    successor.sandbox_cleanup_state = None;
    store.insert_session(&successor).unwrap();
    store
        .update_session_status(f.source, SessionStatus::Archived)
        .unwrap();
    assert!(
        store
            .record_harness_manager_rotation(f.source, successor.id)
            .unwrap()
    );
    store.set_lead_session(f.epic, Some(successor.id)).unwrap();
    successor.id
}

#[tokio::test]
async fn rotated_request_consumers_track_current_reader_and_durable_execution_evidence() {
    use rsi_common::harness_manager::*;
    let mut f = fixture().await;
    f.handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Stage {
                    key: "product".into(),
                    expected_row_version: 1,
                    stage: ManagerWorkStageV2::Implementation,
                    state: ManagerStageStateV2::Partial,
                    note: "Real committed source, awaiting acceptance".into(),
                    evidence: Some(ManagerEvidenceV2 {
                        source_session_id: f.source,
                        source_commit: f.source_head.clone(),
                        artifact_path: String::new(),
                        artifact_commit: f.source_head.clone(),
                        closure_evidence_id: None,
                    }),
                },
                "partial-source",
            ),
        )
        .await
        .unwrap();
    let mut ids = Vec::new();
    for key in ["product", "report"] {
        ids.push(
            f.handle
                .agent_manager_send(
                    f.manager,
                    AgentManagerSendRequestV1 {
                        epic_id: f.epic,
                        message: format!("Track {key} through rotation"),
                        idempotency_key: key.into(),
                    },
                )
                .await
                .unwrap()
                .message_id,
        );
    }
    f.handle
        .agent_manager_inbox(
            f.source,
            AgentManagerInboxRequestV1 {
                request_id: Some(ids[0]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let successor = rotate_mail_lead(&f).await;
    let transition = |id, version, state, work: Option<&str>| {
        req(
            ManagerUpdateV2::Request {
                request_id: id,
                expected_row_version: version,
                state,
                message: format!("Current recipient {state:?}"),
                work_key: work.map(str::to_owned),
            },
            &format!("request-{id}-{version}"),
        )
    };
    for (version, state) in [
        (0, ManagerRequestStateV2::Accepted),
        (1, ManagerRequestStateV2::Running),
        (2, ManagerRequestStateV2::Completed),
    ] {
        reopen_mail_fixture(&mut f).await;
        f.sessions
            .register_agent_token("mail-current".into(), successor)
            .await;
        f.sessions
            .register_agent_token("mail-predecessor".into(), f.source)
            .await;
        let caller = f
            .sessions
            .resolve_agent_token("mail-current")
            .await
            .unwrap();
        let predecessor = f
            .sessions
            .resolve_agent_token("mail-predecessor")
            .await
            .unwrap();
        let rows = f
            .handle
            .agent_manager_inspect(
                f.manager,
                AgentManagerInspectRequestV2 {
                    section: ManagerInspectSectionV2::Requests,
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .rows;
        for (index, id) in ids.iter().copied().enumerate() {
            let row = rows
                .iter()
                .find(|r| r["request_id"] == id.to_string())
                .unwrap();
            assert_eq!(row["recipient_session_id"], f.source.to_string());
            assert_eq!(row["effective_recipient_session_id"], successor.to_string());
            let update = transition(id, version, state, (index == 0).then_some("product"));
            assert!(
                f.handle
                    .agent_manager_update(predecessor, update.clone())
                    .await
                    .is_err()
            );
            if version == 0 {
                assert_eq!(row["retrieved"], false);
                assert_eq!(row["state"], "queued");
                assert!(
                    f.handle
                        .agent_manager_update(caller, update.clone())
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("retrieval_required")
                );
                let inbox = f
                    .handle
                    .agent_manager_inbox(
                        caller,
                        AgentManagerInboxRequestV1 {
                            request_id: Some(id),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(inbox.messages[0].message_id, id);
                let retrieved = f
                    .handle
                    .agent_manager_inspect(
                        f.manager,
                        AgentManagerInspectRequestV2 {
                            section: ManagerInspectSectionV2::Requests,
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                let row = retrieved
                    .rows
                    .iter()
                    .find(|r| r["request_id"] == id.to_string())
                    .unwrap();
                assert_eq!(row["retrieved"], true);
                assert_eq!(row["state"], "retrieved");
            }
            if version == 2 && index == 1 {
                assert!(
                    f.handle
                        .agent_manager_update(caller, update.clone())
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("execution_evidence_required")
                );
                f.handle
                    .agent_manager_reply(
                        caller,
                        AgentManagerReplyRequestV1 {
                            request_id: id,
                            message: "Report delivered; product still unaccepted".into(),
                            idempotency_key: "report-reply".into(),
                        },
                    )
                    .await
                    .unwrap();
            }
            let receipt = f
                .handle
                .agent_manager_update(caller, update.clone())
                .await
                .unwrap();
            assert_eq!(receipt.row_version, version + 1);
            assert!(
                f.handle
                    .agent_manager_update(caller, update)
                    .await
                    .unwrap()
                    .deduplicated
            );
        }
    }
    reopen_mail_fixture(&mut f).await;
    let rows = f
        .handle
        .agent_manager_inspect(
            f.manager,
            AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Requests,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .rows;
    for (index, id) in ids.iter().enumerate() {
        let row = rows
            .iter()
            .find(|r| r["request_id"] == id.to_string())
            .unwrap();
        assert_eq!(row["state"], "completed");
        assert_eq!(row["execution"]["lead_session_id"], successor.to_string());
        assert_eq!(row["retrieved"], true);
        assert_eq!(
            row["execution"]["execution_evidence"]["kind"],
            if index == 0 {
                "committed_source"
            } else {
                "attributed_reply"
            }
        );
        if index == 0 {
            assert_eq!(
                row["execution"]["execution_evidence"]["source_commit"],
                f.source_head
            );
        }
    }
    let work = f
        .handle
        .agent_manager_inspect(
            f.manager,
            AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Work,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(work.rows[0]["source_commit"], f.source_head);
    assert_eq!(work.rows[0]["source_accepted"], false);
    assert_eq!(work.rows[0]["integrated"], false);
}

#[tokio::test]
async fn inherited_request_denies_reassignment_retired_ambiguous_and_revoked_receipts() {
    use rsi_common::harness_manager::*;
    for invalidation in ["reassigned", "retired", "ambiguous", "scope"] {
        let f = fixture().await;
        let id = f
            .handle
            .agent_manager_send(
                f.manager,
                AgentManagerSendRequestV1 {
                    epic_id: f.epic,
                    message: "Unfinished inherited request".into(),
                    idempotency_key: "request".into(),
                },
            )
            .await
            .unwrap()
            .message_id;
        let successor = rotate_mail_lead(&f).await;
        f.handle
            .agent_manager_inbox(successor, AgentManagerInboxRequestV1::default())
            .await
            .unwrap();
        let update = req(
            ManagerUpdateV2::Request {
                request_id: id,
                expected_row_version: 0,
                state: ManagerRequestStateV2::Accepted,
                message: "Explicit acceptance".into(),
                work_key: None,
            },
            "accept",
        );
        f.handle
            .agent_manager_update(successor, update.clone())
            .await
            .unwrap();
        let mut caller = successor;
        {
            let store = f.handle.store.lock().await;
            match invalidation {
                "reassigned" => {
                    store.set_lead_session(f.epic, Some(f.reviewer)).unwrap();
                    caller = f.reviewer;
                }
                "retired" => {
                    store
                        .update_session_status(f.source, SessionStatus::Completed)
                        .unwrap();
                    store
                        .update_session_status(f.source, SessionStatus::Archived)
                        .unwrap();
                }
                "ambiguous" => {
                    // Fault injection only: ordinary writes are already fenced
                    // by this unique index. Exercise defensive read validation
                    // against a corrupt receipt set in this disposable DB.
                    store
                        .conn
                        .execute("DROP INDEX harness_manager_rotation_current", [])
                        .unwrap();
                    let mut fork = store.get_session(successor).unwrap().unwrap();
                    fork.id = Uuid::new_v4();
                    store.insert_session(&fork).unwrap();
                    store.conn.execute("INSERT INTO harness_manager_rotation_edges(predecessor_session_id,successor_session_id,committed_at) VALUES(?1,?2,?3)", params![f.source.to_string(),fork.id.to_string(),Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos,true)]).unwrap();
                }
                "scope" => {
                    let project = store
                        .get_session(f.manager)
                        .unwrap()
                        .unwrap()
                        .project_id
                        .unwrap();
                    store
                        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                            group_ids: Vec::new(),
                            project_id: project,
                            session_id: f.manager,
                            epic_ids: Some(vec![]),
                            expected_row_version: 1,
                        })
                        .unwrap();
                }
                _ => unreachable!(),
            }
        }
        f.sessions
            .register_agent_token("invalidated-mail-caller".into(), caller)
            .await;
        let caller = f
            .sessions
            .resolve_agent_token("invalidated-mail-caller")
            .await
            .unwrap();
        assert!(
            f.handle.agent_manager_update(caller, update).await.is_err(),
            "{invalidation}"
        );
        if invalidation != "scope" {
            let inbox = f
                .handle
                .agent_manager_inbox(caller, AgentManagerInboxRequestV1::default())
                .await
                .unwrap();
            assert!(inbox.messages.is_empty(), "{invalidation}");
            let rows = f
                .handle
                .agent_manager_inspect(
                    f.manager,
                    AgentManagerInspectRequestV2 {
                        section: ManagerInspectSectionV2::Requests,
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .rows;
            assert_eq!(rows[0]["delivery_issue"], "lead_changed");
            assert_eq!(rows[0]["recipient_session_id"], f.source.to_string());
            assert!(rows[0]["effective_recipient_session_id"].is_null());
        }
    }
}
fn req(change: ManagerUpdateV2, key: &str) -> AgentManagerUpdateRequestV2 {
    fenced_req(change, key, 1, 1)
}

fn fenced_req(
    change: ManagerUpdateV2,
    key: &str,
    scope_version: i64,
    policy_version: i64,
) -> AgentManagerUpdateRequestV2 {
    AgentManagerUpdateRequestV2 {
        fence: ManagerFenceV2 {
            scope_version,
            policy_version,
        },
        idempotency_key: key.into(),
        change,
    }
}

#[allow(clippy::unwrap_used)]
async fn rotate_db_review_manager(f: &Fixture) -> (Uuid, i64, i64) {
    let store = f.handle.store.lock().await;
    let project = store
        .get_session(f.manager)
        .unwrap()
        .unwrap()
        .project_id
        .unwrap();
    let config_a = store.get_harness_manager(project).unwrap().unwrap();
    let policy_a = store.get_harness_manager_policy(project).unwrap().unwrap();
    let (row_a, work) = store.manager_v2_work(&config_a, "product").unwrap();
    let mut manager_b = store.get_session(f.manager).unwrap().unwrap();
    manager_b.id = Uuid::new_v4();
    store.insert_session(&manager_b).unwrap();
    let config_b = store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: project,
            session_id: manager_b.id,
            epic_ids: Some(vec![f.epic]),
            expected_row_version: config_a.row_version,
        })
        .unwrap();
    let policy_b = store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: project,
            expected_scope_version: config_b.row_version,
            expected_policy_version: policy_a.row_version,
            idempotency_key: "rotation-grant".into(),
            policy: policy_a.policy,
        })
        .unwrap();
    // D19: the work is the identity, so the successor sees it without a rebuild.
    let (carried, carried_work) = store.manager_v2_work(&config_b, "product").unwrap();
    assert_eq!(carried.row_version, row_a.row_version);
    assert_eq!(
        serde_json::to_value(carried_work).unwrap(),
        serde_json::to_value(work).unwrap()
    );
    (manager_b.id, config_b.row_version, policy_b.row_version)
}

async fn request_and_activate_db_review(f: &Fixture, idempotency_key: &str) -> Uuid {
    record_db_review_source(f, idempotency_key).await;
    let assignment_id = request_db_review(f, idempotency_key).await;
    activate_db_review_assignment(f, assignment_id).await;
    assignment_id
}

async fn record_db_review_source(f: &Fixture, idempotency_key: &str) {
    f.handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Stage {
                    key: "product".into(),
                    expected_row_version: 1,
                    stage: ManagerWorkStageV2::Implementation,
                    state: ManagerStageStateV2::Partial,
                    note: "Exact source ready for DB-native review".into(),
                    evidence: Some(ManagerEvidenceV2 {
                        source_session_id: f.source,
                        source_commit: f.source_head.clone(),
                        artifact_path: String::new(),
                        artifact_commit: f.source_head.clone(),
                        closure_evidence_id: None,
                    }),
                },
                &format!("{idempotency_key}-source"),
            ),
        )
        .await
        .unwrap();
}

async fn request_db_review(f: &Fixture, idempotency_key: &str) -> Uuid {
    let receipt = f
        .handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::RequestReview {
                    key: "product".into(),
                    expected_row_version: 2,
                    source_commit: f.source_head.clone(),
                    query: "Review the exact source and submit the DB receipt.".into(),
                    launch: ManagerLaunchChoiceV2 {
                        provider: SessionProvider::Claude,
                        model: "claude-sonnet-5".into(),
                        effort: None,
                    },
                },
                idempotency_key,
            ),
        )
        .await
        .unwrap();
    Uuid::parse_str(&receipt.key).unwrap()
}

#[tokio::test]
async fn request_review_uses_live_head_after_sandbox_advances_past_allocation() {
    let f = fixture().await;
    assert_ne!(f.allocation_commit, f.source_head);
    record_db_review_source(&f, "review-advanced-allocation-source").await;

    let receipt = f
        .handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::RequestReview {
                    key: "product".into(),
                    expected_row_version: 2,
                    source_commit: f.source_head.clone(),
                    query: "Review the exact source and submit the DB receipt.".into(),
                    launch: ManagerLaunchChoiceV2 {
                        provider: SessionProvider::Claude,
                        model: "claude-sonnet-5".into(),
                        effort: None,
                    },
                },
                "review-advanced-allocation",
            ),
        )
        .await
        .unwrap();

    let store = f.handle.store.lock().await;
    let source_sha: String = store
        .conn
        .query_row(
            "SELECT source_sha FROM manager_review_assignments WHERE assignment_id=?1",
            [receipt.key.clone()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(source_sha, f.source_head);
    let custody = store.live_custody_for_session(f.source).unwrap();
    assert_eq!(custody.source_commit, f.allocation_commit);
    assert_ne!(custody.source_commit, source_sha);
}

async fn activate_db_review_assignment(f: &Fixture, assignment_id: Uuid) {
    let store = f.handle.store.lock().await;
    let state: String = store
        .conn
        .query_row(
            "SELECT state FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    if state == "reserved" {
        store
            .allocate_manager_review_assignment(assignment_id)
            .unwrap();
    }
    let (action_id, state): (String, String) = store
        .conn
        .query_row(
            "SELECT action_operation_id,state FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "allocating");
    let custody = store.live_custody_for_session(f.reviewer).unwrap();
    store
        .update_session_status(f.reviewer, SessionStatus::Running)
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE model_invocations SET status='running',completed_at=NULL WHERE id=?1",
            [f.invocation.to_string()],
        )
        .unwrap();
    let stamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    store
        .conn
        .execute(
            "UPDATE manager_review_assignments
                SET reviewer_session_id=?2,reviewer_invocation_id=?3,
                    reviewer_custody_id=?4,reviewer_custody_generation=?5,
                    state='active',row_version=row_version+1,updated_at=?6
              WHERE assignment_id=?1 AND state='allocating'",
            params![
                assignment_id.to_string(),
                f.reviewer.to_string(),
                f.invocation.to_string(),
                custody.custody_id.to_string(),
                custody.generation as i64,
                stamp
            ],
        )
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE harness_manager_v2_operations
                SET target_session_id=?2,state='succeeded',row_version=row_version+1,updated_at=?3
              WHERE id=?1",
            params![action_id, f.reviewer.to_string(), stamp],
        )
        .unwrap();
}

#[tokio::test]
async fn db_native_review_accepts_only_the_bound_exact_source_receipt() {
    let f = fixture().await;
    let assignment_id = request_and_activate_db_review(&f, "db-review").await;

    std::fs::write(f.source_root.join("lead-advanced.txt"), "later source\n").unwrap();
    command(&f.source_root, &["add", "lead-advanced.txt"]);
    command(
        &f.source_root,
        &["commit", "-m", "lead advances after assignment"],
    );
    assert_eq!(
        command(&f.review_root, &["rev-parse", "HEAD"]),
        f.source_head
    );

    let request = AgentSubmitReviewReceiptRequestV1 {
        assignment_id,
        verdict: ManagerReviewVerdictV1::Accepted,
        findings: vec![],
        idempotency_key: "receipt-one".into(),
    };
    assert!(
        f.handle
            .agent_submit_review_receipt(f.source, request.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("reviewer_required")
    );
    let receipt = f
        .handle
        .agent_submit_review_receipt(f.reviewer, request.clone())
        .await
        .unwrap();
    assert_eq!(receipt.source_commit, f.source_head);
    assert!(!receipt.deduplicated);
    let replay = f
        .handle
        .agent_submit_review_receipt(f.reviewer, request.clone())
        .await
        .unwrap();
    assert_eq!(replay.receipt_id, receipt.receipt_id);
    assert!(replay.deduplicated);
    let mut changed = request;
    changed.verdict = ManagerReviewVerdictV1::Blocked;
    assert!(
        f.handle
            .agent_submit_review_receipt(f.reviewer, changed)
            .await
            .unwrap_err()
            .to_string()
            .contains("idempotency_conflict")
    );

    {
        let store = f.handle.store.lock().await;
        let (config, _) = store.manager_config_for_caller(f.manager).unwrap();
        let (_, work) = store.manager_v2_work(&config, "product").unwrap();
        assert!(
            store
                .manager_v2_accepted_source(&config, &work, &f.source_head)
                .unwrap()
                .is_none(),
            "a receipt is not admissible until its bound invocation completes"
        );
        assert!(
            store
                .conn
                .execute(
                    "UPDATE manager_review_receipts SET verdict='blocked' WHERE receipt_id=?1",
                    [receipt.receipt_id.to_string()]
                )
                .is_err()
        );
        assert!(
            store
                .conn
                .execute(
                    "DELETE FROM manager_review_receipts WHERE receipt_id=?1",
                    [receipt.receipt_id.to_string()]
                )
                .is_err()
        );
        store
            .update_session_status(f.reviewer, SessionStatus::Completed)
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE model_invocations SET status='completed',completed_at=?2 WHERE id=?1",
                params![
                    f.invocation.to_string(),
                    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
        let (config, _) = store.manager_config_for_caller(f.manager).unwrap();
        let (_, work) = store.manager_v2_work(&config, "product").unwrap();
        assert!(
            store
                .manager_v2_accepted_source(&config, &work, &f.source_head)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .manager_v2_accepted_source(&config, &work, &"f".repeat(40))
                .unwrap()
                .is_none()
        );
        let count: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM manager_review_receipts WHERE assignment_id=?1",
                [assignment_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
    let forbidden = f
        .handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Integration {
                    key: "product".into(),
                    expected_row_version: 2,
                    source_commit: f.source_head.clone(),
                    target_commit: f.source_head.clone(),
                    verification: Some(ManagerEvidenceV2 {
                        source_session_id: f.source,
                        source_commit: f.source_head.clone(),
                        artifact_path: "legacy-review.json".into(),
                        artifact_commit: f.source_head.clone(),
                        closure_evidence_id: None,
                    }),
                },
                "legacy-db-integration",
            ),
        )
        .await
        .unwrap_err();
    assert!(
        forbidden
            .to_string()
            .contains("manager_review_legacy_evidence_forbidden")
    );
    command(
        &f.dir.path().join("repo"),
        &["merge", "--ff-only", &f.source_head],
    );
    let repo = f.dir.path().join("repo");
    command(&repo, &["revert", "--no-edit", &f.source_head]);
    let reverted = command(&repo, &["rev-parse", "HEAD"]);
    let lost = f
        .handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Integration {
                    key: "product".into(),
                    expected_row_version: 2,
                    source_commit: f.source_head.clone(),
                    target_commit: reverted.clone(),
                    verification: None,
                },
                "db-reverted-admission",
            ),
        )
        .await
        .unwrap_err();
    assert!(
        lost.to_string()
            .contains("manager_v2_accepted_content_lost")
    );
    let still_open = f
        .handle
        .agent_manager_inspect(
            f.manager,
            AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Work,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(still_open.rows[0]["source_accepted"], true);
    assert_eq!(still_open.rows[0]["integrated"], false);
    command(&repo, &["revert", "--no-edit", &reverted]);
    let restored = command(&repo, &["rev-parse", "HEAD"]);
    f.handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Integration {
                    key: "product".into(),
                    expected_row_version: 2,
                    source_commit: f.source_head.clone(),
                    target_commit: restored,
                    verification: None,
                },
                "db-integrate",
            ),
        )
        .await
        .unwrap();
    let rows = f
        .handle
        .agent_manager_inspect(
            f.manager,
            AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Work,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .rows;
    assert_eq!(rows[0]["review"]["mode"], "db_native");
    assert_eq!(rows[0]["review"]["current"]["verdict"], "accepted");
    assert_eq!(rows[0]["review"]["current"]["source_commit"], f.source_head);
    assert_eq!(rows[0]["review"]["current"]["eligible"], true);
    assert_eq!(rows[0]["source_accepted"], true);
    assert_eq!(rows[0]["integrated"], true);
    assert!(rows[0]["integration"]["verification"].is_null());
    assert!(rows[0]["integration_evidence_policy_digest"].is_null());
}

#[tokio::test]
async fn db_reviewed_fast_forwarded_source_integrates_after_same_file_evolution() {
    let f = fixture_with_merged_source(true).await;
    let repo = f.dir.path().join("repo");
    let assignment_id = request_and_activate_db_review(&f, "merged-source-review").await;
    f.handle
        .agent_submit_review_receipt(
            f.reviewer,
            AgentSubmitReviewReceiptRequestV1 {
                assignment_id,
                verdict: ManagerReviewVerdictV1::Accepted,
                findings: vec![],
                idempotency_key: "merged-source-accepted".into(),
            },
        )
        .await
        .unwrap();
    {
        let store = f.handle.store.lock().await;
        store
            .update_session_status(f.reviewer, SessionStatus::Completed)
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE model_invocations SET status='completed',completed_at=?2 WHERE id=?1",
                params![
                    f.invocation.to_string(),
                    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
    }

    command(&repo, &["merge", "--ff-only", &f.source_head]);
    std::fs::write(
        repo.join("shared"),
        "top evolved\nmiddle\nbottom\nsource addition\n",
    )
    .unwrap();
    command(&repo, &["commit", "-am", "rolling evolves shared line"]);
    let landed = command(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(
        std::fs::read_to_string(repo.join("shared")).unwrap(),
        "top evolved\nmiddle\nbottom\nsource addition\n"
    );
    f.handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Integration {
                    key: "product".into(),
                    expected_row_version: 2,
                    source_commit: f.source_head.clone(),
                    target_commit: landed,
                    verification: None,
                },
                "merged-source-integrated",
            ),
        )
        .await
        .unwrap();
    let work = f
        .handle
        .agent_manager_inspect(
            f.manager,
            AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Work,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(work.rows[0]["source_accepted"], true);
    assert_eq!(work.rows[0]["integrated"], true);
}

#[tokio::test]
async fn db_reviewed_landing_records_its_exact_target_after_later_remote_landings() {
    for later_landings in 1..=2 {
        let f = fixture().await;
        let repo = f.dir.path().join("repo");
        let remote = f.dir.path().join("origin.git");
        std::fs::create_dir(&remote).unwrap();
        command(&remote, &["init", "--bare"]);
        command(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        let assignment_id = request_and_activate_db_review(&f, "later-remote-landings").await;
        f.handle
            .agent_submit_review_receipt(
                f.reviewer,
                AgentSubmitReviewReceiptRequestV1 {
                    assignment_id,
                    verdict: ManagerReviewVerdictV1::Accepted,
                    findings: vec![],
                    idempotency_key: "later-remote-landings-accepted".into(),
                },
            )
            .await
            .unwrap();
        {
            let store = f.handle.store.lock().await;
            store
                .update_session_status(f.reviewer, SessionStatus::Completed)
                .unwrap();
            store
                .conn
                .execute(
                    "UPDATE model_invocations SET status='completed',completed_at=?2 WHERE id=?1",
                    params![
                        f.invocation.to_string(),
                        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                    ],
                )
                .unwrap();
        }

        command(&repo, &["merge", "--ff-only", &f.source_head]);
        command(&repo, &["push", "origin", "rolling:refs/heads/rolling"]);
        for index in 1..=later_landings {
            let path = format!("later-{index}");
            std::fs::write(repo.join(&path), format!("later landing {index}\n")).unwrap();
            command(&repo, &["add", &path]);
            command(&repo, &["commit", "-m", &path]);
            command(&repo, &["push", "origin", "rolling:refs/heads/rolling"]);
        }
        let current_remote = command(&remote, &["rev-parse", "refs/heads/rolling"]);
        assert_ne!(current_remote, f.source_head);

        f.handle
            .agent_manager_update(
                f.manager,
                req(
                    ManagerUpdateV2::Integration {
                        key: "product".into(),
                        expected_row_version: 2,
                        source_commit: f.source_head.clone(),
                        target_commit: f.source_head.clone(),
                        verification: None,
                    },
                    "record-exact-landing-after-remote-advance",
                ),
            )
            .await
            .unwrap();
        let work = f
            .handle
            .agent_manager_inspect(
                f.manager,
                AgentManagerInspectRequestV2 {
                    section: ManagerInspectSectionV2::Work,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(work.rows[0]["integrated"], true);
        assert_eq!(work.rows[0]["integration"]["target_commit"], f.source_head);
        assert_eq!(
            command(&remote, &["rev-parse", "refs/heads/rolling"]),
            current_remote
        );
    }
}

#[tokio::test]
async fn archived_db_reviewed_source_integrates_from_exact_remote_repository() {
    let f = fixture().await;
    let assignment_id = request_and_activate_db_review(&f, "archive-review").await;
    let before_receipt = f
        .handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Integration {
                    key: "product".into(),
                    expected_row_version: 2,
                    source_commit: f.source_head.clone(),
                    target_commit: f.source_head.clone(),
                    verification: None,
                },
                "archive-before-acceptance",
            ),
        )
        .await
        .unwrap_err();
    assert!(
        before_receipt
            .to_string()
            .contains("source_acceptance_required")
    );

    f.handle
        .agent_submit_review_receipt(
            f.reviewer,
            AgentSubmitReviewReceiptRequestV1 {
                assignment_id,
                verdict: ManagerReviewVerdictV1::Accepted,
                findings: vec![],
                idempotency_key: "archive-accepted-receipt".into(),
            },
        )
        .await
        .unwrap();
    {
        let store = f.handle.store.lock().await;
        store
            .update_session_status(f.reviewer, SessionStatus::Completed)
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE model_invocations SET status='completed',completed_at=?2 WHERE id=?1",
                params![
                    f.invocation.to_string(),
                    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
        store
            .update_session_status(f.source, SessionStatus::Archived)
            .unwrap();
    }

    let stage_after_archive = f
        .handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Stage {
                    key: "product".into(),
                    expected_row_version: 2,
                    stage: ManagerWorkStageV2::Implementation,
                    state: ManagerStageStateV2::Partial,
                    note: "archived source".into(),
                    evidence: Some(ManagerEvidenceV2 {
                        source_session_id: f.source,
                        source_commit: f.source_head.clone(),
                        artifact_path: String::new(),
                        artifact_commit: f.source_head.clone(),
                        closure_evidence_id: None,
                    }),
                },
                "archive-stage-still-denied",
            ),
        )
        .await
        .unwrap_err();
    assert!(
        stage_after_archive
            .to_string()
            .contains("manager_v2_evidence_source_unavailable")
    );

    let root = f.dir.path().join("repo");
    let remote = f.dir.path().join("origin.git");
    std::fs::create_dir(&remote).unwrap();
    command(&remote, &["init", "--bare"]);
    command(
        &root,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    command(
        &root,
        &[
            "push",
            "origin",
            &format!("{}:refs/heads/rolling", f.source_head),
        ],
    );
    let local_before = command(&root, &["rev-parse", "refs/heads/rolling"]);
    let wrong_source = f.allocation_commit.clone();
    let wrong = f
        .handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Integration {
                    key: "product".into(),
                    expected_row_version: 2,
                    source_commit: wrong_source,
                    target_commit: f.source_head.clone(),
                    verification: None,
                },
                "archive-wrong-source",
            ),
        )
        .await
        .unwrap_err();
    assert!(wrong.to_string().contains("evidence_source_unavailable"));

    let landing = f.dir.path().join("landing");
    command(
        &root,
        &[
            "worktree",
            "add",
            "--detach",
            landing.to_str().unwrap(),
            &f.source_head,
        ],
    );
    command(&landing, &["revert", "--no-edit", &f.source_head]);
    let reverted = command(&landing, &["rev-parse", "HEAD"]);
    command(
        &landing,
        &["push", "origin", &format!("{reverted}:refs/heads/rolling")],
    );
    let lost = f
        .handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Integration {
                    key: "product".into(),
                    expected_row_version: 2,
                    source_commit: f.source_head.clone(),
                    target_commit: reverted.clone(),
                    verification: None,
                },
                "archive-reverted-content",
            ),
        )
        .await
        .unwrap_err();
    assert!(
        lost.to_string()
            .contains("manager_v2_accepted_content_lost")
    );
    let open = f
        .handle
        .agent_manager_inspect(
            f.manager,
            AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Work,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(open.rows[0]["source_accepted"], true);
    assert_eq!(open.rows[0]["integrated"], false);

    command(&landing, &["revert", "--no-edit", &reverted]);
    let restored = command(&landing, &["rev-parse", "HEAD"]);
    command(
        &landing,
        &["push", "origin", &format!("{restored}:refs/heads/rolling")],
    );

    f.handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Integration {
                    key: "product".into(),
                    expected_row_version: 2,
                    source_commit: f.source_head.clone(),
                    target_commit: restored,
                    verification: None,
                },
                "archive-remote-integrated",
            ),
        )
        .await
        .unwrap();
    let rows = f
        .handle
        .agent_manager_inspect(
            f.manager,
            AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Work,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .rows;
    assert_eq!(rows[0]["integrated"], true);
    assert_eq!(rows[0]["remote_delivery_freshness"], "current");
    assert_eq!(rows[0]["local_checkout_freshness"], "target_advanced");
    assert_eq!(
        command(&root, &["rev-parse", "refs/heads/rolling"]),
        local_before
    );
}

#[tokio::test]
async fn completed_db_review_receipt_survives_manager_seat_rotation() {
    let f = fixture().await;
    let assignment_id = request_and_activate_db_review(&f, "rotation-review").await;
    let receipt = f
        .handle
        .agent_submit_review_receipt(
            f.reviewer,
            AgentSubmitReviewReceiptRequestV1 {
                assignment_id,
                verdict: ManagerReviewVerdictV1::Accepted,
                findings: vec![],
                idempotency_key: "rotation-receipt".into(),
            },
        )
        .await
        .unwrap();

    let store = f.handle.store.lock().await;
    store
        .update_session_status(f.reviewer, SessionStatus::Completed)
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE model_invocations SET status='completed',completed_at=?2 WHERE id=?1",
            params![
                f.invocation.to_string(),
                Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            ],
        )
        .unwrap();
    let project = store
        .get_session(f.manager)
        .unwrap()
        .unwrap()
        .project_id
        .unwrap();
    let config_a = store.get_harness_manager(project).unwrap().unwrap();
    let (_, work) = store.manager_v2_work(&config_a, "product").unwrap();
    let mut manager_b = store.get_session(f.manager).unwrap().unwrap();
    manager_b.id = Uuid::new_v4();
    store.insert_session(&manager_b).unwrap();
    let config_b = store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: project,
            session_id: manager_b.id,
            epic_ids: Some(vec![f.epic]),
            expected_row_version: config_a.row_version,
        })
        .unwrap();
    assert_eq!(config_b.manager_session_id, manager_b.id);
    assert_eq!(config_b.row_version, config_a.row_version + 1);
    let provenance: (String, i64) = store
        .conn
        .query_row(
            "SELECT manager_session_id,scope_version
               FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(provenance, (f.manager.to_string(), config_a.row_version));

    let review = store.manager_review_projection(&config_b, &work).unwrap();
    assert_eq!(
        review["current"]["receipt_id"],
        receipt.receipt_id.to_string()
    );
    assert_eq!(review["current"]["source_commit"], f.source_head);
    assert_eq!(review["current"]["verdict"], "accepted");
    let acceptance = store
        .manager_v2_accepted_source(&config_b, &work, &f.source_head)
        .unwrap()
        .expect("completed receipt remains accepted after manager rotation");
    assert_eq!(acceptance.source_commit, f.source_head);
}

#[tokio::test]
async fn manager_seat_rotation_preserves_the_durable_review_assignment_budget() {
    let f = fixture().await;
    record_db_review_source(&f, "budget-review").await;
    {
        let store = f.handle.store.lock().await;
        let project = store
            .get_session(f.manager)
            .unwrap()
            .unwrap()
            .project_id
            .unwrap();
        let config = store.get_harness_manager(project).unwrap().unwrap();
        let (_, work) = store.manager_v2_work(&config, "product").unwrap();
        // #599 A1: three submitted rounds with receipts spend the budget; the
        // reviewer references are synthetic, so foreign keys relax for the
        // seed only.
        store.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        for ordinal in 1..=3 {
            let assignment = Uuid::new_v4().to_string();
            let stamp = || Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            let created = stamp();
            let request = serde_json::json!({
                "requester_session_id": config.manager_session_id,
                "fence": {"scope_version": config.row_version, "policy_version": 1},
                "query": "Historical review round.",
                "launch": {"provider": "Claude", "model": format!("claude-sonnet-{ordinal}"),
                           "effort": null},
            });
            store
                .conn
                .execute(
                    "INSERT INTO manager_review_assignments(
                        assignment_id,project_id,epic_id,manager_session_id,scope_version,
                        work_key,spec_revision,author_session_id,source_sha,reviewer_session_id,
                        reviewer_invocation_id,reviewer_custody_id,reviewer_custody_generation,
                        action_operation_id,state,row_version,request_json,request_fingerprint,
                        created_at,updated_at)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,1,?13,'active',1,?14,?15,
                            ?16,?16)",
                    params![
                        assignment,
                        project.to_string(),
                        work.epic_id.to_string(),
                        config.manager_session_id.to_string(),
                        config.row_version,
                        work.key,
                        work.spec_revision,
                        f.source.to_string(),
                        format!("{ordinal:040x}"),
                        Uuid::new_v4().to_string(),
                        Uuid::new_v4().to_string(),
                        Uuid::new_v4().to_string(),
                        Uuid::new_v4().to_string(),
                        request.to_string(),
                        format!("sha256:{}", "a".repeat(64)),
                        created
                    ],
                )
                .unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO manager_review_receipts(
                         receipt_id,assignment_id,source_sha,reviewer_session_id,
                         reviewer_invocation_id,reviewer_custody_id,reviewer_custody_generation,
                         verdict,idempotency_key,request_fingerprint,created_at)
                     SELECT ?1,assignment_id,source_sha,reviewer_session_id,
                            reviewer_invocation_id,reviewer_custody_id,
                            reviewer_custody_generation,'changes_requested','verdict',
                            request_fingerprint,?2
                       FROM manager_review_assignments WHERE assignment_id=?3",
                    params![Uuid::new_v4().to_string(), stamp(), assignment],
                )
                .unwrap();
            store
                .conn
                .execute(
                    "UPDATE manager_review_assignments
                        SET state='submitted',row_version=2,updated_at=?2,terminal_at=?2
                      WHERE assignment_id=?1",
                    params![assignment, stamp()],
                )
                .unwrap();
        }
        store.conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    }
    let (manager_b, scope_version, policy_version) = rotate_db_review_manager(&f).await;
    let review_request = |key: &str| {
        fenced_req(
            ManagerUpdateV2::RequestReview {
                key: "product".into(),
                expected_row_version: 2,
                source_commit: f.source_head.clone(),
                query: "Review the exact source without resetting durable history.".into(),
                launch: ManagerLaunchChoiceV2 {
                    provider: SessionProvider::Claude,
                    model: "claude-sonnet-5".into(),
                    effort: None,
                },
            },
            key,
            scope_version,
            policy_version,
        )
    };
    let error = f
        .handle
        .agent_manager_update(manager_b, review_request("rotated-budget-review"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("manager_review_round_budget"));
    let store = f.handle.store.lock().await;
    let durable_count: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM manager_review_assignments
              WHERE project_id=(SELECT project_id FROM sessions WHERE id=?1)
                AND epic_id=?2 AND work_key='product' AND spec_revision=1",
            params![manager_b.to_string(), f.epic.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(durable_count, 3);
}

/// #599 test 6: an infra retry re-reviews the sealed commit object after
/// dirty-tree edits, notes commits, and later code commits.
#[tokio::test]
async fn infra_retry_tolerates_dirty_tree_and_post_seal_code_commit() {
    let f = fixture().await;
    let assignment = request_and_activate_db_review(&f, "retry-dirty-author").await;
    let note = f.source_root.join("thoughts/shared/notes/retry-dirty.md");
    std::fs::create_dir_all(note.parent().unwrap()).unwrap();
    std::fs::write(&note, "review handoff\n").unwrap();
    command(
        &f.source_root,
        &["add", "thoughts/shared/notes/retry-dirty.md"],
    );
    command(&f.source_root, &["commit", "-m", "notes: retry handoff"]);
    std::fs::write(f.source_root.join("code.txt"), "uncommitted edit\n").unwrap();
    std::fs::write(f.source_root.join("scratch.txt"), "untracked\n").unwrap();
    assert!(!command(&f.source_root, &["status", "--porcelain"]).is_empty());

    let store = f.handle.store.lock().await;
    let action: String = store
        .conn
        .query_row(
            "SELECT action_operation_id FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment.to_string()],
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
    assert!(store.refresh_manager_review_assignment(assignment).unwrap());
    let (state, next, next_state, next_sha): (String, String, String, String) = store
        .conn
        .query_row(
            "SELECT a.state,n.assignment_id,n.state,n.source_sha
               FROM manager_review_assignments a
               JOIN manager_review_assignments n
                 ON n.assignment_id=a.superseded_by_assignment_id
              WHERE a.assignment_id=?1",
            [assignment.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (state.as_str(), next_state.as_str()),
        ("superseded", "reserved")
    );
    assert_eq!(next_sha, f.source_head);
    let next = Uuid::parse_str(&next).unwrap();
    assert!(store.allocate_manager_review_assignment(next).unwrap());
    drop(store);

    command(&f.source_root, &["commit", "-am", "code: post-seal fix"]);
    let store = f.handle.store.lock().await;
    let action: String = store
        .conn
        .query_row(
            "SELECT action_operation_id FROM manager_review_assignments WHERE assignment_id=?1",
            [next.to_string()],
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
    assert!(store.refresh_manager_review_assignment(next).unwrap());
    let (state, retry, retry_state, retry_sha): (String, String, String, String) = store
        .conn
        .query_row(
            "SELECT a.state,n.assignment_id,n.state,n.source_sha
               FROM manager_review_assignments a
               JOIN manager_review_assignments n
                 ON n.assignment_id=a.superseded_by_assignment_id
              WHERE a.assignment_id=?1",
            [next.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (state.as_str(), retry_state.as_str()),
        ("superseded", "reserved")
    );
    assert_eq!(retry_sha, f.source_head);
    assert!(Uuid::parse_str(&retry).is_ok());
}

#[tokio::test]
async fn infra_retry_reserves_when_untracked_status_exceeds_bounded_output() {
    let f = fixture().await;
    let assignment = request_and_activate_db_review(&f, "retry-large-status").await;
    let scratch = f.source_root.join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    for index in 0..2_000 {
        std::fs::write(
            scratch.join(format!("{index:04}-{}", "x".repeat(32))),
            b"scratch",
        )
        .unwrap();
    }
    let status = command(
        &f.source_root,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    );
    assert!(
        status.len() > 64 * 1024,
        "status output was {} bytes",
        status.len()
    );

    let store = f.handle.store.lock().await;
    let action: String = store
        .conn
        .query_row(
            "SELECT action_operation_id FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment.to_string()],
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
    assert!(store.refresh_manager_review_assignment(assignment).unwrap());
    let (state, next_state, next_sha): (String, String, String) = store
        .conn
        .query_row(
            "SELECT a.state,n.state,n.source_sha
               FROM manager_review_assignments a
               JOIN manager_review_assignments n
                 ON n.assignment_id=a.superseded_by_assignment_id
              WHERE a.assignment_id=?1",
            [assignment.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (state.as_str(), next_state.as_str()),
        ("superseded", "reserved")
    );
    assert_eq!(next_sha, f.source_head);
}

#[tokio::test]
async fn rotated_manager_supersedes_active_review_and_old_reviewer_loses_authority() {
    let f = fixture().await;
    let first = request_and_activate_db_review(&f, "rotation-active-review").await;
    let (manager_b, scope_version, policy_version) = rotate_db_review_manager(&f).await;
    let second = f
        .handle
        .agent_manager_update(
            manager_b,
            fenced_req(
                ManagerUpdateV2::RequestReview {
                    key: "product".into(),
                    expected_row_version: 2,
                    source_commit: f.source_head.clone(),
                    query: "Reassign the exact source under the current manager seat.".into(),
                    launch: ManagerLaunchChoiceV2 {
                        provider: SessionProvider::OpenRouter,
                        model: "z-ai/glm-5.3-flashx".into(),
                        effort: None,
                    },
                },
                "rotation-reassigned-review",
                scope_version,
                policy_version,
            ),
        )
        .await
        .unwrap();
    let second = Uuid::parse_str(&second.key).unwrap();
    let store = f.handle.store.lock().await;
    let first_history: (String, Option<String>) = store
        .conn
        .query_row(
            "SELECT state,superseded_by_assignment_id
               FROM manager_review_assignments WHERE assignment_id=?1",
            [first.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        first_history,
        ("superseded".into(), Some(second.to_string()))
    );
    let current: (String, String, i64, i64) = store
        .conn
        .query_row(
            "SELECT assignment_id,manager_session_id,scope_version,
                    json_extract(request_json,'$.fence.policy_version')
               FROM manager_review_assignments
              WHERE project_id=(SELECT project_id FROM sessions WHERE id=?1)
                AND epic_id=?2 AND work_key='product' AND spec_revision=1
                AND source_sha=?3 AND superseded_by_assignment_id IS NULL",
            params![manager_b.to_string(), f.epic.to_string(), f.source_head],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        current,
        (
            second.to_string(),
            manager_b.to_string(),
            scope_version,
            policy_version
        )
    );
    drop(store);
    let error = f
        .handle
        .agent_submit_review_receipt(
            f.reviewer,
            AgentSubmitReviewReceiptRequestV1 {
                assignment_id: first,
                verdict: ManagerReviewVerdictV1::Accepted,
                findings: vec![],
                idempotency_key: "revoked-reviewer-receipt".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("manager_review_scope_changed"));
}

#[tokio::test]
async fn db_review_restart_failpoints_preserve_one_assignment_receipt_and_effect() {
    use crate::store::manager_reviews::{ManagerReviewFault, manager_review_fail_next};

    let f = fixture().await;
    record_db_review_source(&f, "restart-review").await;
    let review_request = || {
        req(
            ManagerUpdateV2::RequestReview {
                key: "product".into(),
                expected_row_version: 2,
                source_commit: f.source_head.clone(),
                query: "Review the exact source after durable recovery.".into(),
                launch: ManagerLaunchChoiceV2 {
                    provider: SessionProvider::Claude,
                    model: "claude-sonnet-5".into(),
                    effort: None,
                },
            },
            "restart-review",
        )
    };

    manager_review_fail_next(ManagerReviewFault::AfterReservation);
    assert!(
        f.handle
            .agent_manager_update(f.manager, review_request())
            .await
            .unwrap_err()
            .to_string()
            .contains("AfterReservation")
    );
    {
        let store = f.handle.store.lock().await;
        let assignments: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM manager_review_assignments",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(assignments, 0);
    }

    manager_review_fail_next(ManagerReviewFault::AfterAllocationJournal);
    let reservation = f
        .handle
        .agent_manager_update(f.manager, review_request())
        .await
        .unwrap();
    let assignment_id = Uuid::parse_str(&reservation.key).unwrap();
    {
        let store = f.handle.store.lock().await;
        let (state, effects): (String, i64) = store
            .conn
            .query_row(
                "SELECT a.state,(SELECT count(*) FROM harness_manager_v2_operations o
                   WHERE json_extract(o.payload_json,'$.request.idempotency_key')=?2)
                   FROM manager_review_assignments a WHERE a.assignment_id=?1",
                params![
                    assignment_id.to_string(),
                    format!("manager-review-allocation:{assignment_id}")
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "reserved");
        assert_eq!(effects, 0);
        drop(store);
        std::fs::write(
            f.source_root.join("post-reservation.txt"),
            "lead advanced\n",
        )
        .unwrap();
        let source_session = f
            .handle
            .store
            .lock()
            .await
            .get_session(f.source)
            .unwrap()
            .unwrap();
        let historical = f
            .sessions
            .custody_execution_runtime()
            .prepare_manager_action_fork_at(&source_session, &f.source_head)
            .await
            .unwrap();
        assert_eq!(historical.fork_commit(), f.source_head);
        command(&f.source_root, &["add", "post-reservation.txt"]);
        command(
            &f.source_root,
            &["commit", "-m", "advance after durable reservation"],
        );
        let store = f.handle.store.lock().await;
        assert!(
            store
                .allocate_manager_review_assignment(assignment_id)
                .unwrap()
        );
        assert!(
            !store
                .allocate_manager_review_assignment(assignment_id)
                .unwrap()
        );
    }
    activate_db_review_assignment(&f, assignment_id).await;

    let receipt_request = AgentSubmitReviewReceiptRequestV1 {
        assignment_id,
        verdict: ManagerReviewVerdictV1::Accepted,
        findings: vec![],
        idempotency_key: "restart-receipt".into(),
    };
    manager_review_fail_next(ManagerReviewFault::BeforeReceiptCommit);
    assert!(
        f.handle
            .agent_submit_review_receipt(f.reviewer, receipt_request.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("BeforeReceiptCommit")
    );
    {
        let store = f.handle.store.lock().await;
        let (state, receipts): (String, i64) = store
            .conn
            .query_row(
                "SELECT a.state,(SELECT count(*) FROM manager_review_receipts r
                   WHERE r.assignment_id=a.assignment_id)
                   FROM manager_review_assignments a WHERE a.assignment_id=?1",
                [assignment_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "active");
        assert_eq!(receipts, 0);
    }
    f.handle
        .agent_submit_review_receipt(f.reviewer, receipt_request)
        .await
        .unwrap();
    {
        let store = f.handle.store.lock().await;
        store
            .update_session_status(f.reviewer, SessionStatus::Completed)
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE model_invocations SET status='completed',completed_at=?2 WHERE id=?1",
                params![
                    f.invocation.to_string(),
                    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
    }

    // A newly opened Store models daemon restart after reviewer termination.
    let restarted = Store::open(&f.dir.path().join("rsi.db")).unwrap();
    let (config, _) = restarted.manager_config_for_caller(f.manager).unwrap();
    let (_, work) = restarted.manager_v2_work(&config, "product").unwrap();
    manager_review_fail_next(ManagerReviewFault::BeforeAdmissionRead);
    assert!(
        restarted
            .manager_v2_accepted_source(&config, &work, &f.source_head)
            .unwrap_err()
            .to_string()
            .contains("BeforeAdmissionRead")
    );
    manager_review_fail_next(ManagerReviewFault::AfterReviewerTerminalObservation);
    assert!(
        restarted
            .manager_v2_accepted_source(&config, &work, &f.source_head)
            .unwrap_err()
            .to_string()
            .contains("AfterReviewerTerminalObservation")
    );
    assert!(
        restarted
            .manager_v2_accepted_source(&config, &work, &f.source_head)
            .unwrap()
            .is_some()
    );
    let (assignments, receipts, effects): (i64, i64, i64) = restarted
        .conn
        .query_row(
            "SELECT
                (SELECT count(*) FROM manager_review_assignments),
                (SELECT count(*) FROM manager_review_receipts),
                (SELECT count(*) FROM harness_manager_v2_operations o
                  WHERE json_extract(o.payload_json,'$.request.idempotency_key')=?1)",
            [format!("manager-review-allocation:{assignment_id}")],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!((assignments, receipts, effects), (1, 1, 1));
}

#[tokio::test]
async fn db_review_enrollment_blocks_legacy_override_and_retains_superseded_history() {
    let f = fixture().await;
    let first = request_and_activate_db_review(&f, "blocked-review").await;
    f.handle
        .agent_submit_review_receipt(
            f.reviewer,
            AgentSubmitReviewReceiptRequestV1 {
                assignment_id: first,
                verdict: ManagerReviewVerdictV1::Blocked,
                findings: vec![ManagerReviewFindingV1 {
                    key: "F-001".into(),
                    severity: ManagerReviewFindingSeverityV1::Error,
                    summary: "The exact source is not admissible.".into(),
                    location: Some("code.txt".into()),
                    blocking: true,
                }],
                idempotency_key: "blocked-receipt".into(),
            },
        )
        .await
        .unwrap();
    {
        let store = f.handle.store.lock().await;
        store
            .update_session_status(f.reviewer, SessionStatus::Completed)
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE model_invocations SET status='completed',completed_at=?2 WHERE id=?1",
                params![
                    f.invocation.to_string(),
                    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
        let (config, _) = store.manager_config_for_caller(f.manager).unwrap();
        let (_, work) = store.manager_v2_work(&config, "product").unwrap();
        assert!(
            store
                .manager_v2_accepted_source(&config, &work, &f.source_head)
                .unwrap()
                .is_none()
        );
    }

    let second = f
        .handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::RequestReview {
                    key: "product".into(),
                    expected_row_version: 2,
                    source_commit: f.source_head.clone(),
                    query: "Re-review the same exact source.".into(),
                    launch: ManagerLaunchChoiceV2 {
                        provider: SessionProvider::OpenRouter,
                        model: "z-ai/glm-5.3-flashx".into(),
                        effort: None,
                    },
                },
                "review-again",
            ),
        )
        .await
        .unwrap();
    let second = Uuid::parse_str(&second.key).unwrap();
    assert_ne!(first, second);
    let store = f.handle.store.lock().await;
    let history: Vec<(String, String, Option<String>)> = store
        .conn
        .prepare(
            "SELECT assignment_id,state,superseded_by_assignment_id
               FROM manager_review_assignments WHERE work_key='product' ORDER BY created_at",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(
        history[0],
        (
            first.to_string(),
            "superseded".into(),
            Some(second.to_string())
        )
    );
    assert_eq!(history[1].0, second.to_string());
    assert!(matches!(history[1].1.as_str(), "reserved" | "allocating"));
    let (config, _) = store.manager_config_for_caller(f.manager).unwrap();
    let (record, mut work) = store.manager_v2_work(&config, "product").unwrap();
    work.acceptance = Some(Acceptance {
        source_commit: f.source_head.clone(),
        spec_revision: work.spec_revision,
        evidence_digest: format!("sha256:{}", "a".repeat(64)),
        method: "legacy_fixture".into(),
        accepted_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
    });
    let transaction =
        rusqlite::Transaction::new_unchecked(&store.conn, rusqlite::TransactionBehavior::Immediate)
            .unwrap();
    store
        .manager_v2_put_record(
            &config,
            "work",
            "product",
            Some(work.epic_id),
            record.row_version,
            &serde_json::to_value(&work).unwrap(),
        )
        .unwrap();
    transaction.commit().unwrap();
    assert!(
        store
            .manager_v2_accepted_source(&config, &work, &f.source_head)
            .unwrap()
            .is_none(),
        "current pending DB assignment must dominate legacy acceptance"
    );
}

#[test]
fn review_manifest_path_accepts_canonical_closure_paths_and_preserves_legacy_paths() {
    assert_eq!(
        review_manifest_path(
            "thoughts/shared/reviews/closure/lifecycle-action-clarity-v1/source-review-v1.json"
        )
        .unwrap(),
        "thoughts/shared/verification/closure/lifecycle-action-clarity-v1/source-manifest-v2.md"
    );
    assert_eq!(
        review_manifest_path("thoughts/shared/reviews/manager-v2/product.json").unwrap(),
        "thoughts/shared/reviews/manager-v2/product.manifest.md"
    );
    assert!(
        review_manifest_path(
            "thoughts/shared/reviews/closure/lifecycle-action-clarity-v1/product.json"
        )
        .is_err()
    );
}

async fn evidence(f: &Fixture, head: &str, integration: bool) -> ManagerEvidenceV2 {
    let store = f.handle.store.lock().await;
    let (config, _) = store.manager_config_for_caller(f.manager).unwrap();
    let (_, mut work) = store.manager_v2_work(&config, "product").unwrap();
    drop(store);
    work.source_commit = Some(f.source_head.clone());
    work.source_session_id = Some(f.source);
    let path = "thoughts/shared/reviews/closure/manager-v2/product-review-v1.json";
    let manifest_path = "thoughts/shared/verification/closure/manager-v2/product-manifest-v2.md";
    std::fs::create_dir_all(
        f.review_root
            .join("thoughts/shared/reviews/closure/manager-v2"),
    )
    .unwrap();
    std::fs::create_dir_all(
        f.review_root
            .join("thoughts/shared/verification/closure/manager-v2"),
    )
    .unwrap();
    let review = json!({"schema_version":1,"reviewed_source_head":head,"reviewer":{"session_id":f.reviewer,"model_invocation_id":f.invocation},"verdict":"accepted","findings":[],"unresolved_finding_ids":[],"scope_policy_audit":{"reviewed_scope":[scope_label(&work)],"scope_result":"pass","policy_result":"pass","review_policy_digest":if integration {integration_policy_digest(&work,head).unwrap()}else{policy_digest(&work,head).unwrap()}}});
    std::fs::write(
        f.review_root.join(path),
        serde_json::to_vec(&review).unwrap(),
    )
    .unwrap();
    let mut items = String::new();
    for stage in ["implementation", "review", "verification"] {
        items += &format!(
            "- [PASS] {stage} test evidence\n  satisfies: {}:{stage}\n",
            scope_label(&work)
        );
    }
    if integration {
        items += &format!(
            "- [PASS] combined integration checks\n  satisfies: {}:integration\n",
            scope_label(&work)
        );
    }
    let manifest = format!(
        "---\nschema_version: 2\nsource_head: {head}\nticket: manager-v2\nplan_doc: thoughts/shared/plans/manager-v2.md\ngenerated: 2026-09-07T12:00:00Z\nphases_sealed: [1]\nstatus: verified\n---\n\n# Verification Manifest\n\n## Phase 1 - Product\n\n### Automated\n{items}\n### Daemon-level\n- (none)\n\n### TUI manual\n- (none)\n"
    );
    std::fs::write(f.review_root.join(manifest_path), manifest).unwrap();
    command(&f.review_root, &["add", path, manifest_path]);
    command(&f.review_root, &["commit", "-m", "independent review"]);
    let artifact = command(&f.review_root, &["rev-parse", "HEAD"]);
    let handoff = format!(
        "PIPELINE HANDOFF — REVIEW:\nreviewer_session_id: {}\nreviewer_model_invocation_id: {}\nreview_json_path: {path}\nmanifest_v2_path: {manifest_path}\nsealed_source_sha: {head}\nevidence_commit_sha: {artifact}\n\n## Stage contract\n### Inputs\nStatic input: `{path}`\n### Process\nIndependent review\n### Outputs\nReview and manifest\n### Verify\nExact source checked\n",
        f.reviewer, f.invocation
    );
    let store = f.handle.store.lock().await;
    store.conn.execute("INSERT INTO conversation_events(session_id,sequence,role,event_type,content,created_at) VALUES(?1,1,'Assistant','Message',?2,?3)",params![f.reviewer.to_string(),handoff,Utc::now().to_rfc3339()]).unwrap();
    let id = store.conn.last_insert_rowid();
    store.conn.execute("INSERT INTO conversation_event_provenance(conversation_event_id,model_invocation_id,producer_kind,provider_event_type,created_at) VALUES(?1,?2,'provider_assistant_output','assistant','2026-09-07T12:00:00.000000000Z')",params![id,f.invocation.to_string()]).unwrap();
    ManagerEvidenceV2 {
        source_session_id: f.source,
        source_commit: head.into(),
        artifact_path: path.into(),
        artifact_commit: artifact,
        closure_evidence_id: None,
    }
}
#[tokio::test]
async fn ordinary_work_accepts_only_correlated_independent_committed_evidence() {
    let f = fixture().await;
    let evidence = evidence(&f, &f.source_head, false).await;
    for (i, stage) in [
        ManagerWorkStageV2::Implementation,
        ManagerWorkStageV2::Review,
        ManagerWorkStageV2::Verification,
    ]
    .into_iter()
    .enumerate()
    {
        f.handle
            .agent_manager_update(
                f.manager,
                req(
                    ManagerUpdateV2::Stage {
                        key: "product".into(),
                        expected_row_version: i as i64 + 1,
                        stage,
                        state: ManagerStageStateV2::Passed,
                        note: "independent proof".into(),
                        evidence: Some(evidence.clone()),
                    },
                    &format!("stage-{i}"),
                ),
            )
            .await
            .unwrap();
    }
    f.handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Accept {
                    key: "product".into(),
                    expected_row_version: 4,
                },
                "accept",
            ),
        )
        .await
        .unwrap();
    let work = f
        .handle
        .agent_manager_inspect(
            f.manager,
            AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Work,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(work.rows[0]["source_accepted"], true);
    assert_eq!(work.rows[0]["integrated"], false);
    assert_eq!(work.rows[0]["title"], "Product");
    // Exact integration target must actually contain the accepted source.
    let target = command(&f.dir.path().join("repo"), &["rev-parse", "rolling"]);
    let error = f
        .handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Integration {
                    key: "product".into(),
                    expected_row_version: 5,
                    source_commit: f.source_head.clone(),
                    target_commit: target.clone(),
                    verification: Some(ManagerEvidenceV2 {
                        source_commit: target,
                        ..evidence
                    }),
                },
                "fake-integration",
            ),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("ancestry_mismatch"));
}
#[tokio::test]
async fn read_only_git_rejects_changed_branch_dirty_source_and_wrong_inventory() {
    let f = fixture().await;
    assert!(git::check_custody(&f.source_root, "wrong").await.is_err());
    std::fs::write(f.source_root.join("uncommitted"), "work").unwrap();
    assert!(git::clean(&f.source_root).await.is_err());
    assert!(
        git::migration(&f.source_root, &f.source_head, "sha256:wrong")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn independent_acceptance_and_combined_evidence_record_real_integration() {
    let f = fixture().await;
    let stage_evidence = evidence(&f, &f.source_head, false).await;
    for (expected_row_version, stage) in [
        (1_i64, ManagerWorkStageV2::Implementation),
        (2, ManagerWorkStageV2::Review),
        (3, ManagerWorkStageV2::Verification),
    ]
    .into_iter()
    {
        f.handle
            .agent_manager_update(
                f.manager,
                req(
                    ManagerUpdateV2::Stage {
                        key: "product".into(),
                        expected_row_version,
                        stage,
                        state: ManagerStageStateV2::Passed,
                        note: "independent proof".into(),
                        evidence: Some(stage_evidence.clone()),
                    },
                    &format!("stage-{}", expected_row_version - 1),
                ),
            )
            .await
            .unwrap();
    }
    f.handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Accept {
                    key: "product".into(),
                    expected_row_version: 4,
                },
                "accept-independent-evidence",
            ),
        )
        .await
        .unwrap();
    // Delivery advances the bare origin while the shared checkout stays at base.
    let root = f.dir.path().join("repo");
    let remote = f.dir.path().join("origin.git");
    std::fs::create_dir(&remote).unwrap();
    command(&remote, &["init", "--bare"]);
    command(
        &root,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    command(
        &root,
        &[
            "push",
            "origin",
            &format!("{}:refs/heads/rolling", f.source_head),
        ],
    );
    let local_before = command(&root, &["rev-parse", "refs/heads/rolling"]);
    std::fs::write(
        f.source_root.join("next-product.txt"),
        "another work item\n",
    )
    .unwrap();
    command(&f.source_root, &["add", "next-product.txt"]);
    command(&f.source_root, &["commit", "-m", "next product source"]);
    command(&f.review_root, &["reset", "--hard", &f.source_head]);
    let proof = evidence(&f, &f.source_head, true).await;
    let request = req(
        ManagerUpdateV2::Integration {
            key: "product".into(),
            expected_row_version: 5,
            source_commit: f.source_head.clone(),
            target_commit: f.source_head.clone(),
            verification: Some(proof),
        },
        "integrate",
    );
    let first = f
        .handle
        .agent_manager_update(f.manager, request.clone())
        .await
        .unwrap();
    let replay = f
        .handle
        .agent_manager_update(f.manager, request.clone())
        .await
        .unwrap();
    assert_eq!(first.event_sequence, replay.event_sequence);
    let query = AgentManagerInspectRequestV2 {
        section: ManagerInspectSectionV2::Work,
        ..Default::default()
    };
    let result = f
        .handle
        .agent_manager_inspect(f.manager, query.clone())
        .await
        .unwrap();
    assert_eq!(result.rows[0]["integrated"], true);
    assert_eq!(result.rows[0]["source_freshness"], "head_advanced");
    assert_eq!(
        result.rows[0]["local_checkout_freshness"],
        "target_advanced"
    );
    assert_eq!(result.rows[0]["remote_delivery_freshness"], "current");
    assert_eq!(
        command(&root, &["rev-parse", "refs/heads/rolling"]),
        local_before
    );
    assert_eq!(
        result.rows[0]["integration_evidence_policy_digest"],
        result.rows[0]["integration"]["verification"]["policy_digest"]
    );
    std::fs::write(root.join("next.txt"), "next rolling change\n").unwrap();
    command(&root, &["add", "next.txt"]);
    command(&root, &["commit", "-m", "rolling advanced"]);
    let diverged = command(&root, &["rev-parse", "HEAD"]);
    let source_advanced = command(&f.source_root, &["rev-parse", "HEAD"]);
    command(
        &root,
        &[
            "push",
            "origin",
            &format!("{source_advanced}:refs/heads/rolling"),
        ],
    );
    let advanced = f
        .handle
        .agent_manager_inspect(f.manager, query.clone())
        .await
        .unwrap();
    assert_eq!(advanced.rows[0]["integrated"], true);
    assert_eq!(advanced.rows[0]["target_freshness"], "target_advanced");
    assert_eq!(
        advanced.rows[0]["remote_delivery_freshness"],
        "target_advanced"
    );
    command(
        &root,
        &["push", "origin", &format!("{diverged}:refs/rsi/test")],
    );
    command(&remote, &["update-ref", "refs/heads/rolling", &diverged]);
    let rewound = f
        .handle
        .agent_manager_inspect(f.manager, query)
        .await
        .unwrap();
    assert_eq!(
        rewound.rows[0]["remote_delivery_freshness"],
        "target_rewound_or_diverged"
    );
    let mut stale = request;
    stale.idempotency_key = "stale-target".into();
    let ManagerUpdateV2::Integration {
        target_commit,
        verification,
        ..
    } = &mut stale.change
    else {
        unreachable!();
    };
    *target_commit = diverged.clone();
    verification.as_mut().unwrap().source_commit = diverged;
    let error = f
        .handle
        .agent_manager_update(f.manager, stale)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_remote_ancestry_mismatch")
    );
}

#[tokio::test]
async fn canonical_migration_inventory_requires_every_pin_and_unchanged_released_code() {
    let f = fixture().await;
    let root = f.dir.path().join("repo");
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let inventory_bytes = std::fs::read(repo.join("tools/released-migrations.json")).unwrap();
    let inventory: Value = serde_json::from_slice(&inventory_bytes).unwrap();
    let mut paths = std::collections::BTreeSet::from([
        "tools/released-migrations.json".to_string(),
        "crates/rsid/src/store/mod.rs".to_string(),
    ]);
    for pin in inventory["protected_sections"]
        .as_object()
        .unwrap()
        .values()
    {
        paths.insert(pin["path"].as_str().unwrap().into());
    }
    for path in &paths {
        std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
        std::fs::copy(repo.join(path), root.join(path)).unwrap();
        command(&root, &["add", path]);
    }
    command(&root, &["commit", "-m", "canonical migration catalog"]);
    let baseline = command(&root, &["rev-parse", "HEAD"]);
    let digest = format!("sha256:{:x}", Sha256::digest(&inventory_bytes));
    git::migration(&f.source_root, &baseline, &digest)
        .await
        .unwrap();
    let path = "crates/rsid/src/store/mod.rs";
    let old = std::fs::read_to_string(root.join(path)).unwrap();
    std::fs::write(
        root.join(path),
        old.replace("// V0: Original schema", "// V0: Original schema changed"),
    )
    .unwrap();
    command(&root, &["commit", "-am", "invalid released edit"]);
    let changed = command(&root, &["rev-parse", "HEAD"]);
    assert!(
        git::migration(&f.source_root, &changed, &digest)
            .await
            .unwrap_err()
            .to_string()
            .contains("released_source_changed")
    );
    let mut incomplete = inventory;
    incomplete["blocks"].as_object_mut().unwrap().remove("104");
    let incomplete = serde_json::to_vec(&incomplete).unwrap();
    std::fs::write(root.join("tools/released-migrations.json"), &incomplete).unwrap();
    command(&root, &["commit", "-am", "invalid missing pin"]);
    let changed = command(&root, &["rev-parse", "HEAD"]);
    let digest = format!("sha256:{:x}", Sha256::digest(&incomplete));
    assert!(
        git::migration(&f.source_root, &changed, &digest)
            .await
            .unwrap_err()
            .to_string()
            .contains("released_inventory_changed")
    );
}

#[tokio::test]
async fn a_source_moved_out_of_scope_has_unknown_freshness_without_foreign_observation() {
    let f = fixture().await;
    f.handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Stage {
                    key: "product".into(),
                    expected_row_version: 1,
                    stage: ManagerWorkStageV2::Implementation,
                    state: ManagerStageStateV2::Partial,
                    note: "Committed partial product".into(),
                    evidence: Some(ManagerEvidenceV2 {
                        source_session_id: f.source,
                        source_commit: f.source_head.clone(),
                        artifact_path: String::new(),
                        artifact_commit: f.source_head.clone(),
                        closure_evidence_id: None,
                    }),
                },
                "source",
            ),
        )
        .await
        .unwrap();
    {
        let store = f.handle.store.lock().await;
        let mut other = store.get_session(f.epic).unwrap().unwrap();
        other.id = Uuid::new_v4();
        other.lead_session_id = None;
        store.insert_session(&other).unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET parent_id=?2 WHERE id=?1",
                params![f.source.to_string(), other.id.to_string()],
            )
            .unwrap();
    }
    let result = f
        .handle
        .agent_manager_inspect(
            f.manager,
            AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Work,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.rows[0]["source_freshness"], "unknown");
    assert_eq!(result.rows[0]["source_tree_state"], "unknown");
    assert_eq!(result.rows[0]["target_freshness"], "unknown");
    assert_eq!(result.rows[0]["source_commit"], f.source_head);
}

#[tokio::test]
async fn evidence_reads_ignore_local_commit_replacement_refs() {
    let f = fixture().await;
    let original = command(
        &f.source_root,
        &["--no-replace-objects", "cat-file", "commit", &f.source_head],
    );
    let base = command(&f.dir.path().join("repo"), &["rev-parse", "rolling"]);
    command(&f.source_root, &["replace", &f.source_head, &base]);
    let observed = git::read(&f.source_root, &["cat-file", "commit", &f.source_head])
        .await
        .unwrap();
    assert_eq!(String::from_utf8(observed).unwrap().trim(), original);
}

#[tokio::test]
#[allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::significant_drop_tightening,
    clippy::cast_possible_wrap
)]
async fn accepted_work_survives_seat_move_and_successor_integrates_it() {
    let f = fixture().await;
    let proof = evidence(&f, &f.source_head, false).await;
    for (i, stage) in [
        ManagerWorkStageV2::Implementation,
        ManagerWorkStageV2::Review,
        ManagerWorkStageV2::Verification,
    ]
    .into_iter()
    .enumerate()
    {
        f.handle
            .agent_manager_update(
                f.manager,
                req(
                    ManagerUpdateV2::Stage {
                        key: "product".into(),
                        expected_row_version: i as i64 + 1,
                        stage,
                        state: ManagerStageStateV2::Passed,
                        note: "independent proof".into(),
                        evidence: Some(proof.clone()),
                    },
                    &format!("seat-a-stage-{i}"),
                ),
            )
            .await
            .unwrap();
    }
    f.handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Accept {
                    key: "product".into(),
                    expected_row_version: 4,
                },
                "seat-a-accept",
            ),
        )
        .await
        .unwrap();
    let query = AgentManagerInspectRequestV2 {
        section: ManagerInspectSectionV2::Work,
        ..Default::default()
    };
    let before = f
        .handle
        .agent_manager_inspect(f.manager, query.clone())
        .await
        .unwrap()
        .rows[0]
        .clone();
    assert_eq!(before["source_accepted"], true);
    // Combined evidence is its own review commit directly on the sealed source.
    command(&f.review_root, &["reset", "--hard", &f.source_head]);
    let integration_proof = evidence(&f, &f.source_head, true).await;

    let (manager_b, scope_version, policy_version) = rotate_db_review_manager(&f).await;
    let after = f
        .handle
        .agent_manager_inspect(manager_b, query.clone())
        .await
        .unwrap()
        .rows[0]
        .clone();
    assert_eq!(after["key"], "product");
    assert_eq!(after["source_accepted"], true);
    assert_eq!(after["row_version"], before["row_version"]);
    assert!(before["evidence_policy_digest"].is_string());
    assert_eq!(
        after["evidence_policy_digest"], before["evidence_policy_digest"],
        "evidence digests are byte-identical across the seat move"
    );
    assert_eq!(after["acceptance"], before["acceptance"]);
    assert_eq!(after["stages"], before["stages"]);
    {
        let store = f.handle.store.lock().await;
        let (config_b, _) = store.manager_config_for_caller(manager_b).unwrap();
        let (_, work) = store.manager_v2_work(&config_b, "product").unwrap();
        assert!(
            store
                .manager_v2_accepted_source(&config_b, &work, &f.source_head)
                .unwrap()
                .is_some(),
            "accepted_source resolves through the successor's seat"
        );
    }

    command(
        &f.dir.path().join("repo"),
        &["merge", "--ff-only", &f.source_head],
    );
    f.handle
        .agent_manager_update(
            manager_b,
            fenced_req(
                ManagerUpdateV2::Integration {
                    key: "product".into(),
                    expected_row_version: 5,
                    source_commit: f.source_head.clone(),
                    target_commit: f.source_head.clone(),
                    verification: Some(integration_proof),
                },
                "seat-b-integrate",
                scope_version,
                policy_version,
            ),
        )
        .await
        .unwrap();
    let landed = f
        .handle
        .agent_manager_inspect(manager_b, query)
        .await
        .unwrap()
        .rows[0]
        .clone();
    assert_eq!(landed["integrated"], true);
    assert_eq!(landed["source_accepted"], true);
}

/// P-005: a DB-native review receipt accepted under seat A still admits the
/// exact source after a seat move, and seat B integrates it. Allocating a NEW
/// assignment stays seat-fenced; completed review facts are work-keyed.
#[tokio::test]
#[allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::significant_drop_tightening
)]
async fn db_review_acceptance_survives_seat_move_and_successor_integrates() {
    let f = fixture().await;
    let assignment_id = request_and_activate_db_review(&f, "p005-review").await;
    f.handle
        .agent_submit_review_receipt(
            f.reviewer,
            AgentSubmitReviewReceiptRequestV1 {
                assignment_id,
                verdict: ManagerReviewVerdictV1::Accepted,
                findings: vec![],
                idempotency_key: "p005-receipt".into(),
            },
        )
        .await
        .unwrap();
    {
        let store = f.handle.store.lock().await;
        store
            .update_session_status(f.reviewer, SessionStatus::Completed)
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE model_invocations SET status='completed',completed_at=?2 WHERE id=?1",
                params![
                    f.invocation.to_string(),
                    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
    }
    // Seat A accepts the exact source from the DB-native receipt.
    f.handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Accept {
                    key: "product".into(),
                    expected_row_version: 2,
                },
                "p005-seat-a-accept",
            ),
        )
        .await
        .unwrap();
    let (manager_b, scope_version, policy_version) = rotate_db_review_manager(&f).await;
    {
        let store = f.handle.store.lock().await;
        let (config_b, _) = store.manager_config_for_caller(manager_b).unwrap();
        let (_, work) = store.manager_v2_work(&config_b, "product").unwrap();
        assert!(
            store
                .manager_review_enrolled(&config_b, &work, &f.source_head)
                .unwrap()
        );
        assert!(
            store
                .manager_v2_accepted_source(&config_b, &work, &f.source_head)
                .unwrap()
                .is_some(),
            "the seat-A receipt admits the exact source under seat B"
        );
        assert!(
            store
                .manager_v2_accepted_work_source(&config_b, "product", &f.source_head)
                .unwrap()
                .is_some(),
            "integrate action admission resolves the accepted source"
        );
    }
    command(
        &f.dir.path().join("repo"),
        &["merge", "--ff-only", &f.source_head],
    );
    f.handle
        .agent_manager_update(
            manager_b,
            fenced_req(
                ManagerUpdateV2::Integration {
                    key: "product".into(),
                    expected_row_version: 3,
                    source_commit: f.source_head.clone(),
                    target_commit: f.source_head.clone(),
                    verification: None,
                },
                "p005-seat-b-integrate",
                scope_version,
                policy_version,
            ),
        )
        .await
        .unwrap();
    let rows = f
        .handle
        .agent_manager_inspect(
            manager_b,
            AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Work,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .rows;
    assert_eq!(rows[0]["review"]["mode"], "db_native");
    assert_eq!(rows[0]["review"]["current"]["verdict"], "accepted");
    assert_eq!(rows[0]["source_accepted"], true);
    assert_eq!(rows[0]["integrated"], true);
}

// ---- Issue #627: review terminal states produce a durable manager notice ----

/// Every durable notice recorded for one review assignment:
/// (job_id, kind, direction, subject_version, state).
fn review_notices(
    store: &Store,
    assignment_id: Uuid,
) -> Vec<(Uuid, String, String, String, Value)> {
    let mut statement = store
        .conn
        .prepare(
            "SELECT job_id,kind,direction,subject_version,state_json
               FROM harness_manager_notices WHERE subject_id=?1 ORDER BY sequence",
        )
        .unwrap();
    statement
        .query_map([format!("review:{assignment_id}")], |row| {
            Ok((
                Uuid::parse_str(&row.get::<_, String>(0)?).unwrap(),
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                serde_json::from_str(&row.get::<_, String>(4)?).unwrap(),
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn queued_notice_jobs(
    rx: &mut tokio::sync::broadcast::Receiver<Arc<crate::bus::DaemonEvent>>,
) -> Vec<Uuid> {
    let mut jobs = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let crate::bus::DaemonEvent::ManagerNoticeQueued { job_id } = event.as_ref() {
            jobs.push(*job_id);
        }
    }
    jobs
}

/// Make the active reviewer's invocation finish without a receipt, which the
/// next refresh settles as `manager_review_receipt_missing_final_without_receipt`.
async fn finish_reviewer_without_receipt(f: &Fixture, assignment_id: Uuid) {
    let store = f.handle.store.lock().await;
    let action_id: String = store
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
            "UPDATE model_invocations SET status='completed',completed_at=?2,dedup_key=?3,
                    project_id=(SELECT project_id FROM sessions WHERE id=?4)
              WHERE id=?1",
            params![
                f.invocation.to_string(),
                Utc::now().to_rfc3339(),
                format!("manager.action:{action_id}"),
                f.reviewer.to_string()
            ],
        )
        .unwrap();
    store
        .update_session_status(f.reviewer, SessionStatus::Completed)
        .unwrap();
}

fn review_state(store: &Store, assignment_id: Uuid) -> (String, Option<String>) {
    store
        .conn
        .query_row(
            "SELECT state,failure_code FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

/// #674 fixture: journal `count` succeeded `create_session` operations for the
/// current seat and scope, linked to terminal review assignments when
/// `review_linked` (DB-native reviewer launches).
fn seed_creations(store: &Store, f: &Fixture, count: usize, review_linked: bool) {
    let project = store
        .get_session(f.manager)
        .unwrap()
        .unwrap()
        .project_id
        .unwrap();
    let config = store.get_harness_manager(project).unwrap().unwrap();
    let stamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    for ordinal in 0..count {
        let op = store
            .seed_manager_action_for_test(
                &config,
                ManagerActionV2::CreateSession {
                    parent_id: f.epic,
                    kind: SessionKind::Research,
                    query: "seeded creation".into(),
                    launch: ManagerLaunchChoiceV2 {
                        provider: SessionProvider::Claude,
                        model: "test".into(),
                        effort: None,
                    },
                },
                ManagerActionStateV2::Succeeded,
                Some(f.source),
            )
            .unwrap();
        if review_linked {
            store
                .conn
                .execute(
                    "INSERT INTO manager_review_assignments(
                        assignment_id,project_id,epic_id,manager_session_id,scope_version,
                        work_key,spec_revision,author_session_id,source_sha,state,row_version,
                        request_json,request_fingerprint,action_operation_id,failure_code,
                        created_at,updated_at,terminal_at)
                     VALUES(?1,?2,?3,?4,?5,'seeded',1,?6,?7,'failed',1,'{}',?8,?9,
                            'manager_review_test_history',?10,?10,?10)",
                    params![
                        Uuid::new_v4().to_string(),
                        project.to_string(),
                        f.epic.to_string(),
                        config.manager_session_id.to_string(),
                        config.row_version,
                        f.source.to_string(),
                        format!("{}{ordinal:08x}", Uuid::new_v4().simple()),
                        format!("sha256:{}", "c".repeat(64)),
                        op.to_string(),
                        stamp
                    ],
                )
                .unwrap();
        }
    }
}

#[tokio::test]
async fn db_review_allocation_does_not_consume_the_lifetime_creation_budget() {
    let f = fixture().await;
    // Earlier DB-native reviewer launches are not charged either.
    seed_creations(&*f.handle.store.lock().await, &f, 3, true);
    // The seat's lifetime session quota (8) is fully used by manager workers.
    seed_creations(&*f.handle.store.lock().await, &f, 8, false);
    record_db_review_source(&f, "uncharged-review").await;
    // The request allocates its reviewer launch inline.
    let assignment_id = request_db_review(&f, "uncharged-review").await;
    let store = f.handle.store.lock().await;
    assert_eq!(review_state(&store, assignment_id).0, "allocating");
    let project = store
        .get_session(f.manager)
        .unwrap()
        .unwrap()
        .project_id
        .unwrap();
    let config = store.get_harness_manager(project).unwrap().unwrap();
    assert_eq!(store.manager_v2_created_usage(&config, false).unwrap(), 8);
}

fn review_link_count(store: &Store, operation: Uuid) -> i64 {
    store
        .conn
        .query_row(
            "SELECT count(*) FROM manager_review_assignments WHERE action_operation_id=?1",
            [operation.to_string()],
            |row| row.get(0),
        )
        .unwrap()
}

#[tokio::test]
async fn forged_review_allocation_key_stays_charged_and_genuine_retry_links_once_uncharged() {
    use crate::store::manager_reviews::{ManagerReviewFault, manager_review_fail_next};
    let f = fixture().await;
    record_db_review_source(&f, "forged-review").await;
    manager_review_fail_next(ManagerReviewFault::AfterAllocationJournal);
    let target = request_db_review(&f, "forged-review").await;
    let (config, forged, before) = {
        let store = f.handle.store.lock().await;
        assert_eq!(review_state(&store, target).0, "reserved");
        let project = store
            .get_session(f.manager)
            .unwrap()
            .unwrap()
            .project_id
            .unwrap();
        let config = store.get_harness_manager(project).unwrap().unwrap();
        let forged = store
            .manager_review_allocation_request_for_test(target)
            .unwrap();
        let before = store.manager_v2_created_usage(&config, false).unwrap();
        (config, forged, before)
    };
    // K15A-1 (b): the manager pre-submits the allocator's exact key and
    // payload through ordinary manager control; the reserved key is refused.
    let error = f
        .handle
        .agent_manager_control(f.manager, forged.clone())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_reserved_idempotency_key")
    );
    // K15A-1 (a): an operation already journaled under that key outside the
    // allocator (a pre-fix forge) is never adopted; it stays charged.
    let forged_op = {
        let store = f.handle.store.lock().await;
        let op = store
            .seed_manager_action_for_test(
                &config,
                forged.operation.clone(),
                ManagerActionStateV2::Succeeded,
                Some(f.source),
            )
            .unwrap();
        let payload = json!({
            "origin": crate::store::manager_actions::ManagerActionOriginV2::Agent {
                caller: f.manager,
            },
            "request": forged,
        });
        store
            .conn
            .execute(
                "UPDATE harness_manager_v2_operations
                    SET idempotency_key=?2,payload_json=?3,fingerprint=?4 WHERE id=?1",
                params![
                    op.to_string(),
                    forged.idempotency_key,
                    serde_json::to_string(&payload).unwrap(),
                    crate::store::harness_manager_v2::fingerprint(&payload).unwrap()
                ],
            )
            .unwrap();
        assert_eq!(
            store.manager_v2_created_usage(&config, false).unwrap(),
            before + 1
        );
        store.reconcile_manager_review_assignments_once().unwrap();
        assert_eq!(
            review_state(&store, target),
            (
                "failed".into(),
                Some("manager_review_allocation_conflict".into())
            )
        );
        op
    };
    // A genuine allocation whose first journal rolled back (restart/retry)
    // journals fresh on reconciliation, links once and stays uncharged.
    manager_review_fail_next(ManagerReviewFault::AfterAllocationJournal);
    let genuine = request_db_review(&f, "genuine-review").await;
    let store = f.handle.store.lock().await;
    assert_eq!(review_state(&store, genuine).0, "reserved");
    store.reconcile_manager_review_assignments_once().unwrap();
    assert_eq!(review_state(&store, genuine).0, "allocating");
    let genuine_op: String = store
        .conn
        .query_row(
            "SELECT action_operation_id FROM manager_review_assignments WHERE assignment_id=?1",
            [genuine.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let journaled: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_v2_operations WHERE idempotency_key=?1",
            [format!("manager-review-allocation:{genuine}")],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(journaled, 1);
    assert_eq!(
        review_link_count(&store, Uuid::parse_str(&genuine_op).unwrap()),
        1
    );
    // The forged operation is still charged; the genuine launch is not.
    assert_eq!(review_link_count(&store, forged_op), 0);
    assert_eq!(
        store.manager_v2_created_usage(&config, false).unwrap(),
        before + 1
    );
}

#[tokio::test]
async fn review_submission_records_manager_notice_and_wakes_idle_manager() {
    let f = fixture().await;
    let assignment_id = request_and_activate_db_review(&f, "review-notice").await;
    // The lead is still working, so only the recipient-idle rule can wake
    // the manager for this review result.
    f.handle
        .store
        .lock()
        .await
        .update_session_status(f.source, SessionStatus::Running)
        .unwrap();
    let mut rx = f.handle.event_bus.subscribe();
    f.handle
        .agent_submit_review_receipt(
            f.reviewer,
            AgentSubmitReviewReceiptRequestV1 {
                assignment_id,
                verdict: ManagerReviewVerdictV1::ChangesRequested,
                findings: vec![
                    ManagerReviewFindingV1 {
                        key: "missing-test".into(),
                        severity: ManagerReviewFindingSeverityV1::Error,
                        summary: "No regression test".into(),
                        location: None,
                        blocking: true,
                    },
                    ManagerReviewFindingV1 {
                        key: "naming".into(),
                        severity: ManagerReviewFindingSeverityV1::Info,
                        summary: "Prefer a clearer name".into(),
                        location: None,
                        blocking: false,
                    },
                ],
                idempotency_key: "review-notice-receipt".into(),
            },
        )
        .await
        .unwrap();

    let notices = review_notices(&*f.handle.store.lock().await, assignment_id);
    assert_eq!(notices.len(), 1, "{notices:?}");
    let (job_id, kind, direction, version, state) = &notices[0];
    assert_eq!(
        (kind.as_str(), direction.as_str(), version.as_str()),
        ("ledger_change", "to_manager", "submitted")
    );
    assert_eq!(state["assignment_id"], json!(assignment_id));
    assert_eq!(state["work_key"], "product");
    assert_eq!(state["record_key"], "work:product");
    assert_eq!(state["state"], "submitted");
    assert_eq!(state["verdict"], "changes_requested");
    assert_eq!(state["finding_count"], 2);
    assert_eq!(state["blocking_finding_count"], 1);
    assert_eq!(queued_notice_jobs(&mut rx), vec![*job_id]);

    let job = f
        .handle
        .store
        .lock()
        .await
        .get_scheduled_job(job_id)
        .unwrap()
        .unwrap();
    let crate::session::WatchFirePlan::Deliver { tip, message, .. } =
        f.sessions.plan_terminal_watch_fire(&job).await.unwrap()
    else {
        panic!("a submitted review must wake the idle manager");
    };
    assert_eq!(tip, f.manager);
    assert!(
        message.contains(&format!(
            "ledger_change subject=review:{assignment_id} version=submitted"
        )),
        "{message}"
    );
}

#[tokio::test]
async fn review_receipt_missing_records_manager_notice_and_wakes_idle_manager() {
    let f = fixture().await;
    let assignment_id = request_and_activate_db_review(&f, "review-missing").await;
    f.handle
        .store
        .lock()
        .await
        .update_session_status(f.source, SessionStatus::Running)
        .unwrap();
    finish_reviewer_without_receipt(&f, assignment_id).await;
    let mut rx = f.handle.event_bus.subscribe();
    f.sessions.reconcile_manager_actions_once().await.unwrap();

    let store = f.handle.store.lock().await;
    assert_eq!(
        review_state(&store, assignment_id),
        (
            "failed".to_string(),
            Some("manager_review_receipt_missing_final_without_receipt".to_string())
        )
    );
    let notices = review_notices(&store, assignment_id);
    drop(store);
    assert_eq!(notices.len(), 1, "{notices:?}");
    let (job_id, kind, direction, version, state) = &notices[0];
    assert_eq!(
        (kind.as_str(), direction.as_str(), version.as_str()),
        ("ledger_change", "to_manager", "failed")
    );
    assert_eq!(state["state"], "failed");
    assert_eq!(
        state["failure_code"],
        "manager_review_receipt_missing_final_without_receipt"
    );
    assert_eq!(state["record_key"], "work:product");
    assert!(queued_notice_jobs(&mut rx).contains(job_id));

    let job = f
        .handle
        .store
        .lock()
        .await
        .get_scheduled_job(job_id)
        .unwrap()
        .unwrap();
    let crate::session::WatchFirePlan::Deliver { tip, message, .. } =
        f.sessions.plan_terminal_watch_fire(&job).await.unwrap()
    else {
        panic!("a failed review must wake the idle manager");
    };
    assert_eq!(tip, f.manager);
    assert!(message.contains(&format!("subject=review:{assignment_id} version=failed")));
}

#[tokio::test]
async fn review_failure_without_resolvable_lead_still_settles_the_assignment() {
    let f = fixture().await;
    let assignment_id = request_and_activate_db_review(&f, "review-leadless").await;
    finish_reviewer_without_receipt(&f, assignment_id).await;
    let store = f.handle.store.lock().await;
    store.set_lead_session(f.epic, None).unwrap();

    let (changed, notice_jobs) = store.reconcile_manager_review_assignments_once().unwrap();
    assert_eq!(changed, 1);
    assert_eq!(notice_jobs, Vec::<Uuid>::new());
    assert_eq!(
        review_state(&store, assignment_id),
        (
            "failed".to_string(),
            Some("manager_review_receipt_missing_final_without_receipt".to_string())
        ),
        "the review transition commits even though no notice route resolves"
    );
    assert_eq!(review_notices(&store, assignment_id).len(), 0);
}

mod review_source;
