//! Cold-archive tests, slice S4 (#2384): the production move (P6), the
//! fresh-backup gate, crash and resume, batch isolation, reclaim, the
//! compaction helper and notices. The GM's three conditions are each a test
//! here: P6 deletes only what this pass verified (`a_crash_*`, `*_reverified_*`),
//! a move refuses without a fresh backup (`*_backup_*`), and a restore
//! round-trips every event and provenance row (`restore_round_trips_*`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::similar_names
)]

use super::backup::{BACKUP_STALE_CODE, PASS_BACKUP_PREFIX, backups_dir};
use super::mover::{GUARD_BUSY_CODE, MoveContext, NOT_ADMITTED_CODE, VerifiedChunks};
use super::notices::NOTICE_KEY_PREFIX;
use super::pass::MoveSeams;
use super::reclaim::{
    COMPACT_INTERRUPTED_CODE, CompactResult, compact_paths, reclaim_incremental, run_compaction,
    write_compact_result,
};
use super::tests::Fx;
use super::*;
use crate::store::Store;
use crate::store_support::spawn_single_flight::try_acquire_spawn_guard;
use rsi_common::types::SessionStatus;
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use uuid::Uuid;

const ALL_EVENTS_SQL: &str = "SELECT id, session_id, sequence, event_type, role, content,
        tool_name, tool_input, created_at, tool_use_id, metadata
     FROM conversation_events ORDER BY id";
const ALL_PROVENANCE_SQL: &str = "SELECT conversation_event_id, producer_kind,
        model_invocation_id, provider_event_type, created_at
     FROM conversation_event_provenance ORDER BY conversation_event_id";
const SESSION_EVENTS_SQL: &str = "SELECT COUNT(*) FROM conversation_events WHERE session_id = ?1";

fn limits() -> PassLimits {
    PassLimits {
        batch_max_sessions: 32,
        batch_max_raw_bytes: 64 * 1024 * 1024,
        wall_budget: Duration::from_secs(120),
        batch_sleep: Duration::ZERO,
        gate_poll: Duration::ZERO,
    }
}

fn move_pass_with(fx: &Fx, limits: PassLimits) -> ColdResult<StageReport> {
    let settings = fx.settings(14);
    let run_id = fx.store.cold_begin_run("run_now", ColdMode::Move).unwrap();
    let params = PassParams {
        run_id,
        settings: &settings,
        limits,
        batch_gate: None,
        cancel: None,
    };
    run_move_pass(&DirectStoreAccess(&fx.store), &fx.read, &params, &fx.db)
}

fn move_pass(fx: &Fx) -> ColdResult<StageReport> {
    move_pass_with(fx, limits())
}

fn table(conn: &Connection, sql: &str, columns: usize) -> Vec<Vec<Value>> {
    let mut stmt = conn.prepare(sql).unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut out = Vec::new();
    while let Some(row) = rows.next().unwrap() {
        out.push(
            (0..columns)
                .map(|index| row.get::<_, Value>(index).unwrap())
                .collect(),
        );
    }
    out
}

/// Every event and provenance row of the database.
fn snapshot(fx: &Fx) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    (
        table(&fx.read, ALL_EVENTS_SQL, 11),
        table(&fx.read, ALL_PROVENANCE_SQL, 5),
    )
}

fn restore(fx: &Fx, id: Uuid) -> RestoreReceipt {
    let guard = try_acquire_spawn_guard(id).expect("no other holder of the guard");
    fx.store.restore_cold_session(&guard, id).unwrap()
}

fn state(fx: &Fx, id: Uuid) -> String {
    fx.row(id).map(|row| row.state).unwrap_or_default()
}

fn live_events(fx: &Fx, id: Uuid) -> i64 {
    fx.count(SESSION_EVENTS_SQL, Some(id))
}

fn pass_owned_backups(fx: &Fx) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(backups_dir(&fx.db))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with(PASS_BACKUP_PREFIX))
                })
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

fn set_age(path: &Path, age: Duration) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() - age)
        .unwrap();
}

/// A `VACUUM INTO` copy of the live database in the backups directory.
fn backup_copy(fx: &Fx, name: &str) -> PathBuf {
    let dir = backups_dir(&fx.db);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    fx.read
        .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
        .unwrap();
    path
}

