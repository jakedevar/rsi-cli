use super::*;
use rsi_common::harness_manager::*;
use rsi_common::types::{Project, ScheduledJob, WakeMode};

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening
)]
async fn manager_seat_succession_hands_over_job_reads_waits_and_cancellation() {
    use crate::agent_jobs::JobRuntime;
    use crate::store::agent_jobs::NewAgentJob;
    use rsi_common::agent_jobs::{
        AgentCancelJobRequestV1, AgentGetJobRequestV1, AgentJobResultV1, AgentListJobsRequestV1,
        JobState, JobWake,
    };
    use rsi_common::wake_predicate::WakePredicate;

    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<String>>);
    impl JobRuntime for Recorder {
        fn launch(&self, _: &crate::agent_jobs::LaunchSpec) -> std::result::Result<(), String> {
            Ok(())
        }
        fn unit_active(&self, _: &str) -> bool {
            false
        }
        fn stop_unit(&self, unit: &str) -> std::result::Result<(), String> {
            self.0.lock().unwrap().push(unit.into());
            Ok(())
        }
    }

    for rotation in [true, false] {
        let pilot = pilot().await;
        let successor = Uuid::new_v4();
        let make_job = |owner| {
            let id = Uuid::new_v4();
            NewAgentJob {
                id,
                owner_session_id: owner,
                project_id: Some(pilot.config.project_id),
                name: None,
                params: serde_json::from_value(serde_json::json!({
                    "kind":"build", "command":"check", "workspace":true
                }))
                .unwrap(),
                cwd: pilot._directory.path().display().to_string(),
                unit_name: format!("rsi-job-{id}"),
                log_path: pilot
                    ._directory
                    .path()
                    .join(format!("{id}.log"))
                    .display()
                    .to_string(),
                status_path: pilot
                    ._directory
                    .path()
                    .join(format!("{id}.status"))
                    .display()
                    .to_string(),
                idempotency_key: None,
                wake: JobWake::Owner,
            }
        };
        let (live, queued, done, foreign) = (
            make_job(pilot.owner),
            make_job(pilot.owner),
            make_job(pilot.owner),
            make_job(pilot.leads[0]),
        );
        {
            let store = pilot.manager.store.lock().await;
            let mut row = store.get_session(pilot.owner).unwrap().unwrap();
            row.id = successor;
            row.status = SessionStatus::Completed;
            if rotation {
                row.continued_from = Some(pilot.owner);
                row.rotation_depth += 1;
            }
            store.insert_session(&row).unwrap();
            for job in [&live, &done, &foreign] {
                store.insert_agent_job(job, chrono::Utc::now()).unwrap();
            }
            store
                .insert_agent_job_in_state(&queued, chrono::Utc::now(), JobState::Queued)
                .unwrap();
            store
                .settle_agent_job(
                    done.id,
                    JobState::Succeeded,
                    &AgentJobResultV1::default(),
                    false,
                    chrono::Utc::now(),
                )
                .unwrap();
            // Rotating an ordinary leaf in a configured project transfers no jobs.
            let mut leaf = store.get_session(pilot.leads[0]).unwrap().unwrap();
            leaf.id = Uuid::new_v4();
            leaf.continued_from = Some(pilot.leads[0]);
            leaf.rotation_depth += 1;
            store.insert_session(&leaf).unwrap();
            store
                .update_session_status(pilot.leads[0], SessionStatus::Archived)
                .unwrap();
            store
                .record_harness_manager_rotation(pilot.leads[0], leaf.id)
                .unwrap();
            assert_eq!(
                store
                    .get_agent_job(foreign.id)
                    .unwrap()
                    .unwrap()
                    .job
                    .owner_session_id,
                pilot.leads[0]
            );
        }
        let control = pilot.manager.agent_control();
        let predicate = || WakePredicate {
            jobs_terminal: Some(vec![live.id, queued.id]),
            sha_on_rolling: None,
            issue_closed: None,
            todo_closed: None,
            topology_execution_settled: None,
            health_anomaly: None,
            settlement_run_settled: None,
        };
        assert!(
            control
                .agent_get_job(successor, AgentGetJobRequestV1 { job_id: live.id })
                .await
                .unwrap_err()
                .to_string()
                .contains("job_not_found")
        );
        assert!(
            control
                .arm_wake_when(successor, "wait".into(), None, predicate(), None)
                .await
                .unwrap_err()
                .to_string()
                .contains("wake_when_job_not_found")
        );
        let (existing, _) = control
            .arm_wake_when(
                pilot.owner,
                "batch".into(),
                None,
                WakePredicate {
                    jobs_terminal: Some(vec![live.id, done.id]),
                    sha_on_rolling: None,
                    issue_closed: None,
                    todo_closed: None,
                    topology_execution_settled: None,
                    health_anomaly: None,
                    settlement_run_settled: None,
                },
                None,
            )
            .await
            .unwrap();

        {
            let store = pilot.manager.store.lock().await;
            if rotation {
                store
                    .update_session_status(pilot.owner, SessionStatus::Archived)
                    .unwrap();
                store
                    .record_harness_manager_rotation(pilot.owner, successor)
                    .unwrap();
            } else {
                store
                    .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                        project_id: pilot.config.project_id,
                        session_id: successor,
                        epic_ids: Some(pilot.epics.to_vec()),
                        group_ids: Vec::new(),
                        expected_row_version: pilot.config.row_version,
                    })
                    .unwrap();
            }
            assert_eq!(
                store
                    .get_scheduled_job(&existing.id)
                    .unwrap()
                    .unwrap()
                    .wake_session_id,
                Some(successor)
            );
            assert_eq!(
                store
                    .get_agent_job(foreign.id)
                    .unwrap()
                    .unwrap()
                    .job
                    .owner_session_id,
                pilot.leads[0]
            );
        }
        assert_eq!(
            control
                .agent_get_job(successor, AgentGetJobRequestV1 { job_id: live.id })
                .await
                .unwrap()
                .owner_session_id,
            successor
        );
        // #2470: a job settled before a published rotation stays readable by
        // the successor seat, listed and fetched, without moving its owner;
        // a seat change with no rotation lineage proves no predecessor.
        let listed = control
            .agent_list_jobs(successor, AgentListJobsRequestV1 { limit: None })
            .await
            .unwrap()
            .jobs;
        assert_eq!(listed.len(), if rotation { 3 } else { 2 });
        let settled = control
            .agent_get_job(successor, AgentGetJobRequestV1 { job_id: done.id })
            .await;
        if rotation {
            let settled = settled.unwrap();
            assert_eq!(settled.owner_session_id, pilot.owner);
            assert_eq!(settled.state, JobState::Succeeded);
            assert!(listed.iter().any(|job| job.id == done.id));
        } else {
            assert!(settled.unwrap_err().to_string().contains("job_not_found"));
        }
        // #132: the watch follows the read. The successor arms `jobs_terminal`
        // over the job settled before the rotation only where the read works.
        let settled_watch = control
            .arm_wake_when(
                successor,
                "settled".into(),
                None,
                WakePredicate {
                    jobs_terminal: Some(vec![done.id]),
                    sha_on_rolling: None,
                    issue_closed: None,
                    todo_closed: None,
                    topology_execution_settled: None,
                    health_anomaly: None,
                    settlement_run_settled: None,
                },
                None,
            )
            .await;
        if rotation {
            let (armed, _) = settled_watch.unwrap();
            assert_eq!(armed.wake_session_id, Some(successor));
        } else {
            assert!(
                settled_watch
                    .unwrap_err()
                    .to_string()
                    .contains("wake_when_job_not_found")
            );
        }
        // An in-scope worker's job is readable by the live seat only.
        assert_eq!(
            control
                .agent_get_job(successor, AgentGetJobRequestV1 { job_id: foreign.id })
                .await
                .unwrap()
                .owner_session_id,
            pilot.leads[0]
        );
        for caller in [pilot.owner, pilot.leads[1]] {
            assert!(
                control
                    .agent_get_job(caller, AgentGetJobRequestV1 { job_id: foreign.id })
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("job_not_found")
            );
        }
        // A foreign manager (another project) learns nothing about either job.
        let outsider = Uuid::new_v4();
        for job_id in [done.id, foreign.id] {
            assert!(
                control
                    .agent_get_job(outsider, AgentGetJobRequestV1 { job_id })
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("job_not_found")
            );
        }
        assert_eq!(
            control
                .agent_list_jobs(pilot.owner, AgentListJobsRequestV1 { limit: None })
                .await
                .unwrap()
                .jobs[0]
                .id,
            done.id
        );
        control
            .arm_wake_when(successor, "wait".into(), None, predicate(), None)
            .await
            .unwrap();
        assert!(
            control
                .arm_wake_when(pilot.owner, "wait".into(), None, predicate(), None)
                .await
                .unwrap_err()
                .to_string()
                .contains("wake_when_job_not_found")
        );

        let runtime = std::sync::Arc::new(Recorder::default());
        for caller in [pilot.owner, pilot.leads[1]] {
            assert!(
                control
                    .agent_get_job(caller, AgentGetJobRequestV1 { job_id: live.id })
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("job_not_found")
            );
            assert!(
                control
                    .agent_cancel_job(
                        caller,
                        AgentCancelJobRequestV1 { job_id: live.id },
                        runtime.clone()
                    )
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("job_not_found")
            );
        }
        for job in [&live, &queued] {
            let cancelled = control
                .agent_cancel_job(
                    successor,
                    AgentCancelJobRequestV1 { job_id: job.id },
                    runtime.clone(),
                )
                .await
                .unwrap();
            assert!(cancelled.cancelled);
            assert_eq!(cancelled.job.owner_session_id, successor);
            assert_eq!(cancelled.job.state, JobState::Failed);
            assert_eq!(
                cancelled.job.result.unwrap().refusal.as_deref(),
                Some("job_cancelled")
            );
        }
        assert_eq!(*runtime.0.lock().unwrap(), [live.unit_name]);
    }
}

struct Pilot {
    manager: SessionManager,
    _directory: TempDir,
    owner: Uuid,
    epics: [Uuid; 2],
    leads: [Uuid; 2],
    config: HarnessManagerConfigV1,
}

async fn pilot() -> Pilot {
    let (manager, directory) = manager();
    let project = Project {
        id: Uuid::new_v4(),
        name: "Manager watch pilot".into(),
        path: None,
        description: None,
        color: Project::DEFAULT_COLOR.into(),
        context_files: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let owner = Uuid::new_v4();
    let epics = [Uuid::new_v4(), Uuid::new_v4()];
    let leads = [Uuid::new_v4(), Uuid::new_v4()];
    let config = {
        let store = manager.store.lock().await;
        store.insert_project(&project).unwrap();
        let mut owner_row = bare_session(owner);
        owner_row.project_id = Some(project.id);
        store.insert_session(&owner_row).unwrap();
        let mut group = owner_row.clone();
        group.id = Uuid::new_v4();
        group.session_kind = SessionKind::Group;
        store.insert_session(&group).unwrap();
        for index in 0..2 {
            let mut epic = owner_row.clone();
            epic.id = epics[index];
            epic.session_kind = SessionKind::Epic;
            epic.parent_id = Some(group.id);
            epic.title = Some(format!("Feature {index}"));
            store.insert_session(&epic).unwrap();
            let mut lead = owner_row.clone();
            lead.id = leads[index];
            lead.parent_id = Some(epic.id);
            lead.session_kind = SessionKind::Feature;
            store.insert_session(&lead).unwrap();
            store.set_lead_session(epic.id, Some(lead.id)).unwrap();
        }
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: project.id,
                session_id: owner,
                epic_ids: Some(epics.to_vec()),
                expected_row_version: 0,
            })
            .unwrap()
    };
    Pilot {
        manager,
        _directory: directory,
        owner,
        epics,
        leads,
        config,
    }
}

