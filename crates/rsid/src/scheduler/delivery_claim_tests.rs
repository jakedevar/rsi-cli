//! #797: a crash between a scheduled resume's delivery and the row's
//! settlement must not deliver the same wake twice.
//!
//! The continuation fence (K2) orders a delivery against lead and tip
//! changes; it holds no per-wake memory, so a replay after the target went
//! idle again passed every check. The scheduler therefore claims the delivery
//! durably before the launch, the continuation marks that claim in its own
//! effect-claim transaction, and the next incarnation settles a claim that
//! reached an effect as uncertain instead of delivering again.

use super::*;
use crate::store::manager_actions::fence::{ContinuationAuthorityV1, ContinuationFenceV1};
use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, Session};
use std::sync::Mutex as StdMutex;

/// A launcher that behaves like the real continuation up to its effect claim,
/// then either completes or "crashes" (the scheduler's settlement is cut by
/// the test through `crash_after_delivery`).
struct EffectLauncher {
    store: Arc<rsid_core::store_lock::Mutex<Store>>,
    /// True: take the real effect claim before delivering.
    claim_effect: bool,
    deliveries: StdMutex<Vec<(Uuid, String)>>,
}

impl EffectLauncher {
    fn new(store: &Arc<rsid_core::store_lock::Mutex<Store>>, claim_effect: bool) -> Arc<Self> {
        Arc::new(Self {
            store: store.clone(),
            claim_effect,
            deliveries: StdMutex::new(Vec::new()),
        })
    }

    fn delivered(&self) -> usize {
        self.deliveries.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl SessionLauncher for EffectLauncher {
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
        job_ids: Vec<Uuid>,
    ) -> crate::error::Result<Uuid> {
        if self.claim_effect {
            let guard = self.store.lock().await;
            let fence: ContinuationFenceV1 = guard
                .capture_continuation_fence(target, ContinuationAuthorityV1::Automated)?
                .expect("the target session exists");
            guard.claim_continuation_effect(&fence, &job_ids)?;
        }
        self.deliveries.lock().unwrap().push((target, message));
        Ok(target)
    }

    async fn fire_watch(&self, _: &ScheduledJob) -> crate::error::Result<WatchFireOutcome> {
        Ok(WatchFireOutcome::NotReady)
    }
}

struct Fixture {
    store: Arc<rsid_core::store_lock::Mutex<Store>>,
    bus: Arc<EventBus>,
    target: Uuid,
    job: ScheduledJob,
}

impl Fixture {
    async fn new(recurrence: Recurrence) -> Self {
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let target = Uuid::new_v4();
        let now = Utc::now();
        let job = ScheduledJob {
            id: Uuid::new_v4(),
            name: "wake".into(),
            message: "wake up".into(),
            schedule: ScheduleSpec {
                recurrence,
                anchor: now - chrono::Duration::seconds(120),
            },
            last_fired_at: None,
            next_fire_at: now - chrono::Duration::seconds(30),
            enabled: true,
            working_dir: None,
            provider: None,
            model: None,
            project_id: None,
            created_at: now,
            updated_at: now,
            wake_mode: WakeMode::Resume,
            wake_session_id: Some(target),
        };
        {
            let guard = store.lock().await;
            let mut session: Session = rsid_store::test_support::make_test_session();
            session.id = target;
            guard.insert_session(&session).unwrap();
            guard.insert_scheduled_job(&job).unwrap();
        }
        Self {
            store,
            bus: Arc::new(EventBus::new(64)),
            target,
            job,
        }
    }

    /// One due scan, as the scheduler tick runs it.
    async fn scan(&self, launcher: &Arc<dyn SessionLauncher>) {
        process_due_jobs(&self.store, &self.bus, launcher, None).await;
    }

    async fn row(&self) -> ScheduledJob {
        self.store
            .lock()
            .await
            .get_scheduled_job(&self.job.id)
            .unwrap()
            .unwrap()
    }

