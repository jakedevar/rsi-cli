//! #2477: a satisfied predicate wake whose delivery is refused (the target is
//! mid-turn, the provider circuit is open, any other refusal) is retained and
//! delivered once the refusal clears; its one-shot is never consumed first.

use super::wake_when_tests::{arm, finish, insert_job, jobs_terminal};
use super::*;
use rsi_common::agent_jobs::JobState;
use rsi_common::types::ScheduledJob;
use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;

/// Replays scripted refusals, then accepts. Records every accepted delivery.
struct ScriptedLauncher {
    refusals: StdMutex<VecDeque<DaemonError>>,
    attempts: StdMutex<usize>,
    deliveries: StdMutex<Vec<(Uuid, String)>>,
}

impl ScriptedLauncher {
    fn new(refusals: Vec<DaemonError>) -> Arc<Self> {
        Arc::new(Self {
            refusals: StdMutex::new(refusals.into()),
            attempts: StdMutex::new(0),
            deliveries: StdMutex::new(Vec::new()),
        })
    }

    fn delivered(&self) -> Vec<(Uuid, String)> {
        self.deliveries.lock().unwrap().clone()
    }

    fn attempts(&self) -> usize {
        *self.attempts.lock().unwrap()
    }
}

#[async_trait::async_trait]
impl SessionLauncher for ScriptedLauncher {
    async fn launch(&self, _: LaunchConfig) -> crate::error::Result<Uuid> {
        panic!("a predicate wake never launches");
    }

    async fn launch_scheduled_fresh(&self, _: LaunchConfig, _: bool) -> crate::error::Result<Uuid> {
        panic!("a predicate wake never launches Fresh");
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
        *self.attempts.lock().unwrap() += 1;
        if let Some(refusal) = self.refusals.lock().unwrap().pop_front() {
            return Err(refusal);
        }
        self.deliveries.lock().unwrap().push((target, message));
        Ok(target)
    }

    async fn fire_watch(&self, _: &ScheduledJob) -> crate::error::Result<WatchFireOutcome> {
        Ok(WatchFireOutcome::NotReady)
    }
}

fn busy() -> DaemonError {
    DaemonError::InvalidParam(format!(
        "{}:{}",
        crate::store::manager_actions::fence::CONTINUATION_TARGET_BUSY,
        Uuid::new_v4()
    ))
}

fn circuit_open() -> DaemonError {
    DaemonError::PolicyDenied("provider circuit is open".into())
}

struct Fixture {
    store: Arc<Mutex<Store>>,
    bus: Arc<EventBus>,
    owner: Uuid,
}

impl Fixture {
    fn new() -> Self {
        Self {
            store: Arc::new(Mutex::new(Store::open_in_memory().expect("store"))),
            bus: Arc::new(EventBus::new(64)),
            owner: Uuid::new_v4(),
        }
    }

    /// A finished job and a predicate wake on it: the predicate is already true.
    async fn satisfied_wake(&self, name: &str) -> ScheduledJob {
        let store = self.store.lock().await;
        let job = insert_job(&store, self.owner, name);
        finish(&store, job, JobState::Succeeded, 0, None);
        arm(&store, self.owner, jobs_terminal(&[job]), None).expect("arm")
    }

    /// One fast-lane pass with the row due, as its backoff would make it.
    async fn pass(&self, launcher: &Arc<ScriptedLauncher>, wakes: &[&ScheduledJob]) {
        let dynamic: Arc<dyn SessionLauncher> = launcher.clone();
        {
            let store = self.store.lock().await;
            for wake in wakes {
                store
                    .defer_wake_when(wake.id, Utc::now() - chrono::Duration::seconds(1))
                    .expect("make due");
            }
        }
        process_due_wake_when(&self.store, &self.bus, &dynamic).await;
    }

