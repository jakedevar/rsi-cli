//! The native scoped-test command: resolve and pin the source, derive and plan
//! the run (through `Contracts`), build the `RunPlan`/`HostPlan`, run the
//! orchestration on the production `Host` and print the typed receipt. Every
//! pre-run refusal is a red receipt: no sibling gap can become an empty lane.
use super::{
    contracts::{Contracts, Derivation, Incomplete, Renumbered},
    execution::PackagePlan,
    gitx::Git,
    guards,
    host::{Host, HostPlan, LinuxLane},
    linux_check, options,
    options::Options,
    orchestration::{self, PlannedPackage, RunPlan},
    persistence, planner, process,
    provenance::SourcePin,
    provision::{self, ScratchWorktree},
    receipt::Receipt,
    scratch,
    selection::{self, Plan, Selection},
    warnings::Warnings,
};
use crate::{Ctx, scratch_git::Policy};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicUsize};
use std::time::{Duration, Instant};

pub const RECEIPT_PREFIX: &str = "SCOPED_TEST_RECEIPT ";
const RENUMBER_ROLLING: &str = "origin/rolling";

/// A pre-run refusal: nothing ran, the receipt is red.
pub struct Refusal {
    pub code: i32,
    pub class: &'static str,
    pub diagnostic: String,
    pub fields: Map<String, Value>,
}
impl Refusal {
    fn new(code: i32, class: &'static str, diagnostic: impl Into<String>) -> Self {
        Self {
            code,
            class,
            diagnostic: diagnostic.into(),
            fields: Map::new(),
        }
    }
    fn io(error: io::Error) -> Self {
        let text = error.to_string();
        let class = if text.starts_with("job_source_moved") {
            "job_source_moved"
        } else if text.starts_with("incomplete_contract") {
            "incomplete_contract"
        } else {
            "runner_unavailable"
        };
        Self::new(2, class, text)
    }
    fn contract(incomplete: &Incomplete) -> Self {
        let mut refusal = Self::new(2, "incomplete_contract", incomplete.error().to_string());
        refusal
            .fields
            .insert("incomplete".into(), incomplete.value());
        refusal
    }
}
impl From<io::Error> for Refusal {
    fn from(error: io::Error) -> Self {
        Self::io(error)
    }
}
impl From<Incomplete> for Refusal {
    fn from(incomplete: Incomplete) -> Self {
        Self::contract(&incomplete)
    }
}

pub struct Built {
    pub run: RunPlan,
    pub host: HostPlan,
    pub pin: SourcePin,
    pub work_pin: Option<SourcePin>,
    pub candidate: Option<ScratchWorktree>,
    pub messages: Vec<String>,
}

fn tool_copy(ctx: &Ctx) -> Value {
    let executable = std::env::current_exe().ok();
    json!({
        "runner": "rust",
        "path": executable,
        "source": ctx.var("RSI_SCOPED_TEST_TOOL_SOURCE").unwrap_or("worktree"),
        "rev": ctx.var("RSI_SCOPED_TEST_TOOL_REV"),
        "source_stamp": env!("RSI_REPO_RUNNER_SOURCE_STAMP"),
    })
}

