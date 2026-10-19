//! The test evidence of a topology execution, projected for its assigned
//! reviewer (#3020): the author's verified `scoped_test` receipt per node
//! attempt and the merge-queue gate receipt of each published land entry. Both
//! are read from rows the executor already wrote; nothing here runs a test.

use rsi_common::topology_agent::{
    AgentTopologyGateReceiptV1, AgentTopologyKnownFailureV1, AgentTopologyReviewReceiptsV1,
    AgentTopologyScopedTestReceiptV1, REVIEW_RECEIPTS_MAX,
};
use rsi_graph::data::Value as GraphValue;
use serde_json::Value;
use uuid::Uuid;

use crate::error::Result;
use crate::store::Store;
use crate::topology::review::clip;
use crate::topology::store::{AttemptRow, AttemptStatus, ExecutionRow};

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ReceiptTrust {
    Verified,
    Untrusted,
    Absent,
}

/// An assignment snapshots only these bounded facts, never the job's logs,
/// paths, command, or arbitrary receipt JSON. General job reads remain owner-only.
#[derive(serde::Serialize)]
struct SourceReceipt {
    trust: ReceiptTrust,
    reason: Option<String>,
    source_commit: String,
    source_session_id: Uuid,
    job_id: Option<Uuid>,
    head: Option<String>,
    state: Option<String>,
    exit_code: Option<i32>,
    selected: Option<u64>,
    completed: u64,
    failed: usize,
    failure_names: Vec<String>,
    coverage_mode: Option<String>,
    gate_covers_rest: bool,
    warning_gate: Option<String>,
}

/// Read the producer's exact verification job (or its newest matching scoped
/// run for older nodes). Reuse the landing-candidate verdict; never trust the
/// author's output as evidence, and never project a foreign owner's job.
pub(crate) fn source_receipt_text(
    store: &Store,
    execution: &ExecutionRow,
    producer: &AttemptRow,
    commit: &str,
) -> Result<String> {
    use rsi_common::agent_jobs::JobState;
    use rsi_common::agent_landing_candidate::{assess_candidate, is_scoped_test_job, receipt_head};

    let job = if let Some(verification) = verification_of(execution, producer) {
        match verification
            .get("job_id")
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
        {
            Some(id) => store
                .get_agent_job(id)?
                .map(|row| row.job)
                .filter(|job| job.owner_session_id == producer.session_id),
            None => None,
        }
    } else {
        let jobs: Vec<_> = store
            .list_agent_jobs(producer.session_id, 100)?
            .into_iter()
            .map(|row| row.job)
            .filter(is_scoped_test_job)
            .collect();
        let index = jobs
            .iter()
            .position(|job| receipt_head(job) == Some(commit))
            .unwrap_or(0);
        jobs.into_iter().nth(index)
    };
    let mut summary = SourceReceipt {
        trust: ReceiptTrust::Absent,
        reason: Some("no_owned_scoped_test_receipt".into()),
        source_commit: commit.into(),
        source_session_id: producer.session_id,
        job_id: None,
        head: None,
        state: None,
        exit_code: None,
        selected: None,
        completed: 0,
        failed: 0,
        failure_names: Vec::new(),
        coverage_mode: None,
        gate_covers_rest: false,
        warning_gate: None,
    };
    if let Some(job) = job {
        let verdict = assess_candidate(&job, commit);
        let receipt = job
            .result
            .as_ref()
            .and_then(|result| result.receipt.as_ref());
        summary.warning_gate = receipt
            .and_then(|r| r.pointer("/warnings/status"))
            .and_then(Value::as_str)
            .map(|s| clip(s, ITEM_BYTES));
        let warnings_ok = summary
            .warning_gate
            .as_deref()
            .is_none_or(|status| matches!(status, "ok" | "not_measured"));
        // Failed jobs stay untrusted even when a landing verdict excuses
        // baseline reds. Reviewers see the state and failures, not a green claim.
        let green = job.state == JobState::Succeeded
            && job.exit_code == Some(0)
            && receipt.and_then(|r| r.get("ok")).and_then(Value::as_bool) == Some(true);
        summary.trust = if verdict.ok && green && warnings_ok {
            ReceiptTrust::Verified
        } else {
            ReceiptTrust::Untrusted
        };
        summary.reason = verdict.refusal.or_else(|| {
            if !green {
                Some("job_not_green".into())
            } else if !warnings_ok {
                Some("warning_gate_failed".into())
            } else {
                None
            }
        });
        summary.job_id = Some(job.id);
        summary.head = verdict.receipt_head.map(|s| clip(&s, ITEM_BYTES));
        summary.state = verdict.job_state;
        summary.exit_code = verdict.exit_code;
        summary.selected = verdict.selected_tests;
        summary.completed = verdict.tests_completed;
        summary.coverage_mode = verdict.plan.map(|s| clip(&s, ITEM_BYTES));
        summary.gate_covers_rest = verdict.gate_covers_rest;
        if let Some(failures) = receipt
            .and_then(|r| r.get("failures"))
            .and_then(Value::as_array)
        {
            summary.failed = failures.len();
            summary.failure_names = failures
                .iter()
                .take(ITEMS_MAX)
                .map(|failure| {
                    clip(
                        &format!(
                            "{} {}",
                            failure.get("package").and_then(Value::as_str).unwrap_or(""),
                            failure.get("test").and_then(Value::as_str).unwrap_or("")
                        ),
                        ITEM_BYTES,
                    )
                })
                .collect();
        }
    }
    Ok(format!(
        "\n\nDaemon-verified scoped_test receipt snapshot (immutable for this assignment):\n{}",
        serde_json::to_string(&summary)?
    ))
}

