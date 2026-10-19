//! #2353: the operator DRAIN wait does not wait for a turn whose only open work
//! is a typed control call the daemon is serving, and still waits out its limit
//! for a turn running a real tool, a shell tool included (#2388). Time is the
//! paused tokio clock; no test waits 30 s.
use super::*;
use crate::session::OperatorDrainDecision;
use crate::session::daemon_wait::{DAEMON_WAIT_SETTLE, TurnToolLedger};
use std::time::Duration;

/// `OPERATOR_DRAIN_LIMIT` in `main.rs`.
const LIMIT: Duration = Duration::from_secs(30);
/// The main loop's `drain_request_tick`.
const TICK: Duration = Duration::from_millis(250);
const CONTROL: &str = "mcp__rsi-agent__rsi_control_get_issue";

/// A Claude session mid-turn whose ledger saw the turn from its start and has
/// the given `(tool_use_id, tool name)` calls open.
fn claude_turn(id: Uuid, open: &[(&str, &str)]) -> TrackedSession {
    let mut session = bare_session(id);
    session.status = SessionStatus::Running;
    session.provider = SessionProvider::Claude;
    let mut tracked = TrackedSession::new_for_test(session);
    tracked.turn_tools = TurnToolLedger::default();
    tracked.turn_tools.observe_stream(
        "assistant",
        &serde_json::json!({"message": {"content": open.iter()
            .map(|(call, name)| serde_json::json!(
                {"type": "tool_use", "id": call, "name": name, "input": {}}))
            .collect::<Vec<_>>()}}),
    );
    tracked
}

/// Poll the way the daemon's main loop does until the wait completes. Returns
/// the virtual time that had passed and the decision.
async fn poll_drain(manager: &SessionManager) -> (Duration, OperatorDrainDecision) {
    let started = tokio::time::Instant::now();
    loop {
        let elapsed = started.elapsed();
        match manager.operator_drain_decision(elapsed, LIMIT).await {
            OperatorDrainDecision::Wait => tokio::time::advance(TICK).await,
            decided => return (elapsed, decided),
        }
    }
}

async fn states(manager: &SessionManager) -> Vec<String> {
    manager.drain_restart_status().await["active_sessions"]
        .as_array()
        .expect("active sessions")
        .iter()
        .map(|entry| entry["state"].as_str().expect("state").to_owned())
        .collect()
}

/// Acceptance 1: two sessions blocked on daemon waits end the wait within 5 s
/// (it was the full 30 s).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test(start_paused = true)]
async fn two_sessions_blocked_on_daemon_calls_end_the_drain_wait_within_five_seconds() {
    let (manager, _dir) = manager();
    let mut calls = Vec::new();
    for _ in 0..2 {
        let id = Uuid::new_v4();
        manager
            .active
            .write()
            .await
            .insert(id, claude_turn(id, &[("call-1", CONTROL)]));
        calls.push(manager.daemon_wait.begin(id));
    }
    manager.request_drain_restart();

    let (elapsed, decision) = poll_drain(&manager).await;
    assert_eq!(
        decision,
        OperatorDrainDecision::Complete {
            active_count: 2,
            daemon_waiting: 2,
            deploy_interrupted: 0,
            limit_reached: false,
        }
    );
    assert!(
        elapsed <= Duration::from_secs(5),
        "the wait ended after {elapsed:?}, not within 5 s"
    );
    assert!(elapsed >= DAEMON_WAIT_SETTLE, "a call must first settle");
    assert_eq!(states(&manager).await, vec!["waiting_on_daemon"; 2]);
}

