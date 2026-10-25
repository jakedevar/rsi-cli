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
        observed_restart_cut: None,
        self_wake_event_sequence: None,
    }
}

/// A continuation that emits no event still invalidates the restart cursor.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn restart_cut_claim_refuses_an_invocation_change_without_an_event() {
    let store = Store::open_in_memory().unwrap();
    let tip = session(&store, None);
    let (observed, cursor) = store.restart_cut_observation(tip).unwrap().unwrap();
    assert_eq!(observed.status, cursor.status);
    assert_eq!(observed.stop_reason, cursor.stop_reason);
    let mut fence = fence_for(tip);
    fence.observed_restart_cut = Some(cursor.clone());
    store.claim_continuation_effect(&fence, &[]).unwrap();
    store
        .set_session_model_invocation(tip, Some(Uuid::new_v4()))
        .unwrap();
    assert_eq!(
        store.conversation_max_sequence(tip).unwrap(),
        cursor.event_sequence
    );
    let refused = store.claim_continuation_effect(&fence, &[]).unwrap_err();
    assert_eq!(
        crate::store::manager_actions::fence::continuation_fence_code(&refused),
        Some(crate::store::manager_actions::fence::CONTINUATION_TURN_CHANGED)
    );
}

/// Changes to classification alone must not authorize a stale restart resume.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn restart_cut_claim_revalidates_status_and_stop_reason() {
    for change_status in [false, true] {
        let store = Store::open_in_memory().unwrap();
        let tip = session(&store, None);
        let (_, cursor) = store.restart_cut_observation(tip).unwrap().unwrap();
        let mut fence = fence_for(tip);
        fence.observed_restart_cut = Some(cursor.clone());
        if change_status {
            store
                .update_session_status(tip, rsi_common::types::SessionStatus::Completed)
                .unwrap();
        } else {
            store
                .conn
                .execute(
                    "UPDATE sessions SET stop_reason='operator_interrupt' WHERE id=?1",
                    [tip.to_string()],
                )
                .unwrap();
        }
        assert_eq!(
            store.conversation_max_sequence(tip).unwrap(),
            cursor.event_sequence
        );
        let refused = store.claim_continuation_effect(&fence, &[]).unwrap_err();
        assert_eq!(
            crate::store::manager_actions::fence::continuation_fence_code(&refused),
            Some(crate::store::manager_actions::fence::CONTINUATION_TURN_CHANGED)
        );
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
            portfolio_nodes::PortfolioGrantor::Operator,
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

/// #1902: a new global node after a revoke keeps the last revoked root's
/// direct-report limit and child policy over the same coverage; other
/// coverage starts from the defaults.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn new_global_node_after_a_revoke_inherits_the_last_grants_node_policy() {
    let store = Store::open_in_memory().unwrap();
    let (a, b, c) = (
        project(&store, "A"),
        project(&store, "B"),
        project(&store, "C"),
    );
    let mut child = ManagerPolicyV2::default();
    child.max_active_sessions = 3;
    let first = store
        .configure_portfolio_node(
            &ConfigurePortfolioNodeRequestV1 {
                node_id: None,
                parent_node_id: None,
                adopt_node_ids: Vec::new(),
                expected_parent_grant_version: None,
                tier_label: "global".into(),
                seat_session_id: session(&store, None),
                project_ids: vec![a, b],
                allowed_launches: vec![launch()],
                policy: ManagerPolicyV2::default(),
                child_policy: Some(child.clone()),
                max_direct_reports: 11,
                expected_node_grant_version: 0,
                expected_authority_epoch: 0,
                idempotency_key: "n1".into(),
            },
            portfolio_nodes::PortfolioGrantor::Operator,
            "operator:test",
        )
        .unwrap();
    store
        .revoke_global_manager(&RevokeGlobalManagerRequestV1 {
            expected_grant_version: first.grant.grant_version,
            idempotency_key: "r1".into(),
        })
        .unwrap();
    let other = configure(&store, session(&store, None), &[c], 0, "other").unwrap();
    let other_node = store
        .list_portfolio_nodes(false)
        .unwrap()
        .into_iter()
        .find(|node| node.grant.grant_id == other.grant_id)
        .unwrap();
    assert_eq!(other_node.max_direct_reports, 5);
    assert_eq!(other_node.child_policy, None);
    store
        .revoke_global_manager(&RevokeGlobalManagerRequestV1 {
            expected_grant_version: other.grant_version,
            idempotency_key: "r2".into(),
        })
        .unwrap();

    let replacement = configure(&store, session(&store, None), &[b, a], 0, "again").unwrap();
    let node = store
        .list_portfolio_nodes(false)
        .unwrap()
        .into_iter()
        .find(|node| node.grant.grant_id == replacement.grant_id)
        .unwrap();
    assert_eq!(node.max_direct_reports, 11);
    assert_eq!(node.child_policy, Some(child));
}

