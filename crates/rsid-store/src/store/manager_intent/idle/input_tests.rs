//! #2939: the intent memo is keyed on the pass's own inputs, and an off-lock
//! observation is used only while those inputs are unchanged.

use super::tests::{measured, warm};
use super::*;
use crate::store::manager_coordinator::tests::fixture;
use crate::store::manager_intent::ObservationFallback;
use crate::store::manager_resources::tests::{admitted, complete};
use rsi_common::harness_manager_v2::ManagerPolicyV2;
use rsi_common::types::{ConversationEvent, EventType, Session, SessionStatus};
use rusqlite::params;
use std::collections::BTreeSet;
use std::sync::Mutex;

fn event(store: &Store, session: Uuid) {
    let sequence: i32 = store
        .conn
        .query_row(
            "SELECT COALESCE(MAX(sequence),0)+1 FROM conversation_events WHERE session_id=?1",
            [session.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    store
        .insert_event(&ConversationEvent {
            id: 0,
            session_id: session,
            sequence,
            event_type: EventType::Message,
            role: None,
            created_at: Utc::now(),
            content: "progress".into(),
            tool_name: None,
            tool_input: None,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        })
        .unwrap();
}

/// A write the intent pass never reads: an appended friction event (one of
/// the daemon's ordinary background writes).
fn unrelated_write(store: &Store, project: Uuid, n: usize) {
    let _ = n;
    store
        .conn
        .execute(
            "INSERT INTO friction_events(signature,project_id,recorded_at) VALUES('test:unrelated_2939',?1,?2)",
            params![
                project.to_string(),
                Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            ],
        )
        .unwrap();
}

fn recomputations(store: &Store) -> u64 {
    store.manager_intent_idle.borrow().snapshot_recomputations
}

fn child(store: &Store, lead: &Session, status: SessionStatus, cost: Option<f64>) -> Session {
    let mut child = lead.clone();
    child.id = Uuid::new_v4();
    child.status = status;
    child.cost_usd = cost;
    store.insert_session(&child).unwrap();
    child
}

fn known_spend(store: &Store, config: &HarnessManagerConfigV1) -> Value {
    store
        .manager_v2_record(config, "resource", "cohort")
        .unwrap()
        .unwrap()
        .payload["known_spend_usd"]
        .clone()
}

use rsi_common::harness_manager::HarnessManagerConfigV1;

/// The hub's case: about one conversation event a second for a running
/// worker, plus other daemon writes, between two coordinator ticks. Before
/// #2939 each such write changed `total_changes` and every tick recomputed the
/// whole observation (cohort, work, scoped spend: ~0.5 s on the hub).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn unrelated_writes_keep_the_intent_memo_2939() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let worker = child(&store, &lead, SessionStatus::Running, Some(0.5));
    for _ in 0..64 {
        child(&store, &lead, SessionStatus::Completed, Some(0.25));
    }
    warm(&store, config.project_id);
    let before = recomputations(&store);
    let ((), ops) = measured(&store, || {
        for n in 0..100 {
            event(&store, worker.id);
            event(&store, lead.id);
            unrelated_write(&store, config.project_id, n);
            let result = store
                .reconcile_manager_intent(config.project_id, true)
                .unwrap();
            assert_eq!(result.changed, 0);
            assert_eq!(result.queued, 0);
        }
    });
    assert_eq!(recomputations(&store), before, "no tick recomputed");
    // Per tick: the three writes themselves plus one data_version read, the
    // memo's event re-check and its new high-water mark. The pass that the
    // writes used to force costs tens of thousands of instructions here.
    let mut full = 0;
    for _ in 0..3 {
        store
            .conn
            .execute_batch("UPDATE harness_manager_v2_policies SET project_id=project_id")
            .unwrap();
        let ((), ops) = measured(&store, || {
            store
                .reconcile_manager_intent(config.project_id, true)
                .unwrap();
        });
        full = ops;
    }
    eprintln!(
        "#2939 100 ticks with unrelated writes VM ops={ops}; one recomputed pass VM ops={full}"
    );
    assert!(
        ops / 100 < full / 20,
        "per tick {} vs pass {full}",
        ops / 100
    );
}

/// Every input the issue names still invalidates: spend, ownership and
/// membership, policy, deadlines, foreground (session) state, and the two
/// event-shaped inputs (a `Starting` session's first event, a watched lead).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn relevant_inputs_invalidate_the_intent_memo_2939() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let project = config.project_id;
    let recomputes = |store: &Store, what: &str| {
        let before = recomputations(store);
        store.reconcile_manager_intent(project, true).unwrap();
        assert!(recomputations(store) > before, "{what} must re-observe");
        warm(store, project);
    };
    warm(&store, project);

    // Spend: an invocation completes with a cost.
    let invocation = admitted(&store, &lead);
    recomputes(&store, "admitted invocation");
    complete(&store, invocation, 2.0);
    recomputes(&store, "completed invocation");
    assert_eq!(known_spend(&store, &config), 2.0);

    // Ownership and membership: a new session under the Epic, then a change
    // of the Epic's lead.
    let starting = child(&store, &lead, SessionStatus::Starting, None);
    recomputes(&store, "new member");
    store
        .conn
        .execute(
            "UPDATE sessions SET lead_session_id=?2 WHERE id=?1",
            params![config.epic_ids[0].to_string(), lead.id.to_string()],
        )
        .unwrap();
    recomputes(&store, "lead assignment rewrite");

    // Foreground state: a member's status.
    store
        .update_session_status(starting.id, SessionStatus::Running)
        .unwrap();
    recomputes(&store, "status change");
    store
        .update_session_status(starting.id, SessionStatus::Starting)
        .unwrap();
    recomputes(&store, "status change back");

    // A Starting session with no cost counts as zero spend only until its
    // first event: that event is an input.
    let unknown_before = store
        .manager_v2_record(&config, "resource", "cohort")
        .unwrap()
        .unwrap()
        .payload["unknown_spend_observations"]
        .clone();
    event(&store, starting.id);
    recomputes(&store, "first event of a Starting session");
    let unknown_after = store
        .manager_v2_record(&config, "resource", "cohort")
        .unwrap()
        .unwrap()
        .payload["unknown_spend_observations"]
        .clone();
    assert_eq!(
        unknown_after.as_u64().unwrap(),
        unknown_before.as_u64().unwrap() + 1
    );

    // Policy.
    let grant = store.get_harness_manager_policy(project).unwrap().unwrap();
    let mut policy = grant.policy;
    policy.max_spend_usd = Some(1.0);
    store
        .configure_harness_manager_policy(
            &rsi_common::harness_manager_v2::ConfigureHarnessManagerPolicyRequestV2 {
                project_id: project,
                expected_scope_version: config.row_version,
                expected_policy_version: grant.row_version,
                idempotency_key: "issue-2939-policy".into(),
                policy,
            },
        )
        .unwrap();
    recomputes(&store, "policy");

    // Deadlines: the memo ends at its own observation deadline.
    let at = Utc::now();
    assert!(store.manager_intent_memo(project, at).unwrap().is_some());
    assert!(
        store
            .manager_intent_memo(project, at + Duration::seconds(BACKSTOP_SECONDS))
            .unwrap()
            .is_none()
    );

    // A stale lead the memo watches: its next event ends the memo, another
    // session's event does not.
    let pin = store.manager_intent_input_pin().unwrap().unwrap();
    let result = store.manager_intent_memo(project, at).unwrap().unwrap();
    store.manager_intent_idle.borrow_mut().remember(
        project,
        pin,
        vec![lead.id],
        at,
        at + Duration::seconds(BACKSTOP_SECONDS),
        result,
    );
    let other = child(&store, &lead, SessionStatus::Completed, Some(0.0));
    let pin = store.manager_intent_input_pin().unwrap().unwrap();
    let result = store.manager_intent_memo(project, at).unwrap();
    assert!(result.is_none(), "a new member is an input");
    store.manager_intent_idle.borrow_mut().remember(
        project,
        pin,
        vec![lead.id],
        at,
        at + Duration::seconds(BACKSTOP_SECONDS),
        ManagerIntentReconciliationV2::default(),
    );
    event(&store, other.id);
    assert!(store.manager_intent_memo(project, at).unwrap().is_some());
    event(&store, lead.id);
    assert!(store.manager_intent_memo(project, at).unwrap().is_none());
}

