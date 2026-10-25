//! The bench run adapter (#3178, program #3135 M3 S4).
//!
//! A Harbor trial on the laptop starts, watches and stops exactly one daemon
//! Harness session for its task container, only inside one admitted bench run.
//! The run's budget and launch grant bound it:
//!
//! * [`BenchService`] owns the runs: it records a run in the bench store
//!   ([`BenchService::open_run`]), binds the run's adapter socket
//!   `~/.rsi/bench/<run>/adapter.sock` while the run is active and again after a
//!   daemon restart ([`BenchService::restore`]), and unbinds it when the run ends.
//! * [`listener`] serves the five Bench methods on that socket, for that run
//!   only, and refuses every other method.
//! * [`task`] holds the methods: start (gated by the run, its pinned task set,
//!   `allowed_launches`, the run budget, the concurrency cap and the container
//!   target), status, cancel, finish and the full trace read.
//! * [`cap`] is the per-task USD cap checked before every model call, and
//!   [`trace`] the live, untruncated trace.
//!
//! The five methods are not agent verbs and not daemon-socket methods: the
//! socket is the capability. Anything that can open it can call them, and each
//! call is bounded by the run's budget, the grant and container validation.

pub mod cap;
pub mod listener;
pub mod task;
pub mod trace;

use crate::claude::LaunchConfig;
use crate::config::RuntimeConfig;
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::bench::{BenchRunRow, NewBenchRun};
use async_trait::async_trait;
use rsi_common::bench::BenchRunState;
use rsi_common::exec_target::ContainerExecTarget;
use rsi_common::harness_manager_v2::ManagerLaunchChoiceV2;
use rsi_common::openrouter_spend::RunBudgetReportV1;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;
use uuid::Uuid;

/// The adapter socket's file name inside a run's directory.
pub const SOCKET_NAME: &str = "adapter.sock";

/// The run overlay (#3191) the bench job wrote into a run's directory before it
/// opened the run: the validated [`BenchOverlay`] as JSON. No file is no overlay.
pub const OVERLAY_NAME: &str = "overlay.json";

/// The largest overlay file the daemon reads back: the instructions cap, their
/// hashes and the knobs, with room to spare.
const MAX_OVERLAY_FILE_BYTES: u64 = 256 * 1024;

/// A run's overlay is unreadable, malformed or fails its own checks.
pub const BENCH_OVERLAY_UNAVAILABLE: &str = "bench_overlay_unavailable";

/// A typed bench refusal: the stable `code` and a human `detail`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchError {
    pub code: String,
    pub detail: String,
    /// The same call may succeed later (a full run, a trial still ending).
    pub retryable: bool,
}

impl BenchError {
    #[must_use]
    pub fn new(code: &str, detail: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            detail: detail.into(),
            retryable: false,
        }
    }

    #[must_use]
    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    /// `bench_invalid_request:<reason>`.
    #[must_use]
    pub fn invalid(reason: &str) -> Self {
        Self::new(
            rsi_common::bench::BENCH_INVALID_REQUEST,
            format!("{}:{reason}", rsi_common::bench::BENCH_INVALID_REQUEST),
        )
    }

    /// `code: detail`, the text on the wire.
    #[must_use]
    pub fn message(&self) -> String {
        if self.detail.starts_with(&self.code) {
            self.detail.clone()
        } else {
            format!("{}: {}", self.code, self.detail)
        }
    }

    /// A daemon error: a policy refusal keeps its own leading code.
    #[must_use]
    pub fn from_daemon(error: &DaemonError) -> Self {
        match error {
            DaemonError::PolicyDenied(text) | DaemonError::InvalidParam(text) => {
                let code = text
                    .split([':', ' '])
                    .next()
                    .filter(|code| !code.is_empty())
                    .unwrap_or("bench_internal_error");
                Self::new(code, text.clone())
            }
            other => Self::new("bench_internal_error", other.to_string()),
        }
    }
}

