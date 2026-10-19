use super::*;
use crate::store::Store;
use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
use crate::test_support::make_test_session;
use rsi_common::agent_control_schema::AgentControlVerbV1 as Verb;
use rsi_common::global_manager::ConfigureGlobalManagerRequestV1;
use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
use rsi_common::harness_manager_v2::{
    ConfigureHarnessManagerPolicyRequestV2, ManagerLaunchChoiceV2, ManagerPolicyV2,
};
use rsi_common::types::{Project, SessionProvider};

fn project(store: &Store) -> Uuid {
    let id = Uuid::new_v4();
    let now = chrono::Utc::now();
    store
        .insert_project(&Project {
            id,
            name: id.to_string(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();
    id
}

fn session(store: &Store, project: Option<Uuid>) -> Uuid {
    let mut session = make_test_session();
    session.project_id = project;
    let id = session.id;
    store.insert_session(&session).unwrap();
    id
}

fn policy() -> ManagerPolicyV2 {
    ManagerPolicyV2 {
        mode: ManagerOperatingModeV2::Execute,
        capabilities: vec![ManagerCapabilityV2::StorageControl],
        ..Default::default()
    }
}

fn set_policy(store: &Store, project: Uuid, policy: ManagerPolicyV2) {
    let config = store.get_harness_manager(project).unwrap().unwrap();
    let version = store
        .get_harness_manager_policy(project)
        .unwrap()
        .map_or(0, |p| p.row_version);
    store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: project,
            expected_scope_version: config.row_version,
            expected_policy_version: version,
            idempotency_key: Uuid::new_v4().to_string(),
            policy,
        })
        .unwrap();
}

fn appoint(store: &Store, project: Uuid, pm: Uuid) {
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: project,
            session_id: pm,
            epic_ids: None,
            group_ids: vec![],
            expected_row_version: 0,
        })
        .unwrap();
    set_policy(store, project, policy());
}

fn root(store: &mut Store, project: Option<Uuid>, identity: &str) -> Uuid {
    let mut session = make_test_session();
    session.project_id = project;
    session.status = SessionStatus::Completed;
    session.working_dir = std::path::PathBuf::from("/tmp/agent-settlement/repo");
    session.sandbox_kind = Some(rsi_common::types::SandboxKind::GitWorktree);
    let sandbox_root = std::path::PathBuf::from(format!("/tmp/agent-settlement/{}", session.id));
    let branch = format!("rsi/{}", session.id);
    session.sandbox_root = Some(sandbox_root.clone());
    session.sandbox_branch = Some(branch.clone());
    session.sandbox_cleanup_state = Some(rsi_common::types::SandboxCleanupState::Live);
    let id = session.id;
    store
        .insert_session_with_custody(
            &session,
            SessionCustodyBinding::New(NewCustodyRoot {
                custody_id: Uuid::new_v4(),
                canonical_repo_dir: "/tmp/agent-settlement/repo".into(),
                sandbox_root: sandbox_root.to_str().unwrap().into(),
                sandbox_branch: branch,
                repository_identity: identity.into(),
                source_commit: "a".repeat(40),
                cause: CustodyCause::FreshLaunch,
            }),
        )
        .unwrap();
    id
}

