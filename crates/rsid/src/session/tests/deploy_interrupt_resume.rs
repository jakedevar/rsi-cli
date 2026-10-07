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
