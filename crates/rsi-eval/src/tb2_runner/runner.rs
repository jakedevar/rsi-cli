//! Preflight and run orchestration over remote Docker (#3679 R2, design #3628 "Architecture and
//! transport" items 1-6).
//!
//! [`preflight`] is read-only: plan and pin validation against #3620's library, host
//! correspondence, Docker/Compose/platform, and the laptop's measured disk and memory against
//! the historical floors (largest selected shard + 10 GiB; concurrency x 4 GiB + 4 GiB), failing
//! closed when capacity cannot be measured. [`run`] repeats it, pulls each shard's images by
//! pinned digest, then runs bounded trials through R1's [`run_trial`] and writes one atomic
//! `outcome.json` per attempt, the run journal (R5) and `summary.json`.
//!
//! Harness trials use only the plan's validated run-local bench socket.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use rsi_common::bench_runner::{
    COPY_TIMEOUT_SEC, PREFLIGHT_TIMEOUT_SEC, PULL_TIMEOUT_SEC, PlannedTrial, RemotePlanV1,
    RunnerMode, RunnerPlanV1, TrialOutcomeV1, trial_key,
};
use serde::Serialize;

use super::{
    client::{BenchClient, ClientConfig, validate_socket},
    docker::{DockerClient, DockerFacts, EnvContext, RemoteEnvironment, TrialNames, pinned_ref},
    journal::{
        JournalError, JournalStore, RunIdentity, RunJournalV1, sha256_of, stale_env_facts,
        write_atomic,
    },
    lifecycle::{
        Adopt, BenchSessions, CancelFlag, EnvFactory, EpochPacer, Lifecycle, LifecycleError,
        SessionPort, TrialPaths,
    },
    stamp,
    transport::{
        CommandExecutor, OpError, OpKind, Remote, RemoteFacts, probe, validate_hosts,
        validate_remote_path,
    },
    trial::{Clock, TrialEnvironment},
    whole_seconds,
};
use crate::tb2::{
    dataset::{self, Dataset, Task},
    pin,
};

/// Disk headroom over the largest selected shard (matches the daemon's historical preflight).
pub const DISK_HEADROOM_BYTES: u64 = 10 * (1 << 30);
/// Memory the runner budgets per concurrent trial, and keeps free for the host.
pub const MEM_PER_TRIAL_BYTES: u64 = 4 * (1 << 30);
pub const MEM_RESERVE_BYTES: u64 = 4 * (1 << 30);
/// Directory under the laptop's `$HOME` that holds runner state.
pub const REMOTE_STATE_DIR: &str = ".rsi-bench/tb2-runner";

/// What preflight measured and decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreflightReport {
    pub run_id: String,
    pub docker_host: String,
    pub tasks: usize,
    pub trials: usize,
    pub concurrency: u32,
    pub largest_shard_bytes: u64,
    pub need_disk_bytes: u64,
    pub need_mem_bytes: u64,
    pub docker: DockerFacts,
    pub remote: RemoteFacts,
}

/// A passed preflight, with what `run` needs to continue.
pub struct Preflighted {
    pub report: PreflightReport,
    dataset: Dataset,
    remote_run_root: String,
}

fn invalid(detail: impl Into<String>) -> OpError {
    OpError::new(OpKind::InvalidPlan, detail)
}

fn remote_of(plan: &RunnerPlanV1) -> Result<&RemotePlanV1, OpError> {
    plan.remote
        .as_ref()
        .ok_or_else(|| invalid("the plan has no `remote` section"))
}

fn task_row<'d>(dataset: &'d Dataset, trial: &PlannedTrial) -> Result<&'d Task, OpError> {
    dataset
        .task
        .iter()
        .find(|t| t.name == trial.identity.task)
        .ok_or_else(|| {
            OpError::new(
                OpKind::PinDrift,
                format!("task {} is not in the pinned dataset", trial.identity.task),
            )
        })
}

