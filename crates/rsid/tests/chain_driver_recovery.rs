//! Phase L2.2 — chain driver daemon-restart recovery integration test.
//!
//! Insert an active (`halt_reason` = NULL) `chain_iterations` row, call
//! `recover_active_chains`, confirm the row gets `HaltReason::Error("daemon-restart")`.

#![allow(clippy::unwrap_used, clippy::expect_used)]
// Test code uses unwrap as the canonical failure form — the workspace lints
// these as warn, but tests intentionally crash on unexpected inputs.

use chrono::Utc;
use rsi_common::types::{ChainIteration, HaltReason};
use rsid::session::chain_driver;
use rsid::store::Store;
use uuid::Uuid;

#[test]
fn recover_active_chains_marks_in_flight_row_as_daemon_restart_error() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("chain_driver_recovery.db");
    let store = Store::open(&db_path).unwrap();

    let chain_id = Uuid::new_v4();
    let active_iter = ChainIteration {
        chain_id,
        iteration_index: 0,
        parent_execution_id: None,
        child_execution_id: Uuid::new_v4(),
        halt_reason: None,
        goal_text: "an in-flight goal abandoned by daemon shutdown".to_string(),
        refined_goal_text: None,
        token_count: None,
        pre_failure_count: Some(0),
        post_failure_count: None,
        cap: 5,
        started_at: Utc::now(),
        ended_at: None,
    };
    store.insert_chain_iteration(&active_iter).unwrap();

    // Sanity check: list_active_chains sees the in-flight row before recovery.
    let active_before = store.list_active_chains().unwrap();
    assert_eq!(active_before.len(), 1, "expected one in-flight chain");
    assert_eq!(active_before[0], (chain_id, 0));

    // Run the driver's startup recovery hook.
    chain_driver::recover_active_chains(&store).expect("recover_active_chains failed");

    // After recovery: the row's halt_reason should be Error("daemon-restart").
    let rows = store.list_chain_iterations(chain_id).unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    match row.halt_reason.as_ref() {
        Some(HaltReason::Error(msg)) => assert_eq!(msg, "daemon-restart"),
        other => panic!("expected HaltReason::Error(\"daemon-restart\"), got {other:?}"),
    }
    assert!(
        row.ended_at.is_some(),
        "ended_at should be set by update_chain_iteration_outcome"
    );

    // After recovery: list_active_chains should see no rows.
    let active_after = store.list_active_chains().unwrap();
    assert!(
        active_after.is_empty(),
        "expected no active chains after recovery, got {active_after:?}"
    );
}

#[test]
fn recover_active_chains_is_noop_when_no_active_chains() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("chain_driver_recovery_noop.db");
    let store = Store::open(&db_path).unwrap();

    // No rows inserted — recovery should succeed silently.
    chain_driver::recover_active_chains(&store).expect("recover_active_chains failed on empty db");
}
