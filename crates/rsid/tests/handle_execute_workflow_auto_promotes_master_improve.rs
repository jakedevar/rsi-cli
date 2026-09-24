//! Phase L2.3 — auto-promote-to-chain test for `handle_execute_workflow`.
//!
//! Full `RpcServer` construction requires a live daemon socket and is
//! impractical in integration tests. Instead, this test validates the
//! auto-promote *logic* by:
//!   1. Constructing a `ChainIteration` the same way the auto-promote
//!      spawn block does (using `Uuid::new_v4()` for chain_id, a known
//!      `child_execution_id`, and `MASTER_IMPROVE_DEFAULT_CAP`).
//!   2. Inserting it via `Store::insert_chain_iteration`.
//!   3. Asserting `Store::get_chain_for_execution` returns `Some(chain_id, 0)`
//!      for the same `execution_id` — exactly the post-condition the spawned
//!      task would leave.
//!
//! Workaround note: end-to-end through `RpcServer::handle_execute_workflow`
//! is not feasible without a full daemon process (Unix socket, AI subprocess
//! plumbing). This test exercises the store contract that the auto-promote
//! spawn relies on.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use chrono::Utc;
use rsi_common::types::ChainIteration;
use rsid::session::chain_driver::MASTER_IMPROVE_DEFAULT_CAP;
use rsid::store::Store;
use uuid::Uuid;

fn make_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("auto_promote.db");
    let store = Store::open(&db_path).unwrap();
    (dir, store)
}

/// Build a `ChainIteration` using the same field values the auto-promote
/// spawn block constructs in `handle_execute_workflow`.
fn build_auto_promote_iter(chain_id: Uuid, execution_id: Uuid, goal: &str) -> ChainIteration {
    ChainIteration {
        chain_id,
        iteration_index: 0,
        parent_execution_id: None,
        child_execution_id: execution_id,
        halt_reason: None,
        goal_text: goal.to_string(),
        refined_goal_text: None,
        token_count: None,
        pre_failure_count: Some(0),
        post_failure_count: None,
        cap: MASTER_IMPROVE_DEFAULT_CAP,
        started_at: Utc::now(),
        ended_at: None,
    }
}

/// Simulates the auto-promote spawn logic: insert a chain_iterations row for
/// a "Master Improve" execution and verify the store lookup returns it.
#[test]
fn auto_promote_inserts_chain_iteration_row() {
    let (_dir, store) = make_store();

    let chain_id = Uuid::new_v4();
    let execution_id = Uuid::new_v4();
    let goal = "fix all the things";

    let iter = build_auto_promote_iter(chain_id, execution_id, goal);
    store
        .insert_chain_iteration(&iter)
        .expect("insert_chain_iteration should succeed");

    // Verify: `get_chain_for_execution` finds the row we just inserted.
    let lookup = store
        .get_chain_for_execution(execution_id)
        .expect("get_chain_for_execution query failed");

    assert_eq!(
        lookup,
        Some((chain_id, 0)),
        "chain lookup for execution_id must return (chain_id, 0)"
    );
}

/// Verifies that a non-Master-Improve execution ID returns None from the
/// chain lookup — i.e., auto-promote rows don't pollute unrelated executions.
#[test]
fn unregistered_execution_returns_none() {
    let (_dir, store) = make_store();
    let unrelated_id = Uuid::new_v4();

    let lookup = store
        .get_chain_for_execution(unrelated_id)
        .expect("query should not error");

    assert_eq!(lookup, None, "unrelated execution must have no chain row");
}

/// Verifies the cap stored in the auto-promote row equals MASTER_IMPROVE_DEFAULT_CAP.
#[test]
fn auto_promote_row_uses_default_cap() {
    let (_dir, store) = make_store();

    let chain_id = Uuid::new_v4();
    let execution_id = Uuid::new_v4();
    let iter = build_auto_promote_iter(chain_id, execution_id, "improve codebase");

    store.insert_chain_iteration(&iter).unwrap();

    let rows = store.list_chain_iterations(chain_id).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].cap, MASTER_IMPROVE_DEFAULT_CAP,
        "auto-promoted row must use MASTER_IMPROVE_DEFAULT_CAP"
    );
    assert!(
        rows[0].halt_reason.is_none(),
        "freshly promoted row must have no halt_reason"
    );
    assert_eq!(rows[0].pre_failure_count, Some(0));
}
