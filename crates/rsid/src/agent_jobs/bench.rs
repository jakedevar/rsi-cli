//! #3181 (program #3135 M3 S5): the `bench` job kind.
//!
//! The appointed manager or an Epic lead runs one pinned benchmark configuration
//! with one call and gets one typed result (`result.bench`) and one run record
//! (`~/.rsi/bench/<run>/record.json`). The run cannot spend beyond the budget it
//! was admitted with, and it loads none of the hub's merge-queue CPUs: the unit
//! only runs the stamped Rust TB2 runner (`rsi-eval bench tb2 run`, #3681 R3b), which drives the
//! pinned task images on the laptop's Docker over ssh. No Python, Harbor or venv is involved.
//!
//! Lifecycle:
//! * **Submit** ([`admit`], then [`submit_queued`]): the gates run, the config's
//!   digest and the worktree HEAD are recorded, and the job is stored `queued`.
//! * **Start** ([`start_queued`], every jobs-loop tick): the gates run again, the
//!   run is opened (its adapter socket bound) and the unit is launched.
//! * **Running** ([`tick`]): the run's spend is read through M4's
//!   `run_budget_status`; once the run's budget is spent the stop is written for
//!   the transport, which launches no further shard. Every trial start is also
//!   refused by the adapter's own budget check (#3178).
//! * **Finish** ([`finalize`]): the runner's trial outcomes are joined with the
//!   daemon's `bench_tasks` rows into the `BenchRunRecord`, written beside the
//!   run, and summarised as the job's typed result. A run whose job ended has its
//!   adapter socket closed ([`reap_runs`]).

mod config;
mod overlay;
mod record;
mod transport;

use super::{AgentJobResultV1, AgentJobV1, JobKind, JobParams, JobState, JobWake, NewAgentJob};
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::bench::{BenchRunRow, BenchRunTask, NewBenchRun};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use config::ResolvedRun;
use rsi_common::agent_jobs::{
    BENCH_CONFIG_CHANGED, BENCH_CONFIG_INVALID, BENCH_CONFIG_UNCOMMITTED, BENCH_DISABLED,
    BENCH_DOCKER_HOST_NOT_ALLOWED, BENCH_LAPTOP_HOST_MISMATCH, BENCH_OVERLAY_INVALID,
    BENCH_OVERLAY_LEAK_AUDIT_FAILED, BENCH_PREFLIGHT_FAILED, BENCH_PROJECT_UNKNOWN,
    BENCH_RUN_OPEN_FAILED, BENCH_SPEND_CAP_OFF, BENCH_TRANSPORT_FAILED, BenchJobParams,
    JOB_HELD_FOR_BENCH, JOB_LAUNCH_FAILED,
};
use rsi_common::bench::BenchRunState;
use rsi_common::bench_runner::{RunnerMode, RunnerPlanV1, RunnerSourceManifest};
use rsi_common::harness_manager_v2::ManagerLaunchChoiceV2;
use rsi_common::openrouter_spend::{
    OpenRouterSpendCapV1, RunBudgetStatusV1, SpendCapState, admit_run_budget, run_budget_status,
};
use rsi_common::types::SessionProvider;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use transport::{
    BenchRefusal, PlanHosts, PlanPaths, RunnerBinary, RunnerFailure, evaluate_preflight,
    runner_argv, runner_plan, stamp_argv,
};
use uuid::Uuid;

pub(crate) use transport::exit_refusal;

/// The files the run reads from the job's worktree. All must be committed and
/// clean at the job's HEAD: the transport ships them to the laptop and the record
/// pins their digests.
pub(crate) const DATASET_PATH: &str = "eval/bench/tb2/dataset.toml";
pub(crate) const SPLIT_PATH: &str = "eval/bench/tb2/split.json";
/// Longest run config / dataset file the daemon reads.
const MAX_INPUT_BYTES: u64 = 4 * 1024 * 1024;
/// The unit's lifetime cap (five days): a k=5 run can take a day or more.
pub(crate) const BENCH_RUNTIME_MAX_SECS: u64 = 5 * 24 * 3600;
/// How long the runner's preflight may take (ssh, Docker and Compose probes, a measurement).
const PREFLIGHT_TIMEOUT_SECS: u64 = 180;
/// How long the runner's stamp may take (it only prints constants).
const STAMP_TIMEOUT_SECS: u64 = 30;

/// The operator settings a gate reads, as of now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BenchSettings {
    pub enabled: bool,
    pub max_concurrency: u32,
    pub laptop_host: String,
    pub docker_hosts: Vec<String>,
}

/// What the bench job needs from the rest of the daemon. One seam, so the
/// production ports and a test double differ only in what they pass in.
#[async_trait]
pub(crate) trait BenchJobPorts: Send + Sync {
    /// The operator settings (`bench.*`).
    fn settings(&self) -> BenchSettings;
    /// The OpenRouter spend cap now (M4), refreshed when stale.
    async fn spend_cap(&self) -> OpenRouterSpendCapV1;
    /// The launches the project's grant allows (`allowed_launches`).
    fn allowed_launches(&self, store: &Store, project: Uuid) -> Result<Vec<ManagerLaunchChoiceV2>>;
    /// #3417: why the project's provider grant, if any, did not admit `wanted`
    /// (the grant, its spend and the screen that raises it).
    fn provider_grant_hint(
        &self,
        _store: &Store,
        _project: Uuid,
        _wanted: &ManagerLaunchChoiceV2,
    ) -> Option<String> {
        None
    }
    /// #3417: the USD left under the project's active grant that admits
    /// `wanted`; a run under it is bounded by it and needs no armed account
    /// spend cap.
    fn provider_grant_remaining(
        &self,
        _store: &Store,
        _project: Uuid,
        _wanted: &ManagerLaunchChoiceV2,
    ) -> Option<f64> {
        None
    }
    /// The installed Rust runner (`rsi-eval`, a sibling of this daemon), verified by its
    /// build stamp. Never a copy from `PATH`, never built at trial time.
    async fn runner(&self) -> std::result::Result<RunnerBinary, String>;
    /// The runner's read-only preflight (`rsi-eval bench tb2 preflight --plan`): the pin and the
    /// hosts, Docker, Compose and platform, and the laptop's measured disk and memory.
    async fn preflight(
        &self,
        runner: &RunnerBinary,
        plan: &RunnerPlanV1,
    ) -> std::result::Result<(), RunnerFailure>;
    /// Record an admitted run and bind its adapter socket (or rebind it when the
    /// run exists and is active). Returns the socket path.
    async fn open_run(&self, run: NewBenchRun) -> Result<PathBuf>;
    /// `~/.rsi/bench`.
    fn bench_root(&self) -> PathBuf;
    /// #3191: the Terminal-Bench checkout of the pinned commit on this host, when
    /// there is one. The overlay's leak audit adds the checkout's test file names and
    /// expected-output literals to its terms; without one it audits task names only.
    fn audit_checkout(&self, commit: &str) -> Option<PathBuf> {
        let plain = commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit());
        let dir = pinned_checkout_dir(&self.bench_root(), commit);
        (plain && dir.join(".git").exists()).then_some(dir)
    }
    /// #3191: run the native leak audit over a run overlay and its files.
    /// `Err` is a hit or a failure to run: both refuse the run (fail closed).
    async fn leak_audit(
        &self,
        cwd: &Path,
        plan: overlay::AuditPlan,
    ) -> std::result::Result<(), String> {
        let cwd = cwd.to_path_buf();
        tokio::task::spawn_blocking(move || overlay::audit(&cwd, &plan))
            .await
            .map_err(|error| format!("the leak audit did not finish: {error}"))?
    }
}

