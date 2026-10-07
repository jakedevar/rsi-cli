//! #1504: restart refusals are durable, observable owner-watch outcomes.
use super::*;

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn restart_recovery_refusals_record_andon_and_owner_watch_reason_once() {
    for (provider, thread, paused, expected) in [
        (SessionProvider::Codex, None, false, "not_resumable"),
        (
            SessionProvider::CodexAppServer,
            Some("app-thread"),
            false,
            "not_resumable",
        ),
        (
            SessionProvider::Codex,
            Some("codex-thread"),
            true,
            "operator_paused",
        ),
        (
            SessionProvider::Codex,
            Some("missing-thread"),
            false,
            "dispatch_failed",
        ),
    ] {
        let (mut manager, dir) = manager();
        let owner = Uuid::new_v4();
        insert_row(&manager, &bare_session(owner)).await;
        let mut row = bare_session(Uuid::new_v4());
        row.provider = provider;
        row.claude_session_id = thread.map(str::to_string);
        row.status = SessionStatus::Running;
        row.working_dir = dir.path().to_path_buf();
        if expected == "dispatch_failed" {
            // An isolated, nonexistent thread fails rollout validation before
            // any subprocess can run; do not depend on installed provider CLIs.
            row.claude_session_id = Some(row.id.to_string());
            let binary = std::env::current_exe().unwrap();
            manager.codex_client = Some(CodexClient::with_paths_for_test(
                binary.clone(),
                binary,
                std::sync::Arc::clone(&manager.runtime_config),
            ));
        }
        insert_row(&manager, &row).await;
        let invocation = Uuid::new_v4();
        {
            let store = manager.store.lock().await;
            let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            store
                .conn
                .execute(
                    "INSERT INTO model_invocations
                 (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                  trigger_source,session_id,created_at)
                 VALUES(?1,'session.launch.fresh','session_lifecycle','foreground','paid_capable','admitted','running',
                        'launch_session',?2,?3)",
                    rusqlite::params![invocation.to_string(), row.id.to_string(), at],
                )
                .unwrap();
            store
                .set_session_model_invocation(row.id, Some(invocation))
                .unwrap();
            let boot = Uuid::new_v4();
            assert!(store.record_restart_intent(row.id, boot).unwrap());
            store.mark_restart_interrupt_sent(row.id, boot).unwrap();
            if paused {
                store
                    .set_daemon_setting(&format!("manager_operator_pause:{}", row.id), "true")
                    .unwrap();
            }
        }
        manager.restore_sessions().await.unwrap();
        manager
            .reconcile_restart_intents_at_startup()
            .await
            .unwrap();
        manager
            .reconcile_restart_intents_at_startup()
            .await
            .unwrap();
        let persisted = manager
            .store
            .lock()
            .await
            .get_session(row.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            persisted.stop_reason.as_deref(),
            Some(format!("daemon_restart_resume:{expected}").as_str())
        );
        let signature = format!("terminal:daemon_restart_resume:{expected}");
        let count: i64 = manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT count(*) FROM friction_events WHERE session_id=?1 AND signature=?2",
                rusqlite::params![row.id.to_string(), signature],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        let watch = mk_watch_job_for(row.id, owner, "restart worker");
        manager
            .store
            .lock()
            .await
            .insert_scheduled_job(&watch)
            .unwrap();
        match manager.plan_terminal_watch_fire(&watch).await.unwrap() {
            WatchFirePlan::Deliver { tip, message, .. } => {
                assert_eq!(tip, owner);
                assert!(
                    message.contains(&format!("restart-resume: {expected}")),
                    "{message}"
                );
            }
            _ => panic!("expected owner notice"),
        }
    }
}