impl std::fmt::Display for BenchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl From<DaemonError> for BenchError {
    fn from(error: DaemonError) -> Self {
        Self::from_daemon(&error)
    }
}

/// What the bench service needs from the rest of the daemon. One seam, so the
/// production host and a test double differ only in what they pass in.
#[async_trait]
pub trait BenchHost: Send + Sync {
    /// Launch one session (`SessionManager::launch_session`).
    async fn launch(&self, config: LaunchConfig) -> Result<Uuid>;
    /// Stop a session (`SessionManager::interrupt_session`).
    async fn interrupt(&self, session_id: Uuid) -> Result<()>;
    /// #3992: create a project-level Group (`SessionManager::create_container`)
    /// for a bench run's trials; returns its id.
    async fn create_group(&self, project: Uuid, name: String, tags: Vec<String>) -> Result<Uuid>;
    /// Validate a container target against the operator's `bench.docker_hosts`,
    /// the container's compose labels and the pinned image digests. The error is
    /// the `exec_target_invalid:<reason>` text.
    async fn validate_container(
        &self,
        target: &ContainerExecTarget,
    ) -> std::result::Result<(), String>;
    /// The launches the project's grant allows (`allowed_launches`).
    fn allowed_launches(&self, store: &Store, project: Uuid) -> Result<Vec<ManagerLaunchChoiceV2>>;
    /// The M4 run-budget verdict for a run that has launched `sessions`.
    async fn run_budget(&self, run_budget_usd: f64, sessions: &[Uuid])
    -> Result<RunBudgetReportV1>;
}

/// The daemon's own host.
pub struct DaemonBenchHost {
    sessions: Arc<crate::session::SessionManager>,
    config: Arc<RuntimeConfig>,
}

impl DaemonBenchHost {
    #[must_use]
    pub fn new(sessions: Arc<crate::session::SessionManager>, config: Arc<RuntimeConfig>) -> Self {
        Self { sessions, config }
    }
}

#[async_trait]
impl BenchHost for DaemonBenchHost {
    async fn launch(&self, config: LaunchConfig) -> Result<Uuid> {
        self.sessions.launch_session(config).await
    }

    async fn interrupt(&self, session_id: Uuid) -> Result<()> {
        self.sessions.interrupt_session(session_id).await
    }

    async fn create_group(&self, project: Uuid, name: String, tags: Vec<String>) -> Result<Uuid> {
        let group = self
            .sessions
            .create_container(rsi_common::rpc::CreateContainerParams {
                kind: rsi_common::types::SessionKind::Group,
                name,
                parent_id: None,
                project_id: Some(project),
                tags,
                topology_id: None,
            })
            .await?;
        Ok(group.id)
    }

