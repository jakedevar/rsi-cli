//! Issue #664 S1: one open-request definition, the permanent lead-terminal
//! release, the replaced-lead orphan sweep and the settled-reply refusal.
//! S2: the per-Epic cap, the project ceiling and capacity surfacing.
//! S3 (#656): standing-request rollover and chain-wide reply replay.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::error::Result;
use crate::store::harness_manager::{
    MAX_REPLIES_PER_REQUEST, REQUEST_RELEASED_KIND, REQUEST_ROLLOVER_KIND, REQUEST_SETTLE_KIND,
    REQUEST_UNSETTLED_KIND,
};
use crate::store::harness_manager_v2::BOOKKEEPING_LIMIT;

/// Per-Epic open-request cap (`MAX_PENDING_REQUESTS_PER_EPIC`); every S1
/// scenario fills one Epic.
const CAP: i64 = 32;
const EPIC_FULL: &str = "manager_epic_pending_request_limit";

fn send(f: &Fixture, key: &str) -> Result<HarnessManagerMessageReceiptV1> {
    f.store.manager_send(
        f.manager,
        &AgentManagerSendRequestV1 {
            epic_id: f.epic,
            message: format!("Request {key}"),
            idempotency_key: key.into(),
        },
    )
}

fn fill(f: &Fixture) -> Vec<Uuid> {
    (0..CAP)
        .map(|index| send(f, &format!("fill-{index}")).unwrap().message_id)
        .collect()
}

fn retrieve(f: &Fixture, lead: Uuid) {
    f.store
        .manager_inbox(lead, &AgentManagerInboxRequestV1::default())
        .unwrap();
}

fn lifecycle(
    f: &Fixture,
    lead: Uuid,
    request_id: Uuid,
    state: ManagerRequestStateV2,
    expected_row_version: i64,
    key: &str,
) -> Result<ManagerMutationReceiptV2> {
    f.store.manager_v2_commit_update(
        lead,
        &request(
            ManagerUpdateV2::Request {
                request_id,
                expected_row_version,
                state,
                message: "Lead status".into(),
                work_key: None,
            },
            key,
        ),
        &LedgerObservation::default(),
    )
}

fn reply(
    f: &Fixture,
    lead: Uuid,
    request_id: Uuid,
    key: &str,
) -> Result<HarnessManagerMessageReceiptV1> {
    f.store.manager_reply(
        lead,
        &AgentManagerReplyRequestV1 {
            request_id,
            message: format!("Reply {key}"),
            idempotency_key: key.into(),
        },
    )
}

fn config(f: &Fixture) -> HarnessManagerConfigV1 {
    f.store.get_harness_manager(f.project).unwrap().unwrap()
}

fn open_total(f: &Fixture) -> i64 {
    f.store.manager_open_request_counts(&config(f)).unwrap().0
}

fn settlement(f: &Fixture, id: Uuid) -> Option<ManagerRecordV2> {
    f.store
        .manager_v2_record(&config(f), REQUEST_SETTLE_KIND, &id.to_string())
        .unwrap()
}

#[track_caller]
fn assert_refused<T: std::fmt::Debug>(result: Result<T>, code: &str) {
    let error = result.expect_err(code).to_string();
    assert!(error.contains(code), "expected {code}, got {error}");
}

/// A legal non-rotation replacement lead (raw operator link): a new Feature
/// under the Epic with no committed rotation from the old lead.
fn raw_replace_lead(f: &Fixture) -> Uuid {
    let mut next = f.store.get_session(f.lead).unwrap().unwrap();
    next.id = Uuid::new_v4();
    next.continued_from = None;
    next.title = Some("Replacement lead".into());
    f.store.insert_session(&next).unwrap();
    f.store.set_lead_session(f.epic, Some(next.id)).unwrap();
    next.id
}

fn rotate_lead(f: &Fixture) -> Uuid {
    let mut next = f.store.get_session(f.lead).unwrap().unwrap();
    next.id = Uuid::new_v4();
    next.continued_from = Some(f.lead);
    next.rotation_depth += 1;
    f.store.insert_session(&next).unwrap();
    f.store
        .update_session_status(f.lead, SessionStatus::Archived)
        .unwrap();
    f.store
        .record_harness_manager_rotation(f.lead, next.id)
        .unwrap();
    f.store.set_lead_session(f.epic, Some(next.id)).unwrap();
    next.id
}

#[test]
fn lead_terminal_request_state_frees_pending_slot() {
    let f = fixture();
    let ids = fill(&f);
    assert_refused(send(&f, "over-cap"), EPIC_FULL);
    retrieve(&f, f.lead);
    lifecycle(
        &f,
        f.lead,
        ids[0],
        ManagerRequestStateV2::Declined,
        0,
        "decline",
    )
    .unwrap();
    let freed = send(&f, "after-decline").unwrap();
    assert!(!freed.deduplicated);
    assert_eq!(open_total(&f), CAP);
    assert_refused(send(&f, "over-cap-again"), EPIC_FULL);
}

#[test]
fn lead_failed_then_accepted_does_not_reclaim_slot() {
    let f = fixture();
    let ids = fill(&f);
    retrieve(&f, f.lead);
    lifecycle(
        &f,
        f.lead,
        ids[0],
        ManagerRequestStateV2::Accepted,
        0,
        "accept",
    )
    .unwrap();
    lifecycle(&f, f.lead, ids[0], ManagerRequestStateV2::Failed, 1, "fail").unwrap();
    send(&f, "after-fail").unwrap();
    assert_eq!(open_total(&f), CAP);
    // The lead's truthful reopen succeeds, but never reclaims the slot.
    lifecycle(
        &f,
        f.lead,
        ids[0],
        ManagerRequestStateV2::Accepted,
        2,
        "reopen",
    )
    .unwrap();
    assert_eq!(open_total(&f), CAP);
    assert_refused(send(&f, "after-reopen"), EPIC_FULL);
    let rows = f
        .store
        .manager_v2_request_rows(&config(&f), None, "", 256, false)
        .unwrap();
    let reopened = rows
        .iter()
        .find(|row| row["request_id"] == ids[0].to_string())
        .unwrap();
    assert_eq!(reopened["state"], "accepted");
}

fn health_reports(f: &Fixture) -> Value {
    let health = f
        .store
        .manager_v2_inspect(
            f.manager,
            &AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Health,
                epic_id: Some(f.epic),
                ..Default::default()
            },
        )
        .unwrap();
    health.rows[0].clone()
}

fn unanswered_ids(f: &Fixture) -> Vec<String> {
    f.store
        .manager_v2_request_rows(&config(f), Some(f.epic), "", 256, true)
        .unwrap()
        .iter()
        .map(|row| row["request_id"].as_str().unwrap().to_owned())
        .collect()
}