/// The table list is the pass's actual read set (SQLite's authorizer sees
/// every table a statement reads, views and triggers resolved), and the only
/// columns of conversation events the pass reads are those an insert can
/// change: production updates rewrite only `content` and `metadata`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn intent_input_tables_cover_every_table_the_pass_reads_2939() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    child(&store, &lead, SessionStatus::Starting, None);
    let invocation = admitted(&store, &lead);
    complete(&store, invocation, 1.0);
    event(&store, lead.id);
    store.conn.flush_prepared_statement_cache();
    let reads = std::sync::Arc::new(Mutex::new(BTreeSet::<(String, String)>::new()));
    let sink = std::sync::Arc::clone(&reads);
    store.conn.authorizer(Some(move |context: AuthContext<'_>| {
        if let AuthAction::Read {
            table_name,
            column_name,
        } = context.action
        {
            sink.lock()
                .unwrap()
                .insert((table_name.to_string(), column_name.to_string()));
        }
        Authorization::Allow
    }));
    store
        .reconcile_manager_intent(config.project_id, true)
        .unwrap();
    let pin = store.manager_intent_input_pin().unwrap().unwrap();
    store
        .observe_manager_intent(config.project_id, pin)
        .unwrap();
    store
        .conn
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    let reads = reads.lock().unwrap().clone();
    assert!(reads.iter().any(|(table, _)| table == "model_invocations"));
    for (table, column) in &reads {
        if table == EVENTS_TABLE {
            assert!(
                // "" is a row-existence read (EXISTS, COUNT(*)): no column.
                ["", "id", "session_id", "created_at", "sequence"].contains(&column.as_str()),
                "the pass reads conversation_events.{column}"
            );
        } else if table != "json_each" {
            assert!(
                INTENT_INPUT_TABLES.binary_search(&table.as_str()).is_ok(),
                "the intent pass reads {table}; add it to INTENT_INPUT_TABLES"
            );
        }
    }
    let mut sorted = INTENT_INPUT_TABLES.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted, INTENT_INPUT_TABLES, "sorted and unique");
    for table in INTENT_INPUT_TABLES {
        let sql: String = store
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )
            .unwrap_or_else(|_| panic!("{table} is a table"));
        assert!(
            !sql.to_ascii_uppercase().contains("WITHOUT ROWID"),
            "{table}: the update hook does not see WITHOUT ROWID tables"
        );
    }
}

