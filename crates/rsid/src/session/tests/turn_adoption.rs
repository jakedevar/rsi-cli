//! Real detached Claude handles through deploy shutdown and startup restore.
use super::*;
use crate::claude::{ClaudeProcess, LaunchConfig, turn_spool::DetachedTurn};
use crate::session::types::ProviderProcess;
use crate::store::provider_turn_custody::ProviderTurnCustodyState;

fn reopened(dir: &std::path::Path) -> SessionManager {
    SessionManager::new(
        Arc::new(EventBus::new(32)),
        Store::open(&dir.join("rsi.db")).unwrap(),
        false,
        dir.join("daemon.sock"),
        None,
        Vec::new(),
        RuntimeConfig::from_config(&Config::from_env()),
        dir.join("sandboxes"),
    )
    .unwrap()
}

async fn until(mut predicate: impl AsyncFnMut() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        while !predicate().await {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fixture reached expected state");
}

struct FixtureGuard {
    pid: u32,
    start: Option<u64>,
    dir: std::path::PathBuf,
}
impl Drop for FixtureGuard {
    fn drop(&mut self) {
        if !crate::claude::turn_spool::lock_held(&self.dir).unwrap_or(false) {
            return;
        }
        #[cfg(target_os = "linux")]
        {
            let start = std::fs::read_to_string(format!("/proc/{}/stat", self.pid))
                .ok()
                .and_then(|s| {
                    s.rsplit_once(") ")
                        .and_then(|(_, f)| f.split_whitespace().nth(19))
                        .and_then(|t| t.parse::<u64>().ok())
                });
            if self.start.is_none() || start != self.start {
                return;
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = self.start;
        // This fixture started this exact group, including its provider.
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

async fn seed(
    manager: &SessionManager,
    dir: &std::path::Path,
    register: bool,
    wrong_identity: bool,
) -> (
    Session,
    Uuid,
    DetachedTurn,
    tokio::process::Child,
    FixtureGuard,
) {
    let mut session = bare_session(Uuid::new_v4());
    session.status = SessionStatus::Running;
    session.provider = SessionProvider::Claude;
    session.working_dir = dir.to_path_buf();
    session.work_time_ms = Some(1234);
    session.approval_wait_ms = Some(56);
    let invocation = Uuid::new_v4();
    let spool = dir.join(invocation.to_string());
    std::fs::create_dir(&spool).unwrap();
    // A shim fixture with a genuine provider subprocess, lock, identity and
    // atomic exit. Files control output, so the handoff cannot race turn end.
    let child = tokio::process::Command::new("python3")
        .arg("-c")
        .arg(
            r#"
import fcntl, json, os, subprocess, sys
d = sys.argv[1]
with open(d + '/alive.lock', 'w') as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    with open(d + '/shim.json', 'w') as f:
        json.dump({'pid': os.getpid()}, f)
    with open(d + '/stdout', 'ab') as out, open(d + '/stderr', 'ab') as err:
        p = subprocess.Popen(['python3', '-c', '''
import json, os, sys, time
d = sys.argv[1]
def emit(value):
    print(json.dumps(value), flush=True)
emit({'type':'assistant','message':{'content':[{'type':'text','text':'before restart'}]}})
while not os.path.exists(d + '/continue'): time.sleep(.01)
emit({'type':'assistant','message':{'content':[{'type':'text','text':'after restart'}]}})
while not os.path.exists(d + '/finish'): time.sleep(.01)
emit({'type':'result','subtype':'success','result':'done','is_error':False,'num_turns':1})
''', d], stdout=out, stderr=err)
        status = p.wait()
    with open(d + '/.exit.tmp', 'w') as f:
        json.dump({'exit_code':status,'signal':None,'error':None}, f)
    os.replace(d + '/.exit.tmp', d + '/exit.json')
"#,
        )
        .arg(&spool)
        .env("RSI_SESSION_ID", session.id.to_string())
        .env("RSI_MODEL_INVOCATION_ID", invocation.to_string())
        .env(
            "RSI_PROCESS_OWNERSHIP_NAMESPACE",
            rsi_common::identity::process_ownership_namespace(),
        )
        .process_group(0)
        .kill_on_drop(false)
        .spawn()
        .unwrap();
    until(async || spool.join("stdout").metadata().is_ok_and(|m| m.len() > 0)).await;
    let turn = DetachedTurn::new(invocation, spool);
    let mut row = turn
        .custody(session.id, manager.program_run_boot_id)
        .await
        .unwrap();
    let cleanup = FixtureGuard {
        pid: row.pid,
        start: row.start_time,
        dir: row.spool_dir.clone(),
    };
    if wrong_identity {
        row.start_time = row.start_time.map(|start| start + 1);
    }
    {
        let store = manager.store.lock().await;
        store.insert_session(&session).unwrap();
        // A real admission reserves the resources this turn will use.
        store.conn.execute("INSERT INTO model_invocations
            (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,session_id,created_at,reserved_input_tokens,reserved_output_tokens,reserved_wall_time_ms)
            VALUES(?1,'session.launch.fresh','session_lifecycle','foreground','paid_capable','admitted','running','launch_session',?2,?3,100000,100000,60000)",
            rusqlite::params![invocation.to_string(), session.id.to_string(), chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)]).unwrap();
        store
            .set_session_model_invocation(session.id, Some(invocation))
            .unwrap();
        if register {
            store.insert_provider_turn_custody(&row).unwrap();
        }
    }
    (session, invocation, turn, child, cleanup)
}

fn monitor(
    manager: &SessionManager,
    id: Uuid,
    stop_rx: mpsc::Receiver<()>,
    rx: mpsc::Receiver<StreamEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(SessionManager::monitor_session(
        id,
        1,
        Box::new(crate::provider::CliProviderSession::new(rx)),
        manager.active.clone(),
        manager.completed.clone(),
        manager.event_bus.clone(),
        stop_rx,
        manager.store.clone(),
        manager.model_call_settlements.handle().unwrap(),
        manager.persistence.clone(),
        0,
        false,
        manager.socket_path.clone(),
        manager.token_counter.clone(),
        None,
        manager.retry_tx.clone(),
        manager.tool_registry.clone(),
        crate::turn_controller::TurnController::new(
            crate::turn_controller::ContinuationPolicy::Single,
        ),
        manager.runtime_config.clone(),
        manager.spawn_coordinator.clone(),
        manager.agent_tokens.clone(),
        manager.spawn_epoch.clone(),
        manager.agent_message_arbiter.clone(),
        manager.codegraph_handle.clone(),
        manager.custody_execution_runtime(),
    ))
}

async fn assert_running_invocation(manager: &SessionManager, invocation: Uuid, stage: &str) {
    let record = manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT status,error_class FROM model_invocations WHERE id=?1",
            [invocation.to_string()],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .unwrap();
    assert_eq!(record, ("running".into(), None), "{stage}");
}

#[cfg(target_os = "linux")]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn deploy_shutdown_adopts_real_detached_turn_twice_and_finishes_once() {
    let (old, dir) = manager();
    let (session, invocation, turn, child, _cleanup) = seed(&old, dir.path(), false, false).await;
    let (tx, rx) = mpsc::channel(100);
    turn.spawn_reader(tx);
    let (stop_tx, stop_rx) = mpsc::channel(1);
    let mut tracked = TrackedSession::restored(session.clone(), stop_tx);
    tracked.spawn_generation = 1;
    tracked.process = Some(ProviderProcess::Claude(ClaudeProcess::detached_for_test(
        child,
        turn.clone(),
    )));
    old.active.write().await.insert(session.id, tracked);
    let task = monitor(&old, session.id, stop_rx, rx);
    until(async || {
        old.store
            .lock()
            .await
            .get_provider_turn_custody(invocation)
            .unwrap()
            .is_some_and(|row| row.stdout_offset > 0)
    })
    .await;
    old.request_drain_restart();
    old.shutdown().await.unwrap();
    task.await.unwrap();
    assert!(turn.is_handed_off());
    assert_eq!(
        old.active.read().await[&session.id].session.status,
        SessionStatus::Running
    );
    assert_eq!(
        old.store
            .lock()
            .await
            .get_session(session.id)
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Running
    );
    assert!(
        old.store
            .lock()
            .await
            .get_provider_turn_custody(invocation)
            .unwrap()
            .unwrap()
            .is_adoptable()
            .unwrap()
    );
    assert_running_invocation(&old, invocation, "old shutdown").await;
    drop(old);

    let next = reopened(dir.path());
    next.restore_sessions().await.unwrap();
    assert_running_invocation(&next, invocation, "first restore").await;
    assert!(next.active.read().await.contains_key(&session.id));
    let claimed = next
        .store
        .lock()
        .await
        .get_provider_turn_custody(invocation)
        .unwrap()
        .unwrap();
    assert_eq!(claimed.boot_id, next.program_run_boot_id);
    assert_eq!(claimed.state, ProviderTurnCustodyState::Adopted);
    assert!(next.active.read().await[&session.id].work_time_base_ms >= 1234);
    std::fs::write(claimed.spool_dir.join("continue"), "").unwrap();
    until(async || {
        next.store
            .lock()
            .await
            .load_events(session.id)
            .unwrap()
            .iter()
            .any(|e| e.content == "after restart")
    })
    .await;
    next.request_drain_restart();
    next.shutdown().await.unwrap();
    next.persistence.barrier().await.unwrap();
    assert_running_invocation(&next, invocation, "second shutdown").await;
    drop(next);

    let final_boot = reopened(dir.path());
    let mut notifications = final_boot.event_bus.subscribe();
    final_boot.restore_sessions().await.unwrap();
    assert_running_invocation(&final_boot, invocation, "second restore").await;
    std::fs::write(claimed.spool_dir.join("finish"), "").unwrap();
    until(async || final_boot.completed.read().await.contains_key(&session.id)).await;
    final_boot.persistence.barrier().await.unwrap();
    let store = final_boot.store.lock().await;
    let persisted = store.get_session(session.id).unwrap().unwrap();
    assert_eq!(persisted.status, SessionStatus::Completed);
    let events = store.load_events(session.id).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.content == "before restart")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.content == "after restart")
            .count(),
        1
    );
    let mut completions = 0;
    while let Ok(event) = notifications.try_recv() {
        if matches!(&*event, DaemonEvent::SessionStatusChanged { session_id, new_status: SessionStatus::Completed, .. } if *session_id == session.id)
        {
            completions += 1;
        }
    }
    assert_eq!(completions, 1);
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT count(*) FROM daemon_restart_intents WHERE session_id=?1",
                [session.id.to_string()],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    let settled = store
        .get_provider_turn_custody(invocation)
        .unwrap()
        .unwrap();
    assert_eq!(settled.state, ProviderTurnCustodyState::Finished);
    assert_eq!(
        settled.stdout_offset,
        std::fs::metadata(settled.spool_dir.join("stdout"))
            .unwrap()
            .len()
    );
    assert!(
        !store
            .advance_provider_turn_stdout_offset(
                invocation,
                claimed.boot_id,
                claimed.stdout_offset,
                claimed.stdout_offset + 1
            )
            .unwrap()
    );
    let record = store
        .conn
        .query_row(
            "SELECT status,error_class FROM model_invocations WHERE id=?1",
            [invocation.to_string()],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .unwrap();
    assert_eq!(record, ("completed".into(), None));
}

#[cfg(target_os = "linux")]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn turn_finished_in_restart_gap_replays_remaining_output() {
    let (old, dir) = manager();
    let (session, invocation, turn, mut child, _cleanup) =
        seed(&old, dir.path(), true, false).await;
    std::fs::write(turn.spool_dir.join("continue"), "").unwrap();
    std::fs::write(turn.spool_dir.join("finish"), "").unwrap();
    assert!(child.wait().await.unwrap().success());
    drop(old);
    let next = reopened(dir.path());
    next.restore_sessions().await.unwrap();
    until(async || next.completed.read().await.contains_key(&session.id)).await;
    next.persistence.barrier().await.unwrap();
    let store = next.store.lock().await;
    assert_eq!(
        store.get_session(session.id).unwrap().unwrap().status,
        SessionStatus::Completed
    );
    assert_eq!(
        store
            .get_provider_turn_custody(invocation)
            .unwrap()
            .unwrap()
            .state,
        ProviderTurnCustodyState::Finished
    );
    assert_eq!(
        store.get_session(session.id).unwrap().unwrap().num_turns,
        Some(1)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn prior_boot_live_turn_is_fenced_before_continuation_or_retry() {
    let (old, dir) = manager();
    let (session, invocation, turn, mut child, _cleanup) =
        seed(&old, dir.path(), true, false).await;
    let next = reopened(dir.path());
    assert!(
        next.fence_prior_detached_turn(session.id)
            .await
            .unwrap_err()
            .to_string()
            .contains("live_detached_turn")
    );
    let error = next
        .continue_session(session.id, "retry".into())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("live_detached_turn"), "{error}");
    assert_eq!(
        next.store
            .lock()
            .await
            .get_provider_turn_custody(invocation)
            .unwrap()
            .unwrap()
            .boot_id,
        old.program_run_boot_id
    );
    next.claim_detached_turns_on_startup().await.unwrap();
    assert!(
        next.fence_prior_detached_turn(session.id)
            .await
            .unwrap_err()
            .to_string()
            .contains("live_detached_turn")
    );
    std::fs::write(turn.spool_dir.join("continue"), "").unwrap();
    std::fs::write(turn.spool_dir.join("finish"), "").unwrap();
    assert!(child.wait().await.unwrap().success());
}

#[cfg(target_os = "linux")]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn startup_turn_failure_is_local_and_restart_intents_reconcile() {
    for failure in ["identity", "claim", "restore"] {
        let (old, dir) = manager();
        let (bad, bad_invocation, bad_turn, mut bad_child, _bad_cleanup) =
            seed(&old, dir.path(), true, failure == "identity").await;
        let (healthy, invocation, turn, mut child, _cleanup) =
            seed(&old, dir.path(), true, false).await;
        let mut restart = bare_session(Uuid::new_v4());
        restart.status = SessionStatus::Running;
        restart.provider = SessionProvider::Codex;
        restart.working_dir = dir.path().to_path_buf();
        {
            let store = old.store.lock().await;
            store.insert_session(&restart).unwrap();
            let restart_invocation = Uuid::new_v4();
            store.conn.execute("INSERT INTO model_invocations
                (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,session_id,created_at)
                VALUES(?1,'session.launch.fresh','session_lifecycle','foreground','paid_capable','admitted','running','launch_session',?2,?3)",
                rusqlite::params![restart_invocation.to_string(), restart.id.to_string(), chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)]).unwrap();
            store
                .set_session_model_invocation(restart.id, Some(restart_invocation))
                .unwrap();
            assert!(
                store
                    .record_restart_intent(restart.id, old.program_run_boot_id)
                    .unwrap()
            );
            store
                .mark_restart_interrupt_sent(restart.id, old.program_run_boot_id)
                .unwrap();
            if failure == "claim" {
                // sql-dynamic-ok: the interpolated value is a fixture-generated UUID.
                store.conn.execute_batch(&format!("CREATE TRIGGER refuse_fixture_claim BEFORE UPDATE OF boot_id ON provider_turn_custody
                    WHEN OLD.invocation_id='{}' BEGIN SELECT RAISE(ABORT,'fixture claim failure'); END", bad_invocation)).unwrap(); // sql-dynamic-ok: fixture UUID only
            } else if failure == "restore" {
                // A valid custody claim with an unreadable persisted transcript.
                store.conn.execute("INSERT INTO conversation_events(session_id,sequence,event_type,content,created_at)
                    VALUES(?1,1,'message','fixture','invalid timestamp')", [bad.id.to_string()]).unwrap();
            }
        }
        let prior_boot = old.program_run_boot_id;
        drop(old);
        let next = reopened(dir.path());
        next.restore_sessions().await.unwrap();
        next.reconcile_restart_intents_at_startup().await.unwrap();
        next.persistence.barrier().await.unwrap();
        assert!(
            next.active.read().await.contains_key(&healthy.id),
            "{failure}"
        );
        assert_running_invocation(&next, invocation, failure).await;
        {
            let store = next.store.lock().await;
            assert_eq!(
                store.get_session(bad.id).unwrap().unwrap().status,
                SessionStatus::Failed,
                "{failure}"
            );
            let claimed = store
                .get_provider_turn_custody(invocation)
                .unwrap()
                .unwrap();
            assert_eq!(claimed.boot_id, next.program_run_boot_id);
            assert_eq!(claimed.state, ProviderTurnCustodyState::Adopted);
            let rejected = store
                .get_provider_turn_custody(bad_invocation)
                .unwrap()
                .unwrap();
            if failure == "restore" {
                assert_eq!(rejected.state, ProviderTurnCustodyState::Abandoned);
            } else {
                assert_eq!(rejected.boot_id, prior_boot);
                assert_eq!(rejected.state, ProviderTurnCustodyState::Live);
            }
            assert_eq!(
                store
                    .get_session(restart.id)
                    .unwrap()
                    .unwrap()
                    .stop_reason
                    .as_deref(),
                Some("daemon_restart_resume:not_resumable")
            );
        }
        if failure != "restore" {
            assert!(
                next.fence_prior_detached_turn(bad.id)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("live_detached_turn")
            );
        }
        for spool in [&bad_turn.spool_dir, &turn.spool_dir] {
            std::fs::write(spool.join("continue"), "").unwrap();
            std::fs::write(spool.join("finish"), "").unwrap();
        }
        assert!(bad_child.wait().await.unwrap().success());
        assert!(child.wait().await.unwrap().success());
        until(async || next.completed.read().await.contains_key(&healthy.id)).await;
        next.persistence.barrier().await.unwrap();
        assert_eq!(
            next.store
                .lock()
                .await
                .get_provider_turn_custody(invocation)
                .unwrap()
                .unwrap()
                .state,
            ProviderTurnCustodyState::Finished
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn dead_shim_without_exit_uses_crash_recovery_and_frees_custody() {
    let (old, dir) = manager();
    let (session, invocation, _turn, mut child, cleanup) =
        seed(&old, dir.path(), true, false).await;
    nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(cleanup.pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    child.wait().await.unwrap();
    drop(old);
    let next = reopened(dir.path());
    next.restore_sessions().await.unwrap();
    let store = next.store.lock().await;
    assert_eq!(
        store.get_session(session.id).unwrap().unwrap().status,
        SessionStatus::Failed
    );
    assert_eq!(
        store
            .get_provider_turn_custody(invocation)
            .unwrap()
            .unwrap()
            .state,
        ProviderTurnCustodyState::Abandoned
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn consumed_result_survives_restart_without_replaying_stdout() {
    let (old, dir) = manager();
    let (session, invocation, turn, mut child, _cleanup) =
        seed(&old, dir.path(), true, false).await;
    std::fs::write(turn.spool_dir.join("continue"), "").unwrap();
    std::fs::write(turn.spool_dir.join("finish"), "").unwrap();
    child.wait().await.unwrap();
    let offset = std::fs::metadata(turn.spool_dir.join("stdout"))
        .unwrap()
        .len();
    assert!(
        old.store
            .lock()
            .await
            .advance_provider_turn_stdout_offset(invocation, old.program_run_boot_id, 0, offset)
            .unwrap()
    );
    drop(old);
    let next = reopened(dir.path());
    next.restore_sessions().await.unwrap();
    until(async || next.completed.read().await.contains_key(&session.id)).await;
    next.persistence.barrier().await.unwrap();
    let store = next.store.lock().await;
    assert_eq!(
        store.get_session(session.id).unwrap().unwrap().status,
        SessionStatus::Completed
    );
    assert_eq!(
        store
            .get_provider_turn_custody(invocation)
            .unwrap()
            .unwrap()
            .stdout_offset,
        offset
    );
    assert_eq!(store.load_events(session.id).unwrap().len(), 0);
}

/// Same fixture transport as the Claude slice: real child, independent stdio,
/// held lock and durable completion. Also exercises Codex's --stdin-file argv.
#[cfg(unix)]
fn install_cli_turn_fixture(
    dir: &std::path::Path,
    provider: SessionProvider,
) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let shim = dir.join("turn-shim-fixture");
    std::fs::write(&shim, r#"#!/usr/bin/env python3
import fcntl, json, os, subprocess, sys
args = sys.argv[1:]
d = args[args.index('--spool-dir') + 1]
stdin = open(args[args.index('--stdin-file') + 1], 'rb') if '--stdin-file' in args else subprocess.DEVNULL
with open(d + '/alive.lock', 'w') as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    with open(d + '/shim.json', 'w') as f: json.dump({'pid':os.getpid()}, f)
    with open(d + '/stdout', 'ab') as out, open(d + '/stderr', 'ab') as err:
        p = subprocess.Popen(args[args.index('--') + 1:], stdin=stdin, stdout=out, stderr=err)
        status = p.wait()
    with open(d + '/.exit.tmp', 'w') as f: json.dump({'exit_code':status,'signal':None,'error':None}, f)
    os.replace(d + '/.exit.tmp', d + '/exit.json')
"#).unwrap();
    let binary = dir.join("provider-fixture");
    let codex = provider == SessionProvider::Codex;
    let source = format!(
        r#"#!/usr/bin/env python3
import json, os, sys, time
codex = {codex}
with open('argv.json', 'w') as f: json.dump(sys.argv[1:], f)
if codex:
    with open('query.txt', 'w') as f: f.write(sys.stdin.read())
    print(json.dumps({{'type':'thread.started','thread_id':'fixture-thread'}}), flush=True)
    print(json.dumps({{'type':'turn.started'}}), flush=True)
def emit(text):
    if codex: print(json.dumps({{'type':'item.completed','item':{{'id':text,'type':'agent_message','text':text}}}}), flush=True)
    else: print(text, flush=True)
emit('before restart')
while not os.path.exists('continue'): time.sleep(.01)
emit('after restart')
while not os.path.exists('finish'): time.sleep(.01)
if codex: print(json.dumps({{'type':'turn.completed','usage':{{'input_tokens':12,'output_tokens':3}}}}), flush=True)
else: sys.stdout.write('final plain line'); sys.stdout.flush()
"#,
        codex = if codex { "True" } else { "False" }
    );
    std::fs::write(&binary, source).unwrap();
    for path in [&shim, &binary] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (binary, shim)
}

#[cfg(unix)]
async fn detached_cli_deploy_fixture(provider: SessionProvider) {
    use crate::model_control::{AdmissionPermit, registry::RuntimeExecutionRoute};
    let (old, dir) = manager();
    old.runtime_config
        .update_field("turn_detach_enabled", &serde_json::json!(true))
        .unwrap();
    let mut session = bare_session(Uuid::new_v4());
    session.provider = provider;
    session.status = SessionStatus::Running;
    session.working_dir = dir.path().to_path_buf();
    let route = if provider == SessionProvider::Codex {
        RuntimeExecutionRoute::CodexCli
    } else {
        RuntimeExecutionRoute::AntigravityCli
    };
    let invocation = Uuid::new_v4();
    let permit = AdmissionPermit::for_invocation_test(invocation);
    let execution = permit.claim_cli_execution(route).unwrap();
    let (binary, shim) = install_cli_turn_fixture(dir.path(), provider);
    let query = "literal $HOME and `prompt`\nsecond line";
    let config = LaunchConfig {
        completion_gates: None,
        query: query.into(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        working_dir: Some(dir.path().to_path_buf()),
        provider: Some(provider),
        model: None,
        configured_context_window: None,
        max_turns: None,
        system_prompt: None,
        resume_session_id: None,
        session_kind: None,
        project_id: None,
        rsi_session_id: Some(session.id),
        rsi_socket: Some(old.socket_path.clone()),
        rsi_session_token: None,
        continued_from: None,
        openai_base_url: None,
        openai_api_key: None,
        conversation_history: None,
        workflow_id: None,
        workflow_id_override: None,
        max_retries: None,
        group_id: None,
        parent_id: None,
        effort: None,
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
        scheduled_job_id: None,
        model_invocation_owner: None,
        model_invocation_dedup_key: None,
        model_invocation_request_fingerprint: None,
        skip_project_model_default: false,
        tool_policy: None,
        model_invocation_purpose:
            rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
        sandbox: None,
        cargo_target_dir: None,
        execution_scratch: None,
        is_eval: false,
        skip_context_pipeline: false,
        capability_class: None,
        tags: vec![],
        topology_node_id: None,
        topology_iteration: 0,
        closure_selector: None,
    };
    {
        let store = old.store.lock().await;
        store.insert_session(&session).unwrap();
        store.conn.execute("INSERT INTO model_invocations
            (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,session_id,created_at,reserved_input_tokens,reserved_output_tokens,reserved_wall_time_ms)
            VALUES(?1,'session.launch.fresh','session_lifecycle','foreground','paid_capable','admitted','running','launch_session',?2,?3,100000,100000,60000)",
            rusqlite::params![invocation.to_string(), session.id.to_string(), chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)]).unwrap();
        store
            .set_session_model_invocation(session.id, Some(invocation))
            .unwrap();
    }
    let (process, rx) = if provider == SessionProvider::Codex {
        crate::codex::CodexClient::with_paths_for_test(
            binary,
            "/bin/true".into(),
            old.runtime_config.clone(),
        )
        .launch_inner(&config, execution, Some(&shim))
        .unwrap()
    } else {
        crate::agy::AgyClient::with_path_for_test(binary, old.runtime_config.clone())
            .launch_inner(&config, execution, Some(&shim))
            .unwrap()
    };
    let turn = process
        .detached
        .clone()
        .expect("managed provider used shim");
    let row = turn
        .custody(session.id, old.program_run_boot_id)
        .await
        .unwrap();
    let _cleanup = FixtureGuard {
        pid: row.pid,
        start: row.start_time,
        dir: row.spool_dir.clone(),
    };
    let (stop_tx, stop_rx) = mpsc::channel(1);
    let mut tracked = TrackedSession::restored(session.clone(), stop_tx);
    tracked.spawn_generation = 1;
    tracked.process = Some(if provider == SessionProvider::Codex {
        ProviderProcess::Codex(process)
    } else {
        ProviderProcess::Antigravity(process)
    });
    old.active.write().await.insert(session.id, tracked);
    let task = monitor(&old, session.id, stop_rx, rx);
    until(async || {
        let ready = old
            .store
            .lock()
            .await
            .get_provider_turn_custody(invocation)
            .unwrap();
        ready.is_some_and(|r| {
            if provider == SessionProvider::Codex {
                r.stdout_offset > 0
            } else {
                std::fs::read_to_string(r.spool_dir.join("stdout"))
                    .is_ok_and(|s| s.contains("before restart"))
            }
        })
    })
    .await;
    if provider == SessionProvider::Codex {
        assert_eq!(
            std::fs::read_to_string(dir.path().join("query.txt")).unwrap(),
            format!("{}\n", crate::codex::codex_stdin_payload(&config, false))
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(turn.spool_dir.join("stdin"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    } else {
        let args: Vec<String> =
            serde_json::from_slice(&std::fs::read(dir.path().join("argv.json")).unwrap()).unwrap();
        assert_eq!(args.last().unwrap(), query);
    }
    old.request_drain_restart();
    old.shutdown().await.unwrap();
    task.await.unwrap();
    assert!(turn.is_handed_off());
    assert!(crate::claude::turn_spool::lock_held(&turn.spool_dir).unwrap());
    assert_running_invocation(&old, invocation, "deploy shutdown").await;
    drop(old);
    let next = reopened(dir.path());
    next.restore_sessions().await.unwrap();
    assert_running_invocation(&next, invocation, "startup adoption").await;
    assert!(
        next.active.read().await[&session.id]
            .process
            .as_ref()
            .unwrap()
            .detached_turn()
            .unwrap()
            .adopted
            .is_some()
    );
    let mut notifications = next.event_bus.subscribe();
    std::fs::write(dir.path().join("continue"), "").unwrap();
    if provider == SessionProvider::Codex {
        until(async || {
            next.store
                .lock()
                .await
                .load_events(session.id)
                .unwrap()
                .iter()
                .any(|e| e.content == "after restart")
        })
        .await;
        // A second deploy proves decoding thread state and cursor survive more
        // than one ownership transfer; the prior stdout never gets sent again.
        next.request_drain_restart();
        next.shutdown().await.unwrap();
        drop(next);
        let final_boot = reopened(dir.path());
        notifications = final_boot.event_bus.subscribe();
        final_boot.restore_sessions().await.unwrap();
        std::fs::write(dir.path().join("finish"), "").unwrap();
        until(async || final_boot.completed.read().await.contains_key(&session.id)).await;
        assert_cli_completed_once(
            &final_boot,
            session.id,
            invocation,
            provider,
            &mut notifications,
        )
        .await;
    } else {
        std::fs::write(dir.path().join("finish"), "").unwrap();
        until(async || next.completed.read().await.contains_key(&session.id)).await;
        assert_cli_completed_once(&next, session.id, invocation, provider, &mut notifications)
            .await;
    }
}

#[cfg(unix)]
async fn assert_cli_completed_once(
    manager: &SessionManager,
    id: Uuid,
    invocation: Uuid,
    provider: SessionProvider,
    notifications: &mut tokio::sync::broadcast::Receiver<Arc<DaemonEvent>>,
) {
    manager.persistence.barrier().await.unwrap();
    let store = manager.store.lock().await;
    assert_eq!(
        store.get_session(id).unwrap().unwrap().status,
        SessionStatus::Completed
    );
    let events = store.load_events(id).unwrap();
    if provider == SessionProvider::Codex {
        for text in ["before restart", "after restart"] {
            assert_eq!(events.iter().filter(|e| e.content == text).count(), 1);
        }
    } else {
        assert_eq!(
            events
                .iter()
                .filter(|e| e.content == "before restart\nafter restart\nfinal plain line")
                .count(),
            1
        );
    }
    let row = store
        .get_provider_turn_custody(invocation)
        .unwrap()
        .unwrap();
    assert_eq!(row.state, ProviderTurnCustodyState::Finished);
    assert_eq!(
        row.stdout_offset,
        std::fs::metadata(row.spool_dir.join("stdout"))
            .unwrap()
            .len()
    );
    let mut completions = 0;
    while let Ok(event) = notifications.try_recv() {
        if matches!(&*event, DaemonEvent::SessionStatusChanged { session_id, new_status: SessionStatus::Completed, .. } if *session_id == id)
        {
            completions += 1;
        }
    }
    assert_eq!(completions, 1);
    let status: String = store
        .conn
        .query_row(
            "SELECT status FROM model_invocations WHERE id=?1",
            [invocation.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "completed");
    let intents: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM daemon_restart_intents WHERE session_id=?1",
            [id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(intents, 0);
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn codex_cli_shim_deploy_shutdown_and_startup_adoption_complete_once() {
    detached_cli_deploy_fixture(SessionProvider::Codex).await;
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn antigravity_shim_deploy_shutdown_and_startup_adoption_complete_once() {
    detached_cli_deploy_fixture(SessionProvider::Antigravity).await;
}