async fn notices(pilot: &Pilot) -> Vec<ScheduledJob> {
    pilot
        .manager
        .store
        .lock()
        .await
        .list_scheduled_jobs()
        .unwrap()
        .into_iter()
        .filter(|job| job.wake_session_id == Some(pilot.owner))
        .collect()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening
)]
async fn agent_manager_notify_publishes_manager_notice_job() {
    let pilot = pilot().await;
    let mut events = pilot.manager.event_bus.subscribe();
    let receipt = pilot
        .manager
        .agent_control()
        .agent_manager_notify(
            pilot.leads[0],
            AgentManagerNotifyRequestV1 {
                message: "checks passed".into(),
                idempotency_key: "notify-job".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(receipt.request_id, None);
    let job_id = pilot
        .manager
        .store
        .lock()
        .await
        .manager_notice_job_for_subject(
            "message",
            &receipt.message_id.to_string(),
            &receipt.sequence.to_string(),
        )
        .unwrap()
        .expect("notice job");
    let event = events.try_recv().expect("notice event");
    assert!(
        matches!(event.as_ref(), crate::bus::DaemonEvent::ManagerNoticeQueued { job_id: emitted } if *emitted == job_id)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn manager_watch_coalesces_epics_separately_from_ordinary_watches_and_defers_busy_recipient()
{
    let pilot = pilot().await;
    let notices = notices(&pilot).await;
    assert_eq!(notices.len(), 2);
    let ordinary = mk_watch_job_for(pilot.leads[0], pilot.owner, "ordinary child progress");
    pilot
        .manager
        .store
        .lock()
        .await
        .insert_scheduled_job(&ordinary)
        .unwrap();
    let plan = pilot
        .manager
        .plan_terminal_watch_fire(&notices[0])
        .await
        .unwrap();
    let WatchFirePlan::Deliver {
        tip,
        message,
        mut job_ids,
        ..
    } = plan
    else {
        panic!("expected coalesced manager notice: {plan:?}");
    };
    assert_eq!(tip, pilot.owner);
    assert!(message.contains(&notices[0].message));
    assert!(message.contains(&notices[1].message));
    for lead in pilot.leads {
        assert!(
            message.contains(&lead.to_string()),
            "coalescing retains every exact session identity"
        );
    }
    job_ids.sort_unstable();
    let mut expected: Vec<_> = notices.iter().map(|job| job.id).collect();
    expected.sort_unstable();
    assert_eq!(job_ids, expected);
    let WatchFirePlan::Deliver {
        job_ids, message, ..
    } = pilot
        .manager
        .plan_terminal_watch_fire(&ordinary)
        .await
        .unwrap()
    else {
        panic!("ordinary watch must retain its own delivery");
    };
    assert_eq!(job_ids, vec![ordinary.id]);
    assert!(message.contains("ordinary child progress"));

    let owner = pilot
        .manager
        .store
        .lock()
        .await
        .get_session(pilot.owner)
        .unwrap()
        .unwrap();
    pilot
        .manager
        .active
        .write()
        .await
        .insert(pilot.owner, TrackedSession::new_for_test(owner));
    assert!(matches!(
        pilot
            .manager
            .fire_terminal_watch(&notices[0])
            .await
            .unwrap(),
        crate::issue_tracker::poller::WatchFireOutcome::NotReady
    ));
    let active = pilot.manager.active.read().await;
    assert!(!active.get(&pilot.owner).unwrap().interrupt_requested);
    assert!(
        pilot
            .manager
            .store
            .lock()
            .await
            .get_scheduled_job(&notices[0].id)
            .unwrap()
            .unwrap()
            .enabled
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn manager_watch_busy_turn_output_keeps_undelivered_notice_armed() {
    use crate::issue_tracker::poller::SessionLauncher;

    let pilot = pilot().await;
    let job = notices(&pilot).await.remove(0);
    let owner = pilot.owner;
    let manager = Arc::new(pilot.manager);
    let owner_row = manager
        .store
        .lock()
        .await
        .get_session(owner)
        .unwrap()
        .unwrap();
    manager
        .active
        .write()
        .await
        .insert(owner, TrackedSession::new_for_test(owner_row));
    let launcher: Arc<dyn SessionLauncher> = manager.clone();
    crate::scheduler::fire_job_for_test(&manager.store, &manager.event_bus, &launcher, &job).await;
    push_event(
        &manager,
        owner,
        1,
        EventType::Message,
        Some(Role::Assistant),
        chrono::Utc::now(),
    )
    .await;
    crate::scheduler::fire_job_for_test(&manager.store, &manager.event_bus, &launcher, &job).await;
    let pending = manager
        .store
        .lock()
        .await
        .get_scheduled_job(&job.id)
        .unwrap()
        .unwrap();
    assert!(
        pending.enabled,
        "busy-turn output cannot consume an undelivered notice"
    );
    assert_eq!(pending.last_fired_at, None);
    assert!(
        !manager
            .active
            .read()
            .await
            .get(&owner)
            .unwrap()
            .interrupt_requested
    );
    manager.active.write().await.remove(&owner);
    assert!(matches!(
        manager.plan_terminal_watch_fire(&pending).await.unwrap(),
        WatchFirePlan::Deliver { tip, .. } if tip == owner
    ));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn manager_action_self_transport_reaches_idle_manager_and_settles_only_on_inbox() {
    let pilot = pilot().await;
    let (job_id, subject_id, job) = {
        let store = pilot.manager.store.lock().await;
        // Remove the fixture's Epic notices so this assertion exercises only
        // the project action transport.
        store
            .manager_inbox(pilot.owner, &AgentManagerInboxRequestV1::default())
            .unwrap();
        let (job_id, source, target) = store.manager_action_watch_identity(&pilot.config);
        assert_eq!((source, target), (pilot.owner, pilot.owner));
        let subject_id = Uuid::new_v4();
        store
            .ensure_manager_action_watch(&pilot.config, "self-route")
            .unwrap();
        let recorded = crate::store::harness_manager_v2::now();
        store
            .conn
            .execute(
                "INSERT INTO harness_manager_notices
                 (id,job_id,project_id,manager_session_id,scope_version,epic_id,direction,
                  source_session_id,recipient_session_id,kind,subject_id,subject_version,
                  state_json,recorded_at,queued_at)
                 VALUES(?1,?2,?3,?4,?5,NULL,'to_manager',NULL,?4,
                        'action_result',?6,'1','{}',?7,?7)",
                rusqlite::params![
                    Uuid::new_v4().to_string(),
                    job_id.to_string(),
                    pilot.config.project_id.to_string(),
                    pilot.owner.to_string(),
                    pilot.config.row_version,
                    subject_id.to_string(),
                    recorded
                ],
            )
            .unwrap();
        store.refresh_manager_notice_job(job_id).unwrap();
        let job = store.get_scheduled_job(&job_id).unwrap().unwrap();
        (job_id, subject_id, job)
    };
    {
        let store = pilot.manager.store.lock().await;
        assert_eq!(
            store.manager_watch_delivery_state(job_id).unwrap(),
            Some(true),
            "an unclassified action result still owes a manager wake"
        );
        assert!(
            store
                .manager_notice_first_wake_pending(job_id)
                .unwrap()
                .is_some(),
            "the self-route transport must owe its first inbox wake"
        );
    }
    assert!(
        pilot
            .manager
            .manager_notice_recipient_idle_wake(job_id, pilot.owner)
            .await
            .unwrap()
            .is_some(),
        "the current completed manager is an eligible idle recipient"
    );
    let plan = pilot.manager.plan_terminal_watch_fire(&job).await.unwrap();
    let WatchFirePlan::Deliver { tip, job_ids, .. } = plan else {
        panic!("manager-bound self route must deliver to the idle current manager: {plan:?}");
    };
    assert_eq!(tip, pilot.owner);
    assert_eq!(job_ids, vec![job_id]);

    let delivered = chrono::Utc::now();
    {
        let store = pilot.manager.store.lock().await;
        let (_, capture) = store.capture_watch_fire(job_id).unwrap().unwrap();
        assert!(
            store
                .settle_watch_fire(&capture, job_id, Some(&delivered), Some(&delivered), true)
                .unwrap()
        );
    }
    let delivered_job = pilot
        .manager
        .store
        .lock()
        .await
        .get_scheduled_job(&job_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        pilot
            .manager
            .plan_terminal_watch_fire(&delivered_job)
            .await
            .unwrap(),
        WatchFirePlan::NotReady,
        "delivery cannot loop while exact inbox retrieval is still owed"
    );
    let inbox = pilot
        .manager
        .store
        .lock()
        .await
        .manager_inbox(pilot.owner, &AgentManagerInboxRequestV1::default())
        .unwrap();
    assert!(inbox.notices.iter().any(|notice| {
        notice.kind == "action_result" && notice.subject_id == subject_id.to_string()
    }));
    assert!(
        !pilot
            .manager
            .store
            .lock()
            .await
            .get_scheduled_job(&job_id)
            .unwrap()
            .unwrap()
            .enabled
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn manager_watch_provider_output_does_not_settle_a_delivered_exact_notice() {
    let pilot = pilot().await;
    let job = notices(&pilot).await.remove(0);
    let fired_at = chrono::Utc::now();
    {
        let store = pilot.manager.store.lock().await;
        let (_, capture) = store.capture_watch_fire(job.id).unwrap().unwrap();
        assert!(
            store
                .settle_watch_fire(&capture, job.id, Some(&fired_at), Some(&fired_at), true,)
                .unwrap()
        );
    }
    push_event(
        &pilot.manager,
        pilot.owner,
        1,
        EventType::Message,
        Some(Role::Assistant),
        chrono::Utc::now(),
    )
    .await;
    assert_eq!(
        pilot.manager.plan_terminal_watch_fire(&job).await.unwrap(),
        WatchFirePlan::NotReady,
        "provider output is not acknowledgement of the exact inbox notice"
    );
    let inbox = pilot
        .manager
        .store
        .lock()
        .await
        .manager_inbox(pilot.owner, &AgentManagerInboxRequestV1::default())
        .unwrap();
    assert!(
        inbox
            .notices
            .iter()
            .any(|notice| notice.epic_id == Some(pilot.epics[0]))
    );
    assert!(
        !pilot
            .manager
            .store
            .lock()
            .await
            .get_scheduled_job(&job.id)
            .unwrap()
            .unwrap()
            .enabled
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn pre_v117_manager_watch_keeps_legacy_delivery_confirmation_after_upgrade() {
    let pilot = pilot().await;
    let legacy = {
        let store = pilot.manager.store.lock().await;
        let lead = store.get_session(pilot.leads[0]).unwrap().unwrap();
        store
            .ensure_manager_watch(&pilot.config, &lead, false, "legacy-pending-mail", true)
            .unwrap();
        store
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .find(|job| {
                job.wake_session_id == Some(lead.id)
                    && job.wake_mode == WakeMode::OnTerminal(pilot.owner)
            })
            .unwrap()
    };
    assert!(matches!(
        pilot
            .manager
            .plan_terminal_watch_fire(&legacy)
            .await
            .unwrap(),
        WatchFirePlan::Deliver { tip, .. } if tip == pilot.leads[0]
    ));

    let fired_at = chrono::Utc::now();
    {
        let store = pilot.manager.store.lock().await;
        assert_eq!(store.manager_watch_delivery_state(legacy.id).unwrap(), None);
        let (_, capture) = store.capture_watch_fire(legacy.id).unwrap().unwrap();
        assert!(
            store
                .settle_watch_fire(&capture, legacy.id, Some(&fired_at), Some(&fired_at), true)
                .unwrap()
        );
    }
    push_event(
        &pilot.manager,
        pilot.leads[0],
        1,
        EventType::Message,
        Some(Role::Assistant),
        fired_at + chrono::Duration::seconds(1),
    )
    .await;
    let current = pilot
        .manager
        .store
        .lock()
        .await
        .get_scheduled_job(&legacy.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        pilot
            .manager
            .plan_terminal_watch_fire(&current)
            .await
            .unwrap(),
        WatchFirePlan::Confirmed,
        "only post-delivery provider output confirms a legacy manager watch"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn manager_watch_follows_committed_rotation_and_revalidates_scope() {
    let pilot = pilot().await;
    let job = notices(&pilot)
        .await
        .into_iter()
        .find(|job| job.wake_mode == WakeMode::OnTerminal(pilot.leads[0]))
        .unwrap();
    let current_owner = Uuid::new_v4();
    let current_lead = Uuid::new_v4();
    {
        let store = pilot.manager.store.lock().await;
        for (previous, current) in [(pilot.owner, current_owner), (pilot.leads[0], current_lead)] {
            let mut next = store.get_session(previous).unwrap().unwrap();
            next.id = current;
            next.continued_from = Some(previous);
            next.rotation_depth += 1;
            next.status = SessionStatus::Running;
            store.insert_session(&next).unwrap();
            store
                .update_session_status(previous, SessionStatus::Archived)
                .unwrap();
            store
                .record_harness_manager_rotation(previous, current)
                .unwrap();
        }
        store
            .set_lead_session(pilot.epics[0], Some(current_lead))
            .unwrap();
    }
    assert_eq!(
        pilot.manager.plan_terminal_watch_fire(&job).await.unwrap(),
        WatchFirePlan::NotReady,
        "the archived predecessor does not stand in for its active successor"
    );
    {
        let store = pilot.manager.store.lock().await;
        store
            .update_session_status(current_lead, SessionStatus::Completed)
            .unwrap();
        store.reconcile_harness_manager_watches().unwrap();
    }
    let WatchFirePlan::Deliver { tip, .. } =
        pilot.manager.plan_terminal_watch_fire(&job).await.unwrap()
    else {
        panic!("current lead completion should notify the current manager");
    };
    assert_eq!(tip, current_owner);
    pilot
        .manager
        .store
        .lock()
        .await
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: pilot.config.project_id,
            session_id: pilot.owner,
            epic_ids: Some(Vec::new()),
            expected_row_version: pilot.config.row_version,
        })
        .unwrap();
    assert!(matches!(
        pilot.manager.plan_terminal_watch_fire(&job).await.unwrap(),
        WatchFirePlan::Abandon(_)
    ));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn manager_watch_scheduler_disables_changed_envelope_without_fresh_launch() {
    use crate::issue_tracker::poller::{SessionLauncher, WatchFireOutcome};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct NoProvider(AtomicUsize);
    #[async_trait::async_trait]
    impl SessionLauncher for NoProvider {
        async fn launch(&self, _: crate::claude::LaunchConfig) -> crate::error::Result<Uuid> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(crate::error::DaemonError::Process(
                "provider must not launch".into(),
            ))
        }
        async fn fire_watch(&self, _: &ScheduledJob) -> crate::error::Result<WatchFireOutcome> {
            Ok(WatchFireOutcome::NotReady)
        }
    }

    let pilot = pilot().await;
    let job = notices(&pilot).await.remove(0);
    {
        let store = pilot.manager.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET wake_mode='agent_fresh' WHERE id=?1",
                [job.id.to_string()],
            )
            .unwrap();
    }
    let launcher = Arc::new(NoProvider(AtomicUsize::new(0)));
    let scheduler = crate::scheduler::spawn_scheduler(
        pilot.manager.store.clone(),
        pilot.manager.event_bus.clone(),
        launcher.clone(),
        60,
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if !pilot
                .manager
                .store
                .lock()
                .await
                .get_scheduled_job(&job.id)
                .unwrap()
                .unwrap()
                .enabled
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("changed manager notice must be disabled on the first scheduler pass");
    scheduler.shutdown().await.unwrap();
    assert_eq!(launcher.0.load(Ordering::SeqCst), 0);
    assert_eq!(
        pilot
            .manager
            .store
            .lock()
            .await
            .manager_progress(pilot.owner)
            .unwrap()
            .rows
            .len(),
        2
    );
}

// ---- Issue #627: recipient-idle delivery of durable manager notices ----

/// Settle the fixture's initial lead notices so each test starts from a
/// route with no pending subject.
async fn drain_manager_inbox(pilot: &Pilot) {
    pilot
        .manager
        .store
        .lock()
        .await
        .manager_inbox(pilot.owner, &AgentManagerInboxRequestV1::default())
        .unwrap();
}

async fn set_status(pilot: &Pilot, session: Uuid, status: SessionStatus) {
    pilot
        .manager
        .store
        .lock()
        .await
        .update_session_status(session, status)
        .unwrap();
}

async fn route_job(pilot: &Pilot, source: Uuid, target: Uuid) -> ScheduledJob {
    pilot
        .manager
        .store
        .lock()
        .await
        .list_scheduled_jobs()
        .unwrap()
        .into_iter()
        .find(|job| {
            job.wake_mode == WakeMode::OnTerminal(source) && job.wake_session_id == Some(target)
        })
        .unwrap()
}

async fn manager_request(pilot: &Pilot, key: &str) -> HarnessManagerMessageReceiptV1 {
    pilot
        .manager
        .agent_control()
        .agent_manager_send(
            pilot.owner,
            AgentManagerSendRequestV1 {
                epic_id: pilot.epics[0],
                message: format!("Status request {key}"),
                idempotency_key: key.into(),
                informational: false,
            },
        )
        .await
        .unwrap()
}

async fn lead_reply(pilot: &Pilot, request_id: Uuid, key: &str) -> HarnessManagerMessageReceiptV1 {
    pilot
        .manager
        .agent_control()
        .agent_manager_reply(
            pilot.leads[0],
            AgentManagerReplyRequestV1 {
                request_id,
                message: format!("SEALED {key}"),
                idempotency_key: key.into(),
                still_running: false,
            },
        )
        .await
        .unwrap()
}

/// Record the planner's delivery exactly as the scheduler does after a
/// successful manager-notice continuation.
async fn settle_delivery(pilot: &Pilot, job_id: Uuid) {
    let delivered = chrono::Utc::now();
    let store = pilot.manager.store.lock().await;
    let (_, capture) = store.capture_watch_fire(job_id).unwrap().unwrap();
    assert!(
        store
            .settle_watch_fire(&capture, job_id, Some(&delivered), Some(&delivered), true)
            .unwrap()
    );
}

async fn current_job(pilot: &Pilot, job_id: Uuid) -> ScheduledJob {
    pilot
        .manager
        .store
        .lock()
        .await
        .get_scheduled_job(&job_id)
        .unwrap()
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn running_lead_reply_wakes_idle_manager_once_until_inbox_retrieval() {
    let pilot = pilot().await;
    drain_manager_inbox(&pilot).await;
    set_status(&pilot, pilot.leads[0], SessionStatus::Running).await;
    let request = manager_request(&pilot, "status-1").await;

    let first = lead_reply(&pilot, request.message_id, "reply-1").await;
    let job = route_job(&pilot, pilot.owner, pilot.owner).await;
    let WatchFirePlan::Deliver {
        tip,
        message,
        job_ids,
        ..
    } = pilot.manager.plan_terminal_watch_fire(&job).await.unwrap()
    else {
        panic!("a running lead's reply must wake the idle manager");
    };
    assert_eq!(tip, pilot.owner);
    assert_eq!(job_ids, vec![job.id]);
    assert!(message.contains(&format!("message subject={}", first.message_id)));
    settle_delivery(&pilot, job.id).await;

    // A second report before retrieval rides the outstanding wake.
    let second = lead_reply(&pilot, request.message_id, "reply-2").await;
    let pending = current_job(&pilot, job.id).await;
    assert_eq!(
        pilot
            .manager
            .plan_terminal_watch_fire(&pending)
            .await
            .unwrap(),
        WatchFirePlan::NotReady,
        "an already-woken manager reads the second reply on its inbox pass"
    );
    assert!(pending.enabled);
    assert!(
        pending
            .message
            .contains(&format!("message subject={}", second.message_id))
    );

    let inbox = pilot
        .manager
        .store
        .lock()
        .await
        .manager_inbox(pilot.owner, &AgentManagerInboxRequestV1::default())
        .unwrap();
    for receipt in [&first, &second] {
        assert!(
            inbox
                .notices
                .iter()
                .any(|notice| notice.subject_id == receipt.message_id.to_string())
        );
    }

    let third = lead_reply(&pilot, request.message_id, "reply-3").await;
    let rearmed = current_job(&pilot, job.id).await;
    let WatchFirePlan::Deliver { tip, message, .. } = pilot
        .manager
        .plan_terminal_watch_fire(&rearmed)
        .await
        .unwrap()
    else {
        panic!("a reply after retrieval must wake the idle manager again");
    };
    assert_eq!(tip, pilot.owner);
    assert!(message.contains(&format!("message subject={}", third.message_id)));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn replies_from_two_live_epics_share_one_manager_event_wake() {
    let pilot = pilot().await;
    drain_manager_inbox(&pilot).await;
    for lead in pilot.leads {
        set_status(&pilot, lead, SessionStatus::Running).await;
    }
    let first_request = manager_request(&pilot, "coalesced-first").await;
    let first = lead_reply(&pilot, first_request.message_id, "coalesced-first-reply").await;
    let second_request = pilot
        .manager
        .agent_control()
        .agent_manager_send(
            pilot.owner,
            AgentManagerSendRequestV1 {
                epic_id: pilot.epics[1],
                message: "Second status request".into(),
                idempotency_key: "coalesced-second".into(),
                informational: false,
            },
        )
        .await
        .unwrap();
    let second = pilot
        .manager
        .agent_control()
        .agent_manager_reply(
            pilot.leads[1],
            AgentManagerReplyRequestV1 {
                request_id: second_request.message_id,
                message: "Second result".into(),
                idempotency_key: "coalesced-second-reply".into(),
                still_running: false,
            },
        )
        .await
        .unwrap();
    let store = pilot.manager.store.lock().await;
    let first_job = store
        .manager_notice_job_for_subject(
            "message",
            &first.message_id.to_string(),
            &first.sequence.to_string(),
        )
        .unwrap()
        .unwrap();
    let second_job = store
        .manager_notice_job_for_subject(
            "message",
            &second.message_id.to_string(),
            &second.sequence.to_string(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(first_job, second_job);
    let job = store.get_scheduled_job(&first_job).unwrap().unwrap();
    drop(store);
    let WatchFirePlan::Deliver {
        tip,
        message,
        job_ids,
        ..
    } = pilot.manager.plan_terminal_watch_fire(&job).await.unwrap()
    else {
        panic!("both live-Epic replies must share the idle manager wake");
    };
    assert_eq!(tip, pilot.owner);
    assert_eq!(job_ids, vec![first_job]);
    assert!(message.contains(&first.message_id.to_string()));
    assert!(message.contains(&second.message_id.to_string()));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn running_manager_holds_lead_reply_until_it_goes_idle() {
    let pilot = pilot().await;
    drain_manager_inbox(&pilot).await;
    set_status(&pilot, pilot.leads[0], SessionStatus::Running).await;
    let request = manager_request(&pilot, "status-busy").await;
    set_status(&pilot, pilot.owner, SessionStatus::Running).await;
    let reply = lead_reply(&pilot, request.message_id, "reply-busy").await;
    let job = route_job(&pilot, pilot.owner, pilot.owner).await;
    assert_eq!(
        pilot.manager.plan_terminal_watch_fire(&job).await.unwrap(),
        WatchFirePlan::NotReady
    );
    let held = current_job(&pilot, job.id).await;
    assert!(held.enabled);
    assert!(
        held.message
            .contains(&format!("message subject={}", reply.message_id))
    );

    set_status(&pilot, pilot.owner, SessionStatus::Completed).await;
    let WatchFirePlan::Deliver { tip, job_ids, .. } =
        pilot.manager.plan_terminal_watch_fire(&held).await.unwrap()
    else {
        panic!("the held report must fire once the manager is idle");
    };
    assert_eq!(tip, pilot.owner);
    assert_eq!(job_ids, vec![job.id]);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn reply_event_wake_follows_manager_rotation_and_refuses_revoked_scope() {
    let pilot = pilot().await;
    drain_manager_inbox(&pilot).await;
    set_status(&pilot, pilot.leads[0], SessionStatus::Running).await;
    let request = manager_request(&pilot, "rotate-reply").await;
    let reply = lead_reply(&pilot, request.message_id, "rotate-reply-result").await;
    let job = route_job(&pilot, pilot.owner, pilot.owner).await;
    let successor = Uuid::new_v4();
    {
        let store = pilot.manager.store.lock().await;
        let mut next = store.get_session(pilot.owner).unwrap().unwrap();
        next.id = successor;
        next.continued_from = Some(pilot.owner);
        next.rotation_depth += 1;
        next.status = SessionStatus::Completed;
        store.insert_session(&next).unwrap();
        store
            .update_session_status(pilot.owner, SessionStatus::Archived)
            .unwrap();
        store
            .record_harness_manager_rotation(pilot.owner, successor)
            .unwrap();
    }
    let WatchFirePlan::Deliver { tip, message, .. } =
        pilot.manager.plan_terminal_watch_fire(&job).await.unwrap()
    else {
        panic!("the pending reply must follow the current manager tip");
    };
    assert_eq!(tip, successor);
    assert!(message.contains(&reply.message_id.to_string()));
    pilot
        .manager
        .store
        .lock()
        .await
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: pilot.config.project_id,
            session_id: pilot.owner,
            epic_ids: Some(Vec::new()),
            expected_row_version: pilot.config.row_version,
        })
        .unwrap();
    assert!(matches!(
        pilot.manager.plan_terminal_watch_fire(&job).await.unwrap(),
        WatchFirePlan::Abandon(_)
    ));
    assert!(
        !pilot
            .manager
            .store
            .lock()
            .await
            .harness_manager_wake_authorized(job.id, successor)
            .unwrap()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn reply_event_wake_respects_operator_pause_at_delivery() {
    let pilot = pilot().await;
    drain_manager_inbox(&pilot).await;
    let request = manager_request(&pilot, "paused-reply").await;
    lead_reply(&pilot, request.message_id, "paused-reply-result").await;
    let job = route_job(&pilot, pilot.owner, pilot.owner).await;
    let store = pilot.manager.store.lock().await;
    assert!(
        store
            .harness_manager_wake_authorized(job.id, pilot.owner)
            .unwrap()
    );
    store
        .record_manager_operator_pause(pilot.owner, true)
        .unwrap();
    assert!(
        !store
            .harness_manager_wake_authorized(job.id, pilot.owner)
            .unwrap()
    );
    assert_eq!(store.manager_notice_undelivered_count(job.id).unwrap(), 1);
    store
        .record_manager_operator_pause(pilot.owner, false)
        .unwrap();
    assert!(
        store
            .harness_manager_wake_authorized(job.id, pilot.owner)
            .unwrap()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn manager_mail_wakes_idle_lead_while_manager_is_running() {
    let pilot = pilot().await;
    set_status(&pilot, pilot.owner, SessionStatus::Running).await;
    let request = manager_request(&pilot, "status-to-lead").await;
    let job = route_job(&pilot, pilot.owner, pilot.leads[0]).await;
    let WatchFirePlan::Deliver { tip, message, .. } =
        pilot.manager.plan_terminal_watch_fire(&job).await.unwrap()
    else {
        panic!("manager mail must wake the idle lead without waiting for the manager's turn");
    };
    assert_eq!(tip, pilot.leads[0]);
    assert!(message.starts_with("1 durable manager notices pending; read AgentManagerInbox"));
    assert!(message.contains(&format!("message subject={}", request.message_id)));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn gated_or_failed_recipient_keeps_manager_mail_pending() {
    for status in [SessionStatus::WaitingApproval, SessionStatus::Failed] {
        let pilot = pilot().await;
        set_status(&pilot, pilot.owner, SessionStatus::Running).await;
        let request = manager_request(&pilot, "status-gated").await;
        set_status(&pilot, pilot.leads[0], status).await;
        let job = route_job(&pilot, pilot.owner, pilot.leads[0]).await;
        assert_eq!(
            pilot.manager.plan_terminal_watch_fire(&job).await.unwrap(),
            WatchFirePlan::NotReady,
            "{status:?} recipient is not woken by the recipient-idle rule"
        );
        let armed = current_job(&pilot, job.id).await;
        assert!(armed.enabled, "{status:?}: transport stays armed");
        assert!(
            armed
                .message
                .contains(&format!("message subject={}", request.message_id)),
            "{status:?}: the exact mail subject stays pending"
        );
        let store = pilot.manager.store.lock().await;
        assert_eq!(store.manager_notice_undelivered_count(job.id).unwrap(), 1);
        assert_eq!(
            store.manager_watch_delivery_state(job.id).unwrap(),
            Some(true)
        );
    }
}

/// Issue #669 R1: while the appointed manager seat is Failed, a lead reply
/// whose source went terminal plans delivery, but the manager-notice resume
/// gate defers a non-Completed recipient. The exact subject stays pending and
/// undelivered (never abandoned or consumed) and delivers once the seat's
/// recovered turn completes.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn failed_manager_seat_keeps_lead_reply_pending_until_recovered() {
    let pilot = pilot().await;
    drain_manager_inbox(&pilot).await;
    let request = manager_request(&pilot, "seat-down").await;
    let reply = lead_reply(&pilot, request.message_id, "seat-down-reply").await;
    set_status(&pilot, pilot.leads[0], SessionStatus::Completed).await;
    set_status(&pilot, pilot.owner, SessionStatus::Failed).await;
    let job = route_job(&pilot, pilot.owner, pilot.owner).await;
    let outcome = pilot.manager.fire_terminal_watch(&job).await.unwrap();
    assert!(
        matches!(
            outcome,
            crate::issue_tracker::poller::WatchFireOutcome::NotReady
        ),
        "a Failed manager tip defers delivery"
    );
    let armed = current_job(&pilot, job.id).await;
    assert!(armed.enabled);
    assert!(
        armed
            .message
            .contains(&format!("message subject={}", reply.message_id))
    );
    {
        let store = pilot.manager.store.lock().await;
        assert_eq!(store.manager_notice_undelivered_count(job.id).unwrap(), 1);
        assert_eq!(
            store.manager_watch_delivery_state(job.id).unwrap(),
            Some(true)
        );
    }
    // Seat recovery resumed the tip in place and its turn completed.
    set_status(&pilot, pilot.owner, SessionStatus::Completed).await;
    let WatchFirePlan::Deliver { tip, message, .. } = pilot
        .manager
        .plan_terminal_watch_fire(&current_job(&pilot, job.id).await)
        .await
        .unwrap()
    else {
        panic!("the recovered seat receives the retained reply");
    };
    assert_eq!(tip, pilot.owner);
    assert!(message.contains(&format!("message subject={}", reply.message_id)));
}

/// Reserve a root succession that replaces `predecessor`, as `succeed_manager`
/// leaves it while it awaits the predecessor's settlement.
async fn reserve_succession(pilot: &Pilot, predecessor: Uuid) {
    let store = pilot.manager.store.lock().await;
    let stamp = crate::store::harness_manager_v2::now();
    store.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    store
        .conn
        .execute(
            "INSERT INTO manager_root_successions
             (operation_id,project_id,manager_session_id,predecessor_session_id,
              candidate_session_id,launch_attempt_id,model_invocation_id,scope_version,
              policy_version,authority_epoch,frozen_json,state,row_version,created_at,updated_at)
             VALUES(?1,?2,?3,?3,?4,?5,?6,1,1,1,'{}','reserved',1,?7,?7)",
            rusqlite::params![
                Uuid::new_v4().to_string(),
                pilot.config.project_id.to_string(),
                predecessor.to_string(),
                Uuid::new_v4().to_string(),
                Uuid::new_v4().to_string(),
                Uuid::new_v4().to_string(),
                stamp
            ],
        )
        .unwrap();
    store.conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
}

/// Issue #2463: a queued succession does not hold a notice that needs the
/// manager (here a lead reply). The predecessor is woken with it, so a real
/// event is never left unseen behind a handoff.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn non_routine_notice_wakes_manager_despite_queued_succession() {
    let pilot = pilot().await;
    drain_manager_inbox(&pilot).await;
    let request = manager_request(&pilot, "handoff-request").await;
    let reply = lead_reply(&pilot, request.message_id, "handoff-reply").await;
    set_status(&pilot, pilot.leads[0], SessionStatus::Completed).await;
    set_status(&pilot, pilot.owner, SessionStatus::Completed).await;
    let job = route_job(&pilot, pilot.owner, pilot.owner).await;
    reserve_succession(&pilot, pilot.owner).await;
    {
        let store = pilot.manager.store.lock().await;
        assert_eq!(
            store.manager_notice_undelivered_wake_count(job.id).unwrap(),
            1,
            "the reply is wake-worthy"
        );
        assert!(
            store
                .session_has_unresolved_manager_succession(pilot.owner)
                .unwrap()
        );
    }
    let WatchFirePlan::Deliver { tip, message, .. } = pilot
        .manager
        .plan_terminal_watch_fire(&current_job(&pilot, job.id).await)
        .await
        .unwrap()
    else {
        panic!("a wake-worthy notice is delivered while a succession is unresolved");
    };
    assert_eq!(tip, pilot.owner);
    assert!(message.contains(&format!("message subject={}", reply.message_id)));
}

/// Issue #2422: routine notices (a succeeded action result, a sandbox target
/// reclaim, a worker-state intent bump) never wake the manager through the
/// terminal-watch path, the same #2322 classification the inbox uses.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn routine_notices_never_wake_manager_through_terminal_watch() {
    let pilot = pilot().await;
    drain_manager_inbox(&pilot).await;
    let request = manager_request(&pilot, "routine-request").await;
    lead_reply(&pilot, request.message_id, "routine-reply").await;
    set_status(&pilot, pilot.leads[0], SessionStatus::Completed).await;
    set_status(&pilot, pilot.owner, SessionStatus::Completed).await;
    drain_manager_inbox(&pilot).await;
    let job = route_job(&pilot, pilot.owner, pilot.owner).await;
    {
        let store = pilot.manager.store.lock().await;
        let recorded = crate::store::harness_manager_v2::now();
        for (index, (kind, state)) in [
            ("action_result", r#"{"state":"succeeded"}"#),
            (
                "ledger_change",
                r#"{"record_kind":"sandbox_target_reclaim"}"#,
            ),
            (
                "ledger_change",
                r#"{"record_kind":"intent","payload":{"state":"monitoring"}}"#,
            ),
            (
                "ledger_change",
                r#"{"record_kind":"intent","payload":{"state":"worker"}}"#,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            store
                .conn
                .execute(
                    "INSERT INTO harness_manager_notices
                     (id,job_id,project_id,manager_session_id,scope_version,epic_id,direction,
                      source_session_id,recipient_session_id,kind,subject_id,subject_version,
                      state_json,recorded_at,queued_at)
                     VALUES(?1,?2,?3,?4,?5,NULL,'to_manager',NULL,?4,?6,?7,'1',?8,?9,?9)",
                    rusqlite::params![
                        Uuid::new_v4().to_string(),
                        job.id.to_string(),
                        pilot.config.project_id.to_string(),
                        pilot.owner.to_string(),
                        pilot.config.row_version,
                        kind,
                        format!("routine-{index}"),
                        state,
                        recorded
                    ],
                )
                .unwrap();
        }
        store.refresh_manager_notice_job(job.id).unwrap();
        assert_eq!(store.manager_notice_undelivered_count(job.id).unwrap(), 4);
        assert_eq!(
            store.manager_notice_undelivered_wake_count(job.id).unwrap(),
            0
        );
    }
    assert_eq!(
        pilot
            .manager
            .plan_terminal_watch_fire(&current_job(&pilot, job.id).await)
            .await
            .unwrap(),
        WatchFirePlan::NotReady,
        "routine-only notices never wake the manager"
    );
    // The routine gate also holds with a queued succession (#2463).
    reserve_succession(&pilot, pilot.owner).await;
    assert_eq!(
        pilot
            .manager
            .plan_terminal_watch_fire(&current_job(&pilot, job.id).await)
            .await
            .unwrap(),
        WatchFirePlan::NotReady,
        "routine-only notices never wake a handing-off manager"
    );
}

// ---- Issue #648: an abandoned, never-consumed delivery escalates ----
mod abandoned_delivery {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::significant_drop_tightening,
        clippy::too_many_lines,
        clippy::large_futures
    )]
    use super::*;

    /// Whole minutes the fixture's watched child has been terminal at give-up.
    const UNCONSUMED_MINUTES: i64 = 21;

    /// Arm an ordinary child watch on `tip` whose watched child ended past the
    /// give-up window and whose last delivery was never consumed.
    async fn unconsumed_watch_on(manager: &SessionManager, tip: Uuid) -> (ScheduledJob, Uuid) {
        let child = Uuid::new_v4();
        let mut stale_child = bare_session(child);
        stale_child.updated_at =
            chrono::Utc::now() - super::WATCH_DELIVERY_GIVE_UP_AFTER - chrono::Duration::minutes(1);
        insert_row(manager, &stale_child).await;
        let mut job = mk_watch_job_for(child, tip, "note");
        job.last_fired_at = Some(chrono::Utc::now());
        manager
            .store
            .lock()
            .await
            .insert_scheduled_job(&job)
            .unwrap();
        let job = manager
            .store
            .lock()
            .await
            .get_scheduled_job(&job.id)
            .unwrap()
            .unwrap();
        (job, child)
    }

    /// Fire `job` through the real scheduler settle path with the real
    /// `SessionManager` as launcher; return every bus event it published.
    async fn fire_through_scheduler(
        manager: &Arc<SessionManager>,
        job: &ScheduledJob,
    ) -> Vec<Arc<crate::bus::DaemonEvent>> {
        use crate::issue_tracker::poller::SessionLauncher;
        let mut events = manager.event_bus.subscribe();
        let launcher: Arc<dyn SessionLauncher> = manager.clone();
        crate::scheduler::fire_job_for_test(&manager.store, &manager.event_bus, &launcher, job)
            .await;
        let mut seen = Vec::new();
        while let Ok(event) = events.try_recv() {
            seen.push(event);
        }
        seen
    }

    fn abandon_warning(job: &ScheduledJob, tip: Uuid, watched: Uuid, minutes: i64) -> String {
        format!(
            "terminal watch '{}' (id={}) abandoned: delivery to {tip} was never consumed: no \
             provider output after {minutes} min of re-delivery attempts for watched child \
             {watched}. The notification is being dropped; resume {tip} manually to pick the \
             work back up",
            job.name, job.id
        )
    }

    /// #653: the give-up names the last refusal when every re-delivery was refused.
    fn refused_abandon_warning(
        job: &ScheduledJob,
        tip: Uuid,
        watched: Uuid,
        minutes: i64,
        code: &str,
        count: u32,
    ) -> String {
        format!(
            "terminal watch '{}' (id={}) abandoned: delivery to {tip} was refused: \
             continuation_refused:{code} ({count} refusals) over {minutes} min of re-delivery \
             attempts for watched child {watched}. The notification is being dropped; resume \
             {tip} manually to pick the work back up",
            job.name, job.id
        )
    }

    fn warnings(events: &[Arc<crate::bus::DaemonEvent>]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event.as_ref() {
                crate::bus::DaemonEvent::SystemMessage { level, message } if level == "warn" => {
                    Some(message.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// `(content, metadata)` of every `delivery_abandoned` health fact on `tip`.
    async fn health_facts(manager: &SessionManager, tip: Uuid) -> Vec<(String, serde_json::Value)> {
        let store = manager.store.lock().await;
        let mut statement = store
            .conn
            .prepare(
                "SELECT content,metadata FROM conversation_events
                 WHERE session_id=?1 AND event_type='System'
                   AND json_extract(metadata,'$.health_fact')='delivery_abandoned'
                 ORDER BY sequence",
            )
            .unwrap();
        statement
            .query_map([tip.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    serde_json::from_str(&row.get::<_, String>(1)?).unwrap(),
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    /// `(direction, source, state)` of every abandoned-delivery notice for `job`.
    async fn abandoned_notices(
        manager: &SessionManager,
        job: Uuid,
    ) -> Vec<(String, String, serde_json::Value)> {
        let store = manager.store.lock().await;
        let mut statement = store
            .conn
            .prepare(
                "SELECT direction,subject_id,state_json FROM harness_manager_notices
                 WHERE kind='session_state' AND subject_version=?1 ORDER BY sequence",
            )
            .unwrap();
        statement
            .query_map([format!("delivery_abandoned:{job}")], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    serde_json::from_str(&row.get::<_, String>(2)?).unwrap(),
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    async fn job_enabled(manager: &SessionManager, job: Uuid) -> bool {
        manager
            .store
            .lock()
            .await
            .get_scheduled_job(&job)
            .unwrap()
            .unwrap()
            .enabled
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn abandoned_delivery_to_managed_lead_records_health_fact_and_wakes_manager_once() {
        let Pilot {
            manager,
            _directory,
            owner,
            epics,
            leads,
            ..
        } = pilot().await;
        let manager = Arc::new(manager);
        manager
            .store
            .lock()
            .await
            .manager_inbox(owner, &AgentManagerInboxRequestV1::default())
            .unwrap();
        let lead = leads[0];
        let (job, child) = unconsumed_watch_on(&manager, lead).await;

        let events = fire_through_scheduler(&manager, &job).await;

        // Existing give-up behaviour: row retired and the same loud warning.
        // (a) One durable, transcript-visible health fact on the lead.
        let facts = health_facts(&manager, lead).await;
        assert_eq!(facts.len(), 1);
        let minutes = UNCONSUMED_MINUTES;

        // Existing give-up behaviour: row retired and the same loud warning.
        assert!(!job_enabled(&manager, job.id).await);
        assert_eq!(
            warnings(&events),
            vec![abandon_warning(&job, lead, child, minutes)]
        );

        let (content, metadata) = &facts[0];
        assert!(
            content.starts_with("[rsid-health] delivery_abandoned:"),
            "{content}"
        );
        assert!(content.contains(&job.id.to_string()), "{content}");
        assert_eq!(metadata["job_id"], job.id.to_string());
        assert_eq!(metadata["watched_session_id"], child.to_string());
        assert_eq!(metadata["minutes_unconsumed"], minutes);
        assert!(events.iter().any(|event| matches!(
            event.as_ref(),
            crate::bus::DaemonEvent::ConversationEvent { session_id, event }
                if *session_id == lead && event.content == *content
        )));

        // (b) One to_manager session_state notice, queued and published.
        let notices = abandoned_notices(&manager, job.id).await;
        assert_eq!(notices.len(), 1);
        let (direction, subject, state) = &notices[0];
        assert_eq!(direction, "to_manager");
        assert_eq!(subject, &lead.to_string());
        assert_eq!(state["lead_session_id"], lead.to_string());
        assert_eq!(state["epic_id"], epics[0].to_string());
        assert_eq!(state["watched_session_id"], child.to_string());
        assert_eq!(state["job_id"], job.id.to_string());
        assert_eq!(state["minutes_unconsumed"], minutes);
        assert_eq!(state["abandoned_at"], metadata["abandoned_at"]);
        let route = manager
            .store
            .lock()
            .await
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .find(|candidate| {
                candidate.wake_mode == WakeMode::OnTerminal(lead)
                    && candidate.wake_session_id == Some(owner)
            })
            .unwrap();
        assert!(events.iter().any(|event| matches!(
            event.as_ref(),
            crate::bus::DaemonEvent::ManagerNoticeQueued { job_id } if *job_id == route.id
        )));

        // The idle manager is woken with the exact subject.
        let WatchFirePlan::Deliver { tip, message, .. } =
            manager.plan_terminal_watch_fire(&route).await.unwrap()
        else {
            panic!("the abandoned-delivery notice must wake the idle manager");
        };
        assert_eq!(tip, owner);
        assert!(
            message.contains(&format!(
                "session_state subject={lead} version=delivery_abandoned:{}",
                job.id
            )),
            "{message}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn abandoned_delivery_to_non_lead_worker_records_health_fact_only() {
        let Pilot {
            manager,
            _directory,
            epics,
            ..
        } = pilot().await;
        let manager = Arc::new(manager);
        let worker = Uuid::new_v4();
        let mut row = bare_session(worker);
        row.parent_id = Some(epics[0]);
        row.project_id = manager
            .store
            .lock()
            .await
            .get_session(epics[0])
            .unwrap()
            .unwrap()
            .project_id;
        insert_row(&manager, &row).await;
        let (job, child) = unconsumed_watch_on(&manager, worker).await;

        let events = fire_through_scheduler(&manager, &job).await;

        assert!(!job_enabled(&manager, job.id).await);
        assert_eq!(
            warnings(&events),
            vec![abandon_warning(&job, worker, child, UNCONSUMED_MINUTES)]
        );
        let facts = health_facts(&manager, worker).await;
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].1["job_id"], job.id.to_string());
        assert_eq!(facts[0].1["minutes_unconsumed"], UNCONSUMED_MINUTES);
        assert_eq!(abandoned_notices(&manager, job.id).await.len(), 0);
    }

    /// Re-arm `job` exactly as a fresh delivery epoch of the same row.
    async fn rearm_unconsumed(manager: &SessionManager, job: Uuid) -> ScheduledJob {
        let store = manager.store.lock().await;
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET enabled=1,last_fired_at=?2,updated_at=?2 WHERE id=?1",
                rusqlite::params![job.to_string(), now],
            )
            .unwrap();
        store.get_scheduled_job(&job).unwrap().unwrap()
    }

    fn queued_notice_jobs(events: &[Arc<crate::bus::DaemonEvent>]) -> Vec<Uuid> {
        events
            .iter()
            .filter_map(|event| match event.as_ref() {
                crate::bus::DaemonEvent::ManagerNoticeQueued { job_id } => Some(*job_id),
                _ => None,
            })
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn replayed_abandonment_of_the_same_job_records_one_fact_and_one_notice() {
        let Pilot {
            manager,
            _directory,
            leads,
            ..
        } = pilot().await;
        let manager = Arc::new(manager);
        let lead = leads[0];
        let (job, _) = unconsumed_watch_on(&manager, lead).await;

        let first = fire_through_scheduler(&manager, &job).await;
        assert_eq!(queued_notice_jobs(&first).len(), 1);
        let replayed = rearm_unconsumed(&manager, job.id).await;
        let second = fire_through_scheduler(&manager, &replayed).await;

        assert!(!job_enabled(&manager, job.id).await);
        assert_eq!(
            warnings(&second),
            vec![abandon_warning(
                &job,
                lead,
                replayed_child(&replayed),
                UNCONSUMED_MINUTES
            )],
            "the replay keeps the historical give-up warning"
        );
        assert_eq!(queued_notice_jobs(&second), Vec::<Uuid>::new());
        assert_eq!(health_facts(&manager, lead).await.len(), 1);
        assert_eq!(abandoned_notices(&manager, job.id).await.len(), 1);
    }

    fn replayed_child(job: &ScheduledJob) -> Uuid {
        let WakeMode::OnTerminal(child) = job.wake_mode else {
            panic!("fixture arms terminal watches only");
        };
        child
    }

    /// A failure anywhere inside the atomic abandonment leaves the watch armed
    /// with neither fact nor notice; the next pass settles all three together.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn failure_between_retirement_and_notice_keeps_watch_armed_then_settles_atomically() {
        let Pilot {
            manager,
            _directory,
            owner,
            leads,
            ..
        } = pilot().await;
        let manager = Arc::new(manager);
        manager
            .store
            .lock()
            .await
            .manager_inbox(owner, &AgentManagerInboxRequestV1::default())
            .unwrap();
        let lead = leads[0];
        let (job, child) = unconsumed_watch_on(&manager, lead).await;

        for inject in [
            Store::fail_next_delivery_abandonment_after_retirement as fn(),
            Store::fail_next_delivery_abandonment_after_health_fact,
        ] {
            inject();
            let current = manager
                .store
                .lock()
                .await
                .get_scheduled_job(&job.id)
                .unwrap()
                .unwrap();
            let events = fire_through_scheduler(&manager, &current).await;
            assert!(
                job_enabled(&manager, job.id).await,
                "a rolled-back abandonment keeps the watch armed for retry"
            );
            assert!(
                current_next_fire(&manager, job.id).await > current.next_fire_at,
                "the failed settlement backs off one recurrence"
            );
            drop(events);
            assert_eq!(health_facts(&manager, lead).await.len(), 0);
            assert_eq!(abandoned_notices(&manager, job.id).await.len(), 0);
        }

        let current = manager
            .store
            .lock()
            .await
            .get_scheduled_job(&job.id)
            .unwrap()
            .unwrap();
        let events = fire_through_scheduler(&manager, &current).await;
        assert!(!job_enabled(&manager, job.id).await);
        assert_eq!(
            warnings(&events),
            vec![abandon_warning(&job, lead, child, UNCONSUMED_MINUTES)]
        );
        assert_eq!(health_facts(&manager, lead).await.len(), 1);
        let notices = abandoned_notices(&manager, job.id).await;
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].1, lead.to_string());
        assert_eq!(queued_notice_jobs(&events).len(), 1);
    }

    async fn current_next_fire(
        manager: &SessionManager,
        job: Uuid,
    ) -> chrono::DateTime<chrono::Utc> {
        manager
            .store
            .lock()
            .await
            .get_scheduled_job(&job)
            .unwrap()
            .unwrap()
            .next_fire_at
    }

    /// #530 interaction: a tip behind a retryable custody gate (Prepared
    /// target reclaim) is deferred, never retired, by the unconsumed-delivery
    /// give-up; once the gate clears the same watch settles atomically.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn custody_gated_tip_defers_give_up_then_settles_once_gate_clears() {
        use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
        use crate::store::target_reclaim_sweep::PrepareTargetReclaimIntentResult;
        use rsi_common::types::{SandboxCleanupState, SandboxKind};

        let (manager, directory) = manager();
        let manager = Arc::new(manager);
        let tip = Uuid::new_v4();
        let custody_id = Uuid::new_v4();
        let prepared = {
            let mut store = manager.store.lock().await;
            let mut row = bare_session(tip);
            row.working_dir = directory.path().join("repository");
            row.sandbox_kind = Some(SandboxKind::GitWorktree);
            row.sandbox_root = Some(directory.path().join("sandbox"));
            row.sandbox_branch = Some(format!("rsi/{tip}"));
            row.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
            store
                .insert_session_with_custody(
                    &row,
                    SessionCustodyBinding::New(NewCustodyRoot {
                        custody_id,
                        canonical_repo_dir: row.working_dir.to_string_lossy().into_owned(),
                        sandbox_root: directory
                            .path()
                            .join("sandbox")
                            .to_string_lossy()
                            .into_owned(),
                        sandbox_branch: format!("rsi/{tip}"),
                        repository_identity: "repo:k10d-custody-gate".into(),
                        source_commit: "a".repeat(40),
                        cause: CustodyCause::FreshLaunch,
                    }),
                )
                .unwrap();
            let PrepareTargetReclaimIntentResult::Active(intent) = store
                .prepare_target_reclaim_intent(custody_id, 1, 1, 1)
                .unwrap()
            else {
                panic!("fixture prepares an active reclaim intent");
            };
            intent
        };
        let (job, child) = unconsumed_watch_on(&manager, tip).await;

        assert!(matches!(
            manager.fire_terminal_watch(&job).await.unwrap(),
            crate::issue_tracker::poller::WatchFireOutcome::CustodyUnavailable
        ));
        fire_through_scheduler(&manager, &job).await;
        let deferred = manager
            .store
            .lock()
            .await
            .get_scheduled_job(&job.id)
            .unwrap()
            .unwrap();
        assert!(deferred.enabled, "a custody-gated watch stays armed");
        assert_eq!(deferred.last_fired_at, job.last_fired_at);
        assert!(deferred.next_fire_at > job.next_fire_at);
        assert_eq!(health_facts(&manager, tip).await.len(), 0);

        manager
            .store
            .lock()
            .await
            .mark_target_reclaim_staged(&prepared)
            .unwrap();
        let events = fire_through_scheduler(&manager, &deferred).await;
        assert!(!job_enabled(&manager, job.id).await);
        assert_eq!(
            warnings(&events),
            vec![abandon_warning(&job, tip, child, UNCONSUMED_MINUTES)]
        );
        let facts = health_facts(&manager, tip).await;
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].1["job_id"], job.id.to_string());
    }

    /// Legacy (pre-V117, notice-free) manager `to_lead` watch whose last
    /// delivery to the lead was never consumed, with its watched manager row
    /// terminal past the give-up window.
    async fn stamp_legacy_delivery(manager: &SessionManager, job: Uuid) -> ScheduledJob {
        let store = manager.store.lock().await;
        let fired_at = chrono::Utc::now();
        let (_, capture) = store.capture_watch_fire(job).unwrap().unwrap();
        assert!(
            store
                .settle_watch_fire(&capture, job, Some(&fired_at), Some(&fired_at), true)
                .unwrap()
        );
        store.get_scheduled_job(&job).unwrap().unwrap()
    }

    async fn notice_generation(manager: &SessionManager, job: Uuid) -> i64 {
        manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT notice_generation FROM harness_manager_watches WHERE job_id=?1",
                [job.to_string()],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// Review 8d113238: a capture made stale by a concurrent rearm or disable
    /// settles NOTHING; a later genuine give-up still records one of each.
    async fn stale_legacy_manager_capture_settles_nothing(disable: bool) {
        let Pilot {
            manager,
            _directory,
            owner,
            leads,
            config,
            ..
        } = pilot().await;
        let manager = Arc::new(manager);
        let lead = leads[0];
        let lead_row = manager
            .store
            .lock()
            .await
            .get_session(lead)
            .unwrap()
            .unwrap();
        let job = {
            let store = manager.store.lock().await;
            store
                .ensure_manager_watch(&config, &lead_row, false, "legacy-unconsumed", true)
                .unwrap();
            let job = store
                .list_scheduled_jobs()
                .unwrap()
                .into_iter()
                .find(|job| {
                    job.wake_session_id == Some(lead)
                        && job.wake_mode == WakeMode::OnTerminal(owner)
                })
                .unwrap();
            assert_eq!(store.manager_watch_delivery_state(job.id).unwrap(), None);
            let aged = (chrono::Utc::now()
                - super::WATCH_DELIVERY_GIVE_UP_AFTER
                - chrono::Duration::minutes(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            store
                .conn
                .execute(
                    "UPDATE sessions SET updated_at=?2 WHERE id=?1",
                    rusqlite::params![owner.to_string(), aged],
                )
                .unwrap();
            job
        };
        let planned = stamp_legacy_delivery(&manager, job.id).await;
        let WatchFirePlan::AbandonUnconsumed(delivery) =
            manager.plan_terminal_watch_fire(&planned).await.unwrap()
        else {
            panic!("the legacy manager watch is past its give-up bound");
        };
        assert_eq!(delivery.tip, lead);

        // Capture, then a concurrent writer changes the watch before settlement.
        let (captured, capture) = manager
            .store
            .lock()
            .await
            .capture_watch_fire(job.id)
            .unwrap()
            .unwrap();
        {
            let store = manager.store.lock().await;
            if disable {
                store
                    .conn
                    .execute(
                        "UPDATE scheduled_jobs SET enabled=0,updated_at=?2 WHERE id=?1",
                        rusqlite::params![
                            job.id.to_string(),
                            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                        ],
                    )
                    .unwrap();
            } else {
                store
                    .ensure_manager_watch(&config, &lead_row, false, "rearmed", true)
                    .unwrap();
            }
        }
        let written = manager
            .store
            .lock()
            .await
            .get_scheduled_job(&job.id)
            .unwrap()
            .unwrap();
        let written_generation = notice_generation(&manager, job.id).await;
        assert_eq!(written.enabled, !disable);

        let settled = manager
            .store
            .lock()
            .await
            .abandon_unconsumed_watch(&captured, &capture, &delivery, chrono::Utc::now(), 0)
            .unwrap();
        assert!(settled.is_none(), "a stale capture settles nothing");
        let kept = manager
            .store
            .lock()
            .await
            .get_scheduled_job(&job.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                kept.enabled,
                kept.last_fired_at,
                kept.next_fire_at,
                kept.updated_at
            ),
            (
                written.enabled,
                written.last_fired_at,
                written.next_fire_at,
                written.updated_at
            ),
            "the concurrent writer's row is kept exactly"
        );
        assert_eq!(
            notice_generation(&manager, job.id).await,
            written_generation
        );
        assert_eq!(health_facts(&manager, lead).await.len(), 0);
        assert_eq!(abandoned_notices(&manager, job.id).await.len(), 0);

        // A genuine give-up on the (re)armed watch still records one of each.
        if disable {
            manager
                .store
                .lock()
                .await
                .ensure_manager_watch(&config, &lead_row, false, "rearmed", true)
                .unwrap();
        }
        let rearmed = stamp_legacy_delivery(&manager, job.id).await;
        let events = fire_through_scheduler(&manager, &rearmed).await;
        assert!(!job_enabled(&manager, job.id).await);
        assert_eq!(
            warnings(&events),
            vec![abandon_warning(&job, lead, owner, UNCONSUMED_MINUTES)]
        );
        let facts = health_facts(&manager, lead).await;
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].1["job_id"], job.id.to_string());
        let notices = abandoned_notices(&manager, job.id).await;
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].0, "to_manager");
        assert_eq!(notices[0].1, lead.to_string());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn rearm_after_capture_makes_legacy_manager_abandonment_stale() {
        stale_legacy_manager_capture_settles_nothing(false).await;
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn disable_after_capture_makes_legacy_manager_abandonment_stale() {
        stale_legacy_manager_capture_settles_nothing(true).await;
    }

    fn reopen(directory: &TempDir) -> Arc<SessionManager> {
        let config = Config::from_env();
        Arc::new(
            SessionManager::new(
                Arc::new(EventBus::new(64)),
                Store::open_test_fixture(&directory.path().join("rsi.db")).expect("reopen store"),
                false,
                directory.path().join("daemon.sock"),
                None,
                Vec::new(),
                RuntimeConfig::from_config(&config),
                directory.path().join("sandboxes"),
            )
            .expect("reopened SessionManager"),
        )
    }

    /// The #648 incident: after a daemon restart every continuation of the
    /// lead is refused (`sandbox_custody:cleanup_failed`) before any provider
    /// launch, so the delivery is never consumed. At the give-up the lead
    /// carries one health fact and the idle manager receives one notice.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn restart_with_refused_lead_deliveries_notifies_manager_at_give_up() {
        let Pilot {
            manager,
            _directory: directory,
            owner,
            epics,
            leads,
            ..
        } = pilot().await;
        let lead = leads[0];
        let child = Uuid::new_v4();
        let job = {
            let store = manager.store.lock().await;
            store
                .manager_inbox(owner, &AgentManagerInboxRequestV1::default())
                .unwrap();
            // The lead's sandbox custody is unusable across the restart.
            store
                .conn
                .execute(
                    "UPDATE sessions SET sandbox_cleanup_state='Failed' WHERE id=?1",
                    [lead.to_string()],
                )
                .unwrap();
            let mut child_row = bare_session(child);
            child_row.status = SessionStatus::Failed;
            store.insert_session(&child_row).unwrap();
            // Pre-restart daemon dispatched a delivery (production stamp).
            let job = mk_watch_job_for(child, lead, "note");
            store.insert_scheduled_job(&job).unwrap();
            let armed = store.get_scheduled_job(&job.id).unwrap().unwrap();
            let delivered = chrono::Utc::now() - chrono::Duration::minutes(20);
            assert!(
                store
                    .stamp_delivered_child_watch(job.id, &armed.updated_at, &delivered, &delivered)
                    .unwrap()
            );
            job
        };
        drop(manager);

        let manager = reopen(&directory);
        manager.restore_sessions().await.unwrap();
        let refusal = manager
            .continue_session(lead, "probe".into())
            .await
            .expect_err("the lead's continuation must be refused");
        assert!(refusal.to_string().contains("cleanup_failed"), "{refusal}");
        let age_child = |minutes: i64| {
            let manager = manager.clone();
            async move {
                let at = (chrono::Utc::now() - chrono::Duration::minutes(minutes))
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
                manager
                    .store
                    .lock()
                    .await
                    .conn
                    .execute(
                        "UPDATE sessions SET updated_at=?2 WHERE id=?1",
                        rusqlite::params![child.to_string(), at],
                    )
                    .unwrap();
            }
        };
        let current = |manager: Arc<SessionManager>| async move {
            manager
                .store
                .lock()
                .await
                .get_scheduled_job(&job.id)
                .unwrap()
                .unwrap()
        };

        // Inside the window: the re-delivery is refused and stays armed.
        age_child(19).await;
        let before = current(manager.clone()).await;
        fire_through_scheduler(&manager, &before).await;
        let deferred = current(manager.clone()).await;
        assert!(deferred.enabled, "a refused delivery stays armed");
        assert!(deferred.next_fire_at > before.next_fire_at);
        assert_eq!(deferred.last_fired_at, before.last_fired_at);
        let invocations: Vec<String> = {
            let store = manager.store.lock().await;
            let mut statement = store
                .conn
                .prepare(
                    "SELECT purpose||'/'||trigger_source FROM model_invocations
                     WHERE session_id=?1 ORDER BY created_at",
                )
                .unwrap();
            statement
                .query_map([lead.to_string()], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(
            invocations,
            vec!["session.launch.fresh/legacy_backfill".to_string()],
            "the refused re-deliveries never reached model admission"
        );

        // Past the window: one atomic give-up, naming the refusal (#653).
        age_child(UNCONSUMED_MINUTES).await;
        let events = fire_through_scheduler(&manager, &deferred).await;
        assert!(!job_enabled(&manager, job.id).await);
        assert_eq!(
            warnings(&events),
            vec![refused_abandon_warning(
                &job,
                lead,
                child,
                UNCONSUMED_MINUTES,
                "sandbox_custody:cleanup_failed",
                1
            )]
        );
        let facts = health_facts(&manager, lead).await;
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].1["job_id"], job.id.to_string());
        assert_eq!(facts[0].1["watched_session_id"], child.to_string());
        let notices = abandoned_notices(&manager, job.id).await;
        assert_eq!(notices.len(), 1);
        let (direction, subject, state) = &notices[0];
        assert_eq!(direction, "to_manager");
        assert_eq!(subject, &lead.to_string());
        assert_eq!(state["epic_id"], epics[0].to_string());
        assert_eq!(state["job_id"], job.id.to_string());

        let route = manager
            .store
            .lock()
            .await
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .find(|candidate| {
                candidate.wake_mode == WakeMode::OnTerminal(lead)
                    && candidate.wake_session_id == Some(owner)
            })
            .unwrap();
        assert_eq!(queued_notice_jobs(&events), vec![route.id]);
        let WatchFirePlan::Deliver { tip, message, .. } =
            manager.plan_terminal_watch_fire(&route).await.unwrap()
        else {
            panic!("the reopened daemon must wake the idle manager");
        };
        assert_eq!(tip, owner);
        assert!(
            message.contains(&format!(
                "session_state subject={lead} version=delivery_abandoned:{}",
                job.id
            )),
            "{message}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn abandoned_delivery_without_appointed_manager_records_health_fact_only() {
        let (manager, _directory) = manager();
        let project = Project {
            id: Uuid::new_v4(),
            name: "Unmanaged".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let lead = Uuid::new_v4();
        {
            let store = manager.store.lock().await;
            store.insert_project(&project).unwrap();
            let mut epic = bare_session(Uuid::new_v4());
            epic.project_id = Some(project.id);
            epic.session_kind = SessionKind::Epic;
            store.insert_session(&epic).unwrap();
            let mut lead_row = bare_session(lead);
            lead_row.project_id = Some(project.id);
            lead_row.parent_id = Some(epic.id);
            lead_row.session_kind = SessionKind::Feature;
            store.insert_session(&lead_row).unwrap();
            store.set_lead_session(epic.id, Some(lead)).unwrap();
        }
        let manager = Arc::new(manager);
        let (job, _) = unconsumed_watch_on(&manager, lead).await;

        let events = fire_through_scheduler(&manager, &job).await;

        assert!(!job_enabled(&manager, job.id).await);
        assert_eq!(warnings(&events).len(), 1);
        let facts = health_facts(&manager, lead).await;
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].1["job_id"], job.id.to_string());
        assert_eq!(abandoned_notices(&manager, job.id).await.len(), 0);
    }
}

/// #669 round 2 (v1_seat_unsignaled): a Failed V1-appointed manager without
/// the V2 policy opt-in is still observed: durable Down record, operator
/// `[manager-seat]` SystemMessage, and the lead-visible `manager_seat`.
/// Automatic recovery stays disabled.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn v1_appointed_failed_seat_is_signalled_without_v2_policy() {
    let pilot = pilot().await;
    let mut events = pilot.manager.event_bus.subscribe();
    let request = manager_request(&pilot, "v1-seat").await;
    set_status(&pilot, pilot.owner, SessionStatus::Failed).await;
    pilot
        .manager
        .reconcile_harness_managers_once()
        .await
        .unwrap();
    let mut seat_alerts = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let DaemonEvent::SystemMessage { level, message } = event.as_ref()
            && message.starts_with("[manager-seat]")
        {
            seat_alerts.push((level.clone(), message.clone()));
        }
    }
    assert!(
        seat_alerts
            .iter()
            .any(|(level, message)| level == "error" && message.contains("seat down")),
        "{seat_alerts:?}"
    );
    let inbox = pilot
        .manager
        .store
        .lock()
        .await
        .manager_inbox(pilot.leads[0], &AgentManagerInboxRequestV1::default())
        .unwrap();
    let seat = inbox.manager_seat.unwrap();
    assert_eq!(seat.state, ManagerSeatConditionV1::Down);
    assert_eq!(seat.reason, "manager_seat_policy_absent");
    assert_eq!(seat.tip_session_id, pilot.owner);
    assert_eq!(seat.max_attempts, 0);
    let reply = lead_reply(&pilot, request.message_id, "v1-seat-reply").await;
    assert_eq!(
        reply.manager_seat.map(|seat| seat.state),
        Some(ManagerSeatConditionV1::Down)
    );
    let recovery_rows: i64 = pilot
        .manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_v2_operations WHERE kind='seat_recovery'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(recovery_rows, 0);
}

/// One V1 appointment (no V2 policy) with a single Epic and lead. Returns
/// `(owner, epic, lead)`.
async fn v1_project(manager: &SessionManager, project_id: Uuid) -> (Uuid, Uuid, Uuid) {
    let store = manager.store.lock().await;
    store
        .insert_project(&Project {
            id: project_id,
            name: format!("V1 seat {project_id}"),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .unwrap();
    let mut owner = bare_session(Uuid::new_v4());
    owner.project_id = Some(project_id);
    store.insert_session(&owner).unwrap();
    let mut group = owner.clone();
    group.id = Uuid::new_v4();
    group.session_kind = SessionKind::Group;
    store.insert_session(&group).unwrap();
    let mut epic = owner.clone();
    epic.id = Uuid::new_v4();
    epic.session_kind = SessionKind::Epic;
    epic.parent_id = Some(group.id);
    store.insert_session(&epic).unwrap();
    let mut lead = owner.clone();
    lead.id = Uuid::new_v4();
    lead.parent_id = Some(epic.id);
    lead.session_kind = SessionKind::Feature;
    store.insert_session(&lead).unwrap();
    store.set_lead_session(epic.id, Some(lead.id)).unwrap();
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id,
            session_id: owner.id,
            epic_ids: Some(vec![epic.id]),
            expected_row_version: 0,
        })
        .unwrap();
    (owner.id, epic.id, lead.id)
}

/// #669 round 3 (v1_seat_unsignaled_after_32): with more V1 appointments
/// than one coordinator page, the wrapping cursor still reaches the last
/// project within ceil(n / page) passes: Down record, operator alert and the
/// lead-visible seat.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn v1_failed_seat_beyond_first_page_is_signalled_within_bounded_passes() {
    let (manager, _directory) = manager();
    let page = crate::session::manager_coordinator::MANAGER_SEAT_V1_PAGE;
    let total = page + 1;
    let mut last = None;
    for index in 1..=total {
        // Ascending ids: the Failed seat is the last project in scan order.
        let id = Uuid::parse_str(&format!("00000000-0000-4000-8000-{index:012}")).unwrap();
        last = Some((id, v1_project(&manager, id).await));
    }
    let (project, (owner, _epic, lead)) = last.unwrap();
    manager
        .store
        .lock()
        .await
        .update_session_status(owner, SessionStatus::Failed)
        .unwrap();
    let mut events = manager.event_bus.subscribe();
    let passes = total.div_ceil(page);
    for _ in 0..passes {
        manager.reconcile_harness_managers_once().await.unwrap();
    }
    let mut seat_alerts = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let DaemonEvent::SystemMessage { level, message } = event.as_ref()
            && message.starts_with("[manager-seat]")
        {
            seat_alerts.push((level.clone(), message.clone()));
        }
    }
    assert!(
        seat_alerts.iter().any(|(level, message)| level == "error"
            && message.contains(&project.to_string())
            && message.contains("seat down")),
        "{seat_alerts:?}"
    );
    let store = manager.store.lock().await;
    let config = store.get_harness_manager(project).unwrap().unwrap();
    let seat = store.manager_seat_state(&config).unwrap().unwrap();
    assert_eq!(
        (seat.state, seat.tip_session_id, seat.reason.as_str()),
        (
            ManagerSeatConditionV1::Down,
            owner,
            "manager_seat_policy_absent"
        )
    );
    let inbox = store
        .manager_inbox(lead, &AgentManagerInboxRequestV1::default())
        .unwrap();
    assert_eq!(
        inbox
            .manager_seat
            .map(|seat| (seat.state, seat.tip_session_id)),
        Some((ManagerSeatConditionV1::Down, owner))
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn enqueue_supersedes_only_owned_descendants_and_refuses_a_claim_during_probe() {
    use rsi_common::rolling_queue::{AgentEnqueueLandingSourceRequestV1, RollingQueueEntryState};
    let pilot = pilot().await;
    let repo = crate::test_support::disk_backed_tempdir("enqueue-supersedes");
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    git(&["init", "-q", "-b", "work"]);
    git(&["config", "user.email", "test@example.invalid"]);
    git(&["config", "user.name", "test"]);
    let mut commits = Vec::new();
    for message in ["one", "two", "three", "four"] {
        git(&["commit", "-q", "--allow-empty", "-m", message]);
        commits.push(git(&["rev-parse", "HEAD"]));
    }
    {
        let store = pilot.manager.store.lock().await;
        for caller in [pilot.owner, pilot.leads[0]] {
            store
                .conn
                .execute(
                    "UPDATE sessions SET working_dir=?2 WHERE id=?1",
                    rusqlite::params![caller.to_string(), repo.path().display().to_string()],
                )
                .unwrap();
        }
    }
    let request =
        |index: usize, key: &str, supersedes: Option<Uuid>| AgentEnqueueLandingSourceRequestV1 {
            supersedes,
            source_commit: commits[index].clone(),
            idempotency_key: key.into(),
            derive_filters: false,
            fixes_issue: None,
            fixes_tests: Vec::new(),
            test_filters: Vec::new(),
            project_id: None,
            source_session_id: None,
        };
    let control = pilot.manager.agent_control();
    let old = control
        .agent_enqueue_landing_source(pilot.owner, request(1, "old", None), true)
        .await
        .unwrap()
        .entry;
    let error = control
        .agent_enqueue_landing_source(pilot.owner, request(0, "backwards", Some(old.id)), true)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("queue_supersedes_not_descendant"),
        "{error}"
    );
    let error = control
        .agent_enqueue_landing_source(pilot.leads[0], request(2, "foreign", Some(old.id)), true)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("queue_entry_not_found"),
        "{error}"
    );
    let replacement = control
        .agent_enqueue_landing_source(pilot.owner, request(2, "replacement", Some(old.id)), true)
        .await
        .unwrap();
    assert_eq!(replacement.entry.sequence, old.sequence);
    let replay = control
        .agent_enqueue_landing_source(pilot.owner, request(2, "replacement", Some(old.id)), true)
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.entry.id, replacement.entry.id);
    let error = control
        .agent_enqueue_landing_source(pilot.owner, request(3, "terminal", Some(old.id)), true)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("queue_entry_settled"), "{error}");

    // Claim wins after the ancestry check, before the enqueue transaction.
    crate::session::effect_fence::seam::install(pilot.owner, "enqueue", |store| {
        store
            .claim_next_rolling_queue_entry(chrono::Utc::now())
            .unwrap()
            .unwrap();
    });
    let error = control
        .agent_enqueue_landing_source(
            pilot.owner,
            request(3, "raced", Some(replacement.entry.id)),
            true,
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("queue_entry_in_flight"),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .contains("enqueue normally without supersedes"),
        "{error}"
    );
    let error = control
        .agent_enqueue_landing_source(
            pilot.owner,
            request(3, "in-flight", Some(replacement.entry.id)),
            true,
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("queue_entry_in_flight"),
        "{error}"
    );
    let normal = control
        .agent_enqueue_landing_source(pilot.owner, request(3, "normal", None), true)
        .await
        .unwrap();
    assert!(normal.entry.sequence > replacement.entry.sequence);
    let store = pilot.manager.store.lock().await;
    assert_eq!(
        store
            .get_rolling_queue_entry(replacement.entry.id)
            .unwrap()
            .unwrap()
            .state,
        RollingQueueEntryState::Gating
    );
    assert!(
        store
            .get_rolling_queue_entry_by_key(pilot.owner, "raced")
            .unwrap()
            .is_none()
    );
}

/// #2993: a repository whose `rolling` ref is the base of one `feature`
/// commit and whose filter checker waits for `release` to exist before it
/// prints `body_after_release`; the pilot's manager and lead land from it.
/// Returns the repository, the source commit and the release path.
async fn deriving_fixture(
    pilot: &Pilot,
    name: &str,
    after_release: &str,
) -> (TempDir, String, std::path::PathBuf) {
    let repo = TempDir::new().unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{name}: git {args:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    git(&["init", "-q", "-b", "work"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    git(&["commit", "-q", "--allow-empty", "-m", "base"]);
    git(&["branch", "rolling"]);
    git(&["commit", "-q", "--allow-empty", "-m", "feature"]);
    let source = git(&["rev-parse", "HEAD"]);
    let release = repo.path().join("release");
    std::fs::create_dir_all(repo.path().join("scripts")).unwrap();
    std::fs::write(
        repo.path().join("scripts/check-touched-shards"),
        format!(
            "import json, os, sys, time\nfor _ in range(600):\n    if os.path.exists({release:?}):\n        break\n    time.sleep(0.1)\n{after_release}\n",
            release = release.display().to_string()
        ),
    )
    .unwrap();
    {
        let store = pilot.manager.store.lock().await;
        for caller in [pilot.owner, pilot.leads[0]] {
            store
                .conn
                .execute(
                    "UPDATE sessions SET working_dir=?2 WHERE id=?1",
                    rusqlite::params![caller.to_string(), repo.path().display().to_string()],
                )
                .unwrap();
        }
    }
    (repo, source, release)
}

fn derive_request(
    source: &str,
    key: &str,
) -> rsi_common::rolling_queue::AgentEnqueueLandingSourceRequestV1 {
    rsi_common::rolling_queue::AgentEnqueueLandingSourceRequestV1 {
        supersedes: None,
        derive_filters: true,
        fixes_issue: None,
        fixes_tests: Vec::new(),
        project_id: None,
        source_session_id: None,
        source_commit: source.to_string(),
        test_filters: Vec::new(),
        idempotency_key: key.into(),
    }
}

/// The queue wakes (`merge-queue-*`) aimed at `target`.
async fn queue_wakes_for(pilot: &Pilot, target: Uuid) -> usize {
    pilot
        .manager
        .store
        .lock()
        .await
        .list_scheduled_jobs()
        .unwrap()
        .iter()
        .filter(|job| job.name.starts_with("merge-queue-") && job.wake_session_id == Some(target))
        .count()
}

/// Wait (bounded) until `entry`'s filters are no longer pending.
async fn filters_settled(
    pilot: &Pilot,
    entry: Uuid,
) -> rsi_common::rolling_queue::RollingQueueEntryV1 {
    for _ in 0..300 {
        let row = pilot
            .manager
            .store
            .lock()
            .await
            .get_rolling_queue_entry(entry)
            .unwrap()
            .unwrap();
        if row.filters_state.is_none() {
            return row;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the derivation never finished the entry");
}

/// #2993: a derivation that outlasts the call's budget records the entry
/// `queued` with its filters pending and returns it; the lander admits it only
/// after its filters land; a replay returns the same entry; and the enqueuer is
/// woken exactly once, when the landing settles.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::significant_drop_tightening)]
async fn a_derivation_outlasting_the_call_records_a_pending_entry_admitted_after_its_filters_land_2993()
 {
    use rsi_common::rolling_queue::{
        QueueFiltersState, RollingQueueEntryState, RollingQueueOutcome,
    };
    let pilot = pilot().await;
    let (_repo, source, release) = deriving_fixture(
        &pilot,
        "pending",
        "print(json.dumps({'suggested_filters': ['rsid=rolling_queue']}))",
    )
    .await;
    let control = pilot.manager.agent_control();
    let started = std::time::Instant::now();
    let receipt = control
        .agent_enqueue_landing_source_within(
            pilot.owner,
            derive_request(&source, "derive-1"),
            true,
            std::time::Duration::from_millis(300),
        )
        .await
        .expect("a derivation still running at the budget is not a refusal");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(4),
        "the call returned at its budget: {:?}",
        started.elapsed()
    );
    let entry = receipt.entry;
    assert!(!receipt.replayed);
    assert_eq!(entry.state, RollingQueueEntryState::Queued);
    assert_eq!(entry.filters_state, Some(QueueFiltersState::Deriving));
    assert!(entry.test_filters.is_empty());

    // The lander admits nothing while the filters are pending.
    {
        let store = pilot.manager.store.lock().await;
        assert!(
            store
                .claim_rolling_queue_batch(chrono::Utc::now(), 4)
                .unwrap()
                .is_none(),
            "an entry whose filters are pending is not admitted"
        );
    }
    // A replay under the same key returns the same entry, never a second one.
    let replay = control
        .agent_enqueue_landing_source_within(
            pilot.owner,
            derive_request(&source, "derive-1"),
            true,
            std::time::Duration::from_millis(300),
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.entry.id, entry.id);
    assert_eq!(queue_wakes_for(&pilot, pilot.owner).await, 0);

    // The derivation ends: the filters land and only then is the entry admitted.
    std::fs::write(&release, "go").unwrap();
    let settled = filters_settled(&pilot, entry.id).await;
    assert_eq!(settled.test_filters, vec!["rsid=rolling_queue".to_string()]);
    assert_eq!(settled.state, RollingQueueEntryState::Queued);
    let claimed = {
        let store = pilot.manager.store.lock().await;
        let batch = store
            .claim_rolling_queue_batch(chrono::Utc::now(), 4)
            .unwrap()
            .expect("admitted once its filters landed");
        assert_eq!(batch.entries.len(), 1);
        assert_eq!(batch.entries[0].id, entry.id);
        assert_eq!(batch.entries[0].test_filters, settled.test_filters);
        assert!(batch.entries[0].filters_state.is_none());
        batch
    };
    assert_eq!(
        queue_wakes_for(&pilot, pilot.owner).await,
        0,
        "no wake before the outcome"
    );
    let wake = pilot
        .manager
        .store
        .lock()
        .await
        .settle_rolling_queue_entry(
            claimed.entries[0].id,
            RollingQueueEntryState::Published,
            &RollingQueueOutcome {
                landed_sha: Some(source.clone()),
                ..RollingQueueOutcome::default()
            },
            chrono::Utc::now(),
        )
        .unwrap();
    assert!(wake.is_some());
    assert_eq!(
        queue_wakes_for(&pilot, pilot.owner).await,
        1,
        "exactly one outcome wake"
    );
    // The key still replays the one entry after it settled.
    let after = control
        .agent_enqueue_landing_source_within(
            pilot.owner,
            derive_request(&source, "derive-1"),
            true,
            std::time::Duration::from_millis(300),
        )
        .await
        .unwrap();
    assert!(after.replayed);
    assert_eq!(after.entry.id, entry.id);
    assert_eq!(queue_wakes_for(&pilot, pilot.owner).await, 1);
}

/// #3002: a `derive_filters` replay answers with the existing entry once its
/// filters settled, even when rolling advanced and derivation would now give
/// other filters; derivation is not asked again. A different source commit or
/// different fix claims under the same key is still `queue_idempotency_conflict`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::significant_drop_tightening)]
async fn a_derive_replay_after_the_filters_settle_returns_the_entry_not_a_conflict_3002() {
    use rsi_common::rolling_queue::RollingQueueEntryState;
    let pilot = pilot().await;
    let (repo, source, release) = deriving_fixture(
        &pilot,
        "settled-replay",
        "print(json.dumps({'suggested_filters': ['rsid=rolling_queue']}))",
    )
    .await;
    let control = pilot.manager.agent_control();
    let wait = std::time::Duration::from_millis(300);
    let entry = control
        .agent_enqueue_landing_source_within(
            pilot.owner,
            derive_request(&source, "derive-settled"),
            true,
            wait,
        )
        .await
        .unwrap()
        .entry;
    assert!(entry.filters_state.is_some());

    std::fs::write(&release, "go").unwrap();
    let settled = filters_settled(&pilot, entry.id).await;
    assert_eq!(settled.state, RollingQueueEntryState::Queued);
    assert_eq!(settled.test_filters, vec!["rsid=rolling_queue".to_string()]);

    // Rolling advances and the checker would now answer differently.
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    let tree = git(&["rev-parse", "rolling^{tree}"]);
    let advanced = git(&["commit-tree", &tree, "-p", "rolling", "-m", "advance"]);
    git(&["update-ref", "refs/heads/rolling", &advanced]);
    std::fs::write(
        repo.path().join("scripts/check-touched-shards"),
        "import json\nprint(json.dumps({'suggested_filters': ['rsid=other']}))\n",
    )
    .unwrap();

    let replay = control
        .agent_enqueue_landing_source_within(
            pilot.owner,
            derive_request(&source, "derive-settled"),
            true,
            wait,
        )
        .await
        .expect("a settled entry replays, never queue_idempotency_conflict");
    assert!(replay.replayed);
    assert_eq!(replay.entry.id, entry.id);
    assert_eq!(
        replay.entry.test_filters,
        vec!["rsid=rolling_queue".to_string()],
        "the entry keeps the filters it settled with"
    );

    // A different source commit under the same key is still a conflict.
    let other = git(&["commit-tree", &tree, "-p", &source, "-m", "other"]);
    let conflict = control
        .agent_enqueue_landing_source_within(
            pilot.owner,
            derive_request(&other, "derive-settled"),
            true,
            wait,
        )
        .await
        .expect_err("another source commit is not a replay");
    assert!(
        conflict.to_string().contains("queue_idempotency_conflict"),
        "{conflict}"
    );
    // So are different fix claims.
    let mut claims = derive_request(&source, "derive-settled");
    claims.fixes_tests = vec!["rsid::some::test".into()];
    let conflict = control
        .agent_enqueue_landing_source_within(pilot.owner, claims, true, wait)
        .await
        .expect_err("other fix claims are not a replay");
    assert!(
        conflict.to_string().contains("queue_idempotency_conflict"),
        "{conflict}"
    );
}

/// #2993: a derivation that fails after the entry was recorded settles it
/// `refused` with a typed reason and wakes the enqueuer once; nothing is admitted.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::significant_drop_tightening)]
async fn a_derivation_that_fails_after_the_entry_is_recorded_refuses_it_and_wakes_once_2993() {
    use rsi_common::rolling_queue::{
        QUEUE_FILTER_DERIVATION_FAILED, QueueFiltersState, RollingQueueEntryState,
    };
    let pilot = pilot().await;
    let (_repo, source, release) = deriving_fixture(&pilot, "failing", "sys.exit(3)").await;
    let control = pilot.manager.agent_control();
    let entry = control
        .agent_enqueue_landing_source_within(
            pilot.owner,
            derive_request(&source, "derive-fail"),
            true,
            std::time::Duration::from_millis(300),
        )
        .await
        .unwrap()
        .entry;
    assert_eq!(entry.filters_state, Some(QueueFiltersState::Deriving));
    assert_eq!(entry.state, RollingQueueEntryState::Queued);

    std::fs::write(&release, "go").unwrap();
    let mut refused = None;
    for _ in 0..300 {
        let row = pilot
            .manager
            .store
            .lock()
            .await
            .get_rolling_queue_entry(entry.id)
            .unwrap()
            .unwrap();
        if row.state.is_terminal() {
            refused = Some(row);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let refused = refused.expect("the failed derivation settled the entry");
    assert_eq!(refused.state, RollingQueueEntryState::Refused);
    assert_eq!(
        refused
            .outcome
            .as_ref()
            .and_then(|outcome| outcome.refusal.as_deref()),
        Some(QUEUE_FILTER_DERIVATION_FAILED)
    );
    assert!(
        pilot
            .manager
            .store
            .lock()
            .await
            .claim_rolling_queue_batch(chrono::Utc::now(), 4)
            .unwrap()
            .is_none(),
        "a refused entry is never admitted"
    );
    assert_eq!(queue_wakes_for(&pilot, pilot.owner).await, 1);
    // The sweep and a replay find nothing left to settle: still one wake, one entry.
    crate::rolling_queue::resume_pending_filter_derivations(&pilot.manager.store).await;
    let replay = control
        .agent_enqueue_landing_source_within(
            pilot.owner,
            derive_request(&source, "derive-fail"),
            true,
            std::time::Duration::from_millis(300),
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.entry.id, entry.id);
    assert_eq!(replay.entry.state, RollingQueueEntryState::Refused);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(queue_wakes_for(&pilot, pilot.owner).await, 1);
}

/// #2993: an entry recorded with its filters pending survives a daemon restart:
/// the queue loop's sweep re-attaches the derivation and finishes it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::significant_drop_tightening)]
async fn the_queue_sweep_finishes_a_pending_entry_no_task_is_finishing_2993() {
    use crate::store::rolling_queue::NewQueueEntry;
    use rsi_common::rolling_queue::{QueueFiltersState, RollingQueueBinding};
    let pilot = pilot().await;
    let (repo, source, release) = deriving_fixture(
        &pilot,
        "sweep",
        "print(json.dumps({'suggested_filters': ['rsid=rolling_queue']}))",
    )
    .await;
    // The state a restart leaves: the entry is recorded, no task derives it.
    let entry = {
        let store = pilot.manager.store.lock().await;
        let project = store.get_session(pilot.owner).unwrap().unwrap().project_id;
        let (entry, _) = store
            .enqueue_rolling_queue_source_deriving(
                &NewQueueEntry {
                    project_id: project,
                    repo_path: repo.path().display().to_string(),
                    source_commit: source.clone(),
                    source_session_id: pilot.owner,
                    owner_epic_id: None,
                    binding: RollingQueueBinding::Unbound,
                    work_key: None,
                    migration_version: None,
                    hot_files: Vec::new(),
                    test_filters: Vec::new(),
                    fixes_tests: Vec::new(),
                    idempotency_key: "restart".into(),
                },
                Some(pilot.owner),
                None,
                chrono::Utc::now(),
            )
            .unwrap();
        assert_eq!(entry.filters_state, Some(QueueFiltersState::Deriving));
        assert_eq!(
            store
                .rolling_queue_entries_deriving_filters()
                .unwrap()
                .len(),
            1
        );
        entry
    };
    std::fs::write(&release, "go").unwrap();
    crate::rolling_queue::resume_pending_filter_derivations(&pilot.manager.store).await;
    let settled = filters_settled(&pilot, entry.id).await;
    assert_eq!(settled.test_filters, vec!["rsid=rolling_queue".to_string()]);
    assert!(
        pilot
            .manager
            .store
            .lock()
            .await
            .rolling_queue_entries_deriving_filters()
            .unwrap()
            .is_empty()
    );
}

/// #2993: a rolling-tip read that never finishes is genuine git trouble, not a
/// derivation in progress: the call is refused `queue_tip_read_timeout`, records
/// no entry, and the refusal is recorded as friction like any other.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::significant_drop_tightening)]
async fn a_stalled_tip_read_is_refused_with_its_own_code_and_recorded_as_friction_2993() {
    let pilot = pilot().await;
    let (repo, source, _release) = deriving_fixture(&pilot, "tip", "print('{}')").await;
    crate::rolling_queue::stall_git(repo.path(), "rev-parse");
    let control = pilot.manager.agent_control();
    let started = std::time::Instant::now();
    let error = control
        .agent_enqueue_landing_source_within(
            pilot.owner,
            derive_request(&source, "derive-tip"),
            true,
            std::time::Duration::from_millis(500),
        )
        .await
        .expect_err("the tip read did not finish");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(
        error
            .to_string()
            .contains(rsi_common::rolling_queue::QUEUE_TIP_READ_TIMEOUT),
        "{error}"
    );
    let store = pilot.manager.store.lock().await;
    assert!(
        store
            .get_rolling_queue_entry_by_key(pilot.owner, "derive-tip")
            .unwrap()
            .is_none(),
        "nothing was recorded"
    );
    // Recorded the way the RPC layer records a tokened Agent verb's refusal.
    crate::friction::note_locked(
        &store,
        &crate::friction::agent_refusal_event(
            "AgentEnqueueLandingSource",
            Some(pilot.owner),
            &error,
        ),
    );
    let rollup = store
        .friction_rollup(
            &rsi_common::friction::ListFrictionRollupRequestV1::default(),
            chrono::Utc::now() + chrono::Duration::minutes(1),
        )
        .unwrap();
    assert!(
        rollup
            .rows
            .iter()
            .any(|row| row.signature
                == "agent_refusal:AgentEnqueueLandingSource:queue_tip_read_timeout"),
        "{rollup:?}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening
)]
async fn enqueue_landing_source_admits_manager_and_lead_and_refuses_workers() {
    use rsi_common::rolling_queue::{AgentEnqueueLandingSourceRequestV1, RollingQueueEntryState};

    let pilot = pilot().await;
    let repo = TempDir::new().unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    git(&["init", "-q", "-b", "work"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    git(&["commit", "-q", "--allow-empty", "-m", "one"]);
    let first = git(&["rev-parse", "HEAD"]);
    git(&["branch", "origin/rolling"]);
    git(&["checkout", "-q", "-b", "multi"]);
    std::fs::create_dir_all(repo.path().join("tools/provisional-migrations")).unwrap();
    std::fs::write(
        repo.path().join("tools/provisional-migrations/one.json"),
        "{}\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("tools/provisional-migrations/two.json"),
        "{}\n",
    )
    .unwrap();
    git(&["add", "tools/provisional-migrations"]);
    git(&["commit", "-q", "-m", "two provisional migrations"]);
    let multiple = git(&["rev-parse", "HEAD"]);
    git(&["checkout", "-q", "work"]);
    git(&["commit", "-q", "--allow-empty", "-m", "two"]);
    let second = git(&["rev-parse", "HEAD"]);

    let worker = Uuid::new_v4();
    {
        let store = pilot.manager.store.lock().await;
        let mut row = bare_session(worker);
        row.parent_id = Some(pilot.epics[0]);
        row.session_kind = SessionKind::Task;
        row.status = SessionStatus::Running;
        row.project_id = store.get_session(pilot.owner).unwrap().unwrap().project_id;
        store.insert_session(&row).unwrap();
        for id in [pilot.owner, pilot.leads[0], worker] {
            store
                .conn
                .execute(
                    "UPDATE sessions SET working_dir=?2 WHERE id=?1",
                    rusqlite::params![id.to_string(), repo.path().display().to_string()],
                )
                .unwrap();
        }
    }
    let request = |commit: &str, key: &str| AgentEnqueueLandingSourceRequestV1 {
        supersedes: None,
        derive_filters: false,
        fixes_issue: None,
        fixes_tests: Vec::new(),
        project_id: None,
        source_session_id: None,
        source_commit: commit.to_string(),
        test_filters: vec!["rsid=rolling_queue".into()],
        idempotency_key: key.into(),
    };
    let control = pilot.manager.agent_control();

    let refused = control
        .agent_enqueue_landing_source(worker, request(&first, "w"), true)
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("queue_not_authorized"),
        "{refused}"
    );
    let disabled = control
        .agent_enqueue_landing_source(pilot.leads[0], request(&first, "d"), false)
        .await
        .unwrap_err();
    assert!(
        disabled.to_string().contains("queue_disabled"),
        "{disabled}"
    );
    let invalid = control
        .agent_enqueue_landing_source(pilot.leads[0], request(&"1".repeat(40), "i"), true)
        .await
        .unwrap_err();
    assert!(
        invalid.to_string().contains("queue_source_invalid"),
        "{invalid}"
    );
    let multiple_provisional = control
        .agent_enqueue_landing_source(
            pilot.leads[0],
            request(&multiple, "multi-provisional"),
            true,
        )
        .await
        .unwrap_err();
    assert!(
        multiple_provisional.to_string().contains(
            "queue_multiple_provisional_migrations: drop the extra provisional declarations or split the migrations across separate landings"
        ),
        "{multiple_provisional}"
    );

    let lead = control
        .agent_enqueue_landing_source(pilot.leads[0], request(&first, "l"), true)
        .await
        .unwrap();
    assert!(!lead.replayed);
    assert_eq!(lead.entry.state, RollingQueueEntryState::Queued);
    assert_eq!(lead.entry.source_session_id, pilot.leads[0]);
    // #2112: caller-named filters are exempt from the async full-library QA.
    assert!(crate::rolling_queue::has_explicit_filters(lead.entry.id));
    assert_eq!(lead.entry.owner_epic_id, Some(pilot.epics[0]));
    let replay = control
        .agent_enqueue_landing_source(pilot.leads[0], request(&first, "l"), true)
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.entry.id, lead.entry.id);
    let duplicate = control
        .agent_enqueue_landing_source(pilot.leads[0], request(&first, "l2"), true)
        .await
        .unwrap_err();
    assert!(
        duplicate.to_string().contains("queue_duplicate_source"),
        "{duplicate}"
    );

    let manager = control
        .agent_enqueue_landing_source(pilot.owner, request(&second, "m"), true)
        .await
        .unwrap();
    assert_eq!(manager.entry.source_session_id, pilot.owner);
    assert_eq!(manager.entry.owner_epic_id, None);
    assert!(manager.entry.sequence > lead.entry.sequence);

    // #1129: a filter that selects no test is refused at enqueue, naming it.
    std::fs::create_dir_all(repo.path().join("crates/rsid/src")).unwrap();
    std::fs::write(
        repo.path().join("crates/rsid/src/q.rs"),
        "#[test]\nfn queue_lands_things() {}\n",
    )
    .unwrap();
    git(&["add", "crates"]);
    git(&["commit", "-q", "-m", "three"]);
    let third = git(&["rev-parse", "HEAD"]);
    git(&["commit", "-q", "--allow-empty", "-m", "four"]);
    let fourth = git(&["rev-parse", "HEAD"]);
    // An explicit source equal to the project manager selects its own sandbox.
    let mut own_request = request(&fourth, "manager-own");
    own_request.source_session_id = Some(pilot.owner);
    own_request.test_filters = vec!["rsid=queue_lands".into()];
    let own = control
        .agent_enqueue_landing_source(pilot.owner, own_request, true)
        .await
        .unwrap();
    assert_eq!(own.entry.source_session_id, pilot.owner);
    assert_eq!(own.entry.source_commit, fourth);
    // The self-source carve-out grants nothing else (#1842): a worker naming
    // its own session stays refused, a manager self-source with a foreign
    // project stays refused, and a lead naming another session is told the
    // source is out of scope.
    let mut worker_own = request(&fourth, "worker-own");
    worker_own.source_session_id = Some(worker);
    let refused = control
        .agent_enqueue_landing_source(worker, worker_own, true)
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("queue_not_authorized"),
        "{refused}"
    );
    let mut foreign = request(&fourth, "manager-foreign");
    foreign.source_session_id = Some(pilot.owner);
    foreign.project_id = Some(Uuid::new_v4());
    let refused = control
        .agent_enqueue_landing_source(pilot.owner, foreign, true)
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("manager_project_not_in_scope"),
        "{refused}"
    );
    let mut lead_other = request(&fourth, "lead-other");
    lead_other.source_session_id = Some(pilot.owner);
    let refused = control
        .agent_enqueue_landing_source(pilot.leads[0], lead_other, true)
        .await
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("queue_source_session_out_of_scope"),
        "{refused}"
    );
    let mut empty = request(&third, "e");
    empty.test_filters = vec!["rsid=queue_lands".into(), "rsid=nowhere_to_be_found".into()];
    let refused = control
        .agent_enqueue_landing_source(pilot.leads[0], empty.clone(), true)
        .await
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("filter_matches_no_tests: rsid=nowhere_to_be_found"),
        "{refused}"
    );
    empty.test_filters = vec!["rsid=queue_lands".into()];
    let admitted = control
        .agent_enqueue_landing_source(pilot.leads[0], empty, true)
        .await
        .unwrap();
    assert_eq!(admitted.entry.state, RollingQueueEntryState::Queued);

    // #2167: a fix claim names its tests directly or through the red Issue
    // that lists them on a `Failing tests:` line (the qa Issue format).
    let (listed, unlisted, huge) = {
        let store = pilot.manager.store.lock().await;
        let project = store
            .get_session(pilot.leads[0])
            .unwrap()
            .unwrap()
            .project_id
            .unwrap();
        let issue = |title: &str, body: &str| {
            store
                .create_issue(&rsi_common::types::NewIssue {
                    project_id: project,
                    title: title.into(),
                    body: body.into(),
                    priority: None,
                    labels: vec!["qa".into()],
                    created_by_session_id: None,
                    assignee: None,
                    idea_id: None,
                    source_event_id: None,
                    source_finding_ref: None,
                })
                .unwrap()
                .display_number
        };
        (
            issue(
                "qa: rolling red",
                "Ran on the hub.\nFailing tests: demo::red, x::y (and 1 more)\nRefusal: none",
            ),
            issue("prose red", "three validator tests are red"),
            // #2175: a body listing one test over the 64-claim cap.
            issue(
                "qa: huge red",
                &format!(
                    "Failing tests: {}",
                    (0..=64)
                        .map(|i| format!("huge::t{i}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ),
        )
    };
    git(&["commit", "-q", "--allow-empty", "-m", "five"]);
    let fifth = git(&["rev-parse", "HEAD"]);
    let claim = |key: &str, issue: Option<i64>, tests: &[&str]| {
        let mut claim = request(&fifth, key);
        claim.test_filters = vec!["rsid=queue_lands".into()];
        claim.fixes_issue = issue;
        claim.fixes_tests = tests.iter().map(|name| (*name).to_string()).collect();
        claim
    };
    let refused = control
        .agent_enqueue_landing_source(pilot.leads[0], claim("unlisted", Some(unlisted), &[]), true)
        .await
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("queue_fixes_issue_lists_no_failing_tests"),
        "{refused}"
    );
    let refused = control
        .agent_enqueue_landing_source(pilot.leads[0], claim("missing", Some(999_999), &[]), true)
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("queue_fixes_issue_not_found"),
        "{refused}"
    );
    // #2175: both claim paths refuse over the cap, naming count and limit.
    let refused = control
        .agent_enqueue_landing_source(pilot.leads[0], claim("huge", Some(huge), &[]), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("queue_fixes_too_many"), "{refused}");
    assert!(refused.contains("65 claimed tests"), "{refused}");
    assert!(refused.contains("64-test limit"), "{refused}");
    let explicit: Vec<String> = (0..65).map(|i| format!("explicit::t{i}")).collect();
    let explicit: Vec<&str> = explicit.iter().map(String::as_str).collect();
    let refused = control
        .agent_enqueue_landing_source(pilot.leads[0], claim("explicit", None, &explicit), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("queue_fixes_too_many"), "{refused}");
    assert!(refused.contains("65 claimed tests"), "{refused}");
    // The union counts: 40 explicit names plus the 65 the Issue lists.
    let mixed: Vec<String> = (0..40).map(|i| format!("explicit::t{i}")).collect();
    let mixed: Vec<&str> = mixed.iter().map(String::as_str).collect();
    let refused = control
        .agent_enqueue_landing_source(pilot.leads[0], claim("mixed", Some(huge), &mixed), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("105 claimed tests"), "{refused}");
    let claimed = control
        .agent_enqueue_landing_source(
            pilot.leads[0],
            claim("claimed", Some(listed), &["x::y", "z::w"]),
            true,
        )
        .await
        .unwrap();
    assert_eq!(claimed.entry.fixes_tests, vec!["x::y", "z::w", "demo::red"]);
    assert_eq!(claimed.entry.test_filters, vec!["rsid=queue_lands"]);
    let replay = control
        .agent_enqueue_landing_source(
            pilot.leads[0],
            claim("claimed", Some(listed), &["x::y", "z::w"]),
            true,
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.entry.id, claimed.entry.id);
}

/// #1964: the queue list and cancel verbs. The gate is the advertised
/// projection (manager or Epic lead; a worker is refused), the scope follows
/// the appointed seat across a rotation, and only a `queued` entry can be
/// withdrawn.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening
)]
async fn rolling_queue_list_and_cancel_follow_the_seat_and_refuse_workers() {
    use crate::store::rolling_queue::NewQueueEntry;
    use rsi_common::rolling_queue::{
        AgentCancelRollingQueueEntryRequestV1, AgentListRollingQueueRequestV1, RollingQueueBinding,
        RollingQueueEntryState,
    };

    let pilot = pilot().await;
    let project = pilot.config.project_id;
    let worker = Uuid::new_v4();
    let enqueue = |store: &crate::store::Store, session: Uuid, commit: char| {
        store
            .enqueue_rolling_queue_source(
                &NewQueueEntry {
                    project_id: Some(project),
                    repo_path: "/tmp/repo".into(),
                    source_commit: commit.to_string().repeat(40),
                    source_session_id: session,
                    owner_epic_id: None,
                    binding: RollingQueueBinding::Unbound,
                    work_key: None,
                    migration_version: None,
                    hot_files: Vec::new(),
                    test_filters: Vec::new(),
                    fixes_tests: Vec::new(),
                    idempotency_key: format!("k-{commit}"),
                },
                chrono::Utc::now(),
            )
            .unwrap()
            .0
    };
    let (lead_entry, manager_entry, worker_entry) = {
        let store = pilot.manager.store.lock().await;
        let mut row = bare_session(worker);
        row.parent_id = Some(pilot.epics[0]);
        row.session_kind = SessionKind::Task;
        row.status = SessionStatus::Running;
        row.project_id = Some(project);
        store.insert_session(&row).unwrap();
        (
            enqueue(&store, pilot.leads[0], 'a'),
            enqueue(&store, pilot.owner, 'b'),
            enqueue(&store, worker, 'c'),
        )
    };
    let control = pilot.manager.agent_control();
    let list = |caller: Uuid, request: AgentListRollingQueueRequestV1| {
        let control = control.clone();
        async move { control.agent_list_rolling_queue(caller, request).await }
    };
    let cancel = |caller: Uuid, entry: Uuid, reason: Option<&str>| {
        let control = control.clone();
        let reason = reason.map(str::to_owned);
        async move {
            control
                .agent_cancel_rolling_queue_entry(
                    caller,
                    AgentCancelRollingQueueEntryRequestV1 {
                        entry_id: entry,
                        reason,
                    },
                )
                .await
        }
    };
    fn refusal<T>(result: crate::error::Result<T>) -> String {
        match result {
            Ok(_) => panic!("expected a refusal"),
            Err(error) => error.to_string(),
        }
    }

    // A worker is not a manager or lead: both verbs are refused, even for an
    // entry that names it as the source.
    assert!(refusal(list(worker, Default::default()).await).contains("queue_not_authorized"));
    // #2461: the refusal names what a worker does instead.
    assert!(
        refusal(list(worker, Default::default()).await)
            .contains("git merge-base --is-ancestor <SHA> origin/rolling")
    );
    assert!(refusal(cancel(worker, worker_entry.id, None).await).contains("queue_not_authorized"));

    // Closed-schema bounds are refused before any authority is consulted.
    let zero = AgentListRollingQueueRequestV1 {
        limit: Some(0),
        ..Default::default()
    };
    assert!(refusal(list(pilot.owner, zero).await).contains("queue_invalid_request"));
    assert!(
        refusal(cancel(pilot.owner, manager_entry.id, Some("bad\nreason")).await)
            .contains("queue_reason_invalid")
    );

    // The manager reads the whole project, newest first; a lead reads only
    // the entries whose outcome it would receive.
    let all = list(pilot.owner, Default::default()).await.unwrap();
    assert_eq!(
        all.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![worker_entry.id, manager_entry.id, lead_entry.id]
    );
    assert_eq!(all.next_before_sequence, None);
    let own = list(pilot.leads[0], Default::default()).await.unwrap();
    assert_eq!(
        own.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![lead_entry.id]
    );
    assert!(
        refusal(cancel(pilot.leads[0], manager_entry.id, None).await)
            .contains("queue_entry_not_found")
    );

    // The manager withdraws the lead's entry; the lead hears about it once.
    let cancelled = cancel(
        pilot.owner,
        lead_entry.id,
        Some("replaced by a newer commit"),
    )
    .await
    .unwrap();
    assert!(!cancelled.replayed);
    assert_eq!(cancelled.entry.state, RollingQueueEntryState::Superseded);
    assert!(
        cancelled
            .entry
            .outcome
            .unwrap()
            .detail
            .unwrap()
            .contains("replaced by")
    );
    assert!(
        cancel(pilot.owner, lead_entry.id, None)
            .await
            .unwrap()
            .replayed,
        "a repeated cancel replays"
    );
    {
        let store = pilot.manager.store.lock().await;
        let wakes: Vec<_> = store
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name == format!("merge-queue-cancel-{}", lead_entry.id))
            .collect();
        assert_eq!(wakes.len(), 1);
        assert_eq!(wakes[0].wake_session_id, Some(pilot.leads[0]));
        // The lander takes the next queued entry: the manager's own.
        let claimed = store
            .claim_next_rolling_queue_entry(chrono::Utc::now())
            .unwrap()
            .unwrap();
        assert_eq!(claimed.entry.id, manager_entry.id);
    }
    assert!(
        refusal(cancel(pilot.owner, manager_entry.id, None).await)
            .contains("queue_entry_in_flight")
    );

    // Rotate the seat: the displaced manager loses both verbs, and its
    // successor reaches the project's entries, the predecessor's included.
    let successor = Uuid::new_v4();
    {
        let store = pilot.manager.store.lock().await;
        let mut row = bare_session(successor);
        row.project_id = Some(project);
        row.status = SessionStatus::Running;
        store.insert_session(&row).unwrap();
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: project,
                session_id: successor,
                epic_ids: Some(pilot.epics.to_vec()),
                expected_row_version: pilot.config.row_version,
            })
            .unwrap();
    }
    assert!(refusal(list(pilot.owner, Default::default()).await).contains("queue_not_authorized"));
    assert!(
        refusal(cancel(pilot.owner, worker_entry.id, None).await).contains("queue_not_authorized")
    );
    let seen = list(
        successor,
        AgentListRollingQueueRequestV1 {
            state: Some(RollingQueueEntryState::Queued),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        seen.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![worker_entry.id]
    );
    let by_state = list(
        successor,
        AgentListRollingQueueRequestV1 {
            state: Some(RollingQueueEntryState::Gating),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(by_state.entries[0].id, manager_entry.id);
    assert_eq!(by_state.entries[0].enqueuer_session_id, pilot.owner);
    assert_eq!(by_state.entries[0].wake_session_id, Some(successor));
    assert!(by_state.entries[0].batch_id.is_some());
    // The gating entry stays with the lander for the successor too.
    assert!(
        refusal(cancel(successor, manager_entry.id, None).await).contains("queue_entry_in_flight")
    );
    let withdrawn = cancel(successor, worker_entry.id, None).await.unwrap();
    assert_eq!(withdrawn.entry.state, RollingQueueEntryState::Superseded);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening
)]
async fn check_landing_candidate_reports_the_receipt_to_managers_and_leads_only() {
    use crate::store::agent_jobs::NewAgentJob;
    use rsi_common::agent_jobs::{AgentJobResultV1, JobState, JobWake};
    use rsi_common::agent_landing_candidate::{
        AgentCheckLandingCandidateRequestV1 as Request,
        AgentCheckLandingCandidateResultV1 as Verdict,
    };

    let git = |dir: &std::path::Path, args: &[&str]| -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=tester",
                "-c",
                "user.email=tester@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    // Three commits in the worker's checkout: A, then B (documentation only),
    // then C (code).
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    std::fs::create_dir_all(repo.path().join("crates/x")).unwrap();
    std::fs::create_dir_all(repo.path().join("docs")).unwrap();
    std::fs::write(repo.path().join("crates/x/lib.rs"), "one\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "A"]);
    let commit_a = git(repo.path(), &["rev-parse", "HEAD"]);
    std::fs::write(repo.path().join("docs/note.md"), "note\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "B"]);
    let commit_b = git(repo.path(), &["rev-parse", "HEAD"]);
    std::fs::write(repo.path().join("crates/x/lib.rs"), "two\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "C"]);
    let commit_c = git(repo.path(), &["rev-parse", "HEAD"]);

    let pilot = pilot().await;
    let project = pilot.config.project_id;
    let (worker, foreign, idle, stranger) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let scoped_params = || -> rsi_common::agent_jobs::JobParams {
        serde_json::from_value(serde_json::json!({
            "kind": "test", "scoped_test": {"base": "origin/rolling"}
        }))
        .unwrap()
    };
    let mut counter = 0u32;
    let mut add_job = |store: &crate::store::Store,
                       id: Uuid,
                       owner: Uuid,
                       state: JobState,
                       result: AgentJobResultV1| {
        counter += 1;
        store
            .insert_agent_job(
                &NewAgentJob {
                    id,
                    owner_session_id: owner,
                    project_id: Some(project),
                    name: None,
                    params: scoped_params(),
                    cwd: repo.path().display().to_string(),
                    unit_name: format!("rsi-job-candidate-{counter}"),
                    log_path: repo.path().join("log").display().to_string(),
                    status_path: repo.path().join("status").display().to_string(),
                    idempotency_key: None,
                    wake: JobWake::None,
                },
                chrono::Utc::now(),
            )
            .unwrap();
        if state != JobState::Running {
            store
                .settle_agent_job(id, state, &result, false, chrono::Utc::now())
                .unwrap();
        }
    };
    let receipt = |head: &str, extra: serde_json::Value| -> AgentJobResultV1 {
        let mut receipt = serde_json::json!({
            "ok": true, "exit_code": 0, "head": head, "plan": "targeted_subset",
            "gate_covers_rest": true, "new_failures": null, "baseline_reds": [],
            "packages": [{"package": "rsid", "status": "passed", "completed": 12}],
            "selected_tests": 12, "run_tests": 12
        });
        for (key, value) in extra.as_object().unwrap() {
            receipt[key] = value.clone();
        }
        AgentJobResultV1 {
            exit_code: receipt["exit_code"].as_i64().map(|code| code as i32),
            receipt: Some(receipt),
            ..AgentJobResultV1::default()
        }
    };
    let green_id = Uuid::new_v4();
    let red_id = Uuid::new_v4();
    let docs_id = Uuid::new_v4();
    let timeout_id = Uuid::new_v4();
    let running_id = Uuid::new_v4();
    let ambiguous_one: Uuid = "dddddddd-0000-4000-8000-000000000001".parse().unwrap();
    let ambiguous_two: Uuid = "dddddddd-0000-4000-8000-000000000002".parse().unwrap();
    let foreign_job = Uuid::new_v4();
    {
        let store = pilot.manager.store.lock().await;
        for (id, parent, dir) in [
            (worker, pilot.epics[0], repo.path().to_path_buf()),
            (foreign, pilot.epics[1], repo.path().to_path_buf()),
            (idle, pilot.epics[0], std::path::PathBuf::from("/tmp")),
        ] {
            let mut row = bare_session(id);
            row.parent_id = Some(parent);
            row.session_kind = SessionKind::Task;
            row.status = SessionStatus::Completed;
            row.project_id = Some(project);
            row.working_dir = dir;
            store.insert_session(&row).unwrap();
        }
        store.insert_session(&bare_session(stranger)).unwrap();
        let fail = serde_json::json!({
            "ok": false, "exit_code": 1,
            "new_failures": [{"package": "rsid", "kind": "lib", "target": "rsid", "test": "m::t"}]
        });
        add_job(
            &store,
            green_id,
            worker,
            JobState::Succeeded,
            receipt(&commit_a, serde_json::json!({})),
        );
        add_job(
            &store,
            docs_id,
            worker,
            JobState::Succeeded,
            receipt(&commit_a, serde_json::json!({})),
        );
        add_job(
            &store,
            red_id,
            worker,
            JobState::Failed,
            receipt(&commit_c, fail),
        );
        add_job(
            &store,
            timeout_id,
            worker,
            JobState::Failed,
            AgentJobResultV1 {
                refusal: Some("job_admission_timed_out".into()),
                ..AgentJobResultV1::default()
            },
        );
        add_job(
            &store,
            running_id,
            worker,
            JobState::Running,
            AgentJobResultV1::default(),
        );
        add_job(
            &store,
            ambiguous_one,
            worker,
            JobState::Succeeded,
            receipt(&commit_a, serde_json::json!({})),
        );
        add_job(
            &store,
            foreign_job,
            foreign,
            JobState::Succeeded,
            receipt(&commit_a, serde_json::json!({})),
        );
        // Ambiguous inside the manager's reach only: the second job belongs to
        // a session the lead does not reach.
        add_job(
            &store,
            ambiguous_two,
            foreign,
            JobState::Succeeded,
            receipt(&commit_a, serde_json::json!({})),
        );
    }
    let control = pilot.manager.agent_control();
    let check = |caller: Uuid, job: Option<&str>, session: Option<Uuid>, commit: &str| {
        let control = control.clone();
        let request = Request {
            job_id: job.map(str::to_owned),
            source_session_id: session,
            source_commit: commit.to_owned(),
        };
        async move { control.agent_check_landing_candidate(caller, request).await }
    };
    fn refusal(result: crate::error::Result<Verdict>) -> String {
        match result {
            Ok(verdict) => panic!("expected a refusal, got {verdict:?}"),
            Err(error) => error.to_string(),
        }
    }
    let manager = pilot.owner;
    let lead = pilot.leads[0];
    let prefix = |id: Uuid| id.to_string()[..8].to_owned();

    // A matching head with no new failure is ok, by id prefix and by session.
    let ok = check(manager, Some(&prefix(green_id)), None, &commit_a)
        .await
        .unwrap();
    assert!(ok.ok, "{ok:?}");
    assert_eq!(ok.refusal, None);
    assert_eq!(ok.job_id, Some(green_id));
    assert_eq!(ok.job_kind.as_deref(), Some("test"));
    assert_eq!(ok.job_state.as_deref(), Some("succeeded"));
    assert_eq!(ok.exit_code, Some(0));
    assert_eq!(ok.receipt_head.as_deref(), Some(commit_a.as_str()));
    assert!(ok.head_matches);
    assert_eq!(ok.tests_completed, 12);
    assert_eq!(ok.plan.as_deref(), Some("targeted_subset"));
    assert!(ok.gate_covers_rest);
    let by_session = check(manager, None, Some(worker), &commit_a).await.unwrap();
    assert!(by_session.ok, "{by_session:?}");
    assert_eq!(by_session.job_owner_session_id, Some(worker));
    // The full id works, and naming the owning session as well.
    assert!(
        check(
            manager,
            Some(&green_id.to_string()),
            Some(worker),
            &commit_a
        )
        .await
        .unwrap()
        .ok
    );

    // A new failure is refused with the failing test named; the session form
    // finds the job whose receipt head is the commit.
    let red = check(manager, None, Some(worker), &commit_c).await.unwrap();
    assert!(!red.ok);
    assert_eq!(red.refusal.as_deref(), Some("candidate_new_failures"));
    assert_eq!(red.job_id, Some(red_id));
    assert_eq!(red.exit_code, Some(1));
    assert_eq!(red.new_failures.as_ref().unwrap()[0].test, "m::t");

    // A head mismatch reports whether the delta is documentation only.
    let docs = check(manager, Some(&prefix(docs_id)), None, &commit_b)
        .await
        .unwrap();
    assert!(!docs.ok);
    assert_eq!(docs.refusal.as_deref(), Some("candidate_head_mismatch"));
    assert!(!docs.head_matches);
    assert_eq!(docs.receipt_head.as_deref(), Some(commit_a.as_str()));
    assert_eq!(docs.docs_only_delta, Some(true), "{docs:?}");
    assert_eq!(docs.delta_files, Some(1));
    let code = check(manager, Some(&prefix(docs_id)), None, &commit_c)
        .await
        .unwrap();
    assert_eq!(code.refusal.as_deref(), Some("candidate_head_mismatch"));
    assert_eq!(code.docs_only_delta, Some(false));
    assert_eq!(code.delta_code_paths, vec!["crates/x/lib.rs".to_owned()]);

    // An admission timeout, a running job and a session with no job are
    // verdicts, not errors.
    let timeout = check(manager, Some(&prefix(timeout_id)), None, &commit_a)
        .await
        .unwrap();
    assert_eq!(
        timeout.refusal.as_deref(),
        Some("candidate_admission_timeout")
    );
    assert_eq!(
        timeout.job_refusal.as_deref(),
        Some("job_admission_timed_out")
    );
    let running = check(manager, Some(&prefix(running_id)), None, &commit_a)
        .await
        .unwrap();
    assert_eq!(
        running.refusal.as_deref(),
        Some("candidate_job_not_terminal")
    );
    let none = check(manager, None, Some(idle), &commit_a).await.unwrap();
    assert!(!none.ok);
    assert_eq!(
        none.refusal.as_deref(),
        Some("candidate_no_verification_job")
    );

    // The Epic lead reads its own worker; a lead of another Epic, the worker
    // itself and an unrelated session are refused, typed.
    assert!(
        check(lead, Some(&prefix(green_id)), None, &commit_a)
            .await
            .unwrap()
            .ok
    );
    assert!(
        refusal(check(pilot.leads[1], Some(&prefix(green_id)), None, &commit_a).await)
            .contains("candidate_not_found")
    );
    assert!(
        refusal(check(pilot.leads[1], None, Some(worker), &commit_a).await)
            .contains("candidate_not_found")
    );
    assert!(
        refusal(check(worker, Some(&prefix(green_id)), None, &commit_a).await)
            .contains("candidate_not_authorized")
    );
    assert!(
        refusal(check(stranger, Some(&prefix(green_id)), None, &commit_a).await)
            .contains("candidate_not_authorized")
    );
    // Unknown and out-of-scope look the same.
    assert!(
        refusal(check(manager, Some("12345678"), None, &commit_a).await)
            .contains("candidate_not_found")
    );
    assert!(
        refusal(check(lead, Some(&prefix(foreign_job)), None, &commit_a).await)
            .contains("candidate_not_found")
    );
    assert!(
        refusal(
            check(
                manager,
                Some(&green_id.to_string()),
                Some(foreign),
                &commit_a
            )
            .await
        )
        .contains("candidate_not_found")
    );

    // A prefix is unique inside the caller's reach only.
    assert!(
        refusal(check(manager, Some("dddddddd"), None, &commit_a).await)
            .contains("candidate_job_id_ambiguous")
    );
    assert_eq!(
        check(lead, Some("dddddddd"), None, &commit_a)
            .await
            .unwrap()
            .job_id,
        Some(ambiguous_one),
        "the lead counts only the jobs it reaches"
    );

    // Malformed requests are refused before any authority is consulted.
    for (job, session, commit) in [
        (Some("abc"), None, commit_a.as_str()),
        (Some("GREEN123"), None, commit_a.as_str()),
        (None, None, commit_a.as_str()),
        (None, Some(worker), "abc123"),
    ] {
        assert!(
            refusal(check(stranger, job, session, commit).await)
                .contains("candidate_invalid_request"),
            "{job:?} {session:?} {commit}"
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening
)]
async fn provider_status_admits_manager_and_lead_and_refuses_workers_without_a_key() {
    use crate::provider_status::{OpenRouterApi, ProviderStatusService};
    use crate::vault::check::ProbeOutcome;
    use crate::vault::secret::SecretString;
    use crate::vault::{VaultHandleBuilder, VaultSettings};
    use rsi_common::agent_provider_status::AgentGetProviderStatusRequestV1;
    use std::sync::Arc;

    const KEY: &str = "sk-or-test-verb-key-0009";
    struct Stub;
    #[async_trait::async_trait]
    impl OpenRouterApi for Stub {
        async fn key(&self, _secret: &SecretString) -> ProbeOutcome {
            ProbeOutcome::Http {
                status: 200,
                body: br#"{"data":{"limit":10.0,"usage":9.0,"limit_remaining":1.0}}"#.to_vec(),
            }
        }
        async fn credits(&self, _secret: &SecretString) -> ProbeOutcome {
            ProbeOutcome::Http {
                status: 200,
                body: br#"{"data":{"total_credits":20.0,"total_usage":19.0}}"#.to_vec(),
            }
        }
    }

    let pilot = pilot().await;
    let worker = Uuid::new_v4();
    {
        let store = pilot.manager.store.lock().await;
        let mut row = bare_session(worker);
        row.parent_id = Some(pilot.epics[0]);
        row.session_kind = SessionKind::Task;
        row.status = SessionStatus::Running;
        row.project_id = store.get_session(pilot.owner).unwrap().unwrap().project_id;
        store.insert_session(&row).unwrap();
    }
    let vault = VaultHandleBuilder::new(Arc::new(VaultSettings::default()))
        .env(|name| (name == "OPEN_ROUTER").then(|| KEY.to_string()))
        .open()
        .unwrap();
    let service = ProviderStatusService::new(Arc::new(Stub), std::time::Duration::from_secs(60));
    let control = pilot.manager.agent_control();
    let request = || AgentGetProviderStatusRequestV1 {
        provider: Some("openrouter".into()),
    };

    let refused = control
        .agent_get_provider_status_with(worker, request(), &service, &vault)
        .await
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("provider_status_not_authorized"),
        "{refused}"
    );
    for caller in [pilot.leads[0], pilot.owner] {
        let result = control
            .agent_get_provider_status_with(caller, request(), &service, &vault)
            .await
            .unwrap();
        assert_eq!(result.providers.len(), 1);
        let entry = &result.providers[0];
        assert_eq!(entry.provider, "openrouter");
        assert_eq!(entry.configured, Some(true));
        assert_eq!(
            entry
                .credit
                .as_ref()
                .and_then(|credit| credit.account_remaining),
            Some(1.0)
        );
        assert!(!serde_json::to_string(&result).unwrap().contains(KEY));
    }
    let unknown = control
        .agent_get_provider_status_with(
            pilot.owner,
            AgentGetProviderStatusRequestV1 {
                provider: Some("nope".into()),
            },
            &service,
            &vault,
        )
        .await
        .unwrap_err();
    assert!(
        unknown
            .to_string()
            .contains("provider_status_unknown_provider"),
        "{unknown}"
    );
}

/// #2753: `AgentGetProviderStatus` carries the CLI login state on the `claude`,
/// `codex` and `codex_app_server` entries (state, method, check time), a
/// logged-out CLI reads `refused` with `provider_login_required`, `unknown`
/// stays open, and the other providers carry no login.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::significant_drop_tightening)]
async fn provider_status_reports_the_cli_login_state_and_refuses_only_logged_out() {
    use crate::provider_login::{LoginProbeRunner, LoginProvider, ProbeRun, ProviderLoginService};
    use crate::provider_status::{OpenRouterApi, ProviderStatusService};
    use crate::vault::check::ProbeOutcome;
    use crate::vault::secret::SecretString;
    use crate::vault::{VaultHandleBuilder, VaultSettings};
    use rsi_common::agent_provider_status::AgentGetProviderStatusRequestV1;

    struct NoApi;
    #[async_trait::async_trait]
    impl OpenRouterApi for NoApi {
        async fn key(&self, _secret: &SecretString) -> ProbeOutcome {
            ProbeOutcome::Unsupported
        }
        async fn credits(&self, _secret: &SecretString) -> ProbeOutcome {
            ProbeOutcome::Unsupported
        }
    }
    struct Runner;
    #[async_trait::async_trait]
    impl LoginProbeRunner for Runner {
        async fn run(&self, provider: LoginProvider) -> ProbeRun {
            match provider {
                LoginProvider::Claude => ProbeRun::Exited {
                    code: Some(0),
                    stdout: br#"{"loggedIn":true,"authMethod":"claude.ai","email":"someone@example.test"}"#
                        .to_vec(),
                    stderr: Vec::new(),
                },
                LoginProvider::Codex => ProbeRun::Exited {
                    code: Some(1),
                    stdout: Vec::new(),
                    stderr: b"Not logged in".to_vec(),
                },
            }
        }
    }

    let pilot = pilot().await;
    let vault = VaultHandleBuilder::new(Arc::new(VaultSettings::default()))
        .env(|_| None)
        .open()
        .unwrap();
    let service = ProviderStatusService::new(Arc::new(NoApi), std::time::Duration::from_secs(60));
    let login = ProviderLoginService::new(Arc::new(Runner), std::time::Duration::from_secs(60));
    let result = pilot
        .manager
        .agent_control()
        .agent_get_provider_status_with_login(
            pilot.owner,
            AgentGetProviderStatusRequestV1::default(),
            &service,
            &vault,
            &login,
        )
        .await
        .unwrap();
    let json = serde_json::to_value(&result).unwrap();
    let entry = |name: &str| {
        json["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["provider"] == name)
            .unwrap()
            .clone()
    };
    let claude = entry("claude");
    assert_eq!(claude["login"]["state"], "logged_in");
    assert_eq!(claude["login"]["method"], "claude.ai");
    assert!(claude["login"]["checked_at"].is_string());
    assert_eq!(claude["launch_admission"], "open");
    for name in ["codex", "codex_app_server"] {
        let codex = entry(name);
        assert_eq!(codex["login"]["state"], "logged_out", "{name}");
        assert_eq!(codex["launch_admission"], "refused", "{name}");
        assert_eq!(codex["refusal_detail"], "provider_login_required", "{name}");
    }
    assert_eq!(entry("openrouter").get("login"), None);
    // The login object holds the three facts and the satellite rows, no more.
    let mut keys: Vec<_> = claude["login"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(keys, ["checked_at", "method", "state"]);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::significant_drop_tightening)]
async fn a_draining_deploy_holds_child_jobs_refuses_continuations_and_reports_them() {
    use crate::agent_jobs::{JobRuntime, JobTools, LaunchSpec};
    use crate::daemon_info::DaemonInfoService;
    use crate::store::agent_deploys::DeployRow;
    use rsi_common::agent_jobs::{AgentSubmitJobRequestV1, JobKind, JobState};

    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<LaunchSpec>>);
    impl JobRuntime for Recorder {
        fn launch(&self, spec: &LaunchSpec) -> std::result::Result<(), String> {
            self.0.lock().unwrap().push(spec.clone());
            Ok(())
        }
        fn unit_active(&self, _unit: &str) -> bool {
            true
        }
    }

    let pilot = pilot().await;
    let sandbox = TempDir::new().unwrap();
    let worker = Uuid::new_v4();
    {
        let store = pilot.manager.store.lock().await;
        let mut row = bare_session(worker);
        row.parent_id = Some(pilot.epics[0]);
        row.session_kind = SessionKind::Task;
        row.status = SessionStatus::Completed;
        row.project_id = store.get_session(pilot.owner).unwrap().unwrap().project_id;
        store.insert_session(&row).unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET sandbox_root=?2 WHERE id=?1",
                rusqlite::params![worker.to_string(), sandbox.path().display().to_string()],
            )
            .unwrap();
    }
    let tools = || JobTools {
        cargo_slot: "/x/cargo-slot".into(),
        lander: "/x/rsi-rolling-land".into(),
    };
    let request_named = |name: &str| AgentSubmitJobRequestV1 {
        project_id: None,
        sandbox_session_id: None,
        kind: JobKind::Build,
        params: serde_json::json!({"command": "check", "workspace": true}),
        name: Some(name.into()),
        idempotency_key: None,
        worktree: None,
        wake: None,
    };
    let request = || request_named("check");
    let control = pilot.manager.agent_control();
    let recorder = std::sync::Arc::new(Recorder::default());

    // A job already running when the deploy is requested keeps the deploy
    // waiting.
    let running = control
        .agent_submit_job(worker, request(), recorder.clone(), tools())
        .await
        .unwrap();
    assert_eq!(running.job.state, JobState::Running);
    assert_eq!(recorder.0.lock().unwrap().len(), 1);

    // The deploy's caller is the appointed manager; its deploy is waiting.
    let deadline = chrono::Utc::now() + chrono::Duration::seconds(600);
    let live = DeployRow {
        id: Uuid::new_v4(),
        owner_session_id: Some(pilot.owner),
        sha: "0".repeat(40),
        manifest: Vec::new(),
        state: rsi_common::agent_deploy::DeployState::Staged,
        reason: None,
        deadline_at: deadline,
        operator: false,
        forced: false,
    };
    pilot
        .manager
        .deploy_drain()
        .sync(Some(&live), true, chrono::Utc::now());

    // #1566: a child's new job is held, not refused: recorded `queued` with
    // the reason, no unit launched, readable through AgentGetJob/ListJobs.
    let held = control
        .agent_submit_job(worker, request_named("held"), recorder.clone(), tools())
        .await
        .unwrap();
    assert_eq!(held.job.state, JobState::Queued);
    assert_eq!(held.job.held.as_deref(), Some("held_for_deploy"));
    // #2190: the receipt carries when the hold ends at the latest.
    assert_eq!(
        held.release_by,
        Some(deadline.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
    );
    assert_eq!(held.job.started_at, None);
    assert_eq!(recorder.0.lock().unwrap().len(), 1, "nothing launched");
    let read = control
        .agent_get_job(
            worker,
            rsi_common::agent_jobs::AgentGetJobRequestV1 {
                job_id: held.job.id,
            },
        )
        .await
        .unwrap();
    assert_eq!(read.state, JobState::Queued);
    let listed = control
        .agent_list_jobs(
            worker,
            rsi_common::agent_jobs::AgentListJobsRequestV1 { limit: None },
        )
        .await
        .unwrap();
    assert!(
        listed
            .jobs
            .iter()
            .any(|job| job.id == held.job.id && job.state == JobState::Queued)
    );
    // The deploy still waits on the job that was already running, and the
    // held job is not what it waits on.
    let blockers = |owner| {
        let manager = &pilot.manager;
        async move {
            manager
                .store
                .lock()
                .await
                .deploy_quiet_blockers_with(owner, false)
                .unwrap()
        }
    };
    assert!(blockers(pilot.owner).await.contains(&"job_running"));
    pilot
        .manager
        .store
        .lock()
        .await
        .settle_agent_job(
            running.job.id,
            JobState::Succeeded,
            &rsi_common::agent_jobs::AgentJobResultV1::default(),
            false,
            chrono::Utc::now(),
        )
        .unwrap();
    assert!(
        !blockers(pilot.owner).await.contains(&"job_running"),
        "a held job never keeps the deploy from its quiet point"
    );
    // While the hold is engaged the job loop starts nothing.
    let job_runtime: std::sync::Arc<dyn JobRuntime> = recorder.clone();
    let started = crate::agent_jobs::start_queued_jobs(
        pilot.manager.store(),
        &job_runtime,
        &tools(),
        &pilot.manager.deploy_drain(),
    )
    .await
    .unwrap();
    assert_eq!(started, 0);
    assert_eq!(recorder.0.lock().unwrap().len(), 1);
    // A child's continuation is refused the same way, before any effect.
    let continued = pilot
        .manager
        .continue_session(worker, "more work".into())
        .await
        .unwrap_err();
    assert!(
        continued.to_string().contains("deploy_draining"),
        "{continued}"
    );
    // The hold is visible to the manager.
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("rsid");
    std::fs::write(&exe, b"abc").unwrap();
    let service =
        DaemonInfoService::new(dir.path().to_path_buf(), dir.path().join("sandboxes"), exe);
    let info = control
        .agent_get_daemon_info_with(pilot.owner, &service)
        .await
        .unwrap();
    assert!(info.deploy_drain.draining);
    // The bus counters ride on the same read-only call (#332): the manager sees
    // aggregate emitted/delivered per kind and nothing per session.
    let bus = info.bus.as_ref().expect("bus counters");
    assert_eq!(
        bus.emitted_total,
        bus.by_kind.iter().map(|kind| kind.emitted).sum::<u64>()
    );
    assert_eq!(info.deploy_drain.deploy_id, Some(live.id));
    assert_eq!(
        info.deploy_drain.refused_total, 1,
        "only the continuation is refused; the job was held"
    );

    // The deploy settles: the job loop launches the held job. Once that job
    // completes, a fresh submit runs at once (no peer hold from #1921).
    pilot
        .manager
        .deploy_drain()
        .sync(None, true, chrono::Utc::now());
    let started = crate::agent_jobs::start_queued_jobs(
        pilot.manager.store(),
        &job_runtime,
        &tools(),
        &pilot.manager.deploy_drain(),
    )
    .await
    .unwrap();
    assert_eq!(started, 1);
    assert_eq!(recorder.0.lock().unwrap().len(), 2);
    let after = control
        .agent_get_job(
            worker,
            rsi_common::agent_jobs::AgentGetJobRequestV1 {
                job_id: held.job.id,
            },
        )
        .await
        .unwrap();
    assert_eq!(after.state, JobState::Running);
    assert_eq!(after.held, None);
    assert!(after.started_at.is_some());
    pilot
        .manager
        .store
        .lock()
        .await
        .settle_agent_job(
            held.job.id,
            JobState::Succeeded,
            &rsi_common::agent_jobs::AgentJobResultV1::default(),
            false,
            chrono::Utc::now(),
        )
        .unwrap();
    let receipt = control
        .agent_submit_job(worker, request_named("fresh"), recorder.clone(), tools())
        .await
        .unwrap();
    assert_eq!(receipt.job.state, JobState::Running);
    assert_eq!(recorder.0.lock().unwrap().len(), 3);
    let info = control
        .agent_get_daemon_info_with(pilot.owner, &service)
        .await
        .unwrap();
    assert!(!info.deploy_drain.draining);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::significant_drop_tightening)]
async fn daemon_info_is_manager_and_lead_only_and_well_formed() {
    use crate::daemon_info::DaemonInfoService;

    let pilot = pilot().await;
    let worker = Uuid::new_v4();
    {
        let store = pilot.manager.store.lock().await;
        let mut row = bare_session(worker);
        row.parent_id = Some(pilot.epics[0]);
        row.session_kind = SessionKind::Task;
        row.status = SessionStatus::Running;
        row.project_id = store.get_session(pilot.owner).unwrap().unwrap().project_id;
        store.insert_session(&row).unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("rsid");
    std::fs::write(&exe, b"abc").unwrap();
    let service =
        DaemonInfoService::new(dir.path().to_path_buf(), dir.path().join("sandboxes"), exe);
    let control = pilot.manager.agent_control();

    let refused = control
        .agent_get_daemon_info_with(worker, &service)
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("daemon_info_not_authorized"),
        "{refused}"
    );
    // #2426: the refusal names the worker's alternative.
    assert!(refused.to_string().contains("AgentGetJob"), "{refused}");
    for caller in [pilot.leads[0], pilot.owner] {
        let hub = control
            .agent_get_daemon_info_with(caller, &service)
            .await
            .unwrap()
            .hub;
        assert!(!hub.build_sha.is_empty());
        assert_eq!(
            hub.binary_sha256.as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert!(hub.schema_version > 0);
        assert!(chrono::DateTime::parse_from_rfc3339(&hub.started_at).is_ok());
        assert!(hub.disk_free.data_dir_free_bytes.unwrap() > 0);
        assert!(hub.disk_free.sandbox_base_free_bytes.unwrap() > 0);
        let json = serde_json::to_value(&hub).unwrap();
        assert!(json.get("build_sha").is_some() && json.get("schema_version").is_some());
    }
}

/// Hub #2980: a daemon reads its own most recent deploy from its own
/// `AgentGetDaemonInfo` (`hub.last_deploy`), the field the playbook's deploy
/// step 3 names; absent before any deploy.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::significant_drop_tightening)]
async fn daemon_info_reports_its_own_last_deploy_after_one_is_recorded() {
    use crate::daemon_info::DaemonInfoService;
    use crate::store::agent_deploys::NewOperatorDeploy;
    use rsi_common::agent_deploy::DeployState;

    let pilot = pilot().await;
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("rsid");
    std::fs::write(&exe, b"abc").unwrap();
    let service =
        DaemonInfoService::new(dir.path().to_path_buf(), dir.path().join("sandboxes"), exe);
    let control = pilot.manager.agent_control();

    let before = control
        .agent_get_daemon_info_with(pilot.owner, &service)
        .await
        .unwrap()
        .hub;
    assert!(before.last_deploy.is_none());
    assert!(
        serde_json::to_value(&before)
            .unwrap()
            .get("last_deploy")
            .is_none()
    );

    let id = Uuid::new_v4();
    let sha = "c".repeat(40);
    let settled_at = chrono::Utc::now();
    {
        let store = pilot.manager.store.lock().await;
        store
            .insert_operator_deploy(
                &NewOperatorDeploy {
                    id,
                    sha: &sha,
                    manifest: &[],
                    max_wait_secs: 60,
                    forced: false,
                },
                settled_at - chrono::Duration::seconds(5),
            )
            .unwrap();
    }
    // Staged: reported without a finish time.
    let staged = control
        .agent_get_daemon_info_with(pilot.owner, &service)
        .await
        .unwrap()
        .hub
        .last_deploy
        .unwrap();
    assert_eq!(staged.state, DeployState::Staged);
    assert!(staged.finished_at.is_none());
    {
        let store = pilot.manager.store.lock().await;
        store
            .settle_agent_deploy(id, DeployState::Succeeded, None, settled_at)
            .unwrap();
    }
    let hub = control
        .agent_get_daemon_info_with(pilot.owner, &service)
        .await
        .unwrap()
        .hub;
    let last = hub.last_deploy.clone().unwrap();
    assert_eq!(last.deploy_id.0, id);
    assert_eq!(last.sha, sha);
    assert_eq!(last.state, DeployState::Succeeded);
    assert_eq!(
        last.finished_at.as_deref(),
        Some(
            settled_at
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                .as_str()
        )
    );
    let json = serde_json::to_value(&hub).unwrap();
    assert_eq!(json["last_deploy"]["state"], "succeeded");
    assert_eq!(json["last_deploy"]["sha"], sha);
    assert_eq!(json["last_deploy"]["deploy_id"], id.to_string());
    // A reply from an older daemon (no `last_deploy`) still parses.
    let mut old = json;
    old.as_object_mut().unwrap().remove("last_deploy");
    let parsed: rsi_common::agent_daemon_info::DaemonInfoV1 = serde_json::from_value(old).unwrap();
    assert!(parsed.last_deploy.is_none());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening
)]
async fn submit_job_runs_in_the_callers_sandbox_and_reads_are_owner_scoped() {
    use crate::agent_jobs::{JobRuntime, JobTools, LaunchSpec};
    use rsi_common::agent_jobs::{
        AgentCancelJobRequestV1, AgentGetJobRequestV1, AgentListJobsRequestV1,
        AgentSubmitJobRequestV1, JobKind, JobState,
    };

    #[derive(Default)]
    struct Recorder(
        std::sync::Mutex<Vec<LaunchSpec>>,
        std::sync::Mutex<Vec<String>>,
    );
    impl JobRuntime for Recorder {
        fn launch(&self, spec: &LaunchSpec) -> std::result::Result<(), String> {
            self.0.lock().unwrap().push(spec.clone());
            Ok(())
        }
        fn unit_active(&self, _unit: &str) -> bool {
            self.1.lock().unwrap().is_empty()
        }
        fn stop_unit(&self, unit: &str) -> std::result::Result<(), String> {
            self.1.lock().unwrap().push(unit.to_string());
            Ok(())
        }
    }

    let pilot = pilot().await;
    let sandbox = TempDir::new().unwrap();
    let other = TempDir::new().unwrap();
    let worker = Uuid::new_v4();
    {
        let store = pilot.manager.store.lock().await;
        let mut row = bare_session(worker);
        row.parent_id = Some(pilot.epics[0]);
        row.session_kind = SessionKind::Task;
        row.status = SessionStatus::Running;
        row.project_id = store.get_session(pilot.owner).unwrap().unwrap().project_id;
        store.insert_session(&row).unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET sandbox_root=?2 WHERE id=?1",
                rusqlite::params![worker.to_string(), sandbox.path().display().to_string()],
            )
            .unwrap();
    }
    let tools = || JobTools {
        cargo_slot: "/x/cargo-slot".into(),
        lander: "/x/rsi-rolling-land".into(),
    };
    let request = |worktree: Option<&std::path::Path>| AgentSubmitJobRequestV1 {
        project_id: None,
        sandbox_session_id: None,
        kind: JobKind::Build,
        params: serde_json::json!({"command": "check", "workspace": true}),
        name: Some("check".into()),
        idempotency_key: None,
        worktree: worktree.map(|p| p.display().to_string()),
        wake: None,
    };
    let control = pilot.manager.agent_control();
    let recorder = std::sync::Arc::new(Recorder::default());

    let receipt = control
        .agent_submit_job(worker, request(None), recorder.clone(), tools())
        .await
        .unwrap();
    assert_eq!(receipt.job.state, JobState::Running);
    assert_eq!(receipt.job.owner_session_id, worker);
    // #269: the receipt repeats the id at the top level.
    assert_eq!(receipt.job_id, receipt.job.id);
    assert_eq!(
        std::path::PathBuf::from(&receipt.job.cwd),
        sandbox.path().canonicalize().unwrap()
    );
    let launched = recorder.0.lock().unwrap().clone();
    assert_eq!(launched.len(), 1);
    assert_eq!(launched[0].cwd, sandbox.path().canonicalize().unwrap());

    // A worker cannot point the job at another directory.
    let refused = control
        .agent_submit_job(
            worker,
            request(Some(other.path())),
            recorder.clone(),
            tools(),
        )
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("job_directory_not_allowed"),
        "{refused}"
    );
    // Invalid typed parameters are refused before anything is recorded.
    let mut bad = request(None);
    bad.params = serde_json::json!({"command": "check; id", "workspace": true});
    let invalid = control
        .agent_submit_job(worker, bad, recorder.clone(), tools())
        .await
        .unwrap_err();
    assert!(
        invalid.to_string().contains("job_invalid_params"),
        "{invalid}"
    );
    assert_eq!(recorder.0.lock().unwrap().len(), 1);

    // Reads are scoped to the owner.
    let got = control
        .agent_get_job(
            worker,
            AgentGetJobRequestV1 {
                job_id: receipt.job.id,
            },
        )
        .await
        .unwrap();
    assert_eq!(got.id, receipt.job.id);
    // #2470: the worker's current in-scope manager reads its job receipt.
    let scoped = control
        .agent_get_job(
            pilot.owner,
            AgentGetJobRequestV1 {
                job_id: receipt.job.id,
            },
        )
        .await
        .unwrap();
    assert_eq!(scoped.id, receipt.job.id);
    assert_eq!(scoped.owner_session_id, worker);
    // An unrelated session learns nothing: no oracle for foreign jobs.
    let foreign = control
        .agent_get_job(
            pilot.leads[1],
            AgentGetJobRequestV1 {
                job_id: receipt.job.id,
            },
        )
        .await
        .unwrap_err();
    assert!(foreign.to_string().contains("job_not_found"), "{foreign}");
    // #139: the 8-character prefix a notice prints resolves in the scope the
    // full id reads: the owner and the worker's in-scope manager get the full
    // id, an unrelated session is refused like an unknown job.
    let short = serde_json::json!({"job_id": receipt.job.id.to_string()[..8]});
    for caller in [worker, pilot.owner] {
        let expanded = control
            .expand_id_prefixes(
                caller,
                rsi_common::agent_control_schema::AgentControlVerbV1::GetJob,
                short.clone(),
            )
            .await
            .unwrap();
        assert_eq!(expanded["job_id"], receipt.job.id.to_string());
    }
    let hidden = control
        .expand_id_prefixes(
            pilot.leads[1],
            rsi_common::agent_control_schema::AgentControlVerbV1::GetJob,
            short,
        )
        .await
        .unwrap_err();
    assert!(hidden.to_string().contains("job_not_found"), "{hidden}");
    let mine = control
        .agent_list_jobs(worker, AgentListJobsRequestV1 { limit: None })
        .await
        .unwrap();
    assert_eq!(mine.jobs.len(), 1);
    // #269: a worker recovers a lost job id from its own list.
    assert_eq!(mine.jobs[0].id, receipt.job_id);
    // Lists stay owner-scoped: the manager's list does not carry a worker's jobs.
    for caller in [pilot.owner, pilot.leads[1]] {
        let theirs = control
            .agent_list_jobs(caller, AgentListJobsRequestV1 { limit: None })
            .await
            .unwrap();
        assert!(theirs.jobs.is_empty());
    }

    // #1106/#2233: only the owner (or its in-scope manager) may cancel; a
    // foreign caller (an unrelated non-manager session) learns nothing.
    let refused = control
        .agent_cancel_job(
            pilot.leads[1],
            AgentCancelJobRequestV1 {
                job_id: receipt.job.id,
            },
            recorder.clone(),
        )
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("job_not_found"), "{refused}");
    assert!(recorder.1.lock().unwrap().is_empty());
    // #2233: the owner's current in-scope manager is not foreign; without the
    // session-control capability it sees the typed policy refusal and the job
    // is untouched.
    let refused = control
        .agent_cancel_job(
            pilot.owner,
            AgentCancelJobRequestV1 {
                job_id: receipt.job.id,
            },
            recorder.clone(),
        )
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("manager_v2_capability_denied"),
        "{refused}"
    );
    assert!(recorder.1.lock().unwrap().is_empty());
    let cancelled = control
        .agent_cancel_job(
            worker,
            AgentCancelJobRequestV1 {
                job_id: receipt.job.id,
            },
            recorder.clone(),
        )
        .await
        .unwrap();
    assert!(cancelled.cancelled);
    assert_eq!(cancelled.job.state, JobState::Failed);
    assert_eq!(
        cancelled.job.result.unwrap().refusal.as_deref(),
        Some("job_cancelled")
    );
    assert_eq!(*recorder.1.lock().unwrap(), [receipt.job.unit_name.clone()]);
    let read_back = control
        .agent_get_job(
            worker,
            AgentGetJobRequestV1 {
                job_id: receipt.job.id,
            },
        )
        .await
        .unwrap();
    assert_eq!(read_back.state, JobState::Failed);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening
)]
async fn landing_cloud_gate_and_cloud_sweep_jobs_need_the_manager_or_epic_lead() {
    use crate::agent_jobs::{JobRuntime, JobTools, LaunchSpec};
    use rsi_common::agent_jobs::{AgentSubmitJobRequestV1, JobKind};

    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<LaunchSpec>>);
    impl JobRuntime for Recorder {
        fn launch(&self, spec: &LaunchSpec) -> std::result::Result<(), String> {
            self.0.lock().unwrap().push(spec.clone());
            Ok(())
        }
        fn unit_active(&self, _unit: &str) -> bool {
            true
        }
    }

    let pilot = pilot().await;
    let worker = Uuid::new_v4();
    let sandboxes: Vec<TempDir> = (0..3).map(|_| TempDir::new().unwrap()).collect();
    {
        let store = pilot.manager.store.lock().await;
        let mut row = bare_session(worker);
        row.parent_id = Some(pilot.epics[0]);
        row.session_kind = SessionKind::Task;
        row.status = SessionStatus::Running;
        row.project_id = store.get_session(pilot.owner).unwrap().unwrap().project_id;
        store.insert_session(&row).unwrap();
        for (id, dir) in [worker, pilot.leads[0], pilot.owner]
            .into_iter()
            .zip(&sandboxes)
        {
            store
                .conn
                .execute(
                    "UPDATE sessions SET sandbox_root=?2 WHERE id=?1",
                    rusqlite::params![id.to_string(), dir.path().display().to_string()],
                )
                .unwrap();
        }
    }
    let tools = || JobTools {
        cargo_slot: "/x/cargo-slot".into(),
        lander: "/x/rsi-rolling-land".into(),
    };
    let oid = "0123456789abcdef0123456789abcdef01234567";
    let request = |kind: JobKind, key: &str| AgentSubmitJobRequestV1 {
        project_id: None,
        sandbox_session_id: None,
        kind,
        params: if kind == JobKind::CloudSweep {
            serde_json::json!({"sha": oid})
        } else {
            serde_json::json!({"accepted": oid})
        },
        name: None,
        idempotency_key: Some(key.into()),
        worktree: None,
        wake: None,
    };
    let control = pilot.manager.agent_control();
    let recorder = std::sync::Arc::new(Recorder::default());

    // An ordinary worker cannot publish to rolling or spend the cloud grant.
    for kind in [JobKind::Landing, JobKind::CloudGate, JobKind::CloudSweep] {
        let refused = control
            .agent_submit_job(worker, request(kind, "w"), recorder.clone(), tools())
            .await
            .unwrap_err();
        assert!(
            refused.to_string().contains("job_kind_not_authorized"),
            "{kind:?}: {refused}"
        );
    }
    assert!(recorder.0.lock().unwrap().is_empty(), "nothing launched");

    // The current Epic lead and the appointed manager may land.
    for (caller, key) in [(pilot.leads[0], "l"), (pilot.owner, "m")] {
        let receipt = control
            .agent_submit_job(
                caller,
                request(JobKind::Landing, key),
                recorder.clone(),
                tools(),
            )
            .await
            .unwrap();
        assert_eq!(receipt.job.owner_session_id, caller);
    }
    assert_eq!(recorder.0.lock().unwrap().len(), 2);
}

/// #1337: a test job carries the operator's default timeout (stamped at
/// submit, read live); anyone may ask for less, only the appointed manager or
/// an Epic lead for more.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening
)]
async fn test_jobs_get_the_default_timeout_and_only_managers_or_leads_raise_it() {
    use crate::agent_jobs::{JobRuntime, JobTools, LaunchSpec};
    use rsi_common::agent_jobs::{AgentSubmitJobRequestV1, JobKind, JobParams};

    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<LaunchSpec>>);
    impl JobRuntime for Recorder {
        fn launch(&self, spec: &LaunchSpec) -> std::result::Result<(), String> {
            self.0.lock().unwrap().push(spec.clone());
            Ok(())
        }
        fn unit_active(&self, _unit: &str) -> bool {
            true
        }
    }

    let pilot = pilot().await;
    let worker = Uuid::new_v4();
    // tmpfs-fixture-ok: no sandbox is allocated (passes with TMPDIR on /dev/shm).
    let sandboxes: Vec<TempDir> = (0..3).map(|_| TempDir::new().unwrap()).collect();
    {
        let store = pilot.manager.store.lock().await;
        let mut row = bare_session(worker);
        row.parent_id = Some(pilot.epics[0]);
        row.session_kind = SessionKind::Task;
        row.status = SessionStatus::Running;
        row.project_id = store.get_session(pilot.owner).unwrap().unwrap().project_id;
        store.insert_session(&row).unwrap();
        for (id, dir) in [worker, pilot.leads[0], pilot.owner]
            .into_iter()
            .zip(&sandboxes)
        {
            store
                .conn
                .execute(
                    "UPDATE sessions SET sandbox_root=?2 WHERE id=?1",
                    rusqlite::params![id.to_string(), dir.path().display().to_string()],
                )
                .unwrap();
        }
    }
    let tools = || JobTools {
        cargo_slot: "/x/cargo-slot".into(),
        lander: "/x/rsi-rolling-land".into(),
    };
    let request = |timeout: Option<u32>| {
        let mut params = serde_json::json!({"package": "rsid", "filters": ["agent_jobs"]});
        if let Some(minutes) = timeout {
            params["timeout_minutes"] = minutes.into();
        }
        AgentSubmitJobRequestV1 {
            project_id: None,
            sandbox_session_id: None,
            kind: JobKind::Test,
            params,
            name: None,
            idempotency_key: None,
            worktree: None,
            wake: None,
        }
    };
    let timeout_of = |params: &JobParams| match params {
        JobParams::Test(test) => test.timeout_minutes,
        _ => None,
    };
    let control = pilot.manager.agent_control();
    let recorder = std::sync::Arc::new(Recorder::default());

    // No timeout named: the operator default (20) is stamped on the job and
    // the unit cap follows it, plus the unit grace and the build-slot wait
    // allowance (#1706: admission waits do not spend the execution budget).
    let receipt = control
        .agent_submit_job(worker, request(None), recorder.clone(), tools())
        .await
        .unwrap();
    assert_eq!(timeout_of(&receipt.job.params), Some(20));
    assert_eq!(
        recorder.0.lock().unwrap()[0].command.runtime_max_secs,
        20 * 60 + 5 * 60 + crate::agent_jobs::admission_allowance_secs(&receipt.job.params)
    );
    // A worker may ask for less, never for more.
    let lower = control
        .agent_submit_job(worker, request(Some(10)), recorder.clone(), tools())
        .await
        .unwrap();
    assert_eq!(timeout_of(&lower.job.params), Some(10));
    // #1921: an accepted lower timeout queues behind the first job. Timeout
    // authority must still be checked before this peer queue (#1969).
    assert_eq!(lower.job.state, rsi_common::agent_jobs::JobState::Queued);
    assert_eq!(lower.job.held.as_deref(), Some("sandbox_job_queued"));
    let refused = control
        .agent_submit_job(worker, request(Some(60)), recorder.clone(), tools())
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("job_timeout_not_authorized"),
        "{refused}"
    );
    assert_eq!(recorder.0.lock().unwrap().len(), 1, "nothing else launched");
    // The Epic lead and the appointed manager may raise one job.
    for caller in [pilot.leads[0], pilot.owner] {
        let raised = control
            .agent_submit_job(caller, request(Some(60)), recorder.clone(), tools())
            .await
            .unwrap();
        assert_eq!(timeout_of(&raised.job.params), Some(60));
    }
    // The default is the operator's live setting.
    pilot
        .manager
        .runtime_config
        .update_field("job_test_timeout_mins", &serde_json::json!(90))
        .unwrap();
    let receipt = control
        .agent_submit_job(worker, request(Some(60)), recorder.clone(), tools())
        .await
        .unwrap();
    assert_eq!(timeout_of(&receipt.job.params), Some(60));
    let receipt = control
        .agent_submit_job(worker, request(None), recorder.clone(), tools())
        .await
        .unwrap();
    assert_eq!(timeout_of(&receipt.job.params), Some(90));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening
)]
async fn send_satellite_message_admits_only_the_appointed_manager() {
    use rsi_common::satellite::{
        SatellitePeerConfigV1, SatellitePutPeerRequestV1, SatelliteUuidV1,
    };
    use rsi_common::satellite_dispatch::AgentSendSatelliteMessageRequestV1;

    let pilot = pilot().await;
    let peer = Uuid::new_v4();
    let remote = Uuid::new_v4();
    {
        let store = pilot.manager.store.lock().await;
        store
            .put_satellite_peer(
                &SatellitePutPeerRequestV1 {
                    expected_registry_revision: 1,
                    peer: SatellitePeerConfigV1 {
                        peer_id: SatelliteUuidV1(peer),
                        label: "work laptop".into(),
                        expected_installation_id: Some(SatelliteUuidV1(Uuid::new_v4())),
                        enabled: true,
                        read_enabled: true,
                        dispatch_enabled: true,
                    },
                    repair_quarantine: false,
                },
                std::path::Path::new("/tmp/satellites"),
            )
            .unwrap();
        store
            .put_satellite_peer_scope(2, peer, &[SatelliteUuidV1(remote)])
            .unwrap();
    }
    let request = |key: &str| AgentSendSatelliteMessageRequestV1 {
        peer_id: SatelliteUuidV1(peer),
        remote_session_id: SatelliteUuidV1(remote),
        scope_project_id: None,
        message: "please report status".into(),
        idempotency_key: key.into(),
        expires_at: None,
        issue_refs: Vec::new(),
    };
    let control = pilot.manager.agent_control();

    let lead = control
        .agent_send_satellite_message(pilot.leads[0], request("l"))
        .await
        .unwrap_err();
    assert!(lead.to_string().contains("target_not_authorized"), "{lead}");

    let receipt = control
        .agent_send_satellite_message(pilot.owner, request("m"))
        .await
        .unwrap();
    assert_eq!(receipt.state, "queued");
    assert!(!receipt.replayed);
    let replay = control
        .agent_send_satellite_message(pilot.owner, request("m"))
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.message_id, receipt.message_id);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening
)]
async fn report_to_hub_admits_only_the_appointed_manager_that_is_the_declared_seat() {
    use crate::daemon_info::DaemonInfoService;
    use rsi_common::satellite::SatelliteUuidV1;
    use rsi_common::satellite_dispatch::{
        AgentReportToHubRequestV1, SatelliteInboundPolicyV1, SatelliteReportKindV1,
        SatelliteReportStateV1,
    };

    let pilot = pilot().await;
    let hub = Uuid::new_v4();
    let request = || AgentReportToHubRequestV1 {
        kind: SatelliteReportKindV1::DeployReady,
        text: "DEPLOY-READY abc123".into(),
    };
    let control = pilot.manager.agent_control();

    // No declared seat or hub yet: everyone is refused, with static text.
    let none = control
        .agent_report_to_hub(pilot.owner, request())
        .await
        .unwrap_err();
    assert!(
        none.to_string().contains("satellite_report_not_authorized"),
        "{none}"
    );

    pilot
        .manager
        .store
        .lock()
        .await
        .put_satellite_inbound_policy(&SatelliteInboundPolicyV1 {
            allowed_hub_installations: vec![SatelliteUuidV1(hub)],
            scope_roots: vec![SatelliteUuidV1(pilot.owner)],
            ..Default::default()
        })
        .unwrap();
    let lead = control
        .agent_report_to_hub(pilot.leads[0], request())
        .await
        .unwrap_err();
    assert!(
        lead.to_string().contains("satellite_report_not_authorized"),
        "{lead}"
    );

    let receipt = control
        .agent_report_to_hub(pilot.owner, request())
        .await
        .unwrap();
    assert_eq!(receipt.state, "queued");

    // #2242: the reporting seat reads the report's delivery state in
    // `AgentGetDaemonInfo`; nobody else (a lead) gets the section.
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("rsid");
    std::fs::write(&exe, b"abc").unwrap();
    let service =
        DaemonInfoService::new(dir.path().to_path_buf(), dir.path().join("sandboxes"), exe);
    let info = control
        .agent_get_daemon_info_with(pilot.owner, &service)
        .await
        .unwrap();
    let status = info
        .hub_reports
        .expect("the reporting seat reads its reports");
    assert_eq!(status.reports.len(), 1);
    assert_eq!(status.reports[0].report_id, receipt.report_id);
    assert_eq!(status.reports[0].state, SatelliteReportStateV1::Queued);
    assert!(
        control
            .agent_get_daemon_info_with(pilot.leads[0], &service)
            .await
            .unwrap()
            .hub_reports
            .is_none()
    );

    let pulled = pilot
        .manager
        .store
        .lock()
        .await
        .take_hub_reports(hub, &[], &[])
        .unwrap();
    assert_eq!(pulled.reports.len(), 1);
    assert_eq!(pulled.reports[0].report_id, receipt.report_id);
    let acked = pilot
        .manager
        .store
        .lock()
        .await
        .take_hub_reports(hub, &[receipt.report_id], &[])
        .unwrap();
    assert!(acked.reports.is_empty());
    let info = control
        .agent_get_daemon_info_with(pilot.owner, &service)
        .await
        .unwrap();
    assert_eq!(
        info.hub_reports.unwrap().reports[0].state,
        SatelliteReportStateV1::Delivered
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening
)]
async fn report_to_hub_warns_on_a_sha_no_origin_ref_contains_and_still_queues() {
    use rsi_common::satellite::SatelliteUuidV1;
    use rsi_common::satellite_dispatch::{
        AgentReportToHubRequestV1, SatelliteInboundPolicyV1, SatelliteReportKindV1,
    };

    let pilot = pilot().await;
    let root = tempfile::tempdir().unwrap();
    let origin = root.path().join("origin.git");
    let work = root.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let git = |dir: &std::path::Path, args: &[&str]| -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    git(
        root.path(),
        &["init", "-q", "--bare", origin.to_str().unwrap()],
    );
    git(&work, &["init", "-q", "-b", "rolling"]);
    git(
        &work,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(
        &work,
        &["commit", "-q", "--allow-empty", "-m", "worker result"],
    );
    let sha = git(&work, &["rev-parse", "HEAD"]);
    {
        let store = pilot.manager.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE sessions SET working_dir=?2 WHERE id=?1",
                rusqlite::params![pilot.owner.to_string(), work.display().to_string()],
            )
            .unwrap();
        store
            .put_satellite_inbound_policy(&SatelliteInboundPolicyV1 {
                allowed_hub_installations: vec![SatelliteUuidV1(Uuid::new_v4())],
                scope_roots: vec![SatelliteUuidV1(pilot.owner)],
                ..Default::default()
            })
            .unwrap();
    }
    let request = |text: String| AgentReportToHubRequestV1 {
        kind: SatelliteReportKindV1::Result,
        text,
    };
    let control = pilot.manager.agent_control();

    // Unpublished: queued all the same, with the warning.
    let unpublished = control
        .agent_report_to_hub(pilot.owner, request(format!("ENQUEUE {sha} local254")))
        .await
        .unwrap();
    assert_eq!(unpublished.state, "queued");
    assert_eq!(
        unpublished.warnings,
        vec![format!("sha_not_on_origin: {sha}")]
    );

    // Published to the worker branch: the same report has no warning.
    git(
        &work,
        &["push", "-q", "origin", "HEAD:refs/heads/rsi/worker"],
    );
    let published = control
        .agent_report_to_hub(pilot.owner, request(format!("ENQUEUE {sha} local254")))
        .await
        .unwrap();
    assert_eq!(published.state, "queued");
    assert!(published.warnings.is_empty(), "{:?}", published.warnings);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening
)]
async fn report_to_hub_refuses_a_revoked_manager_appointment() {
    use rsi_common::satellite::SatelliteUuidV1;
    use rsi_common::satellite_dispatch::{
        AgentReportToHubRequestV1, SatelliteInboundPolicyV1, SatelliteReportKindV1,
    };

    let pilot = pilot().await;
    let hub = Uuid::new_v4();
    let request = || AgentReportToHubRequestV1 {
        kind: SatelliteReportKindV1::Result,
        text: "RESULT abc123".into(),
    };
    let control = pilot.manager.agent_control();
    {
        let store = pilot.manager.store.lock().await;
        store
            .put_satellite_inbound_policy(&SatelliteInboundPolicyV1 {
                allowed_hub_installations: vec![SatelliteUuidV1(hub)],
                scope_roots: vec![SatelliteUuidV1(pilot.owner)],
                ..Default::default()
            })
            .unwrap();
    }
    // Unrevoked: still admitted.
    control
        .agent_report_to_hub(pilot.owner, request())
        .await
        .unwrap();

    // The operator revokes the manager (explicit empty scope): the session is
    // still the manager's session, but the appointment is over.
    pilot
        .manager
        .store
        .lock()
        .await
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: pilot.config.project_id,
            session_id: pilot.owner,
            epic_ids: Some(Vec::new()),
            expected_row_version: pilot.config.row_version,
        })
        .unwrap();
    let revoked = control
        .agent_report_to_hub(pilot.owner, request())
        .await
        .unwrap_err();
    assert!(
        revoked
            .to_string()
            .contains("satellite_report_not_authorized"),
        "{revoked}"
    );
    let queued = pilot
        .manager
        .store
        .lock()
        .await
        .take_hub_reports(hub, &[], &[])
        .unwrap();
    assert_eq!(queued.reports.len(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
#[allow(clippy::unwrap_used, clippy::significant_drop_tightening)]
async fn topology_land_is_admitted_before_the_queue_is_consulted() {
    use crate::topology::land::{LandEnqueue, LandRequest};

    let pilot = pilot().await;
    let request = LandRequest {
        execution_id: Uuid::new_v4(),
        attempt_id: Uuid::new_v4(),
        dedup_key: "attempt-1".into(),
        project_id: None,
        epic_id: Some(pilot.epics[0]),
        repo_root: std::env::temp_dir(),
        source_commit: "a".repeat(40),
        acceptance: crate::topology::land::LandAcceptance::Review {
            node: "Review".into(),
            assignment_id: Uuid::new_v4(),
        },
        author_session_id: pilot.leads[0],
        test_filters: Vec::new(),
        review_coverage: None,
    };
    // No execution owns this landing: the answer is an admission loss even
    // with the queue disabled, and nothing is queued.
    for enabled in [true, false] {
        let answer = pilot
            .manager
            .agent_control()
            .enqueue_for_topology(&request, enabled)
            .await
            .unwrap();
        assert!(
            matches!(answer, LandEnqueue::AdmissionLost(_)),
            "{answer:?}"
        );
    }
    let store = pilot.manager.store.lock().await;
    assert!(
        store
            .list_rolling_queue_entries(None, 10)
            .unwrap()
            .is_empty()
    );
}

/// #2055: the operator's `EnqueueRollingQueueSource` admits exactly what the
/// agent enqueue admits (the same probe, filter derivation and insert), reports
/// the source's standing without enqueueing on a dry run, replays by key, and
/// refuses a source `origin/rolling` already contains.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn operator_enqueue_admits_what_the_agent_enqueue_admits() {
    use rsi_common::rolling_queue::{
        AgentEnqueueLandingSourceRequestV1, EnqueueRollingQueueSourceParams,
    };
    let pilot = pilot().await;
    let repo = crate::test_support::disk_backed_tempdir("operator-enqueue");
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    git(&["init", "-q", "-b", "work"]);
    git(&["config", "user.email", "test@example.invalid"]);
    git(&["config", "user.name", "test"]);
    let mut commits = Vec::new();
    for message in ["one", "two", "three", "four"] {
        git(&["commit", "-q", "--allow-empty", "-m", message]);
        commits.push(git(&["rev-parse", "HEAD"]));
    }
    git(&["update-ref", "refs/remotes/origin/rolling", &commits[1]]);
    // A side commit no queue entry names yet (the project-move case below).
    git(&["commit", "-q", "--allow-empty", "-m", "side"]);
    let side = git(&["rev-parse", "HEAD"]);
    git(&["reset", "-q", "--hard", &commits[3]]);
    let worker = pilot.leads[0];
    {
        let store = pilot.manager.store.lock().await;
        for session in [pilot.owner, worker] {
            store
                .conn
                .execute(
                    "UPDATE sessions SET working_dir=?2 WHERE id=?1",
                    rusqlite::params![session.to_string(), repo.path().display().to_string()],
                )
                .unwrap();
        }
        store
            .conn
            .execute(
                "UPDATE sessions SET sandbox_branch='rsi/operator-land' WHERE id=?1",
                rusqlite::params![worker.to_string()],
            )
            .unwrap();
    }
    let control = pilot.manager.agent_control();
    let by_agent = control
        .agent_enqueue_landing_source(
            pilot.owner,
            AgentEnqueueLandingSourceRequestV1 {
                supersedes: None,
                source_commit: commits[2].clone(),
                idempotency_key: "agent".into(),
                derive_filters: true,
                fixes_issue: None,
                fixes_tests: Vec::new(),
                test_filters: Vec::new(),
                project_id: None,
                source_session_id: None,
            },
            true,
        )
        .await
        .unwrap()
        .entry;
    let named =
        |commit: Option<&str>, key: Option<&str>, dry_run: bool| EnqueueRollingQueueSourceParams {
            session_id: None,
            branch: Some("rsi/operator-land".into()),
            commit: commit.map(str::to_owned),
            derive_filters: true,
            test_filters: Vec::new(),
            idempotency_key: key.map(str::to_owned),
            dry_run,
        };

    // A dry run names the branch tip and where it stands, and queues nothing.
    let preview = control
        .operator_enqueue_landing_source(named(None, None, true), true)
        .await
        .unwrap();
    assert!(preview.entry.is_none());
    assert_eq!(preview.source.session_id, worker);
    assert_eq!(preview.source.commit, commits[3]);
    assert_eq!(preview.source.ahead, Some(2));
    assert!(!preview.source.on_rolling && !preview.source.dirty);
    {
        let store = pilot.manager.store.lock().await;
        assert_eq!(
            store.list_rolling_queue_entries(None, 10).unwrap().len(),
            1,
            "only the agent's entry is queued"
        );
    }

    // The operator entry is the agent entry's twin: same state, binding,
    // derived filters and facts; only the identity of the source differs.
    let queued = control
        .operator_enqueue_landing_source(named(None, Some("operator"), false), true)
        .await
        .unwrap();
    let by_operator = queued.entry.expect("the entry");
    assert!(!queued.replayed);
    assert_eq!(by_operator.source_commit, commits[3]);
    assert_eq!(by_operator.source_session_id, worker);
    assert_eq!(by_operator.state, by_agent.state);
    assert_eq!(by_operator.binding, by_agent.binding);
    assert_eq!(by_operator.test_filters, by_agent.test_filters);
    assert_eq!(by_operator.hot_files, by_agent.hot_files);
    // #2112: queue-derived filters owe the async full-library QA on publish.
    for entry in [by_agent.id, by_operator.id] {
        assert!(!crate::rolling_queue::has_explicit_filters(entry));
    }
    assert_eq!(by_operator.migration_version, by_agent.migration_version);

    // The same key replays the same entry.
    let again = control
        .operator_enqueue_landing_source(named(None, Some("operator"), false), true)
        .await
        .unwrap();
    assert!(again.replayed);
    assert_eq!(again.entry.map(|entry| entry.id), Some(by_operator.id));

    // #2073: a project move during the probe re-resolves the project and the
    // manager seat that hears the outcome instead of keeping the former one.
    let seat_of = async |entry_id: Uuid| {
        let store = pilot.manager.store.lock().await;
        store
            .conn
            .query_row(
                "SELECT project_id, wake_session_id FROM rolling_queue_entries WHERE id=?1",
                rusqlite::params![entry_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .unwrap()
    };
    let (before_project, before_wake) = seat_of(by_operator.id).await;
    assert!(before_project.is_some() && before_wake.is_some());
    crate::session::effect_fence::seam::install(worker, "operator_enqueue", move |store| {
        store
            .conn
            .execute(
                "UPDATE sessions SET project_id=NULL WHERE id=?1",
                rusqlite::params![worker.to_string()],
            )
            .unwrap();
    });
    let moved = control
        .operator_enqueue_landing_source(named(Some(&side), Some("moved"), false), true)
        .await
        .unwrap()
        .entry
        .expect("the moved entry");
    assert_eq!(
        seat_of(moved.id).await,
        (None, None),
        "the outcome is not routed to the former project's manager"
    );

    // Refusals: a source already on rolling, a disabled queue, an ambiguous name.
    let landed = control
        .operator_enqueue_landing_source(named(Some(&commits[0]), Some("landed"), false), true)
        .await
        .unwrap_err();
    assert!(
        landed
            .to_string()
            .contains("queue_source_already_on_rolling")
    );
    let disabled = control
        .operator_enqueue_landing_source(named(None, Some("off"), false), false)
        .await
        .unwrap_err();
    assert!(disabled.to_string().contains("queue_disabled"));
    let mut both = named(None, None, true);
    both.session_id = Some(worker);
    let ambiguous = control
        .operator_enqueue_landing_source(both, true)
        .await
        .unwrap_err();
    assert!(ambiguous.to_string().contains("queue_source_invalid"));
    let stranger = EnqueueRollingQueueSourceParams {
        branch: Some("rsi/no-such-branch".into()),
        ..named(None, None, true)
    };
    let unknown = control
        .operator_enqueue_landing_source(stranger, true)
        .await
        .unwrap_err();
    assert!(unknown.to_string().contains("queue_source_invalid"));

    // #2069: once the source has landed, its key still replays the entry it
    // queued, but a different landed commit under that key is a conflict.
    git(&["update-ref", "refs/remotes/origin/rolling", &commits[3]]);
    let replayed = control
        .operator_enqueue_landing_source(named(None, Some("operator"), false), true)
        .await
        .unwrap();
    assert!(replayed.replayed);
    assert_eq!(replayed.entry.map(|entry| entry.id), Some(by_operator.id));
    let other = control
        .operator_enqueue_landing_source(named(Some(&commits[2]), Some("operator"), false), true)
        .await
        .unwrap_err();
    assert!(other.to_string().contains("queue_idempotency_conflict"));
}
