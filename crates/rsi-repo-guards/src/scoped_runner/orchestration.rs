//! Parent sequencing and failure semantics, independent of the not-yet-ported
//! helper adapters. This API is not a facility to import acceptance receipts.
use super::{
    execution::PackagePlan,
    receipt::{Failure, Package, Receipt, WorkerEvidence},
};
use crate::pid_isolation::Isolation;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const REQUIRED_GUARDS: &[&str] = &[
    "whitespace",
    "new_python",
    "shard_gates",
    "released_migrations",
    "starter_pins",
    "settings_preflight",
    "identity_assertions",
    "target_reclaim",
    "guard_regressions",
    "store_boundary",
    "test_env_writes",
    "personal_paths",
    "agents_md_size",
    "portable_shell",
];

pub struct PlannedPackage {
    pub plan: PackagePlan,
    pub downstream_compile: bool,
}
pub struct RunPlan {
    /// Original admitted repository: its job-local snapshot is independent of
    /// any integrated/renumbered candidate or baseline scratch worktree.
    pub repo: PathBuf,
    pub log_dir: PathBuf,
    pub base: String,
    pub head: String,
    pub tool: Value,
    pub filters: Vec<String>,
    pub downstream_compile: Value,
    pub downstream_tests: Value,
    pub packages: Vec<PlannedPackage>,
    pub runtime_max: Duration,
    pub total_max: Duration,
    pub admission_max: Duration,
    pub test_time_box: Option<Duration>,
    pub dry_run: bool,
    /// Planner-derived receipt fields (slow tier, runner, features, notes...).
    /// Applied before the orchestration owns its own keys, so they never win.
    pub receipt_fields: serde_json::Map<String, Value>,
}
pub struct WorkerExit {
    pub code: i32,
    pub timed_out: bool,
}
pub struct BaselineComparison {
    pub complete: bool,
    pub failures: BTreeSet<Failure>,
    pub detail: Value,
    /// Top-level receipt fields the replay owns (for example `stale_base_reds`).
    pub receipt: serde_json::Map<String, Value>,
}

/// Implementations must execute their canonical Rust helpers/lanes and return
/// their observed results. In production this adapter is compiled with the
/// runner, content-pinned by the daemon; no CLI accepts a fabricated backend.
/// No default method silently skips an unavailable mandatory capability.
pub trait Backend {
    fn verify_source(&mut self) -> io::Result<()>;
    /// Probe once on the host. Each lane wraps only its bounded child command;
    /// the admission owner must remain outside the namespace.
    fn pid_isolation(&mut self) -> io::Result<Isolation>;
    fn static_guards(&mut self) -> io::Result<BTreeMap<String, Value>>;
    fn residual(&mut self, dry_run: bool, isolation: &Isolation) -> io::Result<Vec<Value>>;
    fn desktop(&mut self, dry_run: bool, isolation: &Isolation) -> io::Result<Vec<Value>>;
    fn admit(&mut self, cap: Duration) -> io::Result<()>;
    fn package(
        &mut self,
        plan: &PackagePlan,
        budget: Duration,
        time_box: Option<Duration>,
        isolation: &Isolation,
        emit: &mut dyn FnMut(&str),
    ) -> io::Result<WorkerExit>;
    fn warnings(&mut self, payloads: &[Value]) -> io::Result<Value>;
    fn compare_baseline(&mut self, failures: &[Failure]) -> io::Result<BaselineComparison>;
    fn linux_check(&mut self) -> io::Result<Option<Value>>;
    fn merge_check(&mut self) -> io::Result<Value>;
    /// Runs even after errors/timeouts; a concrete adapter also owns its lease
    /// with RAII so panic/cancellation cannot leak an admission.
    fn release(&mut self) -> io::Result<()>;
}

fn lane_failed(entries: &[Value], dry: bool) -> bool {
    entries
        .iter()
        .any(|entry| entry["status"] != if dry { "planned" } else { "passed" })
}
fn lane_timeout(entries: &[Value]) -> bool {
    entries.iter().any(|entry| entry["status"] == "timed_out")
}

