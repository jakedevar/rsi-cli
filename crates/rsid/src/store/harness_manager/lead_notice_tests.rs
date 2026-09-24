//! #404 S1: lead-origin manager notices are direction-correct in the existing
//! message store. No verb yet; these tests drive the store function directly.
#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use super::*;
use crate::session::agent_verbs::tests::test_session;
use crate::store::harness_manager_v2::ManagerAuthorityV2;
use rsi_common::harness_manager_v2::{ConfigureHarnessManagerPolicyRequestV2, ManagerPolicyV2};
use rsi_common::types::Project;
use std::path::PathBuf;

struct Fixture {
    store: Store,
    project: Uuid,
    manager: Uuid,
    epics: [Uuid; 2],
    leads: [Uuid; 2],
    worker: Uuid,
}

/// Appointed project-wide manager (the #404 live shape), two Epics with
/// current leads, and one non-lead worker under Epic 0. The manager is busy.
fn fixture() -> Fixture {
    let store = Store::open_in_memory().unwrap();
    let project = Uuid::new_v4();
    let now = Utc::now();
    store
        .insert_project(&Project {
            id: project,
            name: "Lead notice".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();
    let mut manager = test_session(Uuid::new_v4(), PathBuf::from("/tmp/lead-notice"));
    manager.session_kind = SessionKind::Standard;
    manager.project_id = Some(project);
    manager.status = SessionStatus::Running;
    store.insert_session(&manager).unwrap();
    let mut group = manager.clone();
    group.id = Uuid::new_v4();
    group.session_kind = SessionKind::Group;
    group.status = SessionStatus::Completed;
    store.insert_session(&group).unwrap();
    let epics = [Uuid::new_v4(), Uuid::new_v4()];
    let leads = [Uuid::new_v4(), Uuid::new_v4()];
    for index in 0..2 {
        let mut epic = group.clone();
        epic.id = epics[index];
        epic.session_kind = SessionKind::Epic;
        epic.parent_id = Some(group.id);
        store.insert_session(&epic).unwrap();
        let mut lead = manager.clone();
        lead.id = leads[index];
        lead.session_kind = SessionKind::Feature;
        lead.parent_id = Some(epic.id);
        store.insert_session(&lead).unwrap();
        store.set_lead_session(epic.id, Some(lead.id)).unwrap();
    }
    let mut worker = manager.clone();
    worker.id = Uuid::new_v4();
    worker.session_kind = SessionKind::Feature;
    worker.parent_id = Some(epics[0]);
    store.insert_session(&worker).unwrap();
    let config = store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: project,
            session_id: manager.id,
            epic_ids: None,
            expected_row_version: 0,
        })
        .unwrap();
    assert!(config.epic_ids.contains(&epics[0]) && config.epic_ids.contains(&epics[1]));
    Fixture {
        store,
        project,
        manager: manager.id,
        epics,
        leads,
        worker: worker.id,
    }
}

fn counts(store: &Store) -> (i64, i64) {
    let count = |table: &str| {
        store
            .conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    };
    (
        count("harness_manager_messages"),
        count("harness_manager_notices"),
    )
}

fn rotate(store: &Store, from: Uuid) -> Uuid {
    let mut next = store.get_session(from).unwrap().unwrap();
    next.id = Uuid::new_v4();
    next.continued_from = Some(from);
    next.rotation_depth += 1;
    store.insert_session(&next).unwrap();
    store
        .update_session_status(from, SessionStatus::Archived)
        .unwrap();
    store
        .record_harness_manager_rotation(from, next.id)
        .unwrap();
    next.id
}

fn inbox(store: &Store, caller: Uuid) -> AgentManagerInboxResultV1 {
    store
        .manager_inbox(caller, &AgentManagerInboxRequestV1::default())
        .unwrap()
}

fn send(store: &Store, manager: Uuid, epic: Uuid, key: &str) -> HarnessManagerMessageReceiptV1 {
    store
        .manager_send(
            manager,
            &AgentManagerSendRequestV1 {
                epic_id: epic,
                message: format!("Request {key}"),
                idempotency_key: key.into(),
            },
        )
        .unwrap()
}

fn code(result: Result<HarnessManagerMessageReceiptV1>) -> String {
    match result {
        Err(DaemonError::InvalidParam(code)) => code,
        other => panic!("expected typed refusal, got {other:?}"),
    }
}

