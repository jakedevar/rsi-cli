//! Store-level paging and default-filter tests for the operator
//! `ListScheduledJobs` read (Issue #954 B).

use std::collections::HashSet;

use chrono::{DateTime, Duration, Utc};
use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};
use uuid::Uuid;

use super::Store;
use super::scheduled_jobs::RETENTION_GRACE_MINUTES;
use crate::error::DaemonError;

fn job(
    enabled: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    last_fired_at: Option<DateTime<Utc>>,
) -> ScheduledJob {
    let id = Uuid::new_v4();
    ScheduledJob {
        id,
        name: format!("list-{id}"),
        message: String::new(),
        schedule: ScheduleSpec {
            recurrence: Recurrence::Once,
            anchor: created_at,
        },
        last_fired_at,
        next_fire_at: created_at,
        enabled,
        working_dir: None,
        provider: None,
        model: None,
        project_id: None,
        created_at,
        updated_at,
        wake_mode: WakeMode::Fresh,
        wake_session_id: None,
    }
}

fn old() -> Duration {
    Duration::minutes(RETENTION_GRACE_MINUTES + 15)
}

fn ids(jobs: &[ScheduledJob]) -> Vec<Uuid> {
    jobs.iter().map(|job| job.id).collect()
}

/// Reads every page with `limit`, returning the pages' ids in order.
fn read_all_pages(store: &Store, include_history: bool, limit: usize) -> Vec<Vec<Uuid>> {
    let mut pages = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let (jobs, next) = store
            .list_scheduled_jobs_page(Utc::now(), include_history, limit, cursor.as_deref())
            .expect("page");
        pages.push(ids(&jobs));
        match next {
            Some(next) => cursor = Some(next),
            None => return pages,
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn paging_over_three_pages_has_no_duplicates_or_gaps_newest_first() {
    let store = Store::open_in_memory().expect("store");
    let base = Utc::now() - Duration::hours(2);
    let mut expected = Vec::new();
    for index in 0..7 {
        let at = base + Duration::seconds(index);
        let row = job(true, at, at, None);
        expected.push(row.id);
        store.insert_scheduled_job(&row).expect("insert");
    }
    expected.reverse(); // newest created first

    let pages = read_all_pages(&store, false, 3);

    assert_eq!(
        pages.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![3, 3, 1],
        "3 pages: full, full, remainder"
    );
    assert_eq!(pages.concat(), expected);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn cursor_is_stable_when_rows_are_added_disabled_or_deleted_between_pages() {
    let store = Store::open_in_memory().expect("store");
    let base = Utc::now() - Duration::hours(2);
    let mut all = Vec::new();
    for index in 0..9 {
        let at = base + Duration::seconds(index);
        let row = job(true, at, at, None);
        all.push(row.id);
        store.insert_scheduled_job(&row).expect("insert");
    }
    all.reverse(); // newest first: pages of 3 are all[0..3], all[3..6], all[6..9]

    let (page1, cursor) = store
        .list_scheduled_jobs_page(Utc::now(), true, 3, None)
        .expect("page 1");
    assert_eq!(ids(&page1), all[0..3]);
    let cursor = cursor.expect("more pages");

    // Between pages: a brand-new row lands (newest, so before the cursor), an
    // already-returned row is deleted, and an unseen row is disabled.
    let newcomer = job(true, Utc::now(), Utc::now(), None);
    store.insert_scheduled_job(&newcomer).expect("newcomer");
    store.delete_scheduled_job(&all[1]).expect("delete seen");
    store.toggle_scheduled_job(&all[4]).expect("disable unseen");

    let (page2, cursor) = store
        .list_scheduled_jobs_page(Utc::now(), true, 3, Some(&cursor))
        .expect("page 2");
    assert_eq!(ids(&page2), all[3..6], "no gap and no duplicate");
    let (page3, cursor) = store
        .list_scheduled_jobs_page(Utc::now(), true, 3, Some(&cursor.expect("third page")))
        .expect("page 3");
    assert_eq!(ids(&page3), all[6..9]);
    assert!(cursor.is_none(), "the last page carries no cursor");

    let seen: HashSet<Uuid> = page1
        .iter()
        .chain(&page2)
        .chain(&page3)
        .map(|job| job.id)
        .collect();
    assert_eq!(seen.len(), 9, "every original row exactly once");
    assert!(!seen.contains(&newcomer.id));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn rows_sharing_a_created_at_page_stably_by_id() {
    let store = Store::open_in_memory().expect("store");
    let at = Utc::now() - Duration::hours(1);
    let mut inserted = HashSet::new();
    for _ in 0..5 {
        let row = job(true, at, at, None);
        inserted.insert(row.id);
        store.insert_scheduled_job(&row).expect("insert");
    }

    let pages = read_all_pages(&store, false, 2);

    assert_eq!(
        pages.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![2, 2, 1]
    );
    let flat = pages.concat();
    assert_eq!(flat.iter().copied().collect::<HashSet<_>>(), inserted);
    let mut descending = flat.clone();
    descending.sort_by(|a, b| b.to_string().cmp(&a.to_string()));
    assert_eq!(flat, descending, "ties break by id descending");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn default_filter_shows_enabled_and_recent_and_history_toggle_shows_all() {
    let store = Store::open_in_memory().expect("store");
    let now = Utc::now();
    let long_ago = now - old();
    let recent = now - Duration::minutes(5);
    let enabled_old = job(true, long_ago, long_ago, None);
    let disabled_old = job(false, long_ago, long_ago, Some(long_ago));
    let disabled_recently = job(false, long_ago, recent, None);
    let fired_recently = job(false, long_ago, long_ago, Some(recent));
    for row in [
        &enabled_old,
        &disabled_old,
        &disabled_recently,
        &fired_recently,
    ] {
        store.insert_scheduled_job(row).expect("insert");
    }

    let (default, none) = store
        .list_scheduled_jobs_page(now, false, 50, None)
        .expect("default filter");
    let shown: HashSet<Uuid> = ids(&default).into_iter().collect();
    assert_eq!(
        shown,
        HashSet::from([enabled_old.id, disabled_recently.id, fired_recently.id]),
        "enabled + recently fired/disabled; old disabled history hidden"
    );
    assert!(none.is_none());

    let (history, _) = store
        .list_scheduled_jobs_page(now, true, 50, None)
        .expect("include history");
    assert_eq!(history.len(), 4, "include_history returns every row");
    assert!(ids(&history).contains(&disabled_old.id));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn an_invalid_cursor_is_refused() {
    let store = Store::open_in_memory().expect("store");
    for cursor in [
        "garbage",
        "@2026-01-01T00:00:00.000000000Z",
        "not-a-uuid@2026-01-01T00:00:00.000000000Z",
        "00000000-0000-0000-0000-00000000000A@2026-01-01T00:00:00.000000000Z",
        "00000000-0000-0000-0000-000000000001@",
    ] {
        let error = store
            .list_scheduled_jobs_page(Utc::now(), false, 10, Some(cursor))
            .expect_err(cursor);
        assert!(
            matches!(&error, DaemonError::InvalidParam(_)),
            "{cursor}: {error:?}"
        );
    }
}
