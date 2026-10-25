//! Production `orchestration::Backend`: the host-side adapters that run the
//! mandatory lanes. Every lane is bounded by the shared owned process pump, the
//! run's cancel flag and the source pin; sibling-owned gates are consumed only
//! through `Contracts` and stay red (`incomplete_contract`) until published.
use super::{
    admission::Admission,
    baseline::{self, Inputs as ReplayInputs, Unit, UnitOutcome},
    contracts::Contracts,
    execution::PackagePlan,
    gitx::Git,
    guards,
    linux_check::{self, Toolchain},
    orchestration::{Backend, BaselineComparison, WorkerExit},
    planner, process,
    provenance::{self, SourcePin},
    provision::{self, ScratchWorktree},
    receipt::Failure,
    residual_run,
    selection::Selection,
    warnings::{Baseline, Warnings, evaluate},
};
use crate::scratch_git::Policy;
use crate::{Ctx, desktop_execution, desktop_selection, pid_isolation};
use pid_isolation::Isolation;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicUsize};
use std::time::{Duration, Instant};

pub const BUILD_SLICE: &str = "rsi-build.slice";
pub const WARNING_BASELINE: &str = "tools/warning-baseline.json";
const ADMISSION_GRACE: Duration = Duration::from_secs(5);
/// A baseline replay's warm-pool seed is best effort, bounded like the legacy tool's.
const POOL_SEED_TIMEOUT: Duration = Duration::from_secs(300);

/// Everything the host needs that the planner decided.
pub struct HostPlan {
    pub repo: PathBuf,
    /// The checkout lanes execute in: `repo`, or a renumbered scratch candidate.
    pub work: PathBuf,
    pub base: String,
    /// Merge base the static gates, desktop selection and warning gate diff from.
    pub guard_base: String,
    /// The candidate head that executes (renumbered when a scratch candidate runs).
    pub head: String,
    /// The head the worker committed and the trial merge checks.
    pub source_head: String,
    pub log_dir: PathBuf,
    pub audits: Value,
    pub slot: Option<PathBuf>,
    pub cargo: Vec<String>,
    pub unit_quota: f64,
    pub tmpfs: Option<PathBuf>,
    pub e2e_prebuild: Option<Vec<String>>,
    pub slot_wait_max: Duration,
    pub total_max: Option<Duration>,
    pub runtime_max: Duration,
    pub job_deadline: Option<Instant>,
    pub started: Instant,
    pub linux: Option<LinuxLane>,
    pub features: BTreeMap<String, Vec<String>>,
    /// Remove Cargo's disposable incremental cache after a green package, before the next.
    pub reclaim_incremental: bool,
    pub package_count: usize,
}
/// The planned cross-compile lane (non-Linux hosts only), decided up front so a
/// dry run shows it; `entry` is the receipt entry in its planned state.
pub struct LinuxLane {
    pub entry: Value,
    pub toolchain: Option<Toolchain>,
}

pub struct Host<'a, C: Contracts> {
    pub ctx: &'a Ctx,
    pub contracts: C,
    pub cancel: Arc<AtomicUsize>,
    pub plan: HostPlan,
    pub runner: PathBuf,
    pub env: BTreeMap<String, String>,
    pub dry_run: bool,
    pin: SourcePin,
    work_pin: Option<SourcePin>,
    candidate: Option<ScratchWorktree>,
    admission: Option<Admission>,
    isolation: Option<Isolation>,
    native_broken: bool,
    slot_waited: Duration,
    packages_run: usize,
}

/// Remove only Cargo's disposable incremental cache for this target tree; returns bytes freed.
pub fn reclaim_incremental(target: &Path) -> u64 {
    fn size(path: &Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(path) else {
            return 0;
        };
        entries
            .flatten()
            .map(|entry| match entry.file_type() {
                Ok(kind) if kind.is_dir() => size(&entry.path()),
                Ok(kind) if kind.is_file() => entry.metadata().map_or(0, |m| m.len()),
                _ => 0,
            })
            .sum()
    }
    let cache = target.join("debug").join("incremental");
    if !cache.is_dir()
        || std::fs::symlink_metadata(&cache).is_ok_and(|m| m.file_type().is_symlink())
    {
        return 0;
    }
    let bytes = size(&cache);
    let _ = std::fs::remove_dir_all(&cache);
    bytes
}

/// Under test the outer scoped-test run reads the log with `--nocapture` and parses every line
/// that starts with a `SCOPED_` marker, so a fixture worker's `SCOPED_FAILURE` line would count
/// as a failure of the run itself. The prefix keeps fixture lines out of that grammar.
#[cfg(test)]
fn say(text: &str) {
    for line in text.trim_end_matches('\n').lines() {
        println!("fixture| {line}");
    }
}

#[cfg(not(test))]
fn say(text: &str) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    if !text.ends_with('\n') {
        let _ = out.write_all(b"\n");
    }
    let _ = out.flush();
}

