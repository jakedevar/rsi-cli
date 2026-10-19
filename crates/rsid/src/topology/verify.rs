//! Verified session nodes (#2249).
//!
//! A session step with `require_verified` settles `succeeded` only when the
//! daemon finds, in its own job table, a terminal `scoped_test` job owned by
//! the node's session whose receipt names the exact commit the node produced.
//! That is the check a manager does by hand before enqueueing a landing. The
//! daemon reads recorded job state and never re-runs tests: the receipt is not
//! the node's self-report, and the merge-queue gate stays the authority.
//!
//! A job verifies the commit when its receipt `head` equals it and either the
//! run was green or every failing test is named by an open known-failure
//! signature of the execution's project (#1016). Anything else is
//! `verification_failed` with one reason: `no_receipt` (the session never ran a
//! scoped test), `stale_head` (its newest receipt verifies another commit),
//! `incomplete` (a job still running, or one that ended without a verdict: no
//! receipt, timed out, interrupted by a deploy, a build failure) or
//! `new_failures` (a red no open signature names).

use std::collections::BTreeMap;

use rsi_common::agent_failure_signatures::SignatureIssueSource;
use rsi_common::agent_jobs::{AgentJobV1, JobParams, JobState};
use rsi_common::failure_signature::{parse_issue_blocks, status_is_live};
use serde_json::{Value, json};
use uuid::Uuid;

/// How many of the session's newest jobs are read. A node submits a handful.
pub(crate) const JOBS_READ: usize = 256;

/// Package statuses and phases of a receipt that mean part of the selected
/// tests never produced a verdict, so a green or known-only red proves nothing.
const UNFINISHED_STATUSES: [&str; 2] = ["timed out", "running"];
const UNFINISHED_PHASES: [&str; 3] = ["build_failed", "list_failed", "no_tests_selected"];

/// Why a node's commit is not verified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reason {
    NoReceipt,
    StaleHead,
    Incomplete,
    NewFailures,
}

impl Reason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::NoReceipt => "no_receipt",
            Self::StaleHead => "stale_head",
            Self::Incomplete => "incomplete",
            Self::NewFailures => "new_failures",
        }
    }
}

/// What the daemon read from one `scoped_test` job.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Evidence {
    pub(crate) job_id: Uuid,
    /// The commit the job's receipt verified.
    pub(crate) head: String,
    pub(crate) exit_code: Option<i32>,
    /// The receipt's plan (`precise`, `targeted_subset`).
    pub(crate) plan: Option<String>,
    /// The job's own `<package> <test>` names.
    pub(crate) failing_tests: Vec<String>,
    /// `(test id, owner Issue number)` for each failure an open signature names.
    pub(crate) known_failures: Vec<(String, u64)>,
    pub(crate) new_failures: Vec<String>,
}

/// A node whose commit the daemon could not verify.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Unverified {
    pub(crate) reason: Reason,
    pub(crate) detail: String,
    /// The job that was judged, when there was one.
    pub(crate) evidence: Option<Evidence>,
}

impl Evidence {
    /// The `verification` field of a node output.
    pub(crate) fn to_json(&self, reason: Option<(Reason, &str)>) -> Value {
        let mut value = json!({
            "job_id": self.job_id.to_string(),
            "head": self.head,
            "exit_code": self.exit_code,
            "plan": self.plan,
            "failing_tests": self.failing_tests,
            "known_failures": self
                .known_failures
                .iter()
                .map(|(test, issue)| json!({"test": test, "issue": issue}))
                .collect::<Vec<_>>(),
            "new_failures": self.new_failures,
        });
        if let Some((reason, detail)) = reason {
            value["reason"] = reason.as_str().into();
            value["detail"] = detail.into();
        }
        value
    }
}

impl Unverified {
    /// The `verification` field of a failed node's output: the judged job's
    /// evidence when there is one, an empty record otherwise, plus the reason.
    pub(crate) fn to_json(&self) -> Value {
        let empty = Evidence {
            job_id: Uuid::nil(),
            head: String::new(),
            exit_code: None,
            plan: None,
            failing_tests: Vec::new(),
            known_failures: Vec::new(),
            new_failures: Vec::new(),
        };
        let mut value = self
            .evidence
            .as_ref()
            .unwrap_or(&empty)
            .to_json(Some((self.reason, &self.detail)));
        if self.evidence.is_none() {
            value["job_id"] = Value::Null;
            value["head"] = Value::Null;
        }
        value
    }
}