    async fn claim(&self) -> Option<crate::store::scheduled_jobs::DeliveryClaimV1> {
        self.store.lock().await.delivery_claim(self.job.id).unwrap()
    }
}

/// Cuts the process at the issue's window: the continuation delivered (its
/// effect claim committed) and the daemon died before the row advanced.
fn crash_after_delivery(job: &ScheduledJob) {
    install_fault_for_test(job.id, "crash_before_wake_settlement");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_crash_between_delivery_and_advance_does_not_deliver_the_wake_twice_797() {
    let fixture = Fixture::new(Recurrence::Once).await;
    let first = EffectLauncher::new(&fixture.store, true);
    let first_dyn: Arc<dyn SessionLauncher> = first.clone();
    crash_after_delivery(&fixture.job);

    fixture.scan(&first_dyn).await;

    assert_eq!(first.delivered(), 1, "the wake was delivered once");
    let stranded = fixture.row().await;
    assert!(
        stranded.enabled,
        "the crash left the row due, as in the issue"
    );
    let claim = fixture.claim().await.expect("the delivery stays claimed");
    assert!(claim.effect, "the continuation marked its effect claim");

    // The next incarnation: a fresh launcher, the same store, the same scan.
    let second = EffectLauncher::new(&fixture.store, true);
    let second_dyn: Arc<dyn SessionLauncher> = second.clone();
    fixture.scan(&second_dyn).await;

    assert_eq!(second.delivered(), 0, "the uncertain wake is not replayed");
    let settled = fixture.row().await;
    assert!(!settled.enabled, "a one-shot wake is spent");
    assert!(fixture.claim().await.is_none(), "the claim is settled");
    // A third scan stays quiet.
    fixture.scan(&second_dyn).await;
    assert_eq!(second.delivered(), 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn an_uncertain_recurring_wake_resumes_with_its_next_tick_797() {
    let fixture = Fixture::new(Recurrence::EveryHours(1)).await;
    let first = EffectLauncher::new(&fixture.store, true);
    let first_dyn: Arc<dyn SessionLauncher> = first.clone();
    crash_after_delivery(&fixture.job);
    fixture.scan(&first_dyn).await;
    assert_eq!(first.delivered(), 1);

    let second = EffectLauncher::new(&fixture.store, true);
    let second_dyn: Arc<dyn SessionLauncher> = second.clone();
    fixture.scan(&second_dyn).await;

    assert_eq!(second.delivered(), 0, "the uncertain tick is not replayed");
    let settled = fixture.row().await;
    assert!(settled.enabled, "a recurring wake keeps its future ticks");
    assert!(settled.next_fire_at > Utc::now());
    assert!(fixture.claim().await.is_none());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_claim_that_never_reached_an_effect_is_still_delivered_797() {
    let fixture = Fixture::new(Recurrence::Once).await;
    // The daemon died after the claim and before the continuation's effect
    // claim: nothing reached the target, so the wake must not be lost.
    assert!(
        fixture
            .store
            .lock()
            .await
            .record_delivery_claim(fixture.job.id, Utc::now())
            .unwrap()
    );
    assert!(!fixture.claim().await.unwrap().effect);

    let launcher = EffectLauncher::new(&fixture.store, true);
    let dynamic: Arc<dyn SessionLauncher> = launcher.clone();
    fixture.scan(&dynamic).await;

    assert_eq!(launcher.delivered(), 1, "the unreached wake is delivered");
    assert!(!fixture.row().await.enabled);
    assert!(fixture.claim().await.is_none());
    assert_eq!(fixture.target, fixture.job.wake_session_id.unwrap());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_refused_delivery_releases_its_claim_and_stays_armed_797() {
    struct Refuse;
    #[async_trait::async_trait]
    impl SessionLauncher for Refuse {
        async fn launch(&self, _: LaunchConfig) -> crate::error::Result<Uuid> {
            panic!("never");
        }
        async fn launch_scheduled_fresh(
            &self,
            _: LaunchConfig,
            _: bool,
        ) -> crate::error::Result<Uuid> {
            panic!("never");
        }
        async fn resume_scheduled(&self, t: Uuid, m: String) -> crate::error::Result<Uuid> {
            self.resume_scheduled_job(t, m, Vec::new()).await
        }
        async fn resume_scheduled_job(
            &self,
            _: Uuid,
            _: String,
            _: Vec<Uuid>,
        ) -> crate::error::Result<Uuid> {
            Err(DaemonError::InvalidParam(format!(
                "{}:{}",
                crate::store::manager_actions::fence::CONTINUATION_TARGET_BUSY,
                Uuid::new_v4()
            )))
        }
        async fn fire_watch(&self, _: &ScheduledJob) -> crate::error::Result<WatchFireOutcome> {
            Ok(WatchFireOutcome::NotReady)
        }
    }

    let fixture = Fixture::new(Recurrence::Once).await;
    let launcher: Arc<dyn SessionLauncher> = Arc::new(Refuse);
    fixture.scan(&launcher).await;

    let row = fixture.row().await;
    assert!(row.enabled, "a busy refusal retains the wake");
    assert!(
        fixture.claim().await.is_none(),
        "no claim outlives a refusal"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_delivery_that_settles_clears_its_claim_797() {
    let fixture = Fixture::new(Recurrence::Once).await;
    let launcher = EffectLauncher::new(&fixture.store, true);
    let dynamic: Arc<dyn SessionLauncher> = launcher.clone();

    fixture.scan(&dynamic).await;

    assert_eq!(launcher.delivered(), 1);
    assert!(!fixture.row().await.enabled);
    assert!(fixture.claim().await.is_none());
}