    async fn validate_container(
        &self,
        target: &ContainerExecTarget,
    ) -> std::result::Result<(), String> {
        let allowed = self.config.bench_docker_hosts();
        crate::session::harness::tools::exec_target::validate_container_target(
            target,
            &allowed,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
    }

    fn allowed_launches(&self, store: &Store, project: Uuid) -> Result<Vec<ManagerLaunchChoiceV2>> {
        crate::topology::agent::project_allowed_launches(store, Some(project))
    }

    async fn run_budget(
        &self,
        run_budget_usd: f64,
        sessions: &[Uuid],
    ) -> Result<RunBudgetReportV1> {
        crate::openrouter_spend::SpendEnforcer::production(
            Arc::clone(&self.config),
            Arc::clone(self.sessions.store()),
        )
        .run_budget_report(run_budget_usd, sessions)
        .await
    }
}

/// The bench runs of this daemon.
pub struct BenchService {
    pub(crate) store: Arc<rsid_core::store_lock::Mutex<Store>>,
    pub(crate) host: Arc<dyn BenchHost>,
    /// `~/.rsi/bench`: one directory per run, 0700.
    pub(crate) root: PathBuf,
    listeners: Mutex<HashMap<Uuid, listener::ListenerHandle>>,
    /// Serializes a start's budget admission with its claim, so two starts
    /// cannot both read the same remainder before either has reserved a cap.
    pub(crate) admission: Mutex<()>,
    /// #3992: serializes finding or creating a run's trial group, so two
    /// concurrent starts of one run never create two groups.
    pub(crate) group_lock: Mutex<()>,
}

static GLOBAL: OnceLock<Arc<BenchService>> = OnceLock::new();

/// Install the daemon's service once, at startup.
pub fn install_global(service: Arc<BenchService>) {
    let _ = GLOBAL.set(service);
}

/// The daemon's service, once installed.
#[must_use]
pub fn global() -> Option<Arc<BenchService>> {
    GLOBAL.get().cloned()
}

/// `~/.rsi/bench`.
#[must_use]
pub fn default_root() -> PathBuf {
    rsi_common::identity::data_path("bench", "bench")
}

impl BenchService {
    #[must_use]
    pub fn new(
        store: Arc<rsid_core::store_lock::Mutex<Store>>,
        host: Arc<dyn BenchHost>,
        root: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            host,
            root,
            listeners: Mutex::new(HashMap::new()),
            admission: Mutex::new(()),
            group_lock: Mutex::new(()),
        })
    }

    /// The run's adapter socket path.
    #[must_use]
    pub fn socket_path(&self, run_id: Uuid) -> PathBuf {
        self.root.join(run_id.to_string()).join(SOCKET_NAME)
    }

    /// The run's overlay file (#3191).
    #[must_use]
    pub fn overlay_path(&self, run_id: Uuid) -> PathBuf {
        self.root.join(run_id.to_string()).join(OVERLAY_NAME)
    }

    /// The overlay the bench job admitted for `run_id`, re-verified (every limit and
    /// its digest) on each read. No file is no overlay; a file that cannot be read,
    /// parsed or verified refuses the trial rather than run it unmodified.
    ///
    /// # Errors
    /// `bench_overlay_unavailable` for an unreadable, oversized, malformed or
    /// tampered overlay file.
    pub fn run_overlay(
        &self,
        run_id: Uuid,
    ) -> std::result::Result<Option<rsi_common::bench_overlay::BenchOverlay>, BenchError> {
        use std::io::Read;
        let path = self.overlay_path(run_id);
        let unavailable = |why: String| {
            BenchError::new(
                BENCH_OVERLAY_UNAVAILABLE,
                format!("{BENCH_OVERLAY_UNAVAILABLE}: {why}"),
            )
        };
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(unavailable(error.to_string())),
        };
        let mut bytes = Vec::new();
        file.take(MAX_OVERLAY_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| unavailable(error.to_string()))?;
        if bytes.len() as u64 > MAX_OVERLAY_FILE_BYTES {
            return Err(unavailable("the overlay file is oversized".into()));
        }
        let overlay: rsi_common::bench_overlay::BenchOverlay =
            serde_json::from_slice(&bytes).map_err(|error| unavailable(error.to_string()))?;
        overlay
            .verify()
            .map_err(|error| unavailable(error.to_string()))?;
        Ok(Some(overlay))
    }

    /// The trial's directory (`trace.jsonl` and the empty `scratch` working directory).
    #[must_use]
    pub fn trial_dir(&self, run_id: Uuid, trial: &str) -> PathBuf {
        self.root.join(run_id.to_string()).join(trial)
    }

    /// Record an admitted run and bind its adapter socket. A run whose socket
    /// cannot be bound is cancelled, so no run is active without one.
    ///
    /// # Errors
    /// The run is malformed or the socket cannot be bound.
    pub async fn open_run(self: &Arc<Self>, run: NewBenchRun) -> Result<PathBuf> {
        let id = run.id;
        self.store
            .lock()
            .await
            .insert_bench_run(&run, chrono::Utc::now())?;
        match self.bind_run(id).await {
            Ok(path) => Ok(path),
            Err(error) => {
                let _ = self.store.lock().await.end_bench_run(
                    id,
                    BenchRunState::Cancelled,
                    chrono::Utc::now(),
                );
                Err(error)
            }
        }
    }

    /// End an active run and unbind its socket. A cancelled run also stops its
    /// live trials. Returns whether the run was active.
    ///
    /// # Errors
    /// A store failure.
    pub async fn end_run(self: &Arc<Self>, run_id: Uuid, state: BenchRunState) -> Result<bool> {
        let ended = self
            .store
            .lock()
            .await
            .end_bench_run(run_id, state, chrono::Utc::now())?;
        if let Some(handle) = self.listeners.lock().await.remove(&run_id) {
            handle.close();
        }
        if ended && state == BenchRunState::Cancelled {
            let live: Vec<String> = self
                .store
                .lock()
                .await
                .list_bench_tasks(run_id)?
                .into_iter()
                .filter(|task| task.state.is_live())
                .map(|task| task.trial)
                .collect();
            for trial in live {
                if let Err(error) = self.cancel_trial(run_id, &trial).await {
                    tracing::warn!(%run_id, trial, %error, "bench run end could not cancel a trial");
                }
            }
        }
        Ok(ended)
    }

    /// Bind (or rebind) the adapter socket of an active run.
    ///
    /// # Errors
    /// The run is not active, or the socket cannot be created.
    pub async fn bind_run(self: &Arc<Self>, run_id: Uuid) -> Result<PathBuf> {
        let run = self.active_run(run_id).await?;
        let mut listeners = self.listeners.lock().await;
        if let Some(old) = listeners.remove(&run.id) {
            old.close();
        }
        let path = self.socket_path(run.id);
        let handle = listener::bind(Arc::clone(self), run.id, &path)
            .map_err(|e| DaemonError::Process(format!("bench adapter socket: {e}")))?;
        listeners.insert(run.id, handle);
        Ok(path)
    }

    async fn active_run(&self, run_id: Uuid) -> Result<BenchRunRow> {
        match self.store.lock().await.get_bench_run(run_id)? {
            Some(run) if run.state == BenchRunState::Active => Ok(run),
            _ => Err(DaemonError::PolicyDenied(format!(
                "{}: run {run_id}",
                rsi_common::bench::BENCH_RUN_NOT_ACTIVE
            ))),
        }
    }

    /// Daemon start: every live trial becomes `infra_interrupted` (never
    /// resumed; the adapter reads that state), its session is stopped if it is
    /// still around, and every active run's socket is bound again. Returns the
    /// interrupted trial count.
    ///
    /// # Errors
    /// A store failure. A socket that cannot be bound is logged and skipped, so
    /// one bad run directory never stops the daemon.
    pub async fn restore(self: &Arc<Self>) -> Result<usize> {
        let interrupted = self
            .store
            .lock()
            .await
            .interrupt_live_bench_tasks(chrono::Utc::now())?;
        for (run_id, trial, session) in &interrupted {
            tracing::warn!(%run_id, trial, "bench trial interrupted by a daemon restart");
            if let Some(session) = session
                && let Err(error) = self.host.interrupt(*session).await
            {
                tracing::debug!(%session, %error, "interrupting a restored bench session");
            }
        }
        let runs = self.store.lock().await.list_active_bench_runs()?;
        for run in runs {
            if let Err(error) = self.bind_run(run.id).await {
                tracing::error!(run_id = %run.id, %error, "bench adapter socket not rebound");
            }
        }
        Ok(interrupted.len())
    }

    /// Whether `run_id`'s adapter socket is bound (tests, diagnostics).
    pub async fn is_bound(&self, run_id: Uuid) -> bool {
        self.listeners.lock().await.contains_key(&run_id)
    }
}

/// Create `dir` (and parents) readable by the owner only.
pub(crate) fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
