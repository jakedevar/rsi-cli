//! #2451: a result wake whose target is gone escalates durably instead of
//! retiring with only a log line and a bus message.

use super::*;
use crate::store::agent_jobs::NewAgentJob;
use crate::store::manager_coordinator::tests::fixture;
use rsi_common::agent_jobs::{
    AgentJobResultV1, BuildCommand, BuildJobParams, JobParams, JobState, JobWake,
};
use rsi_common::types::ScheduledJob;
use std::sync::Mutex as StdMutex;

/// Refuses `gone` as `SessionNotFound` and records every other delivery.
struct GoneLauncher {
    gone: Uuid,
    deliveries: StdMutex<Vec<(Uuid, String)>>,
}

#[async_trait::async_trait]
impl SessionLauncher for GoneLauncher {
    async fn launch(&self, _: LaunchConfig) -> crate::error::Result<Uuid> {
        panic!("a Resume wake never launches");
    }

    async fn launch_scheduled_fresh(&self, _: LaunchConfig, _: bool) -> crate::error::Result<Uuid> {
        panic!("a Resume wake never launches Fresh");
    }

    async fn resume_scheduled(&self, target: Uuid, message: String) -> crate::error::Result<Uuid> {
        self.resume_scheduled_job(target, message, Vec::new()).await
    }

    async fn resume_scheduled_job(
        &self,
        target: Uuid,
        message: String,
        _: Vec<Uuid>,
    ) -> crate::error::Result<Uuid> {
        if target == self.gone {
            return Err(DaemonError::SessionNotFound(target));
        }
        self.deliveries.lock().unwrap().push((target, message));
        Ok(target)
    }

    async fn fire_watch(&self, _: &ScheduledJob) -> crate::error::Result<WatchFireOutcome> {
        Ok(WatchFireOutcome::NotReady)
    }
}

fn failed_job_wake(store: &Store, owner: Uuid, project: Uuid) -> ScheduledJob {
    let id = Uuid::new_v4();
    let new = NewAgentJob {
        id,
        owner_session_id: owner,
        project_id: Some(project),
        name: Some("check".into()),
        params: JobParams::Build(BuildJobParams {
            command: BuildCommand::Check,
            package: None,
            workspace: true,
            all_targets: false,
            release: false,
            no_default_features: false,
            features: Vec::new(),
            shards: Vec::new(),
            cpu_quota_percent: None,
            sha: None,
        }),
        cwd: "/tmp/sandbox".into(),
        unit_name: format!("rsi-job-{id}"),
        log_path: format!("/tmp/jobs/{id}.log"),
        status_path: format!("/tmp/jobs/{id}.status"),
        idempotency_key: None,
        wake: JobWake::Owner,
    };
    store.insert_agent_job(&new, Utc::now()).unwrap();
    let wake = store
        .settle_agent_job(
            id,
            JobState::Failed,
            &AgentJobResultV1 {
                exit_code: Some(101),
                ..AgentJobResultV1::default()
            },
            true,
            Utc::now(),
        )
        .unwrap()
        .unwrap();
    store.get_scheduled_job(&wake).unwrap().unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_gone_target_wake_escalates_once_to_the_seat_across_restart_and_refire() {
    let store = Store::open_in_memory().expect("store");
    let (config, _) = fixture(&store, Default::default());
    let seat = config.current_session_id.unwrap();
    let gone = Uuid::new_v4();
    let wake = failed_job_wake(&store, gone, config.project_id);
    let store = Arc::new(Mutex::new(store));

    // Two scheduler lifetimes (fresh bus and launcher each) fire the same
    // due row; the second models a restart before the one-shot settled.
    for _ in 0..2 {
        let bus = Arc::new(EventBus::new(16));
        let launcher = Arc::new(GoneLauncher {
            gone,
            deliveries: StdMutex::new(Vec::new()),
        });
        let dynamic: Arc<dyn SessionLauncher> = launcher.clone();
        fire_job_for_test(&store, &bus, &dynamic, &wake).await;
        assert!(
            launcher.deliveries.lock().unwrap().is_empty(),
            "the gone target is never resumed"
        );
    }

    let guard = store.lock().await;
    let jobs = guard.list_scheduled_jobs().unwrap();
    let escalations: Vec<_> = jobs
        .iter()
        .filter(|job| job.name == format!("gone-wake-escalation:{}", wake.id))
        .collect();
    assert_eq!(escalations.len(), 1, "one durable escalation");
    assert_eq!(escalations[0].wake_session_id, Some(seat));
    assert!(escalations[0].message.contains(&wake.id.to_string()));
    assert!(escalations[0].message.contains(&gone.to_string()));
    let original = jobs.iter().find(|job| job.id == wake.id).unwrap();
    assert!(!original.enabled, "the one-shot is still settled");
    drop(guard);

    // The seat receives it through the ordinary wake path.
    let bus = Arc::new(EventBus::new(16));
    let launcher = Arc::new(GoneLauncher {
        gone,
        deliveries: StdMutex::new(Vec::new()),
    });
    let dynamic: Arc<dyn SessionLauncher> = launcher.clone();
    let escalation = escalations[0].clone();
    fire_job_for_test(&store, &bus, &dynamic, &escalation).await;
    let delivered = launcher.deliveries.lock().unwrap().clone();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].0, seat);
    assert!(delivered[0].1.contains(&wake.id.to_string()));
}