fn which(ctx: &Ctx, name: &str) -> Option<PathBuf> {
    std::env::split_paths(ctx.var("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// `(runner, nextest version)`: nextest when installed and new enough, else libtest.
fn detect_runner(
    ctx: &Ctx,
    repo: &Path,
    requested: &str,
    cancel: &AtomicUsize,
) -> Result<(String, Option<String>), String> {
    let requested = ctx.var("RSI_SCOPED_TEST_RUNNER").unwrap_or(requested);
    if requested == "libtest" {
        return Ok(("libtest".into(), None));
    }
    let mut version = None;
    if let Some(binary) = which(ctx, "cargo-nextest") {
        let env = process::build_env(ctx);
        let mut text = String::new();
        let outcome = process::stream(
            &[
                binary.to_string_lossy().into_owned(),
                "nextest".into(),
                "--version".into(),
            ],
            repo,
            &env,
            Instant::now() + Duration::from_secs(30),
            cancel,
            |line| text.push_str(line),
            |_| {},
        );
        if outcome.is_ok_and(|o| o.code == 0) {
            version = text
                .strip_prefix("cargo-nextest ")
                .and_then(|rest| rest.split_whitespace().next())
                .map(str::to_owned);
        }
    }
    let required = planner::nextest_required(
        &std::fs::read_to_string(repo.join(".config/nextest.toml")).unwrap_or_default(),
    );
    if let Some(version) = version
        && planner::version_tuple(&version) >= required
    {
        return Ok(("nextest".into(), Some(version)));
    }
    if requested == "nextest" {
        return Err(
            "--runner nextest: cargo-nextest is missing or older than .config/nextest.toml requires"
                .into(),
        );
    }
    Ok(("libtest".into(), None))
}

/// `Some((source, target))` when `head` declares a provisional migration that
/// `rolling` does not contain yet (the lander renumbers such a source, #3339).
pub fn declares_provisional_migration(
    git: &Git,
    head: &str,
    rolling: &str,
) -> io::Result<Option<(String, String)>> {
    let (Some(source), Some(target)) = (git.commit(head)?, git.commit(rolling)?) else {
        return Ok(None);
    };
    if git
        .builtin(&["merge-base", "--is-ancestor", &source, &target], &[])?
        .ok()
    {
        return Ok(None);
    }
    let base = git.builtin(&["merge-base", &source, &target], &[])?;
    if !base.ok() || base.first_line().is_empty() {
        return Ok(None);
    }
    let added = git.builtin(
        &[
            "diff",
            "--name-only",
            "--diff-filter=A",
            "--no-renames",
            base.first_line(),
            &source,
            "--",
            "tools/provisional-migrations/",
        ],
        &[],
    )?;
    Ok(
        (added.ok() && added.stdout.lines().any(|name| name.ends_with(".json")))
            .then_some((source, target)),
    )
}

fn items(derived: &Value, key: &str) -> Vec<String> {
    derived[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}
fn dedupe(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    values
        .into_iter()
        .filter(|v| seen.insert(v.clone()))
        .collect()
}

fn cargo_prefix(ctx: &Ctx, options: &Options) -> (Option<PathBuf>, Vec<String>) {
    let unix = !cfg!(windows);
    if let Some(over) = ctx.var("CHECK_TOUCHED_SHARDS_CARGO") {
        let words: Vec<String> = over.split_whitespace().map(str::to_owned).collect();
        return match words.first() {
            Some(first)
                if Path::new(first)
                    .file_name()
                    .is_some_and(|n| n == "cargo-slot") =>
            {
                (Some(PathBuf::from(first)), words[1..].to_vec())
            }
            _ => (None, words),
        };
    }
    let held = ctx
        .var("RSI_CARGO_SLOT_HELD")
        .is_some_and(|v| !v.is_empty());
    let slot = if held {
        None
    } else if let Some(given) = &options.cargo_slot {
        Path::new(given)
            .file_name()
            .is_some_and(|n| n == "cargo-slot")
            .then(|| PathBuf::from(given))
    } else {
        let installed = ctx.home().join(".rsi/bin/cargo-slot");
        installed.exists().then_some(installed)
    };
    let cargo = if unix {
        vec![
            "env".into(),
            "-u".into(),
            "RSI_PROCESS_OWNERSHIP_NAMESPACE".into(),
            "cargo".into(),
        ]
    } else {
        vec!["cargo".into()]
    };
    (slot, cargo)
}

/// Packages in the order their first filter appears (the retained dict order).
fn insertion_order(plan: Plan, filters: &[String], priority: &[String]) -> planner::Ordered {
    let order = dedupe(filters.iter().chain(priority).cloned());
    let rank = |s: &Selection| {
        order
            .iter()
            .position(|f| *f == s.filter)
            .unwrap_or(usize::MAX)
    };
    let mut items: planner::Ordered = plan.into_iter().collect();
    items.sort_by_key(|(package, selections)| {
        (
            selections.iter().map(rank).min().unwrap_or(usize::MAX),
            package.clone(),
        )
    });
    items
}

/// Derive, plan and pin one run.
pub fn build<C: Contracts>(
    options: &Options,
    ctx: &Ctx,
    contracts: &mut C,
    cancel: &Arc<AtomicUsize>,
) -> Result<Built, Refusal> {
    let started = Instant::now();
    let mut messages: Vec<String> = Vec::new();
    let mut fields = Map::new();
    let repo = ctx.path(&options.repo).canonicalize()?;
    let env = process::build_env(ctx);
    let vars: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let repo_ctx = Ctx::new(&repo, &vars);
    let git = Git {
        repo: &repo,
        env: &env,
        cancel,
    };
    let (slot, cargo) = cargo_prefix(ctx, options);

    // The wrapper starts before sandbox/artifact locks; execution starts here.
    let slot_wait = options
        .slot_wait_max
        .unwrap_or_else(|| 600f64.max(options.job_max.unwrap_or(0.0)));
    let mut job_limit = None;
    if let Some(job_max) = options.job_max {
        let startup = ctx
            .var("RSI_JOB_STARTED_AT")
            .and_then(|v| v.parse::<f64>().ok())
            .and_then(|at| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|now| (now.as_secs_f64() - at).max(0.0))
            })
            .unwrap_or(0.0);
        job_limit = Some(Duration::from_secs_f64((job_max - 60.0).max(1.0)));
        fields.insert(
            "admission".into(),
            json!({"count": 0, "startup_wait_sec": (startup * 1000.0).round() / 1000.0,
                   "waited_sec": (startup * 1000.0).round() / 1000.0, "excluded_from_execution": true}),
        );
    }
    let (tmpfs, unavailable) = match scratch::choose(ctx) {
        Ok(parent) => (parent, None),
        Err(error) => (None, Some(error.to_string())),
    };
    if tmpfs.is_none() && options.tmpfs {
        // `--tmpfs` is a parity-only request: with no tmpfs the legacy runner reported
        // it unavailable and skipped, exiting 0. The exit stays 0 and the run is skipped,
        // but the receipt is never ok (nothing ran), so no gate can read it as evidence.
        let reason = unavailable
            .unwrap_or_else(|| format!("no usable tmpfs scratch on {}", std::env::consts::OS));
        let mut refusal = Refusal::new(
            0,
            "tmpfs_unavailable",
            format!("{reason}; lander tmpfs parity was not checked and no test ran"),
        );
        refusal
            .fields
            .insert("scratch_mode".into(), json!("unavailable"));
        refusal.fields.insert("scratch_parent".into(), Value::Null);
        refusal
            .fields
            .insert("parity_unavailable".into(), json!(reason));
        refusal.fields.insert("skipped".into(), json!(true));
        return Err(refusal);
    }
    if let Some(reason) = &unavailable {
        fields.insert("parity_unavailable".into(), json!(reason));
    }
    fields.insert(
        "scratch_mode".into(),
        json!(if tmpfs.is_some() { "tmpfs" } else { "host" }),
    );
    fields.insert("scratch_parent".into(), json!(tmpfs));
    messages.push(match &tmpfs {
        Some(parent) => format!("scoped-test: selected tests use tmpfs TMPDIR under {}", parent.display()),
        None => "TMPFS_PARITY unavailable: no tmpfs scratch; running selected tests with the host TMPDIR".into(),
    });

    let pin = SourcePin::capture_for(&repo_ctx, &options.base, &options.head, options.dry_run)?;
    let mut head = pin.head.clone();
    let source_head = pin.head.clone();
    let explicit = options.filters.is_some();
    let mut derived = Value::Object(Map::new());
    let mut work = repo.clone();
    let mut work_pin: Option<SourcePin> = None;
    let mut candidate = None;
    let policy = Policy::new(
        &repo,
        &std::env::current_dir()?,
        env.iter().map(|(k, v)| (k.into(), v.into())).collect(),
        &[],
    )?;
    let mut derive_failed: Option<(Derivation, Vec<String>)> = None;

    let filters: Vec<String>;
    let mut bounded: Option<Value> = None;
    if let Some(explicit_filters) = &options.filters {
        filters = explicit_filters.clone();
        messages.push(format!(
            "explicit filters: head={head} repo={}",
            repo.display()
        ));
    } else {
        // A candidate whose declared provisional migration number was taken on
        // rolling is renumbered the way the lander does, then gated (#3339).
        if declares_provisional_migration(&git, &head, RENUMBER_ROLLING)?.is_some() {
            match contracts.renumber(&repo, &head, RENUMBER_ROLLING)? {
                Renumbered::Absent => (),
                Renumbered::Done {
                    source,
                    target,
                    old_version,
                    new_version,
                    candidate: renumbered,
                } => {
                    fields.insert(
                        "renumber".into(),
                        json!({"status":"renumbered","source":source,"target":target,
                               "old_version":old_version,"new_version":new_version,"candidate":renumbered}),
                    );
                    messages.push(format!(
                        "scoped-test: declared provisional migration v{old_version} was taken on {}; testing the lander's renumbered candidate {} (v{new_version}) of {}",
                        options.base,
                        &renumbered[..12.min(renumbered.len())],
                        &source[..12.min(source.len())]
                    ));
                    let scratch = ScratchWorktree::create(
                        &git,
                        &policy,
                        "rsi-renumbered-candidate-",
                        &renumbered,
                    )?;
                    let scratch_pin = SourcePin {
                        repo: scratch.path().to_path_buf(),
                        head: renumbered.clone(),
                        base: pin.base.clone(),
                        inspect_only: false,
                    };
                    scratch_pin.verify(&env)?;
                    work = scratch.path().to_path_buf();
                    head = renumbered;
                    work_pin = Some(scratch_pin);
                    candidate = Some(scratch);
                }
                Renumbered::Refused { source, reason } => {
                    let mut refusal = Refusal::new(
                        2,
                        "provisional_renumber_refused",
                        format!("provisional migration renumber refused: {reason}"),
                    );
                    refusal.fields.insert(
                        "renumber".into(),
                        json!({"status":"refused","reason":reason}),
                    );
                    refusal.fields.insert("head".into(), json!(source));
                    return Err(refusal);
                }
            }
        }
        let derive_base = provision::integrated_base(&git, &pin.base, &head, RENUMBER_ROLLING);
        if derive_base != pin.base {
            fields.insert("integrated_base".into(), json!(derive_base));
            messages.push(format!(
                "scoped-test: launch pin {} integrated with rolling; deriving the worker-owned delta from {derive_base}",
                options.base
            ));
        }
        let derivation = contracts.derive(&work, &derive_base, &head)?;
        derived = derivation.data.clone();
        if let Some(extra) = planner::broad_observer_fields(&derived) {
            for (key, value) in extra.as_object().into_iter().flatten() {
                fields.insert(key.clone(), value.clone());
            }
            messages.push(format!(
                "scoped-test: uncertain: broad_observers ({} filters, {} tests); the bounded subset below is not the whole observer set, the merge-queue gate and the full-library QA cover the rest",
                fields["broad_filter_count"], fields["broad_tests"]
            ));
        }
        if derivation.exit_code != 0 {
            let reasons = planner::derive_failure_reasons(&derived, derivation.exit_code);
            messages.push(format!(
                "scoped-test: derivation failed: {}",
                reasons.join("; ")
            ));
            derive_failed = Some((derivation, reasons));
            filters = vec![];
        } else {
            head = derived["head"].as_str().map_or(head.clone(), str::to_owned);
            let mut merged = items(&derived, "suggested_filters");
            if derived["unfiltered_leaf_paths"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
            {
                merged.extend(items(&derived, "worker_filters"));
                merged.extend(items(&derived, "scanned_token_filters"));
            }
            bounded = planner::bounded_worker_selection(&derived);
            if let Some(bounded) = &bounded {
                let mut front: Vec<String> = bounded["filters"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect();
                front.extend(merged);
                merged = front;
            }
            filters = dedupe(merged);
        }
    }

    let guard_base = derived["merge_base"]
        .as_str()
        .map_or_else(|| pin.base.clone(), str::to_owned);
    if derive_failed.is_some() {
        // Evidence first: the static gates still ran when the derivation failed.
        return Err(derive_refusal(
            options,
            &pin,
            &head,
            &guard_base,
            &derived,
            &work,
            &env,
            cancel,
            contracts,
            derive_failed,
        ));
    }
    if !options.dry_run && head != pin.head && work_pin.is_none() {
        return Err(Refusal::new(
            2,
            "job_source_moved",
            "--head differs from this worktree's HEAD; use --dry-run to inspect another candidate",
        ));
    }

    let slow = planner::slow_tier_decision(&derived, explicit);
    let mut slow_tier = planner::slow_tier_entry(&slow);
    let mut slow_tests: BTreeSet<String> = BTreeSet::new();
    if slow_tier["mode"] == "excluded" {
        match contracts.load_slow_tier(&work, &head) {
            Ok(Some(config)) => {
                slow_tier["package"] = json!(config.package);
                slow_tier["tests"] = json!(config.tests.len());
                slow_tests = config.tests;
            }
            Ok(None) => {
                slow_tier["mode"] = json!("full");
                slow_tier["reason"] = json!("no_slow_tier_config");
            }
            // A wired loader that cannot read the config answers with the full tier
            // (never a silent skip), as the legacy runner did; an unwired contract
            // (no detail) still refuses.
            Err(Incomplete {
                detail: Some(detail),
                ..
            }) => {
                slow_tier["mode"] = json!("full");
                slow_tier["reason"] = json!(format!("config_unreadable: {detail}"));
            }
            Err(incomplete) => return Err(incomplete.into()),
        }
    }
    messages.push(format!(
        "scoped-test: {}",
        planner::slow_tier_summary(&slow_tier)
    ));
    let priority_filters = items(&derived, "priority_filters");
    let changed_modules: Vec<Value> = derived["changed_modules"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m.is_object())
        .cloned()
        .collect();
    let changed_packages: BTreeSet<String> = changed_modules
        .iter()
        .filter_map(|m| m["package"].as_str().map(str::to_owned))
        .collect();
    let shards = contracts.shard_packages(&work)?;
    let mut plan: Plan = selection::selection_plan(&filters, &shards, &priority_filters)
        .map_err(|e| Refusal::new(2, "invalid_filter", e))?;
    let mut direct_plan = Plan::new();
    let direct_items: Vec<&Value> = derived["direct_observer_filters"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["filter"].as_str().is_some_and(|f| !f.is_empty()))
        .collect();
    if !direct_items.is_empty() {
        let distances: BTreeMap<String, usize> = direct_items
            .iter()
            .filter_map(|i| {
                Some((
                    i["filter"].as_str()?.to_string(),
                    i["distance"].as_u64().filter(|d| *d > 0).unwrap_or(1) as usize,
                ))
            })
            .collect();
        let names: Vec<String> = distances.keys().cloned().collect();
        direct_plan = selection::selection_plan(&names, &shards, &[])
            .map_err(|e| Refusal::new(2, "invalid_filter", e))?;
        for selections in direct_plan.values_mut() {
            for selection in selections {
                selection.distance = distances[&selection.filter];
            }
        }
        let summary = &derived["direct_observers"];
        fields.insert(
            "direct_observers".into(),
            json!({"filters": direct_items.len(), "tests": summary["tests"], "by_distance": summary["by_distance"]}),
        );
        messages.push(format!(
            "scoped-test: direct observers of the stopped broad traces run nearest first: {} filters",
            direct_items.len()
        ));
    }
    let downstream = match derived.get("downstream_tests").filter(|v| v.is_object()) {
        Some(value) => value.clone(),
        None => contracts.downstream_test_plan(&work, &guard_base, &head)?,
    };
    let mut downstream_compile = downstream.clone();
    let downstream_tests = json!({
        "items": downstream.get("items").cloned().unwrap_or_else(|| json!([])),
        "selections": downstream.get("selections").cloned().unwrap_or_else(|| json!([])),
        "filters": downstream.get("filters").cloned().unwrap_or_else(|| json!([])),
        "notes": downstream.get("notes").cloned().unwrap_or_else(|| json!([])),
    });
    let downstream_order: Vec<String> = downstream["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["package"].as_str().map(str::to_owned))
        .collect();

    let log_root = policy.tempdir("rsi-scoped-test-")?.keep();
    messages.push(format!("logs: {}", log_root.display()));
    if let Some(bounded) = &bounded {
        let marked: BTreeSet<&str> = bounded["bounded"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        for selections in plan.values_mut() {
            for selection in selections {
                if marked.contains(selection.filter.as_str()) {
                    selection.bounded = true;
                }
            }
        }
        planner::with_direct_observers(&mut plan, &direct_plan);
        fields.insert("worker_policy".into(), json!("bounded_deferred"));
        fields.insert(
            "deferred".into(),
            planner::deferred_record(bounded, &head, &derived["direct_observers"]),
        );
        fields.insert(
            "worker_broad_filters".into(),
            json!(bounded["broad_filters_left"].as_array().map_or(0, Vec::len)),
        );
    }
    let direct_packages: BTreeSet<String> = direct_plan.keys().cloned().collect();
    let mut ordered = if priority_filters.is_empty() {
        insertion_order(plan.clone(), &filters, &priority_filters)
    } else {
        messages.push(format!(
            "scoped-test: added/changed tests run first: {}",
            priority_filters.join(" ")
        ));
        planner::package_order(plan.clone(), &changed_packages, &direct_packages)
    };

    let (host_threads, load1) = planner::host_capacity();
    let adaptive = planner::adaptive_threads(host_threads, load1);
    let linux = cfg!(target_os = "linux");
    let mut cpu_quota = options.cpu_quota;
    if let Some(budget) = slice_budget_percent(ctx, linux) {
        if budget < cpu_quota {
            messages.push(format!(
                "scoped-test: --cpu-quota {cpu_quota:.0}% exceeds the build slice budget {budget:.0}%; using the budget"
            ));
            cpu_quota = cpu_quota.min(budget.max(100.0));
        }
    }
    let quota_cores = ((cpu_quota / 100.0) as usize).max(1);
    let test_shards = options
        .test_shards
        .unwrap_or_else(|| adaptive.min(quota_cores));
    let unit_quota = if options.test_shards.is_some() {
        cpu_quota
    } else {
        cpu_quota.min(test_shards as f64 * 100.0)
    };
    let (runner, nextest_version) = detect_runner(ctx, &repo, &options.runner, cancel)
        .map_err(|e| Refusal::new(2, "runner_unavailable", e))?;
    fields.insert("runner".into(), json!(runner));
    fields.insert("nextest_version".into(), json!(nextest_version));
    for (key, value) in [
        ("test_threads", json!(test_shards)),
        ("test_shards", json!(test_shards)),
        ("cpu_quota", json!(unit_quota)),
        ("job_cpu_quota", json!(cpu_quota)),
        ("host_threads", json!(host_threads)),
        ("load1", json!(load1)),
        ("adaptive_threads", json!(adaptive)),
    ] {
        fields.insert(key.into(), value);
    }
    let over_budget =
        options.filter_budget > 0 && planner::plan_filter_count(&ordered) > options.filter_budget;
    let uncertain = fields.get("uncertain").is_some();
    let boxed = over_budget || (options.filter_budget > 0 && uncertain);
    let time_box = (boxed && options.test_time_box > 0.0)
        .then(|| Duration::from_secs_f64(options.test_time_box));
    let mut plan_kind = "precise";
    if boxed {
        plan_kind = "targeted_subset";
        ordered = planner::package_order(
            ordered.into_iter().collect(),
            &changed_packages,
            &direct_packages,
        );
        fields.insert("filter_budget".into(), json!(options.filter_budget));
        fields.insert(
            "test_time_box_sec".into(),
            json!(time_box.map(|t| t.as_secs_f64())),
        );
        fields.insert(
            "selected_filters".into(),
            json!(planner::plan_filter_count(&ordered)),
        );
        fields.insert("run_filters".into(), Value::Null);
        fields.insert("gate_covers_rest".into(), json!(true));
        messages.push(format!(
            "scoped-test: targeted subset: {} filters; running them as named, most specific first, within {}; the merge-queue gate covers the rest",
            planner::plan_filter_count(&ordered),
            match time_box {
                Some(t) => format!("a {:.0}s test time box", t.as_secs_f64()),
                None => "no time box".into(),
            }
        ));
    }
    fields.insert("plan".into(), json!(plan_kind));
    fields.insert(
        "compile".into(),
        json!({"planning": {"source": if explicit {"explicit_filters"} else {"filters_only"}, "cargo_invoked": false},
               "execution": if options.dry_run {"skipped_dry_run"} else {"pending"}, "packages": {}}),
    );
    fields.insert("slow_tier".into(), slow_tier.clone());
    fields.insert("priority_filters".into(), json!(priority_filters));
    if !changed_modules.is_empty() {
        fields.insert(
            "changed_modules".into(),
            json!(
                changed_modules
                    .iter()
                    .map(|item| Value::Object(
                        ["package", "module", "kind", "target", "path", "tests"]
                            .iter()
                            .filter_map(|k| item.get(*k).map(|v| ((*k).to_string(), v.clone())))
                            .collect()
                    ))
                    .collect::<Vec<_>>()
            ),
        );
    }

    // Package plans: selected packages first, then compile-only downstream ones.
    let downstream_set: BTreeSet<String> = downstream_order.iter().cloned().collect();
    let mut packages_order: Vec<(String, Vec<Selection>)> = ordered;
    for package in &downstream_order {
        if !packages_order.iter().any(|(name, _)| name == package) {
            packages_order.push((package.clone(), vec![]));
        }
    }
    let e2e_list = std::fs::read_to_string(work.join("scripts/e2e-prebuild-bins.txt")).ok();
    let mut planned = Vec::new();
    let mut feature_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut compile_features = Map::new();
    for (package, selections) in &packages_order {
        let compile_only = downstream_set.contains(package) && selections.is_empty();
        let in_downstream = downstream_set.contains(package);
        let feature_selections: Vec<Selection> = if in_downstream {
            vec![Selection {
                kind: "lib".into(),
                ..Selection::default()
            }]
        } else {
            let mut all = selections.clone();
            all.extend(direct_plan.get(package).cloned().unwrap_or_default());
            all
        };
        let inventory = contracts.feature_test_inventory(&work, &head, package)?;
        let features = planner::required_features(&inventory, &feature_selections)
            .map_err(|e| Refusal::new(2, "invalid_filter", e))?;
        compile_features.insert(package.clone(), json!(features));
        feature_map.insert(package.clone(), features.clone());
        let build = selection::build_command(&cargo, package, selections, &features, in_downstream);
        let e2e = if planner::selects_e2e(selections) {
            let text = e2e_list.as_deref().ok_or_else(|| {
                Refusal::new(
                    2,
                    "runner_unavailable",
                    "scripts/e2e-prebuild-bins.txt is required to prebuild rsid for e2e_tui",
                )
            })?;
            let prebuild = planner::e2e_prebuild_command(&cargo, text)
                .map_err(|e| Refusal::new(2, "runner_unavailable", e))?;
            Some(json!({"prebuild": prebuild}))
        } else {
            None
        };
        if in_downstream {
            for entry in downstream_compile["packages"]
                .as_array_mut()
                .into_iter()
                .flatten()
            {
                if entry["package"] == package.as_str() {
                    entry["command"] = json!(build);
                    entry["features"] = json!(features);
                    entry["execution"] = json!(if compile_only {
                        "compile_only"
                    } else {
                        "selected_tests_also_run"
                    });
                    entry["status"] = json!(if options.dry_run {
                        "planned"
                    } else {
                        "pending"
                    });
                }
            }
        }
        planned.push(PlannedPackage {
            plan: PackagePlan {
                repo: work.clone(),
                package: package.clone(),
                head: head.clone(),
                build,
                selections: selections.clone(),
                compile_only,
                features,
                exact_tests: None,
                changed_modules: changed_modules
                    .iter()
                    .filter(|m| m["package"] == package.as_str())
                    .cloned()
                    .collect(),
                direct_selections: direct_plan.get(package).cloned().unwrap_or_default(),
                slow_tier_excluded: if slow_tier["package"] == package.as_str() {
                    slow_tests.clone()
                } else {
                    BTreeSet::new()
                },
                time_box_sec: time_box.map(|t| t.as_secs_f64()),
                tmpfs: tmpfs.clone(),
                e2e,
                runner: Some(runner.clone()),
                nextest: (runner == "nextest").then(|| {
                    let mut command = cargo.clone();
                    command.push("nextest".into());
                    command
                }),
                test_shards,
                runtime_max_sec: options.runtime_max,
                tool_repo: Some(repo.clone()),
            },
            downstream_compile: in_downstream,
        });
    }
    fields.insert("features".into(), Value::Object(compile_features));
    if planned.is_empty() {
        messages.push("scoped-test: no test filters derived; no Cargo packages selected".into());
    }

    // The Linux compile lane for non-Linux hosts (#253), decided now so a dry run shows it.
    let mut lane = None;
    if !linux {
        let lane_packages = (|| -> io::Result<Vec<String>> {
            let changed = git.builtin(
                &[
                    "diff",
                    "--name-only",
                    "--no-renames",
                    &format!("{guard_base}..{head}"),
                ],
                &[],
            )?;
            let root = git.builtin(&["show", &format!("{head}:Cargo.toml")], &[])?;
            let manifest_of = |member: &str| {
                git.builtin(&["show", &format!("{head}:{member}/Cargo.toml")], &[])
                    .ok()
                    .filter(|out| out.ok())
                    .map(|out| out.stdout)
            };
            Ok(planner::linux_lane_packages(
                &changed.stdout,
                root.ok().then_some(root.stdout.as_str()),
                manifest_of,
                &downstream_set,
            ))
        })();
        let toolchain = linux_check::discover(&repo_ctx);
        let mut entry = match lane_packages {
            Ok(packages) => {
                let installed = if packages.is_empty() {
                    Some(BTreeSet::new())
                } else {
                    linux_check::installed_targets(&repo_ctx, cancel)
                };
                linux_check::choice(false, &packages, installed.as_ref(), toolchain.as_ref())
            }
            Err(error) => Some(
                json!({"target": linux_check::TARGET, "packages": [], "status": "not_covered",
                "reason": "package_selection_failed", "first_error": error.to_string().chars().take(200).collect::<String>()}),
            ),
        };
        if let Some(entry) = entry.as_mut() {
            if options.dry_run && entry["status"] == "pending" {
                let packages: Vec<String> = items(entry, "packages");
                entry["status"] = json!("planned");
                entry["command"] = json!(linux_check::command(&cargo, &packages, &feature_map));
            }
            // A run that stops before the lane never claims it ran.
            let mut shown = entry.clone();
            if shown["status"] == "pending" {
                shown["status"] = json!("not_covered");
                shown["reason"] = json!("not_reached");
            }
            fields.insert("linux_check".into(), shown);
            messages.push(format!(
                "scoped-test: linux_check lane ({}, after the native lanes): {}",
                linux_check::TARGET,
                entry["status"].as_str().unwrap_or("?")
            ));
        }
        lane = entry.map(|entry| LinuxLane { entry, toolchain });
    }

    let runtime_max = Duration::from_secs_f64(options.runtime_max);
    let total = match (options.total_max.map(Duration::from_secs_f64), job_limit) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let planned_count = planned.len();
    let host = HostPlan {
        repo: repo.clone(),
        work: work.clone(),
        base: pin.base.clone(),
        guard_base: guard_base.clone(),
        head: head.clone(),
        source_head: source_head.clone(),
        log_dir: log_root.clone(),
        audits: derived["audits"].clone(),
        slot,
        cargo: cargo.clone(),
        unit_quota,
        tmpfs: tmpfs.clone(),
        e2e_prebuild: e2e_list
            .as_deref()
            .and_then(|text| planner::e2e_prebuild_command(&cargo, text).ok()),
        slot_wait_max: Duration::from_secs_f64(slot_wait),
        total_max: total,
        runtime_max,
        job_deadline: job_limit.map(|limit| started + limit),
        started,
        linux: lane,
        features: feature_map,
        reclaim_incremental: options.reclaim_incremental,
        package_count: planned_count,
    };
    let run = RunPlan {
        repo: repo.clone(),
        log_dir: log_root,
        base: options.base.clone(),
        head: source_head,
        tool: tool_copy(ctx),
        filters: filters.clone(),
        downstream_compile,
        downstream_tests,
        packages: planned,
        runtime_max,
        total_max: total.unwrap_or(Duration::from_secs(u32::MAX as u64)),
        admission_max: Duration::from_secs_f64(slot_wait),
        test_time_box: time_box,
        dry_run: options.dry_run,
        receipt_fields: fields,
    };
    Ok(Built {
        run,
        host,
        pin,
        work_pin,
        candidate,
        messages,
    })
}

/// A failed derivation keeps its evidence: the static gates run and the receipt
/// names why, with the derivation's own output replayed.
#[allow(clippy::too_many_arguments)]
fn derive_refusal<C: Contracts>(
    options: &Options,
    pin: &SourcePin,
    head: &str,
    guard_base: &str,
    derived: &Value,
    work: &Path,
    env: &BTreeMap<String, String>,
    cancel: &Arc<AtomicUsize>,
    contracts: &mut C,
    failed: Option<(Derivation, Vec<String>)>,
) -> Refusal {
    let (derivation, reasons) = failed.unwrap_or_else(|| {
        (
            Derivation {
                exit_code: 2,
                data: Value::Null,
                stdout: String::new(),
            },
            vec![],
        )
    });
    let git = Git {
        repo: work,
        env,
        cancel,
    };
    let inputs = guards::Inputs {
        repo: work,
        base: guard_base,
        head,
        audits: &derived["audits"],
        environment: env,
    };
    let static_guards = guards::run(&inputs, &git, contracts, |_| {});
    let mut refusal = Refusal::new(
        derivation.exit_code.max(1),
        "derive_failed",
        reasons.join("; "),
    );
    refusal
        .fields
        .insert("derive_failure".into(), json!(reasons));
    refusal
        .fields
        .insert("static_guards".into(), guards::to_object(&static_guards));
    refusal.fields.insert("head".into(), json!(pin.head));
    refusal
        .fields
        .insert("derivation_output".into(), json!(derivation.stdout));
    let _ = options;
    refusal
}

/// The build slice's live `CPUQuota` in percent of one CPU, or None.
fn slice_budget_percent(ctx: &Ctx, linux: bool) -> Option<f64> {
    let name = ctx
        .var("RSI_BUILD_SLICE")
        .unwrap_or(super::host::BUILD_SLICE);
    if !linux || name.is_empty() {
        return None;
    }
    let env = process::build_env(ctx);
    let mut text = String::new();
    let outcome = process::stream(
        &[
            "systemctl".into(),
            "--user".into(),
            "show".into(),
            name.into(),
            "-p".into(),
            "CPUQuotaPerSecUSec".into(),
            "--value".into(),
        ],
        &ctx.cwd,
        &{
            let mut launch = env;
            for key in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
                if let Some(value) = ctx.var(key) {
                    launch.insert(key.into(), value.into());
                }
            }
            launch
        },
        Instant::now() + Duration::from_secs(10),
        &AtomicUsize::new(0),
        |line| text.push_str(line),
        |_| {},
    )
    .ok()?;
    if outcome.code != 0 {
        return None;
    }
    parse_quota_percent(text.trim())
}
pub fn parse_quota_percent(text: &str) -> Option<f64> {
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let seconds = match unit {
        "" | "s" => 1.0,
        "ms" => 1e-3,
        "us" => 1e-6,
        "min" => 60.0,
        _ => return None,
    };
    number.parse::<f64>().ok().map(|n| n * seconds * 100.0)
}

/// `--warning-baseline-from LOG`: the `packages` document a finished job log measured.
pub fn warning_baseline_from(log: &str) -> Value {
    let mut merged = Warnings::default();
    for line in log.lines() {
        if let Some(payload) = line.strip_prefix("SCOPED_WARNINGS ") {
            match serde_json::from_str::<Warnings>(payload) {
                Ok(part) => merged.merge(part),
                Err(_) => merged
                    .degraded
                    .push("a SCOPED_WARNINGS line was not valid JSON".into()),
            }
        }
    }
    json!({"schema": 1, "packages": merged.measured.iter().map(|(name, configs)| {
        (name.clone(), json!({
            "configs": configs,
            "warnings": merged.warnings.get(name).map(|keys| keys.iter().map(|(key, places)| (key.clone(), places.len())).collect::<BTreeMap<_, _>>()).unwrap_or_default(),
        }))
    }).collect::<BTreeMap<_, _>>()})
}

fn print_receipt(out: &mut dyn Write, value: &Value) {
    let _ = writeln!(
        out,
        "{RECEIPT_PREFIX}{}",
        crate::wire_json::legacy_json(value, true)
    );
    let _ = out.flush();
}

fn refuse(
    out: &mut dyn Write,
    err: &mut dyn Write,
    options: &Options,
    ctx: &Ctx,
    refusal: Refusal,
) -> i32 {
    let _ = writeln!(
        err,
        "scoped-test: {}: {}",
        refusal.class, refusal.diagnostic
    );
    let head = refusal
        .fields
        .get("head")
        .and_then(Value::as_str)
        .unwrap_or(&options.head)
        .to_string();
    let mut receipt = Receipt::new(&options.base, &head, tool_copy(ctx));
    receipt.value["dry_run"] = json!(options.dry_run);
    for (key, value) in &refusal.fields {
        receipt.value[key] = value.clone();
    }
    receipt.value["failure_class"] = json!(refusal.class);
    receipt.value["diagnostic"] = json!(refusal.diagnostic);
    receipt.value["dependencies"] = json!(super::dependency_state(&ctx.cwd));
    let value = persistence::wire(&receipt.finish(refusal.code), false);
    print_receipt(out, &value);
    refusal.code
}

/// The native scoped-test. Sibling contracts are supplied by the caller; the
/// staged binary supplies `Staged`, so every unfinished contract is red.
pub fn run<C: Contracts>(
    args: &[String],
    ctx: &Ctx,
    out: &mut dyn Write,
    err: &mut dyn Write,
    mut contracts: C,
) -> i32 {
    let options = match options::parse(args, ctx.var("RSI_SCOPED_TEST_FILTERS"), err) {
        Ok(options) => options,
        Err(code) => return code,
    };
    if let Some(log) = &options.warning_baseline_from {
        return match std::fs::read_to_string(log) {
            Ok(text) => {
                let _ = writeln!(
                    out,
                    "{}",
                    serde_json::to_string_pretty(&warning_baseline_from(&text)).unwrap_or_default()
                );
                0
            }
            Err(error) => {
                let _ = writeln!(err, "scoped-test: {log}: {error}");
                2
            }
        };
    }
    let cancel = match process::install_signals() {
        Ok(cancel) => cancel,
        Err(error) => return refuse(out, err, &options, ctx, error.into()),
    };
    let built = match build(&options, ctx, &mut contracts, &cancel) {
        Ok(built) => built,
        Err(refusal) => return refuse(out, err, &options, ctx, refusal),
    };
    let Built {
        run,
        host,
        pin,
        work_pin,
        candidate,
        messages,
    } = built;
    for line in &messages {
        let _ = writeln!(out, "{line}");
    }
    let runner = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => return refuse(out, err, &options, ctx, error.into()),
    };
    let mut backend = Host::new(
        ctx,
        contracts,
        cancel,
        host,
        runner,
        pin,
        work_pin,
        candidate,
        options.dry_run,
    );
    let receipt = orchestration::run(&run, &mut backend, |progress| print_receipt(out, progress));
    print_receipt(out, &receipt);
    receipt["exit_code"].as_i64().map_or(2, |code| code as i32)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::scoped_runner::contracts::Staged;
    use crate::testutil;

    #[test]
    fn quota_and_baseline_log_parsing() {
        assert_eq!(parse_quota_percent("2s"), Some(200.0));
        assert_eq!(parse_quota_percent("1500ms"), Some(150.0));
        assert_eq!(parse_quota_percent("1.5min"), Some(9000.0));
        assert_eq!(parse_quota_percent("infinity"), None);
        assert_eq!(parse_quota_percent(""), None);
        let log = "noise\nSCOPED_WARNINGS {\"measured\":{\"pkg\":[\"lib\"]},\"warnings\":{\"pkg\":{\"k|1|m\":[\"a\",\"b\"]}},\"unattributed\":0,\"degraded\":[]}\n";
        assert_eq!(
            warning_baseline_from(log),
            json!({"schema":1,"packages":{"pkg":{"configs":["lib"],"warnings":{"k|1|m":2}}}})
        );
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        testutil::git(dir.path(), &["init", "-q", "-b", "main"]);
        testutil::write(dir.path(), "a.txt", "one\n");
        testutil::git(dir.path(), &["add", "a.txt"]);
        testutil::git(
            dir.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "one",
            ],
        );
        dir
    }

    #[test]
    fn staged_contracts_make_every_run_a_red_incomplete_contract_receipt() {
        let dir = repo();
        let ctx = Ctx::new(
            dir.path(),
            &[
                ("PATH", &std::env::var("PATH").unwrap()),
                ("HOME", &std::env::var("HOME").unwrap()),
            ],
        );
        for args in [
            vec!["--dry-run".to_string(), "--base".into(), "HEAD".into()],
            vec![
                "--filter".into(),
                "rsi=test(=one)".into(),
                "--base".into(),
                "HEAD".into(),
            ],
        ] {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = run(&args, &ctx, &mut out, &mut err, Staged);
            assert_eq!(code, 2, "{}", String::from_utf8_lossy(&err));
            let text = String::from_utf8(out).unwrap();
            let line = text
                .lines()
                .rev()
                .find(|l| l.starts_with(RECEIPT_PREFIX))
                .unwrap();
            let receipt: Value = serde_json::from_str(&line[RECEIPT_PREFIX.len()..]).unwrap();
            assert_eq!(receipt["ok"], false);
            assert_eq!(receipt["failure_class"], "incomplete_contract");
            assert_eq!(receipt["incomplete"]["status"], "incomplete_contract");
            assert!(
                receipt["dependencies"]
                    .as_array()
                    .is_some_and(|d| !d.is_empty())
            );
        }
    }

    fn receipt_of(out: Vec<u8>) -> Value {
        let text = String::from_utf8(out).unwrap();
        let line = text
            .lines()
            .rev()
            .find(|l| l.starts_with(RECEIPT_PREFIX))
            .unwrap();
        serde_json::from_str(&line[RECEIPT_PREFIX.len()..]).unwrap()
    }

    #[test]
    fn a_parity_only_tmpfs_request_without_a_tmpfs_skips_with_exit_zero_and_a_never_ok_receipt() {
        let dir = repo();
        let ctx = Ctx::new(
            dir.path(),
            &[
                ("PATH", &std::env::var("PATH").unwrap()),
                ("HOME", &std::env::var("HOME").unwrap()),
                ("CHECK_TOUCHED_SHARDS_TMPDIR", "/nonexistent-rsi-tmpfs"),
            ],
        );
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(
            &["--tmpfs".into(), "--base".into(), "HEAD".into()],
            &ctx,
            &mut out,
            &mut err,
            Staged,
        );
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
        let receipt = receipt_of(out);
        assert_eq!(receipt["exit_code"], 0);
        assert_eq!(receipt["ok"], false);
        assert_eq!(receipt["failure_class"], "tmpfs_unavailable");
        assert_eq!(receipt["scratch_mode"], "unavailable");
        assert_eq!(receipt["skipped"], true);
        assert!(
            receipt["parity_unavailable"]
                .as_str()
                .is_some_and(|r| !r.is_empty())
        );
        // Without --tmpfs the same host runs on (here: reaches the unfinished contract).
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(
            &["--base".into(), "HEAD".into(), "--dry-run".into()],
            &ctx,
            &mut out,
            &mut err,
            Staged,
        );
        assert_eq!(code, 2);
        assert_eq!(receipt_of(out)["failure_class"], "incomplete_contract");
    }

    #[test]
    fn a_dry_run_may_inspect_another_head_but_a_real_run_refuses_it() {
        let dir = repo();
        let first = testutil::git(dir.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_string();
        testutil::git(
            dir.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-qm",
                "two",
            ],
        );
        let ctx = Ctx::new(
            dir.path(),
            &[
                ("PATH", &std::env::var("PATH").unwrap()),
                ("HOME", &std::env::var("HOME").unwrap()),
            ],
        );
        let args = |extra: &[&str]| -> Vec<String> {
            ["--base", "HEAD", "--head", &first]
                .iter()
                .chain(extra)
                .map(|s| (*s).to_string())
                .collect()
        };
        // Planning reaches the unfinished derivation contract instead of a source-move refusal.
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&args(&["--dry-run"]), &ctx, &mut out, &mut err, Staged);
        assert_eq!(code, 2);
        let receipt = receipt_of(out);
        assert_eq!(receipt["failure_class"], "incomplete_contract");
        assert_eq!(receipt["dry_run"], true);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&args(&[]), &ctx, &mut out, &mut err, Staged);
        assert_eq!(code, 2);
        assert_eq!(receipt_of(out)["failure_class"], "job_source_moved");
    }

    #[test]
    fn dirty_or_unresolvable_sources_are_refused_before_any_lane() {
        let dir = repo();
        let ctx = Ctx::new(
            dir.path(),
            &[
                ("PATH", &std::env::var("PATH").unwrap()),
                ("HOME", &std::env::var("HOME").unwrap()),
            ],
        );
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(
            run(
                &["--base".into(), "no-such-ref".into()],
                &ctx,
                &mut out,
                &mut err,
                Staged
            ),
            2
        );
        testutil::write(dir.path(), "dirty.txt", "x\n");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(
            run(
                &["--base".into(), "HEAD".into()],
                &ctx,
                &mut out,
                &mut err,
                Staged
            ),
            2
        );
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("\"failure_class\":\"job_source_moved\""),
            "{text}"
        );
    }

    #[test]
    fn provisional_migration_declaration_is_detected_from_the_rolling_delta_only() {
        let dir = repo();
        let env = super::super::gitx::test_environment();
        let cancel = AtomicUsize::new(0);
        let git = Git {
            repo: dir.path(),
            env: &env,
            cancel: &cancel,
        };
        testutil::git(dir.path(), &["branch", "rolling"]);
        testutil::git(dir.path(), &["checkout", "-q", "-b", "work"]);
        testutil::write(dir.path(), "tools/provisional-migrations/v9.json", "{}\n");
        testutil::git(dir.path(), &["add", "-A"]);
        testutil::git(
            dir.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "declare",
            ],
        );
        testutil::git(dir.path(), &["checkout", "-q", "rolling"]);
        testutil::write(dir.path(), "r.txt", "r\n");
        testutil::git(dir.path(), &["add", "r.txt"]);
        testutil::git(
            dir.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "advance",
            ],
        );
        assert!(
            declares_provisional_migration(&git, "work", "rolling")
                .unwrap()
                .is_some()
        );
        // An already-landed source and a plain source declare nothing.
        assert!(
            declares_provisional_migration(&git, "rolling", "rolling")
                .unwrap()
                .is_none()
        );
        testutil::git(dir.path(), &["branch", "plain", "main"]);
        assert!(
            declares_provisional_migration(&git, "plain", "rolling")
                .unwrap()
                .is_none()
        );
        assert!(
            declares_provisional_migration(&git, "missing", "rolling")
                .unwrap()
                .is_none()
        );
    }
}