/// The coordinator's off-lock path: the observation is taken on a read-only
/// connection, and the pass on the daemon Store uses it only while nothing it
/// read has changed. Same records as a pass that reads under the lock, with
/// most of the SQL moved off the shared connection.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn off_lock_observation_is_used_only_while_inputs_are_unchanged_2939() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fixture.db");
    let store = Store::open_test_fixture(&path).unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let project = config.project_id;
    let worker = child(&store, &lead, SessionStatus::Running, Some(0.5));
    for _ in 0..64 {
        child(&store, &lead, SessionStatus::Completed, Some(0.25));
    }
    let observer = Store::open_read_only_observer(&path).unwrap();
    assert!(
        observer
            .conn
            .execute("UPDATE projects SET description='x'", [])
            .is_err(),
        "the observer connection refuses writes"
    );
    let observe = |store: &Store| {
        let pin = store.manager_intent_input_pin().unwrap().unwrap();
        observer.observe_manager_intent(project, pin).unwrap()
    };

    // A full pass under the lock, for comparison.
    let ((), full) = measured(&store, || {
        store.reconcile_manager_intent(project, true).unwrap();
    });
    let spend = known_spend(&store, &config);

    // Unchanged inputs (unrelated writes and events between pin and pass
    // included): the observation is used and the records are identical.
    // Exercise the supplied observation rather than a memo from the full pass.
    store.manager_intent_idle.borrow_mut().entries.clear();
    let observed = observe(&store);
    unrelated_write(&store, project, 1);
    event(&store, worker.id);
    let observer_before = observer
        .manager_intent_idle
        .borrow()
        .snapshot_recomputations;
    assert!(observer_before > 0);
    let before = recomputations(&store);
    let (result, applied) = measured(&store, || {
        store
            .reconcile_manager_intent_observed(project, true, observed)
            .unwrap()
    });
    assert_eq!(
        recomputations(&store),
        before,
        "nothing re-read on the Store"
    );
    assert!(
        !result.read_under_lock,
        "unchanged pin uses the off-lock snapshot"
    );
    assert_eq!(result.changed, 0, "same records as the full pass");
    let memo = store.reconcile_manager_intent(project, true).unwrap();
    assert!(!memo.read_under_lock, "memo hit performs no snapshot reads");
    assert_eq!(known_spend(&store, &config), spend);
    eprintln!("#2939 full pass VM ops={full}; pass with off-lock observation VM ops={applied}");
    assert!(applied * 2 < full, "{applied} vs {full}");

    // An input changed after the pin: the observation is discarded and the
    // pass reads again, so the new spend is recorded.
    let observed = observe(&store);
    store
        .conn
        .execute(
            "UPDATE sessions SET cost_usd=4.5 WHERE id=?1",
            [worker.id.to_string()],
        )
        .unwrap();
    let result = store
        .reconcile_manager_intent_observed(project, true, observed)
        .unwrap();
    assert!(
        result.read_under_lock,
        "changed inputs re-read under the lock"
    );
    assert!(recomputations(&store) > before);
    assert_eq!(known_spend(&store, &config), spend.as_f64().unwrap() + 4.0);

    // Another connection's commit after the pin is also a change.
    let before = recomputations(&store);
    let observed = observe(&store);
    let writer = rusqlite::Connection::open(&path).unwrap();
    Store::register_sql_functions(&writer).unwrap();
    writer
        .execute(
            "UPDATE sessions SET cost_usd=0.5 WHERE id=?1",
            [worker.id.to_string()],
        )
        .unwrap();
    let result = store
        .reconcile_manager_intent_observed(project, true, observed)
        .unwrap();
    assert!(
        result.read_under_lock,
        "changed inputs re-read under the lock"
    );
    assert!(recomputations(&store) > before);
    assert_eq!(known_spend(&store, &config), spend);

    // An observation of another appointment is never used.
    let before = recomputations(&store);
    let mut observed = observe(&store).unwrap();
    observed.scope_version += 1;
    store
        .conn
        .execute_batch("UPDATE harness_manager_v2_policies SET project_id=project_id")
        .unwrap();
    store
        .reconcile_manager_intent_observed(project, true, Some(observed))
        .unwrap();
    assert!(recomputations(&store) > before);
}

