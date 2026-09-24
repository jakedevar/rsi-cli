//! Round-trip serde coverage for the chain-iteration shared types.
//!
//! L1.1 success criteria:
//! - `chain_iteration_round_trips_serde` — construct each `HaltReason`
//!   variant inside a `ChainIteration`, serialize to JSON, deserialize,
//!   assert equality.
//! - `start_chained_workflow_params_default_optionals` — optional fields
//!   default to `None` when absent on the wire.

use chrono::{TimeZone, Utc};
use rsi_common::rpc::StartChainedWorkflowParams;
use rsi_common::types::{ChainIteration, HaltReason};
use uuid::Uuid;

fn iteration_with(halt: Option<HaltReason>) -> ChainIteration {
    ChainIteration {
        chain_id: Uuid::nil(),
        iteration_index: 0,
        parent_execution_id: None,
        child_execution_id: Uuid::nil(),
        halt_reason: halt,
        goal_text: "test goal".to_string(),
        refined_goal_text: None,
        token_count: None,
        pre_failure_count: Some(0),
        post_failure_count: None,
        cap: 10,
        started_at: Utc.with_ymd_and_hms(2026, 4, 28, 0, 0, 0).unwrap(),
        ended_at: None,
    }
}

fn round_trip(orig: &ChainIteration) {
    let json = serde_json::to_string(orig).expect("serialize ChainIteration");
    let back: ChainIteration = serde_json::from_str(&json).expect("deserialize ChainIteration");
    // halt_reason is the field under test; rest are stable string/uuid/dt forms.
    assert_eq!(back.halt_reason, orig.halt_reason, "halt_reason round-trip");
    assert_eq!(back.iteration_index, orig.iteration_index);
    assert_eq!(back.cap, orig.cap);
    assert_eq!(back.goal_text, orig.goal_text);
}

#[test]
fn chain_iteration_round_trips_serde() {
    // Each HaltReason variant must round-trip cleanly.
    round_trip(&iteration_with(None));
    round_trip(&iteration_with(Some(HaltReason::Done)));
    round_trip(&iteration_with(Some(HaltReason::Cap)));
    round_trip(&iteration_with(Some(HaltReason::Regression {
        pre: 3,
        post: 7,
    })));
    round_trip(&iteration_with(Some(HaltReason::StopFile)));
    round_trip(&iteration_with(Some(HaltReason::JudgeBlocked(
        "plan doc not found".to_string(),
    ))));
    round_trip(&iteration_with(Some(HaltReason::JudgeMalformed)));
    round_trip(&iteration_with(Some(HaltReason::Error(
        "daemon-restart".to_string(),
    ))));
}

#[test]
fn halt_reason_adjacent_tag_wire_format() {
    // Sanity-check the on-the-wire shape; chain_driver / store rely on the
    // adjacent-tagged form so the SQLite text column round-trips through
    // `serde_json::{to_string,from_str}`.
    let json = serde_json::to_value(HaltReason::Regression { pre: 3, post: 7 }).unwrap();
    assert_eq!(json["kind"], "regression");
    assert_eq!(json["data"]["pre"], 3);
    assert_eq!(json["data"]["post"], 7);

    let json = serde_json::to_value(HaltReason::JudgeBlocked("x".to_string())).unwrap();
    assert_eq!(json["kind"], "judge_blocked");
    assert_eq!(json["data"], "x");

    let json = serde_json::to_value(HaltReason::Done).unwrap();
    assert_eq!(json["kind"], "done");
    // Unit variants serialize without `data`.
    assert!(json.get("data").is_none());
}

#[test]
fn start_chained_workflow_params_default_optionals() {
    let workflow_id = Uuid::new_v4();
    let json = format!(
        r#"{{"workflow_id":"{workflow_id}","topology_name":"master_improve","initial_goal":"hello"}}"#
    );
    let params: StartChainedWorkflowParams =
        serde_json::from_str(&json).expect("required fields only deserialize");

    assert_eq!(params.workflow_id, workflow_id);
    assert_eq!(params.topology_name, "master_improve");
    assert_eq!(params.initial_goal, "hello");
    assert!(params.project_id.is_none());
    assert!(params.working_dir.is_none());
    assert!(params.cap_override.is_none());
}

#[test]
fn start_chained_workflow_params_all_optionals_provided() {
    let workflow_id = Uuid::new_v4();
    let project_id = Uuid::new_v4();
    let json = format!(
        r#"{{"workflow_id":"{workflow_id}","topology_name":"master_improve",
            "initial_goal":"do the thing","project_id":"{project_id}",
            "working_dir":"/tmp/x","cap_override":5}}"#
    );
    let params: StartChainedWorkflowParams =
        serde_json::from_str(&json).expect("with optionals deserialize");
    assert_eq!(params.project_id, Some(project_id));
    assert_eq!(params.working_dir.as_deref(), Some("/tmp/x"));
    assert_eq!(params.cap_override, Some(5));
}