/// Review 7c6df190 (reopened-request-capacity), manager ruling (1): the cap
/// is an admission budget on manager sends, not an invariant on the active
/// count. A `failed -> accepted` reopen stays visible in Inspect unanswered
/// and in Health, but never reclaims a slot: the send cap, Progress
/// `pending_requests`, mail capacity and Health `pending_requests` stay 32
/// while Inspect unanswered and Health `active_requests` show all 33.
#[test]
fn reopened_request_stays_visible_but_never_reclaims_a_slot() {
    let f = fixture();
    let ids = fill(&f);
    retrieve(&f, f.lead);
    lifecycle(&f, f.lead, ids[0], ManagerRequestStateV2::Accepted, 0, "a").unwrap();
    lifecycle(&f, f.lead, ids[0], ManagerRequestStateV2::Failed, 1, "f").unwrap();
    let replacement = send(&f, "replacement").unwrap().message_id;
    lifecycle(&f, f.lead, ids[0], ManagerRequestStateV2::Accepted, 2, "r").unwrap();

    // Slots: 32, and the next send is refused.
    let cfg = config(&f);
    let (total, per_epic) = f.store.manager_open_request_counts(&cfg).unwrap();
    assert_eq!(total, CAP);
    assert_eq!(per_epic.get(&f.epic).copied(), Some(32));
    let progress = f.store.manager_progress(f.manager).unwrap();
    assert_eq!(progress_pending(&progress, f.epic), 32);
    assert_eq!(progress.mail_capacity.project_pending, 32);
    assert_refused(send(&f, "over-cap"), EPIC_FULL);

    // Attention: all 33 active requests, the reopened one included.
    let mut expected: Vec<String> = ids
        .iter()
        .chain([&replacement])
        .map(ToString::to_string)
        .collect();
    expected.sort();
    assert_eq!(unanswered_ids(&f), expected, "the reopen stays visible");
    let row = health_reports(&f);
    assert_eq!(row["reports"]["active_requests"], 33);
    assert_eq!(row["reports"]["pending_requests"], 32);
    let all = f
        .store
        .manager_v2_request_rows(&cfg, Some(f.epic), "", 256, false)
        .unwrap();
    let reopened = all
        .iter()
        .find(|row| row["request_id"] == ids[0].to_string())
        .unwrap();
    assert_eq!(reopened["state"], "accepted");
    assert_eq!(reopened["accepted"], true);

    // Answering the reopen leaves attention but frees no slot.
    lifecycle(&f, f.lead, ids[0], ManagerRequestStateV2::Running, 3, "run").unwrap();
    reply(&f, f.lead, ids[0], "answer-reopened").unwrap();
    assert_eq!(open_total(&f), CAP);
    assert_eq!(unanswered_ids(&f).len(), 32);
    let row = health_reports(&f);
    assert_eq!(row["reports"]["active_requests"], 32);
    assert_eq!(row["reports"]["pending_requests"], 32);
    assert_refused(send(&f, "over-cap-after-reply"), EPIC_FULL);
    // Only a genuinely open request frees a slot.
    reply(&f, f.lead, replacement, "answer-replacement").unwrap();
    assert_eq!(open_total(&f), CAP - 1);
    send(&f, "after-real-release").unwrap();
    assert_eq!(open_total(&f), CAP);
}

#[test]
fn pre_upgrade_failed_request_releases_before_reopen() {
    let f = fixture();
    let id = send(&f, "one").unwrap().message_id;
    retrieve(&f, f.lead);
    lifecycle(&f, f.lead, id, ManagerRequestStateV2::Accepted, 0, "accept").unwrap();
    lifecycle(&f, f.lead, id, ManagerRequestStateV2::Failed, 1, "fail").unwrap();
    let cfg = config(&f);
    let released = f
        .store
        .manager_v2_record(&cfg, REQUEST_RELEASED_KIND, &id.to_string())
        .unwrap()
        .expect("terminal transition writes the release marker");
    assert_eq!(released.payload["state"], "failed");
    // Simulate a row that failed before this upgrade: no release marker.
    f.store
        .conn
        .execute(
            "DELETE FROM harness_manager_v2_records WHERE kind=?1",
            [REQUEST_RELEASED_KIND],
        )
        .unwrap();
    assert_eq!(open_total(&f), 0, "the failed state alone is not open");
    lifecycle(&f, f.lead, id, ManagerRequestStateV2::Accepted, 2, "reopen").unwrap();
    let released = f
        .store
        .manager_v2_record(&cfg, REQUEST_RELEASED_KIND, &id.to_string())
        .unwrap()
        .expect("failed -> accepted releases a pre-upgrade row first");
    assert_eq!(released.payload["state"], "failed");
    assert_eq!(released.payload["actor"], f.lead.to_string());
    assert_eq!(open_total(&f), 0);
}

#[test]
fn orphan_on_raw_replaced_lead_settles_as_lead_replaced() {
    let f = fixture();
    let ids = fill(&f);
    let replacement = raw_replace_lead(&f);
    // The first send after the replacement sweeps the orphans, then admits.
    let fresh = send(&f, "after-replace").unwrap();
    assert!(!fresh.deduplicated);
    assert_eq!(open_total(&f), 1);
    for id in &ids {
        let record = settlement(&f, *id).expect("orphan settled");
        assert_eq!(record.payload["disposition"], "lead_replaced");
        assert_eq!(record.payload["actor"], "daemon");
        assert_eq!(record.payload["recipient_tip"], f.lead.to_string());
        assert_eq!(record.payload["current_lead"], replacement.to_string());
    }
    // The replacement's own request is live and never settled.
    assert!(settlement(&f, fresh.message_id).is_none());
    // Progress also sweeps; a second pass rewrites nothing.
    f.store.manager_progress(f.manager).unwrap();
    assert_eq!(settlement(&f, ids[0]).unwrap().row_version, 1);
    let unanswered = f
        .store
        .manager_v2_request_rows(&config(&f), None, "", 256, true)
        .unwrap();
    assert_eq!(unanswered.len(), 1);
    assert_eq!(unanswered[0]["request_id"], fresh.message_id.to_string());
}

#[test]
fn rotated_lead_request_stays_open() {
    let f = fixture();
    let sent = send(&f, "rotation").unwrap();
    let successor = rotate_lead(&f);
    f.store.manager_progress(f.manager).unwrap();
    send(&f, "after-rotation").unwrap();
    assert!(settlement(&f, sent.message_id).is_none());
    assert_eq!(open_total(&f), 2);
    let answered = reply(&f, successor, sent.message_id, "rotated-reply").unwrap();
    assert_eq!(answered.request_id, Some(sent.message_id));
    assert_eq!(open_total(&f), 1);
}

#[test]
fn orphan_sweep_skips_vacant_lead() {
    let f = fixture();
    let sent = send(&f, "vacant").unwrap();
    f.store.set_lead_session(f.epic, None).unwrap();
    f.store.manager_progress(f.manager).unwrap();
    assert!(settlement(&f, sent.message_id).is_none());
    assert_eq!(open_total(&f), 1);
    // A later replacement makes it an orphan of a replaced lead.
    let replacement = raw_replace_lead(&f);
    f.store.manager_progress(f.manager).unwrap();
    let record = settlement(&f, sent.message_id).expect("settled after replacement");
    assert_eq!(record.payload["current_lead"], replacement.to_string());
    assert_eq!(open_total(&f), 0);
}

