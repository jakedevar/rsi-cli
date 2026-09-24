//! K7 (#602): the appointed manager's per-session housekeeping actions
//! (`archive_session`, `restore_session`, `update_session`) under
//! `SessionControl`. Real journal, admission, runtime gates and projections.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::large_futures,
    clippy::significant_drop_tightening,
    clippy::too_many_lines
)]

use super::*;
use crate::bus::DaemonEvent;
use rsi_common::types::SessionLabel;

const ARCHIVE_BLOCKERS: &str = "manager_v2_human_or_recovery_owner";

/// Pilot plus a `SessionControl` grant (policy version 2) and one Completed
/// Task worker under the scoped Epic, hydrated in the completed map.
async fn k7_pilot(mode: ManagerOperatingModeV2) -> (Pilot, Uuid) {
    let p = pilot().await;
    let mut policy = p.policy.clone();
    policy.mode = mode;
    policy
        .capabilities
        .push(ManagerCapabilityV2::SessionControl);
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: "k7-session-control".into(),
            policy,
        })
        .unwrap();
    let worker = add_leaf(&p, p.epic, SessionStatus::Completed).await;
    (p, worker)
}

async fn add_leaf(p: &Pilot, parent: Uuid, status: SessionStatus) -> Uuid {
    let id = Uuid::new_v4();
    let mut row = bare_session(id);
    row.project_id = Some(p.project);
    row.working_dir = p.repo.clone();
    row.session_kind = SessionKind::Task;
    row.parent_id = Some(parent);
    row.status = status;
    row.title = Some("K7 worker".into());
    p.manager.store.lock().await.insert_session(&row).unwrap();
    p.manager
        .completed
        .write()
        .await
        .insert(id, CompletedSession::for_test(row));
    id
}

async fn row(p: &Pilot, id: Uuid) -> Session {
    p.manager
        .store
        .lock()
        .await
        .get_session(id)
        .unwrap()
        .unwrap()
}

async fn control(
    p: &Pilot,
    policy_version: i64,
    key: &str,
    operation: ManagerActionV2,
) -> Result<ManagerActionReceiptV2> {
    let mut request = p.request(key, operation);
    request.fence.policy_version = policy_version;
    p.manager
        .agent_control()
        .agent_manager_control(p.owner, request)
        .await
}

async fn refused_with(p: &Pilot, key: &str, operation: ManagerActionV2, code: &str) {
    let error = control(p, 2, key, operation).await.unwrap_err().to_string();
    assert!(error.contains(code), "expected {code}, got {error}");
}

async fn archive(p: &Pilot, id: Uuid) -> ManagerActionV2 {
    ManagerActionV2::ArchiveSession {
        session_id: id,
        expected_updated_at: row(p, id).await.updated_at,
    }
}

async fn restore(p: &Pilot, id: Uuid) -> ManagerActionV2 {
    ManagerActionV2::RestoreSession {
        session_id: id,
        expected_updated_at: row(p, id).await.updated_at,
    }
}

async fn update(p: &Pilot, id: Uuid, patch: ManagerSessionPatchV2) -> ManagerActionV2 {
    ManagerActionV2::UpdateSession {
        session_id: id,
        expected_updated_at: row(p, id).await.updated_at,
        patch,
    }
}