pub fn run(plan: &RunPlan, backend: &mut impl Backend, mut progress: impl FnMut(&Value)) -> Value {
    let mut receipt = Receipt::new(&plan.base, &plan.head, plan.tool.clone());
    for (key, value) in &plan.receipt_fields {
        receipt.value[key] = value.clone();
    }
    receipt.value["dry_run"] = json!(plan.dry_run);
    receipt.value["filters"] = json!(plan.filters);
    receipt.value["downstream_compile"] = plan.downstream_compile.clone();
    receipt.value["downstream_tests"] = plan.downstream_tests.clone();
    // The planner may box a plan without a time limit (`--test-time-box-sec 0`).
    if !plan.receipt_fields.contains_key("plan") {
        receipt.value["plan"] = json!(if plan.test_time_box.is_some() {
            "targeted_subset"
        } else {
            "precise"
        });
    }
    if !plan.receipt_fields.contains_key("gate_covers_rest") {
        receipt.value["gate_covers_rest"] = json!(plan.test_time_box.is_some());
    }
    let outcome = (|| {
        backend.verify_source()?;
        super::persistence::persist_selection(&plan.log_dir, &mut receipt.value)?;
        emit_progress(plan, &receipt, &mut progress)?;
        run_inner(plan, backend, &mut receipt, &mut progress)
    })();
    let release = backend.release();
    if !plan.dry_run {
        for entry in receipt.value["downstream_compile"]["packages"]
            .as_array_mut()
            .into_iter()
            .flatten()
        {
            if entry["status"] == "pending" {
                entry["status"] = json!("not_run");
                entry["reason"] = json!("run stopped before downstream compilation completed");
            }
        }
    }
    let code = match (outcome, release) {
        (Ok(code), Ok(())) => code,
        (Err(error), _) | (_, Err(error)) => {
            receipt.value["failure_class"] = json!(if error.kind() == io::ErrorKind::TimedOut {
                "admission_timeout"
            } else {
                "runner_unavailable"
            });
            receipt.value["diagnostic"] = json!(error.to_string());
            if error.kind() == io::ErrorKind::TimedOut {
                124
            } else {
                2
            }
        }
    };
    super::persistence::wire(&receipt.finish(code), false)
}
/// How one package's supervised build went, from its SCOPED_PHASE lines.
fn compile_status(phases: &BTreeSet<String>) -> &'static str {
    if phases.contains("build_failed") {
        "failed"
    } else if ["compiled", "listed", "list_failed", "test_failed"]
        .iter()
        .any(|p| phases.contains(*p))
    {
        "executed"
    } else {
        "incomplete"
    }
}
/// Keep the downstream compile entry and `compile.packages` in step with the
/// package's observed verdict (the immutable selection file keeps its plan).
fn mark_downstream(receipt: &mut Receipt, package: &str, status: &str, phases: &BTreeSet<String>) {
    receipt.value["compile"]["packages"][package] = json!(compile_status(phases));
    for entry in receipt.value["downstream_compile"]["packages"]
        .as_array_mut()
        .into_iter()
        .flatten()
    {
        if entry["package"] == package {
            entry["status"] = json!(status);
        }
    }
}
fn emit_progress(
    plan: &RunPlan,
    receipt: &Receipt,
    progress: &mut impl FnMut(&Value),
) -> io::Result<()> {
    let snapshot = super::persistence::persist_progress(&plan.log_dir, &receipt.value)?;
    progress(&snapshot);
    Ok(())
}
fn run_inner(
    plan: &RunPlan,
    backend: &mut impl Backend,
    receipt: &mut Receipt,
    progress: &mut impl FnMut(&Value),
) -> io::Result<i32> {
    backend.verify_source()?;
    let isolation = backend.pid_isolation()?;
    isolation.status().record(&mut receipt.value);
    let started = Instant::now();
    let guards = backend.static_guards()?;
    let static_failed = REQUIRED_GUARDS.iter().any(|name| {
        guards
            .get(*name)
            .is_none_or(|entry| entry["status"] != "ok")
    }) || guards.values().any(|entry| entry["status"] != "ok");
    receipt.value["static_guards"] = json!(guards);
    if static_failed {
        return Ok(1);
    }
    receipt.value["phase_order"] = json!(["residual", "desktop", "cargo"]);
    let residual = backend.residual(plan.dry_run, &isolation)?;
    // Keep the existing receipt lane until #366 changes its consumers together.
    receipt.value["python"] = json!(residual);
    receipt.value["residual"] = json!(residual);
    if lane_failed(&residual, plan.dry_run) {
        return Ok(if lane_timeout(&residual) { 124 } else { 1 });
    }
    let desktop = backend.desktop(plan.dry_run, &isolation)?;
    receipt.value["desktop"] = json!(desktop);
    if lane_timeout(&desktop) {
        return Ok(124);
    }
    let mut failed = lane_failed(&desktop, plan.dry_run);
    if plan.dry_run {
        receipt.value["compile"]["execution"] = json!("skipped_dry_run");
        for package in &plan.packages {
            let mut entry = Package::pending(&package.plan.package);
            entry.status = "planned".into();
            receipt.package(entry);
        }
        return Ok(if failed { 1 } else { 0 });
    }
    let needs_admission = !plan.packages.is_empty();
    let waited = if needs_admission {
        let wait_started = Instant::now();
        backend.admit(plan.admission_max)?;
        let waited = wait_started.elapsed();
        receipt.value["admission"] = json!({"status":"held","waited_sec":waited.as_secs_f64()});
        waited
    } else {
        Duration::ZERO
    };
    let mut box_left = plan.test_time_box;
    let mut warning_payloads = Vec::new();
    let mut failures = BTreeSet::new();
    let mut matched = BTreeSet::new();
    let mut listed = BTreeSet::new();
    let mut subsets = Vec::new();
    for package in &plan.packages {
        backend.verify_source()?;
        let elapsed = started.elapsed().saturating_sub(waited);
        let left = plan
            .total_max
            .checked_sub(elapsed)
            .ok_or_else(|| io::Error::other("scoped_test_timeout: total budget exhausted"))?;
        let budget = plan.runtime_max.min(left);
        if budget.is_zero() {
            return Err(io::Error::other(
                "scoped_test_timeout: total budget exhausted",
            ));
        }
        let owns_changed = !package.plan.changed_modules.is_empty();
        if box_left == Some(Duration::ZERO) && !owns_changed && !package.downstream_compile {
            matched.extend(package.plan.selections.iter().map(|s| s.filter.clone()));
            let mut entry = Package::pending(&package.plan.package);
            entry.status = "not run".into();
            entry.phase = "time_box_expired".into();
            receipt.package(entry);
            continue;
        }
        let package_box = if owns_changed && box_left == Some(Duration::ZERO) {
            Some(Duration::from_micros(1))
        } else {
            box_left
        };
        let mut evidence = WorkerEvidence::default();
        let mut progress_error = None;
        let mut entry = Package::pending(&package.plan.package);
        entry.status = "running".into();
        entry.phase = "building".into();
        receipt.package(entry);
        receipt.value["compile"]["execution"] = json!("executed");
        emit_progress(plan, receipt, progress)?;
        let exit = backend.package(
            &package.plan,
            budget,
            package_box,
            &isolation,
            &mut |line| {
                evidence.observe(&package.plan.package, line);
                let phase = line
                    .trim_end()
                    .strip_prefix("SCOPED_PHASE ")
                    .or_else(|| line.starts_with("SCOPED_TARGET ").then_some("testing"));
                if let Some(phase) = phase {
                    let mut entry = Package::pending(&package.plan.package);
                    entry.status = "running".into();
                    entry.phase = phase.into();
                    entry.completed = evidence.completed.len();
                    entry.failed_tests = evidence.failures.iter().cloned().collect();
                    entry.unfinished_tests = evidence.active.iter().cloned().collect();
                    receipt.package(entry);
                    if let Err(error) = emit_progress(plan, receipt, progress) {
                        progress_error = Some(error);
                    }
                }
            },
        )?;
        if let Some(error) = progress_error {
            return Err(error);
        }
        backend.verify_source()?;
        let entry = evidence.settle(&package.plan.package, exit.code, exit.timed_out);
        failed |= entry.status != "passed";
        if evidence.phases.contains("listed") {
            listed.insert(package.plan.package.clone());
        }
        failures.extend(evidence.failures.clone());
        matched.extend(evidence.matched.clone());
        warning_payloads.extend(evidence.warnings.clone());
        let test_sec = evidence
            .subsets
            .iter()
            .filter_map(|s| s["test_sec"].as_f64())
            .fold(0.0, f64::max);
        if let Some(left) = box_left.as_mut() {
            if test_sec.is_finite() && test_sec > 0.0 {
                *left = left.saturating_sub(Duration::from_secs_f64(test_sec));
            }
        }
        mark_downstream(
            receipt,
            &package.plan.package,
            if evidence.phases.contains("compiled") {
                "passed"
            } else if exit.timed_out {
                "timed out"
            } else if exit.code != 0 {
                "failed"
            } else {
                "incomplete"
            },
            &evidence.phases,
        );
        let signalled = entry.worker_signal.is_some();
        subsets.extend(evidence.subsets.clone());
        receipt.package(entry);
        emit_progress(plan, receipt, progress)?;
        if signalled {
            receipt.value["failure_class"] = json!("infrastructure_interrupted");
            return Ok(1);
        }
        if exit.timed_out || exit.code < 0 || exit.code >= 128 {
            receipt.value["failure_class"] = json!(if exit.code < 0 {
                "infrastructure_interrupted"
            } else {
                "scoped_test_timeout"
            });
            return Ok(if exit.timed_out { 124 } else { exit.code });
        }
    }
    receipt.value["unlisted_packages"] = json!(
        plan.packages
            .iter()
            .filter(|p| !listed.contains(&p.plan.package))
            .map(|p| p.plan.package.clone())
            .collect::<Vec<_>>()
    );
    let coverage_plan = plan
        .packages
        .iter()
        .map(|p| (p.plan.package.clone(), p.plan.selections.clone()))
        .collect();
    let mut counts = BTreeMap::<String, (usize, usize)>::new();
    for subset in &subsets {
        if let Some(filters) = subset["filters"].as_object() {
            for (filter, value) in filters {
                let count = counts.entry(filter.clone()).or_default();
                count.0 += value[0].as_u64().unwrap_or(0) as usize;
                count.1 += value[1].as_u64().unwrap_or(0) as usize;
            }
        }
    }
    let mut kept = BTreeMap::<String, [usize; 3]>::new();
    for subset in &subsets {
        for (filter, value) in subset["bounded"].as_object().into_iter().flatten() {
            let total = kept.entry(filter.clone()).or_default();
            for (slot, count) in total.iter_mut().zip(value.as_array().into_iter().flatten()) {
                *slot += count.as_u64().unwrap_or(0) as usize;
            }
        }
    }
    let deferred_tests = subsets
        .iter()
        .map(|s| s["deferred_tests"].as_u64().unwrap_or(0) as usize)
        .sum();
    let coverage =
        super::selection::coverage(&coverage_plan, &listed, &counts, &kept, deferred_tests);
    record_deferred(receipt, &coverage);
    receipt.value["selected_filters"] = json!(coverage.selected);
    receipt.value["run_filters"] = json!(coverage.run.len());
    receipt.value["filters_partial"] = json!(coverage.partial);
    receipt.value["filters_not_run"] = json!(coverage.not_run);
    receipt.value["selected_tests"] = json!(
        subsets
            .iter()
            .filter_map(|s| s["selected"].as_u64())
            .sum::<u64>()
    );
    receipt.value["run_tests"] = json!(
        subsets
            .iter()
            .filter_map(|s| s["executed"].as_u64())
            .sum::<u64>()
    );
    let unmatched: Vec<_> = plan
        .filters
        .iter()
        .filter(|filter| !matched.contains(*filter))
        .cloned()
        .collect();
    failed |= !unmatched.is_empty();
    receipt.value["unmatched_filters"] = json!(unmatched);
    let warnings = backend.warnings(&warning_payloads)?;
    failed |= warnings["status"]
        .as_str()
        .is_none_or(|s| !["ok", "not_measured", "not_applicable"].contains(&s));
    receipt.value["warnings"] = warnings;
    if let Some(linux) = backend.linux_check()? {
        failed |= linux["status"] == "failed";
        receipt.value["linux_check"] = linux;
    }
    let failures: Vec<_> = failures.into_iter().collect();
    if failures.is_empty() {
        receipt.classify(&[], Some(&BTreeSet::new()));
        receipt.value["baseline_comparison"] = json!({"status":"not_needed"});
    } else {
        let comparison = backend.compare_baseline(&failures)?;
        receipt.classify(
            &failures,
            comparison.complete.then_some(&comparison.failures),
        );
        receipt.value["baseline_comparison"] = comparison.detail;
        for (key, value) in comparison.receipt {
            receipt.value[key] = value;
        }
        if comparison.complete {
            let reds = failures
                .iter()
                .filter(|f| comparison.failures.contains(*f))
                .cloned()
                .collect::<Vec<_>>();
            let mut diagnostics = Vec::new();
            let annotations = super::failure_snapshot::annotate(&plan.repo, &reds, |line| {
                diagnostics.push(line.to_owned())
            });
            receipt.value["baseline_failure_records"] = json!(annotations.records);
            receipt.value["baseline_reds_without_record"] = json!(annotations.without_record);
            receipt.value["baseline_diagnostics"] = json!(diagnostics);
        }
        // A harness/build/list interruption is never established as baseline by
        // merely finding a similarly named failure elsewhere.
        if !comparison.complete {
            failed = true;
        }
    }
    let merge = backend.merge_check()?;
    if let Some(fields) = merge.as_object() {
        for (key, value) in fields {
            receipt.value[key] = value.clone();
        }
    }
    backend.verify_source()?;
    receipt.value["source_verified"] = json!(true);
    emit_progress(plan, receipt, progress)?;
    Ok(if failed { 1 } else { 0 })
}