fn unsettled(f: &Fixture, id: Uuid) -> Option<ManagerRecordV2> {
    f.store
        .manager_v2_record(&config(f), REQUEST_UNSETTLED_KIND, &id.to_string())
        .unwrap()
}

/// Manager ruling (2): a DAEMON `lead_replaced` settlement is lifted when the
/// caller is again the request's current recipient lead. The reply writes a
/// `request_unsettled` marker, lands, and the request is then an ordinary
/// replied request; replay rules are unchanged.
#[test]
fn restored_lead_reply_unsettles_a_daemon_lead_replaced_request() {
    let f = fixture();
    let answered = send(&f, "answered").unwrap().message_id;
    let orphan = send(&f, "orphan").unwrap().message_id;
    let original = reply(&f, f.lead, answered, "pre-settle").unwrap();
    let replacement = raw_replace_lead(&f);
    f.store.manager_progress(f.manager).unwrap();
    let settle = settlement(&f, orphan).expect("orphan settled");
    assert_eq!(settle.payload["disposition"], "lead_replaced");
    assert_eq!(open_total(&f), 0);
    // The replacement is not the recipient lineage: still out of scope.
    assert_refused(
        reply(&f, replacement, orphan, "by-replacement"),
        "manager_request_not_in_scope",
    );
    assert!(unsettled(&f, orphan).is_none());

    // The operator links the original lead back; its reply is accepted.
    f.store.set_lead_session(f.epic, Some(f.lead)).unwrap();
    let coordination = coordination_count(&f);
    let landed = reply(&f, f.lead, orphan, "post-restore").unwrap();
    assert!(!landed.deduplicated);
    assert_eq!(landed.request_id, Some(orphan));
    let marker = unsettled(&f, orphan).expect("unsettle marker written");
    assert_eq!(marker.payload["settle_row_version"], settle.row_version);
    assert_eq!(marker.payload["disposition"], "lead_replaced");
    assert_eq!(marker.payload["actor"], f.lead.to_string());
    assert_eq!(
        coordination_count(&f),
        coordination,
        "request_unsettled is its own bookkeeping class"
    );
    // Normal replied lifecycle: no settlement, not open, replayable, and a
    // further reply is ordinary.
    assert!(
        f.store
            .manager_request_settlement(&config(&f), orphan)
            .unwrap()
            .is_none()
    );
    assert_eq!(open_total(&f), 0);
    let replay = reply(&f, f.lead, orphan, "post-restore").unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.message_id, landed.message_id);
    let follow_up = reply(&f, f.lead, orphan, "follow-up").unwrap();
    assert!(!follow_up.deduplicated);
    assert_eq!(unsettled(&f, orphan).unwrap().row_version, 1);
    let row = f
        .store
        .manager_v2_request_rows(&config(&f), Some(f.epic), "", 256, false)
        .unwrap()
        .into_iter()
        .find(|row| row["request_id"] == orphan.to_string())
        .unwrap();
    assert_eq!(row["replied"], true);
    assert_eq!(row["settlement"], Value::Null);

    // An exact replay of a pre-settle reply still returns the original
    // receipt before any settle check, and writes no unsettle marker.
    f.store
        .manager_v2_put_record(
            &config(&f),
            REQUEST_SETTLE_KIND,
            &answered.to_string(),
            Some(f.epic),
            0,
            &serde_json::json!({"disposition":"lead_replaced","actor":"daemon"}),
        )
        .unwrap();
    let replay = reply(&f, f.lead, answered, "pre-settle").unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.message_id, original.message_id);
    assert_eq!(replay.request_id, Some(answered));
    assert!(unsettled(&f, answered).is_none());
}

#[test]
fn restored_lead_unsettle_succeeds_at_coordination_limit() {
    let f = fixture();
    let orphan = send(&f, "orphan").unwrap().message_id;
    raw_replace_lead(&f);
    f.store.manager_progress(f.manager).unwrap();
    assert!(settlement(&f, orphan).is_some());
    f.store.set_lead_session(f.epic, Some(f.lead)).unwrap();
    let full = fill_coordination(&f);
    reply(&f, f.lead, orphan, "at-limit").unwrap();
    assert!(unsettled(&f, orphan).is_some());
    assert_coordination_still_full(&f, full);
}

/// Plan (a) settled-reply rule for a manager settle: an exact replay still
/// returns its original receipt; any new reply is refused, even from the
/// request's current recipient lead.
#[test]
fn reply_to_manager_settled_request_refuses_after_exact_replay() {
    let f = fixture();
    let answered = send(&f, "answered").unwrap().message_id;
    let withdrawn = send(&f, "withdrawn").unwrap().message_id;
    let original = reply(&f, f.lead, answered, "pre-settle").unwrap();
    for id in [answered, withdrawn] {
        f.store
            .manager_v2_put_record(
                &config(&f),
                REQUEST_SETTLE_KIND,
                &id.to_string(),
                Some(f.epic),
                0,
                &serde_json::json!({"disposition":"withdrawn","actor":f.manager}),
            )
            .unwrap();
    }
    assert_refused(
        reply(&f, f.lead, withdrawn, "post-settle"),
        "manager_request_settled",
    );
    let replay = reply(&f, f.lead, answered, "pre-settle").unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.message_id, original.message_id);
    assert_eq!(replay.request_id, Some(answered));
    assert_refused(
        reply(&f, f.lead, answered, "second"),
        "manager_request_settled",
    );
    assert!(unsettled(&f, withdrawn).is_none());
    assert!(unsettled(&f, answered).is_none());
}

#[test]
fn lead_lifecycle_on_settled_request_refuses() {
    let f = fixture();
    let id = send(&f, "lifecycle").unwrap().message_id;
    retrieve(&f, f.lead);
    let accepted = lifecycle(&f, f.lead, id, ManagerRequestStateV2::Accepted, 0, "accept").unwrap();
    // Accepted is not terminal: the request is still open and addressed to
    // the old lead when a raw replacement makes it an orphan.
    raw_replace_lead(&f);
    f.store.manager_progress(f.manager).unwrap();
    assert!(settlement(&f, id).is_some());
    f.store.set_lead_session(f.epic, Some(f.lead)).unwrap();
    let replay = lifecycle(&f, f.lead, id, ManagerRequestStateV2::Accepted, 0, "accept").unwrap();
    assert_eq!(replay.row_version, accepted.row_version);
    assert_refused(
        lifecycle(&f, f.lead, id, ManagerRequestStateV2::Running, 1, "run"),
        "manager_v2_request_settled",
    );
}

