//! Issue #1098: a worker never silently ends without RESULT.
use super::*;
use crate::session::worker_result_guard::{
    NO_RESULT_STOP_REASON, install_test_provider_pid, no_result_wake_id,
};
#[cfg(target_os = "linux")]
use crate::session::worker_result_guard::{live_descendant_count, settled_descendant_count};
use rsi_common::types::WakeMode;
use std::sync::Arc;

struct Fixture {
    manager: Arc<SessionManager>,
    _dir: TempDir,
}

fn fixture() -> Fixture {
    let (manager, dir) = manager();
    Fixture {
        manager: Arc::new(manager),
        _dir: dir,
    }
}

async fn insert(f: &Fixture, mut row: Session) -> Session {
    row.status = SessionStatus::Running;
    row.provider = SessionProvider::Claude;
    row.max_retries = None;
    row.retry_attempt = None;
    let store = f.manager.store.lock().await;
    if store.get_session(row.id).unwrap().is_none() {
        store.insert_session(&row).unwrap();
    }
    row
}

/// A parent (manager-side) session and a leaf worker owned by it.
async fn worker_pair(f: &Fixture) -> (Session, Session) {
    let parent = insert(f, bare_session(Uuid::new_v4())).await;
    let mut worker = bare_session(Uuid::new_v4());
    worker.parent_id = Some(parent.id);
    let worker = insert(f, worker).await;
    (parent, worker)
}

/// Run one full turn of a fake provider that says `text` then reports success.
async fn run_turn(f: &Fixture, worker: &Session, text: &str) {
    run_turn_messages(f, worker, &[text]).await;
}

/// One turn of a fake provider that sends each of `messages` as its own
/// assistant message, then reports success.
async fn run_turn_messages(f: &Fixture, worker: &Session, messages: &[&str]) {
    let text = messages.last().copied().unwrap_or_default();
    {
        let store = f.manager.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Running' WHERE id=?1",
                [worker.id.to_string()],
            )
            .unwrap();
    }
    f.manager.completed.write().await.remove(&worker.id);
    let (mut tracked, _) = scripted_process_tracked(worker.id, 7, false, 0, false, false, false);
    let mut session = worker.clone();
    session.status = SessionStatus::Running;
    tracked.session = session;
    let (stop_tx, stop_rx) = mpsc::channel(1);
    tracked.stop_tx = stop_tx;
    f.manager.active.write().await.insert(worker.id, tracked);
    let (provider_tx, provider_rx) = mpsc::channel(messages.len() + 8);
    for message in messages {
        provider_tx
            .send(StreamEvent {
                event_type: "assistant".into(),
                data: serde_json::json!({"role":"assistant","content": message}),
            })
            .await
            .unwrap();
    }
    provider_tx
        .send(StreamEvent {
            event_type: "result".into(),
            data: serde_json::json!({"subtype":"success","terminal_reason":"completed",
                "is_error":false,"result": text,"num_turns":1}),
        })
        .await
        .unwrap();
    drop(provider_tx);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        SessionManager::monitor_session(
            worker.id,
            7,
            Box::new(crate::provider::CliProviderSession::new(provider_rx)),
            f.manager.active.clone(),
            f.manager.completed.clone(),
            f.manager.event_bus.clone(),
            stop_rx,
            f.manager.store.clone(),
            f.manager.model_call_settlements.handle().unwrap(),
            f.manager.persistence.clone(),
            0,
            false,
            f.manager.socket_path.clone(),
            f.manager.token_counter.clone(),
            None,
            f.manager.retry_tx.clone(),
            f.manager.tool_registry.clone(),
            crate::turn_controller::TurnController::new(
                crate::turn_controller::ContinuationPolicy::Single,
            ),
            f.manager.runtime_config.clone(),
            f.manager.spawn_coordinator.clone(),
            f.manager.agent_tokens.clone(),
            f.manager.spawn_epoch.clone(),
            f.manager.agent_message_arbiter.clone(),
            f.manager.codegraph_handle.clone(),
            f.manager.custody_execution_runtime(),
        ),
    )
    .await
    .expect("fake provider turn finalizes");
}

async fn stored(f: &Fixture, id: Uuid) -> Session {
    f.manager
        .store
        .lock()
        .await
        .get_session(id)
        .unwrap()
        .unwrap()
}

async fn wake_count(f: &Fixture, worker: Uuid) -> i64 {
    f.manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT COUNT(*) FROM scheduled_jobs WHERE wake_session_id=?1 AND name LIKE 'worker-no-result-%'",
            [worker.to_string()],
            |row| row.get(0),
        )
        .unwrap()
}

/// A real child process tree: `sh` (the stand-in provider) with a live `sleep`.
struct LiveRun {
    sh: std::process::Child,
}

