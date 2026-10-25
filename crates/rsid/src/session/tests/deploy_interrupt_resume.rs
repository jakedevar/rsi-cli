//! #1461: a worker whose turn a deploy restart interrupts is continued by the
//! next process (the existing graceful-restart journal), so
//! `AgentRequestDeploy {interrupt_workers: true}` needs no resume path of its own.
use super::*;
use crate::session::launch::install_controller_candidate_test_process;
use std::sync::atomic::Ordering;

fn git_repo() -> TempDir {
    let repo = disk_backed_tempdir();
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=Deploy Test",
            "-c",
            "user.email=deploy@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ],
    ] {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    repo
}

/// A worker mid-turn on a resumable provider session: a running admitted
/// invocation and verified ordinary custody, as a live turn leaves them.
async fn running_worker(manager: &SessionManager, repo: &TempDir) -> Session {
    let mut session = bare_session(Uuid::new_v4());
    session.status = SessionStatus::Running;
    session.provider = SessionProvider::Claude;
    session.claude_session_id = Some("provider-session-of-the-worker".into());
    session.working_dir = repo.path().to_path_buf();
    insert_row(manager, &session).await;
    let invocation = Uuid::new_v4();
    let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let store = manager.store.lock().await;
    store
        .conn
        .execute(
            "INSERT INTO model_invocations
             (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
              trigger_source,session_id,created_at)
             VALUES(?1,'session.launch.fresh','session_lifecycle','foreground','paid_capable','admitted','running',
                    'launch_session',?2,?3)",
            rusqlite::params![invocation.to_string(), session.id.to_string(), at],
        )
        .expect("running invocation");
    store
        .set_session_model_invocation(session.id, Some(invocation))
        .expect("bind the running invocation");
    store
        .conn
        .execute(
            "INSERT INTO session_execution_projections
             (session_id,schema_version,projection_version,execution_state,freshness,
              canonical_repo_dir,effective_cwd,validated_at,updated_at)
             VALUES(?1,1,1,'ordinary_unsandboxed','verified',?2,?2,?3,?3)
             ON CONFLICT(session_id) DO UPDATE SET execution_state='ordinary_unsandboxed',
               freshness='verified',effective_cwd=excluded.effective_cwd,
               validated_at=excluded.validated_at,updated_at=excluded.updated_at",
            rusqlite::params![
                session.id.to_string(),
                session.working_dir.to_str().expect("UTF-8 fixture path"),
                at
            ],
        )
        .expect("verified ordinary custody");
    session
}

/// The deploy runner's restart trigger starts the drain; the drain interrupts
/// the turn still running when its window ends and journals it. The next
/// process reconciles the journal at startup and dispatches the continuation
/// with the daemon's continue prompt.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn a_worker_cut_off_by_a_deploy_restart_is_continued_by_the_next_process() {
    let (manager, dir) = manager();
    let repo = git_repo();
    let worker = running_worker(&manager, &repo).await;
    manager
        .active
        .write()
        .await
        .insert(worker.id, TrackedSession::new_for_test(worker.clone()));

    // The deploy's restart trigger (`DeployService::set_restart_trigger`).
    manager.request_drain_restart();
    cut_and_continue_in_the_next_process(manager, dir, worker).await;
}