/// The plan must say exactly what the pinned dataset says: image, digest, limits, deadlines.
fn check_trial_against_pin(trial: &PlannedTrial, row: &Task) -> Result<(), OpError> {
    let same_deadlines = whole_seconds("agent_timeout_sec", row.agent_timeout_sec)
        .map_err(invalid)?
        == trial.agent_timeout_sec
        && whole_seconds("verifier_timeout_sec", row.verifier_timeout_sec).map_err(invalid)?
            == trial.verifier_timeout_sec;
    if trial.image == row.image
        && trial.image_digest == row.image_digest
        && trial.cpus == row.cpus
        && trial.memory_mb == row.memory_mb
        && same_deadlines
    {
        Ok(())
    } else {
        Err(OpError::new(
            OpKind::PinDrift,
            format!(
                "the plan's image, digest, limits or deadlines for {} differ from the pinned dataset",
                row.name
            ),
        ))
    }
}

/// The official assets one oracle trial uploads must exist, and the layout must be the single
/// service this slice supports.
fn check_task_layout(checkout: &Path, task: &str) -> Result<(), OpError> {
    let dir = checkout.join(task);
    if dir.join("environment/docker-compose.yaml").exists()
        || dir.join("environment/docker-compose.yml").exists()
    {
        return Err(OpError::new(
            OpKind::Unsupported,
            format!("unsupported_task_layout: {task} declares a Compose file"),
        ));
    }
    for asset in ["solution/solve.sh", "tests/test.sh"] {
        if !dir.join(asset).is_file() {
            return Err(OpError::new(
                OpKind::PinDrift,
                format!("task {task} has no {asset} in the checkout"),
            ));
        }
    }
    Ok(())
}