/// The daemon's own ports.
pub(crate) struct DaemonBenchPorts {
    config: Arc<crate::config::RuntimeConfig>,
    store: Arc<rsid_core::store_lock::Mutex<Store>>,
}

impl DaemonBenchPorts {
    pub(crate) fn new(
        config: Arc<crate::config::RuntimeConfig>,
        store: Arc<rsid_core::store_lock::Mutex<Store>>,
    ) -> Self {
        Self { config, store }
    }
}

/// How a bounded child ended.
struct Bounded {
    exit: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Run `argv` under the store's bounded runner (process-group containment without the Git no-escape
/// seccomp filter, since Docker's `ssh://` helper calls `setsid` (#3890), an
/// output cap and a wall deadline). A hung ssh inside the runner is killed at
/// `timeout_secs`, so a blocking caller never waits on the laptop.
fn run_bounded(argv: &[String], timeout_secs: u64) -> std::result::Result<Bounded, String> {
    let (program, args) = argv.split_first().ok_or("empty command")?;
    let mut command = std::process::Command::new(program);
    command.args(args).stdin(std::process::Stdio::null());
    let output = crate::sandbox::git_worktree::capture_command_bounded_group_for(
        command,
        "bench runner",
        std::time::Duration::from_secs(timeout_secs),
    )
    .map_err(|error| error.to_string())?;
    Ok(Bounded {
        exit: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// `<bench root>/tb2/<commit>`: the pinned Terminal-Bench checkout on the hub (#3620's layout,
/// also the leak audit's). The runner's pin check refuses a run whose checkout is missing or
/// differs from `dataset.toml`.
pub(crate) fn pinned_checkout_dir(bench_root: &Path, commit: &str) -> PathBuf {
    bench_root.join("tb2").join(commit)
}

#[async_trait]
impl BenchJobPorts for DaemonBenchPorts {
    fn settings(&self) -> BenchSettings {
        BenchSettings {
            enabled: self.config.bench_enabled(),
            max_concurrency: self.config.bench_max_concurrency(),
            laptop_host: self.config.bench_laptop_host(),
            docker_hosts: self.config.bench_docker_hosts(),
        }
    }

    async fn spend_cap(&self) -> OpenRouterSpendCapV1 {
        crate::openrouter_spend::SpendEnforcer::production(
            Arc::clone(&self.config),
            Arc::clone(&self.store),
        )
        .state(true)
        .await
    }

    fn allowed_launches(&self, store: &Store, project: Uuid) -> Result<Vec<ManagerLaunchChoiceV2>> {
        crate::topology::agent::project_allowed_launches(store, Some(project))
    }

    fn provider_grant_hint(
        &self,
        store: &Store,
        project: Uuid,
        wanted: &ManagerLaunchChoiceV2,
    ) -> Option<String> {
        store.provider_grant_refusal_hint(project, wanted)
    }

    fn provider_grant_remaining(
        &self,
        store: &Store,
        project: Uuid,
        wanted: &ManagerLaunchChoiceV2,
    ) -> Option<f64> {
        store
            .provider_grant_remaining_usd(project, wanted)
            .ok()
            .flatten()
    }

    async fn runner(&self) -> std::result::Result<RunnerBinary, String> {
        tokio::task::spawn_blocking(|| {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let path = transport::sibling_runner(&exe)?;
            let stamp = run_bounded(&stamp_argv(&path), STAMP_TIMEOUT_SECS)?;
            if stamp.stdout.trim().is_empty() {
                return Err(format!(
                    "the runner printed no stamp: {}",
                    stamp.stderr.lines().last().unwrap_or("no output")
                ));
            }
            RunnerBinary::from_stamp(path, &stamp.stdout)
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn preflight(
        &self,
        runner: &RunnerBinary,
        plan: &RunnerPlanV1,
    ) -> std::result::Result<(), RunnerFailure> {
        let unrunnable = |why: String| RunnerFailure {
            exit: None,
            detail: why,
        };
        let root = self.bench_root();
        let plan_json = serde_json::to_vec(plan).map_err(|e| unrunnable(e.to_string()))?;
        let argv_runner = runner.path.clone();
        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            crate::bench::private_dir(&root).map_err(|e| unrunnable(e.to_string()))?;
            // Removed when it drops, however the preflight ends. It holds no credentials.
            let mut plan_file = tempfile::Builder::new()
                .prefix("preflight-")
                .suffix(".json")
                .tempfile_in(&root)
                .map_err(|e| unrunnable(e.to_string()))?;
            plan_file
                .write_all(&plan_json)
                .and_then(|()| plan_file.flush())
                .map_err(|e| unrunnable(e.to_string()))?;
            let argv = runner_argv(&argv_runner, "preflight", plan_file.path());
            let ran = run_bounded(&argv, PREFLIGHT_TIMEOUT_SECS).map_err(unrunnable)?;
            if ran.exit == Some(0) {
                Ok(())
            } else {
                Err(RunnerFailure::exited(ran.exit, &ran.stderr))
            }
        })
        .await
        .map_err(|e| RunnerFailure {
            exit: None,
            detail: e.to_string(),
        })?
    }

    async fn open_run(&self, run: NewBenchRun) -> Result<PathBuf> {
        let service = crate::bench::global().ok_or_else(|| {
            DaemonError::Process("the bench service is not installed".to_string())
        })?;
        let existing = self.store.lock().await.get_bench_run(run.id)?;
        match existing {
            Some(row) if row.state == BenchRunState::Active => service.bind_run(run.id).await,
            Some(_) => Err(DaemonError::PolicyDenied(format!(
                "{}: run {}",
                rsi_common::bench::BENCH_RUN_NOT_ACTIVE,
                run.id
            ))),
            None => service.open_run(run).await,
        }
    }

    fn bench_root(&self) -> PathBuf {
        crate::bench::global().map_or_else(crate::bench::default_root, |s| s.root.clone())
    }
}

static PORTS: OnceLock<Arc<dyn BenchJobPorts>> = OnceLock::new();

/// Install the daemon's ports once, at startup (beside the bench service).
pub(crate) fn install_ports(ports: Arc<dyn BenchJobPorts>) {
    let _ = PORTS.set(ports);
}

/// The daemon's ports, once installed.
pub(crate) fn installed_ports() -> Option<Arc<dyn BenchJobPorts>> {
    PORTS.get().cloned()
}

/// When a gate runs: at submit, or again when the queued job starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    Submit,
    Start,
}

/// A run config read from the worktree and resolved against the pinned dataset.
pub(crate) struct Inputs {
    pub config_text: String,
    pub dataset_text: String,
    pub split_text: String,
    pub resolved: ResolvedRun,
    /// #3191: the run overlay the config names, read from the same worktree.
    pub overlay: Option<overlay::LoadedOverlay>,
}

fn read_bounded(cwd: &Path, relative: &str) -> std::result::Result<String, BenchRefusal> {
    let path = cwd.join(relative);
    let invalid =
        |why: String| BenchRefusal::new(BENCH_CONFIG_INVALID, format!("{relative}: {why}"));
    let meta = std::fs::symlink_metadata(&path).map_err(|e| invalid(e.to_string()))?;
    if !meta.is_file() {
        return Err(invalid("not a regular file".into()));
    }
    if meta.len() > MAX_INPUT_BYTES {
        return Err(invalid(format!("larger than {MAX_INPUT_BYTES} bytes")));
    }
    std::fs::read_to_string(&path).map_err(|e| invalid(e.to_string()))
}

/// Read and resolve the run config at `cwd`.
pub(crate) fn read_inputs(cwd: &Path, config: &str) -> std::result::Result<Inputs, BenchRefusal> {
    let config_text = read_bounded(cwd, config)?;
    let dataset_text = read_bounded(cwd, DATASET_PATH)?;
    let split_text = read_bounded(cwd, SPLIT_PATH)?;
    let invalid = |why: String| BenchRefusal::new(BENCH_CONFIG_INVALID, why);
    let dataset = config::Dataset::parse(&dataset_text, &split_text).map_err(invalid)?;
    let file = config::RunFile::parse(&config_text).map_err(invalid)?;
    let resolved = file.resolve(&config_text, dataset).map_err(invalid)?;
    let overlay = resolved
        .file
        .overlay
        .as_deref()
        .map(|path| overlay::load(cwd, path))
        .transpose()
        .map_err(|error| BenchRefusal::new(BENCH_OVERLAY_INVALID, error.to_string()))?;
    Ok(Inputs {
        config_text,
        dataset_text,
        split_text,
        resolved,
        overlay,
    })
}

impl Inputs {
    /// The validated overlay, when the config names one that sets anything.
    pub(crate) fn run_overlay(&self) -> Option<&rsi_common::bench_overlay::BenchOverlay> {
        self.overlay
            .as_ref()
            .and_then(|loaded| loaded.overlay.as_ref())
    }
}

/// #3191: the dry-run description of a run config (no model is called, nothing is
/// opened): the effective prompt digest, tool list and knobs a run would launch
/// with. Reads the worktree at `cwd` only.
///
/// # Errors
/// The config's or overlay's typed refusal text.
pub fn dry_run_config(cwd: &Path, config: &str) -> std::result::Result<serde_json::Value, String> {
    let inputs = read_inputs(cwd, config).map_err(|refusal| refusal.text())?;
    let resolved = &inputs.resolved;
    let mut report = overlay::describe(inputs.run_overlay(), resolved.file.loop_knobs);
    if let Some(map) = report.as_object_mut() {
        map.insert("config".into(), serde_json::json!(config));
        map.insert(
            "overlay_path".into(),
            serde_json::json!(resolved.file.overlay),
        );
        map.insert("tasks".into(), serde_json::json!(resolved.tasks.len()));
    }
    Ok(report)
}

/// The worktree paths a run reads. The Rust runner reads nothing else from the worktree: no
/// adapter package, pin script or helper library (the pin check is the runner's own).
fn input_paths(config: &str) -> Vec<String> {
    vec![
        config.to_string(),
        DATASET_PATH.to_string(),
        SPLIT_PATH.to_string(),
    ]
}

/// What a passed admission carries to the launch.
pub(crate) struct Admitted {
    pub inputs: Inputs,
    pub concurrency: u32,
    pub laptop_host: String,
    /// The verified runner binary; its evaluator digest is the record's `adapter_tree_hash`.
    pub runner: RunnerBinary,
    pub head: String,
}

fn refuse_text(refusal: &BenchRefusal) -> DaemonError {
    match refusal.code {
        BENCH_CONFIG_INVALID | BENCH_CONFIG_UNCOMMITTED | BENCH_OVERLAY_INVALID => {
            DaemonError::InvalidParam(refusal.text())
        }
        _ => DaemonError::PolicyDenied(refusal.text()),
    }
}

impl From<BenchRefusal> for DaemonError {
    fn from(refusal: BenchRefusal) -> Self {
        refuse_text(&refusal)
    }
}

/// The submit and start gates (plan D3): `bench.enabled`; the config's docker host
/// is the laptop's and in `bench.docker_hosts`; for a model-driven run the launch
/// grant, an armed OpenRouter spend cap and the cap headroom for `budget_usd`; the
/// stamped runner and its preflight (pin, Docker, platform, capacity). A config that
/// launches no model (the oracle agent) skips the three model gates and may run with the
/// cap off.
///
/// # Errors
/// The first failing gate, typed.
pub(crate) async fn admit(
    ports: &dyn BenchJobPorts,
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    project: Option<Uuid>,
    cwd: &Path,
    params: &BenchJobParams,
    stage: Stage,
) -> std::result::Result<Admitted, BenchRefusal> {
    let settings = ports.settings();
    if !settings.enabled {
        return Err(BenchRefusal::new(
            BENCH_DISABLED,
            "benchmark jobs are off; the operator enables them with the setting bench.enabled, or by ticking bench on the provider grant at TUI: :manager board > p (Policy) > Provider grants",
        ));
    }
    // A missing or unreadable config is invalid; a present one must be committed and clean
    // before its content is trusted (an edited file is `uncommitted`, whatever it now says).
    read_bounded(cwd, &params.config)?;
    let head = {
        let cwd = cwd.to_path_buf();
        let paths = input_paths(&params.config);
        tokio::task::spawn_blocking(move || observe_custody(&cwd, &paths))
            .await
            .map_err(|e| BenchRefusal::new(BENCH_CONFIG_UNCOMMITTED, e.to_string()))??
    };
    let inputs = read_inputs(cwd, &params.config)?;
    let resolved = &inputs.resolved;
    if let Some(loaded) = &inputs.overlay {
        admit_overlay(ports, cwd, &head, loaded, &resolved.dataset.header.commit).await?;
    }
    let wanted_host = rsi_common::bench_settings::laptop_docker_host(&settings.laptop_host);
    if resolved.file.host != wanted_host {
        return Err(BenchRefusal::new(
            BENCH_LAPTOP_HOST_MISMATCH,
            format!(
                "the config runs on {} but bench.laptop_host is {}",
                resolved.file.host, settings.laptop_host
            ),
        ));
    }
    if !settings
        .docker_hosts
        .iter()
        .any(|h| h == &resolved.file.host)
    {
        return Err(BenchRefusal::new(
            BENCH_DOCKER_HOST_NOT_ALLOWED,
            format!(
                "{} is not in bench.docker_hosts; the operator adds it",
                resolved.file.host
            ),
        ));
    }
    let concurrency = resolved
        .file
        .concurrency
        .min(settings.max_concurrency)
        .max(1);
    if resolved.is_paid() {
        gate_paid_run(ports, store, project, resolved, params.budget_usd).await?;
    }
    let runner = ports.runner().await.map_err(|why| {
        BenchRefusal::new(BENCH_PREFLIGHT_FAILED, format!("runner_unavailable: {why}"))
    })?;
    let bench_root = ports.bench_root();
    let checkout_dir = pinned_checkout_dir(&bench_root, &resolved.dataset.header.commit);
    let dataset_path = cwd.join(DATASET_PATH);
    let mut plan = runner_plan(
        Uuid::nil(),
        resolved,
        &PlanHosts {
            laptop_host: &settings.laptop_host,
            docker_hosts: &settings.docker_hosts,
            concurrency,
            max_concurrency: settings.max_concurrency,
        },
        &PlanPaths {
            checkout_dir: &checkout_dir,
            dataset_path: &dataset_path,
            output_root: &bench_root,
        },
    )?;
    // No run is open at admission, so a paid run's adapter socket does not exist yet and the
    // Harness preflight would refuse every paid run (`bench socket .../0000...0/adapter.sock`).
    // The read-only checks here (pin, hosts, Docker, capacity) do not depend on the mode, so
    // they run on the oracle-shaped plan; the real run validates its own bound socket.
    if plan.mode == RunnerMode::Harness {
        plan.mode = RunnerMode::Oracle;
        if let Some(remote) = plan.remote.as_mut() {
            remote.socket_path = None;
        }
    }
    evaluate_preflight(ports.preflight(&runner, &plan).await)?;
    let _ = stage;
    Ok(Admitted {
        inputs,
        concurrency,
        laptop_host: settings.laptop_host,
        runner,
        head,
    })
}

/// #3191: the overlay and every file the audit reads are tracked and unmodified at the
/// HEAD the config was admitted on, and the leak audit finds nothing in them. A hit, or
/// an audit that cannot run, refuses the run.
async fn admit_overlay(
    ports: &dyn BenchJobPorts,
    cwd: &Path,
    head: &str,
    loaded: &overlay::LoadedOverlay,
    dataset_commit: &str,
) -> std::result::Result<(), BenchRefusal> {
    let mut paths = loaded.audited_files.clone();
    paths.extend(overlay::AUDIT_INPUTS.map(str::to_owned));
    let overlay_head = {
        let cwd = cwd.to_path_buf();
        tokio::task::spawn_blocking(move || observe_custody(&cwd, &paths))
            .await
            .map_err(|e| BenchRefusal::new(BENCH_CONFIG_UNCOMMITTED, e.to_string()))??
    };
    if overlay_head != head {
        return Err(BenchRefusal::new(
            BENCH_CONFIG_UNCOMMITTED,
            format!(
                "the worktree HEAD moved from {head} to {overlay_head} while the overlay was read"
            ),
        ));
    }
    let plan = overlay::AuditPlan {
        model_files: loaded.model_files.clone(),
        audited_files: loaded.audited_files.clone(),
        checkout: ports.audit_checkout(dataset_commit),
    };
    ports
        .leak_audit(cwd, plan)
        .await
        .map_err(|detail| BenchRefusal::new(BENCH_OVERLAY_LEAK_AUDIT_FAILED, detail))
}

/// The three gates only a model-driven run needs.
async fn gate_paid_run(
    ports: &dyn BenchJobPorts,
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    project: Option<Uuid>,
    resolved: &ResolvedRun,
    budget_usd: f64,
) -> std::result::Result<(), BenchRefusal> {
    let agent = &resolved.file.agent;
    let (model, effort) = (
        agent.model.clone().unwrap_or_default(),
        agent.effort.clone().unwrap_or_default(),
    );
    let project = project.ok_or_else(|| {
        BenchRefusal::new(
            BENCH_PROJECT_UNKNOWN,
            "the submitting session has no project to grant the launch",
        )
    })?;
    let wanted = ManagerLaunchChoiceV2 {
        provider: SessionProvider::OpenRouter,
        model,
        effort: Some(effort),
    };
    let (allowed, grant_hint, grant_remaining) = {
        let store = store.lock().await;
        let allowed = ports.allowed_launches(&store, project);
        let hint = ports.provider_grant_hint(&store, project, &wanted);
        let remaining = ports.provider_grant_remaining(&store, project, &wanted);
        (allowed, hint, remaining)
    };
    let allowed = allowed.map_err(|e| {
        BenchRefusal::new(rsi_common::bench::BENCH_LAUNCH_NOT_ALLOWED, e.to_string())
    })?;
    if !crate::topology::agent::launch_granted(&allowed, &wanted) {
        // #3417: a spent, expired or wrong-effort provider grant names itself.
        let detail = match grant_hint {
            Some(hint) => format!(
                "{} is not in the project's allowed_launches; {hint}",
                wanted.display()
            ),
            None => format!(
                "{} is not in the project's allowed_launches",
                wanted.display()
            ),
        };
        return Err(BenchRefusal::new(
            rsi_common::bench::BENCH_LAUNCH_NOT_ALLOWED,
            detail,
        ));
    }
    // #3417: an active provider grant bounds the run by what it has left.
    if let Some(remaining) = grant_remaining
        && budget_usd > remaining
    {
        return Err(BenchRefusal::new(
            rsi_common::bench::BENCH_LAUNCH_NOT_ALLOWED,
            format!(
                "the run budget ${budget_usd:.2} is larger than the ${remaining:.2} left under the OpenRouter provider grant; lower the budget or the operator raises the grant at {}",
                rsi_common::provider_grant::provider_grant_raise_path(wanted.provider)
            ),
        ));
    }
    let cap = ports.spend_cap().await;
    if cap.state == SpendCapState::Off && grant_remaining.is_none() {
        return Err(BenchRefusal::new(
            BENCH_SPEND_CAP_OFF,
            "this run launches OpenRouter-billed sessions and the OpenRouter spend cap is off; the operator arms openrouter_spend_cap_usd first, or grants OpenRouter a budget at TUI: :manager board > p (Policy) > Provider grants",
        ));
    }
    admit_run_budget(&cap, budget_usd)
        .map_err(|refusal| BenchRefusal::new(refusal.code, refusal.detail))?;
    if let Some(task_cap) = agent.per_task_cap_usd
        && task_cap > budget_usd
    {
        return Err(BenchRefusal::new(
            rsi_common::openrouter_spend::RUN_BUDGET_INVALID_CODE,
            format!("per_task_cap_usd {task_cap} is larger than the run budget {budget_usd}"),
        ));
    }
    Ok(())
}

/// The worktree HEAD, after checking that every file the run reads is tracked and
/// unmodified there (reusing the build source custody's bounded Git reader).
fn observe_custody(cwd: &Path, paths: &[String]) -> std::result::Result<String, BenchRefusal> {
    let uncommitted = |why: String| BenchRefusal::new(BENCH_CONFIG_UNCOMMITTED, why);
    let Some(state) = super::source::observe(cwd) else {
        return Err(uncommitted(
            "the job worktree is not a git checkout whose HEAD can be read".into(),
        ));
    };
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let dirty = crate::sandbox::git_worktree::paths_not_committed_clean_bounded(cwd, &refs)
        .map_err(|e| uncommitted(format!("cannot check the run's files: {e}")))?;
    if !dirty.is_empty() {
        return Err(uncommitted(format!(
            "commit these before running them (each must be tracked and unmodified at HEAD {}): {}",
            state.head,
            dirty.join(", ")
        )));
    }
    Ok(state.head)
}

/// `<id>.bench`: what the daemon recorded at submit and the run directory.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct BenchSidecar {
    pub config: String,
    pub config_digest: String,
    pub budget_usd: f64,
    pub head: String,
    pub run_dir: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    /// The runner's evaluator digest (`sha256:`), set at start: the record's
    /// `adapter_tree_hash`.
    #[serde(default)]
    pub adapter_hash: Option<String>,
    /// The runner's stamped source manifest (`path=hex;...`), set at start: the evaluator
    /// identity the record is assembled with, so a runner reinstalled mid-run cannot change it.
    #[serde(default)]
    pub runner_manifest: Option<String>,
}

fn sidecar_path(log_path: &str) -> PathBuf {
    Path::new(log_path).with_extension("bench")
}

fn write_sidecar(log_path: &str, sidecar: &BenchSidecar) -> Result<()> {
    let path = sidecar_path(log_path);
    let bytes = serde_json::to_vec_pretty(sidecar)?;
    write_private(&path, &bytes).map_err(|e| {
        DaemonError::Process(format!(
            "{}: could not record the bench job {}: {e}",
            rsi_common::agent_jobs::JOB_SOURCE_RECORD_FAILED,
            path.display()
        ))
    })
}

fn read_sidecar(log_path: &str) -> Option<BenchSidecar> {
    serde_json::from_slice(&std::fs::read(sidecar_path(log_path)).ok()?).ok()
}

/// Write `bytes` to `path` atomically, readable by the owner only.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Record a validated, admitted `bench` job as `queued`; [`start_queued`] runs its
/// gates again and launches it.
///
/// # Errors
/// The unit runtime cannot run bench jobs, the record cannot be written, or the
/// insert failed.
pub(crate) fn submit_queued(
    store: &Store,
    runtime: &dyn super::JobRuntime,
    jobs_dir: &Path,
    ctx: super::SubmitContext,
    admitted: &Admitted,
    now: DateTime<Utc>,
) -> Result<(crate::store::agent_jobs::AgentJobRow, bool)> {
    runtime
        .validate(&ctx.params)
        .map_err(|code| DaemonError::InvalidParam(code.into()))?;
    let JobParams::Bench(params) = &ctx.params else {
        return Err(DaemonError::InvalidParam(
            rsi_common::agent_jobs::JOB_INVALID_PARAMS.into(),
        ));
    };
    let id = Uuid::new_v4();
    let log_path = jobs_dir.join(format!("{id}.log")).display().to_string();
    let new = NewAgentJob {
        id,
        owner_session_id: ctx.owner,
        project_id: ctx.project_id,
        name: ctx.name.clone(),
        params: ctx.params.clone(),
        cwd: ctx.cwd.display().to_string(),
        unit_name: runtime.unit_name(id),
        log_path: log_path.clone(),
        status_path: jobs_dir.join(format!("{id}.status")).display().to_string(),
        idempotency_key: ctx.idempotency_key.clone(),
        wake: ctx.wake,
    };
    // The HEAD guard (hub #2954) and the bench record are written before the row
    // exists, so a bench job that cannot be held to one commit is never accepted.
    let state = super::source::observe(&ctx.cwd).ok_or_else(|| {
        DaemonError::InvalidParam(format!(
            "{BENCH_CONFIG_UNCOMMITTED}: the job worktree is not a git checkout"
        ))
    })?;
    super::source::record(&log_path, &state)?;
    let sidecar = BenchSidecar {
        config: params.config.clone(),
        config_digest: admitted.inputs.resolved.config_digest.clone(),
        budget_usd: params.budget_usd,
        head: admitted.head.clone(),
        run_dir: None,
        started_at: None,
        adapter_hash: None,
        runner_manifest: None,
    };
    if let Err(error) = write_sidecar(&log_path, &sidecar) {
        super::source::discard(&log_path);
        return Err(error);
    }
    let inserted = store.insert_agent_job_with_hold_reason(
        &new,
        now,
        JobState::Queued,
        Some(JOB_HELD_FOR_BENCH),
    );
    let discard = |log: &str| {
        super::source::discard(log);
        let _ = std::fs::remove_file(sidecar_path(log));
    };
    match inserted {
        Ok((row, true)) => {
            // The replay's own job keeps its records; this fresh id has none.
            discard(&log_path);
            Ok((row, true))
        }
        Ok((row, false)) => {
            super::append_phase(
                &log_path,
                now,
                "queue-wait: the bench gates run again when the job starts; the unit has not been launched",
            );
            Ok((row, false))
        }
        Err(error) => {
            discard(&log_path);
            Err(error)
        }
    }
}

fn settle_refused(store: &Store, job: &AgentJobV1, code: &str, detail: String) -> Result<()> {
    let result = AgentJobResultV1 {
        refusal: Some(code.to_string()),
        detail: Some(super::tail(&detail)),
        ..AgentJobResultV1::default()
    };
    let settled = store.settle_agent_job_outcome(
        job.id,
        JobState::Failed,
        &result,
        job.wake == JobWake::Owner,
        Utc::now(),
    )?;
    if settled.is_some() {
        super::append_phase(&job.log_path, Utc::now(), &format!("bench-refused: {code}"));
    }
    Ok(())
}

/// The per-task pin list of a run.
fn run_tasks(resolved: &ResolvedRun) -> Vec<BenchRunTask> {
    resolved
        .tasks
        .iter()
        .filter_map(|name| {
            resolved.task(name).map(|task| BenchRunTask {
                task: name.clone(),
                pinned_digests: vec![task.image_digest.clone()],
            })
        })
        .collect()
}

/// Everything that is written beside the run before its unit starts.
fn write_run_inputs(
    run_dir: &Path,
    admitted: &Admitted,
    plan: &RunnerPlanV1,
) -> std::io::Result<PathBuf> {
    let inputs_dir = run_dir.join("inputs");
    crate::bench::private_dir(&inputs_dir)?;
    write_private(
        &inputs_dir.join("config.toml"),
        admitted.inputs.config_text.as_bytes(),
    )?;
    write_private(
        &inputs_dir.join("dataset.toml"),
        admitted.inputs.dataset_text.as_bytes(),
    )?;
    write_private(
        &inputs_dir.join("split.json"),
        admitted.inputs.split_text.as_bytes(),
    )?;
    let plan_path = run_dir.join("plan.json");
    write_private(&plan_path, &serde_json::to_vec_pretty(plan)?)?;
    Ok(plan_path)
}

/// Launch every `queued` bench job whose gates still hold. Returns how many started.
///
/// # Errors
/// A store or join failure; a job whose gates fail settles `failed` with the
/// typed refusal instead.
pub(crate) async fn start_queued(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    runtime: &Arc<dyn super::JobRuntime>,
    drain: &crate::deploy_drain::DeployDrain,
    ports: &Arc<dyn BenchJobPorts>,
) -> Result<usize> {
    let queued: Vec<_> = {
        let store = Arc::clone(store);
        tokio::task::spawn_blocking(move || {
            crate::store_lock::blocking_lock(&store, "agent_jobs::bench_list_queued")
                .list_queued_agent_jobs()
        })
        .await
        .map_err(|e| DaemonError::Process(format!("bench queued list join: {e}")))??
    };
    let mut started = 0;
    for row in queued
        .into_iter()
        .filter(|row| row.job.kind == JobKind::Bench)
    {
        let job = row.job.clone();
        let (owner_project, has_parent) = {
            let store = crate::store_lock::lock(store, "agent_jobs::bench_start").await;
            let session = store.get_session(job.owner_session_id)?;
            (
                session.as_ref().and_then(|s| s.project_id),
                session.is_none_or(|s| s.parent_id.is_some()),
            )
        };
        // A staged deploy stops what it would break: no run starts under it, whoever owns
        // the job (a bench run is long and a restart would only interrupt it).
        if drain.holds_job_start(Some(job.owner_session_id), has_parent)
            || drain.job_release_by().is_some()
        {
            continue;
        }
        let settle = |code: &str, detail: String| {
            let store = Arc::clone(store);
            let job = job.clone();
            let code = code.to_string();
            async move {
                tokio::task::spawn_blocking(move || {
                    let guard =
                        crate::store_lock::blocking_lock(&store, "agent_jobs::bench_settle");
                    settle_refused(&guard, &job, &code, detail)
                })
                .await
                .map_err(|e| DaemonError::Process(format!("bench settle join: {e}")))?
            }
        };
        // Hub #2954: the worktree must still be the one the job was admitted on.
        if let Err(result) =
            super::source::check(&job, "when the queued bench job was about to start")
        {
            settle(
                result
                    .refusal
                    .as_deref()
                    .unwrap_or(rsi_common::agent_jobs::JOB_SOURCE_MOVED),
                result.detail.unwrap_or_default(),
            )
            .await?;
            continue;
        }
        let Some(mut sidecar) = read_sidecar(&job.log_path) else {
            settle(
                JOB_LAUNCH_FAILED,
                "the bench job's submit record is missing".into(),
            )
            .await?;
            continue;
        };
        let JobParams::Bench(params) = &job.params else {
            continue;
        };
        let cwd = PathBuf::from(&job.cwd);
        let admitted = match admit(&**ports, store, owner_project, &cwd, params, Stage::Start).await
        {
            Ok(admitted) => admitted,
            Err(refusal) => {
                settle(refusal.code, refusal.detail).await?;
                continue;
            }
        };
        if admitted.inputs.resolved.config_digest != sidecar.config_digest {
            settle(
                BENCH_CONFIG_CHANGED,
                format!(
                    "the run config was {} at submit and is {} now",
                    sidecar.config_digest, admitted.inputs.resolved.config_digest
                ),
            )
            .await?;
            continue;
        }
        match launch_one(
            store,
            runtime,
            ports,
            &row,
            &job,
            owner_project,
            params,
            &admitted,
            &mut sidecar,
        )
        .await
        {
            Ok(true) => started += 1,
            Ok(false) => {}
            Err(refusal) => {
                settle(refusal.code, refusal.detail).await?;
            }
        }
    }
    Ok(started)
}

#[allow(clippy::too_many_arguments)]
async fn launch_one(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    runtime: &Arc<dyn super::JobRuntime>,
    ports: &Arc<dyn BenchJobPorts>,
    row: &crate::store::agent_jobs::AgentJobRow,
    job: &AgentJobV1,
    project: Option<Uuid>,
    params: &BenchJobParams,
    admitted: &Admitted,
    sidecar: &mut BenchSidecar,
) -> std::result::Result<bool, BenchRefusal> {
    let resolved = &admitted.inputs.resolved;
    let run_id = job.id;
    let run_dir = ports.bench_root().join(run_id.to_string());
    let open_failed = |why: String| BenchRefusal::new(BENCH_RUN_OPEN_FAILED, why);
    crate::bench::private_dir(&run_dir).map_err(|e| open_failed(e.to_string()))?;
    // #3191: the run's overlay is written before the run opens, so no trial can start
    // without it. The daemon re-verifies it on every trial start.
    if let Some(overlay) = admitted.inputs.run_overlay() {
        let bytes = serde_json::to_vec_pretty(overlay).map_err(|e| open_failed(e.to_string()))?;
        write_private(&run_dir.join(crate::bench::OVERLAY_NAME), &bytes)
            .map_err(|e| open_failed(e.to_string()))?;
    }
    let mut opened_socket: Option<PathBuf> = None;
    // The adapter socket is the capability a Harness client uses (R4/R6); the oracle needs none.
    if resolved.is_paid() {
        let project = project.ok_or_else(|| {
            BenchRefusal::new(BENCH_PROJECT_UNKNOWN, "the job owner has no project")
        })?;
        let agent = &resolved.file.agent;
        let run = NewBenchRun {
            id: run_id,
            project_id: project,
            job_id: Some(job.id.to_string()),
            config_path: params.config.clone(),
            config_digest: resolved.config_digest.clone(),
            dataset_commit: resolved.dataset.header.commit.clone(),
            docker_host: resolved.file.host.clone(),
            model: agent.model.clone().unwrap_or_default(),
            effort: agent.effort.clone().unwrap_or_default(),
            budget_usd: params.budget_usd,
            per_task_cap_usd: agent.per_task_cap_usd.unwrap_or_default(),
            concurrency: admitted.concurrency,
            tasks: run_tasks(resolved),
            loop_knobs: resolved.file.loop_knobs,
        };
        opened_socket = Some(
            ports
                .open_run(run)
                .await
                .map_err(|e| open_failed(e.to_string()))?,
        );
    }
    let bench_root = ports.bench_root();
    let checkout_dir = pinned_checkout_dir(&bench_root, &resolved.dataset.header.commit);
    // The runner grades against the copy of the pinned dataset this run was admitted with.
    let dataset_path = run_dir.join("inputs").join("dataset.toml");
    let settings = ports.settings();
    let plan = runner_plan(
        run_id,
        resolved,
        &PlanHosts {
            laptop_host: &admitted.laptop_host,
            docker_hosts: &settings.docker_hosts,
            concurrency: admitted.concurrency,
            max_concurrency: settings.max_concurrency,
        },
        &PlanPaths {
            checkout_dir: &checkout_dir,
            dataset_path: &dataset_path,
            output_root: &bench_root,
        },
    )?;
    // R6: the socket the runner is told to drive is the one the daemon bound for this run; any
    // disagreement (or a Harness plan with no bound socket) refuses before the unit starts.
    let planned_socket = plan
        .remote
        .as_ref()
        .and_then(|remote| remote.socket_path.as_deref());
    if planned_socket.map(Path::new) != opened_socket.as_deref() {
        return Err(open_failed(format!(
            "the plan's run socket {planned_socket:?} is not the one the daemon bound ({opened_socket:?})"
        )));
    }
    let plan_path =
        write_run_inputs(&run_dir, admitted, &plan).map_err(|e| open_failed(e.to_string()))?;
    super::append_phase(
        &job.log_path,
        Utc::now(),
        &format!(
            "bench-start: {} ({}) {}",
            resolved.file.name, params.config, resolved.file.description
        ),
    );
    sidecar.run_dir = Some(run_dir.display().to_string());
    sidecar.started_at = Some(Utc::now());
    sidecar.adapter_hash = Some(admitted.runner.evaluator_digest.clone());
    sidecar.runner_manifest = Some(admitted.runner.manifest.to_text());
    write_sidecar(&job.log_path, sidecar).map_err(|e| open_failed(e.to_string()))?;
    let command = super::JobCommand {
        argv: runner_argv(&admitted.runner.path, "run", &plan_path),
        runtime_max_secs: BENCH_RUNTIME_MAX_SECS,
        stop_timeout_secs: 120,
        log_max_bytes: 64 * super::MIB,
        memory_max_gib: 2,
        // The unit only drives ssh and docker and waits: one CPU at most, never the
        // merge-queue's (the build slice).
        cpu_quota_percent: 100,
        background: true,
        admission_max_secs: None,
        scoped_tool: None,
    };
    let new = NewAgentJob {
        id: job.id,
        owner_session_id: job.owner_session_id,
        project_id: None,
        name: job.name.clone(),
        params: job.params.clone(),
        cwd: job.cwd.clone(),
        unit_name: job.unit_name.clone(),
        log_path: job.log_path.clone(),
        status_path: row.status_path.clone(),
        idempotency_key: None,
        wake: job.wake,
    };
    let (store, runtime) = (Arc::clone(store), Arc::clone(runtime));
    let launched = tokio::task::spawn_blocking(move || -> Result<bool> {
        let guard = crate::store_lock::blocking_lock(&store, "agent_jobs::bench_launch");
        if !guard.start_queued_agent_job(new.id, Utc::now())? {
            return Ok(false);
        }
        let wake = new.wake == JobWake::Owner;
        Ok(super::launch_recorded(&guard, &*runtime, &new, command, wake).is_ok())
    })
    .await
    .map_err(|e| BenchRefusal::new(JOB_LAUNCH_FAILED, e.to_string()))?
    .map_err(|e| BenchRefusal::new(JOB_LAUNCH_FAILED, e.to_string()))?;
    Ok(launched)
}

/// Where a run stands against its own budget, read from the spend ledger.
pub(crate) trait RunSpend {
    /// What the run's sessions have cost so far (daemon increments).
    ///
    /// # Errors
    /// The ledger cannot be read.
    fn spent_usd(&self, run: Uuid) -> Result<f64>;
}

/// The store-backed ledger: the increment SQL over the run's sessions, never the
/// session-cumulative rows.
pub(crate) struct StoreSpend<'a>(pub &'a Store);

impl RunSpend for StoreSpend<'_> {
    fn spent_usd(&self, run: Uuid) -> Result<f64> {
        let sessions = self.0.bench_run_session_ids(run)?;
        self.0.sessions_invocation_spend(&sessions)
    }
}

/// The per-trial gate: may another trial launch under the run's budget?
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TrialGate {
    Launch(RunBudgetStatusV1),
    Stop(RunBudgetStatusV1),
}

/// Re-read the run's spend and judge it with M4's `run_budget_status`.
///
/// # Errors
/// The ledger cannot be read; the caller treats that as no news, never as a launch.
pub(crate) fn gate_trial(budget_usd: f64, run: Uuid, spend: &dyn RunSpend) -> Result<TrialGate> {
    let status = run_budget_status(budget_usd, spend.spent_usd(run)?);
    Ok(if status.exhausted {
        TrialGate::Stop(status)
    } else {
        TrialGate::Launch(status)
    })
}

/// `<run>/stop.json`: the daemon's instruction that no further shard starts.
pub(crate) const STOP_FILE: &str = "stop.json";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct StopMarker {
    pub reason: String,
    pub spent_usd: f64,
    pub budget_usd: f64,
    pub at: DateTime<Utc>,
}

/// One watcher tick for a running job: write [`STOP_FILE`] once the run's budget is
/// spent. Returns the status when this tick wrote the stop.
///
/// # Errors
/// The ledger or the marker cannot be read or written.
pub(crate) fn watch_budget(
    run_dir: &Path,
    budget_usd: f64,
    run: Uuid,
    spend: &dyn RunSpend,
    now: DateTime<Utc>,
) -> Result<Option<RunBudgetStatusV1>> {
    let marker = run_dir.join(STOP_FILE);
    if marker.exists() {
        return Ok(None);
    }
    match gate_trial(budget_usd, run, spend)? {
        TrialGate::Launch(_) => Ok(None),
        TrialGate::Stop(status) => {
            let stop = StopMarker {
                reason: record::NOT_RUN_BUDGET.to_string(),
                spent_usd: status.spent_usd,
                budget_usd,
                at: now,
            };
            write_private(&marker, &serde_json::to_vec_pretty(&stop)?)
                .map_err(|e| DaemonError::Process(format!("bench stop marker: {e}")))?;
            tracing::warn!(%run, spent = status.spent_usd, budget = budget_usd, "bench run budget spent; no further shard starts");
            Ok(Some(status))
        }
    }
}

/// The jobs-loop pass over running bench jobs: the budget watcher.
///
/// # Errors
/// A store failure; one unreadable run never stops the others.
pub(crate) async fn tick(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    now: DateTime<Utc>,
) -> Result<usize> {
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || -> Result<usize> {
        let guard = crate::store_lock::blocking_lock(&store, "agent_jobs::bench_tick");
        let mut stopped = 0;
        for row in guard.list_running_agent_jobs()? {
            if row.job.kind != JobKind::Bench {
                continue;
            }
            let Some(sidecar) = read_sidecar(&row.job.log_path) else {
                continue;
            };
            let Some(run_dir) = sidecar.run_dir.as_deref() else {
                continue;
            };
            match watch_budget(
                Path::new(run_dir),
                sidecar.budget_usd,
                row.job.id,
                &StoreSpend(&guard),
                now,
            ) {
                Ok(Some(_)) => stopped += 1,
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(job = %row.job.id, %error, "bench budget watch deferred")
                }
            }
        }
        Ok(stopped)
    })
    .await
    .map_err(|e| DaemonError::Process(format!("bench tick join: {e}")))?
}

