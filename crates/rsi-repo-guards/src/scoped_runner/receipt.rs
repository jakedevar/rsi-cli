//! Receipt aggregation from observed worker events. Progress can never be green.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::LazyLock;

// systemd reports an ordinary failing test as `exit-code`, followed by
// `code=exited, status=1/FAILURE`. Neither is evidence of interruption (#3991).
static UNIT_KILLED: LazyLock<crate::re::Re> =
    LazyLock::new(|| crate::re::Re::new(r"\bcode=(?:killed|dumped)\b"));
static UNIT_RESULT: LazyLock<crate::re::Re> =
    LazyLock::new(|| crate::re::Re::new(r#"(?:Finished with result:|Result=)\s*['"]?([\w-]+)"#));

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
pub struct Failure {
    pub package: String,
    pub kind: String,
    pub target: String,
    pub test: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Package {
    pub package: String,
    pub status: String,
    pub phase: String,
    pub completed: usize,
    pub failed_tests: Vec<Failure>,
    pub unfinished_tests: Vec<String>,
    pub worker_signal: Option<i32>,
    pub new_failures: Option<Vec<Failure>>,
    pub baseline_reds: Vec<Failure>,
}
impl Package {
    pub fn pending(package: &str) -> Self {
        Self {
            package: package.into(),
            status: "pending".into(),
            phase: "incomplete".into(),
            completed: 0,
            failed_tests: vec![],
            unfinished_tests: vec![],
            worker_signal: None,
            new_failures: None,
            baseline_reds: vec![],
        }
    }
}

#[derive(Default)]
pub struct WorkerEvidence {
    pub phases: BTreeSet<String>,
    pub matched: BTreeSet<String>,
    pub failures: BTreeSet<Failure>,
    pub active: BTreeSet<String>,
    pub completed: BTreeSet<(String, String)>,
    pub executed: Option<usize>,
    pub complete: Option<usize>,
    pub warnings: Vec<Value>,
    pub subsets: Vec<Value>,
    pub malformed: bool,
    pub child_signal: Option<i32>,
    unit_interrupted: bool,
    binary: String,
}
impl WorkerEvidence {
    pub fn observe(&mut self, package: &str, line: &str) {
        let line = line.trim_end_matches(['\n', '\r']);
        self.unit_interrupted |= UNIT_KILLED.is_match(line)
            || UNIT_RESULT.search(line).is_some_and(|c| {
                !matches!(c.get(1).map(|m| m.as_str()), Some("success" | "exit-code"))
            });
        if let Some(signal) = line.strip_prefix("SCOPED_PROCESS_SIGNAL ") {
            self.child_signal = match signal {
                "SIGINT" => Some(2),
                "SIGKILL" => Some(9),
                "SIGTERM" => Some(15),
                _ => {
                    self.malformed = true;
                    None
                }
            };
        }
        if let Some(name) = line.strip_prefix("SCOPED_TARGET ") {
            self.binary = name.into();
        } else if let Some(name) = line.strip_prefix("SCOPED_START ") {
            self.active.insert(name.into());
        } else if let Some(name) = line.strip_prefix("SCOPED_END ") {
            if self.active.remove(name) {
                self.completed.insert((self.binary.clone(), name.into()));
            }
        } else if let Some(code) = line.strip_prefix("SCOPED_TEST_EXIT ") {
            if let Ok(code) = code.parse::<i32>() {
                if code < 0 {
                    self.child_signal = Some(-code);
                }
            } else {
                self.malformed = true;
            }
        } else if let Some(filter) = line.strip_prefix("SCOPED_MATCH ") {
            self.matched.insert(filter.into());
        } else if let Some(phase) = line.strip_prefix("SCOPED_PHASE ") {
            self.phases.insert(phase.into());
        } else if let Some(rest) = line.strip_prefix("SCOPED_FAILURE ") {
            if let Some((target, test)) = rest.split_once(' ') {
                if let Some((kind, target)) = target.split_once(':') {
                    self.failures.insert(Failure {
                        package: package.into(),
                        kind: kind.into(),
                        target: target.into(),
                        test: test.into(),
                    });
                } else {
                    self.malformed = true;
                }
            } else {
                self.malformed = true;
            }
        } else if let Some(rest) = line.strip_prefix("SCOPED_EXECUTED ") {
            self.executed = Self::count(package, rest);
            if self.executed.is_none() {
                self.malformed = true;
            }
        } else if let Some(rest) = line.strip_prefix("SCOPED_COMPLETE ") {
            self.complete = Self::count(package, rest);
            if self.complete.is_none() {
                self.malformed = true;
            }
        } else if let Some(rest) = line.strip_prefix("SCOPED_WARNINGS ") {
            match serde_json::from_str(rest) {
                Ok(value) => self.warnings.push(value),
                Err(_) => self.malformed = true,
            }
        } else if let Some(rest) = line.strip_prefix("SCOPED_SUBSET ") {
            match serde_json::from_str::<Value>(rest) {
                Ok(value) if value["package"] == package => self.subsets.push(value),
                _ => self.malformed = true,
            }
        } else if let Some(rest) = line.strip_prefix("SCOPED_UNFINISHED ") {
            match serde_json::from_str::<Value>(rest) {
                Ok(value) => {
                    if let Some(tests) = value["tests"].as_array() {
                        self.active
                            .extend(tests.iter().filter_map(Value::as_str).map(Into::into));
                    } else {
                        self.malformed = true;
                    }
                }
                Err(_) => self.malformed = true,
            }
        }
    }
    fn count(package: &str, rest: &str) -> Option<usize> {
        let (owner, count) = rest.rsplit_once(' ')?;
        (owner == package).then(|| count.parse().ok()).flatten()
    }
    pub fn settle(&self, package: &str, code: i32, timed_out: bool) -> Package {
        let mut entry = Package::pending(package);
        entry.completed = self
            .executed
            .unwrap_or(self.completed.len())
            .max(self.completed.len());
        entry.failed_tests = self.failures.iter().cloned().collect();
        entry.unfinished_tests = self.active.iter().cloned().collect();
        entry.worker_signal = (code < 0).then_some(-code).or(self.child_signal);
        entry.phase = if self.malformed {
            "invalid_worker_evidence"
        } else if timed_out {
            "timed_out"
        } else if entry.worker_signal.is_some() || self.unit_interrupted || code >= 128 {
            "incomplete"
        } else if self.phases.contains("build_failed") {
            "build_failed"
        } else if self.phases.contains("list_failed") {
            "list_failed"
        } else if !self.active.is_empty() {
            "incomplete"
        } else if self.phases.contains("test_failed")
            && (self.executed.is_none()
                || self.subsets.iter().any(|s| s["selected"] != s["executed"]))
        {
            "incomplete"
        } else if self.phases.contains("test_failed") {
            "test_failed"
        } else if self.complete == Some(self.executed.unwrap_or(self.completed.len())) && code == 0
        {
            "complete"
        } else {
            "incomplete"
        }
        .into();
        entry.status = if entry.phase == "complete" {
            "passed"
        } else {
            "failed"
        }
        .into();
        entry
    }
}

pub struct Receipt {
    pub value: Value,
}
impl Receipt {
    pub fn new(base: &str, head: &str, tool: Value) -> Self {
        Self {
            value: json!({"schema":1,"base":base,"head":head,"tool":tool,"dry_run":false,"ok":false,"exit_code":2,"packages":[],"failures":[],"new_failures":null,"baseline_reds":[],"python":[],"desktop":[],"static_guards":{},"phase_order":[],"compile":{"planning":{"source":"rust_derivation","cargo_invoked":false},"execution":"pending"},"plan":"precise"}),
        }
    }
    pub fn package(&mut self, entry: Package) {
        let packages = self.value["packages"]
            .as_array_mut()
            .expect("receipt package array");
        if let Some(old) = packages.iter_mut().find(|p| p["package"] == entry.package) {
            *old = json!(entry);
        } else {
            packages.push(json!(entry));
        }
    }
    pub fn progress(&self) -> Value {
        super::persistence::wire(&self.value, true)
    }
    pub fn classify(&mut self, failures: &[Failure], base_failures: Option<&BTreeSet<Failure>>) {
        self.value["failures"] = json!(failures);
        let (new, baseline) = if failures.is_empty() {
            (Some(vec![]), vec![])
        } else if let Some(base) = base_failures {
            (
                Some(
                    failures
                        .iter()
                        .filter(|f| !base.contains(*f))
                        .cloned()
                        .collect::<Vec<_>>(),
                ),
                failures
                    .iter()
                    .filter(|f| base.contains(*f))
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        } else {
            (None, vec![])
        };
        self.value["new_failures"] = json!(new);
        self.value["baseline_reds"] = json!(baseline);
        for package in self.value["packages"].as_array_mut().into_iter().flatten() {
            package["new_failures"] = json!(new.as_ref().map(|failures| {
                failures
                    .iter()
                    .filter(|f| f.package == package["package"])
                    .collect::<Vec<_>>()
            }));
            package["baseline_reds"] = json!(
                baseline
                    .iter()
                    .filter(|f| f.package == package["package"])
                    .collect::<Vec<_>>()
            );
        }
    }
    /// The caller supplies process exit, but cannot make incomplete evidence
    /// green. Mandatory lane slots must be present, even when selected empty.
    pub fn finish(&mut self, code: i32) -> Value {
        let isolation = self.value["pid_isolation"] == "namespaced"
            || (self.value["pid_isolation"] == "unavailable"
                && self.value["pid_isolation_reason"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty() && s != "not_probed"));
        let guards = self.value["static_guards"].as_object().is_some_and(|g| {
            super::orchestration::REQUIRED_GUARDS
                .iter()
                .all(|name| g.get(*name).is_some_and(|v| v["status"] == "ok"))
                && g.values().all(|v| v["status"] == "ok")
        });
        let lanes = ["residual", "desktop"].iter().all(|lane| {
            self.value[*lane]
                .as_array()
                .is_some_and(|entries| entries.iter().all(|v| v["status"] == "passed"))
        });
        let packages = self.value["packages"].as_array().is_some_and(|entries| {
            entries.iter().all(|v| {
                v["status"] == "passed"
                    || (self.value["gate_covers_rest"] == true
                        && v["status"] == "not run"
                        && v["phase"] == "time_box_expired")
                    || (code == 1 && v["status"] == "failed" && v["phase"] == "test_failed")
            })
        });
        let no_unknown_failures = self.value["new_failures"]
            .as_array()
            .is_some_and(Vec::is_empty);
        let warning_ok = self.value["warnings"]["status"]
            .as_str()
            .is_some_and(|s| ["ok", "not_measured", "not_applicable"].contains(&s));
        let baseline_ok = baseline_only_ok_reason(&self.value);
        let ok = guards
            && isolation
            && lanes
            && packages
            && no_unknown_failures
            && warning_ok
            && self.value["unmatched_filters"]
                .as_array()
                .is_some_and(Vec::is_empty)
            && (code == 0 || (code == 1 && baseline_ok.is_some()))
            && self.value["source_verified"] == true
            && self.value["dry_run"] == false;
        self.value["ok"] = json!(ok);
        self.value["exit_code"] = json!(code);
        self.value
            .as_object_mut()
            .expect("receipt object")
            .remove("ok_reason");
        if ok {
            if let Some(reason) = baseline_ok {
                self.value["ok_reason"] = json!(reason);
            }
        }
        self.value.clone()
    }
}

pub fn baseline_only_ok_reason(receipt: &Value) -> Option<&'static str> {
    if receipt["dry_run"] == true
        || receipt["baseline_comparison"]["status"] != "complete"
        || receipt["new_failures"] != json!([])
        || receipt["linux_check"]["status"] == "failed"
    {
        return None;
    }
    if receipt["unmatched_filters"]
        .as_array()
        .is_none_or(|v| !v.is_empty())
    {
        return None;
    }
    if receipt["warnings"]["status"]
        .as_str()
        .is_none_or(|s| !["ok", "not_measured", "not_applicable"].contains(&s))
    {
        return None;
    }
    let failures: Vec<Failure> = serde_json::from_value(receipt["failures"].clone()).ok()?;
    let reds: Vec<Failure> = serde_json::from_value(receipt["baseline_reds"].clone()).ok()?;
    if failures.is_empty() || !failures.iter().all(|f| reds.contains(f)) {
        return None;
    }
    for lane in ["residual", "desktop"] {
        if receipt[lane]
            .as_array()
            .is_none_or(|v| v.iter().any(|e| e["status"] != "passed"))
        {
            return None;
        }
    }
    if receipt["packages"].as_array().is_none_or(|v| {
        v.iter().any(|p| {
            p["status"] != "passed" && !(p["status"] == "failed" && p["phase"] == "test_failed")
        })
    }) {
        return None;
    }
    let records = receipt["baseline_failure_records"].as_array()?;
    let recorded_names: BTreeSet<Failure> = records
        .iter()
        .map(|record| serde_json::from_value(record.clone()))
        .collect::<Result<_, _>>()
        .ok()?;
    if records.len() != reds.len()
        || recorded_names != reds.iter().cloned().collect()
        || records
            .iter()
            .any(|r| r["status"] != "recorded" && r["status"] != "unavailable")
    {
        return None;
    }
    Some(if records.iter().any(|r| r["status"] == "unavailable") {
        "baseline_confirmed_without_snapshot"
    } else {
        "baseline_known_failure"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finished_failure() -> WorkerEvidence {
        let mut evidence = WorkerEvidence::default();
        let fixture = std::fs::read_to_string(
            crate::testutil::repo_root()
                .join("scripts/tests/fixtures/scoped-test-failure/test-failure.txt"),
        )
        .unwrap();
        for line in fixture.lines().chain([
            "SCOPED_EXECUTED rsid 1",
            "          Finished with result: exit-code",
            "Main processes terminated with: code=exited, status=1/FAILURE",
        ]) {
            evidence.observe("rsid", line);
        }
        evidence
    }

    #[test]
    fn ordinary_systemd_failure_keeps_the_finished_test_failure_phase_3991() {
        let evidence = finished_failure();
        let entry = evidence.settle("rsid", 1, false);
        assert_eq!(entry.phase, "test_failed");
        assert_eq!(entry.completed, 1);
        assert_eq!(entry.failed_tests[0].test, "tests::example");
        assert_eq!(entry.worker_signal, None);
    }

    #[test]
    fn interruption_and_incomplete_coverage_veto_finished_failure_markers_3991() {
        for suffix in [
            "SCOPED_PROCESS_SIGNAL SIGINT",
            "SCOPED_PROCESS_SIGNAL SIGKILL",
            "SCOPED_PROCESS_SIGNAL SIGTERM",
            "SCOPED_TEST_EXIT -2",
            "SCOPED_TEST_EXIT -9",
            "SCOPED_TEST_EXIT -15",
            "Main processes terminated with: code=killed, status=2/INT",
            "Main processes terminated with: code=killed, status=9/KILL",
            "Main processes terminated with: code=killed, status=15/TERM",
            "Main process exited, code=dumped, status=11/SEGV",
            "Finished with result: timeout",
            "Result='signal'",
            "Result=oom-kill",
            "SCOPED_START suite::unfinished",
            "SCOPED_UNFINISHED {\"tests\":[\"suite::unfinished\"]}",
            "SCOPED_SUBSET {\"package\":\"rsid\",\"selected\":2,\"executed\":1}",
        ] {
            let mut evidence = finished_failure();
            evidence.observe("rsid", suffix);
            assert_eq!(
                evidence.settle("rsid", 1, false).phase,
                "incomplete",
                "{suffix}"
            );
        }
        for code in [-2, -9, -15, 130, 137, 143] {
            assert_eq!(
                finished_failure().settle("rsid", code, false).phase,
                "incomplete"
            );
        }
        assert_eq!(
            finished_failure().settle("rsid", 1, true).phase,
            "timed_out"
        );
        let mut evidence = finished_failure();
        evidence.executed = None;
        assert_eq!(evidence.settle("rsid", 1, false).phase, "incomplete");
        evidence.observe("rsid", "SCOPED_EXECUTED rsid malformed");
        assert_eq!(
            evidence.settle("rsid", 1, false).phase,
            "invalid_worker_evidence"
        );
    }

    #[test]
    fn successful_completion_requires_matching_counts_and_no_unfinished_tests_3991() {
        for tail in ["SCOPED_COMPLETE rsid 0", "SCOPED_START suite::unfinished"] {
            let mut evidence = WorkerEvidence::default();
            evidence.observe("rsid", "SCOPED_EXECUTED rsid 1");
            evidence.observe("rsid", "SCOPED_COMPLETE rsid 1");
            evidence.observe("rsid", tail);
            assert_eq!(
                evidence.settle("rsid", 0, false).phase,
                "incomplete",
                "{tail}"
            );
        }
        let mut compile_only = WorkerEvidence::default();
        compile_only.observe("rsid", "SCOPED_PHASE compiled");
        compile_only.observe("rsid", "SCOPED_COMPLETE rsid 0");
        assert_eq!(compile_only.settle("rsid", 0, false).phase, "complete");
    }

    #[test]
    fn duplicate_and_binary_scoped_completion_preserves_real_counts() {
        let mut evidence = WorkerEvidence::default();
        for line in [
            "SCOPED_TARGET a",
            "SCOPED_START same",
            "SCOPED_END same",
            "SCOPED_END same",
            "SCOPED_TARGET b",
            "SCOPED_START same",
            "SCOPED_END same",
            "SCOPED_EXECUTED pkg 2",
            "SCOPED_COMPLETE pkg 2",
        ] {
            evidence.observe("pkg", line);
        }
        let entry = evidence.settle("pkg", 0, false);
        assert_eq!(entry.completed, 2);
        assert_eq!(entry.status, "passed");
        evidence.observe("pkg", "SCOPED_START unfinished");
        let interrupted = evidence.settle("pkg", -9, false);
        assert_eq!(interrupted.worker_signal, Some(9));
        assert_eq!(interrupted.unfinished_tests, vec!["unfinished"]);
        assert_eq!(interrupted.status, "failed");
    }
    #[test]
    fn baseline_requires_reproduced_named_failures_and_all_other_gates() {
        let evidence = finished_failure();
        let failure = evidence.failures.iter().next().unwrap().clone();
        let mut receipt = Receipt::new("base", "head", json!({}));
        receipt.package(evidence.settle("rsid", 1, false));
        receipt.classify(
            std::slice::from_ref(&failure),
            Some(&BTreeSet::from([failure.clone()])),
        );
        receipt.value["baseline_comparison"] = json!({"status":"complete"});
        let mut record = json!(failure);
        record["status"] = json!("unavailable");
        receipt.value["baseline_failure_records"] = json!([record]);
        receipt.value["static_guards"] = Value::Object(
            super::super::orchestration::REQUIRED_GUARDS
                .iter()
                .map(|name| ((*name).into(), json!({"status":"ok"})))
                .collect(),
        );
        receipt.value["residual"] = json!([]);
        receipt.value["desktop"] = json!([]);
        receipt.value["warnings"] = json!({"status":"not_measured"});
        receipt.value["unmatched_filters"] = json!([]);
        receipt.value["source_verified"] = json!(true);
        crate::pid_isolation::Isolation::unavailable("not_linux")
            .status()
            .record(&mut receipt.value);
        assert_eq!(receipt.finish(1)["ok"], true);
        assert_eq!(
            receipt.value["ok_reason"],
            "baseline_confirmed_without_snapshot"
        );
        for tail in ["SCOPED_PROCESS_SIGNAL SIGTERM", "SCOPED_START unfinished"] {
            let mut interrupted = finished_failure();
            interrupted.observe("rsid", tail);
            receipt.package(interrupted.settle("rsid", 1, false));
            assert_eq!(receipt.finish(1)["ok"], false, "{tail}");
        }
        receipt.package(evidence.settle("rsid", 1, false));
        receipt.value["baseline_failure_records"][0]["test"] = json!("suite::other");
        assert_eq!(receipt.finish(1)["ok"], false);
        receipt.value["baseline_failure_records"][0]["test"] = json!(failure.test);
        receipt.value["baseline_failure_records"][0]["status"] = json!("unknown");
        receipt.value["baseline_failure_records"][0]["qa_issue_advisories"] =
            json!([{"issue":430}]);
        assert_eq!(receipt.finish(1)["ok"], false);
        receipt.value["baseline_failure_records"][0]["status"] = json!("unavailable");
        receipt.value["desktop"] = json!([{"status":"runner_unavailable"}]);
        assert_eq!(receipt.finish(1)["ok"], false);
        assert_eq!(receipt.progress()["ok"], false);
    }
    #[test]
    fn retained_receipts_cannot_credit_incomplete_baseline_reds_without_worker_evidence_3991() {
        let corpus = std::fs::read_to_string(
            crate::testutil::repo_root()
                .join("crates/rsi-scoped-receipt/tests/golden/receipts.jsonl"),
        )
        .unwrap();
        let mut checked = 0;
        for line in corpus.lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            let receipt = &row["receipt"];
            if receipt["packages"]
                .as_array()
                .is_some_and(|packages| packages.iter().any(|p| p["phase"] == "incomplete"))
            {
                assert_eq!(baseline_only_ok_reason(receipt), None, "{}", row["job_id"]);
                checked += 1;
            }
        }
        assert!(checked > 0);
    }
    #[test]
    fn absent_lanes_malformed_evidence_and_unclassified_failures_never_pass() {
        let mut receipt = Receipt::new("base", "head", json!({}));
        assert_eq!(receipt.finish(0)["ok"], false);
        let mut evidence = WorkerEvidence::default();
        evidence.observe("pkg", "SCOPED_WARNINGS broken");
        evidence.observe("pkg", "SCOPED_COMPLETE pkg 1");
        assert_eq!(
            evidence.settle("pkg", 0, false).phase,
            "invalid_worker_evidence"
        );
        receipt.classify(
            &[Failure {
                package: "pkg".into(),
                kind: "lib".into(),
                target: "pkg".into(),
                test: "red".into(),
            }],
            None,
        );
        assert_eq!(receipt.value["new_failures"], Value::Null);
    }
}