/// Shard groups of the selected tasks, in dataset order: `(shard name, tasks, bytes)` where
/// bytes sum the unique images of the selected tasks of the shard. Tasks outside every shard
/// form an `unsharded` group.
fn selected_groups(dataset: &Dataset, tasks: &BTreeSet<String>) -> Vec<(String, Vec<String>, u64)> {
    let bytes_of = |names: &[String]| -> u64 {
        let images: BTreeMap<&str, u64> = dataset
            .task
            .iter()
            .filter(|t| names.contains(&t.name))
            .map(|t| (t.image.as_str(), t.image_bytes))
            .collect();
        images.values().sum()
    };
    let mut groups = Vec::new();
    let mut placed = BTreeSet::new();
    for shard in &dataset.shard {
        let names: Vec<String> = shard
            .tasks
            .iter()
            .filter(|t| tasks.contains(*t))
            .cloned()
            .collect();
        if names.is_empty() {
            continue;
        }
        placed.extend(names.iter().cloned());
        let bytes = bytes_of(&names);
        groups.push((shard.name.clone(), names, bytes));
    }
    let rest: Vec<String> = tasks.difference(&placed).cloned().collect();
    if !rest.is_empty() {
        let bytes = bytes_of(&rest);
        groups.push(("unsharded".to_string(), rest, bytes));
    }
    groups
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Read-only readiness checks. Nothing is created, pulled or started.
///
/// # Errors
/// The first failing check, typed ([`OpKind::exit_code`] maps it to a process status).
pub fn preflight(plan: &RunnerPlanV1, exec: &dyn CommandExecutor) -> Result<Preflighted, OpError> {
    plan.validate().map_err(invalid)?;
    if plan.trials.is_empty() {
        return Err(invalid("the plan has no trials"));
    }
    let remote_plan = remote_of(plan)?;
    if plan.mode == RunnerMode::Harness {
        let socket = remote_plan
            .socket_path
            .as_deref()
            .ok_or_else(|| invalid("Harness plan has no socket_path"))?;
        validate_socket(Path::new(socket))
            .map_err(|e| OpError::new(OpKind::Unsupported, format!("{e:?}")))?;
    }
    validate_hosts(
        &remote_plan.docker_host,
        &remote_plan.laptop_host,
        &remote_plan.allowed_docker_hosts,
    )?;
    let mut keys = BTreeSet::new();
    for trial in &plan.trials {
        if !keys.insert(trial_key(&trial.identity)) {
            return Err(invalid(format!(
                "trials {} slot {} retry {} collide on one directory key",
                trial.identity.task, trial.identity.slot, trial.identity.retry
            )));
        }
    }

    // Pin: the hub checkout and the plan against #3620's dataset library.
    let dataset = dataset::load(Path::new(&remote_plan.dataset_path)).map_err(|e| {
        OpError::new(
            OpKind::PinDrift,
            format!("{}: {e}", remote_plan.dataset_path),
        )
    })?;
    let checkout = Path::new(&remote_plan.checkout_dir);
    let drift = pin::checkout(checkout, &dataset.dataset.commit, &dataset.task)
        .map_err(|e| OpError::new(OpKind::PinDrift, e.to_string()))?;
    if !drift.is_empty() {
        return Err(OpError::new(
            OpKind::PinDrift,
            format!(
                "{} drift line(s) from the pinned dataset: {}",
                drift.len(),
                drift.iter().take(5).cloned().collect::<Vec<_>>().join("; ")
            ),
        ));
    }
    let mut tasks = BTreeSet::new();
    for trial in &plan.trials {
        check_trial_against_pin(trial, task_row(&dataset, trial)?)?;
        if tasks.insert(trial.identity.task.clone()) {
            check_task_layout(checkout, &trial.identity.task)?;
        }
        pinned_ref(&trial.image, &trial.image_digest)?;
    }

    // Docker, Compose and the platform.
    let docker = DockerClient {
        exec,
        host: &remote_plan.docker_host,
    };
    let facts = docker.facts(secs(PREFLIGHT_TIMEOUT_SEC))?;
    if facts.os_type != "linux" || !matches!(facts.architecture.as_str(), "x86_64" | "amd64") {
        return Err(OpError::new(
            OpKind::UnsupportedPlatform,
            format!(
                "the Docker host is {}/{}, not linux/amd64",
                facts.os_type, facts.architecture
            ),
        ));
    }

    // The laptop: reachable, linux/amd64, capacity measured.
    let remote = Remote {
        exec,
        host: &remote_plan.laptop_host,
    };
    let laptop = probe(&remote, &facts.root_dir, secs(PREFLIGHT_TIMEOUT_SEC))?;
    if laptop.os != "Linux" || !matches!(laptop.arch.as_str(), "x86_64" | "amd64") {
        return Err(OpError::new(
            OpKind::UnsupportedPlatform,
            format!(
                "the laptop is {}/{}, not Linux x86_64",
                laptop.os, laptop.arch
            ),
        ));
    }
    let concurrency = remote_plan.effective_concurrency();
    let groups = selected_groups(&dataset, &tasks);
    let largest = groups.iter().map(|g| g.2).max().unwrap_or(0);
    let need_disk = largest.saturating_add(DISK_HEADROOM_BYTES);
    let largest_trial_mem = plan
        .trials
        .iter()
        .map(|t| t.memory_mb.saturating_mul(1 << 20))
        .max()
        .unwrap_or(0);
    let need_mem = u64::from(concurrency)
        .saturating_mul(MEM_PER_TRIAL_BYTES)
        .saturating_add(MEM_RESERVE_BYTES)
        .max(largest_trial_mem.saturating_add(MEM_RESERVE_BYTES));
    match laptop.disk_free_bytes {
        None => {
            return Err(OpError::new(
                OpKind::CapacityUnmeasured,
                "free disk on the laptop could not be measured",
            ));
        }
        Some(free) if free < need_disk => {
            return Err(OpError::new(
                OpKind::DiskInsufficient,
                format!(
                    "{free} bytes free, {need_disk} needed (largest selected shard {largest} plus 10 GiB)"
                ),
            ));
        }
        Some(_) => {}
    }
    match laptop.mem_available_bytes {
        None => {
            return Err(OpError::new(
                OpKind::CapacityUnmeasured,
                "available memory on the laptop could not be measured",
            ));
        }
        Some(free) if free < need_mem => {
            return Err(OpError::new(
                OpKind::MemoryInsufficient,
                format!(
                    "{free} bytes available, {need_mem} needed ({concurrency} x 4 GiB plus 4 GiB)"
                ),
            ));
        }
        Some(_) => {}
    }
    let remote_run_root = format!(
        "{}/{REMOTE_STATE_DIR}/{}",
        laptop.home.trim_end_matches('/'),
        super::docker::short_hash(&[&plan.run_id])
    );
    validate_remote_path(&remote_run_root)?;
    Ok(Preflighted {
        report: PreflightReport {
            run_id: plan.run_id.clone(),
            docker_host: remote_plan.docker_host.clone(),
            tasks: tasks.len(),
            trials: plan.trials.len(),
            concurrency,
            largest_shard_bytes: largest,
            need_disk_bytes: need_disk,
            need_mem_bytes: need_mem,
            docker: facts,
            remote: laptop,
        },
        dataset,
        remote_run_root,
    })
}

// ---- running -------------------------------------------------------------------------------

/// The laptop memory not yet promised to a running trial. A trial whose declared limit does not
/// fit waits; its limit is never lowered.
pub(crate) struct MemoryBudget {
    free: Mutex<u64>,
    changed: Condvar,
}

impl MemoryBudget {
    pub(crate) const fn new(bytes: u64) -> Self {
        Self {
            free: Mutex::new(bytes),
            changed: Condvar::new(),
        }
    }

    pub(crate) fn acquire(&self, bytes: u64) {
        let mut free = self
            .free
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *free < bytes {
            free = self
                .changed
                .wait(free)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *free -= bytes;
    }

    pub(crate) fn release(&self, bytes: u64) {
        *self
            .free
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += bytes;
        self.changed.notify_all();
    }
}

/// One trial's line in `summary.json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrialSummary {
    pub task: String,
    pub slot: u32,
    pub retry: u32,
    pub passed: bool,
    pub reward_valid: bool,
    pub reward: Option<f64>,
    /// Snake_case kind of the cause when the reward is invalid.
    pub invalid_cause: Option<String>,
    pub collected: bool,
    pub cleaned: bool,
    /// A laptop log directory cleanup kept because its evidence was not collected.
    pub kept_evidence: Option<String>,
    /// `false` for an infrastructure row that a replacement attempt followed.
    pub final_attempt: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunSummary {
    pub run_id: String,
    pub concurrency: u32,
    /// Statistical slots, not attempts.
    pub total: usize,
    /// Slots whose final attempt passed.
    pub passed: usize,
    /// Fresh replacement attempts after infrastructure interruptions.
    pub replacements: u32,
    pub model_sessions: u32,
    pub trials: Vec<TrialSummary>,
}

impl RunSummary {
    /// Every trial passed with a valid reward and a clean teardown.
    #[must_use]
    pub fn all_passed_and_clean(&self) -> bool {
        self.passed == self.total && self.trials.iter().all(|t| t.cleaned)
    }
}

/// Marker file whose appearance in the run directory cancels the run (the daemon's job stop
/// handling writes it; SIGTERM wiring belongs to the job integration, R6).
pub const CANCEL_MARKER: &str = "CANCEL";

/// Signal before the enclosing scope joins its watcher, including during unwinding.
struct FinishOnDrop<'a> {
    finished: &'a AtomicBool,
    watcher: std::thread::Thread,
}

impl Drop for FinishOnDrop<'_> {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::SeqCst);
        self.watcher.unpark();
    }
}

