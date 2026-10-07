//! #1310: manager notices survive succession, page with an honest cursor and
//! never wake the current seat for notices retrieval already settled.
#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use super::*;
use crate::test_support::test_session;
use rsi_common::types::Project;
use std::path::PathBuf;

struct Fixture {
    store: Store,
    config: HarnessManagerConfigV1,
    anchor: Uuid,
}

fn fixture() -> Fixture {
    fixture_with_store(Store::open_in_memory().unwrap())
}

fn fixture_with_store(store: Store) -> Fixture {
    let project = Uuid::new_v4();
    let now = Utc::now();
    store
        .insert_project(&Project {
            id: project,
            name: "Succession notices".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();
    let mut manager = test_session(Uuid::new_v4(), PathBuf::from("/tmp/succession-notices"));
    manager.session_kind = SessionKind::Standard;
    manager.project_id = Some(project);
    manager.status = SessionStatus::Running;
    store.insert_session(&manager).unwrap();
    let mut group = manager.clone();
    group.id = Uuid::new_v4();
    group.session_kind = SessionKind::Group;
    group.status = SessionStatus::Completed;
    store.insert_session(&group).unwrap();
    let mut epic = group.clone();
    epic.id = Uuid::new_v4();
    epic.session_kind = SessionKind::Epic;
    epic.parent_id = Some(group.id);
    store.insert_session(&epic).unwrap();
    let mut lead = manager.clone();
    lead.id = Uuid::new_v4();
    lead.session_kind = SessionKind::Feature;
    lead.parent_id = Some(epic.id);
    store.insert_session(&lead).unwrap();
    store.set_lead_session(epic.id, Some(lead.id)).unwrap();
    let config = store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: project,
            session_id: manager.id,
            epic_ids: None,
            expected_row_version: 0,
        })
        .unwrap();
    Fixture {
        store,
        config,
        anchor: manager.id,
    }
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

/// One retained `action_result` notice the way `record_manager_action_notice`
/// writes it, without building a whole action operation.
fn seed_action_notice(f: &Fixture, index: usize) {
    seed_action_notice_at(f, index, crate::store::harness_manager_v2::now());
}

fn seed_action_notice_at(f: &Fixture, index: usize, recorded: String) {
    let (job, _, recipient) = f.store.manager_action_watch_identity(&f.config);
    let version = "1";
    f.store
        .ensure_manager_action_watch(&f.config, &format!("{index}"))
        .unwrap();
    f.store
        .conn
        .execute(
            "INSERT INTO harness_manager_notices
             (id,job_id,project_id,manager_session_id,scope_version,epic_id,direction,
              source_session_id,recipient_session_id,kind,subject_id,subject_version,
              state_json,recorded_at,queued_at)
             VALUES(?1,?2,?3,?4,?5,NULL,'to_manager',NULL,?6,'action_result',?7,?8,'{}',?9,?9)",
            params![
                Uuid::new_v4().to_string(),
                job.to_string(),
                f.config.project_id.to_string(),
                f.config.manager_session_id.to_string(),
                f.config.row_version,
                recipient.to_string(),
                Uuid::new_v4().to_string(),
                version,
                recorded
            ],
        )
        .unwrap();
    f.store.refresh_manager_notice_job(job).unwrap();
}

fn unsettled(store: &Store) -> i64 {
    store
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_notices
             WHERE settled_at IS NULL AND retired_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

fn inbox(store: &Store, caller: Uuid, after: i64) -> AgentManagerInboxResultV1 {
    store
        .manager_inbox(
            caller,
            &AgentManagerInboxRequestV1 {
                after_sequence: after,
                ..Default::default()
            },
        )
        .unwrap()
}

fn page_all(store: &Store, caller: Uuid) -> Vec<AgentManagerInboxResultV1> {
    let mut pages = Vec::new();
    let mut after = 0;
    loop {
        let page = inbox(store, caller, after);
        let next = page.next_after_sequence;
        pages.push(page);
        assert!(pages.len() < 50, "paging never ends");
        match next {
            Some(cursor) => after = cursor,
            None => return pages,
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn successor_settles_notices_addressed_to_every_predecessor_seat() {
    let f = fixture();
    for index in 0..3 {
        seed_action_notice(&f, index);
    }
    // Two successions: the notices are addressed to the first seat.
    let second = rotate(&f.store, f.anchor);
    seed_action_notice(&f, 3);
    let third = rotate(&f.store, second);
    let pages = page_all(&f.store, third);
    let notices: Vec<_> = pages.iter().flat_map(|page| page.notices.iter()).collect();
    assert_eq!(notices.len(), 4);
    for notice in &notices {
        // Reported as addressed to the logical seat that retrieved it.
        assert_eq!(notice.recipient_session_id, third);
        assert_eq!(notice.settled_at, notice.retrieved_at);
    }
    assert_eq!(unsettled(&f.store), 0);
    // Settled notices stay settled: no second delivery, no new wake.
    assert!(
        page_all(&f.store, third)
            .iter()
            .all(|p| p.notices.is_empty())
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn notice_paging_cursor_reaches_every_notice_across_more_than_one_page() {
    let f = fixture();
    for index in 0..70 {
        seed_action_notice(&f, index);
    }
    let tip = rotate(&f.store, f.anchor);
    let pages = page_all(&f.store, tip);
    assert_eq!(pages.len(), 3, "70 notices at 32 per page");
    let mut seen = std::collections::HashSet::new();
    for page in &pages {
        // `more_notices` is never true without a cursor that continues it.
        assert_eq!(page.more_notices, page.next_after_sequence.is_some());
        for notice in &page.notices {
            assert!(seen.insert(notice.notice_id), "a notice was returned twice");
        }
    }
    assert_eq!(seen.len(), 70);
    assert!(!pages.last().unwrap().more_notices);
    assert_eq!(unsettled(&f.store), 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn notice_page_that_settles_nothing_claims_no_continuation() {
    let f = fixture();
    let page = inbox(&f.store, f.anchor, 0);
    assert!(page.notices.is_empty());
    assert!(!page.more_notices);
    assert_eq!(page.next_after_sequence, None);
}

fn pending_count(store: &Store) -> i64 {
    unsettled(store)
}

fn retired_count(store: &Store) -> i64 {
    store
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_notices WHERE retired_at IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

fn total_count(store: &Store) -> i64 {
    store
        .conn
        .query_row("SELECT count(*) FROM harness_manager_notices", [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn orphaned_notices_retire_after_the_bound_and_are_never_deleted() {
    let f = fixture();
    let old = (Utc::now() - chrono::Duration::days(30))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    // Old notices of the LIVE lineage are retrievable, so they are kept.
    seed_action_notice_at(&f, 0, old.clone());
    // A scope that no longer exists: old and recent notices.
    let ghost = Uuid::new_v4();
    let insert = |manager: Uuid, scope: i64, recorded: &str, subject: Uuid| {
        f.store
            .conn
            .execute(
                "INSERT INTO harness_manager_notices
                 (id,job_id,project_id,manager_session_id,scope_version,epic_id,direction,
                  source_session_id,recipient_session_id,kind,subject_id,subject_version,
                  state_json,recorded_at,queued_at)
                 VALUES(?1,?2,?3,?4,?5,NULL,'to_manager',NULL,?4,'action_result',?6,'1','{}',?7,?7)",
                params![
                    Uuid::new_v4().to_string(),
                    Uuid::new_v4().to_string(),
                    f.config.project_id.to_string(),
                    manager.to_string(),
                    scope,
                    subject.to_string(),
                    recorded
                ],
            )
            .unwrap();
    };
    insert(ghost, 7, &old, Uuid::new_v4());
    insert(ghost, 7, &old, Uuid::new_v4());
    let recent = crate::store::harness_manager_v2::now();
    insert(ghost, 7, &recent, Uuid::new_v4());
    // Current anchor but an older, replaced scope version.
    insert(f.anchor, f.config.row_version + 5, &old, Uuid::new_v4());
    let total = total_count(&f.store);
    assert_eq!(pending_count(&f.store), 5);

    assert_eq!(f.store.retire_orphaned_manager_notices().unwrap(), 3);
    assert_eq!(total_count(&f.store), total, "retirement never deletes");
    assert_eq!(retired_count(&f.store), 3);
    // The live lineage's old notice and the recent ghost notice remain.
    assert_eq!(pending_count(&f.store), 2);
    // A second pass is a no-op, and the retired rows carry no retrieval.
    assert_eq!(f.store.retire_orphaned_manager_notices().unwrap(), 0);
    let fabricated: i64 = f
        .store
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_notices
             WHERE retired_at IS NOT NULL AND (retrieved_at IS NOT NULL OR settled_at IS NOT NULL)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(fabricated, 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn notices_of_a_lineage_with_no_live_seat_retire_after_the_bound() {
    let f = fixture();
    let old = (Utc::now() - chrono::Duration::days(30))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    seed_action_notice_at(&f, 0, old);
    seed_action_notice(&f, 1);
    f.store
        .update_session_status(f.anchor, SessionStatus::Archived)
        .unwrap();
    assert_eq!(f.store.retire_orphaned_manager_notices().unwrap(), 1);
    // The recent notice waits for the bound before it is retired too.
    assert_eq!(pending_count(&f.store), 1);
}

/// The replaced scope sorts after all 64 live rows, regardless of UUIDs.
fn seed_live_prefix_and_orphan(f: &Fixture) -> String {
    let old = (Utc::now() - chrono::Duration::days(30))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    for index in 0..64 {
        seed_action_notice_at(f, index, old.clone());
    }
    let orphan = Uuid::new_v4().to_string();
    f.store
        .conn
        .execute(
            "INSERT INTO harness_manager_notices
         (id,job_id,project_id,manager_session_id,scope_version,direction,
          recipient_session_id,kind,subject_id,subject_version,state_json,recorded_at,queued_at)
         VALUES(?1,?2,?3,?4,?5,'to_manager',?4,'action_result',?1,'1','{}',?6,?6)",
            params![
                orphan,
                Uuid::new_v4().to_string(),
                f.config.project_id.to_string(),
                f.anchor.to_string(),
                f.config.row_version + 1,
                old
            ],
        )
        .unwrap();
    orphan
}

fn assert_orphan_retired(store: &Store, orphan: &str) {
    let state: (bool, bool, bool) = store
        .conn
        .query_row(
            "SELECT retired_at IS NOT NULL,retrieved_at IS NULL,settled_at IS NULL
         FROM harness_manager_notices WHERE id=?1",
            [orphan],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(state, (true, true, true));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn live_owner_prefix_is_excluded_before_orphan_scan_limit() {
    let f = fixture();
    let orphan = seed_live_prefix_and_orphan(&f);
    for _ in 0..3 {
        f.store.reconcile_harness_manager_watches().unwrap();
        assert_orphan_retired(&f.store, &orphan);
        assert_eq!(retired_count(&f.store), 1);
        assert_eq!(pending_count(&f.store), 64);
        assert_eq!(total_count(&f.store), 65);
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn live_lineage_prefix_advances_orphan_cursor_across_restart_and_wraps() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("orphan-cursor.db");
    let f = fixture_with_store(Store::open(&database).unwrap());
    let orphan = seed_live_prefix_and_orphan(&f);
    let successor = rotate(&f.store, f.anchor);
    // All 64 candidates require Rust lineage checks and remain live.
    f.store.reconcile_harness_manager_watches().unwrap();
    assert_eq!(retired_count(&f.store), 0);
    assert_eq!(pending_count(&f.store), 65);
    drop(f);

    let store = Store::open(&database).unwrap();
    // The second pass continues beyond the live prefix even after restart.
    store.reconcile_harness_manager_watches().unwrap();
    assert_orphan_retired(&store, &orphan);
    assert_eq!(retired_count(&store), 1);
    assert_eq!(pending_count(&store), 64);

    // The short second page wraps: rows behind the cursor are reconsidered
    // when their last live seat subsequently disappears.
    store
        .update_session_status(successor, SessionStatus::Archived)
        .unwrap();
    store.reconcile_harness_manager_watches().unwrap();
    assert_eq!(retired_count(&store), 65);
    assert_eq!(pending_count(&store), 0);
    assert_eq!(total_count(&store), 65);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn orphan_retirement_is_capped_at_64_per_pass() {
    let f = fixture();
    seed_live_prefix_and_orphan(&f);
    f.store
        .update_session_status(f.anchor, SessionStatus::Archived)
        .unwrap();
    assert_eq!(f.store.retire_orphaned_manager_notices().unwrap(), 64);
    assert_eq!(retired_count(&f.store), 64);
    assert_eq!(pending_count(&f.store), 1);
    assert_eq!(f.store.retire_orphaned_manager_notices().unwrap(), 1);
    assert_eq!(retired_count(&f.store), 65);
    assert_eq!(total_count(&f.store), 65);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn settled_backlog_never_wakes_the_seat_again() {
    let f = fixture();
    for index in 0..40 {
        seed_action_notice(&f, index);
    }
    let (job, _, _) = f.store.manager_action_watch_identity(&f.config);
    assert_eq!(
        f.store.manager_watch_delivery_state(job).unwrap(),
        Some(true)
    );
    let tip = rotate(&f.store, f.anchor);
    page_all(&f.store, tip);
    // Everything was retrieved: the transport is quiet (not "notice-free",
    // which would take the legacy confirmation path and wake again) and its
    // scheduled job is disarmed.
    assert_eq!(
        f.store.manager_watch_delivery_state(job).unwrap(),
        Some(false)
    );
    assert_eq!(f.store.manager_notice_undelivered_count(job).unwrap(), 0);
    assert_eq!(
        f.store.manager_notice_first_wake_pending(job).unwrap(),
        None
    );
    assert!(!f.store.get_scheduled_job(&job).unwrap().unwrap().enabled);
}

// #1601: satellite reports remain reachable behind routine notice backlogs.
fn seed_inbox_notice(f: &Fixture, index: usize, satellite: bool) -> (Uuid, i64) {
    seed_inbox_notice_for_recipient(f, index, satellite, f.anchor)
}

fn seed_inbox_notice_for_recipient(
    f: &Fixture,
    index: usize,
    satellite: bool,
    recipient: Uuid,
) -> (Uuid, i64) {
    let (job, _, _) = f.store.manager_action_watch_identity(&f.config);
    f.store
        .ensure_manager_action_watch(&f.config, &format!("{index}"))
        .unwrap();
    let id = Uuid::new_v4();
    let recorded = crate::store::harness_manager_v2::now();
    let kind = if satellite {
        "ledger_change"
    } else {
        "action_result"
    };
    let subject = if satellite {
        "satellite_report".to_owned()
    } else {
        Uuid::new_v4().to_string()
    };
    f.store
        .conn
        .execute(
            "INSERT INTO harness_manager_notices
         (id,job_id,project_id,manager_session_id,scope_version,epic_id,direction,
          source_session_id,recipient_session_id,kind,subject_id,subject_version,
          state_json,recorded_at,queued_at)
         VALUES(?1,?2,?3,?4,?5,NULL,'to_manager',NULL,?6,?7,?8,?9,'{}',?10,?10)",
            params![
                id.to_string(),
                job.to_string(),
                f.config.project_id.to_string(),
                f.config.manager_session_id.to_string(),
                f.config.row_version,
                recipient.to_string(),
                kind,
                subject,
                Uuid::new_v4().to_string(),
                recorded
            ],
        )
        .unwrap();
    f.store.refresh_manager_notice_job(job).unwrap();
    let sequence = f
        .store
        .conn
        .query_row(
            "SELECT sequence FROM harness_manager_notices WHERE id=?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    (id, sequence)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn inbox_notice_cursor_reaches_later_satellite_report_and_settles_returned_pages() {
    let f = fixture();
    for index in 0..40 {
        seed_inbox_notice(&f, index, false);
    }
    let (report, _) = seed_inbox_notice(&f, 40, true);
    let first = inbox(&f.store, f.anchor, 0);
    assert_eq!(first.notices.len(), 32);
    assert!(first.more_notices);
    assert_eq!(
        first.next_after_sequence,
        Some(0),
        "legacy message cursor unchanged"
    );
    let after = first.next_after_notice_sequence.unwrap();
    assert_eq!(after, first.notices.last().unwrap().sequence);
    let second = f
        .store
        .manager_inbox(
            f.anchor,
            &AgentManagerInboxRequestV1 {
                after_notice_sequence: after,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(second.notices.len(), 9);
    assert_eq!(second.notices.last().unwrap().notice_id, report);
    assert_eq!(second.next_after_notice_sequence, None);
    assert!(!second.more_notices);
    assert_eq!(unsettled(&f.store), 0);
    let retrieved: i64 = f.store.conn.query_row(
        "SELECT count(*) FROM harness_manager_notices WHERE retrieved_at IS NOT NULL AND settled_at IS NOT NULL",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(retrieved, 41);
    assert!(inbox(&f.store, f.anchor, 0).notices.is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn inbox_notice_kind_filter_reaches_satellite_reports_beyond_scan_limit() {
    let f = fixture();
    for index in 0..300 {
        seed_inbox_notice(&f, index, false);
    }
    let (report, sequence) = seed_inbox_notice(&f, 300, true);
    let (later, _) = seed_inbox_notice(&f, 301, true);
    let mut request = AgentManagerInboxRequestV1 {
        notice_kind: Some("ledger_change".into()),
        limit: 1,
        ..Default::default()
    };
    let first = f.store.manager_inbox(f.anchor, &request).unwrap();
    assert_eq!(first.notices.len(), 1);
    assert_eq!(first.notices[0].notice_id, report);
    assert_eq!(first.next_after_notice_sequence, Some(sequence));
    request.after_notice_sequence = sequence;
    let second = f.store.manager_inbox(f.anchor, &request).unwrap();
    assert_eq!(second.notices.len(), 1);
    assert_eq!(second.notices[0].notice_id, later);
    assert!(!second.more_notices);
    assert_eq!(
        unsettled(&f.store),
        300,
        "filter leaves routine notices pending"
    );
    assert_eq!(inbox(&f.store, f.anchor, 0).notices.len(), 32);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn inbox_explicit_settlement_is_idempotent_and_survives_seat_succession() {
    let f = fixture();
    for index in 0..40 {
        seed_inbox_notice(&f, index, false);
    }
    let (report, sequence) = seed_inbox_notice(&f, 40, true);
    let tip = rotate(&f.store, f.anchor);
    let request = AgentManagerInboxRequestV1 {
        settle_notice_ids: vec![report],
        after_notice_sequence: sequence,
        ..Default::default()
    };
    let first = f.store.manager_inbox(tip, &request).unwrap();
    assert_eq!(first.settled_notice_ids, vec![report]);
    assert!(first.notices.is_empty());
    let timestamps = || {
        f.store
            .conn
            .query_row(
                "SELECT retrieved_at,settled_at FROM harness_manager_notices WHERE id=?1",
                [report.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap()
    };
    let before = timestamps();
    let retry = f.store.manager_inbox(tip, &request).unwrap();
    assert_eq!(retry.settled_notice_ids, vec![report]);
    assert_eq!(timestamps(), before);
    let pages = page_all(&f.store, tip);
    let returned: Vec<_> = pages.iter().flat_map(|page| &page.notices).collect();
    assert_eq!(returned.len(), 40);
    assert!(returned.iter().all(|notice| notice.kind == "action_result"));
    assert_eq!(unsettled(&f.store), 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn inbox_explicit_settlement_refuses_foreign_or_unknown_ids_atomically() {
    let f = fixture();
    let (owned, _) = seed_inbox_notice(&f, 0, false);
    let mut other = f.store.get_session(f.anchor).unwrap().unwrap();
    other.id = Uuid::new_v4();
    f.store.insert_session(&other).unwrap();
    let (foreign, _) = seed_inbox_notice_for_recipient(&f, 1, false, other.id);
    for invalid in [foreign, Uuid::new_v4()] {
        let error = f
            .store
            .manager_inbox(
                f.anchor,
                &AgentManagerInboxRequestV1 {
                    settle_notice_ids: vec![owned, invalid],
                    ..Default::default()
                },
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("manager_notice_not_authorized"));
        assert_eq!(
            unsettled(&f.store),
            2,
            "batch rolls back even owned settlement"
        );
        let retrieved: i64 = f
            .store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices WHERE retrieved_at IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retrieved, 0);
    }
}
