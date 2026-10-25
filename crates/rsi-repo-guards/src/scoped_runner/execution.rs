//! Staged package worker. Builds once, lists every target, then executes exact
//! binary-scoped identities. Missing worker features refuse before building.
use super::{
    batch, process,
    selection::{self, Rank, Selection},
    warnings::Warnings,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

type Names = BTreeSet<String>;
#[derive(Debug, Deserialize, Serialize)]
pub struct PackagePlan {
    pub repo: PathBuf,
    pub package: String,
    pub head: String,
    pub build: Vec<String>,
    pub selections: Vec<Selection>,
    #[serde(default)]
    pub compile_only: bool,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub exact_tests: Option<Names>,
    #[serde(default)]
    pub changed_modules: Vec<Value>,
    #[serde(default)]
    pub direct_selections: Vec<Selection>,
    #[serde(default)]
    pub slow_tier_excluded: Names,
    #[serde(default)]
    pub time_box_sec: Option<f64>,
    #[serde(default)]
    pub tmpfs: Option<PathBuf>,
    #[serde(default)]
    pub e2e: Option<Value>,
    #[serde(default)]
    pub runner: Option<String>,
    #[serde(default)]
    pub nextest: Option<Vec<String>>,
    #[serde(default = "one")]
    pub test_shards: usize,
    #[serde(default = "budget")]
    pub runtime_max_sec: f64,
    /// The checkout this runner binary was built from. A baseline replay runs
    /// on a different revision, so the compiled source stamp is verified here
    /// while the source pin still covers `repo` and `head`.
    #[serde(default)]
    pub tool_repo: Option<PathBuf>,
}
fn one() -> usize {
    1
}
fn budget() -> f64 {
    1800.0
}

#[derive(Debug)]
struct Artifact {
    binary: PathBuf,
    directory: PathBuf,
    target: String,
    kinds: Vec<String>,
}
struct Record {
    artifact: Artifact,
    selected: Names,
    by_filter: BTreeMap<String, Names>,
    bounded: BTreeMap<String, (Names, Names)>,
    deferred: Names,
    changed: Names,
    direct: Names,
    ran: Names,
    tiers: BTreeMap<Rank, Vec<String>>,
}
fn listing(lines: &str) -> Names {
    lines
        .lines()
        .filter_map(|line| {
            line.trim_end_matches('\r')
                .strip_suffix(": test")
                .map(Into::into)
        })
        .collect()
}
fn changed_ranks(names: &Names, records: &[Value], artifact: &Artifact) -> BTreeMap<String, usize> {
    let mut ranks = BTreeMap::<String, usize>::new();
    for record in records {
        let members: Names = if let Some(module) = record["module"].as_str() {
            if !artifact.kinds.iter().any(|k| k == "lib") {
                continue;
            }
            let prefix = format!("{module}::");
            let excluded: Vec<_> = record["excluded"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|s| format!("{s}::"))
                .collect();
            names
                .iter()
                .filter(|n| n.starts_with(&prefix) && !excluded.iter().any(|e| n.starts_with(e)))
                .cloned()
                .collect()
        } else if record["target"] == artifact.target
            && record["kind"]
                .as_str()
                .is_some_and(|kind| artifact.kinds.iter().any(|k| k == kind))
        {
            names.clone()
        } else {
            continue;
        };
        if members.is_empty() || members.len() > 300 {
            continue;
        }
        let size = members.len();
        for name in members {
            ranks
                .entry(name)
                .and_modify(|rank| *rank = (*rank).min(size))
                .or_insert(size);
        }
    }
    ranks
}
fn collected(
    command: &[String],
    plan: &PackagePlan,
    env: &BTreeMap<String, String>,
    deadline: Instant,
    cancel: &AtomicUsize,
) -> io::Result<(process::Outcome, String)> {
    let mut text = String::new();
    let outcome = process::stream(
        command,
        &plan.repo,
        env,
        deadline,
        cancel,
        |line| text.push_str(line),
        |_| {},
    )?;
    Ok((outcome, text))
}
fn verify_head(
    plan: &PackagePlan,
    env: &BTreeMap<String, String>,
    deadline: Instant,
    cancel: &AtomicUsize,
) -> io::Result<()> {
    let command = vec!["git".into(), "rev-parse".into(), "HEAD".into()];
    let (result, text) = collected(&command, plan, env, deadline, cancel)?;
    if result.code != 0 || text.trim() != plan.head {
        return Err(io::Error::other(
            "job_source_moved: package HEAD differs from exact plan head",
        ));
    }
    Ok(())
}

#[derive(Default)]
pub(super) struct Progress {
    pub(super) active: Option<String>,
    pub(super) done: Names,
    pub(super) failures: Names,
    summary: bool,
    pub(super) executed: usize,
}
impl Progress {
    pub(super) fn partial(&mut self, tail: &str, names: &Names, emit: &mut impl FnMut(&str)) {
        if let Some((name, _)) = tail
            .strip_prefix("test ")
            .and_then(|s| s.split_once(" ... "))
        {
            if names.contains(name)
                && !self.done.contains(name)
                && self.active.as_deref() != Some(name)
            {
                self.active = Some(name.into());
                emit(&format!("SCOPED_START {name}\n"));
            }
        }
    }
    pub(super) fn line(&mut self, line: &str, names: &Names, emit: &mut impl FnMut(&str)) {
        if let Some(rest) = line.strip_prefix("test ") {
            if let Some((name, verdict)) = rest.split_once(" ... ") {
                if names.contains(name) {
                    if !self.done.contains(name) && self.active.as_deref() != Some(name) {
                        if let Some(previous) = self.active.take() {
                            self.done.insert(previous.clone());
                            emit(&format!("SCOPED_END {previous}\n"));
                        }
                        self.active = Some(name.into());
                        emit(&format!("SCOPED_START {name}\n"));
                    }
                    if ["ok", "FAILED", "ignored"].contains(&verdict.trim()) {
                        if verdict.trim() == "FAILED" {
                            self.failures.insert(name.into());
                        }
                        if verdict.trim() != "ignored" && self.done.insert(name.into()) {
                            emit(&format!("SCOPED_END {name}\n"));
                        }
                        self.active = None;
                    }
                } else if verdict.trim() == "FAILED" {
                    self.failures.insert(name.into());
                }
            }
        }
        if line.trim() == "FAILED" {
            if let Some(active) = &self.active {
                self.failures.insert(active.clone());
            }
        }
        if line.trim() == "failures:" {
            self.summary = true;
        } else if self.summary && line.starts_with("    ") && names.contains(line.trim()) {
            self.failures.insert(line.trim().into());
        }
        if line.starts_with("test result: ") {
            self.summary = false;
            if let Some(name) = self.active.take() {
                if self.done.insert(name.clone()) {
                    emit(&format!("SCOPED_END {name}\n"));
                }
            }
            let re = crate::re::Re::new(r"(\d+) passed; (\d+) failed;");
            if let Some(captures) = re.search(line) {
                self.executed = captures
                    .get(1)
                    .and_then(|m| m.as_str().parse::<usize>().ok())
                    .unwrap_or(0)
                    + captures
                        .get(2)
                        .and_then(|m| m.as_str().parse::<usize>().ok())
                        .unwrap_or(0);
            }
        }
        emit(line);
    }
}

/// Package execution is deliberately not an acceptance receipt. The parent
/// must still run static/residual/desktop, baseline and warning gates.
pub fn execute(
    plan: &PackagePlan,
    env: &BTreeMap<String, String>,
    cancel: &AtomicUsize,
    mut emit: impl FnMut(&str),
) -> io::Result<i32> {
    if !plan.runtime_max_sec.is_finite() || plan.runtime_max_sec <= 0.0 || plan.test_shards == 0 {
        return Err(io::Error::other("invalid worker budget"));
    }
    if plan
        .runner
        .as_deref()
        .is_some_and(|r| !["libtest", "nextest"].contains(&r))
        || (plan.runner.as_deref() == Some("nextest")
            && plan.nextest.as_ref().is_none_or(Vec::is_empty))
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "selected test runner is unavailable",
        ));
    }
    let deadline = Instant::now() + Duration::from_secs_f64(plan.runtime_max_sec);
    verify_head(plan, env, deadline, cancel)?;
    let mut warnings = Warnings::new(env);
    let mut artifacts = BTreeMap::<String, Artifact>::new();
    let result = process::stream(
        &plan.build,
        &plan.repo,
        env,
        deadline,
        cancel,
        |line| {
            if let Some(value) = warnings.observe(&plan.repo.to_string_lossy(), line) {
                if value["reason"] == "compiler-artifact" && value["profile"]["test"] == true {
                    if let Some(binary) = value["executable"].as_str() {
                        let directory = value["manifest_path"]
                            .as_str()
                            .and_then(|p| Path::new(p).parent())
                            .unwrap_or(&plan.repo)
                            .to_owned();
                        artifacts.insert(
                            binary.into(),
                            Artifact {
                                binary: binary.into(),
                                directory,
                                target: value["target"]["name"].as_str().unwrap_or("").into(),
                                kinds: value["target"]["kind"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .filter_map(Value::as_str)
                                    .map(Into::into)
                                    .collect(),
                            },
                        );
                    }
                }
                if value["reason"] == "compiler-message" {
                    if let Some(rendered) = value["message"]["rendered"].as_str() {
                        emit(rendered);
                    }
                } else if value["reason"] != "compiler-artifact" {
                    emit(line);
                }
            } else {
                emit(line);
            }
        },
        |_| {},
    )?;
    if !result.output_complete {
        warnings
            .degraded
            .push("build output stream did not close".into());
    }
    emit(&format!(
        "SCOPED_WARNINGS {}\n",
        serde_json::to_string(&warnings)?
    ));
    if result.code != 0 {
        emit("SCOPED_PHASE build_failed\n");
        return Ok(result.code);
    }
    verify_head(plan, env, deadline, cancel)?;
    emit("SCOPED_PHASE compiled\n");
    if plan.compile_only {
        emit(&format!("SCOPED_COMPLETE {} 0\n", plan.package));
        return Ok(0);
    }
    let mut test_env = env.clone();
    if let Some(e2e) = &plan.e2e {
        test_env.insert("RSI_E2E".into(), "1".into());
        let prebuild: Vec<String> =
            serde_json::from_value(e2e["prebuild"].clone()).map_err(io::Error::other)?;
        emit("scoped-test: e2e_tui selected; RSI_E2E=1 and building rsid for the harness\n");
        let result = process::stream(
            &prebuild,
            &plan.repo,
            &test_env,
            deadline,
            cancel,
            &mut emit,
            |_| {},
        )?;
        if result.code != 0 || !result.output_complete {
            emit("SCOPED_PHASE build_failed\n");
            return Ok(if result.code == 0 { 1 } else { result.code });
        }
    }
    let _scratch = if let Some(parent) = &plan.tmpfs {
        let scratch = super::scratch::Scratch::tmpfs(parent)?;
        test_env.extend(scratch.environment());
        emit(&format!("TMPFS_PARITY tmpfs={}\n", scratch.path.display()));
        Some(scratch)
    } else {
        None
    };
    if artifacts.is_empty() {
        emit("SCOPED_PHASE build_failed\n");
        return Ok(1);
    }
    let test_started = Instant::now();
    let test_deadline = plan
        .time_box_sec
        .filter(|s| s.is_finite() && *s > 0.0)
        .map(|s| test_started + Duration::from_secs_f64(s));
    let mut records = Vec::new();
    let mut ignored_only = Names::new();
    let mut slow_skipped = Names::new();
    for artifact in artifacts.into_values() {
        let selections: Vec<_> = plan
            .selections
            .iter()
            .filter(|s| s.for_target(&artifact.kinds, &artifact.target))
            .collect();
        if selections.is_empty() {
            continue;
        }
        let command = vec![
            artifact.binary.to_string_lossy().into_owned(),
            "--list".into(),
            "--format=terse".into(),
        ];
        let mut listing_plan = PackagePlan {
            repo: artifact.directory.clone(),
            ..clone_plan(plan)
        };
        let (listed, text) = collected(&command, &listing_plan, &test_env, deadline, cancel)?;
        if listed.code != 0 || !listed.output_complete {
            emit(&text);
            emit("SCOPED_PHASE list_failed\n");
            return Ok(if listed.code == 0 { 1 } else { listed.code });
        }
        let names = listing(&text);
        let ignored_command = vec![
            artifact.binary.to_string_lossy().into_owned(),
            "--list".into(),
            "--ignored".into(),
            "--format=terse".into(),
        ];
        let (ignored_result, text) =
            collected(&ignored_command, &listing_plan, &test_env, deadline, cancel)?;
        if ignored_result.code != 0 || !ignored_result.output_complete {
            emit("SCOPED_PHASE list_failed\n");
            return Ok(1);
        }
        let ignored = listing(&text);
        let mut selected = Names::new();
        let mut sizes = BTreeMap::<String, usize>::new();
        let mut by_filter = BTreeMap::<String, Names>::new();
        let mut priority = Names::new();
        let mut bounded_matches = BTreeMap::<String, Names>::new();
        for selection in selections {
            let mut matches = Names::new();
            for name in &names {
                if selection.matches(name).map_err(io::Error::other)?
                    && plan
                        .exact_tests
                        .as_ref()
                        .is_none_or(|exact| exact.contains(name))
                {
                    matches.insert(name.clone());
                }
            }
            if !selection.direct_only && !matches.is_empty() {
                emit(&format!("SCOPED_MATCH {}\n", selection.filter));
            }
            if !matches.is_empty() && matches.is_subset(&ignored) {
                if !selection.direct_only {
                    ignored_only.insert(selection.filter.clone());
                }
                continue;
            }
            matches.retain(|name| !ignored.contains(name));
            if selection.kind == "lib" {
                slow_skipped.extend(matches.intersection(&plan.slow_tier_excluded).cloned());
                matches.retain(|name| !plan.slow_tier_excluded.contains(name));
            }
            if selection.bounded {
                bounded_matches
                    .entry(selection.filter.clone())
                    .or_default()
                    .extend(matches);
                continue;
            }
            if !selection.direct_only {
                by_filter
                    .entry(selection.filter.clone())
                    .or_default()
                    .extend(matches.clone());
                if selection.priority {
                    priority.extend(matches.clone());
                }
            }
            let size = if selection.direct_only {
                1
            } else {
                matches.len()
            };
            for name in matches {
                sizes
                    .entry(name.clone())
                    .and_modify(|n| *n = (*n).min(size))
                    .or_insert(size);
                selected.insert(name);
            }
        }
        let mut universe = selected.clone();
        for matches in bounded_matches.values() {
            universe.extend(matches.clone());
        }
        let mut changed = changed_ranks(&universe, &plan.changed_modules, &artifact);
        let mut direct = BTreeMap::<String, usize>::new();
        for selection in plan
            .direct_selections
            .iter()
            .filter(|s| s.for_target(&artifact.kinds, &artifact.target))
        {
            for name in &universe {
                if selection.matches(name).map_err(io::Error::other)? {
                    direct
                        .entry(name.clone())
                        .and_modify(|d| *d = (*d).min(selection.distance))
                        .or_insert(selection.distance);
                }
            }
        }
        let mut bounded = BTreeMap::new();
        for (filter, matches) in &bounded_matches {
            let kept: Names = matches
                .iter()
                .filter(|n| changed.contains_key(*n) || direct.contains_key(*n))
                .cloned()
                .collect();
            for name in &kept {
                sizes
                    .entry(name.clone())
                    .and_modify(|n| *n = (*n).min(matches.len()))
                    .or_insert(matches.len());
            }
            selected.extend(kept.clone());
            bounded.insert(filter.clone(), (kept, Names::new()));
        }
        let mut deferred = Names::new();
        for (filter, matches) in bounded_matches {
            let omitted: Names = matches.difference(&selected).cloned().collect();
            deferred.extend(omitted.clone());
            if let Some((_, rest)) = bounded.get_mut(&filter) {
                *rest = omitted;
            }
        }
        changed.retain(|n, _| selected.contains(n));
        direct.retain(|n, _| selected.contains(n));
        let tiers = selection::ranked_tiers(&selected, &sizes, &priority, &changed, &direct);
        listing_plan.selections.clear();
        records.push(Record {
            artifact,
            selected,
            by_filter,
            bounded,
            deferred,
            changed: changed.into_keys().collect(),
            direct: direct.into_keys().collect(),
            ran: Names::new(),
            tiers,
        });
    }
    emit(&format!("SCOPED_SELECTION {}\n",json!(records.iter().map(|r|json!({"binary":r.artifact.binary,"selected":r.selected,"by_filter":r.by_filter,"changed":r.changed,"direct":r.direct,"bounded":r.bounded,"deferred":r.deferred,"ran":r.ran})).collect::<Vec<_>>())));
    let mut queue: Vec<_> = records
        .iter()
        .enumerate()
        .flat_map(|(i, r)| {
            r.tiers
                .iter()
                .map(move |(key, names)| (*key, i, names.clone()))
        })
        .collect();
    queue.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    let mut expired = false;
    let mut failed = false;
    let mut failures = Names::new();
    for (rank, index, names) in queue {
        let core = !rank.1;
        let record = &mut records[index];
        emit(&format!(
            "SCOPED_TARGET {}\n",
            record.artifact.binary.display()
        ));
        let result = batch::run(
            &batch::BatchConfig {
                binary: &record.artifact.binary,
                directory: &record.artifact.directory,
                package: &plan.package,
                kinds: &record.artifact.kinds,
                target: &record.artifact.target,
                nextest: if plan.runner.as_deref() == Some("nextest") {
                    plan.nextest.as_deref()
                } else {
                    None
                },
                features: &plan.features,
                threads: plan.test_shards,
                env: &test_env,
                deadline,
                box_deadline: if core { None } else { test_deadline },
                cancel,
            },
            &names,
            &mut emit,
        )?;
        record.ran.extend(result.ran);
        failures.extend(result.failures);
        expired |= result.expired;
        failed |= result.failed;
        if !result.unfinished.is_empty() {
            emit(&format!(
                "SCOPED_UNFINISHED {}\n",
                json!({"package":plan.package,"binary":record.artifact.binary,"tests":result.unfinished})
            ));
        }
        if let Some(code) = result.interrupted {
            return Ok(code);
        }
    }

    verify_head(plan, env, deadline, cancel)?;
    let count: usize = records.iter().map(|r| r.ran.len()).sum();
    emit(&format!("SCOPED_EXECUTED {} {count}\n", plan.package));
    let mut filters = BTreeMap::<String, (usize, usize)>::new();
    let mut bounded = BTreeMap::<String, (usize, usize, usize)>::new();
    for record in &records {
        for (filter, names) in &record.by_filter {
            let counts = filters.entry(filter.clone()).or_default();
            counts.0 += names.len();
            counts.1 += names.intersection(&record.ran).count();
        }
        for (filter, (kept, deferred)) in &record.bounded {
            let counts = bounded.entry(filter.clone()).or_default();
            counts.0 += kept.len();
            counts.1 += kept.intersection(&record.ran).count();
            counts.2 += deferred.len();
        }
    }
    emit(&format!(
        "SCOPED_SUBSET {}\n",
        json!({"package":plan.package,"selected":records.iter().map(|r|r.selected.len()).sum::<usize>(),"executed":count,"box_expired":expired,"filters":filters,"bounded":bounded,"deferred_tests":records.iter().map(|r|r.deferred.len()).sum::<usize>(),"test_sec":test_started.elapsed().as_secs_f64()})
    ));
    for name in ignored_only {
        emit(&format!("SCOPED_IGNORED_ONLY {name}\n"));
    }
    if !slow_skipped.is_empty() {
        emit(&format!(
            "SCOPED_SLOW_TIER_SKIPPED {}\n",
            slow_skipped.len()
        ));
    }
    emit("SCOPED_PHASE listed\n");
    if failed || !failures.is_empty() {
        emit("SCOPED_PHASE test_failed\n");
        Ok(1)
    } else {
        emit(&format!("SCOPED_COMPLETE {} {count}\n", plan.package));
        Ok(0)
    }
}
fn clone_plan(plan: &PackagePlan) -> PackagePlan {
    // Keep serialized child plans as the protocol, so fixture and unit paths
    // exercise exactly the same values as the future orchestrator.
    PackagePlan {
        repo: plan.repo.clone(),
        package: plan.package.clone(),
        head: plan.head.clone(),
        build: plan.build.clone(),
        selections: plan.selections.clone(),
        compile_only: plan.compile_only,
        features: plan.features.clone(),
        exact_tests: plan.exact_tests.clone(),
        changed_modules: plan.changed_modules.clone(),
        direct_selections: plan.direct_selections.clone(),
        slow_tier_excluded: plan.slow_tier_excluded.clone(),
        time_box_sec: plan.time_box_sec,
        tmpfs: plan.tmpfs.clone(),
        e2e: plan.e2e.clone(),
        runner: plan.runner.clone(),
        nextest: plan.nextest.clone(),
        test_shards: plan.test_shards,
        runtime_max_sec: plan.runtime_max_sec,
        tool_repo: plan.tool_repo.clone(),
    }
}