/// Acceptance 3: a session with a genuinely running tool is not cut early. One
/// session waits on the daemon beside it, yet the wait runs to its limit.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test(start_paused = true)]
async fn a_session_running_a_real_tool_keeps_the_full_drain_wait() {
    let (manager, _dir) = manager();
    let (waiter, builder) = (Uuid::new_v4(), Uuid::new_v4());
    manager
        .active
        .write()
        .await
        .insert(waiter, claude_turn(waiter, &[("call-1", CONTROL)]));
    // The builder's shell tool is open and its session also has a control call
    // in flight (a script that calls the daemon). The shell may be running
    // builtins no process listing shows, so it is a real tool (#2388).
    manager
        .active
        .write()
        .await
        .insert(builder, claude_turn(builder, &[("call-2", "Bash")]));
    let _waiter_call = manager.daemon_wait.begin(waiter);
    let _builder_call = manager.daemon_wait.begin(builder);
    manager.request_drain_restart();

    let (elapsed, decision) = poll_drain(&manager).await;
    assert_eq!(
        decision,
        OperatorDrainDecision::Complete {
            active_count: 2,
            daemon_waiting: 1,
            deploy_interrupted: 0,
            limit_reached: true,
        }
    );
    assert!(elapsed >= LIMIT, "cut early after {elapsed:?}");
    let mut states = states(&manager).await;
    states.sort();
    assert_eq!(states, ["finishing_current_turn", "waiting_on_daemon"]);
}

/// Every other shape of a turn keeps the full wait, even with a call in flight
/// past its settle time.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test(start_paused = true)]
async fn turns_the_daemon_cannot_prove_blocked_keep_the_full_drain_wait() {
    let (manager, _dir) = manager();
    let mut adopted = claude_turn(Uuid::new_v4(), &[("call-1", CONTROL)]);
    adopted.turn_tools = TurnToolLedger::untracked();
    let mut codex = claude_turn(Uuid::new_v4(), &[("call-1", CONTROL)]);
    codex.session.provider = SessionProvider::Codex;
    for (why, tracked) in [
        (
            "the model is sampling: no tool is open",
            claude_turn(Uuid::new_v4(), &[]),
        ),
        (
            "a read tool",
            claude_turn(Uuid::new_v4(), &[("call-1", "Read")]),
        ),
        (
            "an edit beside a control call",
            claude_turn(Uuid::new_v4(), &[("a", "Edit"), ("b", CONTROL)]),
        ),
        (
            "a shell with a daemon call in flight (it may run builtins)",
            claude_turn(Uuid::new_v4(), &[("a", "Bash")]),
        ),
        (
            "a shell beside a control call",
            claude_turn(Uuid::new_v4(), &[("a", "Bash"), ("b", CONTROL)]),
        ),
        (
            "a subagent",
            claude_turn(Uuid::new_v4(), &[("call-1", "Task")]),
        ),
        ("a turn adopted mid-way", adopted),
        ("a provider whose stream is not tracked", codex),
    ] {
        manager.active.write().await.clear();
        let id = tracked.session.id;
        manager.active.write().await.insert(id, tracked);
        let _call = manager.daemon_wait.begin(id);
        manager.request_drain_restart();
        let (elapsed, decision) = poll_drain(&manager).await;
        assert_eq!(
            decision,
            OperatorDrainDecision::Complete {
                active_count: 1,
                daemon_waiting: 0,
                deploy_interrupted: 0,
                limit_reached: true,
            },
            "{why}"
        );
        assert!(elapsed >= LIMIT, "{why}: cut early after {elapsed:?}");
    }
}