/// Exact #404 shape: appointed project-wide manager, current Epic lead, no
/// prior request, operator-directed program notification.
#[test]
fn lead_notice_reaches_busy_project_manager_without_prior_request() {
    let f = fixture();
    assert!(inbox(&f.store, f.leads[0]).messages.is_empty());
    let text = "Operator-directed: remaining harness-efficiency program work is ready.";
    let receipt = f
        .store
        .manager_lead_notice(f.leads[0], text, "program-1")
        .unwrap();
    assert!(!receipt.deduplicated);
    assert_eq!(receipt.request_id, None);

    let read = inbox(&f.store, f.manager);
    let message = read
        .messages
        .iter()
        .find(|m| m.message_id == receipt.message_id)
        .unwrap();
    assert_eq!(message.sender_session_id, f.leads[0]);
    assert_eq!(message.recipient_session_id, f.manager);
    assert_eq!(message.epic_id, f.epics[0]);
    assert_eq!(message.message, text);
    assert_eq!(message.request_id, None);
    assert!(!message.replied);
    let notice = read
        .notices
        .iter()
        .find(|n| n.kind == "message" && n.subject_id == receipt.message_id.to_string())
        .unwrap();
    assert_eq!(notice.direction, "to_manager");
    assert_eq!(notice.recipient_session_id, f.manager);
    assert_eq!(notice.source_session_id, Some(f.leads[0]));
    // Reading mail never interrupts the busy manager's turn.
    assert_eq!(
        f.store.get_session(f.manager).unwrap().unwrap().status,
        SessionStatus::Running
    );

    // Exact replay deduplicates; changed content under the key conflicts.
    let replay = f
        .store
        .manager_lead_notice(f.leads[0], text, "program-1")
        .unwrap();
    assert_eq!(replay.message_id, receipt.message_id);
    assert_eq!(replay.sequence, receipt.sequence);
    assert!(replay.deduplicated);
    let before = counts(&f.store);
    assert_eq!(
        code(
            f.store
                .manager_lead_notice(f.leads[0], "changed", "program-1")
        ),
        "manager_idempotency_conflict"
    );
    assert_eq!(counts(&f.store), before);

    // The sender reads its own notice as acknowledgement, without gaining
    // manager authority.
    let own = inbox(&f.store, f.leads[0]);
    assert_eq!(own.messages[0].message_id, receipt.message_id);
    assert_eq!(
        f.store
            .manager_progress(f.leads[0])
            .unwrap_err()
            .to_string(),
        refused("manager_scope_denied").to_string()
    );
    assert!(
        f.store
            .manager_send(
                f.leads[0],
                &AgentManagerSendRequestV1 {
                    epic_id: f.epics[1],
                    message: "not a manager".into(),
                    idempotency_key: "x".into(),
                }
            )
            .is_err()
    );
}

#[test]
fn lead_notice_follows_manager_and_lead_rotation() {
    let f = fixture();
    let receipt = f
        .store
        .manager_lead_notice(f.leads[0], "status: blocked on locks", "n1")
        .unwrap();
    let manager = rotate(&f.store, f.manager);
    assert!(
        inbox(&f.store, manager)
            .messages
            .iter()
            .any(|m| m.message_id == receipt.message_id)
    );
    let lead = rotate(&f.store, f.leads[0]);
    f.store.set_lead_session(f.epics[0], Some(lead)).unwrap();
    assert!(
        inbox(&f.store, manager)
            .messages
            .iter()
            .any(|m| m.message_id == receipt.message_id)
    );
    // The successor lead inherits the acknowledgement view and may notify.
    assert!(
        inbox(&f.store, lead)
            .messages
            .iter()
            .any(|m| m.message_id == receipt.message_id)
    );
    let second = f
        .store
        .manager_lead_notice(lead, "status: unblocked", "n2")
        .unwrap();
    let read = inbox(&f.store, manager);
    let message = read
        .messages
        .iter()
        .find(|m| m.message_id == second.message_id)
        .unwrap();
    assert_eq!(message.recipient_session_id, manager);
    assert_eq!(message.sender_session_id, lead);
}