/// #2528: a foreground turn still running when model-call settlement starts
/// (it began after the first interrupt pass) gets the ordinary restart
/// treatment, and the settlement wait is bounded: past the bound the unsettled
/// invocation is recorded for the next boot and shutdown completes instead of
/// retrying forever against a turn that never ends.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn a_running_turn_does_not_hold_settlement_shutdown_past_its_bound_2528() {
    let (manager, _dir) = manager();
    let repo = git_repo();
    let worker = running_worker(&manager, &repo).await;
    // The turn's settlement producer, held for as long as the turn runs.
    let producer = manager.model_call_settlements.handle().unwrap();
    let interrupts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let kills = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut tracked = TrackedSession::new_for_test(worker.clone());
    tracked.process = Some(crate::session::types::ProviderProcess::Scripted(
        crate::session::types::ScriptedProcess {
            alive: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            exit_code: Arc::new(std::sync::atomic::AtomicI32::new(0)),
            interrupt_count: interrupts.clone(),
            kill_count: kills.clone(),
            // A turn that does not end on its own.
            exit_on_interrupt: false,
            interrupt_fails: false,
            kill_fails: false,
        },
    ));
    manager.request_drain_restart();
    // The turn becomes active after shutdown's first interrupt pass.
    manager.active.write().await.insert(worker.id, tracked);

    let started = std::time::Instant::now();
    manager
        .settle_model_calls_for_shutdown(std::time::Duration::from_millis(2500))
        .await
        .expect("a bounded shutdown completes");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "the settlement wait is bounded, took {:?}",
        started.elapsed()
    );
    assert!(
        interrupts.load(Ordering::SeqCst) >= 1,
        "the late turn is interrupted"
    );
    assert!(
        kills.load(Ordering::SeqCst) >= 1,
        "a turn that ignores the interrupt is killed"
    );
    assert!(
        !manager.active.read().await.contains_key(&worker.id),
        "the cut turn leaves the active map"
    );

    let store = manager.store.lock().await;
    let intents: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM daemon_restart_intents WHERE session_id=?1",
            [worker.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(intents, 1, "the turn is resumed after the restart");
    let invocation = store
        .session_model_invocation_id(worker.id)
        .unwrap()
        .expect("bound invocation");
    let record = store
        .open_unsettled_model_invocations()
        .unwrap()
        .expect("the unsettled invocation is recorded");
    assert_eq!(record.boot_id, manager.program_run_boot_id);
    assert_eq!(record.invocation_ids, vec![invocation]);

    // The next boot reconciles it and closes the record (#2528 (c)).
    store.reconcile_running_model_invocations().unwrap();
    let recovered = store
        .recover_recorded_unsettled_model_invocations()
        .unwrap();
    assert_eq!(recovered.len(), 1);
    assert_ne!(recovered[0].status.as_deref(), Some("running"));
    assert_eq!(store.open_unsettled_model_invocations().unwrap(), None);
    drop(store);
    drop(producer);
}

/// #2353: a worker blocked only on a control call the daemon is serving ends
/// the DRAIN wait early (the operator wait would otherwise run out its 30 s
/// limit) and is then cut exactly like any other turn the drain interrupts:
/// the same journal, the same continuation by the next process.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn a_worker_blocked_on_a_daemon_call_ends_the_drain_early_and_is_continued_by_the_next_process()
 {
    use crate::session::OperatorDrainDecision;
    use crate::session::daemon_wait::TurnToolLedger;

    let (manager, dir) = manager();
    let repo = git_repo();
    let worker = running_worker(&manager, &repo).await;
    let mut tracked = TrackedSession::new_for_test(worker.clone());
    tracked.turn_tools = TurnToolLedger::default();
    tracked.turn_tools.observe_stream(
        "assistant",
        &serde_json::json!({"message": {"content": [{
            "type": "tool_use",
            "id": "call-1",
            "name": "mcp__rsi-agent__rsi_control_get_issue",
            "input": {},
        }]}}),
    );
    manager.active.write().await.insert(worker.id, tracked);
    let _call = manager.daemon_wait.begin_settled_for_test(worker.id);
    manager.request_drain_restart();

    // The operator wait ends at once, long before its limit.
    assert_eq!(
        manager
            .operator_drain_decision(
                std::time::Duration::ZERO,
                std::time::Duration::from_secs(30)
            )
            .await,
        OperatorDrainDecision::Complete {
            active_count: 1,
            daemon_waiting: 1,
            deploy_interrupted: 0,
            limit_reached: false,
        }
    );
    // `shutdown_drain` does not wait it out a second time.
    assert!(
        !manager
            .wait_for_finishable_turns(std::time::Duration::from_secs(30))
            .await
            .expect("the finishable-turn wait"),
        "the daemon-wait worker does not hold the wait to its deadline"
    );
    cut_and_continue_in_the_next_process(manager, dir, worker).await;
}