/// The marker a verification continuation's user turn carries (#2263). Its
/// unique id is how the daemon recognises that exact turn in the conversation.
pub(crate) fn continuation_marker(delivery_id: Uuid) -> String {
    format!("[Topology verification continuation {delivery_id}]")
}

impl Unverified {
    /// #2263: the daemon message that continues the node's own session with
    /// its missing evidence: the reason, the receipt it read and the exact next
    /// step. `n` of `max` counts this continuation.
    pub(crate) fn continuation_message(&self, n: u32, max: u32, delivery_id: Uuid) -> String {
        let receipt = match &self.evidence {
            Some(evidence) => format!(
                "Receipt read: scoped_test job {} verified {} (exit {}, plan {}); failing tests: \
                 {}; new failures: {}.",
                evidence.job_id,
                evidence.head,
                evidence
                    .exit_code
                    .map_or_else(|| "none".to_owned(), |code| code.to_string()),
                evidence.plan.as_deref().unwrap_or("none"),
                list_or_none(&evidence.failing_tests),
                list_or_none(&evidence.new_failures),
            ),
            None => "Receipt read: none that judges your result commit.".to_owned(),
        };
        let payload = format!(
            "Your handoff was not accepted: the daemon could not verify your result commit \
             (verification continuation {n} of {max}).\n\
             Reason: {}. {}\n\
             {receipt}\n\
             Next step: fix what the receipt names, commit, then submit ONE scoped_test job \
             (AgentSubmitJob {{\"kind\":\"test\",\"wake\":\"none\",\"params\":{{\"scoped_test\":\
             {{\"base\":\"origin/rolling\"}}}}}}), arm ONE AgentScheduleWake mode \"when\" on that \
             job and end your turn; when it returns green, hand off again with no commit after \
             the verified one. Failures an open known-failure record names are accepted.\n\
             {}",
            self.reason.as_str(),
            self.detail,
            continuation_marker(delivery_id),
        );
        rsi_common::daemon_message::wrap("topology-verification", &payload)
    }
}

/// #2563: the daemon message that continues a finished node whose sandbox still
/// holds uncommitted paths. It asks the session to commit or discard them and
/// repeat its handoff line; the session is not asked to redo any work.
pub(crate) fn uncommitted_continuation_message(
    dirty_paths: usize,
    n: u32,
    max: u32,
    delivery_id: Uuid,
) -> String {
    let payload = format!(
        "Your handoff was received, but your sandbox still holds {dirty_paths} uncommitted \
         path(s) (uncommitted-work continuation {n} of {max}).\n\
         Next step: commit them or discard them, then repeat your handoff line. A commit you add \
         is a new result commit: when your node requires verification, submit ONE scoped_test \
         job for it and hand off again once it returns green.\n\
         {}",
        continuation_marker(delivery_id),
    );
    rsi_common::daemon_message::wrap("topology-uncommitted", &payload)
}

/// #2584: the daemon message that continues a finished node whose last message
/// the handoff parser refused. It carries the parser's error and the exact
/// required first line, and asks only for the handoff to be repeated.
pub(crate) fn handoff_format_continuation_message(
    parse_error: &str,
    n: u32,
    max: u32,
    delivery_id: Uuid,
) -> String {
    let payload = format!(
        "Your last message was not accepted as a handoff: {parse_error} (handoff-format \
         continuation {n} of {max}).\n\
         Next step: repeat your final message so that its very first line is exactly \
         `PIPELINE HANDOFF — <STAGE>:` (an em dash, an uppercase stage, a trailing colon; \
         use the stage your instructions name), followed by doc_path, manifest_path, status and a \
         summary. Any prose belongs after that block. Do not redo any work; if you already \
         filed or found everything, say `nothing new` in the summary.\n\
         {}",
        continuation_marker(delivery_id),
    );
    rsi_common::daemon_message::wrap("topology-handoff-format", &payload)
}