/// One settled source-worktree settlement item (with its run row) against
/// `session`: the historical-restore gate must refuse it.
fn seed_settlement_item(p: &Pilot, session: Uuid) {
    let stamp = "2026-09-22T00:00:00.000000000Z";
    let digest = format!("sha256:{}", "a".repeat(64));
    let oid = "b".repeat(40);
    let run = Uuid::new_v4().to_string();
    let store = p.manager.store.try_lock().unwrap();
    // The gate reads only the item row; custody roots are not needed for it.
    store.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    store
        .conn
        .execute(
            "INSERT INTO source_worktree_settlement_runs(
                run_id,schema_version,policy_version,repository_identity,canonical_repo_dir,
                target_ref,target_oid,plan_digest,idempotency_key,authorization_digest,
                request_fingerprint,state,observed_count,eligible_count,retained_count,
                created_at,updated_at)
             VALUES(?1,1,1,'repo','/repo','refs/heads/rolling',?2,?3,?1,?3,?3,
                'intent_committed',1,1,0,?4,?4)",
            rusqlite::params![run, oid, digest, stamp],
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO source_worktree_settlement_items(
                run_id,sequence,session_id,original_status,original_updated_at,custody_id,
                custody_generation,canonical_repo_dir,sandbox_root,sandbox_branch,
                repository_identity,source_ref,source_oid,target_oid,evidence_digest,
                clean_state_digest,reserved_effects,active_effects,participant_count,
                phase,created_at,updated_at)
             VALUES(?1,0,?2,'Completed',?3,?4,1,'/repo','/sandbox','rsi/k7','repo',
                'refs/heads/rsi/k7',?5,?5,?6,?6,0,0,1,'settled',?3,?3)",
            rusqlite::params![
                run,
                session.to_string(),
                stamp,
                Uuid::new_v4().to_string(),
                oid,
                digest
            ],
        )
        .unwrap();
    store.conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
}

#[tokio::test]
async fn manager_session_archive_then_restore_a_completed_scoped_worker() {
    let (p, worker) = k7_pilot(ManagerOperatingModeV2::Execute).await;
    let mut events = p.manager.event_bus.subscribe();
    let operation = archive(&p, worker).await;
    let queued = control(&p, 2, "k7-archive", operation.clone())
        .await
        .unwrap();
    assert_eq!(queued.action_kind, ManagerActionKindV2::ArchiveSession);
    assert_eq!(queued.target_session_id, Some(worker));
    p.execute().await.unwrap();
    let receipt = p.receipt(queued.operation_id).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Succeeded);
    assert_eq!(receipt.result, Some(ManagerActionResultV2::SessionArchived));
    assert_eq!(row(&p, worker).await.status, SessionStatus::Archived);
    assert!(!p.manager.completed.read().await.contains_key(&worker));
    let mut archived = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let DaemonEvent::SessionArchived { session_id, .. } = event.as_ref() {
            archived.push(*session_id);
        }
    }
    assert_eq!(archived, vec![worker]);

    // Identical replay returns the original receipt without a new effect.
    let replay = control(&p, 2, "k7-archive", operation).await.unwrap();
    assert_eq!(replay.operation_id, queued.operation_id);
    assert_eq!(replay.state, ManagerActionStateV2::Succeeded);
    assert!(replay.deduplicated);

    let queued = control(&p, 2, "k7-restore", restore(&p, worker).await)
        .await
        .unwrap();
    p.execute().await.unwrap();
    let receipt = p.receipt(queued.operation_id).await;
    assert_eq!(receipt.result, Some(ManagerActionResultV2::SessionRestored));
    let restored = row(&p, worker).await;
    assert_eq!(restored.status, SessionStatus::Completed);
    assert!(!restored.pending_archive);
    let hydrated = p.manager.completed.read().await;
    let entry = hydrated.get(&worker).expect("restored worker is hydrated");
    assert_eq!(entry.session.status, SessionStatus::Completed);
    assert!(entry.events_hydrated);
    drop(hydrated);
    let mut unarchived = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let DaemonEvent::SessionUnarchived { session_id } = event.as_ref() {
            unarchived.push(*session_id);
        }
    }
    assert_eq!(unarchived, vec![worker]);
}