/// A memo populated by an under-lock pass must report this pass's actual read
/// activity, even when the reusable result originally required a full read.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn memo_hit_reports_no_reads_under_lock_3047() {
    let store = Store::open_in_memory().unwrap();
    let (config, _) = fixture(&store, ManagerPolicyV2::default());
    warm(&store, config.project_id);
    store.manager_intent_idle.borrow_mut().entries.clear();
    let full = store
        .reconcile_manager_intent(config.project_id, true)
        .unwrap();
    assert!(full.read_under_lock);
    let before = recomputations(&store);
    let memo = store
        .reconcile_manager_intent(config.project_id, true)
        .unwrap();
    assert!(!memo.read_under_lock);
    assert_eq!(recomputations(&store), before);
}

/// #3486: a pass offered an unchanged observation never reads under the
/// Store lock; a rejected one names what moved, and under
/// `DeferWithin` it is skipped (nothing written, the stale records stay) while
/// the project's last pass is recent, then reads under the lock once that
/// pass is older than the window or when the project has no completed pass.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn rejected_observation_is_named_and_the_under_lock_read_is_bounded_3486() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fixture.db");
    let store = Store::open_test_fixture(&path).unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let project = config.project_id;
    let worker = child(&store, &lead, SessionStatus::Running, Some(0.5));
    let observer = Store::open_read_only_observer(&path).unwrap();
    let observe = |store: &Store| {
        let pin = store.manager_intent_input_pin().unwrap().unwrap();
        observer.observe_manager_intent(project, pin).unwrap()
    };
    let window = Duration::seconds(45);
    let defer = ObservationFallback::DeferWithin(window);
    let set_cost = |cost: f64| {
        store
            .conn
            .execute(
                "UPDATE sessions SET cost_usd=?2 WHERE id=?1",
                params![worker.id.to_string(), cost],
            )
            .unwrap();
    };

    // An unchanged project: the observation is used, nothing is read under the
    // lock, nothing is rejected.
    let observed = observe(&store);
    let result = store
        .reconcile_manager_intent_observed_with(project, true, observed, defer)
        .unwrap();
    assert!(!result.read_under_lock && !result.deferred);
    assert_eq!(result.observation_rejected, None);
    let spend = known_spend(&store, &config);

    // An input table written after the pin: the reason names it, and with a
    // recent completed pass the pass is deferred (records untouched).
    let observed = observe(&store);
    set_cost(7.5);
    let pin = observed.as_ref().unwrap().pin;
    assert_eq!(
        store.manager_intent_input_change(&pin, &[]).unwrap(),
        Some("tables:sessions".to_owned())
    );
    let before = recomputations(&store);
    let result = store
        .reconcile_manager_intent_observed_with(project, true, observed, defer)
        .unwrap();
    assert!(result.deferred && !result.read_under_lock);
    assert_eq!(
        result.observation_rejected.as_deref(),
        Some("tables:sessions")
    );
    assert_eq!(recomputations(&store), before, "nothing re-read");
    assert_eq!(known_spend(&store, &config), spend, "records untouched");

    // The next tick observes the new state and applies it off the lock.
    let observed = observe(&store);
    let result = store
        .reconcile_manager_intent_observed_with(project, true, observed, defer)
        .unwrap();
    assert!(!result.read_under_lock && !result.deferred);
    assert_eq!(known_spend(&store, &config), spend.as_f64().unwrap() + 7.0);

    // Another connection's commit is named as such.
    let observed = observe(&store);
    let writer = rusqlite::Connection::open(&path).unwrap();
    Store::register_sql_functions(&writer).unwrap();
    writer
        .execute(
            "UPDATE sessions SET cost_usd=0.5 WHERE id=?1",
            [worker.id.to_string()],
        )
        .unwrap();
    let pin = observed.as_ref().unwrap().pin;
    assert_eq!(
        store.manager_intent_input_change(&pin, &[]).unwrap(),
        Some("external_commit".to_owned())
    );

    // Past the window the pass reads under the lock, so a store that never
    // goes quiet is still reconciled; so does a plain caller (no deferral).
    let result = store
        .reconcile_manager_intent_observed_with(
            project,
            true,
            observed,
            ObservationFallback::DeferWithin(Duration::zero()),
        )
        .unwrap();
    assert!(result.read_under_lock && !result.deferred);
    assert_eq!(
        result.observation_rejected.as_deref(),
        Some("external_commit")
    );
    assert_eq!(known_spend(&store, &config), spend);
    let observed = observe(&store);
    set_cost(1.5);
    let result = store
        .reconcile_manager_intent_observed(project, true, observed)
        .unwrap();
    assert!(result.read_under_lock && !result.deferred);

    // A project with no completed pass on this store is never deferred.
    let fresh = Uuid::new_v4();
    assert!(!store.manager_intent_applied_within(fresh, Utc::now(), window));
    assert!(store.manager_intent_applied_within(project, Utc::now(), window));
}