#[test]
fn lead_notice_refusals_fail_closed_without_persisting_mail() {
    let f = fixture();
    let before = counts(&f.store);
    // Non-lead worker and the manager itself.
    assert_eq!(
        code(f.store.manager_lead_notice(f.worker, "hi", "k")),
        "manager_scope_denied"
    );
    assert_eq!(
        code(f.store.manager_lead_notice(f.manager, "hi", "k")),
        "manager_notice_requires_feature_lead"
    );
    // Bounded content.
    assert_eq!(
        code(f.store.manager_lead_notice(f.leads[0], "  ", "k")),
        "manager_invalid_message"
    );
    assert_eq!(
        code(f.store.manager_lead_notice(
            f.leads[0],
            &"x".repeat(HARNESS_MANAGER_MAX_MESSAGE_BYTES + 1),
            "k"
        )),
        "manager_invalid_message"
    );
    assert_eq!(
        code(f.store.manager_lead_notice(f.leads[0], "hi", "")),
        "manager_invalid_idempotency_key"
    );
    // Stale lead after rotation, and a replaced (non-lineage) lead.
    let next = rotate(&f.store, f.leads[1]);
    f.store.set_lead_session(f.epics[1], Some(next)).unwrap();
    let before_stale = counts(&f.store);
    assert_eq!(
        code(f.store.manager_lead_notice(f.leads[1], "hi", "k")),
        "manager_current_session_required"
    );
    // A replacement lead is not a committed rotation: the prior lead loses
    // the right to notify.
    let mut replacement = f.store.get_session(next).unwrap().unwrap();
    replacement.id = Uuid::new_v4();
    replacement.continued_from = None;
    f.store.insert_session(&replacement).unwrap();
    f.store
        .set_lead_session(f.epics[1], Some(replacement.id))
        .unwrap();
    assert_eq!(
        code(f.store.manager_lead_notice(next, "hi", "k")),
        "manager_scope_denied"
    );
    f.store.set_lead_session(f.epics[1], Some(next)).unwrap();
    assert_eq!(counts(&f.store), before_stale);
    assert_eq!(before, (0, 0));

    // Out-of-scope lead after the scope narrows to Epic 0.
    let narrowed = f
        .store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: f.project,
            session_id: f.manager,
            epic_ids: Some(vec![f.epics[0]]),
            expected_row_version: 1,
        })
        .unwrap();
    let scoped = counts(&f.store);
    assert_eq!(
        code(f.store.manager_lead_notice(next, "hi", "k")),
        "manager_scope_denied"
    );
    // A notice sent under the narrowed scope dies with the next revocation.
    let sent = f
        .store
        .manager_lead_notice(f.leads[0], "before revoke", "r")
        .unwrap();
    f.store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: f.project,
            session_id: f.manager,
            epic_ids: Some(Vec::new()),
            expected_row_version: narrowed.row_version,
        })
        .unwrap();
    let revoked = counts(&f.store);
    assert_eq!(revoked.0, scoped.0 + 1);
    assert!(
        f.store
            .manager_lead_notice(f.leads[0], "after", "r2")
            .is_err()
    );
    assert_eq!(counts(&f.store), revoked);
    assert!(
        f.store
            .get_manager_message(sent.message_id)
            .unwrap()
            .is_some()
    );

    // Absent manager: a project with no appointment.
    let other = fixture();
    other
        .store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: other.project,
            session_id: other.manager,
            epic_ids: Some(Vec::new()),
            expected_row_version: 1,
        })
        .unwrap();
    let before_absent = counts(&other.store);
    assert!(
        other
            .store
            .manager_lead_notice(other.leads[0], "hi", "k")
            .is_err()
    );
    // Retired manager with no successor.
    let third = fixture();
    third
        .store
        .update_session_status(third.manager, SessionStatus::Archived)
        .unwrap();
    assert_eq!(
        code(third.store.manager_lead_notice(third.leads[0], "hi", "k")),
        "manager_current_session_required"
    );
    assert_eq!(counts(&third.store), (0, 0));
    assert_eq!(counts(&other.store), before_absent);
}