/// #2387: the classification is read at the instant the decision acts. A tool
/// that opens, or a call that completes, while the decision waits for the
/// active map (the monitor holds its write lock to record the tool) is a full
/// wait, never the early end the earlier state would have given.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test(start_paused = true)]
async fn a_tool_that_opens_or_a_call_that_completes_during_the_decision_keeps_the_full_wait() {
    #[derive(Clone, Copy, Debug)]
    enum Interleaved {
        NothingChanges,
        EditOpens,
        ShellOpens,
        CallCompletes,
    }
    for (what, early) in [
        (Interleaved::NothingChanges, true),
        (Interleaved::EditOpens, false),
        (Interleaved::ShellOpens, false),
        (Interleaved::CallCompletes, false),
    ] {
        for shutdown_wait in [false, true] {
            let (manager, _dir) = manager();
            let id = Uuid::new_v4();
            manager
                .active
                .write()
                .await
                .insert(id, claude_turn(id, &[("call-1", CONTROL)]));
            let mut call = Some(manager.daemon_wait.begin(id));
            manager.request_drain_restart();
            tokio::time::advance(DAEMON_WAIT_SETTLE).await;

            // The monitor holds the active map while it records the event, so
            // the decision is started first and parks on the lock.
            let mut guard = manager.active.write().await;
            let decision = async {
                if shutdown_wait {
                    let deadline_reached = manager.wait_for_finishable_turns(LIMIT).await.unwrap();
                    !deadline_reached
                } else {
                    matches!(
                        manager.operator_drain_decision(Duration::ZERO, LIMIT).await,
                        OperatorDrainDecision::Complete {
                            limit_reached: false,
                            ..
                        }
                    )
                }
            };
            let monitor =
                async {
                    tokio::task::yield_now().await;
                    match what {
                        Interleaved::NothingChanges => {}
                        Interleaved::EditOpens | Interleaved::ShellOpens => {
                            let tool = if matches!(what, Interleaved::EditOpens) {
                                "Edit"
                            } else {
                                "Bash"
                            };
                            guard.get_mut(&id).expect("tracked").turn_tools.observe_stream(
                            "assistant",
                            &serde_json::json!({"message": {"content": [{
                                "type": "tool_use", "id": "call-2", "name": tool, "input": {},
                            }]}}),
                        );
                        }
                        Interleaved::CallCompletes => drop(call.take()),
                    }
                    drop(guard);
                };
            let (ended_early, ()) = tokio::join!(decision, monitor);
            assert_eq!(
                ended_early, early,
                "{what:?} (shutdown wait: {shutdown_wait})"
            );
        }
    }
}

/// A call about to return is a turn between tools, not a daemon wait; and when
/// the call returns the session goes back to waiting out its limit.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test(start_paused = true)]
async fn only_a_call_in_flight_past_the_settle_time_counts_and_only_while_it_runs() {
    let (manager, _dir) = manager();
    let id = Uuid::new_v4();
    manager
        .active
        .write()
        .await
        .insert(id, claude_turn(id, &[("call-1", CONTROL)]));
    let waiting = OperatorDrainDecision::Complete {
        active_count: 1,
        daemon_waiting: 1,
        deploy_interrupted: 0,
        limit_reached: false,
    };

    assert_eq!(
        manager.operator_drain_decision(Duration::ZERO, LIMIT).await,
        OperatorDrainDecision::Wait,
        "no call in flight"
    );
    let call = manager.daemon_wait.begin(id);
    assert_eq!(
        manager.operator_drain_decision(Duration::ZERO, LIMIT).await,
        OperatorDrainDecision::Wait,
        "the call has only just started"
    );
    tokio::time::advance(DAEMON_WAIT_SETTLE).await;
    assert_eq!(
        manager.operator_drain_decision(Duration::ZERO, LIMIT).await,
        waiting
    );
    drop(call);
    assert_eq!(
        manager.operator_drain_decision(Duration::ZERO, LIMIT).await,
        OperatorDrainDecision::Wait,
        "the call returned, so the turn can end or go on"
    );
}

/// `shutdown_drain` waits for the same turns the operator wait does: the early
/// end must not be paid back by a second 30 s wait there.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test(start_paused = true)]
async fn the_shutdown_drain_wait_also_skips_daemon_waiters_but_not_real_tools() {
    let (manager, _dir) = manager();
    manager.request_drain_restart();
    let (first, second, builder) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    for id in [first, second] {
        manager
            .active
            .write()
            .await
            .insert(id, claude_turn(id, &[("call-1", CONTROL)]));
    }
    let _calls = [first, second].map(|id| manager.daemon_wait.begin(id));

    let started = tokio::time::Instant::now();
    let deadline_reached = manager.wait_for_finishable_turns(LIMIT).await.unwrap();
    assert!(!deadline_reached);
    assert!(
        started.elapsed() <= Duration::from_secs(5),
        "waited {:?}",
        started.elapsed()
    );

    manager
        .active
        .write()
        .await
        .insert(builder, claude_turn(builder, &[("call-2", "Bash")]));
    let started = tokio::time::Instant::now();
    let deadline_reached = manager.wait_for_finishable_turns(LIMIT).await.unwrap();
    assert!(
        deadline_reached,
        "a running tool holds the wait to its limit"
    );
    assert!(
        started.elapsed() >= LIMIT,
        "cut early after {:?}",
        started.elapsed()
    );
}