fn panic_error(context: &str, payload: &(dyn std::any::Any + Send)) -> OpError {
    let detail = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("non-string panic payload");
    OpError::new(
        OpKind::RunnerPanicked,
        format!("{context} panicked: {detail}"),
    )
}

fn journal_error(error: &JournalError) -> OpError {
    OpError::new(OpKind::Journal, error.to_string())
}

fn kind_name(outcome: &TrialOutcomeV1) -> Option<String> {
    outcome
        .invalid_cause()
        .and_then(|e| serde_json::to_value(e.kind).ok())
        .and_then(|v| v.as_str().map(str::to_string))
}

/// Builds R2's remote Docker environment for the lifecycle, adopting a dead runner's.
struct RemoteFactory<'a> {
    exec: &'a dyn CommandExecutor,
    remote_plan: &'a RemotePlanV1,
    remote_run_root: &'a str,
    run_dir: &'a Path,
}

impl RemoteFactory<'_> {
    fn work_dir(&self, trial: &PlannedTrial) -> PathBuf {
        self.run_dir.join("trials").join(trial_key(&trial.identity))
    }
}

impl EnvFactory for RemoteFactory<'_> {
    fn open<'e>(
        &'e self,
        trial: &'e PlannedTrial,
        adopt: Adopt<'_>,
    ) -> Result<Box<dyn TrialEnvironment + 'e>, OpError> {
        let mut env = RemoteEnvironment::new(
            EnvContext {
                exec: self.exec,
                docker_host: &self.remote_plan.docker_host,
                ssh_host: &self.remote_plan.laptop_host,
            },
            trial,
            self.remote_run_root,
            self.work_dir(trial).join("compose.json"),
        )?;
        match adopt {
            Adopt::Fresh => {}
            Adopt::Stale => {
                let facts = stale_env_facts(&env.names().project);
                env.adopt(&facts);
            }
            Adopt::Facts(facts) => env.adopt(facts),
        }
        Ok(Box::new(env))
    }
}