    async fn enabled(&self, wake: &ScheduledJob) -> bool {
        self.store
            .lock()
            .await
            .get_scheduled_job(&wake.id)
            .unwrap()
            .expect("the wake row is never deleted before delivery")
            .enabled
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_wake_that_fires_while_its_target_is_mid_turn_is_delivered_once_after_the_turn() {
    let world = Fixture::new();
    let wake = world.satisfied_wake("check").await;
    // Busy for far longer than the retry budget (7 attempts) allows.
    let launcher = ScriptedLauncher::new((0..12).map(|_| busy()).collect());

    for pass in 0..12 {
        world.pass(&launcher, &[&wake]).await;
        assert!(world.enabled(&wake).await, "armed after busy pass {pass}");
        assert!(launcher.delivered().is_empty());
    }
    // The turn ends: the next pass delivers, and only that one.
    world.pass(&launcher, &[&wake]).await;
    let delivered = launcher.delivered();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].0, world.owner);
    assert!(!world.enabled(&wake).await, "spent once delivered");

    world.pass(&launcher, &[&wake]).await;
    assert_eq!(launcher.delivered().len(), 1, "exactly once");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn an_open_provider_circuit_holds_the_wake_until_it_closes() {
    let world = Fixture::new();
    let wake = world.satisfied_wake("check").await;
    let launcher = ScriptedLauncher::new((0..10).map(|_| circuit_open()).collect());

    for _ in 0..10 {
        world.pass(&launcher, &[&wake]).await;
        assert!(world.enabled(&wake).await);
    }
    assert!(launcher.delivered().is_empty());
    let held = world
        .store
        .lock()
        .await
        .get_scheduled_job(&wake.id)
        .unwrap()
        .unwrap();
    assert!(held.next_fire_at > Utc::now(), "re-checked later, not spun");

    world.pass(&launcher, &[&wake]).await;
    assert_eq!(launcher.delivered().len(), 1);
    assert!(!world.enabled(&wake).await);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn three_wakes_firing_together_at_a_turn_boundary_are_all_delivered() {
    let world = Fixture::new();
    let wakes = [
        world.satisfied_wake("a").await,
        world.satisfied_wake("b").await,
        world.satisfied_wake("c").await,
    ];
    // All three find the turn still open; each delivery then opens a new
    // turn that makes the next wake wait once more.
    let launcher = ScriptedLauncher::new(vec![
        busy(),
        busy(),
        busy(),
        // pass 2: first accepted, the other two see its turn
        busy(),
        busy(),
    ]);
    let refs: Vec<&ScheduledJob> = wakes.iter().collect();
    for _ in 0..6 {
        let live: Vec<&ScheduledJob> = {
            let mut live = Vec::new();
            for wake in &refs {
                if world.enabled(wake).await {
                    live.push(*wake);
                }
            }
            live
        };
        world.pass(&launcher, &live).await;
    }
    let delivered = launcher.delivered();
    assert_eq!(delivered.len(), 3, "every fired predicate is delivered");
    for wake in &wakes {
        assert!(!world.enabled(wake).await);
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn an_unclassified_refusal_retains_the_wake_and_exhaustion_escalates_it() {
    let world = Fixture::new();
    let wake = world.satisfied_wake("check").await;
    // The store's CONTINUATION_RETRY_MAX_ATTEMPTS.
    let limit = 7;
    let launcher = ScriptedLauncher::new(
        (0..=limit)
            .map(|_| DaemonError::Process("provider exploded".into()))
            .collect(),
    );

    for pass in 0..limit {
        world.pass(&launcher, &[&wake]).await;
        assert!(world.enabled(&wake).await, "retained after refusal {pass}");
    }
    // The budget runs out: the wake settles, but only behind a durable owner.
    world.pass(&launcher, &[&wake]).await;
    assert!(!world.enabled(&wake).await);
    assert_eq!(launcher.attempts(), limit + 1);
    assert!(launcher.delivered().is_empty());
    let key = format!("gone-wake:{}", wake.id);
    let todos = world
        .store
        .lock()
        .await
        .operator_todo_list(None)
        .unwrap()
        .open;
    assert_eq!(
        todos.iter().filter(|todo| todo.todo_key == key).count(),
        1,
        "the undelivered result has a durable owner"
    );
}

async fn open_todo_count(world: &Fixture, wake: &ScheduledJob) -> usize {
    let key = format!("gone-wake:{}", wake.id);
    world
        .store
        .lock()
        .await
        .operator_todo_list(None)
        .unwrap()
        .open
        .iter()
        .filter(|todo| todo.todo_key == key)
        .count()
}

/// #2487: when the exhausted wake cannot even be classified (a store read
/// error), the satisfied predicate stays enabled and due, with no escalation
/// and no settlement; a healthy tick then escalates and settles it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn an_unreadable_predicate_classification_keeps_an_exhausted_wake_due() {
    let world = Fixture::new();
    let wake = world.satisfied_wake("check").await;
    let limit = 7;
    let launcher = ScriptedLauncher::new(
        (0..limit + 4)
            .map(|_| DaemonError::Process("provider exploded".into()))
            .collect(),
    );
    for _ in 0..limit {
        world.pass(&launcher, &[&wake]).await;
    }
    install_fault_for_test(wake.id, "classify");
    world.pass(&launcher, &[&wake]).await;
    assert!(world.enabled(&wake).await, "not settled on a read error");
    assert_eq!(open_todo_count(&world, &wake).await, 0, "nothing escalated");
    assert!(launcher.delivered().is_empty());

    world.pass(&launcher, &[&wake]).await;
    assert!(!world.enabled(&wake).await, "settled once it has an owner");
    assert_eq!(
        open_todo_count(&world, &wake).await,
        1,
        "the undelivered result has a durable owner"
    );
}

/// #2487: a failed retry write must not consume a satisfied predicate wake.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_failed_retry_record_keeps_a_predicate_wake_enabled_and_due() {
    let world = Fixture::new();
    let wake = world.satisfied_wake("check").await;
    let launcher = ScriptedLauncher::new(
        (0..4)
            .map(|_| DaemonError::Process("provider exploded".into()))
            .collect(),
    );
    install_fault_for_test(wake.id, "record_retry");
    world.pass(&launcher, &[&wake]).await;
    assert!(world.enabled(&wake).await, "a one-shot is never consumed");
    assert!(
        world
            .store
            .lock()
            .await
            .wake_when_state(wake.id)
            .unwrap()
            .is_some()
    );
    assert_eq!(open_todo_count(&world, &wake).await, 0);

    // A healthy tick retries it and records the attempt.
    world.pass(&launcher, &[&wake]).await;
    assert_eq!(launcher.attempts(), 2);
    assert!(world.enabled(&wake).await);
    assert_eq!(
        world
            .store
            .lock()
            .await
            .continuation_retry(wake.id)
            .unwrap()
            .map(|retry| retry.attempts),
        Some(1)
    );
}

fn usage_window_refusal() -> DaemonError {
    DaemonError::PolicyDenied(format!(
        "Invalid parameter: {}",
        crate::provider_exhaustion::PROVIDER_USAGE_WINDOW_REFUSAL_CODE
    ))
}

/// A Claude worker session as the wake's target, and an exhausted five-hour
/// window that resets at `reset`.
async fn exhaust_claude_window(world: &Fixture, reset: chrono::DateTime<Utc>) {
    use rsi_common::rpc::{ProviderRateLimitSnapshot, ProviderRateLimitWindow};
    use rsi_common::types::SessionProvider;
    let store = world.store.lock().await;
    let mut session = rsid_store::test_support::make_test_session();
    session.id = world.owner;
    session.provider = SessionProvider::Claude;
    store.insert_session(&session).unwrap();
    store
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
                    utilization: 1.0,
                    resets_at_epoch: Some(reset.timestamp()),
                }],
                stale_reason: None,
            },
            None,
        )
        .unwrap();
}