fn code(error: DaemonError) -> String {
    match error {
        DaemonError::StructuredRpc { data, .. } => data["code"].as_str().unwrap().into(),
        other => panic!("typed refusal required: {other}"),
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn capability_pause_monitor_and_revoked_seats_refuse_and_catalog_agrees() {
    let store = Store::open_in_memory().unwrap();
    let p = project(&store);
    let pm = session(&store, Some(p));
    let worker = session(&store, Some(p));
    appoint(&store, p, pm);
    assert_eq!(
        code(store.agent_source_worktree_projects(worker).unwrap_err()),
        "storage_control_not_authorized"
    );
    for forbidden in [
        ManagerPolicyV2 {
            capabilities: vec![],
            ..policy()
        },
        ManagerPolicyV2 {
            paused: true,
            ..policy()
        },
        ManagerPolicyV2 {
            mode: ManagerOperatingModeV2::Monitor,
            ..policy()
        },
    ] {
        set_policy(&store, p, forbidden);
        assert_eq!(
            code(store.agent_source_worktree_projects(pm).unwrap_err()),
            "storage_control_not_authorized"
        );
        assert_eq!(
            store
                .agent_authority_projection(pm)
                .unwrap()
                .verbs
                .iter()
                .filter(|v| matches!(
                    v,
                    Verb::ListSourceWorktreeCohorts
                        | Verb::AuditSourceWorktreeCohort
                        | Verb::ApplySourceWorktreeCohort
                ))
                .count(),
            0
        );
    }
    set_policy(&store, p, policy());
    assert_eq!(store.agent_source_worktree_projects(pm).unwrap(), vec![p]);
    for verb in [
        Verb::ListSourceWorktreeCohorts,
        Verb::AuditSourceWorktreeCohort,
        Verb::ApplySourceWorktreeCohort,
    ] {
        assert!(
            store
                .agent_authority_projection(pm)
                .unwrap()
                .verbs
                .contains(&verb)
        );
    }
    let version = store.get_harness_manager(p).unwrap().unwrap().row_version;
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: p,
            session_id: worker,
            epic_ids: None,
            group_ids: vec![],
            expected_row_version: version,
        })
        .unwrap();
    assert_eq!(
        code(store.agent_source_worktree_projects(pm).unwrap_err()),
        "storage_control_not_authorized"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn project_manager_admits_mixed_cohort_and_excludes_foreign_participant_per_root() {
    let mut store = Store::open_in_memory().unwrap();
    let own = project(&store);
    let foreign = project(&store);
    let pm = session(&store, Some(own));
    appoint(&store, own, pm);
    let owner = root(&mut store, Some(own), "repo:own");
    root(&mut store, Some(foreign), "repo:foreign");
    root(&mut store, None, "repo:unknown");
    store
        .authorize_agent_source_worktree_cohort(pm, "repo:own")
        .unwrap();
    for identity in ["repo:foreign", "repo:unknown", "repo:missing"] {
        assert_eq!(
            code(
                store
                    .authorize_agent_source_worktree_cohort(pm, identity)
                    .unwrap_err()
            ),
            "storage_control_not_in_scope"
        );
    }
    // A foreign participant retains only its root; repository admission remains valid.
    let participant = session(&store, Some(foreign));
    store.conn.execute("UPDATE sessions SET sandbox_custody_id=(SELECT sandbox_custody_id FROM sessions WHERE id=?1) WHERE id=?2",
        rusqlite::params![owner.to_string(), participant.to_string()]).unwrap();
    store
        .authorize_agent_source_worktree_cohort(pm, "repo:own")
        .unwrap();
    let mut inventory = store.source_worktree_inventory("repo:own", 256).unwrap();
    store
        .scope_agent_source_worktree_inventory(pm, "repo:own", &mut inventory)
        .unwrap();
    assert_eq!(
        inventory[0].agent_scope_disposition,
        Some(rsi_common::cohort_settlement::SourceWorktreeDispositionV1::RetainedForeignOwner)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn portfolio_storage_grant_reaches_only_coverage_and_is_revocable() {
    let mut store = Store::open_in_memory().unwrap();
    let a = project(&store);
    let b = project(&store);
    let outside = project(&store);
    let seat = session(&store, None);
    root(&mut store, Some(a), "repo:mixed");
    root(&mut store, Some(b), "repo:mixed");
    root(&mut store, Some(outside), "repo:outside");
    let request = ConfigureGlobalManagerRequestV1 {
        session_id: seat,
        project_ids: vec![a, b],
        allowed_launches: vec![ManagerLaunchChoiceV2 {
            provider: SessionProvider::Claude,
            model: "claude-opus-5-5".into(),
            effort: Some("high".into()),
        }],
        project_policy: policy(),
        expected_grant_version: 0,
        idempotency_key: "storage:grant".into(),
    };
    store
        .configure_global_manager(&request, "operator:test")
        .unwrap();
    store
        .authorize_agent_source_worktree_cohort(seat, "repo:mixed")
        .unwrap();
    assert!(
        store
            .agent_authority_projection(seat)
            .unwrap()
            .verbs
            .contains(&Verb::ApplySourceWorktreeCohort)
    );
    assert_eq!(
        code(
            store
                .authorize_agent_source_worktree_cohort(seat, "repo:outside")
                .unwrap_err()
        ),
        "storage_control_not_in_scope"
    );
    store
        .configure_global_manager(
            &ConfigureGlobalManagerRequestV1 {
                project_policy: ManagerPolicyV2 {
                    paused: true,
                    ..policy()
                },
                expected_grant_version: 1,
                idempotency_key: "storage:pause".into(),
                ..request
            },
            "operator:test",
        )
        .unwrap();
    assert_eq!(
        code(store.agent_source_worktree_projects(seat).unwrap_err()),
        "storage_control_not_authorized"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn mixed_roots_retain_foreign_and_unknown_owners_and_admit_covered_deleted_owners() {
    use rsi_common::cohort_settlement::SourceWorktreeDispositionV1 as D;
    let mut store = Store::open_in_memory().unwrap();
    let own = project(&store);
    let foreign = project(&store);
    let pm = session(&store, Some(own));
    appoint(&store, own, pm);
    let covered = root(&mut store, Some(own), "repo:mixed");
    let live = root(&mut store, Some(foreign), "repo:mixed");
    let unknown = root(&mut store, None, "repo:mixed");
    let deleted = root(&mut store, Some(own), "repo:mixed");
    let mut terminal_foreign = Vec::new();
    for status in ["Completed", "Failed", "Interrupted", "Archived", "Deleted"] {
        let id = root(&mut store, Some(foreign), "repo:mixed");
        store
            .conn
            .execute(
                "UPDATE sessions SET status=?1 WHERE id=?2",
                rusqlite::params![status, id.to_string()],
            )
            .unwrap();
        terminal_foreign.push((id, Some(D::RetainedForeignOwner)));
    }
    store
        .conn
        .execute(
            "UPDATE sessions SET status='Running' WHERE id=?1",
            [live.to_string()],
        )
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE sessions SET status='Deleted' WHERE id=?1",
            [deleted.to_string()],
        )
        .unwrap();
    let mut inventory = store.source_worktree_inventory("repo:mixed", 256).unwrap();
    store
        .scope_agent_source_worktree_inventory(pm, "repo:mixed", &mut inventory)
        .unwrap();
    for (id, expected) in [
        (covered, None),
        (live, Some(D::RetainedForeignOwner)),
        (unknown, Some(D::RetainedUnknownOwner)),
        (deleted, None),
    ]
    .into_iter()
    .chain(terminal_foreign)
    {
        let row = inventory.iter().find(|r| r.session_id == Some(id)).unwrap();
        assert_eq!(row.agent_scope_disposition, expected);
        if expected.is_none() {
            store
                .authorize_agent_source_worktree_root(pm, "repo:mixed", row.custody_id)
                .unwrap();
        } else {
            assert_eq!(
                code(
                    store
                        .authorize_agent_source_worktree_root(pm, "repo:mixed", row.custody_id)
                        .unwrap_err()
                ),
                "storage_control_not_in_scope"
            );
        }
    }
}
