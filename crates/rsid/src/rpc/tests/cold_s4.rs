//! Cold-storage archive, slice S4 (#2384): the move through `RunColdArchiveNow`,
//! its fresh-backup gate and notices, and the compaction request.

use super::*;
use rsi_common::types::SessionStatus;

const NOTICE_PREFIX: &str = "system:cold-archive:";

fn live_rows(store: &crate::store::Store, session: Uuid) -> i64 {
    store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM conversation_events WHERE session_id = ?1",
            [session.to_string()],
            |row| row.get(0),
        )
        .unwrap()
}

async fn backups_dir(fixture: &RecursiveDagRpcFixture) -> std::path::PathBuf {
    let store = fixture.manager.store().lock().await;
    std::path::Path::new(store.database_path().unwrap())
        .parent()
        .unwrap()
        .join("backups")
}

async fn run(fixture: &RecursiveDagRpcFixture, mode: &str) -> serde_json::Value {
    let started = call_rpc(
        &fixture.server,
        "RunColdArchiveNow",
        serde_json::json!({ "mode": mode }),
    )
    .await;
    assert!(started.error.is_none(), "{started:?}");
    let started = started.result.unwrap();
    assert_eq!(started["mode"], mode);
    wait_for_cold_archive_run(&fixture.server, started["run_id"].as_str().unwrap()).await
}

async fn open_notices(fixture: &RecursiveDagRpcFixture) -> Vec<String> {
    fixture
        .manager
        .store()
        .lock()
        .await
        .operator_todo_open_system_keys(NOTICE_PREFIX)
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn a_move_run_deletes_only_verified_rows_after_taking_a_backup_and_reads_stay_whole() {
    let fixture = recursive_dag_rpc_fixture();
    let session = seed_cold_candidate(&fixture, SessionStatus::Archived).await;
    let before = {
        let store = fixture.manager.store().lock().await;
        serde_json::to_value(store.load_events(session).unwrap()).unwrap()
    };

    let status = run(&fixture, "move").await;
    assert_eq!(status["last_run"]["outcome"], "completed", "{status}");
    assert_eq!(status["last_run"]["mode"], "move");
    assert_eq!(status["last_run"]["sessions_moved"], 1, "{status}");
    assert_eq!(status["last_run"]["events_moved"], 4, "{status}");
    assert_eq!(status["totals"]["cold_sessions"], 1);
    {
        let store = fixture.manager.store().lock().await;
        assert_eq!(live_rows(&store, session), 3, "the four tool rows left");
        let after = serde_json::to_value(store.load_events(session).unwrap()).unwrap();
        assert_eq!(after, before, "the transcript is served whole");
    }
    let backups: Vec<String> = std::fs::read_dir(backups_dir(&fixture).await)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        backups
            .iter()
            .any(|name| name.starts_with("rsi-cold-pass-")),
        "the pass took its own fresh backup first: {backups:?}"
    );

    // The restore through the operator RPC brings every row back.
    let restored = call_rpc(
        &fixture.server,
        "RestoreColdSession",
        serde_json::json!({ "session_id": session }),
    )
    .await;
    assert_eq!(restored.result.unwrap()["outcome"], "restored");
    let store = fixture.manager.store().lock().await;
    assert_eq!(live_rows(&store, session), 7);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn a_move_without_a_fresh_backup_deletes_nothing_and_raises_a_notice_until_a_pass_succeeds() {
    let fixture = recursive_dag_rpc_fixture();
    let session = seed_cold_candidate(&fixture, SessionStatus::Archived).await;
    let backups = backups_dir(&fixture).await;
    std::fs::write(&backups, b"not a directory").unwrap();

    let status = run(&fixture, "move").await;
    assert_eq!(status["last_run"]["outcome"], "failed", "{status}");
    assert_eq!(
        status["last_run"]["error_code"], "cold_archive_backup_stale",
        "{status}"
    );
    assert_eq!(status["totals"]["cold_sessions"], 0);
    assert!(
        status["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error
                .as_str()
                .unwrap()
                .contains("cold_archive_backup_stale")),
        "{status}"
    );
    {
        let store = fixture.manager.store().lock().await;
        assert_eq!(live_rows(&store, session), 7, "nothing was deleted");
        let friction: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM friction_events
                 WHERE signature = 'cold_archive:cold_archive_backup_stale'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(friction, 1);
    }
    let notices = open_notices(&fixture).await;
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].starts_with("system:cold-archive:cold_archive_backup_stale:"));

    // Fixed: the next pass moves and closes the notice.
    std::fs::remove_file(&backups).unwrap();
    let status = run(&fixture, "move").await;
    assert_eq!(status["last_run"]["outcome"], "completed", "{status}");
    assert_eq!(status["last_run"]["sessions_moved"], 1, "{status}");
    assert!(open_notices(&fixture).await.is_empty());
}