/// #3066: a parked worker's `when` wake refused because the provider's plan
/// window is exhausted stays armed through the whole outage: no retry budget
/// is spent however many passes the outage lasts, nothing is escalated or
/// settled, the wake is held to the window's reset, and the result is
/// delivered once, when delivery succeeds.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_usage_window_outage_holds_the_wake_without_spending_its_retry_budget() {
    let world = Fixture::new();
    let wake = world.satisfied_wake("scoped-test-done").await;
    let reset = Utc::now() + chrono::Duration::hours(5);
    exhaust_claude_window(&world, reset).await;
    // Far more refusals than the retry budget (7) tolerates.
    let launcher = ScriptedLauncher::new((0..20).map(|_| usage_window_refusal()).collect());

    for pass in 0..20 {
        world.pass(&launcher, &[&wake]).await;
        assert!(world.enabled(&wake).await, "armed after outage pass {pass}");
        assert!(launcher.delivered().is_empty());
        let guard = world.store.lock().await;
        assert!(
            guard.continuation_retry(wake.id).unwrap().is_none(),
            "an outage spends no retry attempt (pass {pass})"
        );
        let held = guard.get_scheduled_job(&wake.id).unwrap().unwrap();
        assert!(
            (held.next_fire_at - reset).num_seconds().abs() <= 1,
            "held to the window's reset, got {}",
            held.next_fire_at
        );
    }
    assert_eq!(open_todo_count(&world, &wake).await, 0, "nothing escalated");

    // The window resets and the provider accepts the resume: delivered once.
    world.pass(&launcher, &[&wake]).await;
    world.pass(&launcher, &[&wake]).await;
    let delivered = launcher.delivered();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].0, world.owner);
    assert!(!world.enabled(&wake).await, "spent once delivered");
    assert_eq!(open_todo_count(&world, &wake).await, 0);
}

