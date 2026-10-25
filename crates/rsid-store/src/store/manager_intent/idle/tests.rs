use super::*;
use crate::store::manager_coordinator::tests::fixture;
use crate::store::manager_resources::tests::{admitted, complete};
use rsi_common::harness_manager_v2::{ConfigureHarnessManagerPolicyRequestV2, ManagerPolicyV2};
use rusqlite::params;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

// Nextest hides successful output. Keep these bounded observations in the
// existing stamped target as well, so one job supplies actual numeric evidence.
fn record_cost(scope: &str, ops: Option<u64>, snapshots: Option<u64>, started: std::time::Instant) {
    use std::io::Write;
    let thread = std::thread::current();
    let value = json!({"test":thread.name(),"scope":scope,"pid":std::process::id(),
        "observed_at":Utc::now(),"vm_ops":ops,"snapshot_delta":snapshots,
        "elapsed_micros":started.elapsed().as_micros()});
    eprintln!("issue211_cost {value}");
    if let Some(target) = std::env::var_os("CARGO_TARGET_DIR") {
        // Never the target root: the build-cache reclaim classifier refuses a
        // target whose root holds unrecognized files (#2535). The fixtures dir
        // is recognized scratch.
        let dir = std::path::PathBuf::from(target).join("rsid-test-fixtures");
        std::fs::create_dir_all(&dir).expect("create fixtures dir for cost observation");
        let path = dir.join(format!("issue211-idle-costs-{}.jsonl", std::process::id()));
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open own target cost observation")
            .write_all(format!("{value}\n").as_bytes())
            .expect("retain actual cost observation");
    }
}

struct CostMeter<'a> {
    store: &'a Store,
    scope: &'static str,
    ops: Arc<AtomicU64>,
    snapshots: u64,
    started: std::time::Instant,
}

impl<'a> CostMeter<'a> {
    fn new(store: &'a Store, scope: &'static str) -> Self {
        let ops = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&ops);
        store.conn.progress_handler(
            1,
            Some(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                false
            }),
        );
        Self {
            store,
            scope,
            ops,
            snapshots: store.manager_intent_idle.borrow().snapshot_recomputations,
            started: std::time::Instant::now(),
        }
    }
}

impl Drop for CostMeter<'_> {
    fn drop(&mut self) {
        self.store.conn.progress_handler(1, None::<fn() -> bool>);
        record_cost(
            self.scope,
            Some(self.ops.load(Ordering::Relaxed)),
            Some(
                self.store
                    .manager_intent_idle
                    .borrow()
                    .snapshot_recomputations
                    - self.snapshots,
            ),
            self.started,
        );
    }
}

pub(super) fn measured<R>(store: &Store, work: impl FnOnce() -> R) -> (R, u64) {
    let meter = CostMeter::new(store, "measured_block");
    let result = work();
    let ops = meter.ops.load(Ordering::Relaxed);
    drop(meter);
    (result, ops)
}