impl<'a, C: Contracts> Host<'a, C> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ctx: &'a Ctx,
        contracts: C,
        cancel: Arc<AtomicUsize>,
        plan: HostPlan,
        runner: PathBuf,
        pin: SourcePin,
        work_pin: Option<SourcePin>,
        candidate: Option<ScratchWorktree>,
        dry_run: bool,
    ) -> Self {
        let env = process::build_env(ctx);
        Self {
            ctx,
            contracts,
            cancel,
            plan,
            runner,
            env,
            dry_run,
            pin,
            work_pin,
            candidate,
            admission: None,
            isolation: None,
            native_broken: false,
            slot_waited: Duration::ZERO,
            packages_run: 0,
        }
    }
    fn git(&self) -> Git<'_> {
        Git {
            repo: &self.plan.repo,
            env: &self.env,
            cancel: &self.cancel,
        }
    }
    fn active_pin(&self) -> &SourcePin {
        self.work_pin.as_ref().unwrap_or(&self.pin)
    }
    /// Admission-supplied build knobs over the baseline build environment.
    fn unit_env(&self) -> BTreeMap<String, String> {
        let mut env = self.env.clone();
        if let Some(admission) = &self.admission {
            env.extend(admission.environment.clone());
        }
        env
    }
    /// Variables only the host-side `systemd-run` client needs; they never cross
    /// into the unit's own environment (not in `BUILD_ENV_NAMES`).
    fn launch_env(&self, env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut env = env.clone();
        for name in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
            if let Some(value) = self.ctx.var(name) {
                env.insert(name.into(), value.into());
            }
        }
        env
    }
    fn remaining_total(&self) -> Duration {
        match self.plan.total_max {
            Some(total) => total.saturating_sub(self.plan.started.elapsed()),
            None => self.plan.runtime_max,
        }
    }
    fn isolation(&self) -> io::Result<&Isolation> {
        self.isolation
            .as_ref()
            .ok_or_else(|| io::Error::other("pid isolation was not probed before a lane"))
    }
    fn unit_name(&self) -> String {
        unit_name(self.ctx.var("RSI_JOB_ID"))
    }
    fn slice(&self) -> Option<String> {
        let name = self.ctx.var("RSI_BUILD_SLICE").unwrap_or(BUILD_SLICE);
        (!name.is_empty()).then(|| name.to_string())
    }
}

/// The daemon job id (a lowercase UUID) goes in the name so the CPU andon
/// attributes this separate cgroup to its job.
fn unit_name(job_id: Option<&str>) -> String {
    let tag = job_id
        .filter(|id| {
            id.len() == 36
                && id
                    .bytes()
                    .all(|b| b == b'-' || b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .map(|id| format!("{id}-"))
        .unwrap_or_default();
    format!("rsi-scoped-test-{tag}{}", uuid::Uuid::new_v4().simple())
}

/// Runs a bounded worker unit; shared by package lanes and baseline replays.
fn run_unit(
    runner: &Path,
    plan_path: &Path,
    plan_repo: &Path,
    seconds: f64,
    quota: f64,
    unit: &str,
    slice: Option<&str>,
    isolation: &Isolation,
    unit_env: &BTreeMap<String, String>,
    launch: &BTreeMap<String, String>,
    cancel: &AtomicUsize,
    cwd: &Path,
    mut emit: impl FnMut(&str),
) -> io::Result<process::Outcome> {
    let command = provenance::bounded_command(
        runner, plan_path, plan_repo, seconds, quota, unit, unit_env, slice, isolation,
    )?;
    let deadline = Instant::now() + Duration::from_secs_f64(seconds) + ADMISSION_GRACE;
    process::stream(
        &command,
        cwd,
        launch,
        deadline,
        cancel,
        |line| emit(line),
        |_| {},
    )
}

struct ReplayUnit<'h, C: Contracts> {
    contracts: &'h mut C,
    runner: PathBuf,
    unit_env: BTreeMap<String, String>,
    launch: BTreeMap<String, String>,
    isolation: Isolation,
    cancel: Arc<AtomicUsize>,
    quota: f64,
    slice: Option<String>,
    name: Box<dyn Fn() -> String + 'h>,
}
impl<C: Contracts> Unit for ReplayUnit<'_, C> {
    fn run(
        &mut self,
        plan: &PackagePlan,
        plan_path: &Path,
        budget: Duration,
        target: &Path,
    ) -> io::Result<UnitOutcome> {
        let mut unit_env = self.unit_env.clone();
        unit_env.insert(
            "CARGO_TARGET_DIR".into(),
            target.to_string_lossy().into_owned(),
        );
        let mut launch = self.launch.clone();
        launch.insert(
            "CARGO_TARGET_DIR".into(),
            target.to_string_lossy().into_owned(),
        );
        let mut output = String::new();
        let outcome = run_unit(
            &self.runner,
            plan_path,
            &plan.repo,
            budget.as_secs_f64().max(1.0),
            self.quota,
            &(self.name)(),
            self.slice.as_deref(),
            &self.isolation,
            &unit_env,
            &launch,
            &self.cancel,
            &plan.repo,
            |line| output.push_str(line),
        )?;
        if outcome.cancelled.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "baseline replay interrupted",
            ));
        }
        Ok(UnitOutcome {
            code: outcome.code,
            timed_out: outcome.timed_out,
            output,
        })
    }
    fn features(
        &mut self,
        worktree: &Path,
        base: &str,
        package: &str,
        selections: &[Selection],
    ) -> io::Result<Vec<String>> {
        let inventory = self
            .contracts
            .feature_test_inventory(worktree, base, package)
            .map_err(|incomplete| incomplete.error())?;
        planner::required_features(&inventory, selections).map_err(io::Error::other)
    }
    fn seed(&mut self, worktree: &Path, target: &Path, log_dir: &Path) {
        // The pool seed is a killable child of this binary under the legacy 300 s bound.
        let argv = vec![
            self.runner.to_string_lossy().into_owned(),
            "target-pool".into(),
            "seed".into(),
            "--sandbox".into(),
            worktree.to_string_lossy().into_owned(),
            "--target".into(),
            target.to_string_lossy().into_owned(),
        ];
        let result = super::converged::run_supervised_gate(
            "target-pool seed",
            &argv,
            worktree,
            &self.unit_env,
            Instant::now() + POOL_SEED_TIMEOUT,
            &self.cancel,
        );
        let detail = result["detail"].as_str().unwrap_or_default();
        let _ = std::fs::write(log_dir.join("baseline-seed.log"), format!("{detail}\n"));
    }
}

