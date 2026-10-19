//! Mixed wake/topology fixtures: notices originate only from scheduler effects.
use super::*;
use crate::bus::EventBus;
use crate::issue_tracker::poller::{SessionLauncher, WatchFireOutcome};
use crate::scheduler::wake_when_tests::{arm, finish, insert_job, jobs_terminal};
use chrono::{DateTime, Duration, Utc};
use rsi_common::agent_jobs::JobState;
use rsi_common::harness_manager::{
    AgentManagerInboxRequestV1, ConfigureHarnessManagerRequestV1, HarnessManagerNoticeV1,
};
use rsi_common::harness_manager_v2::{ConfigureHarnessManagerPolicyRequestV2, ManagerPolicyV2};
use rsi_common::rpc::{ProviderRateLimitSnapshot, ProviderRateLimitWindow};
use rsi_common::types::{Project, ScheduledJob, SessionKind, SessionProvider};
use std::sync::atomic::AtomicUsize;

struct WakeLauncher {
    held: AtomicBool,
    delivered: AtomicUsize,
}

#[async_trait::async_trait]
impl SessionLauncher for WakeLauncher {
    async fn launch(&self, _: crate::claude::LaunchConfig) -> Result<Uuid> {
        unreachable!()
    }
    async fn launch_scheduled_fresh(
        &self,
        _: crate::claude::LaunchConfig,
        _: bool,
    ) -> Result<Uuid> {
        unreachable!()
    }
    async fn resume_scheduled(&self, target: Uuid, _: String) -> Result<Uuid> {
        if self.held.load(Ordering::SeqCst) {
            return Err(DaemonError::PolicyDenied(
                crate::provider_exhaustion::PROVIDER_USAGE_WINDOW_REFUSAL_CODE.into(),
            ));
        }
        self.delivered.fetch_add(1, Ordering::SeqCst);
        Ok(target)
    }
    async fn fire_watch(&self, _: &ScheduledJob) -> Result<WatchFireOutcome> {
        Ok(WatchFireOutcome::NotReady)
    }
}

async fn manager_fixture(harness: &Harness) -> (Uuid, Uuid, Uuid) {
    let store = harness.executor.store.lock().await;
    let now = Utc::now();
    let project = Uuid::new_v4();
    store
        .insert_project(&Project {
            id: project,
            name: "Provider window notices".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();
    let mut manager = rsid_store::test_support::make_test_session();
    manager.project_id = Some(project);
    manager.status = SessionStatus::Running;
    store.insert_session(&manager).unwrap();
    let mut group = manager.clone();
    group.id = Uuid::new_v4();
    group.session_kind = SessionKind::Group;
    store.insert_session(&group).unwrap();
    let mut epic = group.clone();
    epic.id = Uuid::new_v4();
    epic.parent_id = Some(group.id);
    epic.session_kind = SessionKind::Epic;
    store.insert_session(&epic).unwrap();
    let mut worker = manager.clone();
    worker.id = Uuid::new_v4();
    worker.parent_id = Some(epic.id);
    worker.provider = SessionProvider::Claude;
    worker.status = SessionStatus::Completed;
    store.insert_session(&worker).unwrap();
    let config = store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: project,
            session_id: manager.id,
            group_ids: vec![],
            epic_ids: None,
            expected_row_version: 0,
        })
        .unwrap();
    store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: project,
            expected_scope_version: config.row_version,
            expected_policy_version: 0,
            idempotency_key: "provider-window-test-policy".into(),
            policy: ManagerPolicyV2::default(),
        })
        .unwrap();
    (project, manager.id, worker.id)
}

async fn window(harness: &Harness, reset: DateTime<Utc>, held: bool) {
    window_snapshot(harness, Some(reset), held).await;
}

async fn window_snapshot(harness: &Harness, reset: Option<DateTime<Utc>>, held: bool) {
    harness
        .executor
        .store
        .lock()
        .await
        .upsert_provider_rate_limit_snapshot(
            &ProviderRateLimitSnapshot {
                provider: SessionProvider::Claude,
                status: None,
                rate_limit_type: Some("five_hour".into()),
                overage_status: None,
                is_using_overage: false,
                observed_at: Utc::now(),
                windows: vec![ProviderRateLimitWindow {
                    window_key: "five_hour".into(),
                    utilization: if held { 1.0 } else { 0.0 },
                    resets_at_epoch: reset.map(|reset| reset.timestamp()),
                }],
                stale_reason: None,
            },
            None,
        )
        .unwrap();
}