/// #2371: a deploy in `restarting` that recorded `named` as its interrupted
/// workers, with `interrupt_workers` as asked.
async fn restart_over(manager: &SessionManager, named: &[Uuid], interrupt_workers: bool) {
    use crate::store::agent_deploys::{NewDeploy, deploy_fingerprint_with};
    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    let owner = bare_session(Uuid::new_v4());
    insert_row(manager, &owner).await;
    let store = manager.store.lock().await;
    let now = chrono::Utc::now();
    let row = store
        .insert_agent_deploy(
            &NewDeploy {
                id: Uuid::new_v4(),
                owner_session_id: owner.id,
                idempotency_key: "drain-test",
                sha: SHA,
                fingerprint: deploy_fingerprint_with(SHA, "x", 60, interrupt_workers),
                manifest: &[],
                max_wait_secs: 60,
                interrupt_workers,
            },
            now,
        )
        .expect("staged deploy");
    assert!(store.mark_agent_deploy_restarting(row.id, now).unwrap());
    assert!(
        store
            .record_agent_deploy_interrupted(row.id, named)
            .unwrap()
    );
}

async fn mid_turn(manager: &SessionManager, id: Uuid) {
    manager
        .active
        .write()
        .await
        .insert(id, claude_turn(id, &[("call-1", "Bash")]));
}

/// #2371: once the `interrupt_workers` deploy named its workers, the wait ends
/// within a tick of the last session that is not one of them finishing, and
/// not before. The workers run real tools the whole time.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test(start_paused = true)]
async fn named_interrupted_workers_do_not_hold_the_drain_wait_past_the_last_other_session() {
    let (manager, _dir) = manager();
    let (first, second, other) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    for id in [first, second, other] {
        mid_turn(&manager, id).await;
    }
    restart_over(&manager, &[first, second], true).await;
    manager.request_drain_restart();

    let started = tokio::time::Instant::now();
    let finished_at = Duration::from_secs(12);
    let mut removed = None;
    let decision = loop {
        let elapsed = started.elapsed();
        if removed.is_none() && elapsed >= finished_at {
            manager.active.write().await.remove(&other);
            removed = Some(elapsed);
        }
        match manager.operator_drain_decision(elapsed, LIMIT).await {
            OperatorDrainDecision::Wait => tokio::time::advance(TICK).await,
            decided => break (elapsed, decided),
        }
    };
    let removed = removed.expect("the other session finished while the wait was open");
    assert_eq!(
        decision.1,
        OperatorDrainDecision::Complete {
            active_count: 2,
            daemon_waiting: 0,
            deploy_interrupted: 2,
            limit_reached: false,
        }
    );
    assert!(
        decision.0 - removed <= Duration::from_secs(1),
        "the wait ended {:?} after the last other session finished",
        decision.0 - removed
    );
    assert_eq!(
        states(&manager).await,
        vec!["deploy_interrupted"; 2],
        "the named workers read as the deploy's interrupts"
    );
    // `shutdown_drain` does not pay the wait a second time.
    assert!(!manager.wait_for_finishable_turns(LIMIT).await.unwrap());
}

/// #2371: a deploy without `interrupt_workers`, and a restart that names no
/// workers, keep the full wait for the same sessions.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test(start_paused = true)]
async fn without_interrupt_workers_the_same_sessions_keep_the_full_drain_wait() {
    for interrupt_workers in [false, true] {
        let (manager, _dir) = manager();
        let worker = Uuid::new_v4();
        mid_turn(&manager, worker).await;
        // A plain deploy that somehow carries a set must not count it, and an
        // interrupt deploy that named nobody has nothing to skip.
        let named: &[Uuid] = if interrupt_workers { &[] } else { &[worker] };
        restart_over(&manager, named, interrupt_workers).await;
        manager.request_drain_restart();

        let (elapsed, decision) = poll_drain(&manager).await;
        assert_eq!(
            decision,
            OperatorDrainDecision::Complete {
                active_count: 1,
                daemon_waiting: 0,
                deploy_interrupted: 0,
                limit_reached: true,
            },
            "interrupt_workers={interrupt_workers}"
        );
        assert!(elapsed >= LIMIT, "cut early after {elapsed:?}");
    }
}
