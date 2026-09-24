#![cfg(target_os = "linux")]

use super::*;
use crate::config::{Config, RuntimeConfig};
use crate::session::launch::{
    ControllerCandidateTestPhase, drop_controller_candidate_test_stream,
    install_controller_candidate_test_pause, install_controller_candidate_test_process,
};
use crate::store::{
    Store,
    manager_resources::tests::{admitted, complete, fixture, raise_question},
};
use rsi_common::harness_manager_v2::*;
use std::sync::atomic::Ordering;

#[derive(Clone, Copy, PartialEq)]
enum CrashWindow {
    BeforeCleanup,
    FailedMarker,
}

async fn run_clear_failure(retain_first_cleanup: bool, crash: Option<CrashWindow>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("test.db")).unwrap();
    let (scope, lead) = fixture(&store, ManagerPolicyV2::default());
    // Measured zero on the preceding invocation is deliberately tempting but
    // must never become telemetry for the answer's unmonitored invocation.
    let prior = admitted(&store, &lead);
    complete(&store, prior, 0.0);
    store
        .set_session_model_invocation(lead.id, Some(prior))
        .unwrap();
    let snapshot_path = dir.path().join("crash.db");
    raise_question(&store, lead.id, 1);
    // Only the fixture's working directory changes; this is an ordinary
    // unsandboxed Claude continuation through the actual guarded lifecycle.
    store
        .conn
        .execute(
            "UPDATE sessions SET working_dir=?2,claude_session_id=?3 WHERE id=?1",
            rusqlite::params![
                lead.id.to_string(),
                dir.path().to_string_lossy(),
                Uuid::new_v4().to_string()
            ],
        )
        .unwrap();
    let session = store.get_session(lead.id).unwrap().unwrap();
    let manager = SessionManager::new(
        Arc::new(EventBus::new(32)),
        store,
        false,
        dir.path().join("daemon.sock"),
        None,
        vec![],
        RuntimeConfig::from_config(&Config::from_env()),
        dir.path().join("sandboxes"),
    )
    .unwrap();
    let mut cached = CompletedSession::for_test(session);
    cached.events = manager.store.lock().await.load_events(lead.id).unwrap();
    manager.completed.write().await.insert(lead.id, cached);
    {
        let store = manager.store.lock().await;
        store.manager_v2_refresh_question_decisions(&scope).unwrap();
        let decision = store
            .manager_v2_record(&scope, "decision", &format!("question:{}", lead.id))
            .unwrap()
            .unwrap();
        store
            .manager_v2_prepare_decision_answer(
                &AnswerHarnessManagerDecisionRequestV2 {
                    project_id: scope.project_id,
                    fence: ManagerFenceV2 {
                        scope_version: scope.row_version,
                        policy_version: 1,
                    },
                    decision_key: decision.key,
                    expected_row_version: decision.row_version,
                    target_digest: decision.payload["target_digest"].as_str().unwrap().into(),
                    answer: "Proceed once".into(),
                    idempotency_key: "cleanup-answer".into(),
                },
                |_, _, _, _| unreachable!(),
            )
            .unwrap();
    }
    let process = install_controller_candidate_test_process(lead.id);
    let (spawned, clear) = install_controller_candidate_test_pause(
        lead.id,
        ControllerCandidateTestPhase::ManagerQuestionBeforeClear,
    );
    let retention = (retain_first_cleanup || crash == Some(CrashWindow::FailedMarker)).then(|| {
        install_controller_candidate_test_pause(
            lead.id,
            ControllerCandidateTestPhase::ManagerQuestionCleanupRetained,
        )
    });
    let orphan_fixture = crate::session::reaper::StartupReaperFixture::new();
    let revoke = async {
        spawned.await.unwrap();
        assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
        assert!(process.alive.load(Ordering::SeqCst));
        let store = manager.store.lock().await;
        let mut grant = store
            .get_harness_manager_policy(scope.project_id)
            .unwrap()
            .unwrap();
        grant.policy.paused = true;
        grant.policy.max_spend_usd = Some(10.0);
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: scope.project_id,
                expected_scope_version: scope.row_version,
                expected_policy_version: grant.row_version,
                idempotency_key: "pause-after-spawn".into(),
                policy: grant.policy,
            })
            .unwrap();
        if retain_first_cleanup {
            crate::session::reaper::fail_runtime_orphan_reap_for_test(lead.id);
        }
        if crash == Some(CrashWindow::FailedMarker) {
            store.conn.execute_batch(
                "CREATE TEMP TRIGGER fail_question_cleanup_marker BEFORE UPDATE OF error_class ON model_invocations
                 WHEN NEW.error_class='manager_question_cleanup_required'
                 BEGIN SELECT RAISE(FAIL,'injected cleanup marker write failure'); END;",
            ).unwrap();
        }
        if crash == Some(CrashWindow::BeforeCleanup) {
            store
                .conn
                .execute("VACUUM INTO ?1", [snapshot_path.to_str().unwrap()])
                .unwrap();
        }
        // A real stamped process models a surviving provider tool writer.
        // The checked production reaper operates on its actual /proc identity
        // and pidfd through the established isolated inventory fixture.
        let orphan = orphan_fixture.spawn_runtime_session(lead.id);
        let guard = orphan_fixture.scoped_runtime_reap_root(lead.id).unwrap();
        clear.send(()).unwrap();
        (orphan, guard)
    };
    let (result, (mut orphan, _orphan_guard)) =
        tokio::time::timeout(Duration::from_secs(30), async {
            tokio::join!(manager.reconcile_harness_managers_once(), revoke)
        })
        .await
        .unwrap();
    result.unwrap();
    if let Some((retained, retry)) = retention {
        retained.await.unwrap();
        assert!(
            std::path::Path::new(&format!("/proc/{}/stat", orphan.pid())).exists(),
            "unproved cleanup still owns the surviving writer"
        );
        let store = manager.store.lock().await;
        assert_eq!(
            store.manager_v2_resource_snapshot(&scope).unwrap()["active_sessions"],
            1,
            "unproved orphan exclusion holds capacity even when the immediate child died"
        );
        let invocation: Uuid = store.conn.query_row(
            "SELECT id FROM model_invocations WHERE session_id=?1 AND dedup_key LIKE 'manager.answer:%'",
            [lead.id.to_string()], |row| row.get::<_, String>(0),
        ).unwrap().parse().unwrap();
        let row = store
            .load_model_invocation_record(invocation)
            .unwrap()
            .unwrap();
        if crash == Some(CrashWindow::FailedMarker) {
            assert_eq!(row.error_class, None);
            assert_eq!(
                store.session_model_invocation_id(lead.id).unwrap(),
                Some(prior)
            );
            store
                .conn
                .execute("VACUUM INTO ?1", [snapshot_path.to_str().unwrap()])
                .unwrap();
            store
                .conn
                .execute_batch("DROP TRIGGER fail_question_cleanup_marker")
                .unwrap();
        } else {
            assert_eq!(
                row.error_class.as_deref(),
                Some("manager_question_cleanup_required")
            );
        }
        drop(store);
        retry.send(()).unwrap();
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while !manager.completed.read().await.contains_key(&lead.id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!process.alive.load(Ordering::SeqCst));
    orphan.wait_signalled("question failure cleanup reaps real writer");
    drop_controller_candidate_test_stream(lead.id);
    let store = manager.store.lock().await;
    assert_eq!(
        store.manager_v2_resource_snapshot(&scope).unwrap()["active_sessions"],
        0
    );
    let invocation = store.session_model_invocation_id(lead.id).unwrap().unwrap();
    let row = store
        .load_model_invocation_record(invocation)
        .unwrap()
        .unwrap();
    assert_eq!(
        row.error_class.as_deref(),
        Some("manager_question_clear_failed")
    );
    assert_eq!(row.usage.estimated_cost_usd, None);
    assert_eq!(
        store.get_session(lead.id).unwrap().unwrap().status,
        SessionStatus::Interrupted
    );
    assert!(
        store.manager_v2_question_target(lead.id).unwrap().is_some(),
        "failed clear retains the exact question"
    );
    let delivery = &store
        .manager_v2_records_of_kind(&scope, "decision_delivery")
        .unwrap()[0];
    assert_eq!(delivery.payload["state"], "uncertain");
    assert_eq!(delivery.payload["effect_started"], true);
    drop(store);
    manager.reconcile_harness_managers_once().await.unwrap();
    assert_eq!(
        process.productive_start_count.load(Ordering::SeqCst),
        1,
        "uncertain answer is never resent"
    );
    if crash.is_some() {
        // The snapshot was taken at the actual effect boundary. Only reopen
        // after the runtime owner has proved both the provider and its stamped
        // writer gone, matching the production process-first startup contract.
        let reopened = Store::open(&snapshot_path).unwrap();
        let answer: Uuid = reopened.conn.query_row(
            "SELECT id FROM model_invocations WHERE session_id=?1 AND dedup_key LIKE 'manager.answer:%'",
            [lead.id.to_string()], |row| row.get::<_, String>(0),
        ).unwrap().parse().unwrap();
        assert_eq!(
            reopened.session_model_invocation_id(lead.id).unwrap(),
            Some(prior)
        );
        assert_eq!(
            reopened
                .load_model_invocation_record(answer)
                .unwrap()
                .unwrap()
                .error_class,
            None
        );
        assert_eq!(reopened.reconcile_running_model_invocations().unwrap(), 1);
        let row = reopened
            .load_model_invocation_record(answer)
            .unwrap()
            .unwrap();
        assert_eq!(row.usage.estimated_cost_usd, None);
        assert_eq!(row.usage.input_tokens, None);
        assert_eq!(row.usage.confidence, ModelUsageConfidence::Unavailable);
        assert_eq!(row.error_class.as_deref(), Some("manager_answer_restart"));
        assert_eq!(
            reopened
                .load_model_invocation_record(prior)
                .unwrap()
                .unwrap()
                .usage
                .estimated_cost_usd,
            Some(0.0)
        );
        let resources = reopened.manager_v2_resource_snapshot(&scope).unwrap();
        assert_eq!(resources["active_sessions"], 0);
        assert_eq!(resources["known_spend_usd"], 0.0);
        assert_eq!(resources["unknown_spend_observations"], 1);
        let mut grant = reopened
            .get_harness_manager_policy(scope.project_id)
            .unwrap()
            .unwrap();
        grant.policy.paused = false;
        reopened
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: scope.project_id,
                expected_scope_version: scope.row_version,
                expected_policy_version: grant.row_version,
                idempotency_key: "unpause-after-reopen".into(),
                policy: grant.policy,
            })
            .unwrap();
        assert!(
            reopened
                .manager_v2_resource_gate(&scope, Some(scope.epic_ids[0]), lead.provider, None)
                .unwrap_err()
                .to_string()
                .contains("spend_unknown")
        );
        let restart = SessionManager::new(
            Arc::new(EventBus::new(32)),
            reopened,
            false,
            dir.path().join("restart.sock"),
            None,
            vec![],
            RuntimeConfig::from_config(&Config::from_env()),
            dir.path().join("restart-sandboxes"),
        )
        .unwrap();
        let unexpected = install_controller_candidate_test_process(lead.id);
        restart.reconcile_harness_managers_once().await.unwrap();
        let store = restart.store.lock().await;
        let delivery = &store
            .manager_v2_records_of_kind(&scope, "decision_delivery")
            .unwrap()[0];
        assert_eq!(delivery.payload["state"], "uncertain");
        assert_eq!(delivery.payload["effect_started"], true);
        assert!(store.manager_v2_question_target(lead.id).unwrap().is_some());
        assert_eq!(unexpected.productive_start_count.load(Ordering::SeqCst), 0);
        drop_controller_candidate_test_stream(lead.id);
    }
}

#[tokio::test]
async fn manager_v2_question_clear_failure_settles_spawned_provider_and_keeps_uncertainty() {
    run_clear_failure(false, None).await;
}

#[tokio::test]
async fn manager_v2_question_clear_failure_retains_capacity_until_checked_reap_succeeds() {
    run_clear_failure(true, None).await;
}

#[tokio::test]
async fn manager_v2_answer_reopen_before_cleanup_marker_does_not_copy_prior_zero_cost() {
    run_clear_failure(false, Some(CrashWindow::BeforeCleanup)).await;
}

#[tokio::test]
async fn manager_v2_answer_reopen_after_failed_cleanup_marker_preserves_unknown_spend() {
    run_clear_failure(false, Some(CrashWindow::FailedMarker)).await;
}