impl<C: Contracts> Backend for Host<'_, C> {
    fn verify_source(&mut self) -> io::Result<()> {
        self.pin.verify(&self.env)?;
        if let Some(pin) = &self.work_pin {
            pin.verify(&self.env)?;
        }
        Ok(())
    }

    fn pid_isolation(&mut self) -> io::Result<Isolation> {
        let probe = pid_isolation::probe(
            self.ctx,
            &[
                self.runner.to_string_lossy().into_owned(),
                "pid-isolate".into(),
                "--canary".into(),
            ],
        );
        self.isolation = Some(probe.clone());
        Ok(probe)
    }

    fn static_guards(&mut self) -> io::Result<BTreeMap<String, Value>> {
        let git = Git {
            repo: &self.plan.work,
            env: &self.env,
            cancel: &self.cancel,
        };
        let inputs = guards::Inputs {
            repo: &self.plan.work,
            base: &self.plan.guard_base,
            head: &self.plan.head,
            audits: &self.plan.audits,
            environment: &self.env,
        };
        Ok(guards::run(&inputs, &git, &mut self.contracts, say))
    }

    fn residual(&mut self, dry_run: bool, isolation: &Isolation) -> io::Result<Vec<Value>> {
        // #366's contract owns the selection; the runner executes the ported (bash) suites.
        let entries =
            match self
                .contracts
                .residual(&self.plan.work, &self.plan.guard_base, &self.plan.head)
            {
                Ok(entries) => entries,
                Err(incomplete) => return Ok(vec![incomplete.value()]),
            };
        // 300 s per suite, as the Python phase allowed: one suite never takes the whole run.
        let run = residual_run::Run {
            work: &self.plan.work,
            env: &self.env,
            isolation,
            cancel: &self.cancel,
            per_suite: self.plan.runtime_max.min(Duration::from_secs(300)),
            total_left: self
                .plan
                .total_max
                .map(|total| total.saturating_sub(self.plan.started.elapsed())),
            log_dir: &self.plan.log_dir,
        };
        residual_run::execute(entries, dry_run, &run, &mut |line| say(line))
    }

    fn desktop(&mut self, dry_run: bool, isolation: &Isolation) -> io::Result<Vec<Value>> {
        let selected = match desktop_selection::lane_selected(
            &self.plan.work,
            &self.plan.guard_base,
            &self.plan.head,
        ) {
            Ok(selected) => selected,
            // No selector script in the candidate is a refusal, never an empty lane (#355).
            Err(reason) => {
                say(&format!("scoped-test: desktop lane: {reason}"));
                return Ok(vec![
                    json!({"status":"failed","lane":"desktop","reason":reason,"executed_tests":0}),
                ]);
            }
        };
        if !selected {
            return Ok(vec![]);
        }
        let mut environment = self.env.clone();
        desktop_execution::forward_lane_inputs(|name| self.ctx.var(name), &mut environment);
        if dry_run {
            // The runner probe is part of the plan: a missing runner is recorded, never hidden.
            let mut quiet = |_: &str| {};
            let mut probe = desktop_execution::ProcessBackend {
                environment: &environment,
                repo: &self.plan.work,
                isolation,
                cancel: &self.cancel,
                emit: &mut quiet,
                deadline: None,
            };
            return Ok(vec![
                match desktop_execution::Backend::preflight(&mut probe)? {
                    Some(missing) => {
                        say(&format!(
                            "scoped-test: runner_unavailable runner={} executed_tests=0 lane=desktop: {} (a real run records failure and continues to Cargo)",
                            missing["runner"].as_str().unwrap_or("?"),
                            missing["reason"].as_str().unwrap_or("?")
                        ));
                        desktop_execution::receipt_entry(missing, 1)
                    }
                    None => {
                        json!({"lane":"desktop","status":"planned","completed":false,"runner":"npm+cargo","executed_tests":null})
                    }
                },
            ]);
        }
        let budget = match self.plan.total_max {
            Some(total) => self
                .plan
                .runtime_max
                .min(total.saturating_sub(self.plan.started.elapsed())),
            None => self.plan.runtime_max,
        };
        if budget.is_zero() {
            say("scoped_test_timeout: total budget exhausted before the desktop lane");
            return Ok(vec![
                json!({"lane":"desktop","status":"timed_out","completed":false,"runner":"npm+cargo","executed_tests":null,"reason":"total budget exhausted before the desktop lane"}),
            ]);
        }
        let scratch_parent = self.plan.tmpfs.clone().unwrap_or_else(std::env::temp_dir);
        let log_path = self.plan.log_dir.join("desktop.log");
        let mut log = std::fs::File::create(&log_path)?;
        let mut write_error = None;
        let mut emit = |line: &str| {
            say(line.trim_end_matches('\n'));
            if let Err(error) = log.write_all(line.as_bytes()) {
                write_error = Some(error);
            }
        };
        let mut backend = desktop_execution::ProcessBackend {
            environment: &environment,
            repo: &self.plan.work,
            isolation,
            cancel: &self.cancel,
            emit: &mut emit,
            deadline: Instant::now().checked_add(budget),
        };
        let (code, mut result) = desktop_execution::run_lane_with_cancel(
            &self.plan.work,
            &environment,
            &scratch_parent,
            &self.cancel,
            &mut backend,
        )?;
        if let Some(error) = write_error {
            return Err(error);
        }
        result["log"] = json!(log_path);
        Ok(vec![desktop_execution::receipt_entry(result, code)])
    }

    fn admit(&mut self, cap: Duration) -> io::Result<()> {
        let Some(slot) = self.plan.slot.clone() else {
            // No governor client: the only safe state is an outer held admission.
            if self.ctx.var("RSI_CARGO_SLOT_HELD") == Some("1") {
                return Ok(());
            }
            return Err(io::Error::other(
                "cargo-slot admission unavailable: no cargo-slot client and no outer held admission",
            ));
        };
        let cap = cap.min(self.plan.slot_wait_max.saturating_sub(self.slot_waited));
        if cap.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "admission-timeout: no admission allowance left",
            ));
        }
        let started = Instant::now();
        let admission =
            Admission::acquire(&slot, &self.runner, self.ctx, cap, &self.cancel, |line| {
                say(line.trim_end_matches('\n'))
            })?;
        self.slot_waited += started.elapsed();
        self.admission = Some(admission);
        Ok(())
    }

    fn package(
        &mut self,
        plan: &PackagePlan,
        budget: Duration,
        time_box: Option<Duration>,
        isolation: &Isolation,
        emit: &mut dyn FnMut(&str),
    ) -> io::Result<WorkerExit> {
        if let Some(admission) = self.admission.as_mut() {
            admission.check()?;
        }
        let mut owned = serde_json::from_value::<PackagePlan>(serde_json::to_value(plan)?)?;
        owned.time_box_sec = time_box.map(|t| t.as_secs_f64());
        owned.runtime_max_sec = budget.as_secs_f64();
        let plan_path = self.plan.log_dir.join(format!("{}.json", owned.package));
        std::fs::write(
            &plan_path,
            format!("{}\n", serde_json::to_string_pretty(&owned)?),
        )?;
        let log_path = self.plan.log_dir.join(format!("{}.log", owned.package));
        let mut log = std::fs::File::create(&log_path)?;
        let unit_env = self.unit_env();
        let launch = self.launch_env(&unit_env);
        let (runner, quota, slice, name) = (
            self.runner.clone(),
            self.plan.unit_quota,
            self.slice(),
            self.unit_name(),
        );
        let cancel = self.cancel.clone();
        let mut broken = false;
        let mut write_error = None;
        let outcome = self.active_pin().monitored(&self.env, &cancel, |cancel| {
            run_unit(
                &runner,
                &plan_path,
                &owned.repo,
                budget.as_secs_f64(),
                quota,
                &name,
                slice.as_deref(),
                isolation,
                &unit_env,
                &launch,
                cancel,
                &owned.repo,
                |line| {
                    if line.starts_with("SCOPED_PHASE build_failed")
                        || line.starts_with("SCOPED_PHASE list_failed")
                    {
                        broken = true;
                    }
                    say(line.trim_end_matches('\n'));
                    if let Err(error) = log.write_all(line.as_bytes()) {
                        write_error = Some(error);
                    }
                    emit(line);
                },
            )
        })?;
        self.native_broken |= broken;
        if let Some(error) = write_error {
            return Err(error);
        }
        if let Some(admission) = self.admission.as_mut() {
            admission.check()?;
        }
        self.packages_run += 1;
        if self.plan.reclaim_incremental
            && self.packages_run < self.plan.package_count
            && outcome.code == 0
            && !outcome.timed_out
            && outcome.cancelled.is_none()
        {
            let target = self
                .ctx
                .var("CARGO_TARGET_DIR")
                .map(|t| self.ctx.path(t))
                .unwrap_or_else(|| self.plan.work.join("target"));
            say(&format!(
                "scoped-test: reclaimed {} bytes of Cargo incremental cache before next package",
                reclaim_incremental(&target)
            ));
        }
        Ok(match outcome.cancelled {
            Some(signal) => WorkerExit {
                code: -(signal as i32),
                timed_out: false,
            },
            None => WorkerExit {
                code: outcome.code,
                timed_out: outcome.timed_out,
            },
        })
    }

    fn warnings(&mut self, payloads: &[Value]) -> io::Result<Value> {
        let path = self.plan.work.join(WARNING_BASELINE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(
                    json!({"status":"not_applicable","detail":"the repository has no warning baseline"}),
                );
            }
            Err(error) => {
                return Ok(
                    json!({"status":"unavailable","unavailable":[format!("{WARNING_BASELINE} is unreadable: {error}")],"packages":{}}),
                );
            }
        };
        let candidate = match Baseline::parse(&text) {
            Ok(candidate) => candidate,
            Err(error) => {
                return Ok(
                    json!({"status":"unavailable","unavailable":[format!("{WARNING_BASELINE} is unreadable: {error}")],"packages":{}}),
                );
            }
        };
        let mut observed = Warnings::new(&self.env);
        for payload in payloads {
            match serde_json::from_value::<Warnings>(payload.clone()) {
                Ok(part) => observed.merge(part),
                Err(_) => observed
                    .degraded
                    .push("a SCOPED_WARNINGS payload was not a valid warning record".into()),
            }
        }
        let git = Git {
            repo: &self.plan.work,
            env: &self.env,
            cancel: &self.cancel,
        };
        let (base, state) = match git.commit(&self.plan.guard_base) {
            Ok(Some(_)) => match git.builtin(
                &[
                    "show",
                    &format!("{}:{WARNING_BASELINE}", self.plan.guard_base),
                ],
                &[],
            ) {
                Ok(out) if out.ok() => match Baseline::parse(&out.stdout) {
                    Ok(base) => (Some(base), "present"),
                    Err(_) => (None, "unreadable"),
                },
                Ok(_) => (None, "absent"),
                Err(_) => (None, "unresolved"),
            },
            _ => (None, "unresolved"),
        };
        Ok(evaluate(&observed, &candidate, base.as_ref(), state))
    }

    fn compare_baseline(&mut self, failures: &[Failure]) -> io::Result<BaselineComparison> {
        let unit_env = self.unit_env();
        let launch = self.launch_env(&unit_env);
        let isolation = self.isolation()?.clone();
        let policy = Policy::new(
            &self.plan.repo.canonicalize()?,
            &std::env::current_dir()?,
            self.env.iter().map(|(k, v)| (k.into(), v.into())).collect(),
            &[],
        )?;
        let remaining = self.remaining_total().min(match self.plan.job_deadline {
            Some(deadline) => deadline.saturating_duration_since(Instant::now()),
            None => Duration::MAX,
        });
        let env = self.env.clone();
        let cancel = self.cancel.clone();
        let repo = self.plan.repo.clone();
        let git = Git {
            repo: &repo,
            env: &env,
            cancel: &cancel,
        };
        let target = self
            .ctx
            .var("CARGO_TARGET_DIR")
            .map(|t| self.ctx.path(t))
            .unwrap_or_else(|| repo.join("target"));
        let tmpfs = self.plan.tmpfs.clone();
        let cargo = self.plan.cargo.clone();
        let inputs = ReplayInputs {
            repo: &repo,
            log_dir: &self.plan.log_dir,
            cargo: &cargo,
            tmpfs: tmpfs.as_deref(),
            e2e_prebuild: self.plan.e2e_prebuild.as_deref(),
            cpu_quota: self.plan.unit_quota,
            target_dir: &target,
        };
        let (runner, quota, slice) = (self.runner.clone(), self.plan.unit_quota, self.slice());
        let tag = self.ctx.var("RSI_JOB_ID").map(str::to_owned);
        let (base, head) = (self.plan.base.clone(), self.plan.head.clone());
        let mut unit = ReplayUnit {
            contracts: &mut self.contracts,
            runner,
            unit_env,
            launch,
            isolation,
            cancel: cancel.clone(),
            quota,
            slice,
            name: Box::new(move || unit_name(tag.as_deref())),
        };
        let tip = baseline::compare(
            &git, &policy, &inputs, &mut unit, &base, failures, remaining,
        );
        let started = self.plan.started;
        let (total, runtime) = (self.plan.total_max, self.plan.runtime_max);
        Ok(baseline::with_fork_point(
            &git,
            &policy,
            &inputs,
            &mut unit,
            &head,
            &base,
            failures,
            tip,
            move || match total {
                Some(total) => total.saturating_sub(started.elapsed()),
                None => runtime,
            },
        ))
    }

    fn linux_check(&mut self) -> io::Result<Option<Value>> {
        let Some(lane) = self.plan.linux.as_ref() else {
            return Ok(None);
        };
        let mut entry = lane.entry.clone();
        if entry["status"] != "pending" {
            return Ok(Some(entry));
        }
        let held = self.admission.is_some() || self.ctx.var("RSI_CARGO_SLOT_HELD") == Some("1");
        let budget =
            self.plan
                .runtime_max
                .min(self.remaining_total())
                .min(match self.plan.job_deadline {
                    Some(deadline) => deadline.saturating_duration_since(Instant::now()),
                    None => Duration::MAX,
                });
        let reason = if !held {
            Some("admission_lost")
        } else if self.native_broken {
            Some("native_build_failed")
        } else if budget < Duration::from_secs(1) {
            Some("budget_exhausted")
        } else {
            None
        };
        if let Some(reason) = reason {
            entry["status"] = json!("not_covered");
            entry["reason"] = json!(reason);
            return Ok(Some(entry));
        }
        let Some(toolchain) = lane.toolchain.as_ref() else {
            entry["status"] = json!("not_covered");
            entry["reason"] = json!("c_toolchain_missing");
            return Ok(Some(entry));
        };
        let packages: Vec<String> = entry["packages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        let unit_env = self.unit_env();
        let mut ctx = Ctx::new(
            &self.plan.work,
            &unit_env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect::<Vec<_>>(),
        );
        ctx.cwd = self.active_pin().repo.clone();
        let check = linux_check::Check {
            source: self.active_pin(),
            cargo: &self.plan.cargo,
            packages: &packages,
            features: &self.plan.features,
            toolchain,
            log_dir: &self.plan.log_dir,
            budget,
        };
        linux_check::run_pending(&mut entry, &check, &ctx, &self.cancel)?;
        Ok(Some(entry))
    }

    fn merge_check(&mut self) -> io::Result<Value> {
        // A renumbered candidate already contains rolling: its trial merge is the one
        // that lands, while the blame names the head the worker committed (#3339).
        let renumbered = self.candidate.is_some() && self.plan.head != self.plan.source_head;
        let candidate = if renumbered {
            &self.plan.head
        } else {
            &self.plan.source_head
        };
        let merge = provision::trial_merge(&self.git(), candidate, "origin/rolling");
        let mut fields = merge.fields;
        let skipped_reason = fields["merge_check_reason"]
            .as_str()
            .unwrap_or("trial merge conflicted; see conflict_paths")
            .to_string();
        let skipped_diagnostic = fields["merge_check_diagnostic"].clone();
        fields["reverts_recent_landing"] = crate::recent_landing::advisory(
            &self.plan.repo,
            &self.plan.source_head,
            merge
                .tree
                .as_ref()
                .map(|(rolling, tree)| (rolling.as_str(), tree.as_str())),
            &skipped_reason,
            &skipped_diagnostic,
            renumbered,
        );
        Ok(fields)
    }

    fn release(&mut self) -> io::Result<()> {
        self.admission = None;
        self.candidate = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_names_carry_only_a_lowercase_job_uuid_and_are_unique() {
        let job = "0123abcd-4567-89ab-cdef-0123456789ab";
        let first = unit_name(Some(job));
        assert!(first.starts_with(&format!("rsi-scoped-test-{job}-")));
        assert_ne!(first, unit_name(Some(job)));
        for bad in [
            "UPPER23abcd-4567-89ab-cdef-0123456789ab",
            "not-a-uuid",
            "../../x",
            "",
        ] {
            let name = unit_name(Some(bad));
            assert!(!name.contains(bad) || bad.is_empty(), "{name}");
            assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
        }
        assert!(unit_name(None).starts_with("rsi-scoped-test-"));
    }

    #[test]
    fn reclaim_removes_only_the_incremental_cache_and_never_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let cache = target.join("debug/incremental/pkg");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("data"), vec![0u8; 100]).unwrap();
        std::fs::write(target.join("debug/keep"), b"keep").unwrap();
        assert_eq!(reclaim_incremental(&target), 100);
        assert!(!target.join("debug/incremental").exists());
        assert!(target.join("debug/keep").is_file());
        assert_eq!(reclaim_incremental(&target), 0);
        #[cfg(unix)]
        {
            let outside = dir.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("precious"), b"x").unwrap();
            std::os::unix::fs::symlink(&outside, target.join("debug/incremental")).unwrap();
            assert_eq!(reclaim_incremental(&target), 0);
            assert!(outside.join("precious").is_file());
        }
    }
}