/// #2560: the next PM seat of `project`: a same-project, same-parent successor
/// of `predecessor`, published through the rotation receipt the way a
/// committed succession or a context-cap rotation publishes it.
#[cfg(test)]
fn rotate_pm(store: &Store, project: Uuid, predecessor: Uuid) -> Uuid {
    let successor = Uuid::new_v4();
    let mut row = test_session(successor, PathBuf::from("/tmp/global-manager"));
    row.project_id = Some(project);
    row.continued_from = Some(predecessor);
    row.rotation_depth = store
        .get_session(predecessor)
        .unwrap()
        .unwrap()
        .rotation_depth
        + 1;
    store.insert_session(&row).unwrap();
    store
        .conn
        .execute(
            "UPDATE sessions SET status='Archived' WHERE id=?1",
            [predecessor.to_string()],
        )
        .unwrap();
    assert!(
        store
            .record_harness_manager_rotation(predecessor, successor)
            .unwrap()
    );
    successor
}

#[cfg(test)]
fn watch_on(store: &Store, owner: Uuid, watched: Uuid) -> Uuid {
    use crate::store_support::schedule_wake_job::{ScheduleWakeRequest, build_agent_scheduled_job};
    let job = build_agent_scheduled_job(ScheduleWakeRequest {
        message: "the PM ended".into(),
        in_seconds: Some(600),
        at: None,
        name: None,
        every_seconds: None,
        mode: Some("on_terminal".into()),
        working_dir: PathBuf::from("/tmp/global-manager"),
        provider: None,
        model: None,
        project_id: None,
        origin_session_id: Some(owner),
        watch_session_id: Some(watched),
    })
    .unwrap();
    store.insert_scheduled_job(&job).unwrap();
    job.id
}

#[cfg(test)]
fn enabled_watches_of(store: &Store, owner: Uuid) -> Vec<(Uuid, String)> {
    let mut statement = store
        .conn
        .prepare(
            "SELECT id, wake_mode FROM scheduled_jobs
             WHERE enabled=1 AND wake_session_id=?1 AND wake_mode LIKE 'on_terminal:%'",
        )
        .unwrap();
    statement
        .query_map([owner.to_string()], |r| {
            Ok((Uuid::parse_str(&r.get::<_, String>(0)?).unwrap(), r.get(1)?))
        })
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}