/// The active runs whose job ended (or never existed) and the state each closes
/// with: `Finished` after a succeeded job, `Cancelled` otherwise.
///
/// # Errors
/// A store failure.
pub(crate) fn ended_runs(store: &Store) -> Result<Vec<(Uuid, BenchRunState)>> {
    let mut ended = Vec::new();
    for run in store.list_active_bench_runs()? {
        let Some(job_id) = run
            .job_id
            .as_deref()
            .and_then(|id| Uuid::parse_str(id).ok())
        else {
            continue;
        };
        match store.get_agent_job(job_id)? {
            Some(job) if !job.job.state.is_terminal() => {}
            Some(job) if job.job.state == JobState::Succeeded => {
                ended.push((run.id, BenchRunState::Finished));
            }
            _ => ended.push((run.id, BenchRunState::Cancelled)),
        }
    }
    Ok(ended)
}

/// Close the adapter socket of every active run whose job ended (or never
/// existed); a cancelled run also stops any trial still live.
///
/// # Errors
/// A store failure.
pub(crate) async fn reap_runs(store: &Arc<rsid_core::store_lock::Mutex<Store>>) -> Result<usize> {
    let Some(service) = crate::bench::global() else {
        return Ok(0);
    };
    let ended = ended_runs(&*store.lock().await)?;
    let mut closed = 0;
    for (run, state) in ended {
        match service.end_run(run, state).await {
            Ok(true) => closed += 1,
            Ok(false) => {}
            Err(error) => tracing::warn!(%run, %error, "bench run could not be closed"),
        }
    }
    Ok(closed)
}