async fn wakes(harness: &Harness, worker: Uuid, count: usize) -> Vec<ScheduledJob> {
    let store = harness.executor.store.lock().await;
    let mut target = worker;
    (0..count)
        .map(|index| {
            // Exercise a project-wide backlog while respecting the real
            // per-session cap on enabled predicate wakes.
            if index > 0 && index % rsi_common::wake_predicate::WAKE_WHEN_MAX_PER_SESSION == 0 {
                let mut session = store.get_session(worker).unwrap().unwrap();
                session.id = Uuid::new_v4();
                target = session.id;
                store.insert_session(&session).unwrap();
            }
            let job = insert_job(&store, target, &format!("test result {index}"));
            finish(&store, job, JobState::Succeeded, 0, None);
            arm(&store, target, jobs_terminal(&[job]), None).unwrap()
        })
        .collect()
}

async fn tick(
    harness: &Harness,
    bus: &Arc<EventBus>,
    launcher: &Arc<WakeLauncher>,
    wakes: &[ScheduledJob],
) {
    {
        let store = harness.executor.store.lock().await;
        for wake in wakes {
            store
                .defer_wake_when(wake.id, Utc::now() - Duration::seconds(1))
                .unwrap();
        }
    }
    let launcher: Arc<dyn SessionLauncher> = launcher.clone();
    crate::scheduler::process_due_wake_when(&harness.executor.store, bus, &launcher).await;
}

async fn notices(harness: &Harness, manager: Uuid) -> Vec<HarnessManagerNoticeV1> {
    harness
        .executor
        .store
        .lock()
        .await
        .manager_inbox(manager, &AgentManagerInboxRequestV1::default())
        .unwrap()
        .notices
        .into_iter()
        .filter(|notice| notice.state["record_kind"] == "provider_window")
        .collect()
}

async fn notice_count(harness: &Harness) -> i64 {
    harness.executor.store.lock().await.conn.query_row(
        "SELECT count(*) FROM harness_manager_notices WHERE subject_id LIKE 'provider_window:%'", [], |row| row.get(0),
    ).unwrap()
}