/// The ledger fixture with `count` scoped Epics, each with its own lead, under
/// one V2 policy. `Fixture.epic`/`lead` are the first pair.
fn multi_fixture(count: usize) -> (Fixture, Vec<Uuid>) {
    let store = Store::open_in_memory().unwrap();
    let project = Uuid::new_v4();
    store
        .insert_project(&Project {
            id: project,
            name: "Capacity project".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .unwrap();
    let mut manager = test_session(Uuid::new_v4(), PathBuf::from("/var/tmp/ham-capacity"));
    manager.session_kind = SessionKind::Standard;
    manager.project_id = Some(project);
    manager.status = SessionStatus::Completed;
    store.insert_session(&manager).unwrap();
    let mut group = manager.clone();
    group.id = Uuid::new_v4();
    group.session_kind = SessionKind::Group;
    store.insert_session(&group).unwrap();
    let mut epics = Vec::new();
    let mut leads = Vec::new();
    for index in 0..count {
        let mut epic = manager.clone();
        epic.id = Uuid::new_v4();
        epic.session_kind = SessionKind::Epic;
        epic.parent_id = Some(group.id);
        epic.title = Some(format!("Capacity Epic {index}"));
        store.insert_session(&epic).unwrap();
        let mut lead = manager.clone();
        lead.id = Uuid::new_v4();
        lead.session_kind = SessionKind::Feature;
        lead.parent_id = Some(epic.id);
        store.insert_session(&lead).unwrap();
        store.set_lead_session(epic.id, Some(lead.id)).unwrap();
        epics.push(epic.id);
        leads.push(lead.id);
    }
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: project,
            session_id: manager.id,
            epic_ids: Some(epics.clone()),
            expected_row_version: 0,
        })
        .unwrap();
    store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: project,
            expected_scope_version: 1,
            expected_policy_version: 0,
            idempotency_key: "policy".into(),
            policy: ManagerPolicyV2 {
                capabilities: vec![ManagerCapabilityV2::WorkPlan],
                ..Default::default()
            },
        })
        .unwrap();
    let fixture = Fixture {
        store,
        project,
        manager: manager.id,
        epic: epics[0],
        lead: leads[0],
    };
    (fixture, epics)
}

fn send_to(f: &Fixture, epic_id: Uuid, key: &str) -> Result<HarnessManagerMessageReceiptV1> {
    f.store.manager_send(
        f.manager,
        &AgentManagerSendRequestV1 {
            epic_id,
            message: format!("Request {key}"),
            idempotency_key: key.into(),
        },
    )
}

fn fill_epic(f: &Fixture, epic_id: Uuid, count: i64) {
    for index in 0..count {
        send_to(f, epic_id, &format!("{epic_id}-{index}")).unwrap();
    }
}

fn progress_pending(progress: &AgentManagerProgressResultV1, epic_id: Uuid) -> u32 {
    progress
        .rows
        .iter()
        .find(|row| row.epic_id == epic_id)
        .unwrap()
        .pending_requests
}

#[test]
fn per_epic_cap_does_not_block_other_epics() {
    let (f, epics) = multi_fixture(2);
    fill_epic(&f, epics[0], CAP);
    assert_refused(send_to(&f, epics[0], "x-over"), EPIC_FULL);
    let other = send_to(&f, epics[1], "y-first").unwrap();
    assert!(!other.deduplicated);
    let progress = f.store.manager_progress(f.manager).unwrap();
    assert_eq!(progress_pending(&progress, epics[0]), 32);
    assert_eq!(progress_pending(&progress, epics[1]), 1);
    assert_eq!(progress.mail_capacity.project_pending, 33);
    // The full Epic still refuses while the other keeps admitting.
    assert_refused(send_to(&f, epics[0], "x-over-again"), EPIC_FULL);
    send_to(&f, epics[1], "y-second").unwrap();
}

#[test]
fn project_ceiling_256() {
    let (f, epics) = multi_fixture(9);
    for epic in &epics[..8] {
        fill_epic(&f, *epic, CAP);
    }
    assert_eq!(open_total(&f), 256);
    // The ninth Epic is empty, so only the project ceiling refuses it.
    let refused = send_to(&f, epics[8], "ninth")
        .expect_err("project ceiling")
        .to_string();
    assert!(
        refused.contains("manager_pending_request_limit"),
        "{refused}"
    );
    assert!(!refused.contains(EPIC_FULL), "{refused}");
    let progress = f.store.manager_progress(f.manager).unwrap();
    assert_eq!(progress.mail_capacity.project_pending, 256);
    assert_eq!(progress.mail_capacity.project_limit, 256);
    assert_eq!(progress_pending(&progress, epics[8]), 0);
}

#[test]
fn progress_and_inspect_report_mail_capacity_with_75pct_warning() {
    let (f, epics) = multi_fixture(2);
    fill_epic(&f, epics[0], 23);
    let progress = f.store.manager_progress(f.manager).unwrap();
    assert_eq!(
        progress.mail_capacity,
        HarnessManagerMailCapacityV1 {
            project_pending: 23,
            project_limit: 256,
            epic_limit: 32,
            warning: None,
        }
    );
    assert_eq!(progress_pending(&progress, epics[0]), 23);
    assert_eq!(progress_pending(&progress, epics[1]), 0);
    // 24 of 32 is exactly 75% of the per-Epic limit.
    send_to(&f, epics[0], "threshold").unwrap();
    let progress = f.store.manager_progress(f.manager).unwrap();
    assert_eq!(progress.mail_capacity.project_pending, 24);
    assert_eq!(
        progress.mail_capacity.warning.as_deref(),
        Some("manager_mail_capacity_75pct")
    );
    let rows = inspect(&f, ManagerInspectSectionV2::Overview).rows;
    let capacity = rows
        .iter()
        .find(|row| row["type"] == "mail_capacity")
        .expect("overview carries a mail_capacity row");
    assert_eq!(capacity["project_pending"], 24);
    assert_eq!(capacity["project_limit"], 256);
    assert_eq!(capacity["epic_limit"], 32);
    assert_eq!(capacity["warning"], "manager_mail_capacity_75pct");
    assert_eq!(
        capacity["per_epic"],
        serde_json::json!([{"epic_id": epics[0], "pending": 24}])
    );
    // A settled orphan leaves the capacity count and reports `settled`.
    let orphan = send_to(&f, epics[1], "orphan").unwrap().message_id;
    let mut replacement = f.store.get_session(f.lead).unwrap().unwrap();
    replacement.id = Uuid::new_v4();
    replacement.parent_id = Some(epics[1]);
    f.store.insert_session(&replacement).unwrap();
    f.store
        .set_lead_session(epics[1], Some(replacement.id))
        .unwrap();
    let progress = f.store.manager_progress(f.manager).unwrap();
    assert_eq!(progress_pending(&progress, epics[1]), 0);
    let summary = progress
        .recent_requests
        .iter()
        .find(|summary| summary.request_id == orphan)
        .unwrap();
    assert_eq!(summary.state, "settled");
}

// ---- S3 (#656): standing-request rollover and chain-wide replay ----

/// Sends one standing request and fills its root generation to the reply cap
/// with keys `r-0..r-31`; returns the root id.
fn full_standing_request(f: &Fixture) -> Uuid {
    let root = send(f, "standing").unwrap().message_id;
    for index in 0..MAX_REPLIES_PER_REQUEST {
        let receipt = reply(f, f.lead, root, &format!("r-{index}")).unwrap();
        assert_eq!(receipt.request_id, Some(root));
        assert_eq!(receipt.rolled_over_from, None);
    }
    root
}