/// #2473: a full operator to-do list is capacity, not a verdict. The wake
/// stays armed with a later fire time; once the operator closes an item the
/// re-fire creates the escalation to-do and settles the wake exactly once.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_full_operator_todo_list_defers_the_gone_target_wake_until_it_drains() {
    use rsi_common::operator_todos::{
        OPERATOR_TODO_MAX_OPEN_PER_SCOPE, OperatorTodoCloseRequestV1, OperatorTodoStatusV1,
    };

    let store = Store::open_in_memory().expect("store");
    let gone = Uuid::new_v4();
    // A project with no manager seat routes the escalation to the operator list.
    let wake = failed_job_wake(&store, gone, Uuid::new_v4());
    let mut filler = Vec::new();
    for index in 0..OPERATOR_TODO_MAX_OPEN_PER_SCOPE {
        let write = store
            .operator_todo_system_notice(&format!("fill-{index}"), None, "filler", "filler", 3)
            .unwrap();
        filler.push(write.todo.id);
    }
    let store = Arc::new(Mutex::new(store));
    let bus = Arc::new(EventBus::new(16));
    let launcher = Arc::new(GoneLauncher {
        gone,
        deliveries: StdMutex::new(Vec::new()),
    });
    let dynamic: Arc<dyn SessionLauncher> = launcher.clone();

    fire_job_for_test(&store, &bus, &dynamic, &wake).await;
    {
        let guard = store.lock().await;
        let held = guard.get_scheduled_job(&wake.id).unwrap().unwrap();
        assert!(held.enabled, "the wake stays armed while the list is full");
        assert!(
            held.next_fire_at > Utc::now() + chrono::Duration::minutes(1),
            "the retry is backed off: {}",
            held.next_fire_at
        );
        assert_eq!(
            guard.operator_todo_list(None).unwrap().open.len(),
            filler.len(),
            "no escalation to-do fit"
        );
    }

    // The operator closes one item; the re-fire escalates and settles once.
    store
        .lock()
        .await
        .operator_todo_operator_close(&OperatorTodoCloseRequestV1 {
            todo_id: filler[0],
            status: OperatorTodoStatusV1::Done,
            note: None,
        })
        .unwrap();
    for _ in 0..2 {
        fire_job_for_test(&store, &bus, &dynamic, &wake).await;
    }
    let guard = store.lock().await;
    let key = format!("gone-wake:{}", wake.id);
    let matching = guard
        .operator_todo_list(None)
        .unwrap()
        .open
        .into_iter()
        .filter(|todo| todo.todo_key == key)
        .count();
    assert_eq!(matching, 1, "exactly one escalation to-do");
    let original = guard.get_scheduled_job(&wake.id).unwrap().unwrap();
    assert!(
        !original.enabled,
        "the one-shot settled after the escalation"
    );
}