#[cfg(all(test, unix, not(target_os = "linux")))]
mod end_to_end {
    //! The whole orchestration on the production `Host` with a scripted worker:
    //! real Git source pin, real static gates, real process supervision and
    //! source monitoring. Linux wraps the worker in a systemd unit, which is the
    //! hub's Linux lane, so this host-neutral contract runs on the other hosts.
    use super::*;
    use crate::scoped_runner::{
        contracts::{Incomplete, Renumbered, SlowTier},
        orchestration::{self, PlannedPackage, RunPlan},
        selection,
    };
    use crate::testutil;
    use std::os::unix::fs::PermissionsExt;

    #[derive(Default)]
    struct AllOk {
        residual: Vec<Value>,
    }
    impl Contracts for AllOk {
        fn derive(
            &mut self,
            _: &Path,
            _: &str,
            _: &str,
        ) -> Result<crate::scoped_runner::contracts::Derivation, Incomplete> {
            unreachable!("explicit filters derive nothing")
        }
        fn shard_packages(
            &mut self,
            _: &Path,
        ) -> Result<BTreeMap<String, Vec<String>>, Incomplete> {
            Ok(BTreeMap::new())
        }
        fn feature_test_inventory(
            &mut self,
            _: &Path,
            _: &str,
            _: &str,
        ) -> Result<Vec<(String, Vec<String>)>, Incomplete> {
            Ok(vec![])
        }
        fn load_slow_tier(&mut self, _: &Path, _: &str) -> Result<Option<SlowTier>, Incomplete> {
            Ok(None)
        }
        fn downstream_test_plan(
            &mut self,
            _: &Path,
            _: &str,
            _: &str,
        ) -> Result<Value, Incomplete> {
            Ok(json!({"packages": []}))
        }
        fn renumber(&mut self, _: &Path, _: &str, _: &str) -> Result<Renumbered, Incomplete> {
            Ok(Renumbered::Absent)
        }
        fn guard(&mut self, _: &str, _: &Path, _: &str, _: &str) -> Result<Value, Incomplete> {
            Ok(json!({"status": "ok", "detail": "test double"}))
        }
        fn residual(&mut self, _: &Path, _: &str, _: &str) -> Result<Vec<Value>, Incomplete> {
            Ok(self.residual.clone())
        }
    }