/// Longest test id or filter quoted from a receipt.
const ITEM_BYTES: usize = 300;
/// Most test ids or filters quoted per receipt list.
const ITEMS_MAX: usize = 64;

/// A whole number out of a graph `Number`, which is stored as `f64`.
fn whole(value: Option<&Value>) -> Option<i64> {
    let number = value?.as_f64()?;
    if number.fract() != 0.0 {
        return None;
    }
    number.to_string().parse().ok()
}

fn texts(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(ITEMS_MAX)
        .map(|text| clip(text, ITEM_BYTES))
        .collect()
}

/// The `verification` record of one attempt's stored output, when it has one.
fn verification_of(execution: &ExecutionRow, attempt: &AttemptRow) -> Option<Value> {
    // A session output carries the whole last message, so it may live in Git:
    // read it through the digest-verified path.
    let data = crate::topology::executor::node_output(&execution.repo_root, attempt).ok()?;
    match data.get("verification")? {
        verification @ GraphValue::Map(_) => serde_json::to_value(verification).ok(),
        _ => None,
    }
}

fn scoped_test_receipt(
    attempt: &AttemptRow,
    verification: &Value,
) -> AgentTopologyScopedTestReceiptV1 {
    let text = |key: &str| {
        verification
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    AgentTopologyScopedTestReceiptV1 {
        attempt_id: attempt.id,
        node_id: attempt.node_id.clone(),
        result_commit: attempt.result_commit.clone(),
        job_id: text("job_id")
            .and_then(|id| Uuid::parse_str(&id).ok())
            .filter(|id| !id.is_nil()),
        head: text("head"),
        exit_code: whole(verification.get("exit_code")).and_then(|code| i32::try_from(code).ok()),
        plan: text("plan"),
        failing_tests: texts(verification.get("failing_tests")),
        known_failures: verification
            .get("known_failures")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|known| {
                Some(AgentTopologyKnownFailureV1 {
                    test: clip(known.get("test")?.as_str()?, ITEM_BYTES),
                    issue: u64::try_from(whole(known.get("issue"))?).ok()?,
                })
            })
            .take(ITEMS_MAX)
            .collect(),
        new_failures: texts(verification.get("new_failures")),
        unverified_reason: text("reason"),
    }
}

fn newest<T>(mut items: Vec<T>) -> Vec<T> {
    if items.len() > REVIEW_RECEIPTS_MAX {
        items.drain(..items.len() - REVIEW_RECEIPTS_MAX);
    }
    items
}

/// The receipts of `execution`: every attempt output that records a
/// `scoped_test` verification, and every published land entry's gate receipt,
/// each keyed to the commit it covers. `attempts` are in execution order.
pub(crate) fn project(
    store: &Store,
    execution: &ExecutionRow,
    attempts: &[AttemptRow],
) -> Result<AgentTopologyReviewReceiptsV1> {
    let scoped_test = attempts
        .iter()
        .filter(|attempt| attempt.node_kind == "session")
        .filter_map(|attempt| {
            let verification = verification_of(execution, attempt)?;
            Some(scoped_test_receipt(attempt, &verification))
        })
        .collect();
    let gate = attempts
        .iter()
        .filter(|attempt| attempt.node_kind == "land" && attempt.status == AttemptStatus::Succeeded)
        .filter_map(|attempt| {
            let entry = attempt.land_entry_id?;
            let receipt = crate::topology::land::gate_receipt(store, entry)?;
            Some(AgentTopologyGateReceiptV1 {
                attempt_id: attempt.id,
                entry_id: entry,
                commit: attempt.base_commit.clone(),
                state: receipt.state,
                test_filters: receipt
                    .test_filters
                    .iter()
                    .take(ITEMS_MAX)
                    .map(|filter| clip(filter, ITEM_BYTES))
                    .collect(),
                log_glob: receipt.log_glob,
            })
        })
        .collect();
    Ok(AgentTopologyReviewReceiptsV1 {
        scoped_test: newest(scoped_test),
        gate: newest(gate),
    })
}
