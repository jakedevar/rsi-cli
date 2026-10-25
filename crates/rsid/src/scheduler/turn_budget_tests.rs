//! #4021: the per-seat wake-turn budget coalesces routine manager notices past
//! the operator's cap and never holds an urgent one.

use super::*;
use crate::store::manager_watch_settlement::tests::manager_watch_fixture;
use rsi_common::types::ScheduledJob;
use std::sync::Mutex as StdMutex;

fn stamp() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// Delivers every watch it is asked to fire, and records the turn the way the
/// real launcher does: one admitted `session.continue.resume` invocation for
/// the seat, with every pending notice of the watch marked delivered.
struct TurnRecordingLauncher {
    store: Arc<rsid_core::store_lock::Mutex<Store>>,
    seat: Uuid,
    fired: StdMutex<Vec<Uuid>>,
}

impl TurnRecordingLauncher {
    fn fired(&self) -> usize {
        self.fired.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl SessionLauncher for TurnRecordingLauncher {
    async fn launch(&self, _: LaunchConfig) -> crate::error::Result<Uuid> {
        panic!("a manager notice never launches");
    }

    async fn launch_scheduled_fresh(&self, _: LaunchConfig, _: bool) -> crate::error::Result<Uuid> {
        panic!("a manager notice never launches Fresh");
    }

    async fn resume_scheduled(&self, _: Uuid, _: String) -> crate::error::Result<Uuid> {
        panic!("a manager notice is delivered through fire_watch");
    }

    async fn fire_watch(&self, job: &ScheduledJob) -> crate::error::Result<WatchFireOutcome> {
        self.fired.lock().unwrap().push(job.id);
        let guard = self.store.lock().await;
        insert_turn(&guard, self.seat, &stamp());
        guard
            .conn
            .execute(
                "UPDATE harness_manager_notices SET delivered_at=?2
                 WHERE job_id=?1 AND delivered_at IS NULL",
                rusqlite::params![job.id.to_string(), stamp()],
            )
            .unwrap();
        Ok(WatchFireOutcome::Delivered {
            session_id: self.seat,
            delivered_job_ids: vec![job.id],
            observed_job_versions: vec![(job.id, job.updated_at)],
        })
    }
}

fn insert_turn(store: &Store, seat: Uuid, created_at: &str) -> String {
    let id = Uuid::new_v4().to_string();
    store
        .conn
        .execute(
            "INSERT INTO model_invocations(id,purpose,invocation_kind,foreground,paid_risk,
                admission_status,status,trigger_source,session_id,policy_snapshot_json,
                usage_confidence,created_at)
             VALUES(?1,'session.continue.resume','session_lifecycle','foreground','paid_capable',
                'admitted','completed','scheduled_wake',?2,'{}','unavailable',?3)",
            rusqlite::params![id, seat.to_string(), created_at],
        )
        .unwrap();
    id
}

/// Queue one notice for the manager watch `job` (a lead reporting to its
/// manager), as the daemon's notice writers do.
fn seed_notice(
    store: &Store,
    job: &ScheduledJob,
    kind: &str,
    subject: &str,
    state: serde_json::Value,
) {
    let (project, epic, scope, source, target): (String, String, i64, String, String) = store
        .conn
        .query_row(
            "SELECT project_id,epic_id,scope_version,source_session_id,target_session_id
             FROM harness_manager_watches WHERE job_id=?1",
            [job.id.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    let at = stamp();
    store
        .conn
        .execute(
            "INSERT INTO harness_manager_notices
             (id,job_id,project_id,manager_session_id,scope_version,epic_id,direction,
              source_session_id,recipient_session_id,kind,subject_id,subject_version,
              state_json,recorded_at,queued_at)
             VALUES(?1,?2,?3,?4,?5,?6,'to_manager',?7,?4,?8,?9,?10,?11,?12,?12)",
            rusqlite::params![
                Uuid::new_v4().to_string(),
                job.id.to_string(),
                project,
                target,
                scope,
                epic,
                source,
                kind,
                subject,
                Uuid::new_v4().to_string(),
                state.to_string(),
                at,
            ],
        )
        .unwrap();
}

fn routine_lead_state() -> serde_json::Value {
    serde_json::json!({"status":"Completed","question":null})
}

struct World {
    store: Arc<rsid_core::store_lock::Mutex<Store>>,
    bus: Arc<EventBus>,
    seat: Uuid,
    job: ScheduledJob,
    launcher: Arc<TurnRecordingLauncher>,
}

async fn world(cap: u32) -> World {
    let store = Store::open_in_memory().expect("store");
    let fixture = manager_watch_fixture(&store);
    let job = fixture.notice(&store, 0, true);
    if cap > 0 {
        store
            .set_daemon_setting("manager_max_turns_per_hour", &cap.to_string())
            .unwrap();
    }
    let store = Arc::new(rsid_core::store_lock::Mutex::new(store));
    World {
        launcher: Arc::new(TurnRecordingLauncher {
            store: store.clone(),
            seat: fixture.manager,
            fired: StdMutex::new(Vec::new()),
        }),
        store,
        bus: Arc::new(EventBus::new(64)),
        seat: fixture.manager,
        job,
    }
}

impl World {
    async fn fire(&self) {
        let launcher: Arc<dyn SessionLauncher> = self.launcher.clone();
        let fresh = self
            .store
            .lock()
            .await
            .get_scheduled_job(&self.job.id)
            .unwrap()
            .unwrap();
        fire_job(&self.store, &self.bus, &launcher, &fresh).await;
    }

    async fn queue(&self, kind: &str, subject: &str, state: serde_json::Value) {
        seed_notice(&*self.store.lock().await, &self.job, kind, subject, state);
    }

    async fn budget(&self) -> rsi_common::coordinator_budget::ManagerTurnBudgetV1 {
        turn_budget::snapshot(&*self.store.lock().await, self.seat, Utc::now())
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn ten_routine_wakes_in_twenty_minutes_deliver_as_at_most_the_cap_4021() {
    let world = world(3).await;
    for index in 0..10 {
        world
            .queue(
                "session_state",
                &format!("lead-{index}"),
                routine_lead_state(),
            )
            .await;
        world.fire().await;
    }
    assert_eq!(
        world.launcher.fired(),
        3,
        "ten routine wakes inside the hour are delivered as at most the cap"
    );
    let budget = world.budget().await;
    assert_eq!(budget.cap_per_hour, 3);
    assert_eq!(budget.turns_last_hour, 3);
    assert_eq!(
        budget.held_wakes, 1,
        "one hold episode of the watch, not one per attempt"
    );
    assert_eq!(budget.urgent_bypasses, 0);

    // The held notices are all still undelivered, waiting for the next slot.
    let pending: i64 = world
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT COUNT(*) FROM harness_manager_notices
             WHERE job_id=?1 AND delivered_at IS NULL",
            [world.job.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pending, 7);

    // A turn ages out of the hour: the next attempt delivers every held notice
    // in one coalesced turn.
    {
        let guard = world.store.lock().await;
        guard
            .conn
            .execute(
                "UPDATE model_invocations SET created_at=?2 WHERE id=
                 (SELECT id FROM model_invocations WHERE session_id=?1 ORDER BY created_at LIMIT 1)",
                rusqlite::params![
                    world.seat.to_string(),
                    (Utc::now() - chrono::Duration::minutes(61))
                        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                ],
            )
            .unwrap();
    }
    world.fire().await;
    assert_eq!(world.launcher.fired(), 4);
    let pending: i64 = world
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT COUNT(*) FROM harness_manager_notices
             WHERE job_id=?1 AND delivered_at IS NULL",
            [world.job.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pending, 0, "the slot carries all seven held notices");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn an_urgent_notice_is_delivered_past_a_spent_budget_and_counted_4021() {
    let world = world(2).await;
    {
        let guard = world.store.lock().await;
        insert_turn(&guard, world.seat, &stamp());
        insert_turn(&guard, world.seat, &stamp());
    }
    world
        .queue("session_state", "lead-routine", routine_lead_state())
        .await;
    world.fire().await;
    assert_eq!(world.launcher.fired(), 0, "routine notice held at the cap");

    world
        .queue(
            "message",
            "operator-reply",
            serde_json::json!({"message":"ruling"}),
        )
        .await;
    world.fire().await;
    assert_eq!(
        world.launcher.fired(),
        1,
        "mail delivers past the spent budget, with the held notice in the same turn"
    );
    let budget = world.budget().await;
    assert_eq!(budget.urgent_bypasses, 1);
    assert_eq!(budget.turns_last_hour, 3);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn without_a_cap_every_wake_delivers_4021() {
    let world = world(0).await;
    for index in 0..5 {
        world
            .queue(
                "session_state",
                &format!("lead-{index}"),
                routine_lead_state(),
            )
            .await;
        world.fire().await;
    }
    assert_eq!(world.launcher.fired(), 5);
    let budget = world.budget().await;
    assert_eq!((budget.cap_per_hour, budget.held_wakes), (0, 0));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn only_routine_notice_classes_are_deferrable_4021() {
    use serde_json::json;
    let world = world(1).await;
    let cases: Vec<(&str, &str, serde_json::Value, bool)> = vec![
        ("session_state", "s1", json!({"status":"Completed"}), true),
        (
            "session_state",
            "s2",
            json!({"status":"Failed","lead_lost":true}),
            false,
        ),
        (
            "session_state",
            "s3",
            json!({"status":"Running","question":{"text":"?"}}),
            false,
        ),
        (
            "session_state",
            "s4",
            json!({"status":"WaitingApproval"}),
            false,
        ),
        (
            "session_state",
            "s5",
            json!({"recovery_code":"codex_transcript_unresumable"}),
            false,
        ),
        (
            "ledger_change",
            "w1",
            json!({"record_kind":"work","payload":{}}),
            true,
        ),
        (
            "ledger_change",
            "i1",
            json!({"record_kind":"intent","payload":{"state":"paused"}}),
            true,
        ),
        (
            "ledger_change",
            "i2",
            json!({"record_kind":"intent","payload":{"state":"blocked"}}),
            false,
        ),
        (
            "ledger_change",
            "i3",
            json!({"record_kind":"intent","payload":{"state":"stale_lead"}}),
            false,
        ),
        (
            "ledger_change",
            "qa_issue:9",
            json!({"record_kind":"work"}),
            false,
        ),
        (
            "ledger_change",
            "r1",
            json!({"record_kind":"request"}),
            false,
        ),
        (
            "ledger_change",
            "r2",
            json!({"record_kind":"issue_result_landed","digest_until_unix":1}),
            false,
        ),
        ("message", "m1", json!({"message":"hi"}), false),
        ("operator_answer", "o1", json!({"answer":"yes"}), false),
        ("action_result", "a1", json!({"state":"failed"}), false),
    ];
    for (kind, subject, state, deferrable) in cases {
        let guard = world.store.lock().await;
        guard
            .conn
            .execute(
                "UPDATE harness_manager_notices SET retired_at=?1 WHERE retired_at IS NULL",
                [stamp()],
            )
            .unwrap();
        seed_notice(&guard, &world.job, kind, subject, state);
        assert_eq!(
            guard.manager_watch_wake_deferrable(world.job.id).unwrap(),
            Some(deferrable),
            "{kind}/{subject}"
        );
    }
    let guard = world.store.lock().await;
    guard
        .conn
        .execute(
            "UPDATE harness_manager_notices SET retired_at=?1 WHERE retired_at IS NULL",
            [stamp()],
        )
        .unwrap();
    assert_eq!(
        guard.manager_watch_wake_deferrable(world.job.id).unwrap(),
        None,
        "no notice, no wake to coalesce"
    );
    seed_notice(
        &guard,
        &world.job,
        "session_state",
        "x",
        routine_lead_state(),
    );
    seed_notice(&guard, &world.job, "message", "y", json!({"message":"hi"}));
    assert_eq!(
        guard.manager_watch_wake_deferrable(world.job.id).unwrap(),
        Some(false),
        "one urgent notice makes the whole delivery urgent"
    );
}