    fn fixture(worker: &str) -> (tempfile::TempDir, tempfile::TempDir, PathBuf, Value) {
        let repo = tempfile::tempdir().unwrap();
        testutil::git(repo.path(), &["init", "-q", "-b", "main"]);
        testutil::write(repo.path(), "a.txt", "one\n");
        testutil::git(repo.path(), &["add", "a.txt"]);
        testutil::git(
            repo.path(),
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
        let logs = tempfile::tempdir().unwrap();
        let script = logs.path().join("worker");
        std::fs::write(&script, format!("#!/bin/sh\n{worker}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let plan =
            selection::selection_plan(&["pkg=test(=suite::one)".into()], &BTreeMap::new(), &[])
                .unwrap();
        let selections = plan["pkg"].clone();
        (
            repo,
            logs,
            script,
            serde_json::to_value(selections).unwrap(),
        )
    }

    fn run_with(worker: &str) -> Value {
        let (repo, logs, script, selections) = fixture(worker);
        let head = testutil::git(repo.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_string();
        let ctx = Ctx::new(
            repo.path(),
            &[
                ("PATH", &std::env::var("PATH").unwrap()),
                ("HOME", &std::env::var("HOME").unwrap()),
                ("RSI_CARGO_SLOT_HELD", "1"),
            ],
        );
        let env = process::build_env(&ctx);
        let pin = SourcePin {
            repo: repo.path().to_path_buf(),
            head: head.clone(),
            base: head.clone(),
            inspect_only: false,
        };
        let selections: Vec<Selection> = serde_json::from_value(selections).unwrap();
        let package = PackagePlan {
            repo: repo.path().to_path_buf(),
            package: "pkg".into(),
            head: head.clone(),
            build: vec!["cargo".into()],
            selections,
            compile_only: false,
            features: vec![],
            exact_tests: None,
            changed_modules: vec![],
            direct_selections: vec![],
            slow_tier_excluded: Default::default(),
            time_box_sec: None,
            tmpfs: None,
            e2e: None,
            runner: Some("libtest".into()),
            nextest: None,
            test_shards: 1,
            runtime_max_sec: 30.0,
            tool_repo: None,
        };
        let run = RunPlan {
            repo: repo.path().to_path_buf(),
            log_dir: logs.path().to_path_buf(),
            base: head.clone(),
            head: head.clone(),
            tool: json!({"runner": "test"}),
            filters: vec!["pkg=test(=suite::one)".into()],
            downstream_compile: json!({"packages": []}),
            downstream_tests: json!({"items": [], "selections": [], "filters": [], "notes": []}),
            packages: vec![PlannedPackage {
                plan: package,
                downstream_compile: false,
            }],
            runtime_max: Duration::from_secs(30),
            total_max: Duration::from_secs(120),
            admission_max: Duration::from_secs(10),
            test_time_box: None,
            dry_run: false,
            receipt_fields: Default::default(),
        };
        let plan = HostPlan {
            repo: repo.path().to_path_buf(),
            work: repo.path().to_path_buf(),
            base: head.clone(),
            guard_base: head.clone(),
            head: head.clone(),
            source_head: head,
            log_dir: logs.path().to_path_buf(),
            audits: json!({}),
            slot: None,
            cargo: vec!["cargo".into()],
            unit_quota: 100.0,
            tmpfs: None,
            e2e_prebuild: None,
            slot_wait_max: Duration::from_secs(10),
            total_max: Some(Duration::from_secs(120)),
            runtime_max: Duration::from_secs(30),
            job_deadline: None,
            started: Instant::now(),
            linux: None,
            features: BTreeMap::new(),
            reclaim_incremental: false,
            package_count: 1,
        };
        let _ = env;
        let mut host = Host::new(
            &ctx,
            AllOk::default(),
            Arc::new(AtomicUsize::new(0)),
            plan,
            script,
            pin,
            None,
            None,
            false,
        );
        orchestration::run(&run, &mut host, |_| {})
    }

    const GREEN: &str = "printf 'SCOPED_PHASE compiled\\nSCOPED_PHASE listed\\nSCOPED_TARGET lib:pkg\\nSCOPED_START suite::one\\nSCOPED_END suite::one\\nSCOPED_MATCH pkg=test(=suite::one)\\n'; printf 'SCOPED_SUBSET {\"package\":\"pkg\",\"filters\":{\"pkg=test(=suite::one)\":[1,1]},\"selected\":1,\"executed\":1,\"test_sec\":0.1}\\nSCOPED_EXECUTED pkg 1\\nSCOPED_COMPLETE pkg 1\\n'";

    #[test]
    fn a_scripted_green_worker_yields_a_green_receipt_only_with_every_lane_attested() {
        let receipt = run_with(GREEN);
        assert_eq!(receipt["exit_code"], 0, "{receipt}");
        assert_eq!(receipt["ok"], true, "{receipt}");
        assert_eq!(receipt["packages"][0]["status"], "passed");
        assert_eq!(receipt["packages"][0]["completed"], 1);
        assert_eq!(receipt["source_verified"], true);
        assert_eq!(receipt["warnings"]["status"], "not_applicable");
        assert_eq!(
            receipt["static_guards"].as_object().unwrap().len(),
            super::super::orchestration::REQUIRED_GUARDS.len()
        );
        assert_eq!(receipt["unmatched_filters"], json!([]));
        assert_eq!(receipt["selection"]["fields"]["filters"]["count"], 1);
    }

    #[test]
    fn a_worker_that_never_attests_completion_cannot_be_green() {
        let receipt = run_with("printf 'SCOPED_PHASE compiled\\nSCOPED_PHASE listed\\n'");
        assert_eq!(receipt["ok"], false, "{receipt}");
        assert_ne!(receipt["packages"][0]["status"], "passed");
    }

    #[test]
    fn a_failing_worker_reports_its_failure_and_needs_a_baseline_before_it_is_anything_but_new() {
        let receipt = run_with(
            "printf 'SCOPED_PHASE compiled\\nSCOPED_PHASE listed\\nSCOPED_PHASE test_failed\\nSCOPED_FAILURE lib:pkg suite::one\\nSCOPED_EXECUTED pkg 1\\n'; exit 1",
        );
        assert_eq!(receipt["ok"], false, "{receipt}");
        assert_eq!(receipt["failures"][0]["test"], "suite::one");
    }
    /// A repository whose head touches a desktop input, with the runner PATH
    /// pointing at an empty directory so no node/npm/cargo can be found.
    fn desktop_host<'a>(
        path_dir: &Path,
        total_max: Option<Duration>,
        dry_run: bool,
    ) -> (tempfile::TempDir, Host<'a, AllOk>) {
        let repo = tempfile::tempdir().unwrap();
        testutil::git(repo.path(), &["init", "-q", "-b", "main"]);
        testutil::write(
            repo.path(),
            "scripts/desktop-test-gate.py",
            "selector marker\n",
        );
        testutil::write(repo.path(), "desktop/package.json", "{}\n");
        testutil::git(repo.path(), &["add", "-A"]);
        testutil::git(repo.path(), &["commit", "-qm", "base"]);
        let base = testutil::git(repo.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_string();
        testutil::write(
            repo.path(),
            "desktop/package.json",
            "{\"name\":\"changed\"}\n",
        );
        testutil::git(repo.path(), &["add", "-A"]);
        testutil::git(repo.path(), &["commit", "-qm", "desktop change"]);
        let head = testutil::git(repo.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_string();
        let logs = repo.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let ctx: &'static Ctx = Box::leak(Box::new(Ctx::new(
            repo.path(),
            &[("PATH", &path_dir.to_string_lossy())],
        )));
        let plan = HostPlan {
            repo: repo.path().to_path_buf(),
            work: repo.path().to_path_buf(),
            base: base.clone(),
            guard_base: base.clone(),
            head: head.clone(),
            source_head: head.clone(),
            log_dir: logs,
            audits: json!({}),
            slot: None,
            cargo: vec!["cargo".into()],
            unit_quota: 100.0,
            tmpfs: None,
            e2e_prebuild: None,
            slot_wait_max: Duration::from_secs(10),
            total_max,
            runtime_max: Duration::from_secs(30),
            job_deadline: None,
            started: Instant::now(),
            linux: None,
            features: BTreeMap::new(),
            reclaim_incremental: false,
            package_count: 0,
        };
        let pin = SourcePin {
            repo: repo.path().to_path_buf(),
            head,
            base,
            inspect_only: false,
        };
        let host = Host::new(
            ctx,
            AllOk::default(),
            Arc::new(AtomicUsize::new(0)),
            plan,
            PathBuf::from("/nonexistent-runner"),
            pin,
            None,
            None,
            dry_run,
        );
        (repo, host)
    }

    #[test]
    fn the_desktop_lane_records_a_missing_runner_in_the_plan_and_in_a_real_run() {
        let empty = tempfile::tempdir().unwrap();
        let isolation = Isolation::unavailable("fixture");
        for dry_run in [true, false] {
            let (_repo, mut host) =
                desktop_host(empty.path(), Some(Duration::from_secs(120)), dry_run);
            let entries = Backend::desktop(&mut host, dry_run, &isolation).unwrap();
            assert_eq!(entries.len(), 1, "{entries:?}");
            assert_eq!(entries[0]["status"], "runner_unavailable", "{entries:?}");
            assert_eq!(entries[0]["lane_status"], "runner_unavailable");
            assert_eq!(entries[0]["runner"], "node");
            assert_eq!(entries[0]["completed"], false);
            assert_eq!(entries[0]["executed_tests"], 0);
        }
    }

    #[test]
    fn an_exhausted_total_budget_stops_the_desktop_lane_before_it_starts() {
        let empty = tempfile::tempdir().unwrap();
        let isolation = Isolation::unavailable("fixture");
        let (_repo, mut host) = desktop_host(empty.path(), Some(Duration::ZERO), false);
        let entries = Backend::desktop(&mut host, false, &isolation).unwrap();
        assert_eq!(entries[0]["status"], "timed_out", "{entries:?}");
        assert_eq!(entries[0]["completed"], false);
        assert_eq!(
            entries[0]["reason"],
            "total budget exhausted before the desktop lane"
        );
    }

    fn suite_host(suites: Vec<Value>, dry_run: bool) -> (tempfile::TempDir, Host<'static, AllOk>) {
        let empty = tempfile::tempdir().unwrap();
        let (repo, mut host) = desktop_host(empty.path(), Some(Duration::from_secs(120)), dry_run);
        host.contracts.residual = suites;
        host.env
            .insert("PATH".into(), std::env::var("PATH").unwrap());
        std::mem::forget(empty);
        (repo, host)
    }

    #[test]
    fn a_ported_bash_suite_runs_in_the_candidate_checkout_and_its_verdict_enters_the_lane() {
        let isolation = Isolation::unavailable("fixture");
        let entry = |name: &str| super::residual_run::planned(name);
        let (repo, mut host) = suite_host(
            vec![
                entry("scripts/tests/test_ok.sh"),
                entry("scripts/tests/test_bad.sh"),
                entry("scripts/tests/test_never.sh"),
            ],
            false,
        );
        testutil::write(
            repo.path(),
            "scripts/tests/test_ok.sh",
            "echo 'ok 1 - fine'; echo 1..1; pwd > ran-here\n",
        );
        testutil::write(
            repo.path(),
            "scripts/tests/test_bad.sh",
            "echo 'not ok 1 - broken'; echo 1..1\n",
        );
        testutil::write(
            repo.path(),
            "scripts/tests/test_never.sh",
            "touch ran-never; echo 'ok 1 - x'\n",
        );
        let entries = Backend::residual(&mut host, false, &isolation).unwrap();
        assert_eq!(
            entries.len(),
            2,
            "the lane stops at the first red suite: {entries:?}"
        );
        assert_eq!(entries[0]["status"], "passed");
        assert_eq!(entries[0]["executed_tests"], 1);
        assert_eq!(entries[1]["status"], "failed");
        assert_eq!(entries[1]["failed_tests"], 1);
        assert!(
            std::fs::read_to_string(repo.path().join("ran-here")).is_ok(),
            "it ran in the checkout"
        );
        assert!(!repo.path().join("ran-never").exists());
        assert!(std::path::Path::new(entries[1]["log"].as_str().unwrap()).is_file());
    }

    #[test]
    fn a_dry_run_plans_a_bash_suite_without_running_it_and_a_python_suite_stays_red() {
        let isolation = Isolation::unavailable("fixture");
        let gap = json!({"status": "incomplete_contract", "lane": "residual", "owner": "#366", "test_file": "scripts/tests/test_x.py", "executed_tests": 0});
        let (repo, mut host) = suite_host(
            vec![
                super::residual_run::planned("scripts/tests/test_ok.sh"),
                gap.clone(),
            ],
            true,
        );
        testutil::write(
            repo.path(),
            "scripts/tests/test_ok.sh",
            "touch ran\necho 'ok 1 - x'\n",
        );
        let entries = Backend::residual(&mut host, true, &isolation).unwrap();
        assert_eq!(
            entries[0],
            super::residual_run::planned("scripts/tests/test_ok.sh")
        );
        assert_eq!(entries[1], gap);
        assert!(!repo.path().join("ran").exists());
    }

    #[test]
    fn an_unrelated_diff_selects_no_desktop_lane() {
        let empty = tempfile::tempdir().unwrap();
        let isolation = Isolation::unavailable("fixture");
        let (repo, mut host) = desktop_host(empty.path(), None, false);
        host.plan.guard_base = host.plan.head.clone();
        let entries = Backend::desktop(&mut host, false, &isolation).unwrap();
        assert!(entries.is_empty(), "{entries:?}");
        drop(repo);
    }
}