/// #3066: with no readable window the hold falls back to a short re-check; it
/// is still not a retry attempt.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_usage_window_refusal_with_no_readable_window_rechecks_soon_without_an_attempt() {
    let world = Fixture::new();
    let wake = world.satisfied_wake("scoped-test-done").await;
    let launcher = ScriptedLauncher::new(vec![usage_window_refusal()]);

    world.pass(&launcher, &[&wake]).await;

    assert!(world.enabled(&wake).await);
    let guard = world.store.lock().await;
    assert!(guard.continuation_retry(wake.id).unwrap().is_none());
    let held = guard.get_scheduled_job(&wake.id).unwrap().unwrap();
    assert!(held.next_fire_at > Utc::now() + chrono::Duration::minutes(4));
    assert!(held.next_fire_at <= Utc::now() + chrono::Duration::minutes(6));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn permanent_ownership_failure_settles_immediately_behind_one_recovery_notice() {
    let world = Fixture::new();
    let wake = world.satisfied_wake("check").await;
    let launcher = ScriptedLauncher::new(vec![crate::error::sandbox_custody_error(
        rsi_common::types::SandboxCustodyErrorV1 {
            version: 1,
            code: rsi_common::types::SandboxCustodyErrorCodeV1::OwnershipMissing,
            session_id: Some(world.owner),
            transition: rsi_common::types::SandboxCustodyTransitionV1::ResumeWake,
            retryable: false,
            recovery: rsi_common::types::SandboxCustodyRecoveryV1::CreateNewSession,
        },
    )]);
    world.pass(&launcher, &[&wake]).await;
    assert!(!world.enabled(&wake).await);
    world.pass(&launcher, &[&wake]).await;
    assert_eq!(launcher.attempts(), 1);
    assert_eq!(open_todo_count(&world, &wake).await, 1);
    let guard = world.store.lock().await;
    assert!(guard.continuation_retry(wake.id).unwrap().is_none());
    let todo = guard
        .operator_todo_list(None)
        .unwrap()
        .open
        .into_iter()
        .find(|todo| todo.todo_key == format!("gone-wake:{}", wake.id))
        .unwrap();
    assert!(todo.how.contains("wake_recoverable"));
    assert!(todo.how.contains("ownership_missing"));
    assert!(todo.how.contains("sandbox_source"));
}
