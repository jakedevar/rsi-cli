//! Store tests for the global manager grant, messages and appointments
//! (#872 Slice B).

use super::*;
use crate::test_support::test_session;
use rsi_common::global_manager::GLOBAL_LAUNCH_NOT_ALLOWED;
use rsi_common::global_manager::GlobalManagerGrantV1;
use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
use rsi_common::harness_manager_v2::ManagerPolicyV2;
use rsi_common::types::{Project, SessionProvider};
use std::path::PathBuf;

fn project(store: &Store, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now();
    store
        .insert_project(&Project {
            id,
            name: format!("{name} {id}"),
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
    let id = Uuid::new_v4();
    let mut row = test_session(id, PathBuf::from("/tmp/global-manager"));
    row.project_id = project;
    store.insert_session(&row).unwrap();
    id
}

fn launch() -> ManagerLaunchChoiceV2 {
    ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "claude-opus-5-5".into(),
        effort: Some("high".into()),
    }
}

fn configure(
    store: &Store,
    seat: Uuid,
    projects: &[Uuid],
    expected: i64,
    key: &str,
) -> Result<GlobalManagerGrantV1> {
    store.configure_global_manager(
        &ConfigureGlobalManagerRequestV1 {
            session_id: seat,
            project_ids: projects.to_vec(),
            allowed_launches: vec![launch()],
            project_policy: ManagerPolicyV2::default(),
            expected_grant_version: expected,
            idempotency_key: key.into(),
        },
        "operator:test",
    )
}

fn appoint_pm(store: &Store, project: Uuid) -> Uuid {
    let pm = session(store, Some(project));
    let expected_row_version = store
        .get_harness_manager(project)
        .unwrap()
        .map_or(0, |config| config.row_version);
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: project,
            session_id: pm,
            epic_ids: None,
            group_ids: vec![],
            expected_row_version,
        })
        .unwrap();
    pm
}

