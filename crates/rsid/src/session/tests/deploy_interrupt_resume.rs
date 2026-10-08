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
    let store = Store::open(&dir.path().join("rsi.db")).expect("reopen after the restart");
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
    let store = Store::open(&dir.path().join("rsi.db")).expect("reopen after the restart");
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