fn decision_hold_blocked(store: &Store, config: &HarnessManagerConfigV1, epic: Uuid) -> bool {
    let key = crate::store::manager_actions::manager_action_hold_key(
        crate::store::manager_actions::ManagerActionHoldReasonV2::Decision,
        Some(epic),
    );
    store
        .manager_v2_record(
            config,
            crate::store::manager_actions::MANAGER_ACTION_HOLD_KIND,
            &key,
        )
        .unwrap()
        .is_some_and(|hold| hold.payload["blocked"] == true)
}

fn decision_status(store: &Store, config: &HarnessManagerConfigV1, session: Uuid) -> Value {
    store
        .manager_v2_record(config, "decision", &format!("question:{session}"))
        .unwrap()
        .unwrap()
        .payload["status"]
        .clone()
}

/// #3518: a question refresh commits its own decision writes, so the pass must
/// not defer after it. A question resolved within the window of a completed
/// pass leaves no stale Epic Decision hold, and a newly pending question is
/// projected into the intent and the hold in the same pass.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn question_refresh_is_never_followed_by_a_deferral_3518() {
    use crate::store::manager_coordinator::tests::raise_question;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fixture.db");
    let store = Store::open_test_fixture(&path).unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let project = config.project_id;
    let epic = config.epic_ids[0];
    let observer = Store::open_read_only_observer(&path).unwrap();
    let observe = |store: &Store| {
        let pin = store.manager_intent_input_pin().unwrap().unwrap();
        observer.observe_manager_intent(project, pin).unwrap()
    };
    let defer = ObservationFallback::DeferWithin(Duration::seconds(45));
    let intent = |store: &Store| {
        store
            .manager_v2_record(&config, "intent", &epic.to_string())
            .unwrap()
            .unwrap()
            .payload
    };

    // A completed pass with no question: no hold.
    let result = store
        .reconcile_manager_intent_observed_with(project, true, observe(&store), defer)
        .unwrap();
    assert!(!result.deferred);
    assert!(!decision_hold_blocked(&store, &config, epic));

    // A new question, observed off the lock before the refresh publishes it:
    // the refresh moves an input; the pass reads again instead of deferring and
    // blocks the epic on the decision.
    raise_question(&store, lead.id, 1);
    let result = store
        .reconcile_manager_intent_observed_with(project, true, observe(&store), defer)
        .unwrap();
    assert!(!result.deferred && result.read_under_lock);
    assert!(
        result
            .observation_rejected
            .as_deref()
            .is_some_and(|reason| reason.starts_with("after_refresh:"))
    );
    assert_eq!(decision_status(&store, &config, lead.id), "pending");
    assert!(decision_hold_blocked(&store, &config, epic));
    assert_eq!(intent(&store)["state"], "blocked");

    // The question is resolved within the window of that completed pass: the
    // refresh records `resolved_externally`, and the same pass releases the hold.
    store
        .update_session_pending_question_json(lead.id, None)
        .unwrap();
    store
        .update_session_status(lead.id, SessionStatus::Running)
        .unwrap();
    let result = store
        .reconcile_manager_intent_observed_with(project, true, observe(&store), defer)
        .unwrap();
    assert!(!result.deferred && result.read_under_lock);
    assert_eq!(
        decision_status(&store, &config, lead.id),
        "resolved_externally"
    );
    assert!(!decision_hold_blocked(&store, &config, epic));
}