/// Mixed fixture: manager requests, a lead reply and lead notices. Every
/// manager-request projection keeps its exact prior shape, and no lead notice
/// is ever a request, a reply target, or pending-budget weight.
#[test]
fn lead_notices_leave_manager_request_projections_unchanged() {
    let f = fixture();
    let r0 = send(&f.store, f.manager, f.epics[0], "shared-key");
    let r1 = send(&f.store, f.manager, f.epics[1], "r1");
    f.store
        .manager_reply(
            f.leads[0],
            &AgentManagerReplyRequestV1 {
                request_id: r0.message_id,
                message: "done".into(),
                idempotency_key: "reply".into(),
            },
        )
        .unwrap();
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let requests_before =
        serde_json::to_value(f.store.manager_progress(f.manager).unwrap().recent_requests).unwrap();
    let rows_before = serde_json::to_value(
        f.store
            .manager_v2_request_rows(&config, None, "", 32, false)
            .unwrap(),
    )
    .unwrap();
    let inbox_before = serde_json::to_value(inbox(&f.store, f.manager).messages).unwrap();

    // A lead key equal to the manager's request key neither collides with
    // nor hijacks the manager's logical replay.
    let n0 = f
        .store
        .manager_lead_notice(f.leads[0], "fyi 0", "shared-key")
        .unwrap();
    let n1 = f
        .store
        .manager_lead_notice(f.leads[1], "fyi 1", "n1")
        .unwrap();
    let replay = send(&f.store, f.manager, f.epics[0], "shared-key");
    assert!(replay.deduplicated);
    assert_eq!(replay.message_id, r0.message_id);

    assert_eq!(
        serde_json::to_value(f.store.manager_progress(f.manager).unwrap().recent_requests).unwrap(),
        requests_before
    );
    assert_eq!(
        serde_json::to_value(
            f.store
                .manager_v2_request_rows(&config, None, "", 32, false)
                .unwrap()
        )
        .unwrap(),
        rows_before
    );
    // The manager inbox holds the prior mail byte-identically, plus the two
    // notices, which are never marked replied.
    let after = inbox(&f.store, f.manager).messages;
    let prior: Vec<_> = after
        .iter()
        .filter(|m| m.message_id != n0.message_id && m.message_id != n1.message_id)
        .collect();
    assert_eq!(serde_json::to_value(&prior).unwrap(), inbox_before);
    for id in [n0.message_id, n1.message_id] {
        let notice = after.iter().find(|m| m.message_id == id).unwrap();
        assert!(!notice.replied);
    }

    // Reply correlation: a notice is never a reply target.
    assert_eq!(
        code(f.store.manager_reply(
            f.leads[0],
            &AgentManagerReplyRequestV1 {
                request_id: n0.message_id,
                message: "reply to self".into(),
                idempotency_key: "bad".into(),
            },
        )),
        "manager_request_not_in_scope"
    );
    assert!(f.store.manager_request_replied(r0.message_id).unwrap());
    assert!(!f.store.manager_request_replied(r1.message_id).unwrap());
    assert!(!f.store.manager_request_replied(n0.message_id).unwrap());

    // v2 request correlation accepts real requests and refuses notices.
    assert!(
        f.store
            .manager_v2_operator_request_live(&config, f.epics[1], r1.message_id)
            .unwrap()
    );
    assert!(
        !f.store
            .manager_v2_operator_request_live(&config, f.epics[0], n0.message_id)
            .unwrap()
    );
    let policy = f
        .store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: f.project,
            expected_scope_version: config.row_version,
            expected_policy_version: 0,
            idempotency_key: "grant".into(),
            policy: ManagerPolicyV2::default(),
        })
        .unwrap();
    let authority = |caller, is_manager| ManagerAuthorityV2 {
        config: config.clone(),
        grant: policy.clone(),
        caller,
        is_manager,
    };
    assert_eq!(
        f.store
            .manager_v2_request_target(&authority(f.manager, true), r1.message_id)
            .unwrap(),
        f.epics[1]
    );
    assert_eq!(
        f.store
            .manager_v2_request_target(&authority(f.leads[1], false), r1.message_id)
            .unwrap(),
        f.epics[1]
    );
    for (caller, is_manager) in [(f.manager, true), (f.leads[0], false)] {
        assert_eq!(
            f.store
                .manager_v2_request_target(&authority(caller, is_manager), n0.message_id)
                .unwrap_err()
                .to_string(),
            refused("manager_v2_request_out_of_scope").to_string()
        );
    }
}

/// Pending request budget counts manager requests only: lead notices never
/// consume it, and the (#664 per-Epic) budget still refuses at exactly its
/// limit.
#[test]
fn lead_notices_do_not_consume_manager_pending_request_budget() {
    let f = fixture();
    for index in 0..MAX_PENDING_REQUESTS_PER_EPIC - 1 {
        send(&f.store, f.manager, f.epics[0], &format!("r{index}"));
    }
    for index in 0..3 {
        f.store
            .manager_lead_notice(f.leads[index % 2], "fyi", &format!("n{index}"))
            .unwrap();
    }
    send(&f.store, f.manager, f.epics[0], "last");
    let before = counts(&f.store);
    assert_eq!(
        code(f.store.manager_send(
            f.manager,
            &AgentManagerSendRequestV1 {
                epic_id: f.epics[0],
                message: "over".into(),
                idempotency_key: "over".into(),
            },
        )),
        "manager_epic_pending_request_limit: next_action=settle_or_send_notice"
    );
    assert_eq!(counts(&f.store), before);
}

/// Unread lead notices are bounded per Epic; manager retrieval drains them.
#[test]
fn lead_notices_are_bounded_until_the_manager_reads_them() {
    let f = fixture();
    for index in 0..MAX_PENDING_LEAD_NOTICES {
        f.store
            .manager_lead_notice(f.leads[0], "fyi", &format!("n{index}"))
            .unwrap();
    }
    let before = counts(&f.store);
    assert_eq!(
        code(f.store.manager_lead_notice(f.leads[0], "fyi", "overflow")),
        "manager_pending_notice_limit"
    );
    assert_eq!(counts(&f.store), before);
    // Another Epic's lead is unaffected.
    f.store
        .manager_lead_notice(f.leads[1], "fyi", "other")
        .unwrap();
    loop {
        let page = f
            .store
            .manager_inbox(
                f.manager,
                &AgentManagerInboxRequestV1 {
                    limit: 32,
                    ..Default::default()
                },
            )
            .unwrap();
        if !page.more_notices {
            break;
        }
    }
    f.store
        .manager_lead_notice(f.leads[0], "fyi", "overflow")
        .unwrap();
}
