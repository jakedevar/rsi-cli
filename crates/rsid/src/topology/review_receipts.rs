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
    /// #3550: the run is red, but the daemon's landing verdict has no new
    /// failure and every failure is named by an open known-failure record, so
    /// it is accepted verification (`baseline_reds` lists them with Issues).
    TrustedWithBaselineReds,
    Untrusted,
    Absent,
}

/// One tracked baseline red a trusted-with-baseline-reds receipt carries.
#[derive(Debug, PartialEq, serde::Serialize)]
struct BaselineRed {
    test: String,
    issue: u64,
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
    /// Failures an open known-failure record names (#3550); empty unless the
    /// trust is `trusted_with_baseline_reds`.
    baseline_reds: Vec<BaselineRed>,
}

/// #3550: the tracked baseline reds of a failed `scoped_test` job, when the job
/// is accepted verification: it failed (not lost or interrupted), the daemon's
/// landing-candidate verdict is `ok` (no new failure, head matches) and
/// `verify::judge` attributes every failing test to an open known-failure
/// record. Any new or unparsed failure, an unfinished run, a head mismatch or a
/// missing receipt yields `None` (the receipt stays untrusted).
fn baseline_reds_of(
    job: &rsi_common::agent_jobs::AgentJobV1,
    commit: &str,
    landing_ok: bool,
    known: &std::collections::BTreeMap<String, u64>,
) -> Option<Vec<BaselineRed>> {
    if !landing_ok || job.state != rsi_common::agent_jobs::JobState::Failed {
        return None;
    }
    let evidence = crate::topology::verify::judge(
        std::slice::from_ref(job),
        job.owner_session_id,
        commit,
        known,
    )
    .ok()?;
    if evidence.known_failures.is_empty() || !evidence.new_failures.is_empty() {
        return None;
    }
    Some(
        evidence
            .known_failures
            .into_iter()
            .take(ITEMS_MAX)
            .map(|(test, issue)| BaselineRed {
                test: clip(&test, ITEM_BYTES),
                issue,
            })
            .collect(),
    )
}