async fn start_launches(harness: &Harness, project: Uuid) -> Uuid {
    let mut graph = workflow(&["A", "B", "C"], &[]);
    for node in &mut graph.nodes {
        node.provider = Some("claude".into());
    }
    harness.start_in(graph, Some(project)).await
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn mixed_parked_work_gets_one_hold_and_recovery_per_provider_reset_episode() {
    let mut harness = Harness::new();
    let (project, manager, worker) = manager_fixture(&harness).await;
    let bus = Arc::new(EventBus::new(128));
    let launcher = Arc::new(WakeLauncher {
        held: AtomicBool::new(true),
        delivered: AtomicUsize::new(0),
    });

    for episode in 0..2 {
        let reset = Utc::now() + Duration::hours(5 + episode);
        window(&harness, reset, true).await;
        let wakes = wakes(&harness, worker, 3).await;
        let execution = start_launches(&harness, project).await;
        *harness.executor.effects.capacity_refusal.lock().unwrap() = Some("provider_usage_window");
        launcher.held.store(true, Ordering::SeqCst);
        tick(&harness, &bus, &launcher, &wakes).await;
        harness.executor.advance(execution).await.unwrap();
        let attempts = harness.attempts(execution).await;
        assert_eq!(attempts.len(), 3);
        for attempt in &attempts {
            assert_eq!(attempt.status, AttemptStatus::Reserved);
        }
        assert_eq!(notice_count(&harness).await, episode * 2 + 1);
        let held = notices(&harness, manager).await;
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].state["phase"], "hold");
        assert_eq!(held[0].state["total_count"], 6);
        let items = held[0].state["items"].as_array().unwrap();
        assert_eq!(items.len(), 6);
        for wake in &wakes {
            let item = items
                .iter()
                .find(|item| item["id"] == wake.id.to_string())
                .unwrap();
            assert_eq!(item["name"], wake.name);
            assert_eq!(item["target"], worker.to_string());
            let earliest: DateTime<Utc> =
                serde_json::from_value(item["earliest_retry_at"].clone()).unwrap();
            assert!(earliest.timestamp() >= reset.timestamp());
        }
        for attempt in &attempts {
            let item = items
                .iter()
                .find(|item| item["id"] == attempt.id.to_string())
                .unwrap();
            assert_eq!(item["kind"], "topology_launch");
            assert_eq!(item["target"], attempt.session_id.to_string());
            assert!(item["name"].as_str().unwrap().contains(&attempt.node_id));
            let earliest: DateTime<Utc> =
                serde_json::from_value(item["earliest_retry_at"].clone()).unwrap();
            assert_eq!(earliest.timestamp(), reset.timestamp());
        }
        let reread = harness
            .executor
            .store
            .lock()
            .await
            .manager_inbox(
                manager,
                &AgentManagerInboxRequestV1 {
                    read_notice_ids: vec![held[0].notice_id],
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(reread.notices.len(), 1);
        assert_eq!(reread.notices[0].notice_id, held[0].notice_id);
        assert_eq!(reread.notices[0].state["total_count"], 6);

        // Retrieval settles the hold. Replayed ticks and a daemon restart
        // retain its dedupe identity and spend no wake retry budget.
        harness.restart();
        *harness.executor.effects.capacity_refusal.lock().unwrap() = Some("provider_usage_window");
        for _ in 0..10 {
            tick(&harness, &bus, &launcher, &wakes).await;
            harness.executor.advance(execution).await.unwrap();
        }
        assert_eq!(notice_count(&harness).await, episode * 2 + 1);
        assert_eq!(notices(&harness, manager).await.len(), 0);
        for wake in &wakes {
            let store = harness.executor.store.lock().await;
            assert!(store.get_scheduled_job(&wake.id).unwrap().unwrap().enabled);
            assert_eq!(
                store
                    .continuation_retry(wake.id)
                    .unwrap()
                    .map(|retry| retry.attempts)
                    .unwrap_or(0),
                0
            );
        }

        window(&harness, reset, false).await;
        launcher.held.store(false, Ordering::SeqCst);
        tick(&harness, &bus, &launcher, &wakes).await;
        // The reset alone and three resumed wakes still leave three launches
        // held. Recovery waits for their real admission as well.
        assert_eq!(notice_count(&harness).await, episode * 2 + 1);
        *harness.executor.effects.capacity_refusal.lock().unwrap() = None;
        harness.executor.advance(execution).await.unwrap();
        tick(&harness, &bus, &launcher, &[]).await;
        let recovered = notices(&harness, manager).await;
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].state["phase"], "recovery");
        assert_eq!(recovered[0].state["resumed_count"], 6);
        assert_eq!(recovered[0].state["pending_count"], 0);
        let resumed = recovered[0].state["items"].as_array().unwrap();
        assert_eq!(resumed.len(), 6);
        for item in items {
            let found = resumed
                .iter()
                .find(|resumed| resumed["id"] == item["id"])
                .unwrap();
            assert_eq!(found["status"], "resumed");
        }
        for wake in &wakes {
            assert!(
                harness
                    .executor
                    .store
                    .lock()
                    .await
                    .get_scheduled_job(&wake.id)
                    .unwrap()
                    .unwrap()
                    .last_fired_at
                    .is_some()
            );
        }
        for attempt in harness.attempts(execution).await {
            assert_eq!(attempt.status, AttemptStatus::Running);
        }
        for _ in 0..3 {
            tick(&harness, &bus, &launcher, &wakes).await;
        }
        assert_eq!(notice_count(&harness).await, (episode + 1) * 2);
        assert_eq!(
            launcher.delivered.load(Ordering::SeqCst),
            ((episode + 1) * 3) as usize
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn provider_episode_notices_bound_lists_and_count_omitted_work() {
    let harness = Harness::new();
    let (_, manager, worker) = manager_fixture(&harness).await;
    let reset = Utc::now() + Duration::hours(5);
    window(&harness, reset, true).await;
    let wakes = wakes(&harness, worker, 35).await;
    let bus = Arc::new(EventBus::new(128));
    let launcher = Arc::new(WakeLauncher {
        held: AtomicBool::new(true),
        delivered: AtomicUsize::new(0),
    });
    for _ in 0..3 {
        tick(&harness, &bus, &launcher, &wakes).await;
    }
    let held = notices(&harness, manager).await;
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].state["items"].as_array().unwrap().len(), 32);
    assert_eq!(held[0].state["omitted_count"], 3);
    assert_eq!(held[0].state["total_count"], 35);
    window(&harness, reset, false).await;
    launcher.held.store(false, Ordering::SeqCst);
    tick(&harness, &bus, &launcher, &wakes).await;
    let recovered = notices(&harness, manager).await;
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].state["items"].as_array().unwrap().len(), 32);
    assert_eq!(recovered[0].state["omitted_count"], 3);
    assert_eq!(recovered[0].state["resumed_count"], 35);
    assert_eq!(notice_count(&harness).await, 2);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn an_unknown_window_reset_keeps_one_episode_through_rechecks() {
    let harness = Harness::new();
    let (_, manager, worker) = manager_fixture(&harness).await;
    window_snapshot(&harness, None, true).await;
    let wakes = wakes(&harness, worker, 3).await;
    let bus = Arc::new(EventBus::new(128));
    let launcher = Arc::new(WakeLauncher {
        held: AtomicBool::new(true),
        delivered: AtomicUsize::new(0),
    });
    for _ in 0..10 {
        tick(&harness, &bus, &launcher, &wakes).await;
    }
    assert_eq!(notice_count(&harness).await, 1);
    let held = notices(&harness, manager).await;
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].state["reset_is_estimate"], true);
    assert_eq!(held[0].state["total_count"], 3);
    window_snapshot(&harness, None, false).await;
    launcher.held.store(false, Ordering::SeqCst);
    tick(&harness, &bus, &launcher, &wakes).await;
    let recovered = notices(&harness, manager).await;
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].state["resumed_count"], 3);
    assert_eq!(notice_count(&harness).await, 2);
}