/// #3519: the completion time that bounds deferral is published only after
/// every fallible check of a successful pass, so a pass that returns `Err`
/// never refreshes it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_failed_memo_check_leaves_the_completion_time_unchanged_3519() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let project = config.project_id;
    let window = Duration::seconds(45);
    let at = Utc::now();
    let pass = ManagerIntentReconciliationV2::default();

    // An event after the pin makes the final memo check read the events table;
    // refuse that read.
    let pin = store.manager_intent_input_pin().unwrap().unwrap();
    event(&store, lead.id);
    store.conn.flush_prepared_statement_cache();
    store
        .conn
        .authorizer(Some(|context: AuthContext<'_>| match context.action {
            AuthAction::Read { table_name, .. } if table_name == EVENTS_TABLE => {
                Authorization::Deny
            }
            _ => Authorization::Allow,
        }));
    let failed = store.finish_intent_pass(project, Some(pin), Vec::new(), at, at, &pass);
    store
        .conn
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    assert!(failed.is_err());
    assert!(!store.manager_intent_applied_within(project, at, window));
    assert!(store.manager_intent_memo(project, at).unwrap().is_none());

    // The same tail without the failure publishes the completion time.
    store.conn.flush_prepared_statement_cache();
    let pin = store.manager_intent_input_pin().unwrap().unwrap();
    store
        .finish_intent_pass(project, Some(pin), Vec::new(), at, at, &pass)
        .unwrap();
    assert!(store.manager_intent_applied_within(project, at, window));
}