fn code(error: DaemonError) -> String {
    match error {
        DaemonError::InvalidParam(code) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn global_grant_names_exactly_its_projects_and_refuses_others() {
    let store = Store::open_in_memory().unwrap();
    let (a, b, c) = (
        project(&store, "A"),
        project(&store, "B"),
        project(&store, "C"),
    );
    let seat = session(&store, None);
    let grant = configure(&store, seat, &[a, b], 0, "g1").unwrap();
    assert_eq!(grant.state, "active");
    assert_eq!(grant.grant_version, 1);
    let rows = store.global_project_rows(&grant).unwrap();
    assert_eq!(
        rows.iter().map(|row| row.project_id).collect::<Vec<_>>(),
        [a, b]
    );
    assert_eq!(
        code(Store::global_grant_covers(&grant, c).unwrap_err()),
        GLOBAL_PROJECT_NOT_IN_GRANT
    );
    assert_eq!(
        store.global_seat_grant(seat).unwrap().grant_id,
        grant.grant_id
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn global_seat_refuses_non_seat_revoked_and_replaced_callers() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let seat = session(&store, None);
    let other = session(&store, None);
    let first = configure(&store, seat, &[a], 0, "g1").unwrap();
    assert_eq!(
        code(store.global_seat_grant(other).unwrap_err()),
        GLOBAL_MANAGER_NOT_SEAT
    );
    // Replay returns the same grant; a stale version is refused.
    assert_eq!(
        configure(&store, seat, &[a], 0, "g1").unwrap().grant_id,
        first.grant_id
    );
    assert_eq!(
        code(configure(&store, other, &[a], 0, "g2").unwrap_err()),
        GLOBAL_MANAGER_STALE
    );
    let second = configure(&store, other, &[a], 1, "g2").unwrap();
    assert_eq!(second.grant_version, 2);
    assert_eq!(
        code(store.global_seat_grant(seat).unwrap_err()),
        GLOBAL_MANAGER_NOT_SEAT,
        "the old seat after a replace"
    );
    assert_eq!(
        store.global_seat_grant(other).unwrap().grant_id,
        second.grant_id
    );
    let revoked = store
        .revoke_global_manager(&RevokeGlobalManagerRequestV1 {
            expected_grant_version: 2,
            idempotency_key: "r1".into(),
        })
        .unwrap();
    assert_eq!(revoked.state, "revoked");
    assert_eq!(
        code(store.global_seat_grant(other).unwrap_err()),
        GLOBAL_MANAGER_NOT_SEAT,
        "a revoked grant"
    );
    assert!(store.active_global_grant().unwrap().is_none());
    // A replayed revoke returns the revoked grant.
    assert_eq!(
        store
            .revoke_global_manager(&RevokeGlobalManagerRequestV1 {
                expected_grant_version: 2,
                idempotency_key: "r1".into(),
            })
            .unwrap()
            .grant_id,
        second.grant_id
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn global_grant_refuses_unknown_projects_and_retains_rows() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let seat = session(&store, None);
    assert_eq!(
        code(configure(&store, seat, &[Uuid::new_v4()], 0, "g0").unwrap_err()),
        "global_manager_unknown_project"
    );
    let grant = configure(&store, seat, &[a], 0, "g1").unwrap();
    let id = grant.grant_id.to_string();
    let deleted = store
        .conn
        .execute("DELETE FROM global_manager_grants WHERE id=?1", [&id]);
    assert!(deleted.is_err(), "grants are retained");
    let widened = store.conn.execute(
        "UPDATE global_manager_grants SET project_ids_json='[]' WHERE id=?1",
        [&id],
    );
    assert!(widened.is_err(), "the project list is immutable");
    store
        .conn
        .execute(
            "UPDATE global_manager_grants SET state='revoked',updated_at=?2 WHERE id=?1",
            params![id, stamp()],
        )
        .expect("state and updated_at may change");
    let reactivated = store.conn.execute(
        "UPDATE global_manager_grants SET state='active' WHERE id=?1",
        [&id],
    );
    assert!(reactivated.is_err(), "a revoked grant stays revoked");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn global_manager_tables_are_installed_at_their_schema_version() {
    let store = Store::open_in_memory().unwrap();
    let version: i32 = store
        .conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert!(version >= GLOBAL_MANAGER_SCHEMA_VERSION);
    for (kind, name) in CATALOG_OBJECTS {
        let present: bool = store
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type=?1 AND name=?2)",
                [kind, name],
                |row| row.get(0),
            )
            .unwrap();
        assert!(present, "{kind} {name}");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn global_seat_reads_only_granted_pm_seats_and_pm_reports_up() {
    let store = Store::open_in_memory().unwrap();
    let (a, c) = (project(&store, "A"), project(&store, "C"));
    let seat = session(&store, None);
    configure(&store, seat, &[a], 0, "g1").unwrap();
    let pm_a = appoint_pm(&store, a);
    let pm_c = appoint_pm(&store, c);
    let worker = session(&store, Some(a));
    assert!(store.global_seat_reads_manager(seat, pm_a).unwrap());
    assert!(!store.global_seat_reads_manager(seat, pm_c).unwrap());
    assert!(!store.global_seat_reads_manager(seat, worker).unwrap());
    assert!(!store.global_seat_reads_manager(worker, pm_a).unwrap());

    let (grant, reported_project) = store.global_report_grant(pm_a).unwrap();
    assert_eq!(reported_project, a);
    assert_eq!(grant.seat_session_id, seat);
    assert_eq!(
        code(store.global_report_grant(pm_c).unwrap_err()),
        GLOBAL_REPORT_NOT_AUTHORIZED,
        "the PM of an ungranted project"
    );
    assert_eq!(
        code(store.global_report_grant(worker).unwrap_err()),
        GLOBAL_REPORT_NOT_AUTHORIZED,
        "a non-PM"
    );
    assert_eq!(store.global_current_manager(a).unwrap(), pm_a);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn global_message_is_a_resume_wake_on_the_pm_and_replays() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let seat = session(&store, None);
    let grant = configure(&store, seat, &[a], 0, "g1").unwrap();
    let pm = appoint_pm(&store, a);
    let message = |text: &str| GlobalMessage {
        grant: &grant,
        direction: GlobalMessageDirection::ToManager,
        project_id: a,
        sender: seat,
        target: pm,
        idempotency_key: "m1",
        request: serde_json::json!({"message": text}),
        delivery: format!("global: {text}"),
    };
    let receipt = store.queue_global_message(&message("hi")).unwrap();
    assert_eq!(receipt.target_session_id, pm);
    assert!(!receipt.deduplicated);
    let job = store
        .get_scheduled_job(&receipt.message_id)
        .unwrap()
        .unwrap();
    assert_eq!(job.wake_mode, WakeMode::Resume);
    assert_eq!(job.wake_session_id, Some(pm));
    assert!(job.enabled);
    assert_eq!(job.message, "global: hi");
    let replay = store.queue_global_message(&message("hi")).unwrap();
    assert_eq!(replay.message_id, receipt.message_id);
    assert!(replay.deduplicated);
    assert_eq!(
        code(store.queue_global_message(&message("changed")).unwrap_err()),
        GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn global_launch_allowlist_is_exact() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let seat = session(&store, None);
    let grant = configure(&store, seat, &[a], 0, "g1").unwrap();
    assert!(Store::global_launch_allowed(&grant, &launch()).is_ok());
    let mut other = launch();
    other.model = "claude-sonnet-5-5".into();
    assert_eq!(
        code(Store::global_launch_allowed(&grant, &other).unwrap_err()),
        GLOBAL_LAUNCH_NOT_ALLOWED
    );
    other = launch();
    other.effort = Some("max".into());
    assert_eq!(
        code(Store::global_launch_allowed(&grant, &other).unwrap_err()),
        GLOBAL_LAUNCH_NOT_ALLOWED
    );
}

fn clear_pm(store: &Store, project: Uuid, pm: Uuid) {
    let expected_row_version = store
        .get_harness_manager(project)
        .unwrap()
        .unwrap()
        .row_version;
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: project,
            session_id: pm,
            epic_ids: Some(vec![]),
            group_ids: vec![],
            expected_row_version,
        })
        .unwrap();
}

fn queue(
    store: &Store,
    grant: &GlobalManagerGrantV1,
    project: Uuid,
    target: Uuid,
    key: &str,
) -> Uuid {
    store
        .queue_global_message(&GlobalMessage {
            grant,
            direction: GlobalMessageDirection::ToManager,
            project_id: project,
            sender: grant.seat_session_id,
            target,
            idempotency_key: key,
            request: serde_json::json!({"message": key}),
            delivery: key.to_string(),
        })
        .unwrap()
        .message_id
}

fn enabled(store: &Store, job: Uuid) -> bool {
    store.get_scheduled_job(&job).unwrap().unwrap().enabled
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_cleared_pm_has_no_global_authority() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let seat = session(&store, None);
    configure(&store, seat, &[a], 0, "g1").unwrap();
    let pm = appoint_pm(&store, a);
    assert!(store.global_seat_reads_manager(seat, pm).unwrap());
    clear_pm(&store, a, pm);
    assert_eq!(store.global_live_manager(a).unwrap(), None);
    assert!(!store.global_seat_reads_manager(seat, pm).unwrap());
    assert_eq!(
        code(store.global_report_grant(pm).unwrap_err()),
        GLOBAL_REPORT_NOT_AUTHORIZED
    );
    assert_eq!(
        code(store.global_current_manager(a).unwrap_err()),
        GLOBAL_PROJECT_HAS_NO_MANAGER
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn global_authority_is_the_exact_seat_not_its_rotation_successor() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let seat = session(&store, None);
    configure(&store, seat, &[a], 0, "g1").unwrap();
    let successor = Uuid::new_v4();
    let mut row = test_session(successor, PathBuf::from("/tmp/global-manager"));
    row.continued_from = Some(seat);
    store.insert_session(&row).unwrap();
    assert_eq!(
        code(store.global_seat_grant(successor).unwrap_err()),
        GLOBAL_MANAGER_NOT_SEAT
    );
    assert!(store.global_seat_grant(seat).is_ok());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn grant_replace_and_revoke_retire_queued_messages() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let seat = session(&store, None);
    let first = configure(&store, seat, &[a], 0, "g1").unwrap();
    let pm = appoint_pm(&store, a);
    let queued = queue(&store, &first, a, pm, "m1");
    assert!(store.global_message_deliverable(queued).unwrap());
    let second = configure(&store, seat, &[a], 1, "g2").unwrap();
    assert!(
        !enabled(&store, queued),
        "a replaced grant retires its mail"
    );
    assert!(!store.global_message_deliverable(queued).unwrap());
    let fresh = queue(&store, &second, a, pm, "m2");
    store
        .revoke_global_manager(&RevokeGlobalManagerRequestV1 {
            expected_grant_version: second.grant_version,
            idempotency_key: "r".into(),
        })
        .unwrap();
    assert!(!enabled(&store, fresh), "a revoked grant retires its mail");
    assert!(!store.global_message_deliverable(fresh).unwrap());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn pm_displacement_and_clear_retire_queued_mail_and_fail_the_fence() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let seat = session(&store, None);
    let grant = configure(&store, seat, &[a], 0, "g1").unwrap();
    let old_pm = appoint_pm(&store, a);
    let queued = queue(&store, &grant, a, old_pm, "m1");
    let new_pm = appoint_pm(&store, a);
    assert!(
        !enabled(&store, queued),
        "a displaced PM gets no queued mail"
    );
    assert!(!store.global_message_deliverable(queued).unwrap());
    let current = queue(&store, &grant, a, new_pm, "m2");
    assert!(store.global_message_deliverable(current).unwrap());
    clear_pm(&store, a, new_pm);
    assert!(
        !enabled(&store, current),
        "a cleared PM gets no queued mail"
    );
    assert!(!store.global_message_deliverable(current).unwrap());
    // A job that is not a global message is never fenced.
    assert!(store.global_message_deliverable(Uuid::new_v4()).unwrap());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn report_up_fence_requires_the_exact_current_seat() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let seat = session(&store, None);
    let grant = configure(&store, seat, &[a], 0, "g1").unwrap();
    let pm = appoint_pm(&store, a);
    let report = store
        .queue_global_message(&GlobalMessage {
            grant: &grant,
            direction: GlobalMessageDirection::ToGlobal,
            project_id: a,
            sender: pm,
            target: seat,
            idempotency_key: "r1",
            request: serde_json::json!({"message": "done"}),
            delivery: "done".into(),
        })
        .unwrap();
    assert!(store.global_message_deliverable(report.message_id).unwrap());
    // The seat rotated: v0 authority does not follow, so the report holds no
    // delivery to the successor.
    let mut successor = test_session(Uuid::new_v4(), PathBuf::from("/tmp/global-manager"));
    successor.continued_from = Some(seat);
    store.insert_session(&successor).unwrap();
    assert!(!store.global_message_deliverable(report.message_id).unwrap());
}

use crate::store::manager_actions::fence::{ContinuationAuthorityV1, ContinuationFenceV1};

/// The fence a scheduled continuation of `tip` carries.
fn fence_for(tip: Uuid) -> ContinuationFenceV1 {
    ContinuationFenceV1 {
        origin: tip,
        tip,
        epic: None,
        lead_generation: None,
        authority: ContinuationAuthorityV1::Automated,
    }
}

fn claim_code(store: &Store, tip: Uuid, job: Uuid) -> Option<String> {
    store
        .claim_continuation_effect(&fence_for(tip), &[job])
        .err()
        .map(|error| error.to_string())
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn effect_claim_delivers_a_current_message_and_ignores_other_jobs() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let seat = session(&store, None);
    let grant = configure(&store, seat, &[a], 0, "g1").unwrap();
    let pm = appoint_pm(&store, a);
    let queued = queue(&store, &grant, a, pm, "m1");
    assert_eq!(claim_code(&store, pm, queued), None);
    assert!(enabled(&store, queued));
    assert_eq!(
        claim_code(&store, pm, Uuid::new_v4()),
        None,
        "non-global job"
    );
}

/// R1 delta: the grant changes after the scheduler's fence passed but before
/// the effect claim (no retirement ran, modelled by a raw state flip). The
/// claim itself refuses, delivers nothing and retires the row.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn effect_claim_refuses_and_retires_after_a_grant_change_past_the_scheduler_fence() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let seat = session(&store, None);
    let grant = configure(&store, seat, &[a], 0, "g1").unwrap();
    let pm = appoint_pm(&store, a);
    let queued = queue(&store, &grant, a, pm, "m1");
    assert!(
        store.global_message_deliverable(queued).unwrap(),
        "scheduler fence passes"
    );
    store
        .conn
        .execute(
            "UPDATE global_manager_grants SET state='revoked',updated_at=?2 WHERE id=?1",
            params![grant.grant_id.to_string(), stamp()],
        )
        .unwrap();
    let before: String = store
        .conn
        .query_row(
            "SELECT updated_at FROM sessions WHERE id=?1",
            [pm.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let refused = claim_code(&store, pm, queued).expect("the claim refuses");
    assert!(refused.contains("continuation_target_retired"), "{refused}");
    assert!(!enabled(&store, queued), "the row is retired");
    let after: String = store
        .conn
        .query_row(
            "SELECT updated_at FROM sessions WHERE id=?1",
            [pm.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(before, after, "no continuation effect was claimed");
}

/// R1 delta: the PM is displaced between the scheduler fence and the claim;
/// the displaced PM's resume is refused at the claim.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn effect_claim_refuses_a_displaced_pm() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let seat = session(&store, None);
    let grant = configure(&store, seat, &[a], 0, "g1").unwrap();
    let old_pm = appoint_pm(&store, a);
    let queued = queue(&store, &grant, a, old_pm, "m1");
    assert!(store.global_message_deliverable(queued).unwrap());
    appoint_pm(&store, a);
    assert!(claim_code(&store, old_pm, queued).is_some());
    assert!(!enabled(&store, queued));
}

/// R1 delta: a report captured onto a rotation successor of the exact seat
/// (global rotation between the scheduler fence and the lineage capture) is
/// refused at the claim and retired; it never reaches the successor.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn effect_claim_refuses_a_report_resumed_on_a_seat_successor() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let seat = session(&store, None);
    let grant = configure(&store, seat, &[a], 0, "g1").unwrap();
    let pm = appoint_pm(&store, a);
    let report = store
        .queue_global_message(&GlobalMessage {
            grant: &grant,
            direction: GlobalMessageDirection::ToGlobal,
            project_id: a,
            sender: pm,
            target: seat,
            idempotency_key: "r1",
            request: serde_json::json!({"message": "done"}),
            delivery: "done".into(),
        })
        .unwrap()
        .message_id;
    let successor = session(&store, None);
    assert!(claim_code(&store, successor, report).is_some());
    assert!(!enabled(&store, report));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn the_mailbox_is_bounded_per_grant_across_recipients() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let seat = session(&store, None);
    let grant = configure(&store, seat, &[a, b], 0, "g1").unwrap();
    let (pm_a, pm_b) = (appoint_pm(&store, a), appoint_pm(&store, b));
    let half = MAX_PENDING_PER_GRANT / 2;
    for n in 0..half {
        queue(&store, &grant, a, pm_a, &format!("a{n}"));
        queue(&store, &grant, b, pm_b, &format!("b{n}"));
    }
    let full = store
        .queue_global_message(&GlobalMessage {
            grant: &grant,
            direction: GlobalMessageDirection::ToManager,
            project_id: a,
            sender: seat,
            target: pm_a,
            idempotency_key: "over",
            request: serde_json::json!({"message": "over"}),
            delivery: "over".into(),
        })
        .unwrap_err();
    assert_eq!(code(full), GLOBAL_MANAGER_MAILBOX_FULL);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn latest_global_grant_keeps_a_revoked_seat_and_rows_flag_revoked_scopes() {
    let store = Store::open_in_memory().unwrap();
    assert!(store.latest_global_grant().unwrap().is_none());
    let (a, b) = (project(&store, "A"), project(&store, "B"));
    let seat = session(&store, None);
    let grant = configure(&store, seat, &[a, b], 0, "g1").unwrap();
    assert_eq!(
        store.latest_global_grant().unwrap().map(|g| g.state),
        Some("active".to_string())
    );
    appoint_pm(&store, a);
    appoint_pm(&store, b);
    // Revoke B's PM scope: no Epics, no groups, not project-wide.
    let row_version = store.get_harness_manager(b).unwrap().unwrap().row_version;
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: b,
            session_id: store
                .get_harness_manager(b)
                .unwrap()
                .unwrap()
                .current_session_id
                .unwrap(),
            epic_ids: Some(vec![]),
            group_ids: vec![],
            expected_row_version: row_version,
        })
        .unwrap();
    let rows = store.global_project_rows(&grant).unwrap();
    assert_eq!(rows[0].project_id, a);
    assert!(rows[0].manager_session_id.is_some());
    assert!(!rows[0].scope_revoked);
    assert_eq!(rows[1].project_id, b);
    assert!(rows[1].scope_revoked);
    assert_eq!(rows[1].manager_session_id, None);

    store
        .revoke_global_manager(&RevokeGlobalManagerRequestV1 {
            expected_grant_version: grant.grant_version,
            idempotency_key: "r1".into(),
        })
        .unwrap();
    let latest = store.latest_global_grant().unwrap().unwrap();
    assert_eq!(latest.state, "revoked");
    assert_eq!(latest.grant_id, grant.grant_id);
    assert!(store.active_global_grant().unwrap().is_none());
}

/// The reported Koplik 20 → 4 surprise is refused atomically by both RPC
/// store paths, including edits; confirmation retains normal CAS and replay.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn portfolio_caps_require_confirmation_and_overview_reports_chain_minimum() {
    use crate::store::portfolio_nodes::PortfolioGrantor;
    use rsi_common::harness_manager_v2::ConfigureHarnessManagerPolicyRequestV2;
    use rsi_common::portfolio_nodes::{
        ConfigurePortfolioNodeRequestV1, PORTFOLIO_CAP_REDUCTION_CONFIRMATION_REQUIRED,
    };
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "Koplik");
    let b = project(&store, "Other");
    for (project_id, max_active_sessions) in [(a, 20), (b, 2)] {
        appoint_pm(&store, project_id);
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id,
                expected_scope_version: 1,
                expected_policy_version: 0,
                idempotency_key: format!("pm-{project_id}"),
                policy: ManagerPolicyV2 {
                    max_active_sessions,
                    max_created_sessions: 50,
                    ..ManagerPolicyV2::default()
                },
            })
            .unwrap();
    }
    let seat = session(&store, Some(a));
    let mut request = ConfigureGlobalManagerRequestV1 {
        session_id: seat,
        project_ids: vec![a, b],
        allowed_launches: vec![launch()],
        project_policy: ManagerPolicyV2 {
            max_created_sessions: 100,
            ..ManagerPolicyV2::default()
        },
        expected_grant_version: 0,
        idempotency_key: "caps-global".into(),
    };
    let error = code(
        store
            .configure_global_manager(&request, "operator:test")
            .unwrap_err(),
    );
    assert!(
        error.contains(PORTFOLIO_CAP_REDUCTION_CONFIRMATION_REQUIRED),
        "{error}"
    );
    assert!(
        error.contains(&format!("Koplik {a} ({a}): active sessions: 20 → 4")),
        "{error}"
    );
    assert!(store.active_global_grant().unwrap().is_none());
    assert!(store.list_portfolio_nodes(false).unwrap().is_empty());
    assert_eq!(
        store
            .project_effective_resource_caps(a)
            .unwrap()
            .unwrap()
            .max_active_sessions,
        20
    );
    let saved = store
        .configure_global_manager_confirmed(&request, "operator:test", true)
        .unwrap();
    // A replay needs no new approval and preserves the saved grant.
    assert_eq!(
        store
            .configure_global_manager(&request, "operator:test")
            .unwrap(),
        saved
    );
    let rows = store.global_project_rows(&saved).unwrap();
    assert_eq!(
        rows[0]
            .policy
            .as_ref()
            .unwrap()
            .effective_caps
            .as_ref()
            .unwrap()
            .max_active_sessions,
        4
    );
    assert_eq!(
        rows[1]
            .policy
            .as_ref()
            .unwrap()
            .effective_caps
            .as_ref()
            .unwrap()
            .max_active_sessions,
        2
    );
    let node = store.list_portfolio_nodes(false).unwrap().remove(0);
    let mut edit = ConfigurePortfolioNodeRequestV1 {
        node_id: Some(node.node_id),
        parent_node_id: None,
        adopt_node_ids: vec![],
        expected_parent_grant_version: None,
        tier_label: "global".into(),
        seat_session_id: seat,
        project_ids: vec![a, b],
        allowed_launches: vec![launch()],
        policy: request.project_policy.clone(),
        child_policy: None,
        max_direct_reports: node.max_direct_reports,
        expected_node_grant_version: saved.grant_version,
        expected_authority_epoch: node.authority_epoch,
        idempotency_key: "caps-edit".into(),
    };
    edit.policy.max_active_sessions = 3;
    let error = code(
        store
            .configure_portfolio_node(&edit, PortfolioGrantor::Operator, "operator:test")
            .unwrap_err(),
    );
    assert!(error.contains("active sessions: 4 → 3"), "{error}");
    assert_eq!(
        store.get_portfolio_node(node.node_id).unwrap().unwrap(),
        node
    );
    let saved_edit = store
        .configure_portfolio_node_confirmed(
            &edit,
            PortfolioGrantor::Operator,
            "operator:test",
            true,
        )
        .unwrap();
    assert_eq!(
        store
            .project_effective_resource_caps(a)
            .unwrap()
            .unwrap()
            .max_active_sessions,
        3
    );
    // Raising a grant must replace its old ceiling, rather than intersect it.
    edit.policy.max_active_sessions = 30;
    edit.expected_node_grant_version = saved_edit.grant.grant_version;
    edit.expected_authority_epoch = saved_edit.authority_epoch;
    edit.idempotency_key = "caps-raise".into();
    let raised = store
        .configure_portfolio_node(&edit, PortfolioGrantor::Operator, "operator:test")
        .unwrap();
    assert_eq!(
        store
            .project_effective_resource_caps(a)
            .unwrap()
            .unwrap()
            .max_active_sessions,
        20
    );
    // Confirmation never bypasses stale-version protection.
    request.expected_grant_version = saved.grant_version;
    request.idempotency_key = "caps-stale".into();
    assert!(
        store
            .configure_global_manager_confirmed(&request, "operator:test", true)
            .is_err()
    );
    assert_eq!(
        store.get_portfolio_node(node.node_id).unwrap().unwrap(),
        raised
    );
}