/// A settled job's exit mapped to its state and refusal. Exit 2 is a run that finished with a
/// trial that did not pass or was not cleaned up: the outcomes are the result, so the job
/// settles and the record carries every verdict.
pub(crate) fn classify_exit(exit_code: Option<i32>, result: &mut AgentJobResultV1) -> JobState {
    if exit_code == Some(0) {
        return JobState::Succeeded;
    }
    if exit_code == Some(transport::EXIT_NOT_ALL_PASSED) {
        result.detail = Some(
            "the runner finished, but a trial did not pass or was not cleaned up; see the record"
                .to_string(),
        );
        return JobState::Succeeded;
    }
    result.refusal = Some(
        exit_code
            .map_or(BENCH_TRANSPORT_FAILED, exit_refusal)
            .to_string(),
    );
    JobState::Failed
}

/// The finish step: join the runner's trial outcomes with the daemon's rows
/// into the sealed record, write `record.json` and put the run summary on the
/// result. Best effort: a run whose inputs are gone keeps the transport's own
/// result and says why.
pub(crate) fn finalize(
    store: &Store,
    job: &AgentJobV1,
    _log: &str,
    state: &mut JobState,
    result: &mut AgentJobResultV1,
) {
    if let Err(why) = finalize_inner(store, job, state, result) {
        tracing::warn!(job = %job.id, %why, "bench record could not be assembled");
        let note = format!("bench record not written: {why}");
        result.detail = Some(match result.detail.take() {
            Some(detail) => format!("{note}\n{detail}"),
            None => note,
        });
    }
}

