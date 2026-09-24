use super::*;
use rsi_common::harness_manager::*;
use rsi_common::types::{Project, ScheduledJob, WakeMode};

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
    let WatchFirePlan::Deliver { tip, job_ids, .. } =
        pilot.manager.plan_terminal_watch_fire(&job).await.unwrap()
    else {
        panic!("manager-bound self route must deliver to the idle current manager");
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

#[tokio::test]
async fn running_lead_reply_wakes_idle_manager_once_until_inbox_retrieval() {
    let pilot = pilot().await;
    drain_manager_inbox(&pilot).await;
    set_status(&pilot, pilot.leads[0], SessionStatus::Running).await;
    let request = manager_request(&pilot, "status-1").await;

    let first = lead_reply(&pilot, request.message_id, "reply-1").await;
    let job = route_job(&pilot, pilot.leads[0], pilot.owner).await;
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
    assert!(
        message.starts_with("1 durable manager notices pending; read AgentManagerInbox"),
        "{message}"
    );
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

#[tokio::test]
async fn running_manager_holds_lead_reply_until_it_goes_idle() {
    let pilot = pilot().await;
    drain_manager_inbox(&pilot).await;
    set_status(&pilot, pilot.leads[0], SessionStatus::Running).await;
    let request = manager_request(&pilot, "status-busy").await;
    set_status(&pilot, pilot.owner, SessionStatus::Running).await;
    let reply = lead_reply(&pilot, request.message_id, "reply-busy").await;
    let job = route_job(&pilot, pilot.leads[0], pilot.owner).await;
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
#[tokio::test]
async fn failed_manager_seat_keeps_lead_reply_pending_until_recovered() {
    let pilot = pilot().await;
    drain_manager_inbox(&pilot).await;
    let request = manager_request(&pilot, "seat-down").await;
    let reply = lead_reply(&pilot, request.message_id, "seat-down-reply").await;
    set_status(&pilot, pilot.leads[0], SessionStatus::Completed).await;
    set_status(&pilot, pilot.owner, SessionStatus::Failed).await;
    let job = route_job(&pilot, pilot.leads[0], pilot.owner).await;
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

    #[tokio::test]
    async fn rearm_after_capture_makes_legacy_manager_abandonment_stale() {
        stale_legacy_manager_capture_settles_nothing(false).await;
    }

    #[tokio::test]
    async fn disable_after_capture_makes_legacy_manager_abandonment_stale() {
        stale_legacy_manager_capture_settles_nothing(true).await;
    }

    fn reopen(directory: &TempDir) -> Arc<SessionManager> {
        let config = Config::from_env();
        Arc::new(
            SessionManager::new(
                Arc::new(EventBus::new(64)),
                Store::open(&directory.path().join("rsi.db")).expect("reopen store"),
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

        // Past the window: one atomic give-up.
        age_child(UNCONSUMED_MINUTES).await;
        let events = fire_through_scheduler(&manager, &deferred).await;
        assert!(!job_enabled(&manager, job.id).await);
        assert_eq!(
            warnings(&events),
            vec![abandon_warning(&job, lead, child, UNCONSUMED_MINUTES)]
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