/// Slice S5: the daemon as shipped, with no cold-archive setting ever written,
/// archives an old session on its first scheduled tick. This is the actual
/// default path (`RuntimeConfig::from_config`, no persisted row, no
/// `update_field`): a fixture that enabled the setting itself would pass on a
/// default of off too.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn the_shipped_default_archives_an_old_session_on_the_first_scheduled_tick() {
    use crate::session::cold_archive::ColdArchiveTick;
    let fixture = recursive_dag_rpc_fixture();
    let (session, before) = {
        let store = fixture.manager.store().lock().await;
        let session = seed_old_session(&store, SessionStatus::Archived);
        let before = serde_json::to_value(store.load_events(session).unwrap()).unwrap();
        (session, before)
    };
    let status = call_rpc(
        &fixture.server,
        "GetColdArchiveStatus",
        serde_json::json!({}),
    )
    .await
    .result
    .unwrap();
    assert_eq!(status["settings"]["enabled"], true, "{status}");

    let tick = fixture.manager.cold_archive_tick(chrono::Utc::now()).await;
    let ColdArchiveTick::Started(run_id) = tick else {
        panic!("the default configuration starts a scheduled pass, got {tick:?}");
    };
    let status = wait_for_cold_archive_run(&fixture.server, &run_id.to_string()).await;
    assert_eq!(status["last_run"]["mode"], "move", "{status}");
    assert_eq!(status["last_run"]["trigger"], "scheduled", "{status}");
    assert_eq!(status["last_run"]["outcome"], "completed", "{status}");
    assert_eq!(status["last_run"]["sessions_moved"], 1, "{status}");
    let store = fixture.manager.store().lock().await;
    assert_eq!(live_rows(&store, session), 3, "the tool rows left");
    assert_eq!(
        serde_json::to_value(store.load_events(session).unwrap()).unwrap(),
        before,
        "the transcript is served whole"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn compact_is_scheduled_for_the_next_supervised_restart_never_run_live() {
    let fixture = recursive_dag_rpc_fixture();
    fixture
        .runtime_config
        .update_field("cold_archive_enabled", &serde_json::json!(true))
        .unwrap();
    let first = call_rpc(
        &fixture.server,
        "RunColdArchiveNow",
        serde_json::json!({ "mode": "compact" }),
    )
    .await
    .result
    .unwrap();
    assert_eq!(first["scheduled"], true);
    assert_eq!(first["mode"], "compact");
    let again = call_rpc(
        &fixture.server,
        "RunColdArchiveNow",
        serde_json::json!({ "mode": "compact" }),
    )
    .await
    .result
    .unwrap();
    assert_eq!(
        again["run_id"], first["run_id"],
        "one request waits at a time"
    );
    let status = call_rpc(
        &fixture.server,
        "GetColdArchiveStatus",
        serde_json::json!({}),
    )
    .await
    .result
    .unwrap();
    assert_eq!(status["compaction"]["pending"], true, "{status}");
    assert_eq!(status["running"], false, "nothing runs in the live daemon");
    let parsed: rsi_common::cold_storage::ColdRunStartedV1 = serde_json::from_value(first).unwrap();
    assert!(parsed.scheduled);
}

// ---------------------------------------------------------------------------
// #205: a supervised restart starts the compaction helper only on a quiet
// database
// ---------------------------------------------------------------------------

/// The host load the pass's gate reads (95 against a threshold of 8, so the gate
/// is closed), and the signals the pass sends from its own thread each time its
/// gate looks at it. That look is the handshake: the pass opened its read
/// connection before it got there and now waits at the gate, so a test needs no
/// sleep and no polling to know the pass is held.
struct GateProbe {
    /// The load the gate reads.
    load: std::sync::Arc<std::sync::Mutex<f64>>,
    /// Notified by the pass thread at its gate (a permit is kept if nobody waits yet).
    reached: std::sync::Arc<tokio::sync::Notify>,
    /// Pass connections open on the database at the pass's last look.
    open_at_gate: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

fn gate_probe(fixture: &RecursiveDagRpcFixture, load: f64, db: &std::path::Path) -> GateProbe {
    use std::sync::atomic::Ordering;
    fixture
        .runtime_config
        .update_field("host_load_admission_threshold", &serde_json::json!(8))
        .unwrap();
    let probe = GateProbe {
        load: std::sync::Arc::new(std::sync::Mutex::new(load)),
        reached: std::sync::Arc::new(tokio::sync::Notify::new()),
        open_at_gate: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX)),
    };
    let shared = std::sync::Arc::clone(&probe.load);
    let reached = std::sync::Arc::clone(&probe.reached);
    let open_at_gate = std::sync::Arc::clone(&probe.open_at_gate);
    let db = db.to_path_buf();
    fixture
        .manager
        .host_load()
        .set_source(std::sync::Arc::new(move || {
            if std::thread::current().name() == Some("rsid-cold-archive") {
                open_at_gate.store(
                    crate::store::cold_archive::live_connections::open_on(&db),
                    Ordering::SeqCst,
                );
                reached.notify_one();
            }
            crate::host_load::LoadReading::Load1(*shared.lock().unwrap())
        }));
    probe
}