/// The identity a journal must match to be resumed.
fn run_identity(plan: &RunnerPlanV1, dataset_path: &Path) -> Result<RunIdentity, OpError> {
    let plan_bytes =
        serde_json::to_vec(plan).map_err(|e| OpError::new(OpKind::InvalidPlan, e.to_string()))?;
    let dataset_bytes = fs::read(dataset_path)
        .map_err(|e| OpError::new(OpKind::Io, format!("{}: {e}", dataset_path.display())))?;
    Ok(RunIdentity {
        plan_sha256: sha256_of(&plan_bytes),
        runner_digest: stamp::stamp()
            .map_or_else(|why| format!("unstamped:{why}"), |s| s.evaluator_digest),
        dataset_sha256: sha256_of(&dataset_bytes),
    })
}

/// Pull every image of the shard's selected tasks by pinned digest; any pin failure is fatal
/// before the shard runs a trial.
fn pull_shard(
    docker: &DockerClient<'_>,
    plan: &RunnerPlanV1,
    names: &[String],
) -> Result<(), OpError> {
    let mut pulled = BTreeSet::new();
    for trial in plan
        .trials
        .iter()
        .filter(|t| names.contains(&t.identity.task))
    {
        let reference = pinned_ref(&trial.image, &trial.image_digest)?;
        if pulled.insert(reference.clone()) {
            docker.pull_pinned(&reference, &trial.image_digest, secs(PULL_TIMEOUT_SEC))?;
        }
    }
    Ok(())
}

/// Run an oracle plan: preflight, then shard by shard pull and bounded trials, every step
/// journaled (R5). Returns the summary (also written as `summary.json` under
/// `<output_root>/<run_id>/`). Running the same plan again over the same output root resumes the
/// run: durable results are reused, interrupted attempts are reconciled, and only the slots
/// still missing are run.
///
/// # Errors
/// A failed preflight, a pin or pull failure (fatal before any trial of the shard), an
/// unwritable output directory, or a journal that is owned by another runner or belongs to
/// another plan, build or dataset, or an internal runner panic. Ordinary per-trial failures
/// are not errors: they are typed outcomes.
pub fn run(plan: &RunnerPlanV1, exec: &dyn CommandExecutor) -> Result<RunSummary, OpError> {
    run_with_cancel(plan, exec, &CancelFlag::new())
}