#[tokio::test]
async fn manager_session_archive_refuses_unsettled_or_structural_targets() {
    let (p, worker) = k7_pilot(ManagerOperatingModeV2::Execute).await;
    // Running.
    let running = add_leaf(&p, p.epic, SessionStatus::Running).await;
    refused_with(
        &p,
        "k7-running",
        archive(&p, running).await,
        "manager_v2_session_not_terminal",
    )
    .await;
    // Pending question is a genuine human gate.
    let asked = add_leaf(&p, p.epic, SessionStatus::Completed).await;
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET pending_question_json='{\"question\":\"ship?\"}' WHERE id=?1",
            [asked.to_string()],
        )
        .unwrap();
    refused_with(
        &p,
        "k7-question",
        archive(&p, asked).await,
        ARCHIVE_BLOCKERS,
    )
    .await;
    // A leaf with a live child is a tree: the container cascade owns it.
    let child = add_leaf(&p, worker, SessionStatus::Completed).await;
    refused_with(
        &p,
        "k7-descendants",
        archive(&p, worker).await,
        "manager_v2_session_has_descendants",
    )
    .await;
    // The Epic's current lead must be replaced or unassigned first.
    refused_with(
        &p,
        "k7-lead",
        archive(&p, p.lead).await,
        "manager_v2_session_is_lead",
    )
    .await;
    // Containers stay under Topology's container actions.
    refused_with(
        &p,
        "k7-container",
        archive(&p, p.epic).await,
        "manager_v2_leaf_required",
    )
    .await;
    for (id, status) in [
        (running, SessionStatus::Running),
        (asked, SessionStatus::Completed),
        (worker, SessionStatus::Completed),
        (child, SessionStatus::Completed),
        (p.lead, SessionStatus::Completed),
    ] {
        assert_eq!(row(&p, id).await.status, status);
    }
    let queued: i64 = p
        .manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_v2_operations WHERE kind='lifecycle_action'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(queued, 0);
}