/// Fold the executed listing counts into the receipt's `deferred` block (exact
/// identities, never a pass). Without a bounded plan there is nothing deferred.
fn record_deferred(receipt: &mut Receipt, coverage: &super::selection::Coverage) {
    if coverage.bounded.is_empty() {
        return;
    }
    let bounded: Vec<Value> = coverage.bounded.iter().take(20).cloned().collect();
    let basis = if coverage.unlisted_packages.is_empty() {
        "every selected package listed".to_string()
    } else {
        format!(
            "incomplete: {} never listed",
            coverage.unlisted_packages.join(", ")
        )
    };
    if let Some(deferred) = receipt
        .value
        .get_mut("deferred")
        .and_then(Value::as_object_mut)
    {
        deferred.insert("tests".into(), json!(coverage.deferred_tests));
        deferred.insert(
            "tests_basis".into(),
            json!(format!(
                "listed test identities no plan filter, changed-module rank or direct-observer rank kept; {basis}"
            )),
        );
        deferred.insert("bounded_coverage".into(), json!(bounded));
        deferred.insert(
            "bounded_coverage_total".into(),
            json!(coverage.bounded.len()),
        );
    }
    receipt.value["filters_bounded"] = json!(bounded);
    receipt.value["filters_bounded_total"] = json!(coverage.bounded.len());
    receipt.value["deferred_tests"] = json!(coverage.deferred_tests);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deferred_counts_replace_pending_once_tests_are_listed() {
        let coverage = super::super::selection::Coverage {
            bounded: vec![json!({"filter":"rsi=b","kept":4,"executed":3,"deferred":9})],
            deferred_tests: 9,
            unlisted_packages: vec!["rsid".into()],
            ..Default::default()
        };
        let mut receipt = Receipt::new("base", "head", json!({}));
        receipt.value["deferred"] = json!({"tests": null, "status": "pending", "tests_basis": "pending: counted from the package test listings when the run lists them"});
        record_deferred(&mut receipt, &coverage);
        let deferred = &receipt.value["deferred"];
        assert_eq!(deferred["tests"], 9);
        assert_eq!(deferred["status"], "pending", "listing is never a pass");
        assert!(
            deferred["tests_basis"]
                .as_str()
                .unwrap()
                .ends_with("incomplete: rsid never listed")
        );
        assert_eq!(deferred["bounded_coverage_total"], 1);
        assert_eq!(receipt.value["deferred_tests"], 9);
        assert_eq!(receipt.value["filters_bounded"][0]["filter"], "rsi=b");
        // Nothing bounded: the block is left exactly as planned.
        let mut plain = Receipt::new("base", "head", json!({}));
        plain.value["deferred"] = json!({"tests": null});
        record_deferred(&mut plain, &super::super::selection::Coverage::default());
        assert_eq!(plain.value["deferred"], json!({"tests": null}));
    }
    struct Fake {
        calls: Vec<&'static str>,
        residual: Vec<Value>,
        desktop: Vec<Value>,
        omit_guard: bool,
    }
    impl Backend for Fake {
        fn pid_isolation(&mut self) -> io::Result<Isolation> {
            Ok(Isolation::unavailable("not_linux"))
        }
        fn verify_source(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn static_guards(&mut self) -> io::Result<BTreeMap<String, Value>> {
            self.calls.push("static");
            Ok(REQUIRED_GUARDS
                .iter()
                .filter(|name| !self.omit_guard || **name != "store_boundary")
                .map(|name| ((*name).into(), json!({"status":"ok"})))
                .collect())
        }
        fn residual(&mut self, _: bool, isolation: &Isolation) -> io::Result<Vec<Value>> {
            assert_eq!(isolation.status().reason, "not_linux");
            self.calls.push("residual");
            Ok(self.residual.clone())
        }
        fn desktop(&mut self, _: bool, isolation: &Isolation) -> io::Result<Vec<Value>> {
            assert_eq!(isolation.status().reason, "not_linux");
            self.calls.push("desktop");
            Ok(self.desktop.clone())
        }
        fn admit(&mut self, _: Duration) -> io::Result<()> {
            self.calls.push("admit");
            Ok(())
        }
        fn package(
            &mut self,
            _: &PackagePlan,
            _: Duration,
            _: Option<Duration>,
            _: &Isolation,
            _: &mut dyn FnMut(&str),
        ) -> io::Result<WorkerExit> {
            panic!("fixture selected no Cargo")
        }
        fn warnings(&mut self, _: &[Value]) -> io::Result<Value> {
            Ok(json!({"status":"not_measured"}))
        }
        fn compare_baseline(&mut self, _: &[Failure]) -> io::Result<BaselineComparison> {
            panic!("fixture had no failures")
        }
        fn linux_check(&mut self) -> io::Result<Option<Value>> {
            Ok(None)
        }
        fn merge_check(&mut self) -> io::Result<Value> {
            Ok(json!({"merge_clean":true}))
        }
        fn release(&mut self) -> io::Result<()> {
            self.calls.push("release");
            Ok(())
        }
    }
    fn plan(log_dir: &std::path::Path) -> RunPlan {
        RunPlan {
            repo: PathBuf::from("."),
            log_dir: log_dir.into(),
            base: "base".into(),
            head: "head".into(),
            tool: json!({}),
            filters: vec![],
            downstream_compile: json!({}),
            downstream_tests: json!({}),
            packages: vec![],
            runtime_max: Duration::from_secs(30),
            total_max: Duration::from_secs(60),
            admission_max: Duration::from_secs(10),
            test_time_box: None,
            dry_run: false,
            receipt_fields: Default::default(),
        }
    }
    #[test]
    fn static_and_residual_fail_fast_preserve_required_order_and_cleanup() {
        let mut fake = Fake {
            calls: vec![],
            residual: vec![json!({"status":"runner_unavailable"})],
            desktop: vec![],
            omit_guard: false,
        };
        let dir = tempfile::tempdir().unwrap();
        let receipt = run(&plan(dir.path()), &mut fake, |_| {});
        assert_eq!(receipt["ok"], false);
        assert_eq!(receipt["pid_isolation"], "unavailable");
        assert_eq!(receipt["pid_isolation_reason"], "not_linux");
        assert_eq!(fake.calls, vec!["static", "residual", "release"]);
        fake.calls.clear();
        fake.omit_guard = true;
        run(&plan(dir.path()), &mut fake, |_| {});
        assert_eq!(fake.calls, vec!["static", "release"]);
    }
    #[test]
    fn desktop_red_does_not_erase_other_evidence_and_dry_run_is_never_acceptance() {
        let mut fake = Fake {
            calls: vec![],
            residual: vec![],
            desktop: vec![json!({"status":"failed","executed_tests":2})],
            omit_guard: false,
        };
        let dir = tempfile::tempdir().unwrap();
        let receipt = run(&plan(dir.path()), &mut fake, |_| {});
        assert_eq!(receipt["ok"], false);
        assert_eq!(receipt["desktop"][0]["executed_tests"], 2);
        assert_eq!(fake.calls, vec!["static", "residual", "desktop", "release"]);
        fake.desktop = vec![];
        let mut plan = plan(dir.path());
        plan.dry_run = true;
        assert_eq!(run(&plan, &mut fake, |_| {})["ok"], false);
    }
}