pub(super) fn warm(store: &Store, project: Uuid) {
    for _ in 0..4 {
        store.reconcile_manager_intent(project, true).unwrap();
        if store.manager_intent_memoized(project).unwrap() {
            return;
        }
    }
    panic!("a settled intent must become write-free and memoizable");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn idle_tick_does_zero_snapshot_recompute_and_constant_vm_work_211() {
    for count in [0, 128] {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, ManagerPolicyV2::default());
        for _ in 0..count {
            let mut child = lead.clone();
            child.id = Uuid::new_v4();
            child.cost_usd = Some(0.25);
            store.insert_session(&child).unwrap();
        }
        let (snapshot, snapshot_ops) = measured(&store, || {
            store.manager_v2_resource_snapshot(&config).unwrap()
        });
        assert!(snapshot["known_spend_usd"].as_f64().unwrap() >= f64::from(count) * 0.25);
        warm(&store, config.project_id);
        let before = store.manager_intent_idle.borrow().snapshot_recomputations;
        let ((), idle_ops) = measured(&store, || {
            for _ in 0..100 {
                let result = store
                    .reconcile_manager_intent(config.project_id, true)
                    .unwrap();
                assert_eq!(result.changed, 0);
                assert_eq!(result.queued, 0);
            }
        });
        assert_eq!(
            store.manager_intent_idle.borrow().snapshot_recomputations,
            before
        );
        // Only one cached PRAGMA data_version per tick; no cohort, tags,
        // scoped-spend or resource snapshot SQL, regardless of member count.
        assert!(idle_ops <= 100 * 32, "idle instructions={idle_ops}");
        assert!(snapshot_ops > idle_ops / 100);
        eprintln!(
            "members={count} manager_v2_resource_snapshot VM ops={snapshot_ops}; reconcile_manager_intent 100 idle ticks VM ops={idle_ops}; snapshot recomputations=0"
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn idle_claim_admission_has_bounded_vm_work_and_no_snapshot_211() {
    let store = Store::open_in_memory().unwrap();
    fixture(&store, ManagerPolicyV2::default());
    let boot = Uuid::new_v4();
    let before = store.manager_intent_idle.borrow().snapshot_recomputations;
    let changes = store.conn.total_changes();
    let ((), ops) = measured(&store, || {
        for _ in 0..100 {
            assert!(
                store
                    .claim_manager_action_with_create_admission(boot, false, &|_, _, _| {
                        panic!("an empty action queue must not invoke host admission")
                    })
                    .unwrap()
                    .is_none()
            );
            assert!(store.conn.is_autocommit());
        }
    });
    assert!(ops <= 100 * 64, "idle claim instructions={ops}");
    assert_eq!(store.conn.total_changes(), changes);
    assert_eq!(
        store.manager_intent_idle.borrow().snapshot_recomputations,
        before
    );
    eprintln!(
        "claim_manager_action_with_admission 100 empty ticks VM ops={ops}; snapshot recomputations=0"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn idle_intent_reobserves_membership_invocation_spend_and_policy_211() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let _cost = CostMeter::new(&store, "post_fixture_test_aggregate");
    warm(&store, config.project_id);
    let mut child = lead.clone();
    child.id = Uuid::new_v4();
    child.cost_usd = Some(1.0);
    store.insert_session(&child).unwrap();
    let before = store.manager_intent_idle.borrow().snapshot_recomputations;
    store
        .reconcile_manager_intent(config.project_id, true)
        .unwrap();
    assert!(store.manager_intent_idle.borrow().snapshot_recomputations > before);
    let snapshot = store
        .manager_v2_record(&config, "resource", "cohort")
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.payload["known_spend_usd"], 1.0);

    warm(&store, config.project_id);
    let invocation = admitted(&store, &lead);
    let before = store.manager_intent_idle.borrow().snapshot_recomputations;
    store
        .reconcile_manager_intent(config.project_id, true)
        .unwrap();
    assert!(store.manager_intent_idle.borrow().snapshot_recomputations > before);
    warm(&store, config.project_id);
    complete(&store, invocation, 2.0);
    store
        .reconcile_manager_intent(config.project_id, true)
        .unwrap();
    let snapshot = store
        .manager_v2_record(&config, "resource", "cohort")
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.payload["known_spend_usd"], 3.0);

    warm(&store, config.project_id);
    let grant = store
        .get_harness_manager_policy(config.project_id)
        .unwrap()
        .unwrap();
    let mut policy = grant.policy;
    policy.max_spend_usd = Some(1.0);
    store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: config.project_id,
            expected_scope_version: config.row_version,
            expected_policy_version: grant.row_version,
            idempotency_key: "idle-policy-invalidation".into(),
            policy,
        })
        .unwrap();
    store
        .reconcile_manager_intent(config.project_id, true)
        .unwrap();
    let snapshot = store
        .manager_v2_record(&config, "resource", "cohort")
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.payload["policy"]["policy"]["max_spend_usd"], 1.0);
    assert!(
        store
            .manager_v2_resource_gate(
                &config,
                Some(config.epic_ids[0]),
                lead.provider,
                Some(lead.id)
            )
            .is_err()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn idle_intent_external_commit_invalidates_and_transactions_never_reuse_211() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fixture.db");
    let store = Store::open_test_fixture(&path).unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let _cost = CostMeter::new(&store, "post_fixture_test_aggregate");
    warm(&store, config.project_id);
    let before = store.manager_intent_idle.borrow().snapshot_recomputations;
    let writer = rusqlite::Connection::open(&path).unwrap();
    Store::register_sql_functions(&writer).unwrap();
    writer
        .execute(
            "UPDATE sessions SET cost_usd=4.0 WHERE id=?1",
            [lead.id.to_string()],
        )
        .unwrap();
    store
        .reconcile_manager_intent(config.project_id, true)
        .unwrap();
    assert!(store.manager_intent_idle.borrow().snapshot_recomputations > before);
    assert_eq!(
        store
            .manager_v2_record(&config, "resource", "cohort")
            .unwrap()
            .unwrap()
            .payload["known_spend_usd"],
        4.0
    );

    warm(&store, config.project_id);
    let version = store.manager_intent_input_version().unwrap();
    store.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(store.manager_intent_input_version().unwrap().is_none());
    store
        .conn
        .execute(
            "UPDATE sessions SET cost_usd=99.0 WHERE id=?1",
            params![lead.id.to_string()],
        )
        .unwrap();
    store.conn.execute_batch("ROLLBACK").unwrap();
    assert!(store.manager_intent_input_version().unwrap() != version);
    store
        .reconcile_manager_intent(config.project_id, true)
        .unwrap();
    assert_eq!(
        store
            .manager_v2_record(&config, "resource", "cohort")
            .unwrap()
            .unwrap()
            .payload["known_spend_usd"],
        4.0
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn idle_intent_deadlines_resets_backstop_clock_and_capacity_are_bounded_211() {
    let started = std::time::Instant::now();
    let at = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
    let deadline = at + Duration::seconds(4);
    let requests = [json!({"deadline":deadline})];
    let resources =
        json!({"provider_windows":[{"windows":[{"resets_at_epoch":at.timestamp()+2}]}]});
    assert_eq!(
        next_observation_at(at, &requests, &resources),
        at + Duration::seconds(2)
    );
    assert_eq!(next_observation_at(at, &requests, &json!({})), deadline);
    assert_eq!(
        next_observation_at(at, &[], &json!({})),
        at + Duration::seconds(BACKSTOP_SECONDS)
    );
    assert_eq!(
        next_observation_at(at, &[json!({"deadline":at})], &json!({})),
        at
    );
    let mut cache = IdleIntentCache::default();
    let version = IntentInputVersion {
        relevant_writes: 1,
        external_changes: 1,
    };
    let pin = IntentInputPin {
        version,
        event_inserts: 0,
        events_high_water: 0,
        table_writes: [0; INPUT_TABLE_COUNT],
        event_deletes: 0,
    };
    let project = Uuid::new_v4();
    cache.remember(
        project,
        pin,
        Vec::new(),
        at,
        deadline,
        ManagerIntentReconciliationV2::default(),
    );
    assert!(cache.entry(project, version, at).is_some());
    assert!(cache.entry(project, version, deadline).is_none());
    assert!(
        cache
            .entry(project, version, at - Duration::seconds(1))
            .is_none()
    );
    for _ in 0..MAX_PROJECTS {
        cache.remember(
            Uuid::new_v4(),
            pin,
            Vec::new(),
            at,
            deadline,
            ManagerIntentReconciliationV2::default(),
        );
    }
    assert_eq!(cache.entries.len(), MAX_PROJECTS);
    assert!(cache.entry(project, version, at).is_none());
    // This fixture exercises only the in-memory deadline/cache kernel: no
    // SQLite operations or snapshot calls are measured or claimed here.
    record_cost("pure_cache_deadline_test", None, None, started);
}