/// #2560: three routine seat handovers leave the parent holding exactly one
/// watch on the slot, the one it armed, now pointing at the live seat; a
/// retired seat of the slot stays readable by the parent, a stranger does not.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_parents_watch_follows_the_pm_slot_across_three_rotations_2560() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let seat = session(&store, None);
    configure(&store, seat, &[a], 0, "g1").unwrap();
    let first = appoint_pm(&store, a);
    let watch = watch_on(&store, seat, first);

    let second = rotate_pm(&store, a, first);
    let third = rotate_pm(&store, a, second);
    let fourth = rotate_pm(&store, a, third);

    assert_eq!(
        enabled_watches_of(&store, seat),
        vec![(watch, format!("on_terminal:{fourth}"))],
        "one watch for the slot, the same job, on the live seat"
    );
    assert_eq!(store.global_live_manager(a).unwrap(), Some(fourth));
    for retired in [first, second, third, fourth] {
        assert!(store.global_seat_reads_manager(seat, retired).unwrap());
    }
    let stranger = session(&store, Some(a));
    assert!(!store.global_seat_reads_manager(seat, stranger).unwrap());
    let other_project = appoint_pm(&store, project(&store, "B"));
    assert!(
        !store
            .global_seat_reads_manager(seat, other_project)
            .unwrap()
    );
}

/// #2560: re-arming is idempotent per slot. A watch the parent already holds
/// on the successor wins; the watch on the retired seat is disabled, never
/// deleted, and a watch on the slot armed by a different parent is untouched.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_duplicate_slot_watch_collapses_and_other_parents_keep_theirs_2560() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let seat = session(&store, None);
    let other_parent = session(&store, None);
    configure(&store, seat, &[a], 0, "g1").unwrap();
    let first = appoint_pm(&store, a);
    let stale = watch_on(&store, seat, first);
    let theirs = watch_on(&store, other_parent, first);
    let second = rotate_pm(&store, a, first);
    // The parent re-arms on the new seat by hand, as the playbook used to say.
    let manual = watch_on(&store, seat, second);
    let third = rotate_pm(&store, a, second);
    assert_eq!(
        enabled_watches_of(&store, seat).len(),
        1,
        "one watch per slot"
    );
    assert!(
        enabled(&store, manual) || enabled(&store, stale),
        "a duplicate is disabled, the other survives"
    );
    assert_eq!(
        enabled_watches_of(&store, seat)[0].1,
        format!("on_terminal:{third}")
    );
    assert_eq!(
        enabled_watches_of(&store, other_parent),
        vec![(theirs, format!("on_terminal:{third}"))]
    );
    for job in [stale, manual] {
        assert!(
            store.get_scheduled_job(&job).unwrap().is_some(),
            "disabled, never deleted"
        );
    }
}

/// #2560: a Completed seat that is mid-handover (a rotation entered and not
/// finished) is not a slot left empty; once the handover completes, is
/// refused, or outlives the grace window, the seat's own terminal counts.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_completed_seat_mid_handover_holds_its_parents_watch_2560() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let pm = appoint_pm(&store, a);
    let mut completed = store.get_session(pm).unwrap().unwrap();
    completed.status = SessionStatus::Completed;
    let pending = |store: &Store, session: &rsi_common::types::Session| {
        store.portfolio_terminal_watch_pending(session).unwrap()
    };
    assert!(!pending(&store, &completed), "no handover, no hold");

    store
        .insert_rotation_event(pm, "r1", "writing_handoff", "entered", None)
        .unwrap();
    assert!(pending(&store, &completed), "handover in flight");
    assert!(
        !store
            .manager_seat_change_in_flight(pm, chrono::Duration::zero())
            .unwrap(),
        "an expired grace window stops holding"
    );
    let mut interrupted = completed.clone();
    interrupted.status = SessionStatus::Interrupted;
    assert!(
        !pending(&store, &interrupted),
        "an Interrupted or Failed seat wakes the parent at once"
    );

    store
        .insert_rotation_event(pm, "r1", "completed", "completed", None)
        .unwrap();
    assert!(!pending(&store, &completed), "handover finished");
    store
        .insert_rotation_event(pm, "r2", "writing_handoff", "entered", None)
        .unwrap();
    store
        .insert_rotation_event(pm, "r2", "refused", "refused:no_handoff", None)
        .unwrap();
    assert!(!pending(&store, &completed), "refused handover");
}