pub fn main(path: &Path, ctx: &crate::Ctx, cancel: &AtomicUsize) -> io::Result<i32> {
    let plan: PackagePlan = serde_json::from_slice(&std::fs::read(path)?)?;
    crate::source_stamp::verify(
        plan.tool_repo.as_ref().unwrap_or(&plan.repo),
        env!("RSI_REPO_RUNNER_SOURCE_STAMP"),
    )?;
    let mut source_ctx = ctx.clone();
    source_ctx.cwd = plan.repo.clone();
    let source = super::provenance::SourcePin::capture(&source_ctx, "HEAD", &plan.head)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut write_error = None;
    let env = process::build_env(ctx);
    let code = source.monitored(&env, cancel, |cancel| {
        execute(&plan, &env, cancel, |line| {
            if let Err(error) = out.write_all(line.as_bytes()).and_then(|()| out.flush()) {
                write_error = Some(error);
            }
        })
    })?;
    if let Some(error) = write_error {
        return Err(error);
    }
    Ok(code)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    pub(super) fn fixture() -> (tempfile::TempDir, String) {
        // Mutating Git is pinned outside the managed sandbox, with hooks and
        // inherited repository selectors removed, for every fixture operation.
        let dir = tempfile::Builder::new()
            .prefix("rsi-worker-")
            .tempdir_in("/tmp")
            .unwrap();
        let git = |args: &[&str]| {
            let mut command = Command::new("git");
            for name in [
                "GIT_DIR",
                "GIT_WORK_TREE",
                "GIT_INDEX_FILE",
                "GIT_COMMON_DIR",
                "GIT_OBJECT_DIRECTORY",
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                "GIT_CONFIG_COUNT",
                "GIT_CONFIG_PARAMETERS",
                "GIT_NAMESPACE",
                "GIT_CEILING_DIRECTORIES",
            ] {
                command.env_remove(name);
            }
            let done = command
                .arg("-C")
                .arg(dir.path())
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.invalid",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                done.status.success(),
                "{}",
                String::from_utf8_lossy(&done.stderr)
            );
            String::from_utf8(done.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q"]);
        git(&["commit", "--allow-empty", "-qm", "fixture"]);
        let head = git(&["rev-parse", "HEAD"]);
        (dir, head)
    }
    pub(super) fn plan(dir: &Path, head: String, harness: &str) -> PackagePlan {
        let binary = dir.join("test-harness");
        std::fs::write(&binary, harness).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let artifact=json!({"reason":"compiler-artifact","package_id":"pkg 0.1.0","executable":binary,"manifest_path":dir.join("Cargo.toml"),"profile":{"test":true},"target":{"name":"pkg","kind":["lib"]}}).to_string();
        // Pass the serialized artifact as argv rather than interpolating shell code.
        let build = vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf '%s\\n' \"$1\"".into(),
            "fixture".into(),
            artifact,
        ];
        PackagePlan {
            repo: dir.into(),
            head,
            package: "pkg".into(),
            build,
            selections: selection::selection_plan(
                &["pkg=test(=suite::one)".into()],
                &BTreeMap::new(),
                &[],
            )
            .unwrap()
            .remove("pkg")
            .unwrap(),
            compile_only: false,
            features: vec![],
            exact_tests: None,
            changed_modules: vec![],
            direct_selections: vec![],
            slow_tier_excluded: Names::new(),
            time_box_sec: None,
            tmpfs: None,
            e2e: None,
            runner: Some("libtest".into()),
            nextest: None,
            test_shards: 1,
            runtime_max_sec: 5.0,
            tool_repo: None,
        }
    }
    #[test]
    fn fake_build_and_harness_prove_exact_identity_completion() {
        let (dir, head) = fixture();
        let plan = plan(
            dir.path(),
            head,
            "#!/bin/sh\ncase \"$*\" in\n*--ignored*) exit 0;;\n*--list*) printf 'suite::one: test\\nsuite::other: test\\n';;\n*) printf 'test suite::one ... ok\\ntest result: ok. 1 passed; 0 failed; 0 ignored\\n';;\nesac\n",
        );
        let mut output = String::new();
        let code = execute(
            &plan,
            &BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
            &AtomicUsize::new(0),
            |line| output.push_str(line),
        )
        .unwrap();
        assert_eq!(code, 0, "{output}");
        assert!(output.contains("SCOPED_END suite::one"));
        assert!(output.contains("SCOPED_COMPLETE pkg 1"));
        assert!(output.contains("\"pkg=test(=suite::one)\":[1,1]"));
    }
    #[test]
    fn zero_test_exit_and_head_mismatch_never_claim_green() {
        let (dir, head) = fixture();
        let mut plan = plan(
            dir.path(),
            head,
            "#!/bin/sh\ncase \"$*\" in\n*--ignored*) exit 0;;\n*--list*) printf 'suite::one: test\\n';;\n*) printf 'test result: ok. 0 passed; 0 failed; 0 ignored\\n';;\nesac\n",
        );
        let mut output = String::new();
        let env = BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]);
        assert_eq!(
            execute(&plan, &env, &AtomicUsize::new(0), |line| output
                .push_str(line))
            .unwrap(),
            1
        );
        assert!(output.contains("SCOPED_PHASE test_failed"));
        plan.head = "0000000000000000000000000000000000000000".into();
        assert!(
            execute(&plan, &env, &AtomicUsize::new(0), |_| {})
                .unwrap_err()
                .to_string()
                .contains("job_source_moved")
        );
    }
    #[test]
    fn worker_signal_keeps_started_tests_unfinished() {
        let (dir, head) = fixture();
        let plan = plan(
            dir.path(),
            head,
            "#!/bin/sh\ncase \"$*\" in\n*--ignored*) exit 0;;\n*--list*) printf 'suite::one: test\\n';;\n*) printf 'test suite::one ... '; kill -TERM $$;;\nesac\n",
        );
        let mut output = String::new();
        assert_eq!(
            execute(
                &plan,
                &BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
                &AtomicUsize::new(0),
                |line| output.push_str(line)
            )
            .unwrap(),
            1
        );
        assert!(output.contains("SCOPED_EXECUTED pkg 0"));
        assert!(output.contains("SCOPED_TEST_EXIT -15"));
    }
}