/// The trust of one scoped_test job (#3550) as `(trust, baseline reds, green,
/// warnings_ok)`. `Verified` is a green run the landing verdict accepts;
/// `TrustedWithBaselineReds` a failed run whose every failure is a tracked
/// baseline red; anything else, including a new or unparsed failure, a head
/// mismatch and a missing receipt, is `Untrusted`.
fn classify(
    job: &rsi_common::agent_jobs::AgentJobV1,
    commit: &str,
    verdict: &rsi_common::agent_landing_candidate::AgentCheckLandingCandidateResultV1,
    warning_gate: Option<&str>,
    known: &std::collections::BTreeMap<String, u64>,
) -> (ReceiptTrust, Vec<BaselineRed>, bool, bool) {
    use rsi_common::agent_jobs::JobState;
    let warnings_ok = warning_gate.is_none_or(|status| matches!(status, "ok" | "not_measured"));
    let receipt = job
        .result
        .as_ref()
        .and_then(|result| result.receipt.as_ref());
    let green = job.state == JobState::Succeeded
        && job.exit_code == Some(0)
        && receipt.and_then(|r| r.get("ok")).and_then(Value::as_bool) == Some(true);
    if verdict.ok && green && warnings_ok {
        return (ReceiptTrust::Verified, Vec::new(), green, warnings_ok);
    }
    if !green
        && warnings_ok
        && let Some(reds) = baseline_reds_of(job, commit, verdict.ok, known)
    {
        return (
            ReceiptTrust::TrustedWithBaselineReds,
            reds,
            green,
            warnings_ok,
        );
    }
    (ReceiptTrust::Untrusted, Vec::new(), green, warnings_ok)
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
        baseline_reds: Vec::new(),
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
        let known = if job.state == JobState::Failed {
            let signatures = match execution.project_id {
                Some(project) => store.open_failure_signature_issues(project)?,
                None => Vec::new(),
            };
            crate::topology::verify::known_failures(&signatures)
        } else {
            std::collections::BTreeMap::new()
        };
        let (trust, baseline_reds, green, warnings_ok) = classify(
            &job,
            commit,
            &verdict,
            summary.warning_gate.as_deref(),
            &known,
        );
        summary.trust = trust;
        let baseline_only = !baseline_reds.is_empty();
        summary.baseline_reds = baseline_reds;
        summary.reason = verdict.refusal.or_else(|| {
            if baseline_only {
                Some("only_tracked_baseline_reds".into())
            } else if !green {
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    use super::{BaselineRed, ReceiptTrust, Uuid, Value, classify};
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    use rsi_common::agent_jobs::AgentJobV1;
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    use rsi_common::agent_landing_candidate::assess_candidate;
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    use serde_json::json;
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    use std::collections::BTreeMap;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    const HEAD: &str = "dab3ce0f592fbf457ec91cd7b7f41d233c83ad7c";
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    const BASELINE: &str =
        "rpc::tests::agent_issue_rpc_lead_can_get_while_generic_issue_get_stays_denied";

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn failure(test: &str) -> Value {
        json!({"kind": "lib", "package": "rsid", "target": "rsid", "test": test})
    }

    /// A `scoped_test` job as the daemon stores it: `new_failures` null means
    /// the receipt never classified its failures.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn job(state: &str, exit: i32, failed: &[&str], new: Option<&[&str]>) -> AgentJobV1 {
        let failures: Vec<_> = failed.iter().map(|t| failure(t)).collect();
        let baseline: Vec<_> = failed
            .iter()
            .filter(|t| new.is_none_or(|new| !new.contains(t)))
            .map(|t| failure(t))
            .collect();
        let new_failures = new.map(|new| new.iter().map(|t| failure(t)).collect::<Vec<_>>());
        serde_json::from_value(json!({
            "id": Uuid::new_v4(), "kind": "test", "name": null, "state": state,
            "owner_session_id": Uuid::new_v4(), "unit_name": "u", "cwd": "/tmp",
            "log_path": "/tmp/log",
            "params": {"kind": "test", "scoped_test": {"base": "origin/rolling"}},
            "exit_code": exit,
            "result": {
                "exit_code": exit,
                "failing_tests": failed.iter().map(|t| format!("rsid {t}")).collect::<Vec<_>>(),
                // The daemon's launch stamp (#579): a receipt is trusted only
                // when the daemon ran the rolling tool.
                "scoped_tool": {
                    "source": "rolling", "path": "/daemon/tool/scripts/scoped-test",
                    "rev": HEAD,
                    "files": [{"path": "/daemon/tool/scripts/scoped-test", "blob": "a".repeat(40)}],
                },
                "receipt": {
                    "schema": 1, "head": HEAD, "ok": exit == 0, "exit_code": exit,
                    "plan": "targeted_subset", "failures": failures,
                    "baseline_reds": baseline, "new_failures": new_failures,
                    "packages": [{"package": "rsid", "status": if exit == 0 { "passed" } else { "failed" }, "completed": 9}],
                },
            },
            "created_at": "2026-10-22T00:00:00.000000000Z", "finished_at": null,
        }))
        .expect("job")
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn classified(
        job: &AgentJobV1,
        known: &BTreeMap<String, u64>,
    ) -> (ReceiptTrust, Vec<BaselineRed>) {
        let verdict = assess_candidate(job, HEAD);
        let (trust, reds, _, _) = classify(job, HEAD, &verdict, None, known);
        (trust, reds)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn known(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
        pairs.iter().map(|(t, i)| ((*t).to_owned(), *i)).collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn a_red_run_of_only_tracked_baseline_reds_is_trusted_with_them_named() {
        let red = job("failed", 1, &[BASELINE], Some(&[]));
        let (trust, reds) = classified(&red, &known(&[(BASELINE, 3474)]));
        assert!(matches!(trust, ReceiptTrust::TrustedWithBaselineReds));
        assert_eq!(
            reds,
            vec![BaselineRed {
                test: BASELINE.into(),
                issue: 3474
            }]
        );
        assert_eq!(
            serde_json::to_value(ReceiptTrust::TrustedWithBaselineReds).unwrap(),
            "trusted_with_baseline_reds"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn one_new_failure_among_baseline_reds_stays_untrusted() {
        let red = job(
            "failed",
            1,
            &[BASELINE, "a::new_red"],
            Some(&["a::new_red"]),
        );
        let (trust, reds) = classified(&red, &known(&[(BASELINE, 3474)]));
        assert!(matches!(trust, ReceiptTrust::Untrusted));
        assert!(reds.is_empty());
        // A failure the receipt calls baseline but no open record names is
        // not accepted either.
        let red = job("failed", 1, &[BASELINE], Some(&[]));
        let (trust, _) = classified(&red, &known(&[]));
        assert!(matches!(trust, ReceiptTrust::Untrusted));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn an_unparsed_failure_or_head_mismatch_stays_untrusted() {
        let known = known(&[(BASELINE, 3474)]);
        // A red exit with no attributable failing test (a build failure).
        let red = job("failed", 101, &[], Some(&[]));
        assert!(matches!(
            classified(&red, &known).0,
            ReceiptTrust::Untrusted
        ));
        // The receipt never classified its failures.
        let red = job("failed", 1, &[BASELINE], None);
        assert!(matches!(
            classified(&red, &known).0,
            ReceiptTrust::Untrusted
        ));
        // The receipt verified another commit.
        let mut red = job("failed", 1, &[BASELINE], Some(&[]));
        red.result.as_mut().unwrap().receipt.as_mut().unwrap()["head"] = json!("a".repeat(40));
        assert!(matches!(
            classified(&red, &known).0,
            ReceiptTrust::Untrusted
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn a_green_run_is_trusted_with_no_baseline_reds() {
        let green = job("succeeded", 0, &[], None);
        let (trust, reds) = classified(&green, &known(&[(BASELINE, 3474)]));
        assert!(matches!(trust, ReceiptTrust::Verified));
        assert!(reds.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn a_green_run_of_a_candidate_tool_is_untrusted_whatever_the_receipt_claims() {
        let mut green = job("succeeded", 0, &[], None);
        let result = green.result.as_mut().unwrap();
        result.receipt.as_mut().unwrap()["tool"] = json!({"source": "rolling"});
        result.scoped_tool.as_mut().unwrap().source =
            rsi_common::agent_jobs::ScopedToolSource::Candidate;
        let (trust, reds) = classified(&green, &known(&[]));
        assert!(matches!(trust, ReceiptTrust::Untrusted));
        assert!(reds.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn a_warning_gate_failure_keeps_a_baseline_red_run_untrusted() {
        let red = job("failed", 1, &[BASELINE], Some(&[]));
        let verdict = assess_candidate(&red, HEAD);
        let known = known(&[(BASELINE, 3474)]);
        let (trust, ..) = classify(&red, HEAD, &verdict, Some("new_warnings"), &known);
        assert!(matches!(trust, ReceiptTrust::Untrusted));
    }
}