/// [`run`] with an external cancellation flag (also set by the `CANCEL` marker file).
///
/// # Errors
/// As [`run`].
pub fn run_with_cancel(
    plan: &RunnerPlanV1,
    exec: &dyn CommandExecutor,
    cancel: &CancelFlag,
) -> Result<RunSummary, OpError> {
    let ready = preflight(plan, exec)?;
    let remote_plan = remote_of(plan)?;
    let concurrency = ready.report.concurrency;
    let run_dir = Path::new(&remote_plan.output_root).join(&plan.run_id);
    let checkout = Path::new(&remote_plan.checkout_dir);
    let docker = DockerClient {
        exec,
        host: &remote_plan.docker_host,
    };
    let wall = EpochPacer;
    let client = if plan.mode == RunnerMode::Harness {
        let socket = remote_plan
            .socket_path
            .as_deref()
            .ok_or_else(|| invalid("Harness plan has no socket_path"))?;
        Some(
            BenchClient::open(Path::new(socket), ClientConfig::default())
                .map_err(|e| OpError::new(OpKind::Unsupported, format!("{e:?}")))?,
        )
    } else {
        None
    };
    let sessions = client.as_ref().map(|client| BenchSessions {
        client,
        wall: &wall,
    });
    let identity = run_identity(plan, Path::new(&remote_plan.dataset_path))?;
    let journal = JournalStore::open(
        &run_dir,
        &plan.run_id,
        &identity,
        u64::try_from(wall.now().as_millis()).unwrap_or(u64::MAX),
    )
    .map_err(|e| journal_error(&e))?;
    let factory = RemoteFactory {
        exec,
        remote_plan,
        remote_run_root: &ready.remote_run_root,
        run_dir: &run_dir,
    };
    // The daemon's stop marker (R6): once it exists no further shard, trial or retry starts;
    // sessions already running finish and are graded.
    let stop_file = remote_plan.stop_file.as_deref().map(Path::new);
    let stop_requested = || stop_file.is_some_and(Path::exists);
    let paths = |trial: &PlannedTrial| TrialPaths {
        solution_dir: checkout.join(&trial.identity.task).join("solution"),
        tests_dir: checkout.join(&trial.identity.task).join("tests"),
        work_dir: factory.work_dir(trial),
    };
    let lifecycle = Lifecycle {
        journal: &journal,
        envs: &factory,
        wall: &wall,
        sessions: sessions.as_ref().map(|s| s as &dyn SessionPort),
        cancel,
        mode: plan.mode,
        timeouts: &plan.timeouts,
        paths: &paths,
        stop_file,
    };
    let tasks: BTreeSet<String> = plan
        .trials
        .iter()
        .map(|t| t.identity.task.clone())
        .collect();
    let budget_bytes = ready
        .report
        .remote
        .mem_available_bytes
        .unwrap_or(0)
        .saturating_sub(MEM_RESERVE_BYTES);
    let memory = MemoryBudget::new(budget_bytes);
    let finished = AtomicBool::new(false);
    let mut failure: Option<OpError> = None;

    catch_unwind(AssertUnwindSafe(|| {
        std::thread::scope(|scope| {
            // Cancellation watcher: the flag or the marker file stops the run, sessions first.
            let watcher = scope.spawn(|| {
                while !finished.load(Ordering::SeqCst) {
                    if cancel.is_set() || run_dir.join(CANCEL_MARKER).exists() {
                        let _ = lifecycle.cancel_run();
                        break;
                    }
                    std::thread::park_timeout(Duration::from_millis(200));
                }
            });
            let _completion = FinishOnDrop {
                finished: &finished,
                watcher: watcher.thread().clone(),
            };
            'shards: for (_, names, _) in selected_groups(&ready.dataset, &tasks) {
                if cancel.is_set() || stop_requested() {
                    break;
                }
                if let Err(error) = pull_shard(&docker, plan, &names) {
                    failure = Some(error);
                    break 'shards;
                }
                let queue: Mutex<VecDeque<&PlannedTrial>> = Mutex::new(
                    plan.trials
                        .iter()
                        .filter(|t| names.contains(&t.identity.task))
                        .collect(),
                );
                let errors: Mutex<Vec<OpError>> = Mutex::new(Vec::new());
                std::thread::scope(|workers| {
                    for _ in 0..concurrency {
                        workers.spawn(|| {
                            loop {
                                if stop_requested() {
                                    break;
                                }
                                let next = queue
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .pop_front();
                                let Some(trial) = next else { break };
                                let need = trial.memory_mb.saturating_mul(1 << 20);
                                memory.acquire(need);
                                // The wait for memory may have outlasted the budget.
                                if stop_requested() {
                                    memory.release(need);
                                    break;
                                }
                                // A trial panic must release its reservation before other workers
                                // can finish. Stop this worker rather than reuse unwound state.
                                let result =
                                    catch_unwind(AssertUnwindSafe(|| lifecycle.run_slot(trial)));
                                memory.release(need);
                                let result = match result {
                                    Ok(result) => result,
                                    Err(payload) => {
                                        errors
                                            .lock()
                                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                                            .push(panic_error(
                                                &format!("trial {}", trial_key(&trial.identity)),
                                                &*payload,
                                            ));
                                        break;
                                    }
                                };
                                match result {
                                    Ok(_) => {}
                                    // A halt for the operator's cancel is the cancel working.
                                    Err(LifecycleError::Halted(_)) if cancel.is_set() => {}
                                    Err(error) => errors
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .push(OpError::new(OpKind::Journal, error.to_string())),
                                }
                            }
                        });
                    }
                });
                if let Some(error) = errors
                    .into_inner()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .into_iter()
                    .next()
                {
                    failure = Some(error);
                    break 'shards;
                }
            }
        })
    }))
    .map_err(|payload| panic_error("run", &*payload))?;
    if let Some(error) = failure {
        return Err(error);
    }
    // A cancel that arrived after the last step boundary is still honoured: sessions first.
    if (cancel.is_set() || run_dir.join(CANCEL_MARKER).exists()) && !journal.cancelled() {
        lifecycle
            .cancel_run()
            .map_err(|e| OpError::new(OpKind::Journal, e.to_string()))?;
    }

    // The run's laptop directory is ours and, once every trial removed its own, empty.
    let _ = Remote {
        exec,
        host: &remote_plan.laptop_host,
    }
    .sh(
        &format!(
            "rmdir -- {} 2>/dev/null; exit 0",
            super::transport::posix_quote(&ready.remote_run_root)
        ),
        secs(COPY_TIMEOUT_SEC),
    );

    let summary = summarize(
        plan,
        &journal.snapshot(),
        &ready.remote_run_root,
        concurrency,
    );
    let bytes =
        serde_json::to_vec_pretty(&summary).map_err(|e| OpError::new(OpKind::Io, e.to_string()))?;
    write_atomic(&run_dir.join("summary.json"), &bytes)
        .map_err(|e| OpError::new(OpKind::Io, e.to_string()))?;
    Ok(summary)
}