/// The drain's interrupt of `worker`, then the next process: it reconciles the
/// journal at startup and continues the same session with one provider turn.
async fn cut_and_continue_in_the_next_process(
    manager: SessionManager,
    dir: TempDir,
    worker: Session,
) {
    manager
        .interrupt_restart_sessions(vec![worker.id])
        .await
        .expect("the drain interrupts the worker mid-turn");
    assert!(manager.active.read().await[&worker.id].interrupt_requested);
    let intent: (String, Option<String>) = manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT state,outcome FROM daemon_restart_intents WHERE session_id=?1",
            [worker.id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("the interrupt is journaled before the process exits");
    assert_eq!(
        intent,
        (
            "pending".to_string(),
            Some("shutdown_cancelled".to_string())
        )
    );

    // The next process: same database, new daemon.
    drop(manager);
    let store =
        Store::open_test_fixture(&dir.path().join("rsi.db")).expect("reopen after the restart");
    let config = Config::from_env();
    let runtime_config = RuntimeConfig::from_config(&config);
    runtime_config
        .sandbox_min_free_gib
        .store(0, std::sync::atomic::Ordering::Relaxed);
    let restarted = SessionManager::new(
        std::sync::Arc::new(EventBus::new(16)),
        store,
        false,
        dir.path().join("restarted.sock"),
        None,
        Vec::new(),
        runtime_config,
        dir.path().join("sandboxes"),
    )
    .expect("restarted manager");
    restarted.restore_sessions().await.expect("startup restore");
    assert_eq!(
        restarted
            .store
            .lock()
            .await
            .get_session(worker.id)
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Interrupted,
        "the cut-off turn is recorded as interrupted, not crashed"
    );

    let process = install_controller_candidate_test_process(worker.id);
    restarted
        .reconcile_restart_intents_at_startup()
        .await
        .expect("startup reconciliation");
    assert_eq!(
        process.productive_start_count.load(Ordering::SeqCst),
        1,
        "the continuation starts exactly one provider turn"
    );
    let store = restarted.store.lock().await;
    let (state, continuation): (String, Option<String>) = store
        .conn
        .query_row(
            "SELECT state,continuation_invocation_id FROM daemon_restart_intents WHERE session_id=?1",
            [worker.id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "delivered");
    assert!(continuation.is_some());
    let purpose: String = store
        .conn
        .query_row(
            "SELECT purpose FROM model_invocations WHERE id=?1",
            [continuation.unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(purpose, "session.continue.resume");
}

/// #1641 S4a: what the topology executor observes of a node session a deploy
/// drain cut off. Until the next process continues it, the restart journal
/// owns it, so the executor must hold it live (never settle or preserve it);
/// once the journal has continued the same session it is an ordinary running
/// node again, and the attempt follows it to completion.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn deploy_drain_cut_topology_node_resumes_and_execution_completes() {
    use crate::topology::executor::NodeEffects;
    let (manager, dir) = manager();
    let repo = git_repo();
    let worker = running_worker(&manager, &repo).await;
    manager
        .active
        .write()
        .await
        .insert(worker.id, TrackedSession::new_for_test(worker.clone()));
    manager.request_drain_restart();
    manager
        .interrupt_restart_sessions(vec![worker.id])
        .await
        .expect("the drain interrupts the worker mid-turn");

    drop(manager);
    let store =
        Store::open_test_fixture(&dir.path().join("rsi.db")).expect("reopen after the restart");
    let config = Config::from_env();
    let runtime_config = RuntimeConfig::from_config(&config);
    runtime_config
        .sandbox_min_free_gib
        .store(0, std::sync::atomic::Ordering::Relaxed);
    let restarted = std::sync::Arc::new(
        SessionManager::new(
            std::sync::Arc::new(EventBus::new(16)),
            store,
            false,
            dir.path().join("restarted.sock"),
            None,
            Vec::new(),
            runtime_config,
            dir.path().join("sandboxes"),
        )
        .expect("restarted manager"),
    );
    restarted.restore_sessions().await.expect("startup restore");
    let executor = restarted.topology_executor().await;

    // Before the startup pass: cut off, owned by the journal, resumable.
    let cut = executor
        .effects
        .session(worker.id)
        .await
        .expect("the node session is observable");
    assert_eq!(cut.status, SessionStatus::Interrupted);
    assert!(cut.restart_intent_pending, "the journal owns the session");
    assert!(cut.resumable);
    assert!(cut.cut_by_restart());

    let process = install_controller_candidate_test_process(worker.id);
    restarted
        .reconcile_restart_intents_at_startup()
        .await
        .expect("startup reconciliation");
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);

    // After it: the same session runs again and the journal no longer owns it.
    let resumed = executor
        .effects
        .session(worker.id)
        .await
        .expect("the same node session");
    assert!(
        matches!(
            resumed.status,
            SessionStatus::Starting | SessionStatus::Running
        ),
        "the continued turn is live, got {:?}",
        resumed.status
    );
    assert!(!resumed.restart_intent_pending);
}

/// #2375: the startup claim and the provider continuation are separate steps.
/// The claim (before readiness) starts no provider turn and leaves the session
/// durably journal-owned for the topology executor; the continuation (after
/// readiness) starts exactly one turn in journal order and delivers the intent.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn restart_claim_starts_no_turn_and_the_deferred_continuation_resumes_the_session() {
    use crate::topology::executor::NodeEffects;
    let (manager, dir) = manager();
    let repo = git_repo();
    let worker = running_worker(&manager, &repo).await;
    manager
        .active
        .write()
        .await
        .insert(worker.id, TrackedSession::new_for_test(worker.clone()));
    manager.request_drain_restart();
    manager
        .interrupt_restart_sessions(vec![worker.id])
        .await
        .expect("the drain interrupts the worker mid-turn");

    drop(manager);
    let store =
        Store::open_test_fixture(&dir.path().join("rsi.db")).expect("reopen after the restart");
    let config = Config::from_env();
    let runtime_config = RuntimeConfig::from_config(&config);
    runtime_config
        .sandbox_min_free_gib
        .store(0, std::sync::atomic::Ordering::Relaxed);
    let restarted = std::sync::Arc::new(
        SessionManager::new(
            std::sync::Arc::new(EventBus::new(16)),
            store,
            false,
            dir.path().join("restarted.sock"),
            None,
            Vec::new(),
            runtime_config,
            dir.path().join("sandboxes"),
        )
        .expect("restarted manager"),
    );
    restarted.restore_sessions().await.expect("startup restore");
    let executor = restarted.topology_executor().await;
    let process = install_controller_candidate_test_process(worker.id);

    // Before readiness: only the claim runs.
    let claimed = restarted
        .claim_restart_intents_at_startup()
        .await
        .expect("startup claim");
    assert_eq!(claimed.len(), 1, "the worker's intent awaits continuation");
    assert_eq!(claimed[0].1, worker.id);
    assert_eq!(
        process.productive_start_count.load(Ordering::SeqCst),
        0,
        "the claim starts no provider turn"
    );
    let state = |restarted: &SessionManager| {
        let id = worker.id.to_string();
        let restarted = restarted.store.clone();
        async move {
            restarted
                .lock()
                .await
                .conn
                .query_row(
                    "SELECT state FROM daemon_restart_intents WHERE session_id=?1",
                    [id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap()
        }
    };
    assert_eq!(state(&restarted).await, "claimed");
    let held = executor
        .effects
        .session(worker.id)
        .await
        .expect("observable");
    assert_eq!(held.status, SessionStatus::Interrupted);
    assert!(
        held.restart_intent_pending,
        "a claimed, undispatched intent still owns the session"
    );

    // After readiness: the deferred continuation completes.
    restarted.continue_claimed_restart_intents(claimed).await;
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    assert_eq!(state(&restarted).await, "delivered");
    let resumed = executor
        .effects
        .session(worker.id)
        .await
        .expect("observable");
    assert!(
        matches!(
            resumed.status,
            SessionStatus::Starting | SessionStatus::Running
        ),
        "the continued turn is live, got {:?}",
        resumed.status
    );
    assert!(!resumed.restart_intent_pending);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn restart_journal_retires_nodes_of_terminal_executions_before_claim_and_dispatch() {
    for status in ["failed", "succeeded", "cancelled"] {
        for settle_after_claim in [false, true] {
            let (manager, _dir) = manager();
            let repo = git_repo();
            let worker = running_worker(&manager, &repo).await;
            let execution = Uuid::new_v4();
            {
                let store = manager.store.lock().await;
                let boot = Uuid::new_v4();
                assert!(store.record_restart_intent(worker.id, boot).unwrap());
                store.mark_restart_interrupt_sent(worker.id, boot).unwrap();
                // A session node still in flight when a sibling's failure
                // settles the execution. Mirror the production row writers.
                store
                    .conn
                    .execute(
                        "INSERT INTO topology_executions (id,topology_name_snapshot,workflow_id, \
                     definition_json,definition_digest,requested_by_kind,repo_root,base_commit, \
                     custody_plan_json,status,created_at,updated_at) \
                     VALUES (?1,'restart-test',?2,'{}','d','operator',?3,'0','{}',?4,?5,?5)",
                        rusqlite::params![
                            execution.to_string(),
                            Uuid::new_v4().to_string(),
                            repo.path().to_str().unwrap(),
                            if settle_after_claim {
                                "running"
                            } else {
                                status
                            },
                            crate::topology::store::now_text()
                        ],
                    )
                    .unwrap();
                let attempt = Uuid::new_v4();
                store.conn.execute(
                    "INSERT INTO topology_node_attempts (id,execution_id,node_id,iteration,attempt_no, \
                     node_kind,status,dedup_key,session_id,base_commit,input_json,created_at,updated_at) \
                     VALUES (?1,?2,'A',0,1,'session','running',?1,?3,'0','{}',?4,?4)",
                    rusqlite::params![attempt.to_string(), execution.to_string(), worker.id.to_string(), crate::topology::store::now_text()],
                ).unwrap();
            }
            manager.restore_sessions().await.unwrap();
            let process = install_controller_candidate_test_process(worker.id);
            let claimed = manager.claim_restart_intents_at_startup().await.unwrap();
            assert_eq!(claimed.len(), usize::from(settle_after_claim));
            if settle_after_claim {
                manager
                    .store
                    .lock()
                    .await
                    .conn
                    .execute(
                        "UPDATE topology_executions SET status=?2 WHERE id=?1",
                        rusqlite::params![execution.to_string(), status],
                    )
                    .unwrap();
            }
            manager.continue_claimed_restart_intents(claimed).await;
            assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
            let store = manager.store.lock().await;
            let state: (String, String) = store
                .conn
                .query_row(
                    "SELECT state,outcome FROM daemon_restart_intents WHERE session_id=?1",
                    [worker.id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(state, ("failed".into(), "not_needed".into()));
            assert_eq!(
                store.get_session(worker.id).unwrap().unwrap().status,
                SessionStatus::Interrupted
            );
        }
    }
}

/// #2617: the cut turn ends in memory before its terminal status reaches the
/// store (the persistence worker lags under a drain). The executor must not
/// see that terminal view next to a row that is still `Running`: it carries no
/// restart journal ownership, so the node was settled `preserved_work` and the
/// resumed turn's handoff arrived at an already blocked node. Until the store
/// catches up the node reads as running; once it does, the journal owns it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn a_cut_node_whose_terminal_status_is_unpersisted_reads_as_running_2617() {
    use crate::topology::executor::NodeEffects;
    let (manager, _dir) = manager();
    let repo = git_repo();
    let worker = running_worker(&manager, &repo).await;
    manager
        .active
        .write()
        .await
        .insert(worker.id, TrackedSession::new_for_test(worker.clone()));
    manager.request_drain_restart();
    manager
        .interrupt_restart_sessions(vec![worker.id])
        .await
        .expect("the drain interrupts the worker mid-turn");
    // The monitor has ended the turn in memory; the row still says Running.
    manager
        .active
        .write()
        .await
        .get_mut(&worker.id)
        .expect("tracked")
        .session
        .status = SessionStatus::Interrupted;
    assert_eq!(
        manager
            .store
            .lock()
            .await
            .get_session(worker.id)
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Running
    );

    let manager = Arc::new(manager);
    let executor = manager.topology_executor().await;
    let before = executor
        .effects
        .session(worker.id)
        .await
        .expect("the node session is observable");
    assert_eq!(
        before.status,
        SessionStatus::Running,
        "an unpersisted terminal status must not settle the node"
    );

    // The store catches up: the journal owns the cut session.
    manager
        .store
        .lock()
        .await
        .update_session_status(worker.id, SessionStatus::Interrupted)
        .unwrap();
    manager.active.write().await.remove(&worker.id);
    let after = executor
        .effects
        .session(worker.id)
        .await
        .expect("the node session is observable");
    assert_eq!(after.status, SessionStatus::Interrupted);
    assert!(
        after.restart_intent_pending,
        "the restart journal owns the cut session, so the executor holds it live"
    );
}

#[cfg(target_os = "linux")]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn deploy_shutdown_preserves_live_detached_turn_and_reclaim_refuses_duplication() {
    use crate::store::provider_turn_custody::{NewProviderTurnCustody, ProviderTurnCustodyState};
    let (manager, _dir) = manager();
    let repo = git_repo();
    let parent = bare_session(Uuid::new_v4());
    insert_row(&manager, &parent).await;
    let worker = running_worker(&manager, &repo).await;
    manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET parent_id=?2 WHERE id=?1",
            rusqlite::params![worker.id.to_string(), parent.id.to_string()],
        )
        .unwrap();
    assert_eq!(
        manager
            .store
            .lock()
            .await
            .deploy_quiet_blockers_with(parent.id, true)
            .unwrap(),
        vec!["worker_mid_turn"]
    );
    let invocation = manager
        .store
        .lock()
        .await
        .session_model_invocation_id(worker.id)
        .unwrap()
        .unwrap();
    let spool = repo.path().join("spool");
    std::fs::create_dir(&spool).unwrap();
    let lock = std::fs::File::create(spool.join("alive.lock")).unwrap();
    lock.lock().unwrap();
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .env("RSI_SESSION_ID", worker.id.to_string())
        .env("RSI_MODEL_INVOCATION_ID", invocation.to_string())
        .env(
            "RSI_PROCESS_OWNERSHIP_NAMESPACE",
            rsi_common::identity::process_ownership_namespace(),
        )
        .spawn()
        .unwrap();
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", child.id())).unwrap();
    let start_time = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    manager
        .store
        .lock()
        .await
        .insert_provider_turn_custody(&NewProviderTurnCustody {
            worker_scope: None,
            invocation_id: invocation,
            session_id: worker.id,
            spool_dir: spool,
            pid: child.id(),
            start_time: Some(start_time),
            boot_id: manager.program_run_boot_id,
        })
        .unwrap();
    let interrupts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let kills = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut tracked = TrackedSession::new_for_test(worker.clone());
    tracked.process = Some(crate::session::types::ProviderProcess::Scripted(
        crate::session::types::ScriptedProcess {
            alive: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            exit_code: Arc::new(std::sync::atomic::AtomicI32::new(0)),
            interrupt_count: interrupts.clone(),
            kill_count: kills.clone(),
            exit_on_interrupt: true,
            interrupt_fails: false,
            kill_fails: false,
        },
    ));
    manager.active.write().await.insert(worker.id, tracked);
    manager.request_drain_restart();
    manager
        .interrupt_restart_sessions(vec![worker.id])
        .await
        .unwrap();
    assert_eq!(interrupts.load(Ordering::SeqCst), 0);
    let store = manager.store.lock().await;
    let refused = crate::session::reaper::reap_orphans_for_session(
        worker.id,
        store.list_active_provider_turn_custody().unwrap(),
    )
    .unwrap_err();
    assert!(refused.to_string().contains("live_detached_turn"));
    assert_eq!(
        store.deploy_quiet_blockers_with(parent.id, true).unwrap(),
        Vec::<&str>::new()
    );
    assert!(store.deploy_mid_turn_workers(parent.id).unwrap().is_empty());
    drop(store);
    manager
        .shutdown_drain(std::time::Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(interrupts.load(Ordering::SeqCst), 0);
    assert_eq!(kills.load(Ordering::SeqCst), 0);
    assert!(child.try_wait().unwrap().is_none());
    let store = manager.store.lock().await;
    let row = store
        .get_provider_turn_custody(invocation)
        .unwrap()
        .unwrap();
    assert_eq!(row.state, ProviderTurnCustodyState::Live);
    assert_eq!(row.stdout_offset, 0);
    assert_eq!(
        store.get_session(worker.id).unwrap().unwrap().status,
        SessionStatus::Running
    );
    let intents: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM daemon_restart_intents WHERE session_id=?1",
            [worker.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(intents, 0);
    child.kill().unwrap();
    child.wait().unwrap();
}

/// Hub #2987: a manager's terminal watch on a worker must not be spent by a
/// deploy interruption the restart journal will continue (#1461). It stays
/// armed through the interruption and the continuation, then fires once on the
/// continuation's real end. An interruption the journal gave up on fires at once.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn a_deploy_interrupted_worker_that_will_be_resumed_does_not_spend_the_managers_watch_2987() {
    let (manager, _dir) = manager();
    let repo = git_repo();
    let worker = running_worker(&manager, &repo).await;
    let master = Uuid::new_v4();
    insert_row(&manager, &bare_session(master)).await;
    push_event(
        &manager,
        master,
        1,
        EventType::Message,
        Some(Role::Assistant),
        chrono::Utc::now() - chrono::Duration::seconds(5),
    )
    .await;
    let job = mk_watch_job_for(worker.id, master, "");
    manager
        .store
        .lock()
        .await
        .insert_scheduled_job(&job)
        .unwrap();
    manager
        .active
        .write()
        .await
        .insert(worker.id, TrackedSession::new_for_test(worker.clone()));
    manager
        .interrupt_restart_sessions(vec![worker.id])
        .await
        .expect("the drain interrupts the worker mid-turn");
    // The monitor records the cut turn as Interrupted; the journal still owns it.
    let set_status = |status: &str| {
        let status = status.to_string();
        let id = worker.id.to_string();
        let manager = &manager;
        async move {
            manager
                .store
                .lock()
                .await
                .conn
                .execute(
                    "UPDATE sessions SET status=?2 WHERE id=?1",
                    rusqlite::params![id, status],
                )
                .unwrap();
        }
    };
    set_status("Interrupted").await;
    assert_eq!(
        manager.plan_terminal_watch_fire(&job).await.unwrap(),
        super::WatchFirePlan::NotReady,
        "an interruption the restart journal will continue is not the worker's end"
    );

    // The next process continues the worker: Running, then a later turn Completed.
    set_status("Running").await;
    assert_eq!(
        manager.plan_terminal_watch_fire(&job).await.unwrap(),
        super::WatchFirePlan::NotReady
    );
    {
        let store = manager.store.lock().await;
        let continuation = Uuid::new_v4();
        let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "INSERT INTO model_invocations
                 (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                  trigger_source,session_id,created_at)
                 VALUES(?1,'session.continue.resume','session_lifecycle','foreground','paid_capable',
                        'admitted','completed','daemon_restart',?2,?3)",
                rusqlite::params![continuation.to_string(), worker.id.to_string(), at],
            )
            .unwrap();
        store
            .set_session_model_invocation(worker.id, Some(continuation))
            .unwrap();
    }
    set_status("Completed").await;
    match manager.plan_terminal_watch_fire(&job).await.unwrap() {
        super::WatchFirePlan::Deliver {
            job_ids, message, ..
        } => {
            assert_eq!(job_ids, vec![job.id], "the watch fires exactly once");
            assert!(message.contains("Completed"));
        }
        other => panic!("expected Deliver on the continuation's Completed turn, got {other:?}"),
    }
}

/// Hub #2987 (other half): an interruption the journal does not own, or gave up
/// on, still fires the watch.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn an_interruption_the_restart_journal_will_not_resume_still_fires_the_watch_2987() {
    let (manager, _dir) = manager();
    let repo = git_repo();
    let master = Uuid::new_v4();
    insert_row(&manager, &bare_session(master)).await;
    push_event(
        &manager,
        master,
        1,
        EventType::Message,
        Some(Role::Assistant),
        chrono::Utc::now() - chrono::Duration::seconds(5),
    )
    .await;

    // No intent at all: an ordinary interruption.
    let plain = running_worker(&manager, &repo).await;
    manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET status='Interrupted' WHERE id=?1",
            [plain.id.to_string()],
        )
        .unwrap();
    let plain_job = mk_watch_job_for(plain.id, master, "");
    manager
        .store
        .lock()
        .await
        .insert_scheduled_job(&plain_job)
        .unwrap();
    assert!(matches!(
        manager.plan_terminal_watch_fire(&plain_job).await.unwrap(),
        super::WatchFirePlan::Deliver { .. }
    ));

    // An intent the journal gave up on (`not_resumable`).
    let refused = running_worker(&manager, &repo).await;
    manager
        .active
        .write()
        .await
        .insert(refused.id, TrackedSession::new_for_test(refused.clone()));
    manager
        .interrupt_restart_sessions(vec![refused.id])
        .await
        .unwrap();
    {
        let store = manager.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Interrupted' WHERE id=?1",
                [refused.id.to_string()],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE daemon_restart_intents SET state='failed',outcome='not_resumable'
                 WHERE session_id=?1",
                [refused.id.to_string()],
            )
            .unwrap();
    }
    let refused_job = mk_watch_job_for(refused.id, master, "");
    manager
        .store
        .lock()
        .await
        .insert_scheduled_job(&refused_job)
        .unwrap();
    assert!(matches!(
        manager
            .plan_terminal_watch_fire(&refused_job)
            .await
            .unwrap(),
        super::WatchFirePlan::Deliver { .. }
    ));
}
