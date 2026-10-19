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

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn restart_recovery_queues_one_manager_resume_notice_unless_one_already_exists() {
    let (manager, dir) = manager();
    let project = Uuid::new_v4();
    let manager_id = Uuid::new_v4();
    let stamp = chrono::Utc::now();
    manager
        .store
        .lock()
        .await
        .insert_project(&rsi_common::types::Project {
            id: project,
            name: "Restart recovery test".into(),
            path: None,
            description: None,
            color: rsi_common::types::Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: stamp,
            updated_at: stamp,
        })
        .unwrap();
    let mut seat = bare_session(manager_id);
    seat.project_id = Some(project);
    seat.status = SessionStatus::Completed;
    seat.working_dir = dir.path().to_path_buf();
    insert_row(&manager, &seat).await;
    {
        let store = manager.store.lock().await;
        store.conn.execute(
            "INSERT INTO harness_manager_scopes
             (project_id,manager_session_id,epic_ids_json,row_version,updated_at,scope_mode,group_ids_json)
             VALUES(?1,?2,'[]',1,?3,'project','[]')",
            rusqlite::params![
                project.to_string(),
                manager_id.to_string(),
                chrono::Utc::now().to_rfc3339()
            ],
        ).unwrap();
    }

    let mut worker = bare_session(Uuid::new_v4());
    worker.project_id = Some(project);
    worker.status = SessionStatus::Interrupted;
    worker.issue_identifier = Some("RSI-1932".into());
    insert_row(&manager, &worker).await;
    // Existing terminal mail watches must not suppress the restart wake.
    manager
        .store
        .lock()
        .await
        .insert_scheduled_job(&mk_watch_job_for(
            worker.id,
            manager_id,
            "worker terminal mail",
        ))
        .unwrap();

    manager
        .queue_restart_recovery_manager_wakes(vec![worker.id])
        .await
        .unwrap();
    manager
        .queue_restart_recovery_manager_wakes(vec![worker.id])
        .await
        .unwrap();
    {
        let store = manager.store.lock().await;
        let mut stmt = store.conn.prepare(
            "SELECT message FROM scheduled_jobs WHERE wake_session_id=?1 AND wake_mode='resume' AND enabled=1",
        ).unwrap();
        let messages = stmt
            .query_map([manager_id.to_string()], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains(&worker.id.to_string()));
        assert!(messages[0].contains("RSI-1932"));
        assert!(messages[0].contains("Interrupted"));
    }

    let second_project = Uuid::new_v4();
    let second_manager_id = Uuid::new_v4();
    manager
        .store
        .lock()
        .await
        .insert_project(&rsi_common::types::Project {
            id: second_project,
            name: "Existing resume test".into(),
            path: None,
            description: None,
            color: rsi_common::types::Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: stamp,
            updated_at: stamp,
        })
        .unwrap();
    let mut second_seat = bare_session(second_manager_id);
    second_seat.project_id = Some(second_project);
    second_seat.status = SessionStatus::Completed;
    second_seat.working_dir = dir.path().to_path_buf();
    insert_row(&manager, &second_seat).await;
    manager.store.lock().await.conn.execute(
        "INSERT INTO harness_manager_scopes
         (project_id,manager_session_id,epic_ids_json,row_version,updated_at,scope_mode,group_ids_json)
         VALUES(?1,?2,'[]',1,?3,'project','[]')",
        rusqlite::params![
            second_project.to_string(),
            second_manager_id.to_string(),
            chrono::Utc::now().to_rfc3339()
        ],
    ).unwrap();
    let mut second_worker = worker.clone();
    second_worker.id = Uuid::new_v4();
    second_worker.project_id = Some(second_project);
    insert_row(&manager, &second_worker).await;

    let mut existing = mk_watch_job_for(second_worker.id, second_manager_id, "existing resume");
    existing.wake_mode = rsi_common::types::WakeMode::Resume;
    existing.wake_session_id = Some(second_manager_id);
    manager
        .store
        .lock()
        .await
        .insert_scheduled_job(&existing)
        .unwrap();
    manager
        .queue_restart_recovery_manager_wakes(vec![second_worker.id])
        .await
        .unwrap();
    let store = manager.store.lock().await;
    let count: i64 = store.conn.query_row(
        "SELECT count(*) FROM scheduled_jobs WHERE wake_session_id=?1 AND wake_mode='resume' AND enabled=1",
        [second_manager_id.to_string()],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(
        count, 1,
        "the existing resume wake suppresses a recovery wake"
    );
}