/// Whether the session's conversation holds the user turn that carries this
/// continuation's marker. Session status and assistant output can belong to
/// other turns; only this turn proves the message was delivered.
pub(crate) fn continuation_turn_observed(
    store: &crate::store::Store,
    session_id: Uuid,
    delivery_id: Uuid,
) -> crate::error::Result<bool> {
    use rusqlite::OptionalExtension;
    Ok(store
        .conn
        .query_row(
            "SELECT 1 FROM conversation_events WHERE session_id=?1 \
             AND event_type='Message' AND role='User' AND instr(content,?2)>0 LIMIT 1",
            rusqlite::params![session_id.to_string(), continuation_marker(delivery_id)],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_owned()
    } else {
        items.join(", ")
    }
}

/// Test id -> owner Issue number for every record of the project's live
/// signature Issues (the lowest Issue when several name one test). Issues whose
/// signature blocks do not parse name nothing, as for `AgentQueryFailureSignatures`.
pub(crate) fn known_failures(sources: &[SignatureIssueSource]) -> BTreeMap<String, u64> {
    let mut known: BTreeMap<String, u64> = BTreeMap::new();
    for source in sources
        .iter()
        .filter(|source| status_is_live(&source.status))
    {
        let Ok(records) = parse_issue_blocks(&source.body, source.display_number) else {
            continue;
        };
        for record in records {
            let issue = known.entry(record.test_id).or_insert(record.issue);
            *issue = (*issue).min(record.issue);
        }
    }
    known
}

fn scoped_test(job: &AgentJobV1) -> bool {
    matches!(&job.params, JobParams::Test(test) if test.is_scoped_test())
}

fn receipt(job: &AgentJobV1) -> Option<&Value> {
    job.result.as_ref()?.receipt.as_ref()
}

fn receipt_head(job: &AgentJobV1) -> Option<&str> {
    receipt(job)?.get("head")?.as_str()
}

fn exit_code(job: &AgentJobV1) -> Option<i32> {
    job.exit_code
        .or_else(|| job.result.as_ref().and_then(|result| result.exit_code))
}

/// The test ids a red receipt names: the receipt's own `failures` (complete),
/// else the job's `failing_tests` names (`<package> [<target>] <test>`).
fn failing_test_ids(job: &AgentJobV1) -> Vec<String> {
    let mut ids: Vec<String> = receipt(job)
        .and_then(|receipt| receipt.get("failures"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|failure| failure.get("test")?.as_str())
        .filter(|test| !test.is_empty())
        .map(str::to_owned)
        .collect();
    if ids.is_empty() {
        ids = job
            .result
            .iter()
            .flat_map(|result| &result.failing_tests)
            .filter_map(|name| name.split_whitespace().last())
            .map(str::to_owned)
            .collect();
    }
    ids.sort();
    ids.dedup();
    ids
}

/// Whether the receipt records a run the script cut short or one with packages
/// that never produced a verdict.
fn unfinished(receipt: &Value) -> Option<String> {
    if receipt.get("partial").and_then(Value::as_bool) == Some(true) {
        return Some("the receipt is partial (the run was stopped)".into());
    }
    if let Some(reasons) = receipt
        .get("derive_failure")
        .filter(|value| !value.is_null())
    {
        return Some(format!("the plan could not be derived: {reasons}"));
    }
    for package in receipt
        .get("packages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let text = |key: &str| package.get(key).and_then(Value::as_str).unwrap_or_default();
        if UNFINISHED_STATUSES.contains(&text("status"))
            || UNFINISHED_PHASES.contains(&text("phase"))
        {
            return Some(format!(
                "package {} did not finish ({} {})",
                text("package"),
                text("status"),
                text("phase")
            ));
        }
    }
    None
}

/// Judge whether `head`, the commit a node session produced, is verified by one
/// of `session`'s jobs. `jobs` are the session's newest-first jobs, `known` the
/// open failure signatures (`known_failures`).
pub(crate) fn judge(
    jobs: &[AgentJobV1],
    session: Uuid,
    head: &str,
    known: &BTreeMap<String, u64>,
) -> Result<Evidence, Unverified> {
    let unverified = |reason, detail: String, evidence| Unverified {
        reason,
        detail,
        evidence,
    };
    let scoped: Vec<&AgentJobV1> = jobs
        .iter()
        .filter(|job| job.owner_session_id == session && scoped_test(job))
        .collect();
    let Some(newest) = scoped.first() else {
        return Err(unverified(
            Reason::NoReceipt,
            "this node's session owns no scoped_test job; after the last commit submit \
             AgentSubmitJob {\"kind\":\"test\",\"wake\":\"none\",\"params\":{\"scoped_test\":\
             {\"base\":\"origin/rolling\"}}} and wait for it"
                .into(),
            None,
        ));
    };
    if !newest.state.is_terminal() {
        return Err(unverified(
            Reason::Incomplete,
            format!(
                "scoped_test job {} is still {}; wait for it before handing off",
                newest.id,
                newest.state.as_str()
            ),
            None,
        ));
    }
    let Some(job) = scoped
        .iter()
        .find(|job| job.state.is_terminal() && receipt_head(job) == Some(head))
    else {
        // No job verified this commit: name the newest receipt's commit, or
        // the job that ended without one.
        return Err(
            match scoped.iter().find(|job| receipt_head(job).is_some()) {
                Some(stale) => {
                    let stale_head = receipt_head(stale).unwrap_or_default();
                    unverified(
                        Reason::StaleHead,
                        format!(
                            "scoped_test job {} verified {stale_head}, not the result commit {head}; \
                         commits after the last verification are unverified",
                            stale.id
                        ),
                        Some(evidence_of(stale, Vec::new(), Vec::new())),
                    )
                }
                None => unverified(
                    Reason::Incomplete,
                    format!(
                        "scoped_test job {} ended without a receipt ({})",
                        newest.id,
                        refusal(newest)
                    ),
                    None,
                ),
            },
        );
    };

    if job.state == JobState::Succeeded && exit_code(job) == Some(0) {
        return Ok(evidence_of(job, Vec::new(), Vec::new()));
    }
    let incomplete = |detail: String| {
        unverified(
            Reason::Incomplete,
            detail,
            Some(evidence_of(job, Vec::new(), Vec::new())),
        )
    };
    if let Some(refusal) = job
        .result
        .as_ref()
        .and_then(|result| result.refusal.as_deref())
    {
        return Err(incomplete(format!(
            "scoped_test job {} ended without a verdict: {refusal}",
            job.id
        )));
    }
    if let Some(why) = receipt(job).and_then(unfinished) {
        return Err(incomplete(format!(
            "scoped_test job {} did not finish: {why}",
            job.id
        )));
    }
    let ids = failing_test_ids(job);
    if ids.is_empty() {
        return Err(incomplete(format!(
            "scoped_test job {} exited {:?} with no failing test to attribute (a build, python or \
             static-guard failure)",
            job.id,
            exit_code(job)
        )));
    }
    let (mut known_failures, mut new_failures) = (Vec::new(), Vec::new());
    for id in ids {
        match known.get(&id) {
            Some(issue) => known_failures.push((id, *issue)),
            None => new_failures.push(id),
        }
    }
    let evidence = evidence_of(job, known_failures, new_failures);
    if evidence.new_failures.is_empty() {
        Ok(evidence)
    } else {
        Err(unverified(
            Reason::NewFailures,
            format!(
                "failing tests with no open known-failure record: {}; fix them (an open \
                 known-failure signature accepts a baseline red)",
                evidence.new_failures.join(", ")
            ),
            Some(evidence),
        ))
    }
}

fn refusal(job: &AgentJobV1) -> String {
    job.result
        .as_ref()
        .and_then(|result| result.refusal.clone())
        .unwrap_or_else(|| job.state.as_str().to_owned())
}

fn evidence_of(
    job: &AgentJobV1,
    known_failures: Vec<(String, u64)>,
    new_failures: Vec<String>,
) -> Evidence {
    Evidence {
        job_id: job.id,
        head: receipt_head(job).unwrap_or_default().to_owned(),
        exit_code: exit_code(job),
        plan: receipt(job)
            .and_then(|receipt| receipt.get("plan"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        failing_tests: job
            .result
            .as_ref()
            .map(|result| result.failing_tests.clone())
            .unwrap_or_default(),
        known_failures,
        new_failures,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use rsi_common::agent_jobs::AgentJobResultV1;
    use rsi_common::failure_signature::{FENCE_INFO, digest};

    /// The `result` of a real green `scoped_test` job (job `7c0c08b8`, a
    /// 2026-10-13 daemon run; only the temp-dir and tool paths are redacted).
    const REAL_GREEN: &str = r#"{"detail":"scoped-test passed","exit_code":0,"failing_tests":[],"landed_sha":null,"receipt":{"adaptive_threads":11,"base":"origin/rolling","baseline_comparison":{"base":"origin/rolling","baseline_reds":[],"status":"not_needed"},"baseline_reds":[],"cpu_quota":800.0,"dry_run":false,"exit_code":0,"failures":[],"filters":[],"head":"15b4bca9287377963c4c73f514d7d4569c120bca","host_threads":32,"job_cpu_quota":800.0,"job_timeout_minutes":60,"load1":8.3486328125,"log_dir":"/tmp/rsi-scoped-test-green","new_failures":null,"nextest_version":"0.9.137","ok":true,"packages":[],"parity_unavailable":null,"plan":"precise","priority_filters":[],"python":[],"runner":"nextest","schema":1,"scratch_mode":"tmpfs","scratch_parent":"/dev/shm","test_shards":8,"test_threads":8,"tool":{"blob":"185c7488cd4e777ecbca73fba8afbd3f485c23fa","path":"/jobs/scoped-test-tools/rev/scripts/scoped-test","rev":"13b0c007c6ccbd16c04ebe8ce5b7afc708e555f7","source":"rolling"}},"refusal":null,"sweep":null}"#;
    const REAL_GREEN_HEAD: &str = "15b4bca9287377963c4c73f514d7d4569c120bca";

    /// An excerpt of the `result` of a real red `scoped_test` job (job
    /// `a8d7bcbd`): one failing test, a `targeted_subset` plan, and the failed
    /// package's `incomplete` phase a red run really reports.
    const REAL_RED: &str = r#"{"exit_code":1,"failing_tests":["rsid session::rotation::tests::final_pipeline_handoff_from_epic_lead_still_rotates"],"receipt":{"base":"origin/rolling","baseline_comparison":{"base":"origin/rolling","baseline_reds":[],"status":"complete"},"baseline_reds":[],"exit_code":1,"failures":[{"kind":"lib","package":"rsid","target":"rsid","test":"session::rotation::tests::final_pipeline_handoff_from_epic_lead_still_rotates"}],"filters_not_run":[],"gate_covers_rest":true,"head":"cfc409c61dde33188c80216bef8b7d7cbd397c1d","new_failures":[{"kind":"lib","package":"rsid","target":"rsid","test":"session::rotation::tests::final_pipeline_handoff_from_epic_lead_still_rotates"}],"ok":false,"packages":[{"completed":40,"package":"rsid-store","run_sec":62.576,"slot_wait_sec":0.024,"status":"passed"},{"completed":789,"package":"rsid","phase":"incomplete","run_sec":369.219,"slot_wait_sec":0.0,"status":"failed"}],"plan":"targeted_subset","schema":1},"refusal":null}"#;
    const REAL_RED_HEAD: &str = "cfc409c61dde33188c80216bef8b7d7cbd397c1d";
    const RED_TEST: &str =
        "session::rotation::tests::final_pipeline_handoff_from_epic_lead_still_rotates";

    fn job_from(owner: Uuid, state: &str, result: Option<&str>, scoped: bool) -> AgentJobV1 {
        let result: Option<AgentJobResultV1> =
            result.map(|text| serde_json::from_str(text).expect("result"));
        let params = if scoped {
            json!({"kind":"test","scoped_test":{"base":"origin/rolling"}})
        } else {
            json!({"kind":"test","package":"rsid"})
        };
        serde_json::from_value(json!({
            "id": Uuid::new_v4(), "kind": "test", "name": null, "state": state,
            "owner_session_id": owner, "unit_name": "u", "cwd": "/tmp", "log_path": "/tmp/log",
            "params": params,
            "exit_code": result.as_ref().and_then(|r| r.exit_code),
            "result": result, "created_at": "2026-10-13T00:00:00.000000000Z",
            "finished_at": null,
        }))
        .expect("job")
    }

    fn known(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
        pairs.iter().map(|(t, i)| ((*t).to_owned(), *i)).collect()
    }

    /// The first test of #2249 (plan section 9): a receipt's `head` is the full
    /// 40-hex commit id, so it compares exactly with the node's observed
    /// `git rev-parse HEAD`, against a real receipt the daemon recorded.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn receipt_head_format_is_pinned_against_a_real_receipt() {
        let owner = Uuid::new_v4();
        let job = job_from(owner, "succeeded", Some(REAL_GREEN), true);
        let head = receipt_head(&job).expect("a real receipt names its head");
        assert_eq!(head, REAL_GREEN_HEAD);
        assert!(
            head.len() == 40 && head.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "a receipt head is a full lowercase commit id: {head}"
        );
        // The same receipt, read the way the executor reads it, verifies that
        // exact commit and no abbreviation or other commit.
        let evidence = judge(&[job.clone()], owner, REAL_GREEN_HEAD, &known(&[])).unwrap();
        assert_eq!(evidence.job_id, job.id);
        assert_eq!(evidence.head, REAL_GREEN_HEAD);
        assert_eq!(evidence.exit_code, Some(0));
        assert_eq!(evidence.plan.as_deref(), Some("precise"));
        let abbreviated = &REAL_GREEN_HEAD[..12];
        let stale = judge(&[job], owner, abbreviated, &known(&[])).unwrap_err();
        assert_eq!(stale.reason, Reason::StaleHead);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn a_session_without_a_scoped_test_job_has_no_receipt() {
        let owner = Uuid::new_v4();
        let stranger = job_from(Uuid::new_v4(), "succeeded", Some(REAL_GREEN), true);
        let unscoped = job_from(owner, "succeeded", Some(REAL_GREEN), false);
        let none = judge(&[stranger, unscoped], owner, REAL_GREEN_HEAD, &known(&[])).unwrap_err();
        assert_eq!(none.reason, Reason::NoReceipt);
        assert_eq!(none.evidence, None);
        let value = none.to_json();
        assert_eq!(value["reason"], "no_receipt");
        assert!(value["job_id"].is_null() && value["head"].is_null());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn a_receipt_for_another_commit_is_stale_and_a_later_green_one_verifies() {
        let owner = Uuid::new_v4();
        let old = job_from(owner, "succeeded", Some(REAL_GREEN), true);
        let stale = judge(&[old.clone()], owner, REAL_RED_HEAD, &known(&[])).unwrap_err();
        assert_eq!(stale.reason, Reason::StaleHead);
        assert_eq!(
            stale.evidence.as_ref().map(|e| e.head.as_str()),
            Some(REAL_GREEN_HEAD)
        );
        assert!(stale.detail.contains(REAL_RED_HEAD), "{}", stale.detail);

        // Newest first: a green job for the new commit verifies it.
        let fresh = job_from(
            owner,
            "succeeded",
            Some(&REAL_GREEN.replace(REAL_GREEN_HEAD, REAL_RED_HEAD)),
            true,
        );
        let evidence = judge(&[fresh.clone(), old], owner, REAL_RED_HEAD, &known(&[])).unwrap();
        assert_eq!(evidence.job_id, fresh.id);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn a_job_still_running_or_ended_without_a_receipt_is_incomplete() {
        let owner = Uuid::new_v4();
        let running = job_from(owner, "running", None, true);
        let done = job_from(owner, "succeeded", Some(REAL_GREEN), true);
        let early = judge(
            &[running, done.clone()],
            owner,
            REAL_GREEN_HEAD,
            &known(&[]),
        )
        .unwrap_err();
        assert_eq!(early.reason, Reason::Incomplete);
        assert!(early.detail.contains("still running"), "{}", early.detail);

        for refusal in [
            "job_timed_out",
            "interrupted_by_deploy",
            "scoped_test_receipt_missing",
        ] {
            let text = json!({"exit_code": null, "refusal": refusal}).to_string();
            let ended = job_from(owner, "failed", Some(&text), true);
            let err = judge(&[ended], owner, REAL_GREEN_HEAD, &known(&[])).unwrap_err();
            assert_eq!(err.reason, Reason::Incomplete, "{refusal}");
            assert!(err.detail.contains(refusal), "{}", err.detail);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn a_red_receipt_of_known_failures_only_is_verified_and_names_each_owner_issue() {
        let owner = Uuid::new_v4();
        let red = job_from(owner, "failed", Some(REAL_RED), true);
        let evidence = judge(
            &[red.clone()],
            owner,
            REAL_RED_HEAD,
            &known(&[(RED_TEST, 2123)]),
        )
        .unwrap();
        assert_eq!(evidence.exit_code, Some(1));
        assert_eq!(evidence.known_failures, vec![(RED_TEST.to_owned(), 2123)]);
        assert!(evidence.new_failures.is_empty());
        assert_eq!(evidence.plan.as_deref(), Some("targeted_subset"));
        let value = evidence.to_json(None);
        assert_eq!(value["known_failures"][0]["test"], RED_TEST);
        assert_eq!(value["known_failures"][0]["issue"], 2123);
        assert_eq!(value["exit_code"], 1);

        // The same red with no open signature is the node's own failure.
        let new = judge(&[red], owner, REAL_RED_HEAD, &known(&[("other::test", 9)])).unwrap_err();
        assert_eq!(new.reason, Reason::NewFailures);
        assert_eq!(
            new.evidence.unwrap().new_failures,
            vec![RED_TEST.to_owned()]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn known_failures_do_not_excuse_a_run_that_never_finished() {
        let owner = Uuid::new_v4();
        let signatures = known(&[(RED_TEST, 2123)]);
        let unfinished = |edit: &dyn Fn(&mut Value)| {
            let mut result: Value = serde_json::from_str(REAL_RED).unwrap();
            edit(&mut result["receipt"]);
            job_from(owner, "failed", Some(&result.to_string()), true)
        };
        for (label, job) in [
            (
                "build_failed",
                unfinished(&|r| r["packages"][1]["phase"] = "build_failed".into()),
            ),
            (
                "timed out",
                unfinished(&|r| r["packages"][1]["status"] = "timed out".into()),
            ),
            ("partial", unfinished(&|r| r["partial"] = true.into())),
            (
                "derive_failure",
                unfinished(&|r| r["derive_failure"] = json!(["no merge"])),
            ),
            ("job refusal", {
                let mut result: Value = serde_json::from_str(REAL_RED).unwrap();
                result["refusal"] = "job_timed_out".into();
                job_from(owner, "failed", Some(&result.to_string()), true)
            }),
        ] {
            let err = judge(&[job], owner, REAL_RED_HEAD, &signatures).unwrap_err();
            assert_eq!(err.reason, Reason::Incomplete, "{label}");
        }
        // A non-zero exit with no failing test to attribute is not a verdict.
        let mut result: Value = serde_json::from_str(REAL_RED).unwrap();
        result["failing_tests"] = json!([]);
        result["receipt"]["failures"] = json!([]);
        let blind = job_from(owner, "failed", Some(&result.to_string()), true);
        let err = judge(&[blind], owner, REAL_RED_HEAD, &signatures).unwrap_err();
        assert_eq!(err.reason, Reason::Incomplete);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn the_job_names_stand_in_when_a_receipt_lists_no_failures() {
        let owner = Uuid::new_v4();
        let mut result: Value = serde_json::from_str(REAL_RED).unwrap();
        result["receipt"]["failures"] = json!([]);
        let job = job_from(owner, "failed", Some(&result.to_string()), true);
        let evidence = judge(&[job], owner, REAL_RED_HEAD, &known(&[(RED_TEST, 7)])).unwrap();
        assert_eq!(evidence.known_failures, vec![(RED_TEST.to_owned(), 7)]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn known_failures_come_from_live_signature_issues_only() {
        let record = |test: &str, issue: u64| {
            let record = json!({
                "test_id": test, "matcher": {"digest": digest("boom")},
                "issue": issue, "class": "regression",
            });
            format!("```{FENCE_INFO}\n{record}\n```\n")
        };
        let source = |number: u64, status: &str, body: String| SignatureIssueSource {
            issue_id: Uuid::new_v4(),
            display_number: number,
            status: status.into(),
            body,
        };
        let map = known_failures(&[
            source(30, "Open", record("a::b", 30)),
            source(10, "InProgress", record("a::b", 10)),
            source(20, "Closed", record("c::d", 20)),
            source(40, "Open", format!("```{FENCE_INFO}\n{{not json}}\n```\n")),
        ]);
        // Lowest owner for a test two Issues name; a closed or malformed Issue names nothing.
        assert_eq!(map, known(&[("a::b", 10)]));
    }
}