/// #118: an explicit reappointment of a fresh, parentless session starts a
/// new lineage; the new holder still reads the displaced appointment's seats
/// (the durable `harness_manager_notice_scope_retirements` record), and
/// nothing else: not another project's displaced seat, not a stranger, and a
/// displaced seat gains no reach of its own.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_reappointed_pm_reads_its_projects_displaced_seats_only_118() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let b = project(&store, "B");
    let first = appoint_pm(&store, a);
    let second = rotate_pm(&store, a, first);
    let other = appoint_pm(&store, b);
    let seat = appoint_pm(&store, a);
    let other_seat = appoint_pm(&store, b);
    assert_eq!(
        store
            .get_harness_manager(a)
            .unwrap()
            .unwrap()
            .current_session_id,
        Some(seat)
    );

    assert!(store.manager_seat_reads_predecessor(seat, second).unwrap());
    assert!(store.manager_seat_reads_predecessor(seat, first).unwrap());
    assert!(
        store
            .manager_seat_reads_predecessor(other_seat, other)
            .unwrap()
    );
    assert!(!store.manager_seat_reads_predecessor(seat, other).unwrap());
    assert!(
        !store
            .manager_seat_reads_predecessor(other_seat, first)
            .unwrap()
    );
    assert!(!store.manager_seat_reads_predecessor(second, seat).unwrap());
    assert!(!store.manager_seat_reads_predecessor(second, first).unwrap());
    let stranger = session(&store, Some(a));
    assert!(
        !store
            .manager_seat_reads_predecessor(stranger, first)
            .unwrap()
    );

    // The next reappointment reads every earlier appointment; the displaced
    // holder reads nothing any more.
    let third_seat = appoint_pm(&store, a);
    for earlier in [first, second, seat] {
        assert!(
            store
                .manager_seat_reads_predecessor(third_seat, earlier)
                .unwrap()
        );
    }
    assert!(!store.manager_seat_reads_predecessor(seat, first).unwrap());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn predecessor_appointments_stay_index_bounded_over_large_retirement_history() {
    use rusqlite::StatementStatus;

    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let b = project(&store, "B");
    let seat = appoint_pm(&store, a);
    let anchors: Vec<_> = (0..40).map(|_| session(&store, Some(a))).collect();
    for (i, anchor) in anchors.iter().enumerate() {
        store
            .conn
            .execute(
                "INSERT INTO harness_manager_notice_scope_retirements
                (project_id,manager_session_id,scope_version,retired_at)
             VALUES(?1,?2,1,?3)",
                params![
                    a.to_string(),
                    anchor.to_string(),
                    format!("2026-01-01T00:00:00.{i:09}Z")
                ],
            )
            .unwrap();
    }
    // A tie at the cutoff keeps the same manager-id ordering as GROUP BY.
    store
        .conn
        .execute(
            "INSERT INTO harness_manager_notice_scope_retirements
            (project_id,manager_session_id,scope_version,retired_at)
         VALUES(?1,?2,2,'2026-01-01T00:00:00.000000008Z')",
            params![a.to_string(), anchors[7].to_string()],
        )
        .unwrap();
    // The current anchor must be excluded before LIMIT, even when retired
    // scope generations of its own are the newest records in the project.
    store
        .conn
        .execute(
            "INSERT INTO harness_manager_notice_scope_retirements
            (project_id,manager_session_id,scope_version,retired_at)
         VALUES(?1,?2,1,'2026-01-03T00:00:00.000000000Z')",
            params![a.to_string(), seat.to_string()],
        )
        .unwrap();

    let arguments = params![
        a.to_string(),
        seat.to_string(),
        PREDECESSOR_APPOINTMENTS_MAX
    ];
    let bounded_read = || {
        let mut statement = store.conn.prepare(PREDECESSOR_APPOINTMENTS_SQL).unwrap();
        let rows = statement
            .query_map(arguments, |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(statement.get_status(StatementStatus::FullscanStep), 0);
        assert_eq!(statement.get_status(StatementStatus::Sort), 0);
        let steps = statement.get_status(StatementStatus::VmStep);
        assert!(
            steps > 0 && steps < 1000,
            "bounded predecessor read: {steps} VM steps"
        );
        (rows, steps)
    };
    let (before, before_steps) = bounded_read();

    let tx = store.conn.unchecked_transaction().unwrap();
    // 20,000 older distinct appointments, plus 20,000 newer rows in a
    // different project. None may increase the bounded read's work.
    for project in [a, b] {
        store.conn.execute(
            "WITH RECURSIVE history(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM history WHERE n<20000)
             INSERT INTO harness_manager_notice_scope_retirements
                (project_id,manager_session_id,scope_version,retired_at)
             SELECT ?1,printf('%08x-0000-4000-8000-000000000000',n),1,?2 FROM history",
            params![project.to_string(), if project == a {
                "2025-01-01T00:00:00.000000000Z"
            } else {
                "2027-01-01T00:00:00.000000000Z"
            }],
        ).unwrap();
    }
    // Thousands of scope edits under one recent appointment would crowd
    // out the other 31 anchors if LIMIT were applied to raw retirements.
    for (anchor, retired_at) in [
        (anchors[39], "2026-01-02T00:00:00.000000000Z"),
        (seat, "2026-01-03T00:00:00.000000000Z"),
    ] {
        store.conn.execute(
            "WITH RECURSIVE scopes(n) AS (VALUES(2) UNION ALL SELECT n+1 FROM scopes WHERE n<2001)
             INSERT INTO harness_manager_notice_scope_retirements
                (project_id,manager_session_id,scope_version,retired_at)
             SELECT ?1,?2,n,?3 FROM scopes",
            params![a.to_string(), anchor.to_string(), retired_at],
        ).unwrap();
    }
    tx.commit().unwrap();
    let (after, after_steps) = bounded_read();
    let legacy: Vec<String> = store
        .conn
        .prepare(
            "SELECT manager_session_id FROM harness_manager_notice_scope_retirements
         WHERE project_id=?1 AND manager_session_id<>?2
         GROUP BY manager_session_id ORDER BY MAX(retired_at) DESC LIMIT ?3",
        )
        .unwrap()
        .query_map(arguments, |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(after, legacy);
    assert_eq!(after, before);
    assert_eq!(after.len(), PREDECESSOR_APPOINTMENTS_MAX as usize);
    assert_eq!(
        after_steps, before_steps,
        "history growth must not grow read work"
    );
    for anchor in &anchors {
        assert_eq!(
            store.manager_seat_reads_predecessor(seat, *anchor).unwrap(),
            after.contains(&anchor.to_string())
        );
    }

    let plan: Vec<String> = store
        .conn
        .prepare(&format!(
            "EXPLAIN QUERY PLAN {PREDECESSOR_APPOINTMENTS_SQL}"
        ))
        .unwrap()
        .query_map(arguments, |row| row.get(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(plan.len(), 1, "one bounded index search: {plan:?}");
    assert!(plan[0].contains("SEARCH harness_manager_retired_appointments USING COVERING INDEX harness_manager_retired_appointments_recent (project_id=?)"), "{plan:?}");
    assert!(
        !plan
            .iter()
            .any(|step| step.contains("SCAN") || step.contains("TEMP B-TREE")),
        "{plan:?}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn retired_appointments_migration_backfills_and_tracks_latest_retirement() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let b = project(&store, "B");
    let anchor = session(&store, Some(a));
    let latest = "2026-01-03T00:00:00.000000000Z";
    crate::store::tests::rewind_store_to_schema_version(&store.conn, 183);
    let old_catalog = crate::store::catalog_sql_by_name(&store.conn);
    let insert = |project: Uuid, version: i64, stamp: &str| {
        store
            .conn
            .execute(
                "INSERT OR IGNORE INTO harness_manager_notice_scope_retirements
                (project_id,manager_session_id,scope_version,retired_at)
             VALUES(?1,?2,?3,?4)",
                params![project.to_string(), anchor.to_string(), version, stamp],
            )
            .unwrap();
    };
    insert(a, 1, latest);
    insert(a, 2, "2026-01-01T00:00:00.000000000Z");
    insert(b, 1, "2026-01-02T00:00:00.000000000Z");
    store.init_schema().unwrap();
    crate::store::assert_catalog_preserved_across_replay(&old_catalog, &store.conn, 183);
    let check_projection = || {
        let projected: Vec<(String, String, String)> = store.conn.prepare(
            "SELECT project_id,manager_session_id,retired_at FROM harness_manager_retired_appointments
             ORDER BY project_id,manager_session_id"
        ).unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap().collect::<rusqlite::Result<_>>().unwrap();
        let historical: Vec<(String, String, String)> = store.conn.prepare(
            "SELECT project_id,manager_session_id,MAX(retired_at) FROM harness_manager_notice_scope_retirements
             GROUP BY project_id,manager_session_id ORDER BY project_id,manager_session_id"
        ).unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(projected, historical);
        assert_eq!(projected.len(), 2);
    };
    check_projection();
    // Older, equal, replayed, then newer retirement timestamps.
    insert(a, 3, "2026-01-02T00:00:00.000000000Z");
    insert(a, 4, latest);
    insert(a, 1, "2026-01-09T00:00:00.000000000Z");
    check_projection();
    insert(a, 5, "2026-01-04T00:00:00.000000000Z");
    store.conn.execute(
        "UPDATE harness_manager_notice_scope_retirements SET completed_at=?1 WHERE project_id=?2",
        params!["2026-01-05T00:00:00.000000000Z", a.to_string()],
    ).unwrap();
    check_projection();
    let tx = store.conn.unchecked_transaction().unwrap();
    insert(a, 6, "2026-01-06T00:00:00.000000000Z");
    tx.rollback().unwrap();
    check_projection();
    store.init_schema().unwrap();
    check_projection();
}

/// #2531: the live holder of a PM slot reads earlier seats of that slot, and
/// nobody else: not a later seat, itself, a sibling project's seat, a
/// stranger, or any seat once the appointment is revoked.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_pm_successor_reads_earlier_seats_of_its_slot_only_2531() {
    let store = Store::open_in_memory().unwrap();
    let a = project(&store, "A");
    let first = appoint_pm(&store, a);
    let second = rotate_pm(&store, a, first);
    let third = rotate_pm(&store, a, second);

    assert!(store.manager_seat_reads_predecessor(third, second).unwrap());
    assert!(store.manager_seat_reads_predecessor(third, first).unwrap());
    // Only the live holder reads: a retired seat reaches nothing, even earlier ones.
    assert!(!store.manager_seat_reads_predecessor(second, first).unwrap());
    // Not itself, and never forward in the lineage: a retired seat holds nothing.
    assert!(!store.manager_seat_reads_predecessor(third, third).unwrap());
    assert!(!store.manager_seat_reads_predecessor(first, second).unwrap());
    assert!(!store.manager_seat_reads_predecessor(second, third).unwrap());
    // A stranger in the project, and a sibling project's slot.
    let stranger = session(&store, Some(a));
    assert!(
        !store
            .manager_seat_reads_predecessor(third, stranger)
            .unwrap()
    );
    assert!(
        !store
            .manager_seat_reads_predecessor(stranger, first)
            .unwrap()
    );
    let b = project(&store, "B");
    let other = appoint_pm(&store, b);
    let other_next = rotate_pm(&store, b, other);
    assert!(!store.manager_seat_reads_predecessor(third, other).unwrap());
    assert!(
        !store
            .manager_seat_reads_predecessor(other_next, first)
            .unwrap()
    );
    assert!(
        store
            .manager_seat_reads_predecessor(other_next, other)
            .unwrap()
    );
}