impl LiveRun {
    /// `sleeper` replaces `sleep` with another copy of it (a provider helper
    /// stand-in for #1399); `None` runs plain `sleep`.
    fn start(sleeper: Option<&std::path::Path>) -> Self {
        let script = sleeper.map_or_else(
            || "sleep 60 & wait".to_string(),
            |path| "sleep 60 & wait".replacen("sleep", &format!("'{}'", path.display()), 1),
        );
        let sh = std::process::Command::new("sh")
            .args(["-c", &script])
            .spawn()
            .expect("spawn stand-in provider tree");
        let pid = sh.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while raw_child_count(pid) == 0 {
            assert!(std::time::Instant::now() < deadline, "child never started");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Self { sh }
    }
}

/// Every child of `pid` (helpers included), by `/proc/*/stat`.
fn raw_child_count(pid: u32) -> usize {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("stat")).ok())
        .filter(|stat| {
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().nth(1))
                .and_then(|ppid| ppid.parse::<u32>().ok())
                == Some(pid)
        })
        .count()
}

impl Drop for LiveRun {
    fn drop(&mut self) {
        // Killing the group is not needed: the sleeper dies with its parent
        // shell's stdin-less wait only when signalled, so signal both.
        let _ = std::process::Command::new("pkill")
            .args(["-P", &self.sh.id().to_string()])
            .status();
        let _ = self.sh.kill();
        let _ = self.sh.wait();
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn no_result_with_live_child_gets_one_continuation_then_no_result() {
    let f = fixture();
    let (_parent, worker) = worker_pair(&f).await;
    let run = LiveRun::start(None);
    install_test_provider_pid(worker.id, run.sh.id());

    run_turn(&f, &worker, "waiting for the run").await;
    let first = stored(&f, worker.id).await;
    assert_eq!(first.status, SessionStatus::Completed);
    assert_ne!(first.stop_reason.as_deref(), Some(NO_RESULT_STOP_REASON));
    let wake = f
        .manager
        .store
        .lock()
        .await
        .get_scheduled_job(&no_result_wake_id(worker.id))
        .unwrap()
        .expect("one automatic continuation is scheduled");
    assert!(wake.enabled);
    assert_eq!(wake.wake_mode, WakeMode::Resume);
    assert_eq!(wake.wake_session_id, Some(worker.id));
    assert!(wake.message.contains("RESULT"));
    assert!(wake.message.contains("PIPELINE HANDOFF — <STAGE>:"));
    assert_eq!(wake_count(&f, worker.id).await, 1);

    // The continuation also ends without RESULT: terminal `no_result`, and no
    // second continuation.
    run_turn(&f, &worker, "still waiting").await;
    let second = stored(&f, worker.id).await;
    assert_eq!(second.status, SessionStatus::Completed);
    assert_eq!(second.stop_reason.as_deref(), Some(NO_RESULT_STOP_REASON));
    assert_eq!(wake_count(&f, worker.id).await, 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn running_agent_job_counts_as_owned_work() {
    let f = fixture();
    let (_parent, worker) = worker_pair(&f).await;
    {
        let store = f.manager.store.lock().await;
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "INSERT INTO agent_jobs(id,owner_session_id,kind,params_json,cwd,unit_name,log_path,status_path,state,created_at,row_version)
                 VALUES(?1,?2,'test','{}','/tmp','unit-1098','/tmp/log','/tmp/status','running',?3,1)",
                rusqlite::params![Uuid::new_v4().to_string(), worker.id.to_string(), now],
            )
            .unwrap();
    }
    run_turn(&f, &worker, "waiting for the job").await;
    assert_eq!(wake_count(&f, worker.id).await, 1);
}

/// #1734: a running job whose completion is awaited by an enabled
/// `when.jobs_terminal` wake is a correct wait, so the worker gets no nudge.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn running_job_with_completion_wake_is_not_nudged() {
    use rsi_common::wake_predicate::{WakePredicate, WakeWhenState};
    let f = fixture();
    let (_parent, worker) = worker_pair(&f).await;
    let job_id = Uuid::new_v4();
    {
        let store = f.manager.store.lock().await;
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "INSERT INTO agent_jobs(id,owner_session_id,kind,params_json,cwd,unit_name,log_path,status_path,state,created_at,row_version)
                 VALUES(?1,?2,'test','{}','/tmp','unit-1734','/tmp/log','/tmp/status','running',?3,1)",
                rusqlite::params![job_id.to_string(), worker.id.to_string(), now],
            )
            .unwrap();
        let mut wake = mk_watch_job_for(worker.id, worker.id, "job finished");
        wake.wake_mode = WakeMode::Resume;
        wake.schedule.recurrence = rsi_common::types::Recurrence::Once;
        wake.next_fire_at = chrono::Utc::now() + chrono::Duration::hours(1);
        store
            .insert_wake_when(
                &wake,
                &WakeWhenState {
                    predicate: WakePredicate {
                        sha_on_rolling: None,
                        issue_closed: None,
                        todo_closed: None,
                        topology_execution_settled: None,
                        health_anomaly: None,
                        settlement_run_settled: None,
                        jobs_terminal: Some(vec![job_id]),
                    },
                    armed_at: chrono::Utc::now(),
                    deadline: None,
                    repo_dir: None,
                },
                false,
            )
            .unwrap();
        assert!(
            store
                .owner_running_job_has_completion_wake(worker.id)
                .unwrap()
        );
    }
    run_turn(&f, &worker, "waiting for the job wake").await;
    assert_eq!(wake_count(&f, worker.id).await, 0);
    assert_eq!(stored(&f, worker.id).await.status, SessionStatus::Completed);
}

/// #1958: an armed durable wake the worker owns (`resume`, `on_terminal`) is
/// the pending result, even with a live child: no no-result continuation.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn armed_own_wake_is_the_pending_result_and_gets_no_nudge() {
    for mode in [WakeMode::Resume, WakeMode::OnTerminal(Uuid::new_v4())] {
        let f = fixture();
        let (_parent, worker) = worker_pair(&f).await;
        let run = LiveRun::start(None);
        install_test_provider_pid(worker.id, run.sh.id());
        {
            let store = f.manager.store.lock().await;
            let mut wake = mk_watch_job_for(worker.id, worker.id, "armed wake");
            wake.wake_mode = mode;
            wake.next_fire_at = chrono::Utc::now() + chrono::Duration::hours(1);
            store.insert_scheduled_job(&wake).unwrap();
            assert!(store.owner_has_armed_result_wake(worker.id).unwrap());
        }
        run_turn(&f, &worker, "waiting on my armed wake").await;
        assert_eq!(wake_count(&f, worker.id).await, 0);
        assert_eq!(stored(&f, worker.id).await.status, SessionStatus::Completed);
        assert_ne!(
            stored(&f, worker.id).await.stop_reason.as_deref(),
            Some(NO_RESULT_STOP_REASON)
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn result_line_or_idle_worker_is_never_continued() {
    let f = fixture();
    let (_parent, with_result) = worker_pair(&f).await;
    let run = LiveRun::start(None);
    install_test_provider_pid(with_result.id, run.sh.id());
    run_turn(&f, &with_result, "done\nRESULT ok commit=abc123").await;
    assert_eq!(wake_count(&f, with_result.id).await, 0);
    assert_ne!(
        stored(&f, with_result.id).await.stop_reason.as_deref(),
        Some(NO_RESULT_STOP_REASON)
    );

    // Nothing owned is running: an ordinary end of turn is left alone.
    let (_parent, idle) = worker_pair(&f).await;
    run_turn(&f, &idle, "just thinking out loud").await;
    assert_eq!(wake_count(&f, idle.id).await, 0);
    assert_eq!(stored(&f, idle.id).await.status, SessionStatus::Completed);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_and_epic_lead_sessions_are_never_continued() {
    let f = fixture();
    // Operator session: no parent.
    let operator = insert(&f, bare_session(Uuid::new_v4())).await;
    let run = LiveRun::start(None);
    install_test_provider_pid(operator.id, run.sh.id());
    run_turn(&f, &operator, "waiting for the run").await;
    assert_eq!(wake_count(&f, operator.id).await, 0);

    // A lead owns an Epic via `lead_session_id`.
    let (_parent, lead) = worker_pair(&f).await;
    {
        let store = f.manager.store.lock().await;
        let mut epic = bare_session(Uuid::new_v4());
        epic.session_kind = SessionKind::Epic;
        epic.lead_session_id = Some(lead.id);
        store.insert_session(&epic).unwrap();
    }
    install_test_provider_pid(lead.id, run.sh.id());
    run_turn(&f, &lead, "waiting for the run").await;
    assert_eq!(wake_count(&f, lead.id).await, 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[test]
fn result_line_detection_accepts_report_forms_only() {
    use crate::session::worker_result_guard::has_result_line;
    assert!(has_result_line("RESULT ok"));
    assert!(has_result_line("notes\n  RESULT: done"));
    assert!(has_result_line("**RESULT** ok") || has_result_line("RESULT"));
    // #1392: markup closing the keyword and the reviewer's handoff marker.
    assert!(has_result_line("**RESULT**: green"));
    assert!(has_result_line(
        "PIPELINE HANDOFF — REVIEW:\nREVIEW: ACCEPTED"
    ));
    assert!(has_result_line("## PIPELINE HANDOFF — 07e501d9f"));
    // #1616: a reviewer's closing verdict line is a report.
    assert!(has_result_line("verdict\nREVIEW APPROVE commit=abc"));
    assert!(has_result_line("REVIEW CHANGES: two blockers"));
    assert!(has_result_line("**REVIEW APPROVE**"));
    // #1791: reviewers also use lowercase or mixed-case verdicts.
    assert!(has_result_line("REVIEW approve 0 findings\nFriction: none"));
    assert!(has_result_line("REVIEW changes 2 findings"));
    assert!(has_result_line("**REVIEW Approve**"));
    assert!(!has_result_line("REVIEW: ACCEPTED"));
    assert!(!has_result_line("REVIEW APPROVED by nobody yet"));
    assert!(!has_result_line("REVIEW approved by nobody yet"));
    assert!(!has_result_line("REVIEW changeset pending"));
    assert!(!has_result_line("REVIEW 批准 findings"));
    assert!(!has_result_line("REVIEWS pending"));
    assert!(!has_result_line("the result is pending"));
    assert!(!has_result_line("RESULTS are pending"));
    assert!(!has_result_line("waiting for the run"));
}

// ---- #1124: delivery-time eligibility of the automatic continuation ----

/// A Completed leaf worker with its one armed continuation wake.
async fn worker_with_armed_wake(
    f: &Fixture,
) -> (Session, LiveRun, rsi_common::types::ScheduledJob) {
    let (_parent, worker) = worker_pair(f).await;
    let run = LiveRun::start(None);
    install_test_provider_pid(worker.id, run.sh.id());
    run_turn(f, &worker, "waiting for the run").await;
    let wake = f
        .manager
        .store
        .lock()
        .await
        .get_scheduled_job(&no_result_wake_id(worker.id))
        .unwrap()
        .expect("continuation armed");
    assert!(wake.enabled);
    (worker, run, wake)
}

async fn deliver(
    f: &Fixture,
    worker: &Session,
    wake: &rsi_common::types::ScheduledJob,
) -> Result<Uuid> {
    crate::issue_tracker::poller::SessionLauncher::resume_scheduled_job(
        f.manager.as_ref(),
        worker.id,
        "continue".into(),
        vec![wake.id],
    )
    .await
}

async fn assert_retired_at_delivery(
    f: &Fixture,
    worker: &Session,
    wake: &rsi_common::types::ScheduledJob,
) {
    let error = deliver(f, worker, wake)
        .await
        .expect_err("ineligible wake is refused");
    assert!(
        crate::session::worker_result_guard::is_no_result_retired(&error),
        "{error}"
    );
    // The scheduler path settles (retires) the one-shot row and starts nothing.
    let launcher: Arc<dyn crate::issue_tracker::poller::SessionLauncher> = f.manager.clone();
    crate::scheduler::fire_job_for_test(&f.manager.store, f.manager.event_bus(), &launcher, wake)
        .await;
    let settled = f
        .manager
        .store
        .lock()
        .await
        .get_scheduled_job(&wake.id)
        .unwrap()
        .unwrap();
    assert!(!settled.enabled, "the ineligible wake is retired");
    assert!(!f.manager.active.read().await.contains_key(&worker.id));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn eligible_wake_passes_the_delivery_check_and_stays_armed() {
    let f = fixture();
    let (worker, _run, wake) = worker_with_armed_wake(&f).await;
    let store = f.manager.store.lock().await;
    store
        .check_scheduled_wake_owner(worker.id, &[wake.id])
        .expect("an eligible Completed leaf worker is continued");
    assert!(store.no_result_wake_pending(worker.id).unwrap());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn pause_between_scheduling_and_delivery_retires_the_wake() {
    for pause in [
        crate::store::manager_actions::OperatorPause::Hard,
        crate::store::manager_actions::OperatorPause::Soft,
    ] {
        let f = fixture();
        let (worker, _run, wake) = worker_with_armed_wake(&f).await;
        f.manager
            .store
            .lock()
            .await
            .set_operator_pause(worker.id, pause)
            .unwrap();
        assert_retired_at_delivery(&f, &worker, &wake).await;
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_halt_between_scheduling_and_delivery_retires_the_wake() {
    let f = fixture();
    let (worker, _run, wake) = worker_with_armed_wake(&f).await;
    // The operator halts the worker between scheduling and delivery: the row
    // is Interrupted (an idle Completed worker has no live process to halt).
    f.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET status='Interrupted' WHERE id=?1",
            [worker.id.to_string()],
        )
        .unwrap();
    assert_retired_at_delivery(&f, &worker, &wake).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn pending_approval_or_question_retires_the_wake() {
    let f = fixture();
    let (worker, _run, wake) = worker_with_armed_wake(&f).await;
    {
        let store = f.manager.store.lock().await;
        store
            .conn
            .execute(
                "INSERT INTO approvals(id,session_id,tool_name,tool_input,status,created_at)
                 VALUES(?1,?2,'Bash','{}','Pending',?3)",
                rusqlite::params![
                    Uuid::new_v4().to_string(),
                    worker.id.to_string(),
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
    }
    assert_retired_at_delivery(&f, &worker, &wake).await;

    let f = fixture();
    let (worker, _run, wake) = worker_with_armed_wake(&f).await;
    f.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET pending_question_json='not json' WHERE id=?1",
            [worker.id.to_string()],
        )
        .unwrap();
    assert_retired_at_delivery(&f, &worker, &wake).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn archived_or_pending_archive_worker_retires_the_wake() {
    let f = fixture();
    let (worker, _run, wake) = worker_with_armed_wake(&f).await;
    f.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET pending_archive=1 WHERE id=?1",
            [worker.id.to_string()],
        )
        .unwrap();
    assert_retired_at_delivery(&f, &worker, &wake).await;

    let f = fixture();
    let (worker, _run, wake) = worker_with_armed_wake(&f).await;
    f.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET status='Archived' WHERE id=?1",
            [worker.id.to_string()],
        )
        .unwrap();
    assert_retired_at_delivery(&f, &worker, &wake).await;
}

// ---- #1109: the terminal watch fires once, on the final Completed ----

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn terminal_watch_waits_for_the_automatic_continuation_then_fires() {
    use crate::session::WatchDecision;
    let f = fixture();
    let (worker, _run, wake) = worker_with_armed_wake(&f).await;
    // Transient Completed with the continuation armed: the watch is held.
    let (held, _) = f
        .manager
        .watch_decision_for(worker.id, true, worker.parent_id.unwrap())
        .await
        .unwrap();
    assert_eq!(held, WatchDecision::NotReady);

    // The continuation is retired (ineligible at delivery): the watch fires.
    f.manager
        .store
        .lock()
        .await
        .set_operator_pause(
            worker.id,
            crate::store::manager_actions::OperatorPause::Hard,
        )
        .unwrap();
    assert_retired_at_delivery(&f, &worker, &wake).await;
    let (decision, _) = f
        .manager
        .watch_decision_for(worker.id, true, worker.parent_id.unwrap())
        .await
        .unwrap();
    assert!(
        matches!(decision, WatchDecision::Fire { .. }),
        "{decision:?}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn terminal_watch_fires_on_the_final_no_result_completed() {
    use crate::session::WatchDecision;
    let f = fixture();
    let (worker, _run, _wake) = worker_with_armed_wake(&f).await;
    // The continuation runs (the row settles on delivery) and the worker
    // ends without RESULT again: the final Completed is not held.
    f.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE scheduled_jobs SET enabled=0 WHERE id=?1",
            [no_result_wake_id(worker.id).to_string()],
        )
        .unwrap();
    run_turn(&f, &worker, "still waiting").await;
    let second = stored(&f, worker.id).await;
    assert_eq!(second.stop_reason.as_deref(), Some(NO_RESULT_STOP_REASON));
    let (decision, _) = f
        .manager
        .watch_decision_for(worker.id, true, worker.parent_id.unwrap())
        .await
        .unwrap();
    assert!(
        matches!(decision, WatchDecision::Fire { .. }),
        "{decision:?}"
    );
}

// ---- #1392 / #1399: finished workers and reviewers get no nudge ----

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn finished_worker_and_reviewer_get_no_nudge() {
    let f = fixture();
    let run = LiveRun::start(None);

    // #1392: the RESULT is its own message after prose with no trailing
    // newline (Codex), so the turn accumulator holds it mid-line.
    let (_parent, worker) = worker_pair(&f).await;
    install_test_provider_pid(worker.id, run.sh.id());
    run_turn_messages(
        &f,
        &worker,
        &[
            "All 44 tests passed; recording the handoff for review.",
            "RESULT ba2f389e6b issue=#1351 status=green",
        ],
    )
    .await;
    assert_eq!(wake_count(&f, worker.id).await, 0);
    let stored_worker = stored(&f, worker.id).await;
    assert_eq!(stored_worker.status, SessionStatus::Completed);
    assert_ne!(
        stored_worker.stop_reason.as_deref(),
        Some(NO_RESULT_STOP_REASON)
    );

    // A reviewer reports with a PIPELINE HANDOFF and REVIEW fields, no RESULT.
    let (_parent, reviewer) = worker_pair(&f).await;
    install_test_provider_pid(reviewer.id, run.sh.id());
    run_turn(
        &f,
        &reviewer,
        "PIPELINE HANDOFF — REVIEW:\nREVIEW: ACCEPTED commit=ba2f389e6b\nFriction: none",
    )
    .await;
    assert_eq!(wake_count(&f, reviewer.id).await, 0);
    assert_eq!(
        stored(&f, reviewer.id).await.status,
        SessionStatus::Completed
    );

    // #1791: a bare lowercase verdict is also a report, even with a live run.
    for report in ["REVIEW approve 0 findings", "REVIEW changes 2 findings"] {
        let (_parent, reviewer) = worker_pair(&f).await;
        install_test_provider_pid(reviewer.id, run.sh.id());
        run_turn(&f, &reviewer, report).await;
        assert_eq!(wake_count(&f, reviewer.id).await, 0);
        assert_eq!(
            stored(&f, reviewer.id).await.status,
            SessionStatus::Completed
        );
    }
}

/// #1459: a foreground command that has just returned is still winding down
/// (its `sleep` stands in for the shell, wrapper or pipe tail) when the
/// provider emits its result. One sample sees it; the settled count does not.
#[cfg(target_os = "linux")]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[test]
fn just_finished_foreground_command_is_not_a_live_descendant() {
    let mut sh = std::process::Command::new("sh")
        .args(["-c", "sleep 0.3 & wait"])
        .spawn()
        .expect("spawn stand-in provider tree");
    let pid = sh.id();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while raw_child_count(pid) == 0 {
        assert!(std::time::Instant::now() < deadline, "child never started");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        live_descendant_count(pid),
        1,
        "one sample sees the winding-down command"
    );
    assert_eq!(
        settled_descendant_count(pid, std::time::Duration::from_millis(800)),
        0,
        "the command exited within the settle window"
    );
    let _ = sh.wait();
}

#[cfg(target_os = "linux")]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[test]
fn still_running_command_survives_the_settle_window() {
    let run = LiveRun::start(None);
    assert_eq!(
        settled_descendant_count(run.sh.id(), std::time::Duration::from_millis(100)),
        1
    );
}

/// A copy of `sleep` named like the provider's MCP gateway.
#[cfg(target_os = "linux")]
fn helper_named_sleeper(dir: &std::path::Path) -> std::path::PathBuf {
    let sleep = ["/usr/bin/sleep", "/bin/sleep"]
        .into_iter()
        .map(std::path::PathBuf::from)
        .find(|path| path.exists())
        .expect("a sleep binary");
    let helper = dir.join("rsi-agent-mcp");
    std::fs::copy(sleep, &helper).expect("copy sleep");
    helper
}

#[cfg(target_os = "linux")]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn finished_run_with_only_provider_helpers_is_not_still_going() {
    let f = fixture();
    let dir = tempfile::tempdir().unwrap();
    let helper = helper_named_sleeper(dir.path());
    // #1399: every command returned; only the provider's helper remains.
    let run = LiveRun::start(Some(&helper));
    assert_eq!(raw_child_count(run.sh.id()), 1, "the helper is running");
    assert_eq!(
        live_descendant_count(run.sh.id()),
        0,
        "a provider helper is not a run"
    );
    let (_parent, worker) = worker_pair(&f).await;
    install_test_provider_pid(worker.id, run.sh.id());
    run_turn(&f, &worker, "all three cargo commands exited 0").await;
    assert_eq!(wake_count(&f, worker.id).await, 0);
    assert_eq!(stored(&f, worker.id).await.status, SessionStatus::Completed);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn armed_wake_is_retired_when_the_latest_turn_already_reported() {
    let f = fixture();
    let (worker, _run, wake) = worker_with_armed_wake(&f).await;
    {
        let store = f.manager.store.lock().await;
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "INSERT INTO conversation_events(session_id,sequence,event_type,role,content,created_at)
                 VALUES(?1,(SELECT COALESCE(MAX(sequence),0)+1000 FROM conversation_events WHERE session_id=?1),
                        'Message','Assistant','RESULT abc1234 status=green',?2)",
                rusqlite::params![worker.id.to_string(), now],
            )
            .unwrap();
        assert!(store.latest_turn_reported(worker.id).unwrap());
    }
    assert_retired_at_delivery(&f, &worker, &wake).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn result_sha_terminal_report_persists_mismatch_and_verified_replacement() {
    let f = fixture();
    let (_, mut worker) = worker_pair(&f).await;
    let repo = crate::test_support::disk_backed_tempdir("result-sha-terminal");
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q"]);
    git(&["commit", "--allow-empty", "-qm", "fixture"]);
    let head = git(&["rev-parse", "HEAD"]);
    worker.sandbox_root = Some(repo.path().to_path_buf());
    let fabricated = format!(
        "{}{}",
        &head[..7],
        if head[7..].chars().all(|c| c == '0') {
            "1".repeat(33)
        } else {
            "0".repeat(33)
        }
    );
    let report =
        format!("PIPELINE HANDOFF — IMPLEMENTATION:\nRESULT {fabricated} issue=#1494 status=green");
    run_turn(&f, &worker, &report).await;
    let row = stored(&f, worker.id).await;
    assert_eq!(row.status, SessionStatus::Completed);
    assert_eq!(row.stop_reason.as_deref(), Some("result_sha_unknown"));
    let store = f.manager.store.lock().await;
    let raw = store
        .last_assistant_message_event(worker.id)
        .unwrap()
        .unwrap();
    assert_eq!(raw.content, report, "raw provider report remains auditable");
    let normalized =
        crate::session::worker_result_guard::result_sha::normalized_report(&store, &raw)
            .unwrap()
            .unwrap();
    assert_eq!(normalized, report.replace(&fabricated, &head));
    let notice = compose_watch_line(&row, None, false, None, None, "worker completed");
    assert!(
        notice.contains("result-sha: result_sha_unknown"),
        "{notice}"
    );
    let diagnostics = store
        .list_session_diagnostics(worker.id, None, 100)
        .unwrap();
    assert!(diagnostics.iter().any(|d| {
        d.fields
            .as_ref()
            .is_some_and(|fields| fields["code"] == "result_sha_unknown")
    }));
    drop(store);

    // A later valid abbreviated report has its own verification and cause.
    run_turn(&f, &worker, &format!("RESULT {} status=green", &head[..7])).await;
    let row = stored(&f, worker.id).await;
    assert_eq!(row.stop_reason.as_deref(), Some("success"));
    let store = f.manager.store.lock().await;
    let raw = store
        .last_assistant_message_event(worker.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        crate::session::worker_result_guard::result_sha::normalized_report(&store, &raw)
            .unwrap()
            .unwrap(),
        format!("RESULT {head} status=green")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn result_sha_without_sandbox_is_flagged_without_guessing_head() {
    let f = fixture();
    let (_, worker) = worker_pair(&f).await;
    run_turn(&f, &worker, "PIPELINE HANDOFF — BATON 1234567").await;
    let row = stored(&f, worker.id).await;
    assert_eq!(row.status, SessionStatus::Completed);
    assert_eq!(row.stop_reason.as_deref(), Some("result_sha_unknown"));
    let store = f.manager.store.lock().await;
    let raw = store
        .last_assistant_message_event(worker.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        crate::session::worker_result_guard::result_sha::normalized_report(&store, &raw)
            .unwrap()
            .unwrap(),
        raw.content
    );
}

/// #1555: the manager's watch line carries the stranded-worker note.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn watch_line_carries_the_stranded_worker_note() {
    let f = fixture();
    let (_, worker) = worker_pair(&f).await;
    let row = stored(&f, worker.id).await;
    let note = "no-result: stranded; owns job 12345678 unit rsi-job-unit";
    let line = compose_watch_line(&row, None, false, None, Some(note), "worker completed");
    assert!(line.contains(&format!("({note})")), "{line}");
}

/// #31: the lead's watch line names a child that stopped at a rejected tool
/// call, with the exact id to continue and the dirty-sandbox flag.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn watch_line_names_a_rejected_tool_stop_with_the_exact_recovery_id() {
    let f = fixture();
    let (_, worker) = worker_pair(&f).await;
    let mut row = stored(&f, worker.id).await;
    row.terminal_reason = Some("provider_tool_rejected".to_string());
    let line = compose_watch_line(&row, Some(false), false, Some(true), None, "worker ended");
    assert!(line.contains("stopped-at-rejected-tool: true"), "{line}");
    assert!(
        line.contains(&format!("recover: AgentContinueChild {}", worker.id)),
        "{line}"
    );
    assert!(line.contains("sandbox-worktree-dirty: true"), "{line}");

    row.terminal_reason = Some("completed".to_string());
    let plain = compose_watch_line(&row, None, false, None, None, "worker ended");
    assert!(!plain.contains("AgentContinueChild"), "{plain}");
}

/// A running daemon job owned by `worker`.
async fn own_running_job(f: &Fixture, worker: &Session) {
    let store = f.manager.store.lock().await;
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    store
        .conn
        .execute(
            "INSERT INTO agent_jobs(id,owner_session_id,kind,params_json,cwd,unit_name,log_path,status_path,state,created_at,row_version)
             VALUES(?1,?2,'test','{}','/tmp',?4,'/tmp/log','/tmp/status','running',?3,1)",
            rusqlite::params![
                Uuid::new_v4().to_string(),
                worker.id.to_string(),
                now,
                format!("unit-151-{}", Uuid::new_v4()),
            ],
        )
        .unwrap();
}

/// The worker appended a handoff to an Issue (`AgentUpdateIssue {handoff}`).
async fn publish_issue_handoff(f: &Fixture, worker: &Session) {
    let store = f.manager.store.lock().await;
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    store.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    store
        .conn
        .execute(
            "INSERT INTO issue_handoffs
                 (issue_id, sequence, project_id, author_session_id, idempotency_key, body, created_at)
             VALUES (?1, 1, ?2, ?3, 'handoff-151', 'LANDED abc1234', ?4)",
            rusqlite::params![
                Uuid::new_v4().to_string(),
                Uuid::new_v4().to_string(),
                worker.id.to_string(),
                now
            ],
        )
        .unwrap();
    store.conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
}

/// The launch brief: the session's first user message.
async fn record_launch_brief(f: &Fixture, worker: &Session, brief: &str) {
    let store = f.manager.store.lock().await;
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    store
        .conn
        .execute(
            "INSERT INTO conversation_events(session_id,sequence,event_type,role,content,created_at)
             VALUES(?1,-1000,'Message','User',?2,?3)",
            rusqlite::params![worker.id.to_string(), brief, now],
        )
        .unwrap();
}

/// #151: a worker that published its handoff to its Issue has reported, even
/// when its last message carries no `RESULT`/`PIPELINE HANDOFF` marker and a
/// job row is still open: no nudge. One that published nothing still gets it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn published_issue_handoff_gets_no_nudge_and_a_silent_worker_still_does() {
    let f = fixture();
    let (_parent, published) = worker_pair(&f).await;
    own_running_job(&f, &published).await;
    publish_issue_handoff(&f, &published).await;
    run_turn(&f, &published, "Landed and published the handoff.").await;
    assert_eq!(wake_count(&f, published.id).await, 0);
    assert_ne!(
        stored(&f, published.id).await.stop_reason.as_deref(),
        Some(NO_RESULT_STOP_REASON)
    );

    let (_parent, silent) = worker_pair(&f).await;
    own_running_job(&f, &silent).await;
    run_turn(&f, &silent, "Landed and published the handoff.").await;
    assert_eq!(wake_count(&f, silent.id).await, 1);
}

/// #151: the result format the launch brief declares is a final report; a
/// worker that does not use it, or has no brief format, is still nudged.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn launch_brief_result_format_is_a_final_report() {
    let brief = "Land the change. End with a final line starting `LANDED <sha>`.";
    let f = fixture();

    let (_parent, declared) = worker_pair(&f).await;
    record_launch_brief(&f, &declared, brief).await;
    own_running_job(&f, &declared).await;
    run_turn(&f, &declared, "LANDED abc1234 on rolling").await;
    assert_eq!(wake_count(&f, declared.id).await, 0);

    let (_parent, wrong_format) = worker_pair(&f).await;
    record_launch_brief(&f, &wrong_format, brief).await;
    own_running_job(&f, &wrong_format).await;
    run_turn(&f, &wrong_format, "still waiting on the run").await;
    assert_eq!(wake_count(&f, wrong_format.id).await, 1);

    let (_parent, no_brief_format) = worker_pair(&f).await;
    own_running_job(&f, &no_brief_format).await;
    run_turn(&f, &no_brief_format, "LANDED abc1234 on rolling").await;
    assert_eq!(wake_count(&f, no_brief_format.id).await, 1);
}

/// #151: an armed continuation is retired at delivery when the worker has
/// published its Issue handoff since it was armed.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn armed_wake_is_retired_when_the_worker_published_its_issue_handoff() {
    let f = fixture();
    let (worker, _run, wake) = worker_with_armed_wake(&f).await;
    publish_issue_handoff(&f, &worker).await;
    assert_retired_at_delivery(&f, &worker, &wake).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[test]
fn declared_result_prefixes_come_from_report_format_lines_only() {
    use crate::store::worker_no_result::declared_result_prefixes;
    assert_eq!(
        declared_result_prefixes("End with a final line starting `LANDED <sha>`."),
        vec!["LANDED".to_string()]
    );
    assert_eq!(
        declared_result_prefixes("Your final message must start with \"DONE:\" then a summary."),
        vec!["DONE".to_string()]
    );
    // Built-in keywords, non-report lines and mixed-case spans are not formats.
    assert!(
        declared_result_prefixes(
            "The final report is a `PIPELINE HANDOFF — <STAGE>:` or `RESULT ...`.\n\
             Run `FOO_BAR` first.\nEnd your final message with `Friction: none`."
        )
        .is_empty()
    );
}