/// The summary of the journal: one line per attempt (infrastructure rows and their replacement
/// are distinct lines), one result per slot.
fn summarize(
    plan: &RunnerPlanV1,
    journal: &RunJournalV1,
    remote_run_root: &str,
    concurrency: u32,
) -> RunSummary {
    let mut trials = Vec::new();
    let mut slots = 0_usize;
    let mut passed = 0_usize;
    let mut replacements = 0_u32;
    let mut model_sessions = 0_u32;
    for planned in &plan.trials {
        let attempts = journal.slot(&planned.identity.task, planned.identity.slot);
        slots += 1;
        if attempts.last().is_some_and(|r| r.draft.passed()) {
            passed += 1;
        }
        for record in attempts {
            let outcome = &record.draft;
            replacements += u32::from(record.replaces.is_some());
            model_sessions += outcome.model_sessions;
            let kept = record
                .env
                .as_ref()
                .filter(|f| f.dirs_created && !f.collected)
                .and_then(|_| TrialNames::new(&record.planned.identity, remote_run_root).ok())
                .map(|names| names.trial_dir);
            trials.push(TrialSummary {
                task: outcome.identity.task.clone(),
                slot: outcome.identity.slot,
                retry: outcome.identity.retry,
                passed: outcome.passed(),
                reward_valid: outcome.reward_valid,
                reward: outcome.reward,
                invalid_cause: kind_name(outcome),
                collected: outcome.collection.collected,
                cleaned: outcome.cleanup.cleaned,
                kept_evidence: kept,
                final_attempt: record.replaced_by.is_none(),
            });
        }
    }
    RunSummary {
        run_id: plan.run_id.clone(),
        concurrency,
        total: slots,
        passed,
        replacements,
        model_sessions,
        trials,
    }
}