#[cfg(all(test, unix))]
mod parity_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    // The fixture helper always pins mutation to an external absolute Git root.
    #[test]
    fn nextest_and_e2e_use_real_status_and_prebuild_evidence() {
        let (dir, head) = super::tests::fixture();
        let mut plan = super::tests::plan(
            dir.path(),
            head,
            "#!/bin/sh\ncase \"$*\" in *--ignored*) exit 0;; *--list*) printf 'suite::one: test\\n';; esac\n",
        );
        let nextest = dir.path().join("fake-nextest");
        std::fs::write(
            &nextest,
            "#!/bin/sh\nprintf ' PASS [ 0.001s] (1/1) pkg suite::one\\n'\n",
        )
        .unwrap();
        std::fs::set_permissions(&nextest, std::fs::Permissions::from_mode(0o755)).unwrap();
        plan.runner = Some("nextest".into());
        plan.nextest = Some(vec![nextest.to_string_lossy().into_owned()]);
        plan.e2e = Some(
            json!({"prebuild":["/bin/sh","-c","test \"$RSI_E2E\" = 1 && printf e2e-prebuilt"]}),
        );
        let env = BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]);
        let mut output = String::new();
        assert_eq!(
            execute(&plan, &env, &AtomicUsize::new(0), |line| output
                .push_str(line))
            .unwrap(),
            0,
            "{output}"
        );
        assert!(output.contains("e2e-prebuilt"));
        assert!(output.contains("SCOPED_COMPLETE pkg 1"));
        std::fs::write(&nextest, "#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(
            execute(&plan, &env, &AtomicUsize::new(0), |_| {}).unwrap(),
            1
        );
    }
    #[test]
    fn changed_module_tests_survive_a_spent_box() {
        let (dir, head) = super::tests::fixture();
        let mut plan = super::tests::plan(
            dir.path(),
            head,
            "#!/bin/sh\ncase \"$*\" in *--ignored*) exit 0;; *--list*) printf 'suite::one: test\\nobserver::far: test\\n';; *) printf 'test suite::one ... ok\\ntest result: ok. 1 passed; 0 failed; 0 ignored\\n';; esac\n",
        );
        plan.selections = selection::selection_plan(
            &["pkg=test(/^(suite|observer)::/)".into()],
            &BTreeMap::new(),
            &[],
        )
        .unwrap()
        .remove("pkg")
        .unwrap();
        plan.changed_modules = vec![json!({"package":"pkg","module":"suite"})];
        plan.time_box_sec = Some(1e-9);
        let mut output = String::new();
        let env = BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]);
        assert_eq!(
            execute(&plan, &env, &AtomicUsize::new(0), |line| output
                .push_str(line))
            .unwrap(),
            0,
            "{output}"
        );
        assert!(output.contains("SCOPED_COMPLETE pkg 1"));
        assert!(output.contains("\"box_expired\":true"));
        assert!(output.contains("\"selected\":2"));
    }
}