fn reply_count(f: &Fixture, request_id: Uuid) -> i64 {
    f.store
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_messages WHERE request_id=?1",
            [request_id.to_string()],
            |row| row.get(0),
        )
        .unwrap()
}

fn key_count(f: &Fixture, key: &str) -> i64 {
    f.store
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_messages WHERE idempotency_key=?1",
            [key],
            |row| row.get(0),
        )
        .unwrap()
}

fn rollover_record(f: &Fixture, id: Uuid) -> Option<ManagerRecordV2> {
    f.store
        .manager_v2_record(&config(f), REQUEST_ROLLOVER_KIND, &id.to_string())
        .unwrap()
}

/// The ordered generation list of the chain record keyed by `root`.
fn generations(f: &Fixture, root: Uuid) -> Vec<Uuid> {
    let record = rollover_record(f, root).expect("chain record");
    assert_eq!(record.payload["root_request_id"], root.to_string());
    record.payload["generations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().parse().unwrap())
        .collect()
}

/// Every inbox message a caller sees under a `request_id` filter, all pages.
fn filtered_inbox(f: &Fixture, caller: Uuid, request_id: Uuid) -> Vec<HarnessManagerMessageV1> {
    let mut after_sequence = 0;
    let mut out = Vec::new();
    loop {
        let page = f
            .store
            .manager_inbox(
                caller,
                &AgentManagerInboxRequestV1 {
                    after_sequence,
                    request_id: Some(request_id),
                    ..Default::default()
                },
            )
            .unwrap();
        out.extend(page.messages);
        match page.next_after_sequence {
            Some(next) => after_sequence = next,
            None => return out,
        }
    }
}

#[test]
fn standing_request_rolls_over_at_33rd_reply() {
    let f = fixture();
    let root = full_standing_request(&f);
    let rolled = reply(&f, f.lead, root, "r-32").unwrap();
    let successor = rolled.request_id.expect("reply correlates to a generation");
    assert_ne!(successor, root);
    assert_eq!(rolled.rolled_over_from, Some(root));
    assert!(!rolled.deduplicated);
    // The successor is a manager->lead request carrying the text verbatim
    // under the server-owned key, sender = manager tip, recipient = the lead.
    let (message, key, sender, recipient, request_id): (
        String,
        String,
        String,
        String,
        Option<String>,
    ) = f
        .store
        .conn
        .query_row(
            "SELECT message,idempotency_key,sender_session_id,recipient_session_id,request_id
             FROM harness_manager_messages WHERE id=?1",
            [successor.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(message, "Request standing");
    assert_eq!(key, format!("rollover:{root}:1"));
    assert_eq!(sender, f.manager.to_string());
    assert_eq!(recipient, f.lead.to_string());
    assert_eq!(request_id, None);
    assert_eq!(generations(&f, root), vec![root, successor]);
    assert_eq!(rollover_record(&f, root).unwrap().row_version, 1);
    // Replied in the same transaction: never pending.
    assert_eq!(open_total(&f), 0);
    // The 34th reply naming the root lands on the same successor.
    let next = reply(&f, f.lead, root, "r-33").unwrap();
    assert_eq!(next.request_id, Some(successor));
    assert_eq!(next.rolled_over_from, Some(root));
    assert!(rollover_record(&f, successor).is_none());
    assert_eq!(reply_count(&f, root), MAX_REPLIES_PER_REQUEST);
    assert_eq!(reply_count(&f, successor), 2);

    // A root filter returns every generation and all of their replies.
    for caller in [f.lead, f.manager] {
        let seen = filtered_inbox(&f, caller, root);
        assert_eq!(seen.len(), 2 + 34, "root, successor and 34 replies");
        let root_row = seen.iter().find(|m| m.message_id == root).unwrap();
        assert_eq!(root_row.standing_root_id, None);
        assert!(root_row.replied);
        let successor_row = seen.iter().find(|m| m.message_id == successor).unwrap();
        assert_eq!(successor_row.standing_root_id, Some(root));
        assert!(successor_row.replied);
        let rolled_row = seen
            .iter()
            .find(|m| m.message_id == rolled.message_id)
            .unwrap();
        assert_eq!(rolled_row.request_id, Some(successor));
        assert_eq!(rolled_row.standing_root_id, Some(root));
        // Filtering by the successor resolves the same chain.
        let by_successor: Vec<Uuid> = filtered_inbox(&f, caller, successor)
            .into_iter()
            .map(|m| m.message_id)
            .collect();
        assert_eq!(
            by_successor,
            seen.iter().map(|m| m.message_id).collect::<Vec<_>>()
        );
    }
    let progress = f.store.manager_progress(f.manager).unwrap();
    let summary = |id: Uuid| {
        progress
            .recent_requests
            .iter()
            .find(|r| r.request_id == id)
            .unwrap()
    };
    assert_eq!(summary(root).state, "rolled_over");
    assert_eq!(summary(root).rolled_over_to, Some(successor));
    assert_eq!(summary(successor).state, "replied");
    assert_eq!(summary(successor).rolled_over_to, None);
}

#[test]
fn rollover_replay_is_idempotent() {
    let f = fixture();
    let root = full_standing_request(&f);
    let rolled = reply(&f, f.lead, root, "r-32").unwrap();
    let successor = rolled.request_id.unwrap();
    let replay = reply(&f, f.lead, root, "r-32").unwrap();
    assert_eq!(
        replay,
        HarnessManagerMessageReceiptV1 {
            deduplicated: true,
            ..rolled
        }
    );
    // An early root-generation reply still replays to its original row.
    let early = reply(&f, f.lead, root, "r-0").unwrap();
    assert!(early.deduplicated);
    assert_eq!(early.request_id, Some(root));
    assert_eq!(early.rolled_over_from, None);
    // No second successor, no duplicate reply.
    assert!(rollover_record(&f, successor).is_none());
    assert_eq!(reply_count(&f, successor), 1);
    assert_eq!(key_count(&f, "r-32"), 1);
    // Same key, different content: conflict, nothing written.
    assert_refused(
        f.store.manager_reply(
            f.lead,
            &AgentManagerReplyRequestV1 {
                request_id: root,
                message: "different".into(),
                idempotency_key: "r-32".into(),
            },
        ),
        "manager_idempotency_conflict",
    );
    assert_eq!(reply_count(&f, successor), 1);
}

#[test]
fn concurrent_rollover_single_successor() {
    let dir = tempfile::Builder::new()
        .prefix("rollover-")
        .tempdir()
        .unwrap();
    let path = dir.path().join("store.db");
    let f = fixture_using(Store::open(&path).unwrap());
    let root = full_standing_request(&f);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let lead = f.lead;
    let spawn = |key: &'static str| {
        let barrier = barrier.clone();
        let path = path.clone();
        std::thread::spawn(move || {
            let store = Store::open(&path).unwrap();
            barrier.wait();
            store
                .manager_reply(
                    lead,
                    &AgentManagerReplyRequestV1 {
                        request_id: root,
                        message: format!("Reply {key}"),
                        idempotency_key: key.into(),
                    },
                )
                .unwrap()
        })
    };
    let (a, b) = (spawn("c-a"), spawn("c-b"));
    let receipts = [a.join().unwrap(), b.join().unwrap()];
    let chain = generations(&f, root);
    assert_eq!(chain.len(), 2, "one successor");
    let successor = chain[1];
    for receipt in &receipts {
        assert_eq!(receipt.request_id, Some(successor));
        assert_eq!(receipt.rolled_over_from, Some(root));
    }
    assert!(rollover_record(&f, successor).is_none());
    assert_eq!(reply_count(&f, successor), 2);
    let successors: i64 = f
        .store
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_messages WHERE idempotency_key GLOB 'rollover:*'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(successors, 1);
}

#[test]
fn manager_send_rejects_rollover_key_prefix() {
    let f = fixture();
    let root = send(&f, "root").unwrap().message_id;
    for key in [
        format!("rollover:{root}"),
        format!("rollover:{root}:1"),
        "rollover:".to_owned(),
    ] {
        assert_refused(send(&f, &key), "manager_reserved_idempotency_key");
        assert_eq!(key_count(&f, &key), 0);
    }
    assert_eq!(open_total(&f), 1);
    // The reserved prefix is exact: an ordinary key containing it elsewhere passes.
    send(&f, "not-rollover:x").unwrap();
    assert_eq!(open_total(&f), 2);
}

#[test]
fn rollover_replay_after_lead_rotation_dedups() {
    let f = fixture();
    let root = full_standing_request(&f);
    let rolled = reply(&f, f.lead, root, "r-32").unwrap();
    let successor = rolled.request_id.unwrap();
    let rotated = rotate_lead(&f);
    // The successor lead retries the same key naming the root: it lives on the
    // successor generation and was sent by the predecessor lead.
    let replay = reply(&f, rotated, root, "r-32").unwrap();
    assert_eq!(
        replay,
        HarnessManagerMessageReceiptV1 {
            deduplicated: true,
            ..rolled
        }
    );
    assert_eq!(key_count(&f, "r-32"), 1);
    assert_eq!(reply_count(&f, root), MAX_REPLIES_PER_REQUEST);
    assert_eq!(reply_count(&f, successor), 1);
    // A new reply from the rotated lead continues on the same successor.
    let fresh = reply(&f, rotated, root, "after-rotation").unwrap();
    assert_eq!(fresh.request_id, Some(successor));
    assert!(!fresh.deduplicated);
    assert_eq!(reply_count(&f, successor), 2);
}

#[test]
fn rollover_replay_naming_successor_id_dedups() {
    let f = fixture();
    let root = full_standing_request(&f);
    let rolled = reply(&f, f.lead, root, "r-32").unwrap();
    let successor = rolled.request_id.unwrap();
    // Naming the successor replays the reply that named the root, with the
    // identical (root-relative) receipt.
    let replay = reply(&f, f.lead, successor, "r-32").unwrap();
    assert_eq!(
        replay,
        HarnessManagerMessageReceiptV1 {
            deduplicated: true,
            ..rolled
        }
    );
    // A root-generation reply replays through the successor id too.
    let early = reply(&f, f.lead, successor, "r-5").unwrap();
    assert!(early.deduplicated);
    assert_eq!(early.request_id, Some(root));
    assert_eq!(key_count(&f, "r-5"), 1);
    // A new reply naming the successor stores there.
    let fresh = reply(&f, f.lead, successor, "via-successor").unwrap();
    assert_eq!(fresh.request_id, Some(successor));
    assert_eq!(fresh.rolled_over_from, Some(root));
    assert_eq!(reply_count(&f, successor), 2);
}

#[test]
fn existing_reply_digest_unchanged_for_unrolled_request() {
    use sha2::{Digest, Sha256};
    let f = fixture();
    let root = send(&f, "golden").unwrap().message_id;
    let receipt = reply(&f, f.lead, root, "golden-reply").unwrap();
    // Golden: the pre-#656 digest is sha256 of the reply request serialized
    // with the named id; for an unrolled request the root IS the named id.
    let golden = format!(
        r#"{{"request_id":"{root}","message":"Reply golden-reply","idempotency_key":"golden-reply"}}"#
    );
    let stored: String = f
        .store
        .conn
        .query_row(
            "SELECT request_fingerprint FROM harness_manager_messages WHERE id=?1",
            [receipt.message_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, format!("{:x}", Sha256::digest(golden.as_bytes())));
    // The receipt wire shape is byte-identical to V1 (no rollover field).
    assert_eq!(
        serde_json::to_string(&receipt).unwrap(),
        format!(
            r#"{{"message_id":"{}","sequence":{},"request_id":"{root}","deduplicated":false}}"#,
            receipt.message_id, receipt.sequence
        )
    );
    let replay = reply(&f, f.lead, root, "golden-reply").unwrap();
    assert_eq!(
        replay,
        HarnessManagerMessageReceiptV1 {
            deduplicated: true,
            ..receipt
        }
    );
    assert!(rollover_record(&f, root).is_none());
}

// ---- #664 review a39cb5c1: request markers at the coordination limit ----

fn coordination_count(f: &Fixture) -> i64 {
    let cfg = config(f);
    f.store
        .conn
        .query_row(
            crate::store::harness_manager_v2::COORDINATION_BUDGET_COUNT_SQL,
            params![
                cfg.project_id.to_string(),
                cfg.manager_session_id.to_string(),
                cfg.row_version
            ],
            |row| row.get(0),
        )
        .unwrap()
}

/// Fills the scope to exactly `MANAGER_V2_MAX_RECORDS` coordination records
/// (pattern at `manager_resources/tests.rs:1535`) and returns that count.
fn fill_coordination(f: &Fixture) -> i64 {
    let cfg = config(f);
    let limit = i64::try_from(MANAGER_V2_MAX_RECORDS).unwrap();
    let stamp = crate::store::harness_manager_v2::now();
    let tx = f.store.conn.unchecked_transaction().unwrap();
    for n in coordination_count(f)..limit {
        tx.execute(
            "INSERT INTO harness_manager_v2_records(project_id,manager_session_id,scope_version,
                 kind,record_key,row_version,payload_json,archived,created_at,updated_at)
             VALUES(?1,?2,?3,'intent',?4,1,'{}',0,?5,?5)",
            params![
                cfg.project_id.to_string(),
                cfg.manager_session_id.to_string(),
                cfg.row_version,
                format!("filler:{n}"),
                stamp
            ],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    let full = coordination_count(f);
    assert_eq!(full, limit);
    full
}

/// The coordination budget stays exactly full and still refuses its next row.
#[track_caller]
fn assert_coordination_still_full(f: &Fixture, full: i64) {
    assert_eq!(coordination_count(f), full);
    assert_refused(
        f.store
            .manager_v2_put_record(&config(f), "intent", "over", None, 0, &json!({})),
        "manager_v2_record_limit",
    );
}

fn released(f: &Fixture, id: Uuid) -> Option<ManagerRecordV2> {
    f.store
        .manager_v2_record(&config(f), REQUEST_RELEASED_KIND, &id.to_string())
        .unwrap()
}

#[test]
fn lead_terminal_release_succeeds_at_coordination_limit() {
    use ManagerRequestStateV2::{Accepted, Blocked, Declined, Failed};
    let f = fixture();
    let ids = fill(&f);
    retrieve(&f, f.lead);
    // Every request record exists before the budget fills, so each terminal
    // transition below only updates it; only the release marker is new.
    lifecycle(&f, f.lead, ids[0], Accepted, 0, "accept-0").unwrap();
    lifecycle(&f, f.lead, ids[1], Accepted, 0, "accept-1").unwrap();
    lifecycle(&f, f.lead, ids[1], Blocked, 1, "block-1").unwrap();
    lifecycle(&f, f.lead, ids[2], Accepted, 0, "accept-2").unwrap();
    lifecycle(&f, f.lead, ids[2], Failed, 1, "fail-2").unwrap();
    // ids[2] becomes a pre-upgrade failed row: failed, with no marker.
    f.store
        .conn
        .execute(
            "DELETE FROM harness_manager_v2_records WHERE kind=?1 AND record_key=?2",
            params![REQUEST_RELEASED_KIND, ids[2].to_string()],
        )
        .unwrap();
    let full = fill_coordination(&f);
    let open = open_total(&f);
    assert_eq!(open, CAP - 1);

    lifecycle(&f, f.lead, ids[0], Failed, 1, "fail-0").unwrap();
    assert_eq!(released(&f, ids[0]).unwrap().payload["state"], "failed");
    assert_eq!(open_total(&f), open - 1, "failed frees its slot");
    assert_coordination_still_full(&f, full);

    lifecycle(&f, f.lead, ids[1], Declined, 2, "decline-1").unwrap();
    assert_eq!(released(&f, ids[1]).unwrap().payload["state"], "declined");
    assert_eq!(open_total(&f), open - 2, "declined frees its slot");
    assert_coordination_still_full(&f, full);

    lifecycle(&f, f.lead, ids[2], Accepted, 2, "reopen-2").unwrap();
    let pre_upgrade = released(&f, ids[2]).expect("failed -> accepted releases first");
    assert_eq!(pre_upgrade.payload["state"], "failed");
    assert_eq!(open_total(&f), open - 2, "the reopen never reclaims a slot");
    assert_coordination_still_full(&f, full);

    // Three slots are free: the two releases above plus the pre-upgrade
    // failed row, which was never open. They admit new sends (mail rows, not
    // v2 records) and the cap then refuses again.
    assert_eq!(open_total(&f), CAP - 3);
    for key in ["after-release-a", "after-release-b", "after-release-c"] {
        send(&f, key).unwrap();
    }
    assert_refused(send(&f, "after-release-d"), EPIC_FULL);
    assert_coordination_still_full(&f, full);
}

#[test]
fn orphan_sweep_settles_at_coordination_limit() {
    let f = fixture();
    let ids = fill(&f);
    raw_replace_lead(&f);
    let full = fill_coordination(&f);
    let fresh = send(&f, "after-replace").unwrap();
    assert!(!fresh.deduplicated);
    for id in &ids {
        let record = settlement(&f, *id).expect("orphan settled at the limit");
        assert_eq!(record.payload["disposition"], "lead_replaced");
    }
    assert_eq!(open_total(&f), 1);
    assert_coordination_still_full(&f, full);
}

fn marker_rows(f: &Fixture, kind: &str) -> i64 {
    let cfg = config(f);
    f.store
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_v2_records WHERE project_id=?1
             AND manager_session_id=?2 AND scope_version=?3 AND kind=?4 AND archived=0",
            params![
                cfg.project_id.to_string(),
                cfg.manager_session_id.to_string(),
                cfg.row_version,
                kind
            ],
            |row| row.get(0),
        )
        .unwrap()
}

/// Review 1273507c: send a full Epic, replace its lead and run Progress, over
/// and over. Earlier settled orphans are seeded as the same daemon markers so
/// that the real batches run exactly at and past the old 1,024-row
/// `request_settle` class cap. The sweep still settles every new orphan batch
/// and `manager_send` admits again; a restored lead can still lift a
/// settlement past that many settled rows.
#[test]
fn orphan_sweep_never_stops_after_1024_settled_orphans() {
    let f = fixture();
    let cfg = config(&f);
    let seeded = BOOKKEEPING_LIMIT - CAP;
    let tx = f.store.conn.unchecked_transaction().unwrap();
    for _ in 0..seeded {
        f.store
            .manager_v2_put_record(
                &cfg,
                REQUEST_SETTLE_KIND,
                &Uuid::new_v4().to_string(),
                Some(f.epic),
                0,
                &serde_json::json!({"disposition":"lead_replaced","actor":"daemon"}),
            )
            .unwrap();
    }
    tx.commit().unwrap();
    // Batch 0 fills the class to the old cap; batch 1 is the one the old cap
    // left open forever; batch 2 lands past it.
    let batches = 3;
    let mut first_batch = Vec::new();
    for batch in 0..batches {
        let ids: Vec<Uuid> = (0..CAP)
            .map(|index| send(&f, &format!("b{batch}-{index}")).unwrap().message_id)
            .collect();
        assert_refused(send(&f, &format!("b{batch}-over")), EPIC_FULL);
        raw_replace_lead(&f);
        f.store.manager_progress(f.manager).unwrap();
        for id in &ids {
            let record = settlement(&f, *id).expect("orphan settled");
            assert_eq!(record.payload["disposition"], "lead_replaced");
        }
        assert_eq!(open_total(&f), 0, "batch {batch}");
        if batch == 0 {
            first_batch = ids;
        }
    }
    let settled = marker_rows(&f, REQUEST_SETTLE_KIND);
    assert_eq!(settled, seeded + batches * CAP);
    assert!(settled > BOOKKEEPING_LIMIT);
    // The Epic admits a full batch again, and it is live, not settled.
    let fresh: Vec<Uuid> = (0..CAP)
        .map(|index| send(&f, &format!("after-{index}")).unwrap().message_id)
        .collect();
    assert_eq!(open_total(&f), CAP);
    assert!(fresh.iter().all(|id| settlement(&f, *id).is_none()));
    // Restoring the original lead still lifts its daemon settlement.
    f.store.set_lead_session(f.epic, Some(f.lead)).unwrap();
    reply(&f, f.lead, first_batch[0], "restored").unwrap();
    assert!(unsettled(&f, first_batch[0]).is_some());
    assert!(
        f.store
            .manager_request_settlement(&config(&f), first_batch[0])
            .unwrap()
            .is_none()
    );
}

#[test]
fn standing_request_rolls_over_at_coordination_limit() {
    let f = fixture();
    let root = full_standing_request(&f);
    let full = fill_coordination(&f);
    let rolled = reply(&f, f.lead, root, "r-32").unwrap();
    let successor = rolled.request_id.expect("reply correlates to a generation");
    assert_eq!(rolled.rolled_over_from, Some(root));
    assert_eq!(generations(&f, root), vec![root, successor]);
    assert_eq!(reply_count(&f, successor), 1);
    assert_coordination_still_full(&f, full);
}

// ---- #656 review 1cdaa85f: rollover capacity is per chain, not per scope ----

/// Seeds one standing request whose chain is full at `LINEAGE_LIMIT` (256)
/// generations, in the exact shape the daemon writes: the root is sent
/// through the API, each successor is a manager->lead request keyed
/// `rollover:{root}:{generation}`, every generation carries 32 replies, and
/// the chain record is written by the real `manager_v2_put_record`. Driving
/// 8,192 replies per chain through the API costs about a minute per chain.
fn seed_full_chain(f: &Fixture, key: &str) -> Vec<Uuid> {
    const GENERATIONS: usize = 256;
    let root = send(f, key).unwrap().message_id;
    let cfg = config(f);
    let stamp = crate::store::harness_manager_v2::now();
    let tx = f.store.conn.unchecked_transaction().unwrap();
    let row = |id: Uuid, sender: Uuid, recipient: Uuid, request: Option<Uuid>, k: String| {
        tx.execute(
            "INSERT INTO harness_manager_messages(id,project_id,manager_session_id,epic_id,scope_version,
                sender_session_id,recipient_session_id,request_id,idempotency_key,request_fingerprint,message,created_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'seeded',?10,?11)",
            params![id.to_string(), cfg.project_id.to_string(), cfg.manager_session_id.to_string(),
                f.epic.to_string(), cfg.row_version, sender.to_string(), recipient.to_string(),
                request.map(|id| id.to_string()), k, format!("Request {key}"), stamp],
        )
        .unwrap();
    };
    let mut ids = vec![root];
    for generation in 0..GENERATIONS {
        if generation > 0 {
            let successor = Uuid::new_v4();
            row(
                successor,
                f.manager,
                f.lead,
                None,
                format!("rollover:{root}:{generation}"),
            );
            ids.push(successor);
        }
        for index in 0..MAX_REPLIES_PER_REQUEST {
            let id = Uuid::new_v4();
            row(
                id,
                f.lead,
                f.manager,
                Some(ids[generation]),
                format!("{key}-{generation}-{index}"),
            );
        }
    }
    tx.commit().unwrap();
    f.store
        .manager_v2_put_record(
            &cfg,
            REQUEST_ROLLOVER_KIND,
            &root.to_string(),
            Some(f.epic),
            0,
            &serde_json::json!({
                "root_request_id": root,
                "generations": ids.iter().map(ToString::to_string).collect::<Vec<_>>(),
            }),
        )
        .unwrap();
    ids
}

fn rollover_rows(f: &Fixture) -> i64 {
    let cfg = config(f);
    f.store
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_v2_records WHERE project_id=?1
             AND manager_session_id=?2 AND scope_version=?3 AND kind=?4",
            params![
                cfg.project_id.to_string(),
                cfg.manager_session_id.to_string(),
                cfg.row_version,
                REQUEST_ROLLOVER_KIND
            ],
            |row| row.get(0),
        )
        .unwrap()
}

/// The reviewer's example: four standing requests at 256 generations each
/// (1,020 rollovers, which was 1,020 per-generation markers) no longer starve
/// a fifth request. Its fifth rollover (which used to be refused with
/// `manager_v2_request_rollover_record_limit`) and later ones succeed, the
/// scope holds one chain record per chain, the per-request 256-generation
/// bound still refuses, and replay naming any generation, including after
/// lead rotation, returns the original receipt.
#[test]
fn rollover_succeeds_when_other_chains_exceed_1024_generations() {
    let f = fixture();
    let full: Vec<Vec<Uuid>> = (0..4)
        .map(|chain| seed_full_chain(&f, &format!("big-{chain}")))
        .collect();
    let seeded_generations: usize = full.iter().map(Vec::len).sum();
    assert_eq!(seeded_generations, 1024);
    // The per-request bound stays: a full chain refuses its 257th generation
    // and writes nothing.
    for chain in &full {
        assert_eq!(generations(&f, chain[0]), *chain);
        assert_refused(
            reply(&f, f.lead, chain[0], "past-lineage"),
            "manager_request_rollover_limit",
        );
        assert_eq!(key_count(&f, "past-lineage"), 0);
        assert_eq!(generations(&f, chain[0]).len(), 256);
    }

    let root = full_standing_request(&f);
    let mut receipts = Vec::new();
    for index in MAX_REPLIES_PER_REQUEST..MAX_REPLIES_PER_REQUEST * 6 {
        let receipt = reply(&f, f.lead, root, &format!("r-{index}")).unwrap();
        assert!(!receipt.deduplicated);
        assert_eq!(receipt.rolled_over_from, Some(root));
        receipts.push(receipt);
    }
    let chain = generations(&f, root);
    assert_eq!(chain.len(), 6, "five rollovers");
    for (offset, receipt) in receipts.iter().enumerate() {
        let generation = 1 + offset / usize::try_from(MAX_REPLIES_PER_REQUEST).unwrap();
        assert_eq!(receipt.request_id, Some(chain[generation]));
    }
    for id in &chain {
        assert_eq!(reply_count(&f, *id), MAX_REPLIES_PER_REQUEST);
    }
    // Total generations in the scope exceed the old 1,024 marker cap; the
    // stored rollover rows are one per chain.
    assert!(seeded_generations + chain.len() > 1024);
    assert_eq!(rollover_rows(&f), 5);

    // The fifth rollover's first reply replays from every generation.
    let fifth = receipts[usize::try_from(MAX_REPLIES_PER_REQUEST * 4).unwrap()].clone();
    assert_eq!(fifth.request_id, Some(chain[5]));
    for named in [root, chain[2], chain[5]] {
        let replay = reply(&f, f.lead, named, "r-160").unwrap();
        assert_eq!(
            replay,
            HarnessManagerMessageReceiptV1 {
                deduplicated: true,
                ..fifth.clone()
            }
        );
    }
    let rotated = rotate_lead(&f);
    let replay = reply(&f, rotated, chain[5], "r-160").unwrap();
    assert_eq!(
        replay,
        HarnessManagerMessageReceiptV1 {
            deduplicated: true,
            ..fifth
        }
    );
    assert_eq!(key_count(&f, "r-160"), 1);
    // The rotated lead's next reply performs the sixth rollover.
    let fresh = reply(&f, rotated, root, "after-rotation").unwrap();
    let chain = generations(&f, root);
    assert_eq!(chain.len(), 7);
    assert_eq!(fresh.request_id, Some(chain[6]));
    assert_eq!(fresh.rolled_over_from, Some(root));
    assert_eq!(rollover_rows(&f), 5);
    // A root filter still returns every generation and every reply.
    let seen = filtered_inbox(&f, f.manager, root);
    assert_eq!(seen.len(), 7 + 32 * 6 + 1);
}