#[tokio::test]
async fn manager_session_archive_refuses_an_in_memory_retry_owner_at_execution() {
    let (p, worker) = k7_pilot(ManagerOperatingModeV2::Execute).await;
    let (retry_cancel, mut retry_observer) = tokio::sync::oneshot::channel();
    p.manager
        .completed
        .write()
        .await
        .get_mut(&worker)
        .unwrap()
        .retry_cancel = Some(retry_cancel);
    let queued = control(&p, 2, "k7-retry-owner", archive(&p, worker).await)
        .await
        .unwrap();
    assert!(
        p.execute()
            .await
            .unwrap_err()
            .to_string()
            .contains(ARCHIVE_BLOCKERS)
    );
    assert_eq!(row(&p, worker).await.status, SessionStatus::Completed);
    assert!(matches!(
        retry_observer.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    assert!(p.manager.completed.read().await.contains_key(&worker));
    assert_eq!(
        p.receipt(queued.operation_id).await.state,
        ManagerActionStateV2::Running
    );
}

#[tokio::test]
async fn manager_session_actions_require_scope_grant_and_execute_mode() {
    // Missing SessionControl: the base pilot policy (version 1) lacks it.
    let p = pilot().await;
    let worker = add_leaf(&p, p.epic, SessionStatus::Completed).await;
    let error = control(&p, 1, "k7-no-grant", archive(&p, worker).await)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("manager_v2_capability_denied"), "{error}");
    assert_eq!(row(&p, worker).await.status, SessionStatus::Completed);

    // Monitor mode with the grant still refuses mutations.
    let (p, worker) = k7_pilot(ManagerOperatingModeV2::Monitor).await;
    refused_with(
        &p,
        "k7-monitor",
        archive(&p, worker).await,
        "manager_v2_execute_required",
    )
    .await;
    refused_with(
        &p,
        "k7-monitor-update",
        update(
            &p,
            worker,
            ManagerSessionPatchV2 {
                title: Some("Monitor rename".into()),
                ..Default::default()
            },
        )
        .await,
        "manager_v2_execute_required",
    )
    .await;
    assert_eq!(row(&p, worker).await.status, SessionStatus::Completed);
    assert_eq!(row(&p, worker).await.title.as_deref(), Some("K7 worker"));

    // A worker under an Epic outside the manager's scope is out of reach.
    let (p, _) = k7_pilot(ManagerOperatingModeV2::Execute).await;
    let other_group = Uuid::new_v4();
    let other_epic = Uuid::new_v4();
    {
        let store = p.manager.store.lock().await;
        for (id, kind, parent) in [
            (other_group, SessionKind::Group, None),
            (other_epic, SessionKind::Epic, Some(other_group)),
        ] {
            let mut container = bare_session(id);
            container.project_id = Some(p.project);
            container.working_dir = p.repo.clone();
            container.session_kind = kind;
            container.parent_id = parent;
            store.insert_session(&container).unwrap();
        }
    }
    let outside = add_leaf(&p, other_epic, SessionStatus::Completed).await;
    refused_with(
        &p,
        "k7-out-of-scope",
        archive(&p, outside).await,
        "manager_v2_session_out_of_scope",
    )
    .await;
    assert_eq!(row(&p, outside).await.status, SessionStatus::Completed);
}

#[tokio::test]
async fn manager_session_restore_refuses_purged_sandbox_and_settlement_history() {
    let (p, worker) = k7_pilot(ManagerOperatingModeV2::Execute).await;
    let settled = add_leaf(&p, p.epic, SessionStatus::Completed).await;
    for (key, id) in [
        ("k7-archive-purged", worker),
        ("k7-archive-settled", settled),
    ] {
        control(&p, 2, key, archive(&p, id).await).await.unwrap();
        p.execute().await.unwrap();
        assert_eq!(row(&p, id).await.status, SessionStatus::Archived);
    }
    p.manager.store.lock().await.conn.execute(
        "UPDATE sessions SET sandbox_kind='GitWorktree',sandbox_cleanup_state='Purged' WHERE id=?1",
        [worker.to_string()],
    ).unwrap();
    seed_settlement_item(&p, settled);
    for (key, id) in [
        ("k7-restore-purged", worker),
        ("k7-restore-settled", settled),
    ] {
        refused_with(
            &p,
            key,
            restore(&p, id).await,
            "manager_v2_historical_restore_refused",
        )
        .await;
        assert_eq!(row(&p, id).await.status, SessionStatus::Archived);
    }
}

/// K4 reviewer follow-up: a cascade restore also honours settlement history
/// recorded against one of its recorded members.
#[tokio::test]
async fn manager_container_restore_refuses_a_member_with_settlement_history() {
    let (p, _worker) = k7_pilot(ManagerOperatingModeV2::Execute).await;
    let group = row(&p, p.group).await;
    control(
        &p,
        2,
        "k7-archive-group",
        ManagerActionV2::ArchiveContainer {
            container_id: p.group,
            expected_updated_at: group.updated_at,
        },
    )
    .await
    .unwrap();
    p.execute().await.unwrap();
    seed_settlement_item(&p, p.lead);
    let archived = row(&p, p.group).await;
    control(
        &p,
        2,
        "k7-restore-group",
        ManagerActionV2::RestoreContainer {
            container_id: p.group,
            expected_updated_at: archived.updated_at,
        },
    )
    .await
    .unwrap();
    assert!(
        p.execute()
            .await
            .unwrap_err()
            .to_string()
            .contains("manager_v2_historical_restore_refused")
    );
    for id in [p.group, p.epic, p.lead] {
        assert_eq!(row(&p, id).await.status, SessionStatus::Archived);
    }
}

#[tokio::test]
async fn manager_session_update_applies_every_field_with_operator_side_effects() {
    let (p, worker) = k7_pilot(ManagerOperatingModeV2::Execute).await;
    let label = SessionLabel {
        id: Uuid::new_v4(),
        name: "k7-label".into(),
        description: None,
        project_id: Some(p.project),
        color: "#cba6f7".into(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    p.manager.store.lock().await.insert_label(&label).unwrap();
    let mut events = p.manager.event_bus.subscribe();
    let queued = control(
        &p,
        2,
        "k7-update-all",
        update(
            &p,
            worker,
            ManagerSessionPatchV2 {
                title: Some("Renamed worker".into()),
                description: Some(ManagerFieldPatchV2::Set("Why it exists".into())),
                rating: Some(ManagerFieldPatchV2::Set(7)),
                active_task: Some(ManagerFieldPatchV2::Set("Ship K7".into())),
                label: Some(ManagerFieldPatchV2::Set(label.id)),
                tags: Some(vec!["Beta".into(), "alpha".into(), "beta".into()]),
            },
        )
        .await,
    )
    .await
    .unwrap();
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(queued.operation_id).await.result,
        Some(ManagerActionResultV2::SessionUpdated)
    );
    let stored = row(&p, worker).await;
    let memory = p
        .manager
        .completed
        .read()
        .await
        .get(&worker)
        .unwrap()
        .session
        .clone();
    for session in [&stored, &memory] {
        assert_eq!(session.title.as_deref(), Some("Renamed worker"));
        assert_eq!(session.description.as_deref(), Some("Why it exists"));
        assert_eq!(session.rating, Some(7));
        assert_eq!(session.active_task.as_deref(), Some("Ship K7"));
        assert_eq!(session.group_id, Some(label.id));
        assert_eq!(session.tags, vec!["alpha".to_string(), "beta".to_string()]);
        assert_eq!(session.tag, "alpha");
        assert_eq!(session.status, SessionStatus::Completed);
    }
    let mut changed = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let DaemonEvent::SessionMetadataChanged { session_id, .. } = event.as_ref() {
            changed.push(*session_id);
        }
    }
    assert_eq!(changed, vec![worker]);

    // Explicit clears.
    control(
        &p,
        2,
        "k7-update-clear",
        update(
            &p,
            worker,
            ManagerSessionPatchV2 {
                description: Some(ManagerFieldPatchV2::Clear),
                rating: Some(ManagerFieldPatchV2::Clear),
                active_task: Some(ManagerFieldPatchV2::Clear),
                label: Some(ManagerFieldPatchV2::Clear),
                ..Default::default()
            },
        )
        .await,
    )
    .await
    .unwrap();
    p.execute().await.unwrap();
    let stored = row(&p, worker).await;
    let memory = p
        .manager
        .completed
        .read()
        .await
        .get(&worker)
        .unwrap()
        .session
        .clone();
    for session in [&stored, &memory] {
        assert_eq!(session.title.as_deref(), Some("Renamed worker"));
        assert_eq!(session.description, None);
        assert_eq!(session.rating, None);
        assert_eq!(session.active_task, None);
        assert_eq!(session.group_id, None);
        assert_eq!(session.tags, vec!["alpha".to_string(), "beta".to_string()]);
    }
}

#[tokio::test]
async fn manager_session_update_refuses_invalid_patch_stale_fence_and_archived_target() {
    let (p, worker) = k7_pilot(ManagerOperatingModeV2::Execute).await;
    for (key, patch, code) in [
        (
            "k7-bad-rating",
            ManagerSessionPatchV2 {
                rating: Some(ManagerFieldPatchV2::Set(11)),
                ..Default::default()
            },
            "manager_v2_invalid_rating",
        ),
        (
            "k7-unknown-label",
            ManagerSessionPatchV2 {
                label: Some(ManagerFieldPatchV2::Set(Uuid::new_v4())),
                ..Default::default()
            },
            "manager_v2_label_unavailable",
        ),
        (
            "k7-bad-tag",
            ManagerSessionPatchV2 {
                tags: Some(vec!["not a tag!".into()]),
                ..Default::default()
            },
            "manager_v2_invalid_tags",
        ),
        (
            "k7-empty",
            ManagerSessionPatchV2::default(),
            "manager_v2_empty_session_patch",
        ),
    ] {
        refused_with(&p, key, update(&p, worker, patch).await, code).await;
    }
    let before = row(&p, worker).await;
    let stale = ManagerActionV2::UpdateSession {
        session_id: worker,
        expected_updated_at: before.updated_at - chrono::Duration::seconds(1),
        patch: ManagerSessionPatchV2 {
            title: Some("Stale rename".into()),
            ..Default::default()
        },
    };
    refused_with(&p, "k7-stale", stale, "manager_v2_session_changed").await;
    let after = row(&p, worker).await;
    assert_eq!(after.title.as_deref(), Some("K7 worker"));
    assert_eq!(after.rating, before.rating);
    assert_eq!(after.updated_at, before.updated_at);

    control(&p, 2, "k7-archive-for-update", archive(&p, worker).await)
        .await
        .unwrap();
    p.execute().await.unwrap();
    refused_with(
        &p,
        "k7-update-archived",
        update(
            &p,
            worker,
            ManagerSessionPatchV2 {
                title: Some("Archived rename".into()),
                ..Default::default()
            },
        )
        .await,
        "manager_v2_session_state_changed",
    )
    .await;
    assert_eq!(row(&p, worker).await.title.as_deref(), Some("K7 worker"));
}