/// The stage run (the default mode of `RunColdArchiveNow`), settled.
async fn stage_run(fixture: &RecursiveDagRpcFixture) {
    let started = call_rpc(&fixture.server, "RunColdArchiveNow", serde_json::json!({})).await;
    let run_id = started.result.expect("the stage run starts")["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let status = wait_for_cold_archive_run(&fixture.server, &run_id).await;
    assert_eq!(status["last_run"]["outcome"], "completed", "{status}");
}

/// The compaction an operator asked for, claimed the way the deploy loop's quiet
/// point claims it, as the plan the exiting daemon would run.
async fn claimed_compaction(
    fixture: &RecursiveDagRpcFixture,
) -> (std::path::PathBuf, crate::cold_compact::CompactOnExit) {
    let store = fixture.manager.store().lock().await;
    let db = std::path::PathBuf::from(store.database_path().unwrap());
    let requested = store.cold_request_compaction().unwrap();
    assert_eq!(store.cold_claim_compaction(false).unwrap(), Some(requested));
    let (tmp_dir, result_path) = crate::store::cold_archive::reclaim::compact_paths(&db, requested);
    let plan = crate::cold_compact::CompactOnExit {
        run_id: requested,
        db_path: db.clone(),
        tmp_dir,
        result_path,
        deadline: std::time::Duration::from_secs(30),
    };
    (db, plan)
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::too_many_lines)]
async fn a_restart_starts_the_helper_only_after_the_running_pass_closed_its_database() {
    use crate::cold_compact::{HelperOutcome, quiesce_then_run};
    use crate::store::cold_archive::live_connections;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    let fixture = recursive_dag_rpc_fixture();
    let session = seed_cold_candidate(&fixture, SessionStatus::Archived).await;
    stage_run(&fixture).await;
    let (db, plan) = claimed_compaction(&fixture).await;
    assert_eq!(
        live_connections::open_on(&db),
        0,
        "the settled stage pass closed its connection"
    );

    // A real move pass, held at its host-load gate. The gate's own look is the
    // readiness event: the bound is only a safety net against a pass that never
    // gets there.
    let probe = gate_probe(&fixture, 95.0, &db);
    let started = call_rpc(
        &fixture.server,
        "RunColdArchiveNow",
        serde_json::json!({ "mode": "move" }),
    )
    .await;
    let run_id = started.result.expect("the move run starts")["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    tokio::time::timeout(Duration::from_secs(60), probe.reached.notified())
        .await
        .expect("the pass reached its gate");
    let lifecycle = Arc::clone(fixture.manager.cold_archive_lifecycle());
    assert_eq!(
        lifecycle.active(),
        1,
        "the pass thread is alive at its gate"
    );
    assert_eq!(
        probe.open_at_gate.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "at its gate the pass holds exactly its read connection to rsi.db"
    );

    // The restart starts to drain (the deploy loop's quiet point does this through
    // the process-wide slot): no pass starts, the running one is asked to stop,
    // and the helper launch waits for it.
    lifecycle.close();
    assert!(lifecycle.cancel_requested());
    let seen = Arc::new(Mutex::new(None::<(usize, usize)>));
    let outcome = tokio::task::spawn_blocking({
        let lifecycle = Arc::clone(&lifecycle);
        let seen = Arc::clone(&seen);
        let db = db.clone();
        move || {
            quiesce_then_run(
                &plan,
                Some(lifecycle.as_ref()),
                Duration::from_secs(30),
                |_| {
                    // What the helper would find: the pass thread gone, its SQLite
                    // connection closed.
                    *seen.lock().unwrap() =
                        Some((lifecycle.active(), live_connections::open_on(&db)));
                    std::process::Command::new("sh")
                        .args(["-c", "exit 0"])
                        .spawn()
                },
                Duration::from_millis(20),
            )
        }
    })
    .await
    .unwrap();
    assert_eq!(outcome, HelperOutcome::Exited(Some(0)));
    let (active, open) = seen.lock().unwrap().expect("the helper launched");
    assert_eq!(active, 0, "the pass thread had returned its ticket");
    assert_eq!(open, 0, "the pass's connection to rsi.db was closed first");

    // The cancelled pass is partial, not failed; nothing it did is lost.
    let status = wait_for_cold_archive_run(&fixture.server, &run_id).await;
    assert_eq!(status["last_run"]["outcome"], "partial", "{status}");
    assert_eq!(status["totals"]["cold_sessions"], 0, "{status}");
    assert!(open_notices(&fixture).await.is_empty());
    let before = {
        let store = fixture.manager.store().lock().await;
        assert_eq!(live_rows(&store, session), 7, "every row is still live");
        serde_json::to_value(store.load_events(session).unwrap()).unwrap()
    };

    // While draining no pass starts; if the restart is abandoned, they may again.
    let refused = call_rpc(
        &fixture.server,
        "RunColdArchiveNow",
        serde_json::json!({ "mode": "move" }),
    )
    .await;
    assert!(
        refused
            .error
            .unwrap()
            .message
            .contains("cold_archive_shutting_down")
    );
    lifecycle.reopen();
    *probe.load.lock().unwrap() = 0.0;
    let status = run(&fixture, "move").await;
    assert_eq!(status["last_run"]["outcome"], "completed", "{status}");
    assert_eq!(status["last_run"]["sessions_moved"], 1, "{status}");
    let store = fixture.manager.store().lock().await;
    assert_eq!(live_rows(&store, session), 3, "live + archived = 7");
    assert_eq!(
        serde_json::to_value(store.load_events(session).unwrap()).unwrap(),
        before,
        "the transcript is whole"
    );
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::too_many_lines)]
async fn a_pass_that_will_not_settle_defers_the_compaction_and_the_next_daemon_puts_it_back() {
    use crate::cold_compact::{HelperOutcome, quiesce_then_run};
    use crate::store::cold_archive::{ColdMode, live_connections};
    use std::time::{Duration, Instant};

    let fixture = recursive_dag_rpc_fixture();
    let session = seed_cold_candidate(&fixture, SessionStatus::Archived).await;
    stage_run(&fixture).await;
    let (db, plan) = claimed_compaction(&fixture).await;
    let result_path = plan.result_path.clone();
    let lifecycle = std::sync::Arc::clone(fixture.manager.cold_archive_lifecycle());

    // A controlled store seam: the pass thread is started, then the store is held
    // so the thread (alive, read connection open) cannot reach its next safe
    // boundary, and cannot see the cancel flag.
    let run_id = fixture
        .manager
        .start_cold_archive_run("scheduled", ColdMode::Move)
        .await
        .unwrap();
    let held = fixture.manager.store().lock().await;
    // The readiness event is the pass opening its read connection (it does so
    // before its first store access, where it now blocks); the bound is only a
    // safety net.
    let opened = tokio::task::spawn_blocking({
        let db = db.clone();
        move || live_connections::wait_for(&db, 1, Duration::from_secs(60))
    })
    .await
    .unwrap();
    assert_eq!(opened, 1, "the pass thread holds its read connection");
    assert_eq!(lifecycle.active(), 1, "the pass thread is alive");

    // The restart drains: the wait is bounded, the helper never starts, the
    // deferral is recorded for the next daemon.
    lifecycle.close();
    let started = Instant::now();
    let outcome = tokio::task::spawn_blocking({
        let lifecycle = std::sync::Arc::clone(&lifecycle);
        move || {
            quiesce_then_run(
                &plan,
                Some(lifecycle.as_ref()),
                Duration::from_millis(300),
                |_| panic!("the helper must not start while a pass thread is alive"),
                Duration::from_millis(20),
            )
        }
    })
    .await
    .unwrap();
    assert!(matches!(outcome, HelperOutcome::Deferred(_)), "{outcome:?}");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the wait is bounded"
    );
    assert!(result_path.is_file(), "the deferral is on disk");

    // The pass gets the store back, sees the cancel flag at its next boundary and
    // ends partial; nothing is lost or half moved.
    drop(held);
    let status = wait_for_cold_archive_run(&fixture.server, &run_id.to_string()).await;
    assert_eq!(status["last_run"]["outcome"], "partial", "{status}");
    assert_eq!(lifecycle.active(), 0);
    assert_eq!(live_connections::open_on(&db), 0, "its connection closed");
    {
        let store = fixture.manager.store().lock().await;
        assert_eq!(live_rows(&store, session), 7);

        // The next daemon's reconcile settles the run: the request is back in the
        // queue, not failed and not completed.
        let settled = store.cold_reconcile_compactions(&db, None).unwrap();
        assert_eq!(settled.len(), 1, "{settled:?}");
        assert_eq!(settled[0].outcome, "deferred");
        assert_eq!(settled[0].error_code, None);
        assert!(store.cold_last_compaction().unwrap().pending);
    }
    assert!(!result_path.exists());

    // The restart that follows (or none) does not strand the archive.
    lifecycle.reopen();
    let status = run(&fixture, "move").await;
    assert_eq!(status["last_run"]["sessions_moved"], 1, "{status}");
    let store = fixture.manager.store().lock().await;
    assert_eq!(live_rows(&store, session), 3);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn the_weekly_verify_is_due_only_for_chunks_unread_for_a_week() {
    use crate::session::cold_archive::cold_verify_pass_due;
    let now = chrono::Utc::now();
    let week = chrono::Duration::days(7);
    assert!(!cold_verify_pass_due(None, None, now), "nothing archived");
    assert!(!cold_verify_pass_due(
        Some(now - chrono::Duration::days(6)),
        None,
        now
    ));
    assert!(cold_verify_pass_due(Some(now - week), None, now));
    assert!(
        !cold_verify_pass_due(
            Some(now - week),
            Some(now - chrono::Duration::hours(1)),
            now
        ),
        "a verify that started within the interval is not restarted"
    );
    assert!(cold_verify_pass_due(
        Some(now - week),
        Some(now - chrono::Duration::hours(7)),
        now
    ));
}