/// #3191: the identity of the overlay the daemon wrote into the run directory, for
/// the record's sampling: its digest and the sha256 of each file it references. A run
/// directory with no overlay file had none.
fn read_run_overlay(run_dir: &Path) -> Option<serde_json::Value> {
    let bytes = match std::fs::read(run_dir.join(crate::bench::OVERLAY_NAME)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return Some(serde_json::json!({"digest": "unreadable"})),
    };
    Some(
        match serde_json::from_slice::<rsi_common::bench_overlay::BenchOverlay>(&bytes) {
            Ok(overlay) => serde_json::json!({"digest": overlay.digest, "files": overlay.files()}),
            Err(_) => serde_json::json!({"digest": "unreadable"}),
        },
    )
}

fn finalize_inner(
    store: &Store,
    job: &AgentJobV1,
    state: &mut JobState,
    result: &mut AgentJobResultV1,
) -> std::result::Result<(), String> {
    let JobParams::Bench(params) = &job.params else {
        return Ok(());
    };
    let sidecar = read_sidecar(&job.log_path).ok_or("the bench job's record is missing")?;
    let run_dir = PathBuf::from(
        sidecar
            .run_dir
            .as_deref()
            .ok_or("the job never started its run")?,
    );
    let read = |name: &str| {
        std::fs::read_to_string(run_dir.join("inputs").join(name))
            .map_err(|e| format!("inputs/{name}: {e}"))
    };
    let (config_text, dataset_text, split_text) = (
        read("config.toml")?,
        read("dataset.toml")?,
        read("split.json")?,
    );
    let dataset = config::Dataset::parse(&dataset_text, &split_text)?;
    let resolved = config::RunFile::parse(&config_text)?.resolve(&config_text, dataset)?;
    // The evaluator identity is the one the runner was admitted with (sidecar), never whatever
    // is installed now.
    let evaluator = RunnerSourceManifest::parse(
        sidecar
            .runner_manifest
            .as_deref()
            .ok_or("the job's record names no runner build")?,
    )?;
    let marker: Option<StopMarker> = std::fs::read(run_dir.join(STOP_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let (outcomes, notes) = transport::read_outcomes(&run_dir, &job.id.to_string());
    let bench_tasks = store.list_bench_tasks(job.id).map_err(|e| e.to_string())?;
    let mut calls = std::collections::BTreeMap::new();
    let mut provenance = std::collections::BTreeMap::new();
    for session in bench_tasks.iter().filter_map(|t| t.session_id) {
        if let Ok(usage) = store.bench_session_usage(session) {
            calls.insert(session, usage.calls);
        }
        // #3192: each trial records the build and harness it actually ran on.
        let digest_row = crate::session::bench_route::read_build_digest_row(store, session)
            .ok()
            .flatten();
        let version_hash = store
            .get_session(session)
            .ok()
            .flatten()
            .and_then(|s| s.harness_version_hash);
        provenance.insert(
            session,
            record::TrialProvenance::from_digest_row(digest_row.as_ref(), version_hash),
        );
    }
    let stop = if marker.is_some() {
        Some(record::StopNote {
            reason: record::NOT_RUN_BUDGET.to_string(),
            cause: record::NOT_RUN_BUDGET,
        })
    } else {
        (*state != JobState::Succeeded)
            .then(|| "transport_failed".to_string())
            .or_else(|| {
                transport::slots_missing(&resolved, &outcomes).then(|| "incomplete".to_string())
            })
            .map(|reason| record::StopNote {
                reason,
                cause: record::NOT_RUN_STOPPED,
            })
    };
    let trace_text = |row: &crate::store::bench::BenchTaskRow| -> Option<String> {
        let path = row.trace_path.as_deref()?;
        let len = std::fs::metadata(path).ok()?.len();
        if len > record::MAX_TRACE_SCAN_BYTES {
            return None;
        }
        std::fs::read_to_string(path).ok()
    };
    let concurrency = store
        .get_bench_run(job.id)
        .ok()
        .flatten()
        .map_or(resolved.file.concurrency, |run: BenchRunRow| {
            run.concurrency
        });
    let stopped = stop.as_ref().map(|s| s.reason.clone());
    let shards = transport::shard_results(&resolved, &outcomes);
    let uncleaned = transport::uncleaned(&outcomes);
    let assembled = record::assemble_runner(record::RunnerRecordInputs {
        resolved: &resolved,
        host: &resolved.file.host,
        concurrency,
        evaluator: &evaluator,
        started_at: sidecar.started_at.unwrap_or_else(Utc::now),
        finished_at: Utc::now(),
        outcomes,
        bench_tasks,
        calls,
        stop,
        trace_text: &trace_text,
        provenance,
        overlay: read_run_overlay(&run_dir),
    });
    for note in notes {
        tracing::warn!(job = %job.id, %note, "unreadable runner trial outcome skipped");
    }
    let json = assembled.record.to_json().map_err(|e| e.to_string())?;
    let record_path = run_dir.join("record.json");
    write_private(&record_path, json.as_bytes()).map_err(|e| e.to_string())?;
    let summary = record::summarize(
        job.id,
        &params.config,
        &resolved,
        &assembled,
        shards,
        Some(record_path.display().to_string()),
        params.budget_usd,
        stopped.clone(),
    );
    if let Some(reason) = &stopped
        && *state == JobState::Succeeded
    {
        let note = format!(
            "bench run stopped early ({reason}): {} of {} trials ran, {} not run",
            summary.trials, summary.expected_trials, summary.not_run_trials
        );
        result.detail = Some(match result.detail.take() {
            Some(detail) => format!("{note}\n{detail}"),
            None => note,
        });
    }
    if !uncleaned.is_empty() {
        let note = format!(
            "owned resources were not cleaned up for: {}",
            uncleaned.join(", ")
        );
        result.detail = Some(match result.detail.take() {
            Some(detail) => format!("{note}\n{detail}"),
            None => note,
        });
    }
    result.bench = Some(summary);
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