// ---------------------------------------------------------------------------
// The move, end to end
// ---------------------------------------------------------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_move_pass_moves_the_cold_set_reads_it_whole_and_conserves_every_row() {
    let fx = Fx::new();
    let first = fx.cold_session(SessionStatus::Archived, 60, 3);
    let second = fx.cold_session(SessionStatus::Deleted, 90, 2);
    // Too new to be cold: never touched.
    let recent = fx.cold_session(SessionStatus::Archived, 2, 2);
    let (events_before, provenance_before) = snapshot(&fx);
    let transcripts: Vec<_> = [first, second, recent]
        .iter()
        .map(|id| serde_json::to_value(fx.store.load_events(*id).unwrap()).unwrap())
        .collect();

    let report = move_pass(&fx).unwrap();
    assert_eq!(report.moved.sessions_moved, 2, "{report:?}");
    assert_eq!(report.sessions_failed, 0, "{report:?}");
    assert_eq!(state(&fx, first), "cold");
    assert_eq!(state(&fx, second), "cold");
    assert!(fx.row(recent).is_none(), "a recent session is never staged");
    assert!(
        report.moved.backup_produced,
        "no backup existed: the pass took one"
    );

    // Per table: live + archived == before; per session: pre == archived + live.
    let (events_after, provenance_after) = snapshot(&fx);
    let mut archived_events = 0;
    let mut archived_provenance = 0;
    for id in [first, second] {
        let row = fx.row(id).unwrap();
        assert_eq!(
            row.pre_event_count,
            row.archived_event_count + live_events(&fx, id)
        );
        assert!(row.archived_event_count > 0);
        archived_events += row.archived_event_count;
        archived_provenance += row.archived_provenance_count;
    }
    assert_eq!(
        i64::try_from(events_after.len()).unwrap() + archived_events,
        i64::try_from(events_before.len()).unwrap()
    );
    assert_eq!(
        i64::try_from(provenance_after.len()).unwrap() + archived_provenance,
        i64::try_from(provenance_before.len()).unwrap()
    );
    assert_eq!(
        u64::try_from(archived_events).unwrap(),
        report.moved.events_moved
    );

    // Every reader still gets the whole transcript.
    for (id, before) in [first, second, recent].iter().zip(&transcripts) {
        let after = serde_json::to_value(fx.store.load_events(*id).unwrap()).unwrap();
        assert_eq!(&after, before, "transcript of {id} changed");
    }

    // The run row records the move.
    let summary = report.run_summary(None);
    assert_eq!(summary.sessions_moved, 2);
    assert_eq!(summary.events_moved, archived_events);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn restore_round_trips_every_event_and_provenance_row_after_a_production_move() {
    // GM condition 3: move one session, restore it, and compare every event and
    // provenance row (every column, ids included) with the pre-move snapshot.
    let fx = Fx::new();
    let id = fx.cold_session(SessionStatus::Archived, 45, 4);
    let bystander = fx.cold_session(SessionStatus::Archived, 1, 1);
    let before = snapshot(&fx);

    let report = move_pass(&fx).unwrap();
    assert_eq!(report.moved.sessions_moved, 1, "{report:?}");
    assert_eq!(state(&fx, id), "cold");
    assert_ne!(snapshot(&fx).0, before.0, "rows really left the live table");

    let receipt = restore(&fx, id);
    assert_eq!(receipt.outcome, "restored");
    assert_eq!(state(&fx, id), "restored");
    let after = snapshot(&fx);
    assert_eq!(after.0, before.0, "every event row is back, exactly");
    assert_eq!(after.1, before.1, "every provenance row is back, exactly");
    assert!(live_events(&fx, bystander) > 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_second_move_pass_writes_no_chunk_and_deletes_no_row() {
    let fx = Fx::new();
    fx.cold_session(SessionStatus::Archived, 60, 2);
    move_pass(&fx).unwrap();
    let chunks = fx.archive_chunk_count();
    let counts = fx.live_counts();
    let report = move_pass(&fx).unwrap();
    assert_eq!(report.moved.sessions_moved, 0, "{report:?}");
    assert_eq!(fx.archive_chunk_count(), chunks);
    assert_eq!(fx.live_counts(), counts);
}

// ---------------------------------------------------------------------------
// GM condition 1: delete only what this pass read back and verified
// ---------------------------------------------------------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_crash_before_the_move_commits_loses_nothing_and_the_resumed_pass_re_verifies_first() {
    let fx = Fx::new();
    let id = fx.cold_session(SessionStatus::Archived, 60, 3);
    let before = snapshot(&fx);

    failpoint::arm("move_before_commit");
    let error = move_pass(&fx).unwrap_err();
    failpoint::disarm();
    assert_eq!(error.code, "cold_archive_injected_failure");
    assert_eq!(
        snapshot(&fx),
        before,
        "the rolled-back move deleted nothing"
    );
    assert_eq!(state(&fx, id), "staged");
    assert!(fx.row(id).unwrap().verified_at.is_some());

    // The resumed pass must read the chunk back again before it deletes: with
    // the archive file gone it deletes nothing.
    let file = fx.archive_paths().pop().unwrap();
    let aside = file.with_extension("aside");
    std::fs::rename(&file, &aside).unwrap();
    let report = move_pass(&fx).unwrap();
    assert_eq!(report.moved.sessions_moved, 0, "{report:?}");
    assert_eq!(report.sessions_failed, 1, "{report:?}");
    assert_eq!(snapshot(&fx), before);
    let row = fx.row(id).unwrap();
    assert_eq!(row.state, "staged");
    assert!(row.last_error_code.is_some());
    assert!(row.verified_at.is_none());

    // With the file back, the next pass verifies again and moves.
    std::fs::rename(&aside, &file).unwrap();
    let report = move_pass(&fx).unwrap();
    assert_eq!(report.moved.sessions_moved, 1, "{report:?}");
    assert_eq!(state(&fx, id), "cold");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_session_staged_by_an_earlier_pass_is_reverified_before_it_moves() {
    let fx = Fx::new();
    let id = fx.cold_session(SessionStatus::Archived, 60, 2);
    let staged = fx.stage(14).unwrap();
    assert_eq!(staged.sessions_staged, 1);
    assert_eq!(live_events(&fx, id), fx.row(id).unwrap().pre_event_count);

    let report = move_pass(&fx).unwrap();
    assert_eq!(report.moved.sessions_reverified, 1, "{report:?}");
    assert_eq!(report.moved.sessions_moved, 1, "{report:?}");
    assert_eq!(state(&fx, id), "cold");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_corrupt_chunk_found_on_read_back_keeps_every_row() {
    let fx = Fx::new();
    let id = fx.cold_session(SessionStatus::Archived, 60, 2);
    fx.stage(14).unwrap();
    let before = snapshot(&fx);
    // Flip the archived digest the index row records: the read-back fails.
    let file = fx.archive_paths().pop().unwrap();
    let conn = Connection::open(&file).unwrap();
    let triggers: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'trigger'")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    for trigger in triggers {
        conn.execute_batch(&format!("DROP TRIGGER {trigger};"))
            .unwrap();
    }
    conn.execute("UPDATE chunks SET payload = zeroblob(16)", [])
        .unwrap();
    drop(conn);

    let report = move_pass(&fx).unwrap();
    assert_eq!(report.moved.sessions_moved, 0, "{report:?}");
    assert_eq!(snapshot(&fx), before);
    assert_eq!(state(&fx, id), "staged");
}

// ---------------------------------------------------------------------------
// GM condition 2: only while a fresh backup exists
// ---------------------------------------------------------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_move_pass_without_a_fresh_backup_refuses_and_deletes_nothing() {
    let fx = Fx::new();
    let id = fx.cold_session(SessionStatus::Archived, 60, 2);
    let before = snapshot(&fx);
    // The backups directory cannot be created: no backup can be found or taken.
    std::fs::write(backups_dir(&fx.db), b"not a directory").unwrap();

    let error = move_pass(&fx).unwrap_err();
    assert_eq!(error.code, BACKUP_STALE_CODE, "{error}");
    assert_eq!(snapshot(&fx), before, "nothing was deleted");
    let row = fx.row(id).unwrap();
    assert_eq!(row.state, "staged", "staging is kept for the next pass");
    assert!(row.verified_at.is_some());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_fresh_pre_migration_backup_qualifies_and_the_pass_takes_no_copy() {
    let fx = Fx::new();
    fx.cold_session(SessionStatus::Archived, 60, 2);
    let pre = backup_copy(&fx, "rsi-pre-v175-20261014T043711.970600078Z.db");

    let report = move_pass(&fx).unwrap();
    assert_eq!(report.moved.sessions_moved, 1, "{report:?}");
    assert!(!report.moved.backup_produced);
    assert_eq!(report.moved.backup.as_deref(), Some(pre.to_str().unwrap()));
    assert!(pass_owned_backups(&fx).is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn stale_or_foreign_backups_do_not_qualify_and_the_pass_takes_its_own() {
    let fx = Fx::new();
    fx.cold_session(SessionStatus::Archived, 60, 2);
    let dir = backups_dir(&fx.db);
    // A real backup, 25 hours old.
    let stale = backup_copy(&fx, "rsi-pre-v174-old.db");
    set_age(&stale, Duration::from_secs(25 * 3600));
    // Fresh, but another application's database, a newer schema, or no database.
    for (name, application_id, user_version) in [("foreign.db", 7, 1), ("newer.db", 0, 999_999)] {
        let conn = Connection::open(dir.join(name)).unwrap();
        conn.execute_batch(&format!(
            "PRAGMA application_id = {application_id}; PRAGMA user_version = {user_version};
             CREATE TABLE t(x);"
        ))
        .unwrap();
    }
    std::fs::write(dir.join("garbage.db"), b"not sqlite at all").unwrap();

    let report = move_pass(&fx).unwrap();
    assert_eq!(report.moved.sessions_moved, 1, "{report:?}");
    assert!(report.moved.backup_produced, "{report:?}");
    let owned = pass_owned_backups(&fx);
    assert_eq!(owned.len(), 1, "{owned:?}");
    assert_eq!(
        report.moved.backup.as_deref(),
        Some(owned[0].to_str().unwrap())
    );
    // The copy is a readable database with the live schema version.
    let copy = Connection::open_with_flags(&owned[0], OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let version: i64 = copy
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, i64::from(fx.store.schema_version().unwrap()));
    assert!(
        stale.exists(),
        "a pre-migration backup is never removed by the pass"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn the_pass_keeps_only_its_newest_backup_copy() {
    let fx = Fx::new();
    fx.cold_session(SessionStatus::Archived, 60, 1);
    let old = backup_copy(&fx, &format!("{PASS_BACKUP_PREFIX}20260101T000000Z.db"));
    set_age(&old, Duration::from_secs(30 * 3600));
    let partial = backups_dir(&fx.db).join(format!("{PASS_BACKUP_PREFIX}20260102T000000Z.db.tmp"));
    std::fs::write(&partial, b"half written").unwrap();

    let report = move_pass(&fx).unwrap();
    assert!(report.moved.backup_produced, "{report:?}");
    let owned = pass_owned_backups(&fx);
    assert_eq!(owned.len(), 1, "{owned:?}");
    assert_ne!(owned[0], old);
    assert!(
        !old.exists(),
        "the older pass-owned copy went after the new one qualified"
    );
    assert!(
        !partial.exists(),
        "a partial copy of a crashed pass is removed"
    );
}

// ---------------------------------------------------------------------------
// Batches, guards, changed rows
// ---------------------------------------------------------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_failing_session_is_isolated_and_the_rest_of_its_batch_moves() {
    let fx = Fx::new();
    let ids: Vec<Uuid> = (0..3)
        .map(|_| fx.cold_session(SessionStatus::Archived, 60, 1))
        .collect();
    let culprit = ids[1];
    let culprit_rows = live_events(&fx, culprit);

    failpoint::fail_session(&culprit.to_string());
    let report = move_pass(&fx);
    failpoint::clear_session_failure();
    let report = report.unwrap();
    assert_eq!(report.moved.sessions_moved, 2, "{report:?}");
    assert_eq!(report.sessions_failed, 1, "{report:?}");
    assert_eq!(state(&fx, ids[0]), "cold");
    assert_eq!(state(&fx, ids[2]), "cold");
    let row = fx.row(culprit).unwrap();
    assert_eq!(row.state, "staged");
    assert_eq!(row.last_error_code.as_deref(), Some("cold_move_injected"));
    assert_eq!(
        live_events(&fx, culprit),
        culprit_rows,
        "the culprit kept its rows"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn p6_skips_a_session_whose_spawn_guard_is_held() {
    let fx = Fx::new();
    let busy = fx.cold_session(SessionStatus::Archived, 60, 1);
    let free = fx.cold_session(SessionStatus::Archived, 60, 1);
    let rows = live_events(&fx, busy);
    let guard = try_acquire_spawn_guard(busy).unwrap();

    let report = move_pass(&fx).unwrap();
    assert_eq!(report.holds.get(GUARD_BUSY_CODE), Some(&1), "{report:?}");
    assert_eq!(state(&fx, busy), "staged");
    assert_eq!(live_events(&fx, busy), rows);
    assert_eq!(state(&fx, free), "cold");

    drop(guard);
    let report = move_pass(&fx).unwrap();
    assert_eq!(report.moved.sessions_moved, 1, "{report:?}");
    assert_eq!(state(&fx, busy), "cold");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn small_batches_move_everything_in_several_transactions() {
    let fx = Fx::new();
    let ids: Vec<Uuid> = (0..5)
        .map(|_| fx.cold_session(SessionStatus::Archived, 60, 1))
        .collect();
    let report = move_pass_with(
        &fx,
        PassLimits {
            batch_max_sessions: 2,
            ..limits()
        },
    )
    .unwrap();
    assert_eq!(report.moved.sessions_moved, 5, "{report:?}");
    assert_eq!(report.moved.move_batches, 3, "{report:?}");
    for id in ids {
        assert_eq!(state(&fx, id), "cold");
    }
}

// ---------------------------------------------------------------------------
// The S4 review's findings (#202-#205): the backup is derived per transaction,
// resumed sessions keep their column metadata and their throttle, a cancelled
// pass stops at a safe boundary
// ---------------------------------------------------------------------------

fn never() -> bool {
    false
}

fn always() -> bool {
    true
}

fn quiet() {}

fn no_skew() -> Duration {
    Duration::ZERO
}

/// A move pass with a host-load gate, a cancel flag and a clock the test moves.
fn move_pass_hooked(
    fx: &Fx,
    limits: PassLimits,
    gate: &dyn Fn() -> bool,
    cancel: &dyn Fn() -> bool,
    clock: &dyn Fn() -> SystemTime,
) -> ColdResult<StageReport> {
    let seams = MoveSeams {
        wall: clock,
        skew: &no_skew,
        before_store: &quiet,
        in_writer: &quiet,
        after_qualify: &quiet,
    };
    move_pass_seamed(fx, limits, gate, cancel, &seams)
}

/// [`move_pass_hooked`] with every clock and both wait handshakes of the pass:
/// `before_store` runs after the backup qualified and before the store is
/// requested, `in_writer` with the store and `SQLite`'s write lock held.
fn move_pass_seamed(
    fx: &Fx,
    limits: PassLimits,
    gate: &dyn Fn() -> bool,
    cancel: &dyn Fn() -> bool,
    seams: &MoveSeams<'_>,
) -> ColdResult<StageReport> {
    let settings = fx.settings(14);
    let run_id = fx.store.cold_begin_run("run_now", ColdMode::Move).unwrap();
    let params = PassParams {
        run_id,
        settings: &settings,
        limits,
        batch_gate: Some(gate),
        cancel: Some(cancel),
    };
    super::pass::run_move_pass_with_seams(
        &DirectStoreAccess(&fx.store),
        &fx.read,
        &params,
        &fx.db,
        seams,
    )
}

fn one_session_batches() -> PassLimits {
    PassLimits {
        batch_max_sessions: 1,
        ..limits()
    }
}

/// Sessions the index holds in the cold state.
fn cold_sessions(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM cold_archive_sessions WHERE state = 'cold'",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

/// Old cold-candidate sessions in the order a pass walks them (by id).
fn cold_candidates(fx: &Fx, count: usize) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = (0..count)
        .map(|_| fx.cold_session(SessionStatus::Archived, 60, 1))
        .collect();
    ids.sort_by_key(|id| id.to_string());
    ids
}

/// The event and provenance rows of one session out of a [`snapshot`].
fn rows_of(
    snap: &(Vec<Vec<Value>>, Vec<Vec<Value>>),
    id: Uuid,
) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    let session = Value::Text(id.to_string());
    let events: Vec<Vec<Value>> = snap
        .0
        .iter()
        .filter(|row| row[1] == session)
        .cloned()
        .collect();
    let event_ids: Vec<Value> = events.iter().map(|row| row[0].clone()).collect();
    let provenance = snap
        .1
        .iter()
        .filter(|row| event_ids.contains(&row[0]))
        .cloned()
        .collect();
    (events, provenance)
}

fn touch(path: &Path, at: SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(at)
        .unwrap();
}

fn split_by_cold(fx: &Fx, ids: &[Uuid]) -> (Vec<Uuid>, Vec<Uuid>) {
    ids.iter().copied().partition(|id| state(fx, *id) == "cold")
}

fn restore_all(fx: &Fx, ids: &[Uuid]) {
    for id in ids {
        assert_eq!(restore(fx, *id).outcome, "restored");
    }
}

// --- #202: the backup is a per-transaction proof, not a once-true flag -----

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_backup_that_turns_24_hours_old_between_batches_ends_the_deleting_and_a_fresh_one_resumes_it() {
    let fx = Fx::new();
    let ids = [
        fx.cold_session(SessionStatus::Archived, 60, 3),
        fx.cold_session(SessionStatus::Archived, 61, 2),
    ];
    let before = snapshot(&fx);
    // The pass reads its clock `skew` seconds ahead of the real one.
    let skew = Cell::new(0_u64);
    let clock = || SystemTime::now() + Duration::from_secs(skew.get());
    // The first batch commits under the copy the pass takes; then 25 hours pass.
    let jumped = Cell::new(false);
    let gate = || {
        if cold_sessions(&fx.read) >= 1 && !jumped.replace(true) {
            skew.set(25 * 3600);
        }
        true
    };

    let report = move_pass_hooked(&fx, one_session_batches(), &gate, &never, &clock).unwrap();
    assert!(jumped.get(), "the first batch committed before the jump");
    let (moved, kept) = split_by_cold(&fx, &ids);
    assert_eq!((moved.len(), kept.len()), (1, 1), "{report:?}");
    assert_eq!(report.moved.sessions_moved, 1, "{report:?}");
    assert_eq!(report.moved.sessions_deferred, 1, "{report:?}");
    let refusal = report
        .move_refused
        .clone()
        .expect("the second transaction was refused");
    assert_eq!(refusal.code, BACKUP_STALE_CODE);
    // Nothing of the refused session left the live tables.
    assert_eq!(state(&fx, kept[0]), "staged");
    assert_eq!(rows_of(&snapshot(&fx), kept[0]), rows_of(&before, kept[0]));
    // What the first batch committed stays accounted, and the run says why it
    // stopped; the notice is raised from that summary.
    let summary = report.run_summary(None);
    assert_eq!(summary.outcome, "partial");
    assert_eq!(summary.sessions_moved, 1);
    assert_eq!(summary.error_code.as_deref(), Some(BACKUP_STALE_CODE));
    fx.store.cold_archive_settle_run_notices(&summary);
    let notices = open_notices(&fx);
    assert!(
        notices.iter().any(|key| key.contains(BACKUP_STALE_CODE)),
        "{notices:?}"
    );

    // A replacement that is fresh by the pass's clock lets the rest move.
    let replacement = backup_copy(&fx, "rsi-pre-v999-replacement.db");
    touch(
        &replacement,
        SystemTime::now() + Duration::from_secs(skew.get()),
    );
    let report = move_pass_hooked(&fx, one_session_batches(), &always, &never, &clock).unwrap();
    assert_eq!(report.moved.sessions_moved, 1, "{report:?}");
    assert!(report.move_refused.is_none(), "{report:?}");
    assert_eq!(
        report.moved.backup.as_deref(),
        Some(replacement.to_str().unwrap())
    );
    assert!(!report.moved.backup_produced);
    assert_eq!(cold_sessions(&fx.read), 2);

    // Both sessions restore exactly.
    restore_all(&fx, &ids);
    assert_eq!(snapshot(&fx), before);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_backup_removed_between_batches_ends_the_deleting_when_none_can_be_taken() {
    let fx = Fx::new();
    let ids = [
        fx.cold_session(SessionStatus::Archived, 60, 3),
        fx.cold_session(SessionStatus::Archived, 61, 2),
    ];
    let before = snapshot(&fx);
    let dir = backups_dir(&fx.db);
    let removed = Cell::new(false);
    // After the first batch the pass's own copy is gone, and a file stands where
    // the directory was, so no replacement can be taken.
    let gate = || {
        if cold_sessions(&fx.read) >= 1 && !removed.replace(true) {
            std::fs::remove_dir_all(&dir).unwrap();
            std::fs::write(&dir, b"not a directory").unwrap();
        }
        true
    };

    let report =
        move_pass_hooked(&fx, one_session_batches(), &gate, &never, &SystemTime::now).unwrap();
    let (moved, kept) = split_by_cold(&fx, &ids);
    assert_eq!((moved.len(), kept.len()), (1, 1), "{report:?}");
    assert_eq!(
        report
            .move_refused
            .as_ref()
            .map(|refusal| refusal.code.as_str()),
        Some(BACKUP_STALE_CODE),
        "{report:?}"
    );
    assert_eq!(rows_of(&snapshot(&fx), kept[0]), rows_of(&before, kept[0]));

    // With the directory usable again the next pass takes a copy and finishes.
    std::fs::remove_file(&dir).unwrap();
    let report = move_pass(&fx).unwrap();
    assert_eq!(report.moved.sessions_moved, 1, "{report:?}");
    assert!(report.moved.backup_produced, "{report:?}");
    restore_all(&fx, &ids);
    assert_eq!(snapshot(&fx), before);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_backup_replaced_by_a_non_database_is_not_relied_on_again() {
    let fx = Fx::new();
    let ids = [
        fx.cold_session(SessionStatus::Archived, 60, 3),
        fx.cold_session(SessionStatus::Archived, 61, 2),
    ];
    let before = snapshot(&fx);
    let replaced = RefCell::new(None::<PathBuf>);
    let gate = || {
        if cold_sessions(&fx.read) >= 1 && replaced.borrow().is_none() {
            let copy = pass_owned_backups(&fx).pop().unwrap();
            std::fs::write(&copy, b"not a database").unwrap();
            *replaced.borrow_mut() = Some(copy);
        }
        true
    };

    let report =
        move_pass_hooked(&fx, one_session_batches(), &gate, &never, &SystemTime::now).unwrap();
    assert_eq!(report.moved.sessions_moved, 2, "{report:?}");
    assert!(report.move_refused.is_none(), "{report:?}");
    // The second batch took a new copy, and the unusable one is gone.
    let replaced = replaced.borrow().clone().unwrap();
    let owned = pass_owned_backups(&fx);
    assert_eq!(owned.len(), 1, "{owned:?}");
    assert_ne!(owned[0], replaced);
    assert!(!replaced.exists());
    assert!(report.moved.backup_produced);
    assert_eq!(
        report.moved.backup.as_deref(),
        Some(owned[0].to_str().unwrap())
    );
    restore_all(&fx, &ids);
    assert_eq!(snapshot(&fx), before);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_backup_in_its_last_minutes_is_replaced_before_the_pass_deletes() {
    let fx = Fx::new();
    fx.cold_session(SessionStatus::Archived, 60, 2);
    // Two minutes of life left: inside the reserve a transaction needs.
    let nearly = backup_copy(&fx, "rsi-pre-v174-nearly-stale.db");
    set_age(&nearly, Duration::from_secs(24 * 3600 - 120));

    let report = move_pass(&fx).unwrap();
    assert_eq!(report.moved.sessions_moved, 1, "{report:?}");
    assert!(report.moved.backup_produced, "{report:?}");
    assert_ne!(
        report.moved.backup.as_deref(),
        Some(nearly.to_str().unwrap())
    );
    assert!(nearly.exists(), "a pre-migration backup is never removed");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_backup_out_of_reserve_at_the_transaction_boundary_is_refused_and_nothing_is_deleted() {
    let fx = Fx::new();
    let id = fx.cold_session(SessionStatus::Archived, 60, 2);
    let before = snapshot(&fx);
    // The first look at the clock admits the backup; the second, at the
    // transaction boundary with the locks held, finds 25 hours gone, every time
    // the pass goes round again.
    let looks = Cell::new(0_u32);
    let clock = || {
        looks.set(looks.get() + 1);
        if looks.get() % 2 == 0 {
            SystemTime::now() + Duration::from_secs(25 * 3600)
        } else {
            SystemTime::now()
        }
    };

    let error = move_pass_hooked(&fx, limits(), &always, &never, &clock).unwrap_err();
    assert_eq!(error.code, BACKUP_STALE_CODE, "{error}");
    assert_eq!(snapshot(&fx), before, "nothing was deleted");
    assert_eq!(state(&fx, id), "staged");
}

// --- #202 (boundary): the backup is proved again after the store and writer waits ---

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_backup_removed_after_it_qualified_while_the_move_waits_for_the_store_is_never_relied_on() {
    let fx = Fx::new();
    let ids = [
        fx.cold_session(SessionStatus::Archived, 60, 3),
        fx.cold_session(SessionStatus::Archived, 61, 2),
    ];
    let before = snapshot(&fx);
    let dir = backups_dir(&fx.db);
    let removed = Cell::new(false);
    // The second batch's backup has qualified and the pass is about to ask for the
    // store; while it waits the pass's copy disappears and a file stands where the
    // backups directory was, so no other backup can be taken. Time has not run out.
    let before_store = || {
        if cold_sessions(&fx.read) >= 1 && !removed.replace(true) {
            std::fs::remove_dir_all(&dir).unwrap();
            std::fs::write(&dir, b"not a directory").unwrap();
        }
    };
    let seams = MoveSeams {
        wall: &SystemTime::now,
        skew: &no_skew,
        before_store: &before_store,
        in_writer: &quiet,
        after_qualify: &quiet,
    };

    let report = move_pass_seamed(&fx, one_session_batches(), &always, &never, &seams).unwrap();
    assert!(
        removed.get(),
        "the first batch committed, the second waited"
    );
    let (moved, kept) = split_by_cold(&fx, &ids);
    assert_eq!((moved.len(), kept.len()), (1, 1), "{report:?}");
    assert_eq!(report.moved.sessions_moved, 1, "{report:?}");
    assert_eq!(report.moved.sessions_deferred, 1, "{report:?}");
    assert_eq!(
        report
            .move_refused
            .as_ref()
            .map(|refusal| refusal.code.as_str()),
        Some(BACKUP_STALE_CODE),
        "{report:?}"
    );
    assert_eq!(state(&fx, kept[0]), "staged");
    assert_eq!(rows_of(&snapshot(&fx), kept[0]), rows_of(&before, kept[0]));
    let summary = report.run_summary(None);
    assert_eq!(summary.outcome, "partial");
    assert_eq!(summary.sessions_moved, 1, "the first batch stays counted");
    assert_eq!(summary.error_code.as_deref(), Some(BACKUP_STALE_CODE));
    fx.store.cold_archive_settle_run_notices(&summary);
    let notices = open_notices(&fx);
    assert!(
        notices.iter().any(|key| key.contains(BACKUP_STALE_CODE)),
        "{notices:?}"
    );

    // A fresh backup lets the rest move, and both sessions restore exactly.
    std::fs::remove_file(&dir).unwrap();
    let report = move_pass(&fx).unwrap();
    assert_eq!(report.moved.sessions_moved, 1, "{report:?}");
    assert!(report.moved.backup_produced, "{report:?}");
    restore_all(&fx, &ids);
    assert_eq!(snapshot(&fx), before);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_backup_replaced_after_it_qualified_while_the_move_waits_is_replaced_outside_the_store() {
    let fx = Fx::new();
    let ids = [
        fx.cold_session(SessionStatus::Archived, 60, 3),
        fx.cold_session(SessionStatus::Archived, 61, 2),
    ];
    let before = snapshot(&fx);
    let waits = Cell::new(0_u32);
    let replaced = RefCell::new(None::<PathBuf>);
    // While the second batch waits for the store, the pass's copy is overwritten
    // with something that is not a database. The directory stays usable, so the
    // pass takes a new copy (outside the store) and goes round again.
    let before_store = || {
        waits.set(waits.get() + 1);
        if cold_sessions(&fx.read) >= 1 && replaced.borrow().is_none() {
            let copy = pass_owned_backups(&fx).pop().unwrap();
            std::fs::write(&copy, b"not a database").unwrap();
            *replaced.borrow_mut() = Some(copy);
        }
    };
    let seams = MoveSeams {
        wall: &SystemTime::now,
        skew: &no_skew,
        before_store: &before_store,
        in_writer: &quiet,
        after_qualify: &quiet,
    };

    let report = move_pass_seamed(&fx, one_session_batches(), &always, &never, &seams).unwrap();
    assert_eq!(report.moved.sessions_moved, 2, "{report:?}");
    assert!(report.move_refused.is_none(), "{report:?}");
    assert_eq!(
        waits.get(),
        3,
        "batch 1 once, batch 2 twice: the boundary sent it round again"
    );
    let replaced = replaced.borrow().clone().unwrap();
    let owned = pass_owned_backups(&fx);
    assert_eq!(owned.len(), 1, "{owned:?}");
    assert_ne!(owned[0], replaced);
    assert!(!replaced.exists());
    assert_eq!(
        report.moved.backup.as_deref(),
        Some(owned[0].to_str().unwrap())
    );
    restore_all(&fx, &ids);
    assert_eq!(snapshot(&fx), before);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_writer_wait_that_uses_up_the_backups_reserve_is_caught_at_the_boundary() {
    let fx = Fx::new();
    let ids = [fx.cold_session(SessionStatus::Archived, 60, 3)];
    let before = snapshot(&fx);
    // A pre-migration backup with ten minutes of its day left: it qualifies, with
    // the five-minute reserve, when the move is admitted.
    let near_the_limit = backup_copy(&fx, "rsi-pre-v174-near-the-limit.db");
    set_age(&near_the_limit, Duration::from_secs(23 * 3600 + 50 * 60));
    let late = Cell::new(Duration::ZERO);
    let wall = || SystemTime::now() + late.get();
    let writer_locks = Cell::new(0_u32);
    // The first time the transaction holds SQLite's write lock, six minutes have
    // gone by (it waited for the lock): the reserve is gone.
    let in_writer = || {
        writer_locks.set(writer_locks.get() + 1);
        if writer_locks.get() == 1 {
            late.set(Duration::from_secs(6 * 60));
        }
    };
    let seams = MoveSeams {
        wall: &wall,
        skew: &no_skew,
        before_store: &quiet,
        in_writer: &in_writer,
        after_qualify: &quiet,
    };

    let report = move_pass_seamed(&fx, limits(), &always, &never, &seams).unwrap();
    assert_eq!(report.moved.sessions_moved, 1, "{report:?}");
    assert_eq!(
        writer_locks.get(),
        2,
        "refused at the boundary, then admitted"
    );
    assert!(
        report.moved.backup_produced,
        "a new copy was taken outside the store: {report:?}"
    );
    assert_ne!(
        report.moved.backup.as_deref(),
        Some(near_the_limit.to_str().unwrap())
    );
    assert!(near_the_limit.exists(), "a pre-migration backup stays");
    restore_all(&fx, &ids);
    assert_eq!(snapshot(&fx), before);
}

// --- #204 (boundary): load, cancel and budget are current when the delete starts ---

#[derive(Debug, Clone, Copy)]
enum Held {
    LoadClosed,
    Cancelled,
    BudgetSpent,
}

#[derive(Debug, Clone, Copy)]
enum Route {
    /// A full batch inside the walk.
    FullBatch,
    /// The batch left at the end of the walk.
    TrailingBatch,
    /// The one-session retry of a batch that failed.
    IsolatedRetry,
}

#[derive(Debug, Clone, Copy)]
enum Point {
    /// While the delete waits for the store and `SQLite`'s write lock.
    BeforeStore,
    /// At the transaction boundary, while the backup is being read again: after
    /// the backup answered, before the final load, cancel and budget answers.
    AfterQualification,
}

const POINTS: [Point; 2] = [Point::BeforeStore, Point::AfterQualification];

/// Holds the pass at `point` of its `route`'s delete (after its own admission),
/// changes `held` meanwhile, and checks that nothing is deleted then.
fn hold_the_final_wait(held: Held, route: Route, point: Point) {
    let label = format!("{held:?} on {route:?} at {point:?}");
    let fx = Fx::new();
    let ids = cold_candidates(&fx, 3);
    fx.stage(14).unwrap();
    let before = snapshot(&fx);
    let (pass_limits, fire_on) = match route {
        Route::FullBatch => (one_session_batches(), 2),
        Route::TrailingBatch => (limits(), 1),
        Route::IsolatedRetry => (limits(), 2),
    };
    let open = Cell::new(true);
    let asked_to_stop = Cell::new(false);
    let spent = Cell::new(Duration::ZERO);
    let closed_looks = Cell::new(0_u32);
    // A gate that closed stays closed until the pass is told to stop.
    let gate = || {
        let is_open = open.get();
        if !is_open {
            closed_looks.set(closed_looks.get() + 1);
        }
        is_open
    };
    let cancel = || asked_to_stop.get() || closed_looks.get() >= 2;
    let skew = || spent.get();
    // The pass is held at the chosen point of the `fire_on`-th delete attempt; what
    // it admitted a moment ago changes meanwhile.
    let waits = Cell::new(0_u32);
    let fire = || {
        waits.set(waits.get() + 1);
        if waits.get() != fire_on {
            return;
        }
        match held {
            Held::LoadClosed => open.set(false),
            Held::Cancelled => asked_to_stop.set(true),
            Held::BudgetSpent => spent.set(Duration::from_secs(10_000)),
        }
    };
    let before_store = || {
        if matches!(point, Point::BeforeStore) {
            fire();
        }
    };
    let after_qualify = || {
        if matches!(point, Point::AfterQualification) {
            fire();
        }
    };
    let seams = MoveSeams {
        wall: &SystemTime::now,
        skew: &skew,
        before_store: &before_store,
        in_writer: &quiet,
        after_qualify: &after_qualify,
    };
    if matches!(route, Route::IsolatedRetry) {
        // The last session always fails, so the whole batch rolls back and its
        // sessions are retried one by one.
        failpoint::fail_session(&ids[2].to_string());
    }
    let report = move_pass_seamed(&fx, pass_limits, &gate, &cancel, &seams);
    failpoint::clear_session_failure();
    let report = report.unwrap();

    let (moved, deferred) = match route {
        Route::FullBatch => (1, 1),
        Route::TrailingBatch | Route::IsolatedRetry => (0, 3),
    };
    assert_eq!(report.moved.sessions_moved, moved, "{label}: {report:?}");
    assert_eq!(
        report.moved.sessions_deferred, deferred,
        "{label}: {report:?}"
    );
    assert_eq!(report.sessions_failed, 0, "{label}: {report:?}");
    assert!(report.budget_exhausted, "{label}: {report:?}");
    match held {
        Held::LoadClosed | Held::Cancelled => assert!(report.cancelled, "{label}"),
        Held::BudgetSpent => assert!(!report.cancelled, "{label}"),
    }
    // Whatever was held opened no delete: its rows are all live and staged.
    let (done, kept) = split_by_cold(&fx, &ids);
    assert_eq!(done.len(), usize::try_from(moved).unwrap(), "{label}");
    for id in &kept {
        assert_eq!(state(&fx, *id), "staged", "{label}");
        assert_eq!(
            rows_of(&snapshot(&fx), *id),
            rows_of(&before, *id),
            "{label}"
        );
    }

    // The next pass reads the rest back and moves it; everything restores exactly.
    let report = move_pass(&fx).unwrap();
    assert_eq!(
        report.moved.sessions_moved,
        3 - moved,
        "{label}: {report:?}"
    );
    restore_all(&fx, &ids);
    assert_eq!(snapshot(&fx), before, "{label}");
}

const ROUTES: [Route; 3] = [Route::FullBatch, Route::TrailingBatch, Route::IsolatedRetry];

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_load_gate_that_closes_during_the_final_waits_stops_the_delete_on_every_route() {
    for route in ROUTES {
        for point in POINTS {
            hold_the_final_wait(Held::LoadClosed, route, point);
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_cancel_that_arrives_during_the_final_waits_stops_the_delete_on_every_route() {
    for route in ROUTES {
        for point in POINTS {
            hold_the_final_wait(Held::Cancelled, route, point);
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_wall_budget_that_runs_out_during_the_final_waits_stops_the_delete_on_every_route() {
    for route in ROUTES {
        for point in POINTS {
            hold_the_final_wait(Held::BudgetSpent, route, point);
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_backup_another_connection_has_locked_is_not_waited_for_with_the_store_held() {
    let fx = Fx::new();
    let ids = [
        fx.cold_session(SessionStatus::Archived, 60, 3),
        fx.cold_session(SessionStatus::Archived, 61, 2),
    ];
    let before = snapshot(&fx);
    let contender = RefCell::new(None::<Connection>);
    let boundary_began = Cell::new(None::<Instant>);
    let released_after = Cell::new(None::<Duration>);
    let attempts = Cell::new(0_u32);
    // While the second batch waits for the store, another connection takes an
    // exclusive lock on the pass's backup (a rollback-journal file). Reading it
    // would wait for that lock for SQLite's default 5 s; the boundary must not.
    let before_store = || {
        attempts.set(attempts.get() + 1);
        if cold_sessions(&fx.read) < 1 || contender.borrow().is_some() {
            return;
        }
        if released_after.get().is_some() {
            return;
        }
        let copy = pass_owned_backups(&fx).pop().unwrap();
        let other = Connection::open(&copy).unwrap();
        other
            .execute_batch("PRAGMA journal_mode = DELETE; BEGIN EXCLUSIVE;")
            .unwrap();
        *contender.borrow_mut() = Some(other);
    };
    let in_writer = || {
        if contender.borrow().is_some() && boundary_began.get().is_none() {
            boundary_began.set(Some(Instant::now()));
        }
    };
    // The first look at the gate after the boundary refused is the pass's wait
    // outside both locks: the contending lock is released there, so any time the
    // boundary spent waiting for it would show in `released_after`.
    let gate = || {
        let held_lock = contender.borrow_mut().take();
        if held_lock.is_some() {
            released_after.set(boundary_began.get().map(|began| began.elapsed()));
        }
        drop(held_lock);
        true
    };
    let seams = MoveSeams {
        wall: &SystemTime::now,
        skew: &no_skew,
        before_store: &before_store,
        in_writer: &in_writer,
        after_qualify: &quiet,
    };

    let report = move_pass_seamed(&fx, one_session_batches(), &gate, &never, &seams).unwrap();
    let released_after = released_after.get().expect("the lock was released outside");
    assert!(
        released_after < Duration::from_secs(2),
        "the boundary waited {released_after:?} for a contended backup (default busy timeout is 5 s)"
    );
    assert_eq!(
        attempts.get(),
        3,
        "batch 1 once, batch 2 twice: the contended backup sent it round, nothing more"
    );
    assert_eq!(report.moved.sessions_moved, 2, "{report:?}");
    assert!(report.move_refused.is_none(), "contention is not staleness");
    assert_eq!(
        pass_owned_backups(&fx).len(),
        1,
        "no second copy was needed"
    );
    restore_all(&fx, &ids);
    assert_eq!(snapshot(&fx), before);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn the_admission_check_runs_with_sqlites_write_lock_held_and_a_refusal_deletes_nothing() {
    let fx = Fx::new();
    let id = fx.cold_session(SessionStatus::Archived, 60, 2);
    fx.stage(14).unwrap();
    let before = snapshot(&fx);
    let row = fx.row(id).unwrap();
    let guard = try_acquire_spawn_guard(id).expect("no other holder of the guard");
    let now = chrono::Utc::now();
    let context = MoveContext { now, cutoff: now };
    let writer_busy_at_the_check = Cell::new(false);
    let refuse = || -> ColdResult<()> {
        // A second connection cannot take the write lock: this transaction has it.
        let other = Connection::open(&fx.db).unwrap();
        other.busy_timeout(Duration::ZERO).unwrap();
        writer_busy_at_the_check.set(other.execute_batch("BEGIN IMMEDIATE").is_err());
        Err(ColdError::new(NOT_ADMITTED_CODE, "refused by the test"))
    };

    let error = fx
        .store
        .cold_move_batch_admitted(&[(VerifiedChunks::of_row(&row), &guard)], context, &refuse)
        .unwrap_err();
    assert_eq!(error.code, NOT_ADMITTED_CODE, "{error}");
    assert!(
        writer_busy_at_the_check.get(),
        "the check ran while the transaction held the write lock"
    );
    assert_eq!(snapshot(&fx), before, "the refusal rolled everything back");
    assert_eq!(state(&fx, id), "staged");

    // The same proof is admitted when the check says so.
    let moved = fx
        .store
        .cold_move_batch_admitted(&[(VerifiedChunks::of_row(&row), &guard)], context, &|| {
            Ok(())
        })
        .unwrap();
    assert_eq!(moved.len(), 1);
    assert_eq!(state(&fx, id), "cold");
}

// --- #203: a resumed session proves its column metadata before it moves -----

/// Rewrites the `columns_json` of the events chunk in the archive files through
/// `change`. The archive is append-only (its triggers refuse an update), so this
/// corruption seam drops them first.
fn corrupt_event_columns(fx: &Fx, change: fn(&mut [(String, String)])) {
    for file in fx.archive_paths() {
        let conn = Connection::open(&file).unwrap();
        let triggers: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'trigger'")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        for trigger in triggers {
            conn.execute_batch(&format!("DROP TRIGGER {trigger};"))
                .unwrap();
        }
        let (chunk_id, raw): (i64, String) = conn
            .query_row(
                "SELECT chunk_id, columns_json FROM chunks
                 WHERE table_name = 'conversation_events'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let mut columns: Vec<(String, String)> = serde_json::from_str(&raw).unwrap();
        change(&mut columns);
        conn.execute(
            "UPDATE chunks SET columns_json = ?1 WHERE chunk_id = ?2",
            rusqlite::params![serde_json::to_string(&columns).unwrap(), chunk_id],
        )
        .unwrap();
    }
}

fn rename_tool_name(columns: &mut [(String, String)]) {
    let at = columns
        .iter()
        .position(|(name, _)| name == "tool_name")
        .unwrap();
    columns[at].0 = "tool_nam3".into();
}

fn swap_tool_name_and_tool_use_id(columns: &mut [(String, String)]) {
    let name = columns
        .iter()
        .position(|(name, _)| name == "tool_name")
        .unwrap();
    let use_id = columns
        .iter()
        .position(|(name, _)| name == "tool_use_id")
        .unwrap();
    let (left, right) = (columns[name].0.clone(), columns[use_id].0.clone());
    columns[name].0 = right;
    columns[use_id].0 = left;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_resumed_session_whose_archived_column_metadata_changed_is_never_moved_and_restaging_repairs_it()
 {
    let variants: [(&str, fn(&mut [(String, String)])); 2] = [
        ("an unknown name of the same length", rename_tool_name),
        ("two TEXT columns swapped", swap_tool_name_and_tool_use_id),
    ];
    for (label, change) in variants {
        let fx = Fx::new();
        let id = fx.cold_session(SessionStatus::Archived, 60, 3);
        let originals = snapshot(&fx);
        let transcript = serde_json::to_value(fx.store.load_events(id).unwrap()).unwrap();
        fx.stage(14).unwrap();
        corrupt_event_columns(&fx, change);

        // The production resume path: the earlier pass's chunk reads back with
        // the right digest, but its column metadata is not the live tables'.
        let report = move_pass(&fx).unwrap();
        assert_eq!(report.moved.sessions_moved, 0, "{label}: {report:?}");
        assert_eq!(report.sessions_failed, 1, "{label}: {report:?}");
        assert_eq!(report.verify_errors, 1, "{label}: {report:?}");
        assert_eq!(
            report.errors.first().map(|error| error.code.as_str()),
            Some("cold_archive_verify_columns"),
            "{label}: {report:?}"
        );
        assert_eq!(snapshot(&fx), originals, "{label}: every live row stays");
        let row = fx.row(id).unwrap();
        assert_eq!(row.state, "staged", "{label}");
        assert_eq!(
            row.last_error_code.as_deref(),
            Some("cold_archive_verify_columns"),
            "{label}"
        );
        assert!(row.verified_at.is_none(), "{label}");

        // Re-staging writes the chunk again under the next generation; the same
        // pass reads it back and moves it.
        let report = move_pass(&fx).unwrap();
        assert_eq!(report.moved.sessions_moved, 1, "{label}: {report:?}");
        assert!(fx.row(id).unwrap().generation >= 2, "{label}");
        assert_eq!(state(&fx, id), "cold", "{label}");
        let after = serde_json::to_value(fx.store.load_events(id).unwrap()).unwrap();
        assert_eq!(after, transcript, "{label}: the cold read is whole");
        restore(&fx, id);
        assert_eq!(
            snapshot(&fx),
            originals,
            "{label}: restore reproduces every column"
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn the_verify_pass_fails_a_staged_session_whose_column_metadata_changed() {
    let fx = Fx::new();
    fx.cold_session(SessionStatus::Archived, 60, 2);
    fx.stage(14).unwrap();
    assert_eq!(fx.verify().unwrap().sessions_failed, 0);
    corrupt_event_columns(&fx, swap_tool_name_and_tool_use_id);

    let report = fx.verify().unwrap();
    assert_eq!(report.sessions_failed, 1, "{report:?}");
    assert_eq!(
        report.errors.first().map(|error| error.code.as_str()),
        Some("cold_archive_verify_columns"),
        "{report:?}"
    );
}

// --- #204: resumed moves count in the batch, and every batch is admitted ----

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn resumed_moves_count_toward_the_batch_so_a_closed_gate_holds_the_rest() {
    let fx = Fx::new();
    let ids = cold_candidates(&fx, 3);
    fx.stage(14).unwrap();
    let before = snapshot(&fx);
    // The gate closes once the first batch has committed; the host stays loaded
    // until the pass is told to stop after two looks.
    let closed_looks = Cell::new(0_u32);
    let gate = || {
        let open = cold_sessions(&fx.read) < 1;
        if !open {
            closed_looks.set(closed_looks.get() + 1);
        }
        open
    };
    let cancel = || closed_looks.get() >= 2;

    let report =
        move_pass_hooked(&fx, one_session_batches(), &gate, &cancel, &SystemTime::now).unwrap();
    assert_eq!(report.moved.sessions_reverified, 1, "{report:?}");
    assert_eq!(report.moved.sessions_moved, 1, "{report:?}");
    assert_eq!(report.batches, 1, "{report:?}");
    assert!(report.budget_exhausted && report.cancelled, "{report:?}");
    assert_eq!(state(&fx, ids[0]), "cold");
    for id in &ids[1..] {
        assert_eq!(state(&fx, *id), "staged");
        assert_eq!(rows_of(&snapshot(&fx), *id), rows_of(&before, *id));
    }

    // Admission reopens: the next pass reads the rest back and moves them.
    let report = move_pass(&fx).unwrap();
    assert_eq!(report.moved.sessions_reverified, 2, "{report:?}");
    assert_eq!(report.moved.sessions_moved, 2, "{report:?}");
    restore_all(&fx, &ids);
    assert_eq!(snapshot(&fx), before);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn freshly_staged_moves_obey_the_same_gate_one_batch_at_a_time() {
    let fx = Fx::new();
    let ids = cold_candidates(&fx, 3);
    let before = snapshot(&fx);
    let closed_looks = Cell::new(0_u32);
    let gate = || {
        let open = cold_sessions(&fx.read) < 1;
        if !open {
            closed_looks.set(closed_looks.get() + 1);
        }
        open
    };
    let cancel = || closed_looks.get() >= 2;

    let report =
        move_pass_hooked(&fx, one_session_batches(), &gate, &cancel, &SystemTime::now).unwrap();
    assert_eq!(report.sessions_staged, 1, "{report:?}");
    assert_eq!(report.moved.sessions_moved, 1, "{report:?}");
    assert_eq!(report.batches, 1, "{report:?}");
    assert_eq!(state(&fx, ids[0]), "cold");
    for id in &ids[1..] {
        assert_eq!(
            live_events(&fx, *id),
            i64::try_from(rows_of(&before, *id).0.len()).unwrap()
        );
        assert_eq!(rows_of(&snapshot(&fx), *id), rows_of(&before, *id));
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_pass_that_only_reverifies_sleeps_between_its_batches_and_counts_them() {
    let fx = Fx::new();
    let ids = cold_candidates(&fx, 3);
    fx.stage(14).unwrap();
    let limits = PassLimits {
        batch_max_sessions: 1,
        batch_sleep: Duration::from_millis(60),
        ..limits()
    };

    let started = Instant::now();
    let report = move_pass_with(&fx, limits).unwrap();
    let elapsed = started.elapsed();
    assert_eq!(report.moved.sessions_reverified, 3, "{report:?}");
    assert_eq!(report.moved.sessions_moved, 3, "{report:?}");
    assert_eq!(report.batches, 3, "one batch per re-verified session");
    assert!(
        elapsed >= Duration::from_millis(180),
        "three batch sleeps of 60 ms, took {elapsed:?}"
    );
    for id in ids {
        assert_eq!(state(&fx, id), "cold");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn the_trailing_batch_is_admitted_like_any_other_and_waits_staged_when_the_gate_is_closed() {
    let fx = Fx::new();
    let ids = cold_candidates(&fx, 3);
    fx.stage(14).unwrap();
    let before = snapshot(&fx);
    // Open for the walk's first look, closed from then on: the three proofs sit
    // in the trailing batch when the gate closes. The host stays loaded until the
    // pass is told to stop after two more looks (no wall clock involved).
    let looks = Cell::new(0_u32);
    let gate = || {
        looks.set(looks.get() + 1);
        looks.get() == 1
    };
    let cancel = || looks.get() >= 3;

    let report = move_pass_hooked(&fx, limits(), &gate, &cancel, &SystemTime::now).unwrap();
    assert_eq!(report.moved.sessions_reverified, 3, "{report:?}");
    assert_eq!(report.moved.sessions_moved, 0, "{report:?}");
    assert_eq!(report.moved.sessions_deferred, 3, "{report:?}");
    assert!(report.budget_exhausted && report.cancelled, "{report:?}");
    assert_eq!(snapshot(&fx), before, "no row left the live tables");
    for id in &ids {
        assert_eq!(state(&fx, *id), "staged");
    }

    let report = move_pass(&fx).unwrap();
    assert_eq!(report.moved.sessions_moved, 3, "{report:?}");
    restore_all(&fx, &ids);
    assert_eq!(snapshot(&fx), before);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn an_isolated_retry_is_admitted_on_its_own_and_waits_staged_when_the_gate_closes() {
    let fx = Fx::new();
    let ids = cold_candidates(&fx, 3);
    fx.stage(14).unwrap();
    let before = snapshot(&fx);
    // The last session of the batch always fails, so the batch rolls back and its
    // sessions are retried one by one; the gate closes once the first retry moved.
    let culprit = ids[2];
    let closed_looks = Cell::new(0_u32);
    let gate = || {
        let open = cold_sessions(&fx.read) < 1;
        if !open {
            closed_looks.set(closed_looks.get() + 1);
        }
        open
    };
    let cancel = || closed_looks.get() >= 2;

    failpoint::fail_session(&culprit.to_string());
    let report = move_pass_hooked(&fx, limits(), &gate, &cancel, &SystemTime::now);
    failpoint::clear_session_failure();
    let report = report.unwrap();
    assert_eq!(report.moved.sessions_moved, 1, "{report:?}");
    assert_eq!(report.moved.sessions_deferred, 2, "{report:?}");
    assert_eq!(report.sessions_failed, 0, "{report:?}");
    assert!(report.cancelled, "{report:?}");
    assert_eq!(state(&fx, ids[0]), "cold");
    for id in &ids[1..] {
        assert_eq!(state(&fx, *id), "staged");
        assert_eq!(rows_of(&snapshot(&fx), *id), rows_of(&before, *id));
    }

    let report = move_pass(&fx).unwrap();
    assert_eq!(report.moved.sessions_moved, 2, "{report:?}");
    restore_all(&fx, &ids);
    assert_eq!(snapshot(&fx), before);
}

// --- #205 (test seam): which pass connections to the live database are open -

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_pass_connection_is_counted_while_it_is_open_and_not_after_it_closes() {
    use super::live_connections::{open_on, wait_for};
    let fx = Fx::new();
    // The fixture's own read connection is the one open connection.
    assert_eq!(open_on(&fx.db), 1);
    let second = open_read_connection(&fx.db).unwrap();
    assert_eq!(open_on(&fx.db), 2);
    assert_eq!(wait_for(&fx.db, 2, Duration::from_secs(5)), 2);
    drop(second);
    assert_eq!(open_on(&fx.db), 1, "closing the connection took it off");
    // A wait for a count that never comes ends at its bound with what is open.
    assert_eq!(wait_for(&fx.db, 7, Duration::from_millis(50)), 1);
}

// --- #205 (store half): a cancelled pass stops at a safe boundary ------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_cancelled_pass_stops_before_it_touches_the_next_session_and_loses_nothing() {
    let fx = Fx::new();
    let ids = cold_candidates(&fx, 3);
    let before = snapshot(&fx);

    let report = move_pass_hooked(&fx, limits(), &always, &always, &SystemTime::now).unwrap();
    assert!(report.cancelled && report.budget_exhausted, "{report:?}");
    assert_eq!(report.sessions_staged, 0, "{report:?}");
    assert_eq!(report.moved.sessions_moved, 0, "{report:?}");
    assert_eq!(snapshot(&fx), before);
    // A cancelled pass is partial, not failed: no error code, no notice.
    let summary = report.run_summary(None);
    assert_eq!(summary.outcome, "partial");
    assert_eq!(summary.error_code, None);

    // The next pass does all of it.
    let report = move_pass(&fx).unwrap();
    assert_eq!(report.moved.sessions_moved, 3, "{report:?}");
    restore_all(&fx, &ids);
    assert_eq!(snapshot(&fx), before);
}

// ---------------------------------------------------------------------------
// Reclaim
// ---------------------------------------------------------------------------

/// A new database file is created in incremental auto-vacuum mode, and the
/// incremental reclaim returns freed pages to the file system.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_new_database_is_incremental_and_the_reclaim_returns_freed_pages() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rsi.db");
    let store = Store::open(&path).unwrap();
    let auto_vacuum: i64 = store
        .conn
        .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
        .unwrap();
    assert_eq!(auto_vacuum, 2, "a new database starts incremental");
    assert_eq!(
        store.schema_version().unwrap(),
        crate::store::LATEST_SCHEMA_VERSION
    );

    store
        .conn
        .execute_batch(
            "CREATE TABLE scratch(x BLOB);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 600)
             INSERT INTO scratch SELECT randomblob(16384) FROM n;
             DROP TABLE scratch;",
        )
        .unwrap();
    let free_before: i64 = store
        .conn
        .query_row("PRAGMA freelist_count", [], |row| row.get(0))
        .unwrap();
    assert!(free_before > 1000, "{free_before}");
    let report = reclaim_incremental(&DirectStoreAccess(&store), Duration::from_secs(30)).unwrap();
    assert!(report.incremental);
    assert!(report.pages_reclaimed >= free_before / 2, "{report:?}");
    let pages: i64 = store
        .conn
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .unwrap();
    assert!(report.freelist_after * 100 <= pages || report.freelist_after < free_before);
    drop(store);

    // Reopening keeps the mode (an existing file changes only through compaction).
    let reopened = Store::open(&path).unwrap();
    let auto_vacuum: i64 = reopened
        .conn
        .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
        .unwrap();
    assert_eq!(auto_vacuum, 2);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn the_reclaim_does_nothing_on_a_file_that_is_not_incremental() {
    let fx = Fx::new();
    let report =
        reclaim_incremental(&DirectStoreAccess(&fx.store), Duration::from_secs(5)).unwrap();
    assert!(!report.incremental);
    assert_eq!(report.steps, 0);
}

// ---------------------------------------------------------------------------
// Compaction (the helper's core, the run rows and their reconcile)
// ---------------------------------------------------------------------------

fn catalog(conn: &Connection) -> Vec<(String, Option<String>)> {
    let mut stmt = conn
        .prepare("SELECT name, sql FROM sqlite_master ORDER BY type, name")
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn row_counts(conn: &Connection) -> Vec<(String, i64)> {
    let tables: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    tables
        .into_iter()
        .map(|name| {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM \"{name}\""), [], |row| {
                    row.get(0)
                })
                .unwrap();
            (name, count)
        })
        .collect()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn the_compaction_switches_a_wal_database_to_incremental_and_shrinks_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rsi.db");
    {
        let store = Store::open_test_fixture(&path).unwrap();
        let fx_session = crate::test_support::make_test_session();
        store.insert_session(&fx_session).unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TABLE scratch(x BLOB);
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 800)
                 INSERT INTO scratch SELECT randomblob(16384) FROM n;
                 DROP TABLE scratch;
                 PRAGMA wal_checkpoint(TRUNCATE);",
            )
            .unwrap();
        let mode: String = store
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }
    let (catalog_before, counts_before, auto_before) = {
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let auto: i64 = conn
            .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
            .unwrap();
        (catalog(&conn), row_counts(&conn), auto)
    };
    assert_eq!(auto_before, 0);
    let size_before = std::fs::metadata(&path).unwrap().len();

    let run_id = Uuid::new_v4();
    let result = run_compaction(&path, run_id);
    assert_eq!(result.outcome, "completed", "{result:?}");
    assert_eq!(result.auto_vacuum, 2);
    assert!(result.bytes_after < result.bytes_before, "{result:?}");

    let conn = Connection::open(&path).unwrap();
    Store::register_sql_functions(&conn).unwrap();
    let auto: i64 = conn
        .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
        .unwrap();
    assert_eq!(auto, 2);
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    assert_eq!(
        catalog(&conn),
        catalog_before,
        "sqlite_master SQL unchanged"
    );
    assert_eq!(row_counts(&conn), counts_before, "row counts unchanged");
    assert!(std::fs::metadata(&path).unwrap().len() < size_before / 2);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn the_compaction_refuses_a_schema_newer_than_its_build_and_leaves_it_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rsi.db");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE t(x); PRAGMA user_version = {};",
            crate::store::LATEST_SCHEMA_VERSION + 1
        ))
        .unwrap();
    }
    let result = run_compaction(&path, Uuid::new_v4());
    assert_eq!(result.outcome, "failed");
    assert_eq!(result.error_code.as_deref(), Some("compact_schema_newer"));
    let conn = Connection::open(&path).unwrap();
    let auto: i64 = conn
        .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
        .unwrap();
    assert_eq!(auto, 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_requested_compaction_is_claimed_at_the_quiet_point_and_settled_from_its_result() {
    let fx = Fx::new();
    // Not due on a small file, and nothing asked for: no claim.
    assert_eq!(fx.store.cold_claim_compaction(true).unwrap(), None);
    let requested = fx.store.cold_request_compaction().unwrap();
    assert_eq!(
        fx.store.cold_request_compaction().unwrap(),
        requested,
        "idempotent"
    );
    assert!(fx.store.cold_last_compaction().unwrap().pending);

    let claimed = fx.store.cold_claim_compaction(false).unwrap();
    assert_eq!(claimed, Some(requested));
    assert!(!fx.store.cold_last_compaction().unwrap().pending);
    // A pass's stale-run sweep never settles a compaction row.
    fx.store.cold_fail_stale_runs(Uuid::new_v4()).unwrap();
    // The process that scheduled it does not reconcile its own run.
    assert!(
        fx.store
            .cold_reconcile_compactions(&fx.db, Some(requested))
            .unwrap()
            .is_empty()
    );

    let (_tmp, result_path) = compact_paths(&fx.db, requested);
    write_compact_result(
        &result_path,
        &CompactResult {
            run_id: requested.to_string(),
            outcome: "completed".into(),
            error_code: None,
            error_detail: None,
            compact_ms: 1234,
            bytes_before: 9_000,
            bytes_after: 3_000,
            auto_vacuum: 2,
        },
    )
    .unwrap();
    let settled = fx.store.cold_reconcile_compactions(&fx.db, None).unwrap();
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].outcome, "completed");
    assert!(!result_path.exists());
    let last = fx.store.cold_last_compaction().unwrap();
    assert_eq!(last.last_outcome.as_deref(), Some("completed"));
    assert_eq!(last.last_ms, Some(1234));
    assert_eq!(last.bytes_after, Some(3_000));
    let status = fx
        .store
        .cold_archive_status(&fx.settings(14), false)
        .unwrap();
    assert!(status.compaction.available);
    assert_eq!(status.compaction.last_outcome.as_deref(), Some("completed"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_compaction_that_left_no_result_is_settled_interrupted() {
    let fx = Fx::new();
    let requested = fx.store.cold_request_compaction().unwrap();
    fx.store.cold_claim_compaction(false).unwrap();
    let settled = fx.store.cold_reconcile_compactions(&fx.db, None).unwrap();
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].run_id, requested.to_string());
    assert_eq!(settled[0].outcome, "failed");
    assert_eq!(
        settled[0].error_code.as_deref(),
        Some(COMPACT_INTERRUPTED_CODE)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn an_abandoned_restart_puts_the_operators_request_back() {
    let fx = Fx::new();
    let requested = fx.store.cold_request_compaction().unwrap();
    fx.store.cold_claim_compaction(false).unwrap();
    fx.store.cold_abandon_compaction(requested).unwrap();
    assert_eq!(fx.store.cold_pending_compaction().unwrap(), Some(requested));
}

// ---------------------------------------------------------------------------
// Notices
// ---------------------------------------------------------------------------

fn open_notices(fx: &Fx) -> Vec<String> {
    fx.store
        .operator_todo_open_system_keys(NOTICE_KEY_PREFIX)
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_notice_is_raised_once_per_code_and_closed_by_the_next_success() {
    let fx = Fx::new();
    fx.store
        .cold_archive_raise_notice(BACKUP_STALE_CODE, "no backup");
    fx.store
        .cold_archive_raise_notice(BACKUP_STALE_CODE, "still no backup");
    fx.store
        .cold_archive_raise_notice(COMPACT_INTERRUPTED_CODE, "helper killed");
    fx.store
        .cold_archive_raise_notice("cold_archive_unavailable", "file gone");
    let open = open_notices(&fx);
    assert_eq!(open.len(), 3, "{open:?}");
    assert!(
        open.iter()
            .any(|key| key.starts_with(&format!("{NOTICE_KEY_PREFIX}{BACKUP_STALE_CODE}:")))
    );
    let friction = fx.count(
        "SELECT COUNT(*) FROM friction_events WHERE signature = 'cold_archive:cold_archive_backup_stale'",
        None,
    );
    assert_eq!(friction, 2, "every occurrence is a friction event");

    // A successful pass closes the pass notice only.
    assert_eq!(fx.store.cold_archive_resolve_notices(false).unwrap(), 1);
    let open = open_notices(&fx);
    assert_eq!(open.len(), 2, "{open:?}");
    // A completed compaction closes the compaction notice; the read notice stays.
    assert_eq!(fx.store.cold_archive_resolve_notices(true).unwrap(), 1);
    let open = open_notices(&fx);
    assert_eq!(open.len(), 1, "{open:?}");
    assert!(open[0].contains("cold_archive_unavailable"));

    // The failure recurs: a new item opens.
    std::thread::sleep(Duration::from_millis(1100));
    fx.store
        .cold_archive_raise_notice(BACKUP_STALE_CODE, "again");
    assert_eq!(open_notices(&fx).len(), 2);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_cold_read_of_a_missing_archive_queues_a_notice_the_loop_raises() {
    let fx = Fx::new();
    let id = fx.cold_session(SessionStatus::Archived, 60, 2);
    move_pass(&fx).unwrap();
    for path in fx.archive_paths() {
        std::fs::remove_file(path).unwrap();
    }
    assert!(fx.store.load_events(id).is_err());
    fx.store.cold_archive_flush_notices();
    let open = open_notices(&fx);
    assert!(
        open.iter()
            .any(|key| key.contains("cold_archive_unavailable")),
        "{open:?}"
    );
}

// ---------------------------------------------------------------------------
// Source checks
// ---------------------------------------------------------------------------

/// Plan §5 (review #2398): the daemon never changes `SQLite`'s process-global
/// temporary directory, neither with the pragma nor through its environment;
/// only the compaction helper's own `Command` environment names it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn no_production_code_moves_the_sqlite_temp_directory_in_process() {
    let crates = PathBuf::from(rsi_common::manifest_dir!())
        .parent()
        .unwrap()
        .to_path_buf();
    let pragma = ["temp_store", "_directory"].concat();
    let mut forbidden = vec![pragma.clone()];
    for call in ["set_var(", "remove_var("] {
        for name in ["SQLITE_TMPDIR", "TMPDIR", "TMP", "TEMP"] {
            forbidden.push(format!("{call}\"{name}\""));
        }
    }
    let mut scanned = 0;
    let mut offenders = Vec::new();
    for entry in walkdir::WalkDir::new(&crates)
        .into_iter()
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            name != "target" && name != ".git" && name != "tests"
        })
        .filter_map(Result::ok)
    {
        let path = entry.path();
        let relative = path
            .strip_prefix(&crates)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let name = path.file_name().unwrap().to_string_lossy();
        if !relative.ends_with(".rs")
            || !relative.contains("/src/")
            || name == "tests.rs"
            || name.ends_with("_tests.rs")
        {
            continue;
        }
        scanned += 1;
        let text: String = std::fs::read_to_string(path)
            .unwrap()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        for needle in &forbidden {
            if text.contains(needle.as_str()) {
                offenders.push(format!("{relative}: {needle}"));
            }
        }
    }
    assert!(
        scanned > 100,
        "the scan read {scanned} files under {}",
        crates.display()
    );
    assert!(offenders.is_empty(), "{offenders:#?}");
}
