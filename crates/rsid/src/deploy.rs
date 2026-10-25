//! `AgentRequestDeploy` (#1045 slice 2): stage verified binaries, wait for a
//! quiet point, swap them in and restart under `rsid-supervisor.sh`.
//!
//! The daemon, not a session it manages, owns the restart: at the quiet point
//! it marks the deploy `restarting`, renames the staged binaries over the
//! install paths (keeping the old ones as `<name>.prev`) and starts the
//! established graceful drain, then exits 75 so the supervisor relaunches the
//! new binary with its own environment intact. On the next start the loop
//! verifies the running build against the deploy, settles the row and wakes the
//! owner once.

use crate::daemon_info::{BUILD_SHA, DaemonInfoService, binary_sha256};
use crate::deploy_drain::DeployDrain;
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::agent_deploys::{DeployRow, deploy_fingerprint_with};
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use rsi_common::agent_deploy::{
    DEPLOY_BINARIES, DEPLOY_BINARY_MISSING, DEPLOY_DIR_NOT_ALLOWED, DEPLOY_GUARD_UNRESOLVABLE,
    DEPLOY_MEMBER_UNRESOLVABLE, DEPLOY_NEEDS_SUPERVISOR, DEPLOY_RESTART_BUDGET,
    DEPLOY_RESTARTS_PER_HOUR_DEFAULT, DEPLOY_RESTARTS_PER_HOUR_MAX, DEPLOY_RESTARTS_PER_HOUR_MIN,
    DEPLOY_RETRY_AFTER_MARKER, DEPLOY_SCHEMA_DOWNGRADE, DEPLOY_SHA_MISMATCH, DEPLOY_STAGE_FAILED,
    DEPLOY_STAGED_CHANGED, DEPLOY_TARGET_MISMATCH, DeployBinaryV1, DeployState, ObjectIdentityV1,
    SUPERVISOR_SHUTDOWN_GRACE_MAX_SECS, SUPERVISOR_SHUTDOWN_GRACE_MIN_SECS, is_deploy_tool_script,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use uuid::Uuid;

/// Publish the validated operator restart budget (line 1) and shutdown grace
/// period (line 2, #2561) for the parent supervisor. The supervisor cannot read
/// the daemon's SQLite store, so rsid mirrors these settings into an atomic
/// launcher snapshot on startup and updates. A supervisor that predates line 2
/// reads only line 1.
pub fn write_supervisor_restart_budget_snapshot(
    path: &Path,
    runtime_config: &crate::config::RuntimeConfig,
) -> std::io::Result<()> {
    use std::io::Write;

    let value = runtime_config
        .deploy_restarts_per_hour
        .load(Ordering::Relaxed);
    if !(DEPLOY_RESTARTS_PER_HOUR_MIN..=DEPLOY_RESTARTS_PER_HOUR_MAX).contains(&value) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "deploy_restarts_per_hour is outside its validated limits",
        ));
    }
    let grace = runtime_config
        .supervisor_shutdown_grace_secs
        .load(Ordering::Relaxed);
    if !(SUPERVISOR_SHUTDOWN_GRACE_MIN_SECS..=SUPERVISOR_SHUTDOWN_GRACE_MAX_SECS).contains(&grace) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "supervisor_shutdown_grace_secs is outside its validated limits",
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "supervisor restart budget path has no parent directory",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    writeln!(temporary, "{value}\n{grace}")?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Poll interval while no deploy is waiting for its quiet point.
const TICK: Duration = Duration::from_secs(5);
/// Poll interval while a staged deploy waits for its quiet point (#2355): the
/// confirming quiet poll lands about one second after the first, not five.
const ACTIVE_TICK: Duration = Duration::from_secs(1);
/// Consecutive quiet polls required before the swap.
pub const QUIET_POLLS_REQUIRED: u32 = 2;
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Wakes the deploy loop out of its idle sleep when a deploy is staged, so the
/// first quiet poll does not wait out a whole [`TICK`] (#2355).
static DEPLOY_WAKE: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Ask the deploy loop to poll now. A permit is kept if the loop is mid-poll.
pub(crate) fn wake_deploy_loop() {
    DEPLOY_WAKE.notify_one();
}

/// How long the deploy loop sleeps after a poll: one second while a staged
/// deploy is waiting for its quiet point, the idle tick otherwise.
fn next_poll_delay(outcome: &Result<PollOutcome>) -> Duration {
    match outcome {
        Ok(PollOutcome::Waiting(_)) => ACTIVE_TICK,
        _ => TICK,
    }
}

static EXIT_75: AtomicBool = AtomicBool::new(false);

/// Ask `main` to exit 75 once the graceful shutdown finishes.
pub fn request_exit_75() {
    EXIT_75.store(true, Ordering::Release);
}

#[must_use]
pub fn exit_75_requested() -> bool {
    EXIT_75.load(Ordering::Acquire)
}

/// Reports a staged binary's `(build sha, schema version)`.
pub type BuildProbe =
    Arc<dyn Fn(&Path) -> std::result::Result<(String, i64), String> + Send + Sync>;
type Trigger = Arc<dyn Fn() + Send + Sync>;

/// Paths, probes and the restart trigger a deploy needs. Injectable in tests.
pub struct DeployService {
    plan: StagePlan,
    supervised: Box<dyn Fn() -> bool + Send + Sync>,
    /// The rsid binary the supervisor relaunches (#1164); `None` skips the
    /// target check (tests, or a supervisor argv that cannot be read).
    supervisor_binary: Box<dyn Fn() -> Option<PathBuf> + Send + Sync>,
    /// The `rsid-supervisor.sh` the running supervisor executes (#2368), read
    /// from its argv; `None` falls back to the directory of the resolved rsid.
    supervisor_script: Box<dyn Fn() -> Option<PathBuf> + Send + Sync>,
    /// Where `bin-<sha>` staging directories live (#2681); `None` prunes
    /// nothing (tests).
    staging_root: Option<PathBuf>,
    restart: Mutex<Option<Trigger>>,
    /// Operator `deploy_restarts_per_hour` (#1777), refreshed by the deploy
    /// runner from the live config.
    restarts_per_hour: AtomicU64,
    /// The supervisor's published restart history (#3492); `None` judges only
    /// the recorded deploys (tests, a host with no supervisor file).
    supervisor_restarts: Option<PathBuf>,
}

/// What staging needs; cloneable so blocking file work leaves the runtime.
#[derive(Clone)]
pub struct StagePlan {
    install_dir: PathBuf,
    /// Where `cargo-slot` and `target-pool` are published (`~/.rsi/bin` for
    /// target-pool and `~/.rsi/lib` for cargo-slot's implementation, #2722).
    tools_dir: Option<PathBuf>,
    allowed_roots: Vec<PathBuf>,
    probe: BuildProbe,
    /// Where the running supervisor reads its script (#2368), set by
    /// [`DeployService::stage_plan`].
    supervisor_script: Option<PathBuf>,
    /// Whether a build must leave a trusted `rsi-repo-guard` beside the `rsid`
    /// and `rsi-rolling-land` it installs (hub #3886). Production only: the
    /// fixtures that deploy a lone `rsid` do not carry one.
    require_guard: bool,
    /// Operator home anchor for installed-guard trust; injected by fixtures.
    guard_home: Option<PathBuf>,
}

static GLOBAL: OnceLock<DeployService> = OnceLock::new();

impl DeployService {
    #[must_use]
    pub fn new(
        install_dir: PathBuf,
        allowed_roots: Vec<PathBuf>,
        supervised: Box<dyn Fn() -> bool + Send + Sync>,
        probe: BuildProbe,
    ) -> Self {
        Self {
            plan: StagePlan {
                install_dir,
                tools_dir: None,
                allowed_roots,
                probe,
                supervisor_script: None,
                require_guard: false,
                guard_home: dirs::home_dir(),
            },
            supervised,
            supervisor_binary: Box::new(|| None),
            supervisor_script: Box::new(|| None),
            staging_root: None,
            restart: Mutex::new(None),
            restarts_per_hour: AtomicU64::new(DEPLOY_RESTARTS_PER_HOUR_DEFAULT),
            supervisor_restarts: None,
        }
    }

    /// Judge deploy admission against the restart history the supervisor
    /// publishes at `path` (#3492), so crash and watchdog restarts count too.
    #[must_use]
    pub fn with_supervisor_restarts(mut self, path: PathBuf) -> Self {
        self.supervisor_restarts = Some(path);
        self
    }

    /// Set the restarts allowed per rolling hour (the operator setting),
    /// clamped to its bounds.
    pub fn set_restarts_per_hour(&self, limit: u64) {
        self.restarts_per_hour.store(
            limit.clamp(DEPLOY_RESTARTS_PER_HOUR_MIN, DEPLOY_RESTARTS_PER_HOUR_MAX),
            Ordering::Relaxed,
        );
    }

    /// Name the rsid binary the supervisor relaunches, so a deploy that would
    /// install somewhere else is refused up front (#1164).
    #[must_use]
    pub fn with_supervisor_binary(
        mut self,
        supervisor_binary: Box<dyn Fn() -> Option<PathBuf> + Send + Sync>,
    ) -> Self {
        self.supervisor_binary = supervisor_binary;
        self
    }

    /// Name the `rsid-supervisor.sh` the running supervisor executes, so a
    /// deploy installs the script where the supervisor refreshes from (#2368).
    #[must_use]
    pub fn with_supervisor_script(
        mut self,
        supervisor_script: Box<dyn Fn() -> Option<PathBuf> + Send + Sync>,
    ) -> Self {
        self.supervisor_script = supervisor_script;
        self
    }

    /// Name the directory used for deploy-managed scripts: target-pool lives
    /// here (`~/.rsi/bin`), and cargo-slot's implementation lives in its
    /// sibling `lib` directory (#2722).
    #[must_use]
    pub fn with_tools_dir(mut self, tools_dir: PathBuf) -> Self {
        self.plan.tools_dir = Some(tools_dir);
        self
    }

    /// Refuse a build, at staging and again after its restart, that would leave
    /// the daemon or the lander with no trusted `rsi-repo-guard` beside it (hub
    /// #3886).
    #[must_use]
    pub fn with_guard_required(mut self) -> Self {
        self.plan.require_guard = true;
        self
    }

    /// Name the directory whose old `bin-<sha>` children a verified deploy
    /// prunes (#2681).
    #[must_use]
    pub fn with_staging_root(mut self, staging_root: PathBuf) -> Self {
        self.staging_root = Some(staging_root);
        self
    }

    /// After `deployed` verified: remove the staging directories no deploy
    /// needs (the deployed and previous build, a live deploy) off the async
    /// runtime, logging what went and the bytes freed (#2681). The keep set is
    /// read here, under the caller's store lock; the removal runs detached.
    pub(crate) fn prune_staging_after_success(
        &self,
        store: &Store,
        deployed: &DeployRow,
    ) -> Option<tokio::task::JoinHandle<crate::deploy_staging_prune::PruneReport>> {
        let root = self.staging_root.clone()?;
        let keep = match crate::deploy_staging_prune::keep_shas(store, deployed) {
            Ok(keep) => keep,
            Err(error) => {
                tracing::warn!(%error, deploy = %deployed.id, "staging prune skipped: keep set unreadable");
                return None;
            }
        };
        Some(tokio::task::spawn_blocking(move || {
            crate::deploy_staging_prune::prune_and_log(&root, &keep)
        }))
    }

    /// Refuse a deploy whose `rsid` install path is not the binary the
    /// supervisor relaunches: the restart would bring the old build back and
    /// verification would fail after a full drain.
    ///
    /// # Errors
    /// `deploy_target_mismatch`, naming both paths.
    pub(crate) fn check_target(&self) -> Result<()> {
        let Some(running) = (self.supervisor_binary)() else {
            return Ok(());
        };
        let installed = self.plan.install_dir.join("rsid");
        let real = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let (running_real, installed_real) = (real(&running), real(&installed));
        if running_real == installed_real {
            return Ok(());
        }
        Err(DaemonError::PolicyDenied(format!(
            "{DEPLOY_TARGET_MISMATCH}: the supervisor runs {} but a deploy installs {}; \
             restart the supervisor on the installed path (make release-install NOW=1)",
            running_real.display(),
            installed_real.display(),
        )))
    }

    fn production(sandbox_base: PathBuf) -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        Self::new(
            home.join(".local/bin"),
            vec![
                sandbox_base,
                rsi_common::identity::data_dir().join("staging"),
                home.join(".cargo/shared-target"),
            ],
            Box::new(|| {
                crate::daemon_info::supervisor_mode().as_deref()
                    == Some(rsi_common::agent_daemon_info::DAEMON_SUPERVISOR_SCRIPT)
            }),
            Arc::new(probe_binary),
        )
        .with_supervisor_binary(Box::new(crate::daemon_info::supervisor_binary))
        .with_supervisor_script(Box::new(crate::daemon_info::supervisor_script))
        .with_tools_dir(home.join(".rsi/bin"))
        .with_guard_required()
        .with_staging_root(rsi_common::identity::data_dir().join("staging"))
        .with_supervisor_restarts(rsi_common::supervisor_restarts::default_path())
    }

    /// Initialise the process-wide service with the configured sandbox base.
    pub fn init_global(sandbox_base: PathBuf) -> &'static Self {
        GLOBAL.get_or_init(|| Self::production(sandbox_base))
    }

    /// Source roots must remain available to future deploys, even while idle.
    pub(crate) fn allowed_roots(&self) -> &[PathBuf] {
        &self.plan.allowed_roots
    }

    pub fn global() -> &'static Self {
        GLOBAL.get_or_init(|| {
            let base = dirs::home_dir().map_or_else(
                || PathBuf::from("/tmp/rsi-sandboxes"),
                |home| home.join(".rsi/sandboxes"),
            );
            Self::production(base)
        })
    }

    /// Register what starts the graceful drain and exit 75.
    pub fn set_restart_trigger(&self, trigger: Trigger) {
        if let Ok(mut slot) = self.restart.lock() {
            *slot = Some(trigger);
        }
    }

    fn fire_restart(&self) -> bool {
        let trigger = self.restart.lock().ok().and_then(|slot| slot.clone());
        trigger.is_some_and(|trigger| {
            trigger();
            true
        })
    }

    #[must_use]
    pub fn is_supervised(&self) -> bool {
        (self.supervised)()
    }

    /// Refuse a restart budget that would starve the supervisor's window.
    ///
    /// # Errors
    /// `deploy_restart_budget retry_after=<RFC3339>` (the time the oldest
    /// restart that must leave the window does, #1777) or a persistence error.
    pub(crate) fn check_budget(&self, store: &Store, now: DateTime<Utc>) -> Result<()> {
        let window = ChronoDuration::hours(1);
        let times = store.agent_deploy_restart_times_since(now - window)?;
        let limit =
            usize::try_from(self.restarts_per_hour.load(Ordering::Relaxed)).unwrap_or(usize::MAX);
        let mut frees = (times.len() >= limit).then(|| {
            // Room opens once enough restarts left the window to get under it.
            times[times.len() - limit] + window
        });
        // #3492: the supervisor spends ONE budget on every restart it makes
        // (deploys, watchdog exits, unannounced signal stops), so a deploy it
        // would refuse or delay is refused here, with the same retry time.
        if let Some(at) = self.supervisor_budget_frees_at(now)
            && frees.is_none_or(|current| at > current)
        {
            frees = Some(at);
        }
        if let Some(frees) = frees {
            return Err(DaemonError::PolicyDenied(format!(
                "{DEPLOY_RESTART_BUDGET}{DEPLOY_RETRY_AFTER_MARKER}{}",
                frees.to_rfc3339_opts(SecondsFormat::Secs, true),
            )));
        }
        Ok(())
    }

    /// When the supervisor's published budget has room for this deploy's
    /// restart plus the slot it reserves for rollback and watchdog recovery;
    /// `None` when it has room now or publishes nothing.
    fn supervisor_budget_frees_at(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let published = rsi_common::supervisor_restarts::SupervisorRestarts::read(
            self.supervisor_restarts.as_deref()?,
        )?;
        let now_secs = now.timestamp();
        let recent = published.in_window(now_secs);
        let allowance = published.deploy_allowance();
        let mut frees_secs = (recent.len() >= allowance)
            .then(|| recent[recent.len() - allowance] + published.window_secs);
        if let Some(until) = published.backoff_until.filter(|until| *until > now_secs) {
            frees_secs = Some(frees_secs.map_or(until, |current| current.max(until)));
        }
        frees_secs.and_then(|secs| DateTime::from_timestamp(secs, 0))
    }

    /// The cloneable staging inputs.
    #[must_use]
    pub fn stage_plan(&self) -> StagePlan {
        StagePlan {
            supervisor_script: (self.supervisor_script)(),
            ..self.plan.clone()
        }
    }

    /// Insert-side helper for the verb: build the row inputs.
    #[must_use]
    pub(crate) fn fingerprint(sha: &str, dir: &str, wait: u32, interrupt_workers: bool) -> String {
        deploy_fingerprint_with(sha, dir, wait, interrupt_workers)
    }
}

impl StagePlan {
    /// Stage, verify and record nothing: copies each binary next to its
    /// install path and returns the manifest. Blocking file work.
    ///
    /// # Errors
    /// A stable `deploy_*` refusal; staged copies are removed on any failure.
    pub fn stage_binaries(
        &self,
        id: Uuid,
        binaries_dir: &str,
        expected_sha: &str,
        live_schema: i64,
    ) -> Result<Vec<DeployBinaryV1>> {
        self.stage_checked(id, binaries_dir, Some(expected_sha), live_schema)
            .map(|(manifest, _)| manifest)
    }

    /// Stage like [`Self::stage_binaries`] for an operator restart (#1122): the
    /// build sha is whatever the probed `rsid` reports.
    ///
    /// # Errors
    /// A stable `deploy_*` refusal; staged copies are removed on any failure.
    pub fn stage_binaries_discovering_sha(
        &self,
        id: Uuid,
        binaries_dir: &str,
        live_schema: i64,
    ) -> Result<(Vec<DeployBinaryV1>, String)> {
        self.stage_checked(id, binaries_dir, None, live_schema)
    }

    fn stage_checked(
        &self,
        id: Uuid,
        binaries_dir: &str,
        expected_sha: Option<&str>,
        live_schema: i64,
    ) -> Result<(Vec<DeployBinaryV1>, String)> {
        let invalid = |msg: String| DaemonError::InvalidParam(msg);
        let dir = std::fs::canonicalize(binaries_dir).map_err(|_| {
            invalid(directory_not_allowed_message(
                binaries_dir,
                "does not exist",
                &self.allowed_roots,
            ))
        })?;
        let allowed = self
            .allowed_roots
            .iter()
            .any(|root| std::fs::canonicalize(root).is_ok_and(|root| dir.starts_with(root)));
        if !allowed {
            return Err(invalid(directory_not_allowed_message(
                binaries_dir,
                "is outside every allowed root",
                &self.allowed_roots,
            )));
        }
        let mut manifest = Vec::new();
        match self.stage_all(id, &dir, &mut manifest, expected_sha, live_schema) {
            Ok(sha) => Ok((manifest, sha)),
            Err(error) => {
                remove_staged(&manifest, id);
                Err(error)
            }
        }
    }

    /// Where `name` is installed. Binaries resolve through `install_dir` links.
    /// The supervisor script goes where the running supervisor reads it (its
    /// argv path), else next to the resolved `rsid`: `install_dir` may hold a
    /// stale regular file the supervisor never executes (#2368).
    fn install_path(&self, name: &str) -> PathBuf {
        let real = |path: PathBuf| std::fs::canonicalize(&path).unwrap_or(path);
        if name == "rsid-supervisor.sh" {
            if let Some(running) = self
                .supervisor_script
                .as_ref()
                .and_then(|path| std::fs::canonicalize(path).ok())
                .filter(|path| path.is_file())
            {
                return running;
            }
            if let Some(parent) = real(self.install_dir.join("rsid")).parent() {
                return parent.join(name);
            }
        }
        if is_deploy_tool_script(name) {
            // cargo-slot's stable bin entry is a thin wrapper. Keep its
            // replaceable implementation beside it in `~/.rsi/lib`.
            if name == "cargo-slot" {
                if let Some(tools) = self.tools_dir.as_ref() {
                    return real(tools.parent().unwrap_or(tools).join("lib/cargo-slot"));
                }
                return real(self.install_dir.join(name));
            }
            // The daemon runs these from `~/.rsi/bin`, not from the binaries'
            // install directory (#2722).
            return real(
                self.tools_dir
                    .as_ref()
                    .unwrap_or(&self.install_dir)
                    .join(name),
            );
        }
        let link = self.install_dir.join(name);
        // A plain member with no link in `install_dir` (absent, or a stale regular
        // file such as the one a pre-#3886 deploy left in `~/.local/bin`) goes
        // beside the resolved `rsid`, like every member `make release-install`
        // links, so the sibling lookup of the daemon and the lander finds it.
        // The deploy then points the `install_dir` link at it (`link_members`).
        if name != "rsid"
            && !std::fs::symlink_metadata(&link).is_ok_and(|meta| meta.file_type().is_symlink())
        {
            return self.new_member_paths(name).0;
        }
        real(link)
    }

    fn stage_all(
        &self,
        id: Uuid,
        dir: &Path,
        manifest: &mut Vec<DeployBinaryV1>,
        expected_sha: Option<&str>,
        live_schema: i64,
    ) -> Result<String> {
        let invalid = |msg: String| DaemonError::InvalidParam(msg);
        for name in DEPLOY_BINARIES {
            let source = dir.join(name);
            let is_file =
                std::fs::symlink_metadata(&source).is_ok_and(|meta| meta.file_type().is_file());
            if !is_file {
                if name == "rsid" {
                    return Err(invalid(format!(
                        "{DEPLOY_BINARY_MISSING}: member rsid is not a regular file in {}",
                        dir.display()
                    )));
                }
                continue;
            }
            let dest = self.install_path(name);
            let fail = |step: &str, detail: String| stage_failure(name, step, &detail);
            let parent = dest.parent().ok_or_else(|| {
                fail(
                    "resolve install directory",
                    format!("{} has no parent", dest.display()),
                )
            })?;
            let staged = staged_path(&dest, id);
            let source_hash = binary_sha256(&source)
                .ok_or_else(|| fail("hash source", format!("cannot read {}", source.display())))?;
            std::fs::create_dir_all(parent).map_err(|error| {
                fail(
                    "create install directory",
                    format!("{}: {error}", parent.display()),
                )
            })?;
            let prior_present = std::fs::symlink_metadata(&dest).is_ok();
            std::fs::copy(&source, &staged).map_err(|error| {
                fail(
                    "copy",
                    format!("{} to {}: {error}", source.display(), staged.display()),
                )
            })?;
            manifest.push(DeployBinaryV1 {
                name: name.to_string(),
                dest: dest.to_string_lossy().into_owned(),
                sha256: source_hash.clone(),
                prior_present,
                staged_identity: None,
            });
            set_executable(&staged)
                .map_err(|error| fail("chmod", format!("{}: {error}", staged.display())))?;
            if binary_sha256(&staged).as_deref() != Some(source_hash.as_str()) {
                return Err(fail(
                    "verify copy hash",
                    format!(
                        "{} does not match the source hash {source_hash}",
                        staged.display()
                    ),
                ));
            }
            if name == "rsid-supervisor.sh" || name == "cargo-slot" || name == "target-pool" {
                script_syntax_check(&staged).map_err(|reason| fail("syntax check", reason))?;
            }
            let identity = object_identity(&staged).ok_or_else(|| {
                fail(
                    "record staged identity",
                    format!("{} is not a regular file", staged.display()),
                )
            })?;
            if let Some(entry) = manifest.last_mut() {
                entry.staged_identity = Some(identity);
            }
        }
        self.stage_cargo_slot_wrapper(id, manifest)
            .map_err(|error| {
                stage_failure(CARGO_SLOT_WRAPPER_NAME, "stage wrapper", &error.to_string())
            })?;
        // Hub #3886: refuse now a build whose rsid or lander would run with no
        // trusted guard beside it, rather than let it go live and refuse landings.
        if let Some(reason) = self.guard_unresolvable(manifest, true) {
            tracing::warn!(%reason, "deploy refused at staging: no trusted rsi-repo-guard");
            return Err(invalid(reason));
        }
        let rsid = manifest
            .iter()
            .find(|entry| entry.name == "rsid")
            .ok_or_else(|| {
                invalid(format!(
                    "{DEPLOY_BINARY_MISSING}: member rsid was not staged"
                ))
            })?;
        let (sha, schema) = (self.probe)(&staged_path(Path::new(&rsid.dest), id))
            .map_err(|error| stage_failure("rsid", "probe --build-info", &error))?;
        if expected_sha.is_some_and(|expected| sha != expected) {
            let requested = expected_sha.unwrap();
            return Err(invalid(format!(
                "{DEPLOY_SHA_MISMATCH}: requested SHA {} but binaries contain SHA {}",
                requested, sha
            )));
        }
        if schema < live_schema {
            return Err(invalid(DEPLOY_SCHEMA_DOWNGRADE.to_string()));
        }
        Ok(sha)
    }
}

impl StagePlan {
    /// The stable `~/.rsi/bin/cargo-slot` entry is a daemon-managed wrapper that
    /// execs the implementation in `~/.rsi/lib` (#2777). Stage it whenever the
    /// bin entry is absent or not that wrapper (a host upgraded from a daemon
    /// that published the full script into `bin`, or one that never had it), so
    /// the layout is complete without `install-release.sh`. It is appended
    /// after the implementation entry, so the swap publishes `lib` first and a
    /// rollback never leaves a wrapper pointing at nothing. With no
    /// implementation staged or installed it is left alone.
    fn stage_cargo_slot_wrapper(
        &self,
        id: Uuid,
        manifest: &mut Vec<DeployBinaryV1>,
    ) -> std::io::Result<()> {
        let Some(tools) = self.tools_dir.as_ref() else {
            return Ok(());
        };
        let implementation = self.install_path("cargo-slot");
        let staging_impl = manifest.iter().any(|entry| entry.name == "cargo-slot");
        if !staging_impl && !implementation.is_file() {
            return Ok(());
        }
        let dest = std::fs::canonicalize(tools)
            .unwrap_or_else(|_| tools.clone())
            .join("cargo-slot");
        if is_cargo_slot_wrapper(&dest) {
            return Ok(());
        }
        let prior_present = std::fs::symlink_metadata(&dest).is_ok();
        let staged = staged_path(&dest, id);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let sha256 = {
            use sha2::Digest;
            hex::encode(sha2::Sha256::digest(CARGO_SLOT_WRAPPER))
        };
        std::fs::write(&staged, CARGO_SLOT_WRAPPER)?;
        manifest.push(DeployBinaryV1 {
            name: CARGO_SLOT_WRAPPER_NAME.to_string(),
            dest: dest.to_string_lossy().into_owned(),
            sha256,
            prior_present,
            staged_identity: None,
        });
        set_executable(&staged)?;
        let identity = object_identity(&staged)
            .ok_or_else(|| std::io::Error::other("staged wrapper vanished"))?;
        if let Some(entry) = manifest.last_mut() {
            entry.staged_identity = Some(identity);
        }
        Ok(())
    }
}

/// The wrapper the daemon publishes at `~/.rsi/bin/cargo-slot`.
pub(crate) const CARGO_SLOT_WRAPPER: &[u8] = include_bytes!("../../../scripts/cargo-slot-wrapper");

/// Manifest name of the wrapper entry (not a `DEPLOY_BINARIES` member: it is
/// derived from `cargo-slot`, never carried in the binaries directory).
const CARGO_SLOT_WRAPPER_NAME: &str = "cargo-slot-wrapper";

/// Whether `path` is a regular, executable file holding exactly the wrapper.
pub(crate) fn is_cargo_slot_wrapper(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        && std::fs::read(path).is_ok_and(|bytes| bytes == CARGO_SLOT_WRAPPER)
}

fn staged_path(dest: &Path, id: Uuid) -> PathBuf {
    let name = dest
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    dest.with_file_name(format!(".{name}.deploy-{id}"))
}

fn prev_path(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    dest.with_file_name(format!("{name}.prev"))
}

/// `<real rsid>.deploy-inflight`: written right before the deploy restart so
/// `rsid-supervisor.sh` may restore `.prev` only for a deploy in flight.
fn marker_path(manifest: &[DeployBinaryV1]) -> Option<PathBuf> {
    let rsid = manifest.iter().find(|entry| entry.name == "rsid")?;
    let dest = Path::new(&rsid.dest);
    let name = dest.file_name()?.to_string_lossy().into_owned();
    Some(dest.with_file_name(format!("{name}.deploy-inflight")))
}

fn write_marker(manifest: &[DeployBinaryV1], database: Option<&str>) -> std::io::Result<()> {
    let path = marker_path(manifest)
        .ok_or_else(|| std::io::Error::other("deploy manifest has no rsid entry"))?;
    std::fs::write(
        path,
        database.map_or_else(
            || "deploy in flight".into(),
            |path| format!("database={path}"),
        ),
    )
}

fn remove_marker(manifest: &[DeployBinaryV1]) {
    if let Some(path) = marker_path(manifest)
        && let Err(error) = std::fs::remove_file(&path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, path = %path.display(), "deploy in-flight marker not removed");
    }
}

/// Drop the `.prev` files of a verified deploy so a later unrelated crash can
/// never restore a stale binary. A failure is logged, not fatal.
fn remove_prev_files(manifest: &[DeployBinaryV1]) {
    for entry in manifest {
        let prev = prev_path(Path::new(&entry.dest));
        match std::fs::remove_file(&prev) {
            Ok(()) => tracing::info!(path = %prev.display(), "removed .prev after verified deploy"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(%error, path = %prev.display(), ".prev not removed after verified deploy");
            }
        }
    }
}

fn set_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

/// What [`StagePlan::complete_upgrade_members`] did for one verified deploy.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct UpgradeMembersReport {
    /// Members installed from the verified stage that the staging daemon's
    /// older compiled inventory did not carry.
    pub installed: Vec<String>,
    /// Members the stage holds but that could not be installed, with the cause.
    pub failed: Vec<(String, String)>,
}

impl UpgradeMembersReport {
    /// A note for the deploy's settled reason; `None` when nothing happened.
    fn note(&self) -> Option<String> {
        let mut parts = Vec::new();
        if !self.installed.is_empty() {
            parts.push(format!(
                "installed new members from the verified stage: {}",
                self.installed.join(", ")
            ));
        }
        if !self.failed.is_empty() {
            let failed: Vec<String> = self
                .failed
                .iter()
                .map(|(name, cause)| format!("{name} ({cause})"))
                .collect();
            parts.push(format!(
                "PARTIAL INSTALL, members left uninstalled: {}; \
                 re-run the deploy or `make release-install NOW=1`",
                failed.join(", ")
            ));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

/// The manifest `install-release.sh --stage-dir` writes into a stage.
const STAGE_MANIFEST_NAME: &str = "MANIFEST.sha256";

/// Parse a stage manifest: one `<sha256>  <relative path>` line per file
/// (`sha256sum` text or binary mode). A malformed line makes the whole
/// manifest untrusted.
fn parse_stage_manifest(text: &str) -> std::result::Result<HashMap<String, String>, String> {
    let mut hashes = HashMap::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let (digest, rest) = line
            .split_once(' ')
            .ok_or_else(|| format!("malformed {STAGE_MANIFEST_NAME} line"))?;
        let path = rest.strip_prefix([' ', '*']).unwrap_or(rest);
        let valid = digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit());
        if !valid || path.is_empty() {
            return Err(format!("malformed {STAGE_MANIFEST_NAME} line"));
        }
        hashes.insert(path.to_string(), digest.to_ascii_lowercase());
    }
    Ok(hashes)
}

/// The stage's manifest hashes; the error names why they cannot be trusted.
fn read_stage_manifest(dir: &Path) -> std::result::Result<HashMap<String, String>, String> {
    let text = std::fs::read_to_string(dir.join(STAGE_MANIFEST_NAME))
        .map_err(|_| format!("stage {STAGE_MANIFEST_NAME} unreadable, not installed"))?;
    parse_stage_manifest(&text).map_err(|cause| format!("{cause}, not installed"))
}

/// The verified stage directory of `sha`: a real `bin-<hex>` directory under
/// `root` whose hex part is a prefix of `sha` (a deploy row stores no
/// directory, #2681).
fn stage_dir_for(root: &Path, sha: &str) -> Option<PathBuf> {
    std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| {
            let name = entry.file_name();
            name.to_str()
                .and_then(|name| name.strip_prefix("bin-"))
                .is_some_and(|hex| hex.len() >= 7 && sha.starts_with(hex))
        })
        .map(|entry| entry.path())
        .next()
}

impl StagePlan {
    /// Where a plain binary that has no install path yet goes: beside the
    /// resolved `rsid` (so `~/.local/bin` links to the install directory as it
    /// does for every other member), plus the `install_dir` link that names it.
    fn new_member_paths(&self, name: &str) -> (PathBuf, Option<PathBuf>) {
        let link = self.install_dir.join(name);
        let rsid = std::fs::canonicalize(self.install_dir.join("rsid"))
            .unwrap_or_else(|_| self.install_dir.join("rsid"));
        let dest = rsid
            .parent()
            .map_or_else(|| link.clone(), |parent| parent.join(name));
        let needs_link = dest != link;
        (dest, needs_link.then_some(link))
    }

    /// The directory each installed consumer of the guard runs from, with the
    /// consumer's name: the `rsid` this deploy installs and the
    /// `rsi-rolling-land` it installs or that is already installed.
    fn guard_consumers(&self, manifest: &[DeployBinaryV1]) -> Vec<(&'static str, PathBuf)> {
        let mut consumers = Vec::new();
        for consumer in ["rsid", "rsi-rolling-land"] {
            let dir = manifest
                .iter()
                .find(|entry| entry.name == consumer)
                .map(|entry| PathBuf::from(&entry.dest))
                .or_else(|| {
                    let installed = self.install_dir.join(consumer);
                    std::fs::symlink_metadata(&installed)
                        .is_ok()
                        .then(|| std::fs::canonicalize(&installed).unwrap_or(installed))
                })
                .and_then(|path| path.parent().map(Path::to_path_buf));
            if let Some(dir) = dir {
                consumers.push((consumer, dir));
            }
        }
        consumers
    }

    /// Why the build `manifest` installs cannot resolve a trusted guard, or
    /// `None` (hub #3886). Each consumer's directory must hold what
    /// [`crate::repo_guard_call::installed_tool_in`] accepts, the very lookup the
    /// running binary makes, and an identity record there must not refuse it.
    /// `planned` is the staging-time answer: a guard this manifest swaps into the
    /// directory counts as present. Always `None` unless the plan requires one.
    pub(crate) fn guard_unresolvable(
        &self,
        manifest: &[DeployBinaryV1],
        planned: bool,
    ) -> Option<String> {
        if !self.require_guard {
            return None;
        }
        let guard = crate::repo_guard_call::GUARD_BIN;
        for (consumer, dir) in self.guard_consumers(manifest) {
            use crate::repo_guard_call::{
                InstalledToolError, installed_guard_dir_in_home, installed_tool_in_home,
            };
            let refusal = |why: InstalledToolError| {
                format!(
                    "{DEPLOY_GUARD_UNRESOLVABLE}: {guard} beside {consumer} in {} is refused: {why}",
                    dir.display()
                )
            };
            let Some(home) = self.guard_home.as_deref() else {
                return Some(refusal(InstalledToolError::HomeUnresolvable));
            };
            // A manifest entry is not a trust grant. Even a future regular
            // guard must land in a protected directory, not a cargo target.
            if let Err(why) = installed_guard_dir_in_home(&dir, home) {
                return Some(refusal(why));
            }
            let staged_here = planned
                && manifest
                    .iter()
                    .any(|entry| entry.name == guard && Path::new(&entry.dest) == dir.join(guard));
            if staged_here {
                continue;
            }
            let tool = match installed_tool_in_home(&dir, home) {
                Ok(tool) => tool,
                Err(why) => return Some(refusal(why)),
            };
            if let crate::repo_guard_call::Identity::Refused(why) =
                crate::repo_guard_call::check_identity(&tool, None)
            {
                return Some(format!(
                    "{DEPLOY_GUARD_UNRESOLVABLE}: {} beside {consumer} is refused: {why}",
                    tool.display()
                ));
            }
        }
        None
    }

    /// The plain binary members of `manifest` (not `rsid`, the supervisor or a
    /// tool script): the ones that live beside `rsid` as `make release-install`
    /// lays them out and that the daemon and the lander find by that sibling
    /// lookup (`rsi-repo-guard`, `rsi-eval`, `rsi-rpc`, `rsi-agent-mcp`, ...).
    fn plain_members<'a>(
        manifest: &'a [DeployBinaryV1],
    ) -> impl Iterator<Item = &'a DeployBinaryV1> {
        manifest.iter().filter(|entry| {
            DEPLOY_BINARIES.contains(&entry.name.as_str())
                && entry.name != "rsid"
                && entry.name != "rsid-supervisor.sh"
                && !is_deploy_tool_script(&entry.name)
        })
    }

    /// The directory the deployed `rsid` runs from.
    fn rsid_dir(&self, manifest: &[DeployBinaryV1]) -> Option<PathBuf> {
        self.guard_consumers(manifest)
            .into_iter()
            .find(|(name, _)| *name == "rsid")
            .map(|(_, dir)| dir)
    }

    /// Lay the links of the plain members `manifest` installed (hub #3886). With
    /// `install_dir_links` false: for a member an older staging daemon put outside
    /// the directory of `rsid`, a link beside `rsid` so the sibling lookup finds
    /// it (before verification, and harmless to a rollback). With it true: the
    /// `install_dir` link (`~/.local/bin/<name>`), only after verification passed,
    /// because replacing a stale file there before a rollback could loop a restored
    /// hand-made link. Each link is a temporary symlink renamed
    /// over the old one, so a stale regular file or a hand-made link is replaced
    /// atomically; a regular file beside `rsid` that is not ours is left and
    /// reported. Returns `(member, cause)` for each link it could not make: the
    /// install itself is unaffected, so the caller reports it and carries on.
    pub(crate) fn link_members(
        &self,
        manifest: &[DeployBinaryV1],
        install_dir_links: bool,
    ) -> Vec<(String, String)> {
        let mut failed = Vec::new();
        let rsid_dir = self.rsid_dir(manifest);
        for entry in Self::plain_members(manifest) {
            let dest = Path::new(&entry.dest);
            let mut links = Vec::new();
            if install_dir_links {
                links.push((self.install_dir.join(&entry.name), true));
            } else if let Some(dir) = &rsid_dir {
                links.push((dir.join(&entry.name), false));
            }
            for (link, replace_regular) in links {
                if link == dest {
                    continue;
                }
                if let Err(cause) = point_link(&link, dest, replace_regular) {
                    failed.push((entry.name.clone(), format!("{}: {cause}", link.display())));
                }
            }
        }
        failed
    }

    /// Why a plain member of the deployed build is not where the daemon and the
    /// lander look for it (hub #3886): after [`Self::link_members`], the file
    /// beside `rsid` must be the very file this deploy installed. `None` unless
    /// the plan checks the layout.
    pub(crate) fn member_unresolvable(&self, manifest: &[DeployBinaryV1]) -> Option<String> {
        if !self.require_guard {
            return None;
        }
        let dir = self.rsid_dir(manifest)?;
        for entry in Self::plain_members(manifest) {
            let beside = dir.join(&entry.name);
            if binary_sha256(&beside).as_deref() != Some(entry.sha256.as_str()) {
                return Some(format!(
                    "{DEPLOY_MEMBER_UNRESOLVABLE}: {} is not the deployed {} (installed at {}); \
                     the daemon and the lander resolve it beside rsid",
                    beside.display(),
                    entry.name,
                    entry.dest
                ));
            }
        }
        None
    }

    /// Finish an upgrade whose staging daemon predates a member (#587).
    ///
    /// `stage_all` stages only the members the *old* daemon compiled in, so a
    /// commit that adds a member cannot carry it on its first upgrade even
    /// when the verified stage holds it. The new daemon knows the full
    /// inventory: once the deploy verified, install each plain binary of
    /// [`DEPLOY_BINARIES`] that the manifest skipped and the stage `dir`
    /// holds, with the same copy, mode and sha-256 checks, and no change to
    /// the allowlist. `rsid`, the supervisor and the tool scripts keep their
    /// manifest-driven swap and rollback; they are never installed here. A
    /// member already installed at the staged hash is left alone.
    pub(crate) fn complete_upgrade_members(
        &self,
        dir: &Path,
        manifest: &[DeployBinaryV1],
        id: Uuid,
    ) -> UpgradeMembersReport {
        let mut report = UpgradeMembersReport::default();
        let manifest_hashes = read_stage_manifest(dir);
        for name in DEPLOY_BINARIES {
            let special =
                name == "rsid" || name == "rsid-supervisor.sh" || is_deploy_tool_script(name);
            if special || manifest.iter().any(|entry| entry.name == name) {
                continue;
            }
            let source = dir.join(name);
            if !std::fs::symlink_metadata(&source).is_ok_and(|meta| meta.file_type().is_file()) {
                continue;
            }
            let expected = match manifest_hashes.as_ref() {
                Ok(hashes) => hashes.get(name).map(String::as_str).ok_or_else(|| {
                    format!("no entry in the stage {STAGE_MANIFEST_NAME}, not installed")
                }),
                Err(cause) => Err(cause.clone()),
            };
            let installed =
                expected.and_then(|expected| self.install_new_member(name, &source, expected, id));
            match installed {
                Ok(true) => report.installed.push(name.to_string()),
                Ok(false) => {}
                Err(cause) => report.failed.push((name.to_string(), cause)),
            }
        }
        report
    }

    /// `Ok(true)` when installed, `Ok(false)` when already current. The staged
    /// bytes must hash to `expected`, the stage manifest entry, before any
    /// destination is touched (hub #3856); that same hash then guards the copy.
    fn install_new_member(
        &self,
        name: &str,
        source: &Path,
        expected: &str,
        id: Uuid,
    ) -> std::result::Result<bool, String> {
        let source_hash = binary_sha256(source).ok_or("stage unreadable")?;
        if source_hash != expected {
            return Err(format!(
                "staged {name} hashes to {source_hash} but the stage {STAGE_MANIFEST_NAME} lists {expected}, not installed"
            ));
        }
        let existing = {
            let installed = self.install_path(name);
            std::fs::symlink_metadata(&installed)
                .is_ok()
                .then_some(installed)
        };
        let (dest, link) = match existing {
            Some(path) if binary_sha256(&path).as_deref() == Some(source_hash.as_str()) => {
                return Ok(false);
            }
            Some(path) => (path, None),
            None => self.new_member_paths(name),
        };
        let _guard = INSTALL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let staged = staged_path(&dest, id);
        let copy = || -> std::io::Result<()> {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(source, &staged)?;
            set_executable(&staged)?;
            if binary_sha256(&staged).as_deref() != Some(source_hash.as_str()) {
                return Err(std::io::Error::other("staged copy hash mismatch"));
            }
            std::fs::rename(&staged, &dest)
        };
        if let Err(error) = copy() {
            let _ = std::fs::remove_file(&staged);
            return Err(error.to_string());
        }
        if binary_sha256(&dest).as_deref() != Some(source_hash.as_str()) {
            return Err("installed hash mismatch".to_string());
        }
        let Some(link) = link else {
            return Ok(true);
        };
        if std::fs::symlink_metadata(&link).is_ok() {
            return Ok(true);
        }
        std::os::unix::fs::symlink(&dest, &link).map_err(|error| error.to_string())?;
        Ok(true)
    }
}

impl DeployService {
    /// After `deployed` verified: complete the members its staging daemon did
    /// not know (#587), logging the outcome. Returns the note for the settled
    /// reason. Blocking file work, called under the verify step.
    pub(crate) fn complete_upgrade_members(&self, deployed: &DeployRow) -> Option<String> {
        let root = self.staging_root.as_ref()?;
        let dir = stage_dir_for(root, &deployed.sha)?;
        let report = self
            .plan
            .complete_upgrade_members(&dir, &deployed.manifest, deployed.id);
        for name in &report.installed {
            tracing::info!(member = %name, deploy = %deployed.id, "installed upgrade-boundary deploy member from the verified stage");
        }
        for (name, cause) in &report.failed {
            tracing::warn!(member = %name, %cause, deploy = %deployed.id, "upgrade-boundary deploy member left uninstalled (partial install)");
        }
        report.note()
    }
}

/// Remove staged copies of a manifest (best effort).
pub(crate) fn remove_staged(manifest: &[DeployBinaryV1], id: Uuid) {
    for entry in manifest {
        let _ = std::fs::remove_file(staged_path(Path::new(&entry.dest), id));
    }
}

/// Why a swap stopped.
#[derive(Debug)]
enum SwapError {
    /// A staged copy no longer hashes to its verified sha256.
    StagedChanged,
    Io(std::io::Error),
}

impl std::fmt::Display for SwapError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StagedChanged => formatter.write_str(DEPLOY_STAGED_CHANGED),
            Self::Io(error) => write!(formatter, "swap failed: {error}"),
        }
    }
}

/// Identity (device, inode) of the file object at `path`, without following a
/// symlink; `None` when it is absent or not a regular file.
fn object_identity(path: &Path) -> Option<ObjectIdentityV1> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).ok()?;
    meta.file_type().is_file().then(|| ObjectIdentityV1 {
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

/// Make `link` a symlink to `dest`, atomically: a temporary symlink in the same
/// directory is renamed over the old entry. A link that already resolves to
/// `dest` stays. A regular file at `link` is replaced only when
/// `replace_regular`; otherwise it is reported, never overwritten.
fn point_link(link: &Path, dest: &Path, replace_regular: bool) -> std::io::Result<()> {
    match std::fs::symlink_metadata(link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if std::fs::canonicalize(link).ok() == std::fs::canonicalize(dest).ok() {
                return Ok(());
            }
        }
        Ok(meta) if meta.file_type().is_file() && !replace_regular => {
            return Err(std::io::Error::other(
                "a regular file that is not the deployed member is in the way",
            ));
        }
        Ok(meta) if meta.file_type().is_dir() => {
            return Err(std::io::Error::other("a directory is in the way"));
        }
        _ => {}
    }
    let (Some(parent), Some(name)) = (link.parent(), link.file_name()) else {
        return Err(std::io::Error::other("link has no parent"));
    };
    let temp = parent.join(format!(
        ".{}.link-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&temp);
    std::fs::create_dir_all(parent)?;
    let made = std::os::unix::fs::symlink(dest, &temp).and_then(|()| std::fs::rename(&temp, link));
    if made.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    made
}

/// Serializes every mutation and removal of an install destination: the swap
/// and the rollback both hold it (#1127).
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

fn install_lock() -> std::sync::MutexGuard<'static, ()> {
    INSTALL_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Rename staged copies over the install paths, keeping `<name>.prev`. Each
/// staged copy is re-hashed immediately before its rename, and must still be
/// the object recorded at staging, so a copy changed since staging is never
/// installed. Undoes any partial swap on failure.
fn swap_in(manifest: &[DeployBinaryV1], id: Uuid) -> std::result::Result<(), SwapError> {
    let _lock = install_lock();
    let mut done: Vec<&DeployBinaryV1> = Vec::new();
    for entry in manifest {
        let dest = Path::new(&entry.dest);
        let staged = staged_path(dest, id);
        if binary_sha256(&staged).as_deref() != Some(entry.sha256.as_str())
            || (entry.staged_identity.is_some()
                && object_identity(&staged) != entry.staged_identity)
        {
            rollback_locked(&done);
            return Err(SwapError::StagedChanged);
        }
        let step = (|| {
            if dest.exists() {
                std::fs::rename(dest, prev_path(dest))?;
            }
            std::fs::rename(&staged, dest)
        })();
        if let Err(error) = step {
            if !dest.exists() && prev_path(dest).exists() {
                let _ = std::fs::rename(prev_path(dest), dest);
            }
            rollback_locked(&done);
            return Err(SwapError::Io(error));
        }
        done.push(entry);
    }
    Ok(())
}

/// Restore the prior set: `<name>.prev` over each install path that has one,
/// and removal of a binary this deploy installed that was absent before it
/// (an optional first-time install). Only the exact file object this deploy
/// installed is removed; anything else at the path is not ours (#1114, #1127).
fn rollback(manifest: &[&DeployBinaryV1]) {
    let _lock = install_lock();
    rollback_locked(manifest);
}

/// [`rollback`] with the install lock already held.
fn rollback_locked(manifest: &[&DeployBinaryV1]) {
    for entry in manifest {
        let dest = Path::new(&entry.dest);
        let prev = prev_path(dest);
        if prev.exists() {
            let _ = std::fs::rename(&prev, dest);
        } else if !entry.prior_present
            && let Some(identity) = entry.staged_identity
        {
            remove_if_installed(dest, identity);
        }
    }
}

/// Remove `dest` only when it is the object `identity`. The path is first moved
/// aside atomically, so the identity is checked on the very object that is then
/// unlinked: a replacement installed at any moment either is moved aside and
/// put back untouched, or lands after the move and is never touched.
fn remove_if_installed(dest: &Path, identity: ObjectIdentityV1) {
    let name = dest
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let aside = dest.with_file_name(format!(".{name}.rollback-{}", std::process::id()));
    if std::fs::rename(dest, &aside).is_err() {
        return;
    }
    #[cfg(test)]
    after_move_aside_hook();
    if object_identity(&aside) == Some(identity) {
        let _ = std::fs::remove_file(&aside);
        return;
    }
    // Not ours: put it back, never over something installed since.
    #[cfg(target_os = "linux")]
    let restored = nix::fcntl::renameat2(
        None,
        &aside,
        None,
        dest,
        nix::fcntl::RenameFlags::RENAME_NOREPLACE,
    )
    .map_err(std::io::Error::from);
    #[cfg(target_os = "macos")]
    let restored = restore_without_replacing(&aside, dest);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let restored: std::io::Result<()> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-replace restore unavailable",
    ));
    if let Err(error) = restored {
        tracing::warn!(%error, path = %aside.display(), "rollback kept a replaced file aside");
    }
}

#[cfg(target_os = "macos")]
fn restore_without_replacing(aside: &Path, dest: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let aside = CString::new(aside.as_os_str().as_bytes())?;
    let dest = CString::new(dest.as_os_str().as_bytes())?;
    // SAFETY: both paths are NUL-terminated and live through the syscall.
    // RENAME_EXCL atomically refuses an occupied destination, matching Linux.
    let result = unsafe {
        nix::libc::renameatx_np(
            nix::libc::AT_FDCWD,
            aside.as_ptr(),
            nix::libc::AT_FDCWD,
            dest.as_ptr(),
            nix::libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
static AFTER_MOVE_ASIDE: Mutex<Option<Box<dyn Fn() + Send>>> = Mutex::new(None);

#[cfg(test)]
fn after_move_aside_hook() {
    if let Some(hook) = AFTER_MOVE_ASIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
    {
        hook();
    }
}

/// #3423: the `deploy_directory_not_allowed` refusal names the directory it
/// received and the roots it accepts, so an operator whose build output sits
/// elsewhere sees the fix. The stable code stays first: the satellite
/// pass-through (`satellite::deploy_link`) reads it there.
pub(crate) fn directory_not_allowed_message(
    received: &str,
    why: &str,
    allowed_roots: &[PathBuf],
) -> String {
    let roots = allowed_roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{DEPLOY_DIR_NOT_ALLOWED}: {received} {why}; allowed roots: {roots} \
         (stage the build into a bin-<sha> directory under the staging root)"
    )
}

/// Longest reason text a stage refusal carries (bytes).
const STAGE_REASON_LIMIT: usize = 512;

/// The typed `deploy_stage_failed` refusal: it names the staged member, the
/// step that failed and a bounded reason, and logs the same text (hub #3884).
fn stage_failure(member: &str, step: &str, reason: &str) -> DaemonError {
    let mut reason = reason.trim().to_string();
    if reason.len() > STAGE_REASON_LIMIT {
        let mut cut = STAGE_REASON_LIMIT;
        while !reason.is_char_boundary(cut) {
            cut -= 1;
        }
        reason.truncate(cut);
        reason.push_str("...");
    }
    let message = format!("{DEPLOY_STAGE_FAILED}: member {member}: {step} failed: {reason}");
    tracing::warn!(member, step, %reason, "deploy refused at staging");
    DaemonError::InvalidParam(message)
}

/// Syntax-check a staged shell script with the interpreter its shebang names
/// (`sh`/`bash`, directly or through `env`); no shebang means bash. Any other
/// interpreter is refused. No Python (operator directive).
fn script_syntax_check(path: &Path) -> std::result::Result<(), String> {
    let head = std::fs::read(path).map_err(|error| format!("cannot read script: {error}"))?;
    let first = head.split(|byte| *byte == b'\n').next().unwrap_or_default();
    let interpreter = if first.starts_with(b"#!") {
        let line = String::from_utf8_lossy(&first[2..]).into_owned();
        let mut words = line.split_whitespace();
        let program = words.next().unwrap_or_default();
        let program = if program.rsplit('/').next() == Some("env") {
            words
                .find(|word| !word.starts_with('-'))
                .unwrap_or_default()
        } else {
            program
        };
        match program.rsplit('/').next().unwrap_or_default() {
            "sh" => "sh",
            "bash" => "bash",
            other => {
                return Err(format!(
                    "unsupported script interpreter {other:?} in shebang {line:?}"
                ));
            }
        }
    } else {
        "bash"
    };
    let output = std::process::Command::new(interpreter)
        .arg("-n")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("cannot run {interpreter} -n: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{interpreter} -n exited {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    ))
}

/// Production probe: `<binary> --build-info` prints `<sha> <schema>`.
fn probe_binary(path: &Path) -> std::result::Result<(String, i64), String> {
    let mut child = std::process::Command::new(path)
        .arg("--build-info")
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait().map_err(|error| error.to_string())? {
            Some(status) if status.success() => break,
            Some(_) => return Err("probe exited non-zero".into()),
            None if started.elapsed() > PROBE_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("probe timed out".into());
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let mut text = String::new();
    std::io::Read::read_to_string(
        &mut child.stdout.take().ok_or("no probe output")?,
        &mut text,
    )
    .map_err(|error| error.to_string())?;
    parse_build_info(&text).ok_or_else(|| "unparseable probe output".into())
}

/// Supervisor-only offline recovery, before starting the previous binary.
/// The caller holds the database instance lease.
pub fn restore_database_for_binary(database: &Path, binary: &Path) -> Result<()> {
    let (_, supported) = probe_binary(binary).map_err(crate::error::DaemonError::Store)?;
    let supported = i32::try_from(supported)
        .map_err(|_| crate::error::DaemonError::Store("invalid binary schema version".into()))?;
    if let Some(backup) = crate::store::migration_backup::restore(database, supported)? {
        eprintln!(
            "restored database from pre-migration backup {}",
            backup.display()
        );
    }
    Ok(())
}

/// Keep the recovery-capable new executable and ask the supervisor to restore
/// offline, after every Store connection in this incarnation has closed.
fn prepare_database_rollback(
    store: &Store,
    service: &DeployService,
    row: &DeployRow,
) -> Result<Option<PathBuf>> {
    let Some(database) = store.database_path().map(Path::new) else {
        return Ok(None);
    };
    let Some(rsid) = row.manifest.iter().find(|entry| entry.name == "rsid") else {
        return Ok(None);
    };
    let dest = Path::new(&rsid.dest);
    let previous = prev_path(dest);
    if !previous.exists() {
        return Ok(None);
    }
    let (_, supported) =
        (service.plan.probe)(&previous).map_err(crate::error::DaemonError::Store)?;
    let supported = i32::try_from(supported)
        .map_err(|_| crate::error::DaemonError::Store("invalid binary schema version".into()))?;
    let live = store.schema_version()?;
    if live <= supported {
        return Ok(None);
    }
    let marker = crate::store::migration_backup::read_marker(database)?.ok_or_else(|| {
        crate::error::DaemonError::Store(
            "binary rollback refused: pre-migration backup missing".into(),
        )
    })?;
    if !crate::store::migration_backup::needs_restore(live, supported, &marker)
        || !marker.path.exists()
    {
        return Err(crate::error::DaemonError::Store(
            "binary rollback refused: compatible pre-migration backup missing".into(),
        ));
    }
    // The existing rollback() restores all installed binaries. Save this one
    // first, because the old binary does not have the recovery command.
    let failed = dest.with_file_name(format!(
        "{}.failed",
        dest.file_name().unwrap().to_string_lossy()
    ));
    std::fs::copy(dest, &failed)?;
    std::fs::write(
        dest.with_file_name(format!(
            "{}.db-rollback",
            dest.file_name().unwrap().to_string_lossy()
        )),
        database.as_os_str().as_encoded_bytes(),
    )?;
    Ok(Some(marker.path))
}

/// `<40-hex sha> <schema>` as printed by `rsid --build-info`.
#[must_use]
pub fn parse_build_info(text: &str) -> Option<(String, i64)> {
    let mut fields = text.split_whitespace();
    let sha = fields.next()?.to_string();
    let schema = fields.next()?.parse().ok()?;
    fields.next().is_none().then_some((sha, schema))
}

/// The `--build-info` line for this binary.
#[must_use]
pub fn build_info_line() -> String {
    format!("{BUILD_SHA} {}", crate::store::LATEST_SCHEMA_VERSION)
}

/// In-memory quiet-point counter; not persisted (a restart resets the wait).
#[derive(Debug, Default)]
pub struct GateState {
    quiet_polls: u32,
    /// Blockers seen on the latest poll, for the timeout wake.
    pub last_blockers: Vec<&'static str>,
}

/// What one poll did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    Idle,
    Waiting(Vec<&'static str>),
    Restarting(Uuid),
    Settled(Uuid, DeployState),
}

fn settle(
    store: &Store,
    row: &DeployRow,
    state: DeployState,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<PollOutcome> {
    remove_staged(&row.manifest, row.id);
    store.settle_agent_deploy(row.id, state, Some(reason), now)?;
    Ok(PollOutcome::Settled(row.id, state))
}

/// #1333 andon recording point: one `deploy_timeout:<blocker>` event per
/// blocker still holding the quiet point at the deadline (`no_blocker` when
/// the quiet polls simply ran out).
fn note_deploy_timeout(store: &Store, row: &DeployRow, blockers: &[&'static str]) {
    let blockers = if blockers.is_empty() {
        &["no_blocker"][..]
    } else {
        blockers
    };
    for blocker in blockers {
        let event = rsi_common::friction::NewFrictionEventV1::new(
            rsi_common::friction::FrictionKind::DeployTimeout,
            &[blocker],
        )
        .session(row.owner_session_id)
        .evidence("deploy", row.id);
        crate::friction::note_locked(store, &event);
    }
}

/// #1461: record the workers a deploy restart interrupts: the row keeps their
/// ids for the outcome wake and each gets one `deploy_interrupt` andon event.
/// Telemetry and the record never fail the restart that already began.
fn note_deploy_interrupts(store: &Store, row: &DeployRow, workers: &[Uuid]) {
    if let Err(error) = store.record_agent_deploy_interrupted(row.id, workers) {
        tracing::warn!(%error, deploy = %row.id, "interrupted workers not recorded on the deploy");
    }
    for worker in workers {
        let event = rsi_common::friction::NewFrictionEventV1::new(
            rsi_common::friction::FrictionKind::DeployInterrupt,
            &["worker_mid_turn"],
        )
        .session(Some(*worker))
        .evidence("deploy", row.id);
        crate::friction::note_locked(store, &event);
    }
}

/// #1461: whether the operator's drain hold on `row` is over, so a deploy that
/// asked to `interrupt_workers` stops waiting for workers mid-turn. With the
/// drain setting off nothing was ever held, so the hold is over from the start;
/// an uncapped hold lasts to the deadline.
fn drain_hold_over(
    store: &Store,
    row: &DeployRow,
    drain: &DeployDrain,
    drain_enabled: bool,
    now: DateTime<Utc>,
) -> Result<bool> {
    if !drain_enabled {
        return Ok(true);
    }
    let Some(cap) = drain.hold_cap() else {
        return Ok(false);
    };
    Ok(store
        .agent_deploy_created_at(row.id)?
        .is_some_and(|created| now >= created + cap))
}

/// One pass of the deploy runner over the single live deploy.
///
/// # Errors
/// A persistence error; the caller logs and retries next tick.
pub async fn poll_once(
    store: &rsid_core::store_lock::Mutex<Store>,
    service: &DeployService,
    now: DateTime<Utc>,
    gate: &mut GateState,
    drain: &DeployDrain,
    drain_enabled: bool,
) -> Result<PollOutcome> {
    let outcome = poll_step(store, service, now, gate, drain, drain_enabled).await;
    // Re-derive the hold from the durable row after every step: a settle or a
    // timeout releases it, a restart keeps it until the process exits.
    let synced = {
        let store = store.lock().await;
        store.live_agent_deploy().and_then(|live| {
            let end = match &live {
                Some(row) => hold_end(&store, row, drain, gate.quiet_polls > 0)?,
                None => None,
            };
            drain.sync_until(live.as_ref(), end, drain_enabled, now);
            reconcile_retry_hold(&store, drain, live.as_ref())
        })
    };
    match synced {
        Ok(()) => {}
        Err(error) if outcome.is_ok() => return Err(error),
        Err(_) => {}
    }
    outcome
}

/// #2551: the retry window is installed with the staged job hold, before the
/// deadline can pass. Keep it only for a deploy that timed out (or is still
/// live); a deploy that settled any other way, or is gone, drops it, so a job
/// is not held for a retry that will never come.
fn reconcile_retry_hold(
    store: &Store,
    drain: &DeployDrain,
    live: Option<&DeployRow>,
) -> Result<()> {
    let Some(owner) = drain.retry_hold_owner() else {
        return Ok(());
    };
    if live.is_some_and(|row| row.id == owner) {
        return Ok(());
    }
    match store.get_agent_deploy(owner)? {
        Some(row) if row.state == DeployState::TimedOut => {}
        _ => drain.clear_retry_hold(owner),
    }
    Ok(())
}

/// When the hold on new worker starts ends for `row`, if before its deadline
/// (#1320/#1311). An agent deploy holds for at most the operator's
/// `deploy_drain_hold_secs` after it was requested; past that it waits for a
/// quiet point without holding. From its first quiet poll (`quiet_seen`) the
/// hold runs to the deadline again, so nothing new starts before the swap. An
/// operator restart (#1122) and a `restarting` deploy hold to the deadline.
fn hold_end(
    store: &Store,
    row: &DeployRow,
    drain: &DeployDrain,
    quiet_seen: bool,
) -> Result<Option<DateTime<Utc>>> {
    if row.owner_session_id.is_none() || row.state != DeployState::Staged || quiet_seen {
        return Ok(None);
    }
    let Some(cap) = drain.hold_cap() else {
        return Ok(None);
    };
    Ok(store
        .agent_deploy_created_at(row.id)?
        .map(|created| created + cap))
}

/// Engage the holds the durable live deploy `row` calls for: the worker-start
/// hold (to `hold_end`, or the deploy's deadline) and, for a staged deploy,
/// the hold on new test/build jobs (with a retry window for an
/// `interrupt_workers` one; none for the others, #3373). Returns whether
/// the deploy asked to interrupt workers. The one derivation shared by every
/// poll, staging and the startup restore ([`restore_drain_at_startup`]).
fn engage_hold(
    store: &Store,
    row: &DeployRow,
    drain: &DeployDrain,
    quiet_seen: bool,
    drain_enabled: bool,
    now: DateTime<Utc>,
) -> Result<bool> {
    let end = hold_end(store, row, drain, quiet_seen)?;
    drain.sync_until(Some(row), end, drain_enabled, now);
    // #2190: new test/build jobs wait for a staged `interrupt_workers` deploy
    // whatever the worker-start hold cap says (the setting-off case included: the
    // restart does not depend on it).
    let interrupt_requested = row.state == DeployState::Staged
        && row.owner_session_id.is_some()
        && store.agent_deploy_interrupt(row.id)?.requested;
    if interrupt_requested {
        drain.hold_jobs_until_settled(row, now);
    } else if drain_enabled && row.state == DeployState::Staged && row.owner_session_id.is_some() {
        // #3373: a deploy that stops no job still needs the running ones to
        // drain; past the worker-start hold a newly started job would keep
        // `job_running` set to the deadline and starve the held merge queue.
        drain.hold_jobs_until_deadline(row, now);
    }
    Ok(interrupt_requested)
}

/// Engage admission holds before releasing the staged insert's store lock
/// (#2203), using the operator's settings as they stand at staging time.
pub(crate) fn engage_hold_at_stage(
    store: &Store,
    row: &DeployRow,
    drain: &DeployDrain,
    config: &crate::config::RuntimeConfig,
    now: DateTime<Utc>,
) -> Result<()> {
    let enabled = config.deploy_drain_enabled.load(Ordering::Relaxed);
    drain.set_hold_cap_secs(config.deploy_drain_hold_secs.load(Ordering::Relaxed));
    engage_hold(store, row, drain, false, enabled, now)?;
    wake_deploy_loop();
    Ok(())
}

/// Rebuild the deploy hold from the durable live deploy row, once, at startup
/// (#2201, #2202). `DeployDrain` starts empty and the deploy runner only
/// reloads the row after startup verification and its first tick, so without
/// this a restarted daemon admits queued jobs and the next merge-queue batch
/// while a staged deploy is still draining. Applies the operator's current
/// drain settings first, then engages what a poll would. Returns the live
/// deploy's id, if any.
///
/// # Errors
/// A persistence error reading the live deploy or its interrupt request.
pub async fn restore_drain_at_startup(
    store: &rsid_core::store_lock::Mutex<Store>,
    drain: &DeployDrain,
    config: &crate::config::RuntimeConfig,
    now: DateTime<Utc>,
) -> Result<Option<Uuid>> {
    let enabled = config.deploy_drain_enabled.load(Ordering::Relaxed);
    drain.set_hold_cap_secs(config.deploy_drain_hold_secs.load(Ordering::Relaxed));
    let store = store.lock().await;
    let Some(row) = store.live_agent_deploy()? else {
        drain.sync(None, enabled, now);
        // #2552: a staged `interrupt_workers` deploy that timed out keeps new
        // jobs held until its retry window ends. The window ends at a fixed time
        // derived from the durable row, so a restart restores the rest of it
        // before any job is admitted and never extends it.
        if let Some(latest) = store.latest_agent_deploy()?
            && latest.state == DeployState::TimedOut
            && store.agent_deploy_interrupt(latest.id)?.requested
        {
            drain.hold_jobs_for_retry(&latest);
        }
        return Ok(None);
    };
    engage_hold(&store, &row, drain, false, enabled, now)?;
    Ok(Some(row.id))
}

/// The daemon's startup order for work admission (#2201, #2202): restore the
/// deploy hold ([`restore_drain_at_startup`]), then run `start`, which spawns
/// the loops that admit queued jobs and merge-queue batches. `start` is only
/// reached once the hold is in place; a restore failure fails startup instead
/// of admitting work past an unread hold.
///
/// # Errors
/// The restore's persistence error; `start` has not run then.
pub async fn start_admission_after_restore<F, Fut, T>(
    store: &rsid_core::store_lock::Mutex<Store>,
    drain: &DeployDrain,
    config: &crate::config::RuntimeConfig,
    start: F,
) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    restore_drain_at_startup(store, drain, config, Utc::now()).await?;
    Ok(start().await)
}

async fn poll_step(
    store: &rsid_core::store_lock::Mutex<Store>,
    service: &DeployService,
    now: DateTime<Utc>,
    gate: &mut GateState,
    drain: &DeployDrain,
    drain_enabled: bool,
) -> Result<PollOutcome> {
    let store = store.lock().await;
    let Some(row) = store.live_agent_deploy()? else {
        gate.quiet_polls = 0;
        drain.set_blockers(&[]);
        drain.sync(None, drain_enabled, now);
        return Ok(PollOutcome::Idle);
    };
    // Hold new work before judging the quiet point, so nothing new arrives
    // between the read of the blockers and the swap (#1320: within the
    // operator's hold window, or from the first quiet poll on).
    let interrupt_requested = engage_hold(
        &store,
        &row,
        drain,
        gate.quiet_polls > 0,
        drain_enabled,
        now,
    )?;
    if row.state != DeployState::Staged {
        // `restarting`: the drain is in flight; startup verification settles it.
        drain.set_blockers(&[]);
        return Ok(PollOutcome::Idle);
    }
    // #1461: past the drain hold, an `interrupt_workers` deploy no longer waits
    // for workers mid-turn; a landing and a local job still block.
    let mut interrupting = false;
    // #3554: an `interrupt_workers` deploy does not wait for, stop or requeue an
    // ordinary test job: its unit outlives the restart and the new daemon
    // re-adopts it. A landing, a build and a protected test still block.
    let surviving = crate::agent_jobs::deploy_surviving_jobs_or_none(&store, &row);
    let blockers = match row.owner_session_id {
        Some(owner) => {
            interrupting =
                interrupt_requested && drain_hold_over(&store, &row, drain, drain_enabled, now)?;
            store.deploy_quiet_blockers_surviving(owner, !interrupting, &surviving)?
        }
        // An operator restart also waits for managers mid-turn (#1122).
        None => store.operator_quiet_blockers()?.0,
    };
    gate.last_blockers.clone_from(&blockers);
    drain.set_blockers(&blockers);
    if blockers.is_empty() {
        gate.quiet_polls += 1;
        // A lull: hold new worker starts through the confirming poll.
        drain.sync_until(Some(&row), None, drain_enabled, now);
    } else {
        gate.quiet_polls = 0;
    }
    if row.forced || gate.quiet_polls >= QUIET_POLLS_REQUIRED {
        gate.quiet_polls = 0;
        if !service.is_supervised() {
            return settle(
                &store,
                &row,
                DeployState::Failed,
                DEPLOY_NEEDS_SUPERVISOR,
                now,
            );
        }
        if let Err(error) = service.check_budget(&store, now) {
            return settle(&store, &row, DeployState::Failed, &error.to_string(), now);
        }
        // The workers this restart will cut off, read before the swap while the
        // same store lock still holds the quiet judgement above.
        let interrupted = match row.owner_session_id {
            Some(owner) if interrupting => store.deploy_mid_turn_workers(owner)?,
            _ => Vec::new(),
        };
        // #2384: whether this restart compacts the database. Decided here, under
        // the quiet judgement's store lock and never on the startup path; the
        // compaction runs in an isolated helper after the graceful shutdown
        // (`cold_compact`). Every way out below that does not restart gives the
        // claim back.
        let compaction = crate::cold_compact::decide_at_quiet_point(&store);
        let restarting = store.mark_agent_deploy_restarting(row.id, now);
        if !matches!(restarting, Ok(true)) {
            crate::cold_compact::abandon(&store, compaction);
        }
        if !restarting? {
            return Ok(PollOutcome::Idle);
        }
        if let Err(error) = swap_in(&row.manifest, row.id) {
            crate::cold_compact::abandon(&store, compaction);
            return settle(&store, &row, DeployState::Failed, &error.to_string(), now);
        }
        if let Err(error) = write_marker(&row.manifest, store.database_path()) {
            crate::cold_compact::abandon(&store, compaction);
            rollback(&row.manifest.iter().collect::<Vec<_>>());
            return settle(
                &store,
                &row,
                DeployState::Failed,
                &format!("in-flight marker not written: {error}"),
                now,
            );
        }
        if !service.fire_restart() {
            crate::cold_compact::abandon(&store, compaction);
            remove_marker(&row.manifest);
            rollback(&row.manifest.iter().collect::<Vec<_>>());
            return settle(
                &store,
                &row,
                DeployState::Failed,
                "restart_unavailable",
                now,
            );
        }
        // The restart is under way: the archive pass threads stop at their next
        // safe boundary and admit no successor (#2384 review, #205), whether or not
        // this restart compacts.
        if let Some(lifecycle) = crate::cold_archive_lifecycle::process() {
            lifecycle.close();
        }
        if interrupting {
            note_deploy_interrupts(&store, &row, &interrupted);
        }
        if !surviving.is_empty() {
            // The jobs still running at the restart, named for the outcome.
            match store.record_agent_deploy_surviving_jobs(row.id, &surviving) {
                Ok(marked) => {
                    tracing::info!(deploy = %row.id, jobs = ?marked, "test jobs kept running across the deploy restart")
                }
                Err(error) => {
                    tracing::warn!(%error, deploy = %row.id, "surviving jobs not recorded on the deploy")
                }
            }
        }
        return Ok(PollOutcome::Restarting(row.id));
    }
    if now >= row.deadline_at {
        note_deploy_timeout(&store, &row, &blockers);
        // #2278/#2551: the retry window of a timed-out `interrupt_workers`
        // deploy was installed with its job hold (`engage_hold`) and runs from
        // the deadline, so no job was admitted between the deadline and this
        // settlement. `reconcile_retry_hold` keeps it for a timed-out row.
        let reason = format!("quiet point not reached: {}", blockers.join(","));
        return settle(&store, &row, DeployState::TimedOut, &reason, now);
    }
    Ok(PollOutcome::Waiting(blockers))
}

/// Milliseconds (plus one) since process start of the latest startup
/// milestone; zero until the first (#2095).
static LAST_STARTUP_MILESTONE_MS: AtomicU64 = AtomicU64::new(0);
static PROCESS_START: OnceLock<std::time::Instant> = OnceLock::new();

/// Record that startup made progress (`startup_milestone` events call this), so
/// deploy verification keeps waiting for a slow but advancing startup (#2095).
pub fn note_startup_milestone() {
    let start = PROCESS_START.get_or_init(std::time::Instant::now);
    let ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX - 1);
    LAST_STARTUP_MILESTONE_MS.store(ms.saturating_add(1), Ordering::Relaxed);
}

/// Time since the last startup milestone, or since process start before any.
fn startup_progress_age() -> Duration {
    let start = *PROCESS_START.get_or_init(std::time::Instant::now);
    let elapsed = start.elapsed();
    match LAST_STARTUP_MILESTONE_MS.load(Ordering::Relaxed) {
        0 => elapsed,
        marked => elapsed.saturating_sub(Duration::from_millis(marked - 1)),
    }
}

/// How long verification waits for a running binary that cannot be hashed yet.
#[derive(Debug, Clone, Copy)]
pub struct VerifyPatience {
    /// Give up when no startup milestone arrived for this long.
    pub stall: Duration,
    /// Give up after this long in total, milestones or not.
    pub hard_cap: Duration,
    /// Pause between attempts.
    pub poll: Duration,
}

impl VerifyPatience {
    /// Daemon defaults: a stall window of two minutes, a 45-minute hard cap.
    pub const DEFAULT: Self = Self {
        stall: Duration::from_secs(120),
        hard_cap: Duration::from_secs(45 * 60),
        poll: Duration::from_secs(5),
    };
}

/// What the running build says about the deploy.
#[derive(Debug, PartialEq, Eq)]
enum BuildCheck {
    Verified,
    /// The running binary could not be hashed (yet); worth another attempt.
    Unreadable(String),
    /// The named check failed with the observed value.
    Failed(String),
}

/// Judge the running build. A failure names the check and the observed and
/// expected values, never a check that passed (#2095).
fn check_running_build(
    build_sha: &str,
    deploy_sha: &str,
    expected_sha256: Option<&str>,
    running_sha256: Option<&str>,
) -> BuildCheck {
    if build_sha != deploy_sha {
        return BuildCheck::Failed(format!(
            "build_sha check: running {build_sha}, expected {deploy_sha}"
        ));
    }
    let Some(expected) = expected_sha256 else {
        return BuildCheck::Failed("manifest check: the deploy manifest has no rsid entry".into());
    };
    match running_sha256 {
        None => BuildCheck::Unreadable(format!(
            "binary_sha256 check: the running binary could not be hashed, expected {expected}"
        )),
        Some(running) if running != expected => BuildCheck::Failed(format!(
            "binary_sha256 check: running {running}, expected {expected} (build_sha {build_sha} matched)"
        )),
        Some(_) => BuildCheck::Verified,
    }
}

enum VerifyStep {
    NoDeploy,
    Settled(PollOutcome),
    /// Nothing settled: the running binary is unreadable and the caller may retry.
    Deferred(String),
}

/// Startup verification of a `restarting` deploy against the running build.
///
/// # Errors
/// A persistence error.
pub async fn verify_after_restart(
    store: &rsid_core::store_lock::Mutex<Store>,
    service: &DeployService,
    build_sha: &str,
    running_sha256: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<PollOutcome>> {
    match verify_step(store, service, build_sha, running_sha256, now, false).await? {
        VerifyStep::Settled(outcome) => Ok(Some(outcome)),
        VerifyStep::NoDeploy | VerifyStep::Deferred(_) => Ok(None),
    }
}

async fn verify_step(
    store: &rsid_core::store_lock::Mutex<Store>,
    service: &DeployService,
    build_sha: &str,
    running_sha256: Option<&str>,
    now: DateTime<Utc>,
    defer_unreadable: bool,
) -> Result<VerifyStep> {
    let store = store.lock().await;
    let Some(row) = store.live_agent_deploy()? else {
        return Ok(VerifyStep::NoDeploy);
    };
    if row.state != DeployState::Restarting {
        return Ok(VerifyStep::NoDeploy);
    }
    let rsid_entry = row.manifest.iter().find(|entry| entry.name == "rsid");
    let expected = rsid_entry.map(|entry| entry.sha256.as_str());
    let mut upgrade_note = None;
    let mut verdict = check_running_build(build_sha, &row.sha, expected, running_sha256);
    if matches!(verdict, BuildCheck::Verified) {
        upgrade_note = service.complete_upgrade_members(&row);
        // Hub #3886: a member the staging daemon put outside rsid's directory is
        // linked beside it first, then the build that came up must resolve a
        // trusted guard and every deployed member where the daemon and the lander
        // look; otherwise it is rolled back like any failed verification.
        for (member, cause) in service.plan.link_members(&row.manifest, false) {
            tracing::warn!(%member, %cause, deploy = %row.id, "link beside rsid for a deployed member not made");
        }
        if let Some(reason) = service
            .plan
            .guard_unresolvable(&row.manifest, false)
            .or_else(|| service.plan.member_unresolvable(&row.manifest))
        {
            tracing::warn!(deploy = %row.id, %reason, "deploy verification failed: member layout");
            verdict = BuildCheck::Failed(reason);
        }
    }
    let failure = match verdict {
        BuildCheck::Verified => {
            let note = upgrade_note;
            for (member, cause) in service.plan.link_members(&row.manifest, true) {
                tracing::warn!(%member, %cause, deploy = %row.id, "install-directory link for a deployed member not made");
            }
            store.settle_agent_deploy(row.id, DeployState::Succeeded, note.as_deref(), now)?;
            remove_marker(&row.manifest);
            remove_prev_files(&row.manifest);
            drop(service.prune_staging_after_success(&store, &row));
            return Ok(VerifyStep::Settled(PollOutcome::Settled(
                row.id,
                DeployState::Succeeded,
            )));
        }
        BuildCheck::Unreadable(reason) if defer_unreadable => {
            return Ok(VerifyStep::Deferred(reason));
        }
        BuildCheck::Unreadable(reason) | BuildCheck::Failed(reason) => reason,
    };
    let backup = prepare_database_rollback(&store, service, &row)?;
    // The new build did not come up as deployed: put the old binaries back.
    remove_marker(&row.manifest);
    rollback(&row.manifest.iter().collect::<Vec<_>>());
    // The running process must equal what is installed now. Restart into the
    // restored binaries unless the running bits already are those (#2095).
    let installed_sha256 = rsid_entry.and_then(|entry| binary_sha256(Path::new(&entry.dest)));
    let running_is_installed =
        running_sha256.is_some() && running_sha256 == installed_sha256.as_deref();
    let restart = !running_is_installed || backup.is_some();
    let mut reason = format!("verification failed: {failure}; previous binaries restored");
    if let Some(path) = &backup {
        reason.push_str(&format!(
            "; offline database restore queued from {}",
            path.display()
        ));
    }
    if restart {
        reason.push_str("; restarting into the restored build");
    }
    store.settle_agent_deploy(row.id, DeployState::Failed, Some(&reason), now)?;
    if restart {
        service.fire_restart();
    }
    Ok(VerifyStep::Settled(PollOutcome::Settled(
        row.id,
        DeployState::Failed,
    )))
}

/// Verify the running build, waiting while the binary cannot be hashed yet and
/// startup keeps emitting milestones, up to `patience.hard_cap` (#2095).
/// `hash` reads the running binary's sha256; `progress_age` is the time since
/// the last startup milestone.
///
/// # Errors
/// A persistence error.
pub async fn verify_with_patience<H, P>(
    store: &rsid_core::store_lock::Mutex<Store>,
    service: &DeployService,
    build_sha: &str,
    patience: VerifyPatience,
    mut hash: H,
    progress_age: P,
) -> Result<Option<PollOutcome>>
where
    H: FnMut() -> Option<String>,
    P: Fn() -> Duration,
{
    let started = std::time::Instant::now();
    loop {
        let running = hash();
        let waiting = started.elapsed() < patience.hard_cap && progress_age() < patience.stall;
        match verify_step(
            store,
            service,
            build_sha,
            running.as_deref(),
            Utc::now(),
            waiting,
        )
        .await?
        {
            VerifyStep::Settled(outcome) => return Ok(Some(outcome)),
            VerifyStep::NoDeploy => return Ok(None),
            VerifyStep::Deferred(reason) => {
                tracing::info!(%reason, "deploy verification waiting for startup");
                tokio::time::sleep(patience.poll).await;
            }
        }
    }
}

/// The daemon-owned runner: verify once at startup, then poll.
pub async fn run_deploy_loop(
    store: Arc<rsid_core::store_lock::Mutex<Store>>,
    service: &'static DeployService,
    drain: Arc<DeployDrain>,
    config: Arc<crate::config::RuntimeConfig>,
) {
    let hasher = || DaemonInfoService::global().running_binary_sha256();
    let verified = verify_with_patience(
        &store,
        service,
        BUILD_SHA,
        VerifyPatience::DEFAULT,
        || {
            // Hashing reads the whole binary: keep it off the async threads.
            tokio::task::block_in_place(hasher)
        },
        startup_progress_age,
    )
    .await;
    match verified {
        Ok(Some(outcome)) => tracing::info!(?outcome, "deploy verified after restart"),
        Ok(None) => {}
        Err(error) => tracing::warn!(%error, "deploy verification deferred"),
    }
    let mut gate = GateState::default();
    let mut delay = TICK;
    loop {
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = DEPLOY_WAKE.notified() => {}
        }
        let drain_enabled = config.deploy_drain_enabled.load(Ordering::Relaxed);
        drain.set_hold_cap_secs(config.deploy_drain_hold_secs.load(Ordering::Relaxed));
        service.set_restarts_per_hour(config.deploy_restarts_per_hour.load(Ordering::Relaxed));
        // #2384: a due compaction is claimed at a restart only while the
        // cold archive is on (an operator's request always is).
        crate::cold_compact::set_auto_compaction(
            config.cold_archive_enabled.load(Ordering::Relaxed),
        );
        let outcome = poll_once(
            &store,
            service,
            Utc::now(),
            &mut gate,
            &drain,
            drain_enabled,
        )
        .await;
        delay = next_poll_delay(&outcome);
        match outcome {
            Ok(PollOutcome::Idle | PollOutcome::Waiting(_)) => {}
            Ok(outcome) => tracing::info!(?outcome, "deploy poll"),
            Err(error) => tracing::warn!(%error, "deploy poll deferred"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::agent_deploys::deploy_fingerprint;
    use crate::store::rolling_queue::NewQueueEntry;
    use rsi_common::rolling_queue::RollingQueueBinding;
    use std::sync::atomic::AtomicUsize;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn staged_deploy_polls_each_second_and_the_gate_closes_within_three() {
        // #2355: two confirming quiet polls one active tick apart.
        assert_eq!(QUIET_POLLS_REQUIRED, 2);
        let waiting = Ok(PollOutcome::Waiting(vec!["local_job"]));
        assert_eq!(next_poll_delay(&waiting), ACTIVE_TICK);
        assert!(ACTIVE_TICK * QUIET_POLLS_REQUIRED <= Duration::from_secs(3));
        for idle in [
            Ok(PollOutcome::Idle),
            Ok(PollOutcome::Restarting(Uuid::nil())),
            Err(DaemonError::InvalidParam("store busy".into())),
        ] {
            assert_eq!(next_poll_delay(&idle), TICK);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn staging_wakes_the_idle_deploy_loop_before_the_idle_tick() {
        wake_deploy_loop();
        tokio::time::timeout(Duration::from_millis(500), DEPLOY_WAKE.notified())
            .await
            .expect("a wake sent before the loop sleeps is not lost");
    }

    /// #3423: a refused `binaries_dir` names the directory it got and the
    /// roots it accepts; the stable code stays first.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_refused_binaries_dir_names_the_path_and_the_allowed_roots() {
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let service = DeployService::new(
            allowed.path().join("install"),
            vec![allowed.path().to_path_buf()],
            Box::new(|| true),
            Arc::new(|_: &Path| Ok((SHA.to_string(), 999))),
        );
        let outside_dir = outside.path().to_string_lossy().into_owned();
        let error = service
            .stage_plan()
            .stage_binaries_discovering_sha(Uuid::new_v4(), &outside_dir, 999)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!(
                "{DEPLOY_DIR_NOT_ALLOWED}: {outside_dir} is outside"
            )),
            "{error}"
        );
        assert!(
            error.contains(&allowed.path().display().to_string()),
            "{error}"
        );
        let missing = allowed.path().join("absent");
        let missing_dir = missing.to_string_lossy().into_owned();
        let error = service
            .stage_plan()
            .stage_binaries_discovering_sha(Uuid::new_v4(), &missing_dir, 999)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!(
                "{DEPLOY_DIR_NOT_ALLOWED}: {missing_dir} does not exist"
            )),
            "{error}"
        );
        assert!(
            error.contains(&allowed.path().display().to_string()),
            "{error}"
        );
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const OLD: &[u8] = b"old-rsid";
    const NEW: &[u8] = b"new-rsid";

    struct Fixture {
        _dir: tempfile::TempDir,
        source: PathBuf,
        install: PathBuf,
        service: DeployService,
        restarts: Arc<AtomicUsize>,
        store: rsid_core::store_lock::Mutex<Store>,
        owner: Uuid,
        drain: DeployDrain,
    }

    fn session(store: &Store, id: Uuid, status: &str, parent: Option<Uuid>) {
        store
            .conn
            .execute(
                "INSERT INTO sessions (id, provider, query, working_dir, status, created_at, \
                 updated_at, session_kind, parent_id) VALUES (?1,'Claude','q','/tmp',?2,?3,?3,'Task',?4)",
                rusqlite::params![
                    id.to_string(),
                    status,
                    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                    parent.map(|p| p.to_string()),
                ],
            )
            .unwrap();
    }

    fn fixture(supervised: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("build");
        let install = dir.path().join("install");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(source.join("rsid"), NEW).unwrap();
        std::fs::write(install.join("rsid"), OLD).unwrap();
        let service = DeployService::new(
            install.clone(),
            vec![dir.path().to_path_buf()],
            Box::new(move || supervised),
            Arc::new(|_: &Path| Ok((SHA.to_string(), 999))),
        );
        let restarts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&restarts);
        service.set_restart_trigger(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        let store = Store::open_in_memory().unwrap();
        let owner = Uuid::new_v4();
        session(&store, owner, "Running", None);
        Fixture {
            _dir: dir,
            source,
            install,
            service,
            restarts,
            store: rsid_core::store_lock::Mutex::new(store),
            owner,
            drain: DeployDrain::new(),
        }
    }

    /// Stage the fixture's build and record a `staged` deploy.
    async fn stage(f: &Fixture, key: &str, wait: u32) -> DeployRow {
        stage_with(f, key, wait, false).await
    }

    /// [`stage`], optionally asking to interrupt workers past the hold (#1461).
    async fn stage_with(f: &Fixture, key: &str, wait: u32, interrupt_workers: bool) -> DeployRow {
        let id = Uuid::new_v4();
        let manifest = f
            .service
            .stage_plan()
            .stage_binaries(id, f.source.to_str().unwrap(), SHA, 1)
            .unwrap();
        f.store
            .lock()
            .await
            .insert_agent_deploy(
                &crate::store::agent_deploys::NewDeploy {
                    id,
                    owner_session_id: f.owner,
                    idempotency_key: key,
                    sha: SHA,
                    fingerprint: deploy_fingerprint_with(SHA, "x", wait, interrupt_workers),
                    manifest: &manifest,
                    max_wait_secs: wait,
                    interrupt_workers,
                },
                Utc::now(),
            )
            .unwrap()
    }

    fn installed(f: &Fixture) -> Vec<u8> {
        std::fs::read(f.install.join("rsid")).unwrap()
    }

    fn wakes(store: &Store, owner: Uuid) -> Vec<rsi_common::types::ScheduledJob> {
        store
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.wake_session_id == Some(owner))
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn rolling_land_and_remote_deploy_when_present_and_are_named_when_absent() {
        let f = fixture(true);
        std::fs::write(f.source.join("rsi-rolling-land"), b"new-lander").unwrap();
        std::fs::write(f.source.join("rsi-remote"), b"new-remote").unwrap();
        let manifest = f
            .service
            .stage_plan()
            .stage_binaries(Uuid::new_v4(), f.source.to_str().unwrap(), SHA, 1)
            .unwrap();
        let names: Vec<&str> = manifest.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["rsid", "rsi-rolling-land", "rsi-remote"]);
        let lander = manifest
            .iter()
            .find(|entry| entry.name == "rsi-rolling-land")
            .unwrap();
        assert_eq!(
            lander.sha256,
            binary_sha256(&f.source.join("rsi-rolling-land")).unwrap()
        );
        assert_eq!(
            rsi_common::agent_deploy::skipped_binaries(&manifest),
            vec![
                "rsi",
                "rsi-rpc",
                "rsi-agent-mcp",
                "rsi-build-rustc",
                "rsi-contract-validate",
                "rsi-known-failure",
                "rsi-repo-guard",
                "rsi-turn-shim",
                "rsi-socket-hold",
                "rsi-cloud",
                "rsi-eval",
                "rsid-supervisor.sh",
                "cargo-slot",
                "target-pool",
                "rsi-spill"
            ]
        );
        let id = Uuid::new_v4();
        let manifest = f
            .service
            .stage_plan()
            .stage_binaries(id, f.source.to_str().unwrap(), SHA, 1)
            .unwrap();
        swap_in(&manifest, id).unwrap();
        assert_eq!(
            std::fs::read(f.install.join("rsi-rolling-land")).unwrap(),
            b"new-lander"
        );
        assert_eq!(
            std::fs::read(f.install.join("rsi-remote")).unwrap(),
            b"new-remote"
        );
        assert_eq!(installed(&f), NEW);
    }

    /// #451: the lander and the queue probe run `rsi-repo-guard` installed beside
    /// themselves (`repo_guard_call::trusted_tool`), so a deploy stages, verifies
    /// and swaps it in with the lander, and names it when a build did not carry it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn repo_guard_deploys_beside_the_lander_when_present_and_is_named_when_absent() {
        let f = fixture(true);
        let manifest = f
            .service
            .stage_plan()
            .stage_binaries(Uuid::new_v4(), f.source.to_str().unwrap(), SHA, 1)
            .unwrap();
        assert!(
            rsi_common::agent_deploy::skipped_binaries(&manifest)
                .contains(&"rsi-repo-guard".to_string())
        );
        std::fs::write(f.source.join("rsi-rolling-land"), b"new-lander").unwrap();
        std::fs::write(f.source.join("rsi-repo-guard"), b"new-guard").unwrap();
        let id = Uuid::new_v4();
        let manifest = f
            .service
            .stage_plan()
            .stage_binaries(id, f.source.to_str().unwrap(), SHA, 1)
            .unwrap();
        let names: Vec<&str> = manifest.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["rsid", "rsi-rolling-land", "rsi-repo-guard"]);
        let guard = manifest
            .iter()
            .find(|entry| entry.name == "rsi-repo-guard")
            .unwrap();
        assert_eq!(
            guard.sha256,
            binary_sha256(&f.source.join("rsi-repo-guard")).unwrap()
        );
        assert!(
            !rsi_common::agent_deploy::skipped_binaries(&manifest)
                .contains(&"rsi-repo-guard".to_string())
        );
        swap_in(&manifest, id).unwrap();
        assert_eq!(
            std::fs::read(f.install.join("rsi-repo-guard")).unwrap(),
            b"new-guard"
        );
        assert_eq!(
            std::fs::read(f.install.join("rsi-rolling-land")).unwrap(),
            b"new-lander"
        );
    }

    /// Hub #3886 production layout: `~/.rsi/install` holds the real `rsid` and
    /// `rsi-rolling-land`, and `~/.local/bin` (the plan's install dir) links them.
    struct Layout {
        _dir: tempfile::TempDir,
        source: PathBuf,
        /// `~/.rsi/install`: where the daemon and the lander run from.
        real: PathBuf,
        /// `~/.local/bin`: the plan's install dir.
        bin: PathBuf,
        service: DeployService,
    }

    fn layout(guard_required: bool) -> Layout {
        let dir = crate::test_support::disk_backed_tempdir("guard-deploy-layout");
        let home = dir.path().canonicalize().unwrap();
        let source = home.join("build");
        let real = home.join(".rsi/install");
        let bin = home.join(".local/bin");
        for path in [&source, &real, &bin] {
            std::fs::create_dir_all(path).unwrap();
        }
        for name in ["rsid", "rsi-rolling-land"] {
            std::fs::write(real.join(name), b"old").unwrap();
            std::os::unix::fs::symlink(real.join(name), bin.join(name)).unwrap();
        }
        std::fs::write(source.join("rsid"), NEW).unwrap();
        std::fs::write(source.join("rsi-rolling-land"), b"new-lander").unwrap();
        let mut service = DeployService::new(
            bin.clone(),
            vec![dir.path().to_path_buf()],
            Box::new(|| true),
            Arc::new(|_: &Path| Ok((SHA.to_string(), 999))),
        );
        if guard_required {
            service = service.with_guard_required();
        }
        service.plan.guard_home = Some(home);
        Layout {
            _dir: dir,
            source,
            real,
            bin,
            service,
        }
    }

    fn stage_layout(l: &Layout) -> (Uuid, Result<Vec<DeployBinaryV1>>) {
        let id = Uuid::new_v4();
        let staged = l
            .service
            .stage_plan()
            .stage_binaries(id, l.source.to_str().unwrap(), SHA, 1);
        (id, staged)
    }

    /// Hub #3886: the guard lands where `installed_tool()` of the deployed `rsid`
    /// and of the lander finds it (beside them, not in `~/.local/bin`), the
    /// `install_dir` link follows, and the same holds for `rsi-eval`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn members_deploy_beside_rsid_where_the_sibling_lookup_finds_them_3886() {
        let l = layout(true);
        std::fs::write(l.source.join("rsi-repo-guard"), b"new-guard").unwrap();
        std::fs::write(l.source.join("rsi-eval"), b"new-eval").unwrap();
        let (id, staged) = stage_layout(&l);
        let manifest = staged.unwrap();
        for name in ["rsi-repo-guard", "rsi-eval"] {
            let entry = manifest.iter().find(|entry| entry.name == name).unwrap();
            assert_eq!(Path::new(&entry.dest), l.real.join(name), "{name}");
        }
        swap_in(&manifest, id).unwrap();
        let plan = l.service.stage_plan();
        assert_eq!(plan.guard_unresolvable(&manifest, false), None);
        assert_eq!(plan.member_unresolvable(&manifest), None);
        // The lookup rsid and the lander each make from their own directory.
        for consumer in ["rsid", "rsi-rolling-land"] {
            let dir = std::fs::canonicalize(l.bin.join(consumer))
                .unwrap()
                .parent()
                .unwrap()
                .to_path_buf();
            assert_eq!(
                crate::repo_guard_call::installed_tool_in_home(&dir, l._dir.path()),
                Ok(l.real.join("rsi-repo-guard")),
                "{consumer}"
            );
        }
        assert!(plan.link_members(&manifest, true).is_empty());
        for name in ["rsi-repo-guard", "rsi-eval"] {
            let link = l.bin.join(name);
            assert!(
                std::fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(
                std::fs::canonicalize(&link).unwrap(),
                std::fs::canonicalize(l.real.join(name)).unwrap()
            );
        }
    }

    /// Hub #3886: a build with no guard, installed nowhere, is refused at staging
    /// with the named reason; a missing guard after the restart fails
    /// verification with it too.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_build_whose_guard_would_not_resolve_is_refused_with_the_named_reason_3886() {
        let l = layout(true);
        let (_, staged) = stage_layout(&l);
        let error = staged.unwrap_err().to_string();
        assert!(
            error.contains(rsi_common::agent_deploy::DEPLOY_GUARD_UNRESOLVABLE),
            "{error}"
        );
        assert!(error.contains("rsi-repo-guard"), "{error}");
        // A guard already installed beside rsid is enough; the build need not carry it.
        std::fs::write(l.real.join("rsi-repo-guard"), b"old-guard").unwrap();
        let (id, staged) = stage_layout(&l);
        let manifest = staged.unwrap();
        swap_in(&manifest, id).unwrap();
        let plan = l.service.stage_plan();
        assert_eq!(plan.guard_unresolvable(&manifest, false), None);
        // The guard vanishes before verification: the deploy fails closed.
        std::fs::remove_file(l.real.join("rsi-repo-guard")).unwrap();
        let reason = plan.guard_unresolvable(&manifest, false).unwrap();
        assert!(
            reason.starts_with(rsi_common::agent_deploy::DEPLOY_GUARD_UNRESOLVABLE),
            "{reason}"
        );
        // A plan that does not check the layout (fixtures) is never refused.
        let lax = layout(false);
        assert!(stage_layout(&lax).1.is_ok());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn staging_refuses_writable_guard_targets_and_redirected_install_dirs_3911() {
        let l = layout(true);
        let guard = l.real.join("rsi-repo-guard");
        let operator_guard = l.bin.join("rsi-repo-guard");
        std::fs::write(&operator_guard, b"operator-guard").unwrap();
        std::os::unix::fs::symlink(&operator_guard, &guard).unwrap();
        // The legitimate hand fix, with no guard in this build, still stages.
        assert!(stage_layout(&l).1.is_ok());
        std::fs::remove_file(&guard).unwrap();
        let writable = l._dir.path().join(".rsi/sandboxes/worker/target/release");
        std::fs::create_dir_all(&writable).unwrap();
        let target = writable.join("rsi-repo-guard");
        std::fs::write(&target, b"agent-guard").unwrap();
        std::os::unix::fs::symlink(&target, &guard).unwrap();
        let error = stage_layout(&l).1.unwrap_err().to_string();
        assert!(error.contains(DEPLOY_GUARD_UNRESOLVABLE), "{error}");
        assert!(error.contains("guard_untrusted_path"), "{error}");

        // A planned guard must not bypass trust when the whole install
        // directory resolves into writable storage, even without a file link.
        std::fs::remove_file(&guard).unwrap();
        let redirected = l._dir.path().join("agent-install");
        std::fs::rename(&l.real, &redirected).unwrap();
        std::os::unix::fs::symlink(&redirected, &l.real).unwrap();
        std::fs::write(l.source.join("rsi-repo-guard"), b"new-guard").unwrap();
        let error = stage_layout(&l).1.unwrap_err().to_string();
        assert!(error.contains(DEPLOY_GUARD_UNRESOLVABLE), "{error}");
        assert!(error.contains("guard_untrusted_path"), "{error}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn restart_verification_accepts_operator_guard_and_rolls_back_writable_target_3911() {
        for writable_target in [false, true] {
            let l = layout(true);
            let mut f = fixture(true);
            // Reuse the deploy/store driver with a hermetic operator layout.
            std::fs::write(l.real.join("rsid"), OLD).unwrap();
            f.install = l.real.clone();
            f.source = l.source.clone();
            f.service.plan = l.service.stage_plan();
            let operator_guard = l.bin.join("rsi-repo-guard");
            std::fs::write(&operator_guard, b"operator-guard").unwrap();
            let guard = l.real.join("rsi-repo-guard");
            std::os::unix::fs::symlink(&operator_guard, &guard).unwrap();
            let row = stage(&f, "verify-guard-trust", 900).await;
            let mut gate = GateState::default();
            let now = Utc::now();
            for _ in 0..2 {
                poll_once(&f.store, &f.service, now, &mut gate, &f.drain, true)
                    .await
                    .unwrap();
            }
            if writable_target {
                let writable = l._dir.path().join(".rsi/sandboxes/worker/target/release");
                std::fs::create_dir_all(&writable).unwrap();
                let target = writable.join("rsi-repo-guard");
                std::fs::write(&target, b"agent-guard").unwrap();
                std::fs::remove_file(&guard).unwrap();
                std::os::unix::fs::symlink(&target, &guard).unwrap();
            }
            let new_sha = binary_sha256(&f.install.join("rsid")).unwrap();
            let outcome = verify_after_restart(&f.store, &f.service, SHA, Some(&new_sha), now)
                .await
                .unwrap();
            let expected = if writable_target {
                DeployState::Failed
            } else {
                DeployState::Succeeded
            };
            assert_eq!(outcome, Some(PollOutcome::Settled(row.id, expected)));
            assert_eq!(installed(&f), if writable_target { OLD } else { NEW });
            if writable_target {
                let reason = f
                    .store
                    .lock()
                    .await
                    .get_agent_deploy(row.id)
                    .unwrap()
                    .unwrap()
                    .reason
                    .unwrap();
                assert!(reason.contains(DEPLOY_GUARD_UNRESOLVABLE), "{reason}");
                assert!(reason.contains("guard_untrusted_path"), "{reason}");
            }
        }
    }

    /// Hub #3886: the PM's hand symlink (`install/rsi-repo-guard` ->
    /// `bin/rsi-repo-guard`, a regular file) is replaced by a regular file beside
    /// rsid, and the stale file in `bin` becomes a link to it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn an_existing_hand_symlink_is_replaced_cleanly_3886() {
        let l = layout(true);
        std::fs::write(l.bin.join("rsi-repo-guard"), b"old-guard").unwrap();
        std::os::unix::fs::symlink(l.bin.join("rsi-repo-guard"), l.real.join("rsi-repo-guard"))
            .unwrap();
        std::fs::write(l.source.join("rsi-repo-guard"), b"new-guard").unwrap();
        let (id, staged) = stage_layout(&l);
        let manifest = staged.unwrap();
        let entry = manifest
            .iter()
            .find(|entry| entry.name == "rsi-repo-guard")
            .unwrap();
        assert_eq!(Path::new(&entry.dest), l.real.join("rsi-repo-guard"));
        swap_in(&manifest, id).unwrap();
        let beside = l.real.join("rsi-repo-guard");
        assert!(
            std::fs::symlink_metadata(&beside)
                .unwrap()
                .file_type()
                .is_file()
        );
        assert_eq!(std::fs::read(&beside).unwrap(), b"new-guard");
        let plan = l.service.stage_plan();
        assert!(plan.link_members(&manifest, true).is_empty());
        let link = l.bin.join("rsi-repo-guard");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&link).unwrap(), b"new-guard");
        assert_eq!(plan.guard_unresolvable(&manifest, false), None);
        // Running it again changes nothing.
        assert!(plan.link_members(&manifest, true).is_empty());
    }

    /// Hub #3886: a member an older staging daemon put in `bin` is linked beside
    /// rsid after the restart, and a member that stays unlinked fails verification
    /// with the named reason.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_member_installed_outside_rsids_directory_is_bridged_or_fails_verification_3886() {
        let l = layout(true);
        std::fs::write(l.real.join("rsi-repo-guard"), b"guard").unwrap();
        std::fs::write(l.bin.join("rsi-eval"), b"eval").unwrap();
        let entry = |name: &str, dest: PathBuf, body: &[u8]| {
            use sha2::Digest;
            DeployBinaryV1 {
                name: name.to_string(),
                dest: dest.to_string_lossy().into_owned(),
                sha256: hex::encode(sha2::Sha256::digest(body)),
                prior_present: false,
                staged_identity: None,
            }
        };
        let manifest = vec![
            entry("rsid", l.real.join("rsid"), b"old"),
            entry("rsi-eval", l.bin.join("rsi-eval"), b"eval"),
        ];
        let plan = l.service.stage_plan();
        let reason = plan.member_unresolvable(&manifest).unwrap();
        assert!(
            reason.starts_with(rsi_common::agent_deploy::DEPLOY_MEMBER_UNRESOLVABLE),
            "{reason}"
        );
        assert!(reason.contains("rsi-eval"), "{reason}");
        assert!(plan.link_members(&manifest, false).is_empty());
        assert_eq!(plan.member_unresolvable(&manifest), None);
        assert_eq!(
            std::fs::canonicalize(l.real.join("rsi-eval")).unwrap(),
            std::fs::canonicalize(l.bin.join("rsi-eval")).unwrap()
        );
        // A different regular file already beside rsid is never overwritten.
        std::fs::remove_file(l.real.join("rsi-eval")).unwrap();
        std::fs::write(l.real.join("rsi-eval"), b"someone else's").unwrap();
        let failed = plan.link_members(&manifest, false);
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(
            std::fs::read(l.real.join("rsi-eval")).unwrap(),
            b"someone else's"
        );
        assert!(plan.member_unresolvable(&manifest).is_some());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn supervisor_is_verified_swapped_and_rolled_back_with_the_manifest() {
        let f = fixture(true);
        let name = "rsid-supervisor.sh";
        let old = b"#!/usr/bin/env bash\nexit 0\n";
        let new = include_bytes!("../../../scripts/rsid-supervisor.sh");
        std::fs::write(f.source.join(name), new).unwrap();
        std::fs::write(f.install.join(name), old).unwrap();
        let id = Uuid::new_v4();
        let manifest = f
            .service
            .stage_plan()
            .stage_binaries(id, f.source.to_str().unwrap(), SHA, 1)
            .unwrap();
        let entry = manifest.iter().find(|entry| entry.name == name).unwrap();
        assert_eq!(entry.sha256, binary_sha256(&f.source.join(name)).unwrap());
        assert!(entry.staged_identity.is_some());
        assert_eq!(
            std::fs::read(staged_path(Path::new(&entry.dest), id)).unwrap(),
            new
        );
        swap_in(&manifest, id).unwrap();
        assert_eq!(std::fs::read(f.install.join(name)).unwrap(), new);
        assert_eq!(
            std::fs::read(f.install.join(format!("{name}.prev"))).unwrap(),
            old
        );
        rollback(&manifest.iter().collect::<Vec<_>>());
        assert_eq!(std::fs::read(f.install.join(name)).unwrap(), old);
        assert_eq!(installed(&f), OLD);
    }

    /// A layout like the hub: `bin/rsid` links into `real/`, `bin/rsid-supervisor.sh`
    /// is a stale regular file the supervisor never reads (#2368).
    #[cfg(unix)]
    fn linked_layout(running_script: bool) -> (tempfile::TempDir, PathBuf, PathBuf, DeployService) {
        let dir = tempfile::tempdir().unwrap();
        let running = running_script.then(|| dir.path().join("running/rsid-supervisor.sh"));
        if let Some(running) = &running {
            std::fs::create_dir_all(running.parent().unwrap()).unwrap();
            std::fs::write(running, b"#!/bin/bash\n# running\n").unwrap();
        }
        let (source, bin, real) = (
            dir.path().join("build"),
            dir.path().join("bin"),
            dir.path().join("real"),
        );
        for d in [&source, &bin, &real] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(source.join("rsid"), NEW).unwrap();
        std::fs::write(real.join("rsid"), OLD).unwrap();
        std::os::unix::fs::symlink(real.join("rsid"), bin.join("rsid")).unwrap();
        std::fs::write(bin.join("rsid-supervisor.sh"), b"#!/bin/bash\n# stale\n").unwrap();
        std::fs::write(real.join("rsid-supervisor.sh"), b"#!/bin/bash\n# running\n").unwrap();
        std::fs::write(source.join("rsid-supervisor.sh"), b"#!/bin/bash\n# new\n").unwrap();
        let service = DeployService::new(
            bin.clone(),
            vec![dir.path().to_path_buf()],
            Box::new(|| true),
            Arc::new(|_: &Path| Ok((SHA.to_string(), 999))),
        )
        .with_supervisor_script(Box::new(move || running.clone()));
        (dir, source, real, service)
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn supervisor_script_installs_next_to_the_resolved_rsid_not_the_link_dir() {
        let (dir, source, real, service) = linked_layout(false);
        let id = Uuid::new_v4();
        let manifest = service
            .stage_plan()
            .stage_binaries(id, source.to_str().unwrap(), SHA, 1)
            .unwrap();
        swap_in(&manifest, id).unwrap();
        let name = "rsid-supervisor.sh";
        assert_eq!(
            std::fs::read(real.join(name)).unwrap(),
            b"#!/bin/bash\n# new\n"
        );
        assert_eq!(std::fs::read(real.join("rsid")).unwrap(), NEW);
        assert_eq!(
            std::fs::read(dir.path().join("bin").join(name)).unwrap(),
            b"#!/bin/bash\n# stale\n"
        );
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn supervisor_script_installs_at_the_running_supervisors_script_path_by_rename() {
        use std::os::unix::fs::MetadataExt;
        let (dir, source, real, service) = linked_layout(true);
        let running = dir.path().join("running/rsid-supervisor.sh");
        let inode = std::fs::metadata(&running).unwrap().ino();
        let id = Uuid::new_v4();
        let manifest = service
            .stage_plan()
            .stage_binaries(id, source.to_str().unwrap(), SHA, 1)
            .unwrap();
        swap_in(&manifest, id).unwrap();
        assert_eq!(std::fs::read(&running).unwrap(), b"#!/bin/bash\n# new\n");
        // The old inode lives on as `.prev`: the swap renamed, never rewrote.
        let prev = dir.path().join("running/rsid-supervisor.sh.prev");
        assert_eq!(std::fs::metadata(&prev).unwrap().ino(), inode);
        assert_eq!(
            std::fs::read(real.join("rsid-supervisor.sh")).unwrap(),
            b"#!/bin/bash\n# running\n"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn syntax_invalid_supervisor_is_refused_and_staged_files_are_removed() {
        let f = fixture(true);
        std::fs::write(f.source.join("rsid-supervisor.sh"), b"if then\n").unwrap();
        let id = Uuid::new_v4();
        let error = f
            .service
            .stage_plan()
            .stage_binaries(id, f.source.to_str().unwrap(), SHA, 1)
            .unwrap_err();
        assert!(error.to_string().contains(DEPLOY_STAGE_FAILED), "{error}");
        assert_eq!(installed(&f), OLD);
        assert_eq!(std::fs::read_dir(&f.install).unwrap().count(), 1);
    }

    /// A fixture whose tool scripts publish into their own `~/.rsi/bin` stand-in
    /// (absent on the hub until the first deploy creates it, #2722).
    #[cfg(unix)]
    fn fixture_with_tools_dir() -> (Fixture, PathBuf) {
        let mut f = fixture(true);
        let tools = f._dir.path().join("rsi-home/bin");
        f.service = DeployService::new(
            f.install.clone(),
            vec![f._dir.path().to_path_buf()],
            Box::new(|| true),
            Arc::new(|_: &Path| Ok((SHA.to_string(), 999))),
        )
        .with_tools_dir(tools.clone());
        (f, tools)
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn cargo_slot_deploy_refreshes_lib_implementation_and_keeps_bin_wrapper() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let (f, tools) = fixture_with_tools_dir();
        let slot = include_bytes!("../../../scripts/cargo-slot");
        let pool = include_bytes!("../../../scripts/target-pool");
        let spill = include_bytes!("../../../scripts/rsi-spill");
        std::fs::write(f.source.join("cargo-slot"), slot).unwrap();
        std::fs::write(f.source.join("target-pool"), pool).unwrap();
        std::fs::write(f.source.join("rsi-spill"), spill).unwrap();
        // The bin path stays a stable wrapper while the running implementation
        // is atomically replaced from the deploy manifest.
        std::fs::create_dir_all(&tools).unwrap();
        let impl_path = tools.parent().unwrap().join("lib/cargo-slot");
        std::fs::create_dir_all(impl_path.parent().unwrap()).unwrap();
        std::fs::write(&impl_path, b"#!/bin/bash\n# stale implementation\n").unwrap();
        std::fs::write(tools.join("cargo-slot"), CARGO_SLOT_WRAPPER).unwrap();
        std::fs::set_permissions(
            tools.join("cargo-slot"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let running_inode = std::fs::metadata(&impl_path).unwrap().ino();
        let id = Uuid::new_v4();
        let manifest = f
            .service
            .stage_plan()
            .stage_binaries(id, f.source.to_str().unwrap(), SHA, 1)
            .unwrap();
        let skipped = rsi_common::agent_deploy::skipped_binaries(&manifest);
        assert!(!skipped.contains(&"cargo-slot".to_string()), "{skipped:?}");
        assert!(!skipped.contains(&"target-pool".to_string()), "{skipped:?}");
        assert!(!skipped.contains(&"rsi-spill".to_string()), "{skipped:?}");
        let canonical_tools = std::fs::canonicalize(&tools).unwrap();
        for name in ["cargo-slot", "target-pool", "rsi-spill"] {
            let entry = manifest.iter().find(|entry| entry.name == name).unwrap();
            let expected = if name == "cargo-slot" {
                canonical_tools.parent().unwrap().join("lib/cargo-slot")
            } else {
                canonical_tools.join(name)
            };
            assert_eq!(Path::new(&entry.dest), expected);
        }
        swap_in(&manifest, id).unwrap();
        assert_eq!(std::fs::read(&impl_path).unwrap(), slot);
        assert_eq!(
            std::fs::read(tools.join("cargo-slot")).unwrap(),
            CARGO_SLOT_WRAPPER
        );
        assert_eq!(std::fs::read(tools.join("target-pool")).unwrap(), pool);
        assert_eq!(std::fs::read(tools.join("rsi-spill")).unwrap(), spill);
        for name in ["cargo-slot", "target-pool", "rsi-spill"] {
            let deployed = if name == "cargo-slot" {
                impl_path.clone()
            } else {
                tools.join(name)
            };
            let mode = std::fs::metadata(&deployed).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "{name} must be executable");
            assert!(
                !f.install.join(name).exists(),
                "{name} belongs in the tools dir"
            );
        }
        // The running copy keeps its inode: it moved aside, was not rewritten.
        let prev = std::fs::metadata(impl_path.with_file_name("cargo-slot.prev")).unwrap();
        assert_eq!(prev.ino(), running_inode);
        assert_eq!(
            std::fs::read(impl_path.with_file_name("cargo-slot.prev")).unwrap(),
            b"#!/bin/bash\n# stale implementation\n"
        );
        // The installed set is what `AgentGetDaemonInfo` reports.
        let reported = crate::daemon_info::installed_tools_in(&tools);
        assert!(
            reported
                .iter()
                .all(|tool| tool.installed && tool.sha256.is_some())
        );
        assert_eq!(
            reported[1].sha256.as_deref(),
            binary_sha256(&f.source.join("target-pool")).as_deref()
        );
        // A failed deploy puts the prior set back and removes a first-time install.
        rollback(&manifest.iter().collect::<Vec<_>>());
        assert_eq!(
            std::fs::read(&impl_path).unwrap(),
            b"#!/bin/bash\n# stale implementation\n"
        );
        assert_eq!(
            std::fs::read(tools.join("cargo-slot")).unwrap(),
            CARGO_SLOT_WRAPPER
        );
        assert!(!tools.join("target-pool").exists());
        assert!(!tools.join("rsi-spill").exists());
    }

    /// Stage and swap a deploy that carries the full `cargo-slot` script.
    #[cfg(unix)]
    fn stage_cargo_slot_deploy(f: &Fixture) -> (Uuid, Vec<DeployBinaryV1>) {
        std::fs::write(
            f.source.join("cargo-slot"),
            include_bytes!("../../../scripts/cargo-slot"),
        )
        .unwrap();
        let id = Uuid::new_v4();
        let manifest = f
            .service
            .stage_plan()
            .stage_binaries(id, f.source.to_str().unwrap(), SHA, 1)
            .unwrap();
        (id, manifest)
    }

    /// An old daemon published the full script into `bin` and has no `lib`: the
    /// deploy moves the implementation to `lib`, installs the wrapper in `bin`
    /// (after `lib`), and a rollback restores the old layout.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn cargo_slot_deploy_migrates_an_old_bin_implementation_to_a_wrapper_and_rolls_back() {
        let (f, tools) = fixture_with_tools_dir();
        std::fs::create_dir_all(&tools).unwrap();
        let old = b"#!/bin/bash\n# old full implementation\n";
        std::fs::write(tools.join("cargo-slot"), old).unwrap();
        let slot = include_bytes!("../../../scripts/cargo-slot");
        let (id, manifest) = stage_cargo_slot_deploy(&f);
        let names: Vec<&str> = manifest.iter().map(|entry| entry.name.as_str()).collect();
        let lib_at = names.iter().position(|n| *n == "cargo-slot").unwrap();
        let wrapper_at = names
            .iter()
            .position(|n| *n == "cargo-slot-wrapper")
            .unwrap();
        assert!(lib_at < wrapper_at, "lib is published before the wrapper");
        let lib = tools.parent().unwrap().join("lib/cargo-slot");
        swap_in(&manifest, id).unwrap();
        assert_eq!(std::fs::read(&lib).unwrap(), slot);
        assert_eq!(
            std::fs::read(tools.join("cargo-slot")).unwrap(),
            CARGO_SLOT_WRAPPER
        );
        assert!(is_cargo_slot_wrapper(&tools.join("cargo-slot")));
        assert!(
            crate::daemon_info::installed_tools_in(&tools)[0].installed,
            "the migrated layout reports installed"
        );
        rollback(&manifest.iter().collect::<Vec<_>>());
        assert_eq!(std::fs::read(tools.join("cargo-slot")).unwrap(), old);
        assert!(!lib.exists(), "a first-time lib install is removed");
    }

    /// A host with no `cargo-slot` anywhere gets both halves from the deploy and
    /// a rollback removes both.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn cargo_slot_deploy_bootstraps_a_missing_wrapper_and_implementation() {
        let (f, tools) = fixture_with_tools_dir();
        let (id, manifest) = stage_cargo_slot_deploy(&f);
        swap_in(&manifest, id).unwrap();
        let lib = tools.parent().unwrap().join("lib/cargo-slot");
        assert!(lib.is_file());
        assert!(is_cargo_slot_wrapper(&tools.join("cargo-slot")));
        rollback(&manifest.iter().collect::<Vec<_>>());
        assert!(!lib.exists());
        assert!(!tools.join("cargo-slot").exists());
    }

    /// A host that already runs the wrapper keeps it (no wrapper entry); a
    /// deploy without a `cargo-slot` and no installed `lib` never publishes a
    /// wrapper that would point at nothing.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn cargo_slot_wrapper_is_staged_only_when_needed_and_only_over_an_implementation() {
        let (f, tools) = fixture_with_tools_dir();
        let (_, manifest) = {
            std::fs::create_dir_all(&tools).unwrap();
            std::fs::write(tools.join("cargo-slot"), CARGO_SLOT_WRAPPER).unwrap();
            std::fs::set_permissions(
                tools.join("cargo-slot"),
                std::os::unix::fs::PermissionsExt::from_mode(0o755),
            )
            .unwrap();
            stage_cargo_slot_deploy(&f)
        };
        assert!(
            manifest
                .iter()
                .all(|entry| entry.name != "cargo-slot-wrapper")
        );
        let (f, tools) = fixture_with_tools_dir();
        let manifest = f
            .service
            .stage_plan()
            .stage_binaries(Uuid::new_v4(), f.source.to_str().unwrap(), SHA, 1)
            .unwrap();
        assert!(
            manifest
                .iter()
                .all(|entry| entry.name != "cargo-slot-wrapper")
        );
        assert!(!tools.join("cargo-slot").exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_missing_rsid_member_is_refused_naming_the_member_and_directory() {
        let f = fixture(true);
        std::fs::remove_file(f.source.join("rsid")).unwrap();
        let error = f
            .service
            .stage_plan()
            .stage_binaries(Uuid::new_v4(), f.source.to_str().unwrap(), SHA, 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains(DEPLOY_BINARY_MISSING), "{error}");
        assert!(error.contains("member rsid"), "{error}");
        assert!(error.contains(f.source.to_str().unwrap()), "{error}");
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_member_failing_its_syntax_check_is_refused_naming_member_step_and_reason() {
        let (f, _tools) = fixture_with_tools_dir();
        std::fs::write(f.source.join("target-pool"), b"#!/bin/sh\nif then\n").unwrap();
        let error = f
            .service
            .stage_plan()
            .stage_binaries(Uuid::new_v4(), f.source.to_str().unwrap(), SHA, 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains(DEPLOY_STAGE_FAILED), "{error}");
        assert!(error.contains("member target-pool"), "{error}");
        assert!(error.contains("syntax check failed"), "{error}");
        assert!(error.contains("sh -n exited"), "{error}");
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_script_with_an_unsupported_shebang_is_refused_not_run_as_another_language() {
        let (f, _tools) = fixture_with_tools_dir();
        std::fs::write(
            f.source.join("target-pool"),
            b"#!/usr/bin/env ruby\nputs 1\n",
        )
        .unwrap();
        let error = f
            .service
            .stage_plan()
            .stage_binaries(Uuid::new_v4(), f.source.to_str().unwrap(), SHA, 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("member target-pool"), "{error}");
        assert!(error.contains("unsupported script interpreter"), "{error}");
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_binaries_dir_without_the_tool_scripts_names_them_as_skipped() {
        let (f, tools) = fixture_with_tools_dir();
        let manifest = f
            .service
            .stage_plan()
            .stage_binaries(Uuid::new_v4(), f.source.to_str().unwrap(), SHA, 1)
            .unwrap();
        let skipped = rsi_common::agent_deploy::skipped_binaries(&manifest);
        assert!(skipped.contains(&"cargo-slot".to_string()), "{skipped:?}");
        assert!(skipped.contains(&"target-pool".to_string()), "{skipped:?}");
        assert!(!tools.join("cargo-slot").exists() && !tools.join("target-pool").exists());
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn an_unparseable_tool_script_is_refused_and_leaves_nothing_staged() {
        let broken: &[(&str, &[u8])] =
            &[("cargo-slot", b"if then\n"), ("target-pool", b"if then\n")];
        for (name, body) in broken {
            let (f, tools) = fixture_with_tools_dir();
            std::fs::write(f.source.join(name), body).unwrap();
            let error = f
                .service
                .stage_plan()
                .stage_binaries(Uuid::new_v4(), f.source.to_str().unwrap(), SHA, 1)
                .unwrap_err();
            assert!(
                error.to_string().contains(DEPLOY_STAGE_FAILED),
                "{name}: {error}"
            );
            assert_eq!(installed(&f), OLD);
            let left = std::fs::read_dir(&tools).map_or(0, Iterator::count);
            assert_eq!(left, 0, "{name}: staged copy must be removed");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn absent_optional_binaries_are_skipped_and_only_rsid_is_required() {
        let f = fixture(true);
        let manifest = f
            .service
            .stage_plan()
            .stage_binaries(Uuid::new_v4(), f.source.to_str().unwrap(), SHA, 1)
            .unwrap();
        assert_eq!(manifest.len(), 1);
        let skipped = rsi_common::agent_deploy::skipped_binaries(&manifest);
        assert!(skipped.contains(&"rsi-rolling-land".to_string()));
        assert!(skipped.contains(&"rsi-remote".to_string()));
        assert!(!skipped.contains(&"rsid".to_string()));
        std::fs::remove_file(f.source.join("rsid")).unwrap();
        let error = f
            .service
            .stage_plan()
            .stage_binaries(Uuid::new_v4(), f.source.to_str().unwrap(), SHA, 1)
            .unwrap_err();
        assert!(error.to_string().contains(DEPLOY_BINARY_MISSING), "{error}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn quiet_point_gate_waits_for_workers_then_swaps_and_restarts() {
        let f = fixture(true);
        let worker = Uuid::new_v4();
        session(&*f.store.lock().await, worker, "Running", Some(f.owner));
        let row = stage(&f, "gate", 900).await;
        let mut gate = GateState::default();
        let now = Utc::now();

        // A scoped worker mid-turn blocks; the caller's own turn does not.
        let waiting = poll_once(&f.store, &f.service, now, &mut gate, &f.drain, true)
            .await
            .unwrap();
        assert_eq!(waiting, PollOutcome::Waiting(vec!["worker_mid_turn"]));
        assert_eq!(installed(&f), OLD);
        f.store
            .lock()
            .await
            .conn
            .execute(
                "UPDATE sessions SET status='Completed' WHERE id=?1",
                [worker.to_string()],
            )
            .unwrap();

        // Quiet must hold on two consecutive polls before anything is swapped.
        let first = poll_once(&f.store, &f.service, now, &mut gate, &f.drain, true)
            .await
            .unwrap();
        assert_eq!(first, PollOutcome::Waiting(vec![]));
        assert_eq!(installed(&f), OLD);
        assert_eq!(f.restarts.load(Ordering::SeqCst), 0);
        let second = poll_once(&f.store, &f.service, now, &mut gate, &f.drain, true)
            .await
            .unwrap();
        assert_eq!(second, PollOutcome::Restarting(row.id));
        assert_eq!(f.restarts.load(Ordering::SeqCst), 1);
        assert_eq!(installed(&f), NEW);
        assert_eq!(std::fs::read(f.install.join("rsid.prev")).unwrap(), OLD);
        let state = f
            .store
            .lock()
            .await
            .get_agent_deploy(row.id)
            .unwrap()
            .unwrap()
            .state;
        assert_eq!(state, DeployState::Restarting);
    }

    /// #1320/#1311: an agent deploy holds new worker starts only inside the
    /// operator's hold window. Past it the deploy keeps waiting for a quiet
    /// point while launches run; at the first quiet poll the hold engages again
    /// through the confirming poll, and the deploy restarts before its deadline.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn the_hold_is_capped_and_the_deploy_restarts_at_a_later_lull() {
        let f = fixture(true);
        let worker = Uuid::new_v4();
        session(&*f.store.lock().await, worker, "Running", Some(f.owner));
        let row = stage(&f, "lull", 3600).await;
        f.drain.set_hold_cap_secs(60);
        let mut gate = GateState::default();
        let start = Utc::now();
        let other = Some(Uuid::new_v4());

        let inside = poll_once(&f.store, &f.service, start, &mut gate, &f.drain, true)
            .await
            .unwrap();
        assert_eq!(inside, PollOutcome::Waiting(vec!["worker_mid_turn"]));
        assert!(f.drain.holds(other, true), "held inside the window");
        let status = f.drain.status();
        assert!(status.draining && status.waiting);
        // Worker starts are released by the window, not the 3600 s deadline;
        // new test/build jobs stay held to the deadline (#3373).
        assert!(
            !f.drain
                .holds_at(other, true, start + ChronoDuration::seconds(61)),
            "worker hold released by the window"
        );
        assert_eq!(f.drain.job_release_by(), Some(row.deadline_at));

        // Past the window: still waiting on the worker, nothing held.
        let past = start + ChronoDuration::seconds(120);
        let unheld = poll_once(&f.store, &f.service, past, &mut gate, &f.drain, true)
            .await
            .unwrap();
        assert_eq!(unheld, PollOutcome::Waiting(vec!["worker_mid_turn"]));
        assert!(!f.drain.holds(other, true), "launches run past the window");
        let status = f.drain.status();
        assert!(
            status.draining,
            "only the new-job hold (#3373) remains past the worker window"
        );
        assert!(f.drain.holds_job_start(other, true));
        assert!(status.waiting, "the deploy still waits for a lull");
        assert_eq!(status.deploy_id, Some(row.id));
        assert_eq!(status.blockers, vec!["worker_mid_turn".to_string()]);
        assert_eq!(installed(&f), OLD);

        // The worker's turn ends: a lull. The hold engages for the confirming
        // poll, then the deploy swaps and restarts.
        f.store
            .lock()
            .await
            .conn
            .execute(
                "UPDATE sessions SET status='Completed' WHERE id=?1",
                [worker.to_string()],
            )
            .unwrap();
        let lull = poll_once(&f.store, &f.service, past, &mut gate, &f.drain, true)
            .await
            .unwrap();
        assert_eq!(lull, PollOutcome::Waiting(vec![]));
        assert!(f.drain.holds(other, true), "held from the first quiet poll");
        let restart = poll_once(&f.store, &f.service, past, &mut gate, &f.drain, true)
            .await
            .unwrap();
        assert_eq!(restart, PollOutcome::Restarting(row.id));
        assert_eq!(installed(&f), NEW);
        assert!(f.drain.holds(other, true), "held through the restart");
    }

    /// A zero window never holds: the deploy only waits for a lull.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn a_zero_hold_window_waits_for_a_lull_without_holding() {
        let f = fixture(true);
        let worker = Uuid::new_v4();
        session(&*f.store.lock().await, worker, "Running", Some(f.owner));
        stage(&f, "no-hold", 900).await;
        f.drain.set_hold_cap_secs(0);
        let mut gate = GateState::default();
        let waiting = poll_once(&f.store, &f.service, Utc::now(), &mut gate, &f.drain, true)
            .await
            .unwrap();
        assert_eq!(waiting, PollOutcome::Waiting(vec!["worker_mid_turn"]));
        assert!(!f.drain.is_draining());
        assert!(f.drain.status().waiting);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn quiet_point_gate_blocks_on_lander_and_job_and_a_dirty_poll_resets_the_count() {
        let f = fixture(true);
        stage(&f, "blockers", 900).await;
        let mut gate = GateState::default();
        let now = Utc::now();
        assert_eq!(
            poll_once(&f.store, &f.service, now, &mut gate, &f.drain, true)
                .await
                .unwrap(),
            PollOutcome::Waiting(vec![])
        );
        {
            let store = f.store.lock().await;
            store
                .enqueue_rolling_queue_source(
                    &NewQueueEntry {
                        project_id: None,
                        repo_path: "/tmp/repo".into(),
                        source_commit: "b".repeat(40),
                        source_session_id: f.owner,
                        owner_epic_id: None,
                        binding: RollingQueueBinding::Unbound,
                        work_key: None,
                        migration_version: None,
                        hot_files: vec![],
                        test_filters: vec![],
                        fixes_tests: Vec::new(),
                        idempotency_key: "land".into(),
                    },
                    now,
                )
                .unwrap();
            store.claim_next_rolling_queue_entry(now).unwrap().unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO agent_jobs(id, owner_session_id, kind, params_json, cwd, \
                     unit_name, log_path, status_path, state, created_at, row_version) \
                     VALUES (?1,?2,'build','{}','/tmp','u','/tmp/l','/tmp/s','running',?3,1)",
                    rusqlite::params![
                        Uuid::new_v4().to_string(),
                        f.owner.to_string(),
                        now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                    ],
                )
                .unwrap();
        }
        // The clean first poll above was reset by this dirty one.
        assert_eq!(
            poll_once(&f.store, &f.service, now, &mut gate, &f.drain, true)
                .await
                .unwrap(),
            PollOutcome::Waiting(vec!["landing_in_progress", "job_running"])
        );
        assert_eq!(f.restarts.load(Ordering::SeqCst), 0);
        assert_eq!(installed(&f), OLD);
    }

    /// #1177: a deploy waits only for work a restart would break. Off-host jobs
    /// that survive and reattach (`cloud_sweep`, `cloud_gate`) do not block;
    /// a local job does, and the blocker list is visible in the drain status.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn off_host_cloud_jobs_do_not_block_a_deploy_but_a_local_job_does_and_the_drain_names_it()
    {
        let f = fixture(true);
        stage(&f, "cloud-jobs", 900).await;
        let mut gate = GateState::default();
        let now = Utc::now();
        let insert = |kind: &str| {
            let store = f.store.try_lock().unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO agent_jobs(id, owner_session_id, kind, params_json, cwd, \
                     unit_name, log_path, status_path, state, created_at, row_version) \
                     VALUES (?1,?2,?3,'{}','/tmp',?1,'/tmp/l','/tmp/s','running',?4,1)",
                    rusqlite::params![
                        Uuid::new_v4().to_string(),
                        f.owner.to_string(),
                        kind,
                        now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                    ],
                )
                .unwrap();
        };
        insert("cloud_sweep");
        insert("cloud_gate");
        // #3181: a benchmark run lasts hours in its own unit; a deploy never waits
        // on it (the trials a restart interrupts are rerun once).
        insert("bench");
        assert_eq!(
            poll_once(&f.store, &f.service, now, &mut gate, &f.drain, true)
                .await
                .unwrap(),
            PollOutcome::Waiting(vec![]),
            "running cloud and bench jobs are not blockers"
        );
        assert!(f.drain.status().blockers.is_empty());
        assert_eq!(gate.quiet_polls, 1);
        insert("build");
        assert_eq!(
            poll_once(&f.store, &f.service, now, &mut gate, &f.drain, true)
                .await
                .unwrap(),
            PollOutcome::Waiting(vec!["job_running"])
        );
        let status = f.drain.status();
        assert!(status.draining);
        assert_eq!(status.blockers, vec!["job_running".to_string()]);
        assert_eq!(gate.quiet_polls, 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn quiet_point_wait_is_bounded_and_the_timeout_wakes_the_owner_once() {
        let f = fixture(true);
        let worker = Uuid::new_v4();
        session(&*f.store.lock().await, worker, "Running", Some(f.owner));
        let row = stage(&f, "timeout", 5).await;
        let mut gate = GateState::default();
        let late = Utc::now() + ChronoDuration::seconds(10);
        let settled = poll_once(&f.store, &f.service, late, &mut gate, &f.drain, true)
            .await
            .unwrap();
        assert_eq!(settled, PollOutcome::Settled(row.id, DeployState::TimedOut));
        let again = poll_once(&f.store, &f.service, late, &mut gate, &f.drain, true)
            .await
            .unwrap();
        assert_eq!(again, PollOutcome::Idle);
        let store = f.store.lock().await;
        let wake = wakes(&store, f.owner);
        assert_eq!(wake.len(), 1);
        assert!(wake[0].message.contains("timed_out"), "{}", wake[0].message);
        assert!(
            wake[0].message.contains("worker_mid_turn"),
            "{}",
            wake[0].message
        );
        // #1333 andon: the timeout is friction data for the deploy's owner.
        let friction = store
            .friction_rollup(
                &rsi_common::friction::ListFrictionRollupRequestV1::default(),
                late,
            )
            .unwrap();
        let row_signature = friction
            .rows
            .iter()
            .find(|r| r.signature == "deploy_timeout:worker_mid_turn")
            .expect("deploy timeout friction recorded");
        assert_eq!(row_signature.occurrences, 1);
        assert_eq!(
            row_signature.evidence_refs,
            vec![format!("deploy:{}", row.id)]
        );
        assert_eq!(installed(&f), OLD);
        // The staged copy is removed when the wait times out.
        let leftovers = std::fs::read_dir(&f.install)
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".deploy-")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    fn set_status(store: &Store, id: Uuid, status: &str) {
        store
            .conn
            .execute(
                "UPDATE sessions SET status=?2 WHERE id=?1",
                rusqlite::params![id.to_string(), status],
            )
            .unwrap();
    }

    async fn wait_for_held(drain: &DeployDrain, count: usize) {
        for _ in 0..400 {
            if drain.status().held.len() == count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("expected {count} held launches: {:?}", drain.status());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn a_waiting_deploy_holds_new_children_and_the_held_launch_runs_after_it_settles() {
        let f = fixture(true);
        let worker = Uuid::new_v4();
        session(&*f.store.lock().await, worker, "Running", Some(f.owner));
        let operator = Uuid::new_v4();
        session(&*f.store.lock().await, operator, "Running", None);
        let row = stage(&f, "drain", 900).await;
        let drain = Arc::new(DeployDrain::new());
        let mut gate = GateState::default();
        let now = Utc::now();

        // One running child: the deploy waits and the hold engages.
        let waiting = poll_once(&f.store, &f.service, now, &mut gate, &drain, true)
            .await
            .unwrap();
        assert_eq!(waiting, PollOutcome::Waiting(vec!["worker_mid_turn"]));
        assert!(drain.is_draining());
        let status = drain.status();
        assert_eq!(status.deploy_id, Some(row.id));

        // A new child launch is parked; nobody else is held.
        let held_child = Uuid::new_v4();
        let launched = Arc::new(AtomicUsize::new(0));
        let waiter = {
            let (drain, launched) = (Arc::clone(&drain), Arc::clone(&launched));
            tokio::spawn(async move {
                drain
                    .wait_released(
                        crate::deploy_drain::HeldKind::ChildSpawn,
                        Some(held_child),
                        true,
                    )
                    .await;
                launched.fetch_add(1, Ordering::SeqCst);
            })
        };
        wait_for_held(&drain, 1).await;
        assert_eq!(launched.load(Ordering::SeqCst), 0, "no new child starts");
        assert!(drain.holds(Some(worker), true));
        assert!(!drain.holds(Some(operator), false), "parentless operator");
        assert!(!drain.holds(Some(f.owner), true), "the deploy's caller");
        assert_eq!(drain.status().held[0].reason, "deploy_draining");

        // The running child is never interrupted: it ends on its own.
        set_status(&*f.store.lock().await, worker, "Completed");
        for _ in 0..2 {
            poll_once(&f.store, &f.service, now, &mut gate, &drain, true)
                .await
                .unwrap();
        }
        assert_eq!(f.restarts.load(Ordering::SeqCst), 1, "the deploy proceeds");
        assert!(drain.is_draining(), "held through the restart");
        assert_eq!(launched.load(Ordering::SeqCst), 0);

        // The restarted daemon verifies and settles; the next poll releases.
        let new_sha = binary_sha256(&f.install.join("rsid")).unwrap();
        verify_after_restart(&f.store, &f.service, SHA, Some(&new_sha), now)
            .await
            .unwrap();
        poll_once(&f.store, &f.service, now, &mut gate, &drain, true)
            .await
            .unwrap();
        waiter.await.unwrap();
        assert_eq!(launched.load(Ordering::SeqCst), 1, "the held launch ran");
        assert!(!drain.is_draining());
        assert!(drain.status().held.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn the_drain_timeout_releases_the_hold_and_the_deploy_settles_refused() {
        let f = fixture(true);
        let worker = Uuid::new_v4();
        session(&*f.store.lock().await, worker, "Running", Some(f.owner));
        let row = stage(&f, "drain-timeout", 60).await;
        let drain = Arc::new(DeployDrain::new());
        let mut gate = GateState::default();
        let now = Utc::now();
        poll_once(&f.store, &f.service, now, &mut gate, &drain, true)
            .await
            .unwrap();
        assert!(drain.is_draining());
        let held_child = Uuid::new_v4();
        let waiter = {
            let drain = Arc::clone(&drain);
            tokio::spawn(async move {
                drain
                    .wait_released(
                        crate::deploy_drain::HeldKind::TopologyNode,
                        Some(held_child),
                        true,
                    )
                    .await
            })
        };
        wait_for_held(&drain, 1).await;

        // The child never ends: past the max wait the deploy times out, the
        // hold is released and the parked launch runs; nothing is lost.
        let late = now + ChronoDuration::seconds(61);
        let settled = poll_once(&f.store, &f.service, late, &mut gate, &drain, true)
            .await
            .unwrap();
        assert_eq!(settled, PollOutcome::Settled(row.id, DeployState::TimedOut));
        assert!(!drain.is_draining());
        assert!(waiter.await.unwrap(), "the held launch waited, then ran");
        assert_eq!(installed(&f), OLD);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn the_operator_setting_off_holds_nothing_while_the_deploy_waits() {
        let f = fixture(true);
        session(
            &*f.store.lock().await,
            Uuid::new_v4(),
            "Running",
            Some(f.owner),
        );
        stage(&f, "drain-off", 900).await;
        let drain = DeployDrain::new();
        let mut gate = GateState::default();
        let now = Utc::now();
        let waiting = poll_once(&f.store, &f.service, now, &mut gate, &drain, false)
            .await
            .unwrap();
        assert_eq!(waiting, PollOutcome::Waiting(vec!["worker_mid_turn"]));
        assert!(!drain.is_draining());
        assert!(!drain.holds(Some(Uuid::new_v4()), true));
        // Turning it on mid-wait engages the hold on the next poll.
        poll_once(&f.store, &f.service, now, &mut gate, &drain, true)
            .await
            .unwrap();
        assert!(drain.is_draining());
        // And off again releases it.
        poll_once(&f.store, &f.service, now, &mut gate, &drain, false)
            .await
            .unwrap();
        assert!(!drain.is_draining());
    }

    /// #1461 helper: a worker (a child of the deploy's owner) mid-turn.
    async fn worker_mid_turn(f: &Fixture) -> Uuid {
        let worker = Uuid::new_v4();
        session(&*f.store.lock().await, worker, "Running", Some(f.owner));
        worker
    }

    async fn poll_at(
        f: &Fixture,
        gate: &mut GateState,
        at: DateTime<Utc>,
        drain_enabled: bool,
    ) -> PollOutcome {
        poll_once(&f.store, &f.service, at, gate, &f.drain, drain_enabled)
            .await
            .unwrap()
    }

    /// #1461: `interrupt_workers` waits out the operator's hold like any
    /// deploy, then stops treating a worker mid-turn as a blocker: after the
    /// two quiet polls the deploy swaps and restarts, records which workers it
    /// interrupted on its row, files one andon event per worker, and the outcome
    /// (written after the restart) names them.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn interrupt_workers_restarts_over_workers_mid_turn_once_the_hold_is_over() {
        let f = fixture(true);
        let w1 = worker_mid_turn(&f).await;
        let w2 = worker_mid_turn(&f).await;
        let mut expected = vec![w1, w2];
        expected.sort();
        let request: rsi_common::agent_deploy::AgentRequestDeployRequestV1 =
            serde_json::from_value(serde_json::json!({
                "sha": SHA, "binaries_dir": f.source, "idempotency_key": "interrupt",
                "peer_id": Uuid::new_v4(), "max_wait_secs": 3600
            }))
            .unwrap();
        let row = stage_with(&f, "interrupt", 3600, request.interrupts_workers()).await;
        {
            let store = f.store.lock().await;
            let receipt = crate::session::deploy_verb::receipt_with_blockers(
                &store,
                &row,
                store.agent_deploy_interrupt(row.id).unwrap(),
                false,
            )
            .unwrap();
            let mut predicted = receipt.expected_interrupted_workers;
            predicted.sort();
            assert_eq!(predicted, expected);
        }
        f.drain.set_hold_cap_secs(60);
        let mut gate = GateState::default();
        let start = Utc::now();
        let other = Some(Uuid::new_v4());

        // Inside the hold the workers still block, and new starts are held.
        for at in [start, start + ChronoDuration::seconds(59)] {
            assert_eq!(
                poll_at(&f, &mut gate, at, true).await,
                PollOutcome::Waiting(vec!["worker_mid_turn"])
            );
        }
        assert!(f.drain.holds(other, true));
        assert_eq!(installed(&f), OLD);

        // Past it they do not: a quiet poll (the hold engages again), then the
        // confirming poll swaps and restarts.
        let past = start + ChronoDuration::seconds(120);
        assert_eq!(
            poll_at(&f, &mut gate, past, true).await,
            PollOutcome::Waiting(vec![])
        );
        assert!(f.drain.holds(other, true), "held from the first quiet poll");
        assert_eq!(f.restarts.load(Ordering::SeqCst), 0);
        assert_eq!(
            poll_at(&f, &mut gate, past, true).await,
            PollOutcome::Restarting(row.id)
        );
        assert_eq!(f.restarts.load(Ordering::SeqCst), 1);
        assert_eq!(installed(&f), NEW);
        {
            let store = f.store.lock().await;
            assert_eq!(
                store.agent_deploy_interrupt(row.id).unwrap(),
                crate::store::agent_deploys::DeployInterrupt {
                    requested: true,
                    interrupted: expected.clone(),
                    cancelled_jobs: Vec::new(),
                }
            );
            let rollup = store
                .friction_rollup(
                    &rsi_common::friction::ListFrictionRollupRequestV1::default(),
                    past,
                )
                .unwrap();
            let event = rollup
                .rows
                .iter()
                .find(|r| r.signature == "deploy_interrupt:worker_mid_turn")
                .expect("an andon event per interrupted worker");
            assert_eq!((event.occurrences, event.sessions), (2, 2));
            assert_eq!(event.evidence_refs, vec![format!("deploy:{}", row.id)]);
        }

        // The next process settles the deploy and tells the manager who was cut off.
        let new_sha = binary_sha256(&f.install.join("rsid")).unwrap();
        assert_eq!(
            verify_after_restart(&f.store, &f.service, SHA, Some(&new_sha), past)
                .await
                .unwrap(),
            Some(PollOutcome::Settled(row.id, DeployState::Succeeded))
        );
        let store = f.store.lock().await;
        let wake = wakes(&store, f.owner);
        assert_eq!(wake.len(), 1);
        for worker in &expected {
            assert!(
                wake[0].message.contains(&worker.to_string()),
                "{}",
                wake[0].message
            );
        }
    }

    /// #1461: without the option nothing changes: a worker mid-turn blocks past
    /// the hold for as long as it runs, and nothing is interrupted or recorded.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn without_interrupt_workers_a_worker_mid_turn_blocks_past_the_hold() {
        let f = fixture(true);
        worker_mid_turn(&f).await;
        let request: rsi_common::agent_deploy::AgentRequestDeployRequestV1 =
            serde_json::from_value(serde_json::json!({
                "sha": SHA, "binaries_dir": f.source, "idempotency_key": "plain",
                "peer_id": Uuid::new_v4(), "interrupt_workers": false
            }))
            .unwrap();
        let row = stage_with(&f, "plain", 3600, request.interrupts_workers()).await;
        f.drain.set_hold_cap_secs(60);
        let mut gate = GateState::default();
        let past = Utc::now() + ChronoDuration::seconds(120);
        for _ in 0..3 {
            assert_eq!(
                poll_at(&f, &mut gate, past, true).await,
                PollOutcome::Waiting(vec!["worker_mid_turn"])
            );
        }
        assert_eq!(f.restarts.load(Ordering::SeqCst), 0);
        assert_eq!(installed(&f), OLD);
        assert_eq!(
            f.store.lock().await.agent_deploy_interrupt(row.id).unwrap(),
            crate::store::agent_deploys::DeployInterrupt::default()
        );
    }

    /// #3373: a staged deploy that does not interrupt workers still needs the
    /// running jobs to drain, so it holds new `test`/`build` job starts to its
    /// deadline, past the worker-start hold cap, with no retry window; with the
    /// drain setting off nothing is held, and a cancel releases the hold.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn a_staged_deploy_without_interrupt_workers_holds_new_jobs_past_the_worker_hold() {
        for enabled in [true, false] {
            let f = fixture(true);
            worker_mid_turn(&f).await;
            let row = stage_with(&f, "plain-jobs", 3600, false).await;
            f.drain.set_hold_cap_secs(60);
            let mut gate = GateState::default();
            let past = Utc::now() + ChronoDuration::seconds(120);
            let worker = Some(Uuid::new_v4());
            for _ in 0..3 {
                assert_eq!(
                    poll_at(&f, &mut gate, past, enabled).await,
                    PollOutcome::Waiting(vec!["worker_mid_turn"])
                );
            }
            assert!(!f.drain.holds(worker, true), "the worker hold is over");
            assert_eq!(f.drain.holds_job_start_at(worker, true, past), enabled);
            assert_eq!(
                f.drain.holds_job_start_at(Some(f.owner), false, past),
                enabled,
                "the deploy's own caller is held too"
            );
            assert_eq!(f.drain.job_release_by(), enabled.then_some(row.deadline_at));
            // No retry window: the hold ends at the deadline.
            assert!(!f.drain.holds_job_start_at(worker, true, row.deadline_at));
            f.drain.release(row.id);
            assert!(!f.drain.holds_job_start_at(worker, true, past));
        }
    }

    /// A landing and a local build job still block the runner past the hold; an
    /// ordinary test job does not (#3554: it survives the restart).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn interrupt_workers_still_waits_for_a_landing_and_a_local_job() {
        let f = fixture(true);
        worker_mid_turn(&f).await;
        let row = stage_with(&f, "keep-waiting", 3600, true).await;
        f.drain.set_hold_cap_secs(60);
        let mut gate = GateState::default();
        let now = Utc::now() + ChronoDuration::seconds(120);
        {
            let store = f.store.lock().await;
            store
                .enqueue_rolling_queue_source(
                    &NewQueueEntry {
                        project_id: None,
                        repo_path: "/tmp/repo".into(),
                        source_commit: "b".repeat(40),
                        source_session_id: f.owner,
                        owner_epic_id: None,
                        binding: RollingQueueBinding::Unbound,
                        work_key: None,
                        migration_version: None,
                        hot_files: vec![],
                        test_filters: vec![],
                        fixes_tests: Vec::new(),
                        idempotency_key: "land".into(),
                    },
                    now,
                )
                .unwrap();
            store.claim_next_rolling_queue_entry(now).unwrap().unwrap();
        }
        assert_eq!(
            poll_at(&f, &mut gate, now, true).await,
            PollOutcome::Waiting(vec!["landing_in_progress"])
        );
        f.store
            .lock()
            .await
            .conn
            .execute(
                "INSERT INTO agent_jobs(id, owner_session_id, kind, params_json, cwd, \
                 unit_name, log_path, status_path, state, created_at, row_version) \
                 VALUES (?1,?2,'test','{}','/tmp','u','/tmp/l','/tmp/s','running',?3,1)",
                rusqlite::params![
                    Uuid::new_v4().to_string(),
                    f.owner.to_string(),
                    now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
        assert_eq!(
            poll_at(&f, &mut gate, now, true).await,
            PollOutcome::Waiting(vec!["landing_in_progress", "job_running"])
        );
        assert_eq!(gate.quiet_polls, 0);
        assert_eq!(f.restarts.load(Ordering::SeqCst), 0);
        assert_eq!(installed(&f), OLD);
        assert!(
            f.store
                .lock()
                .await
                .agent_deploy_interrupt(row.id)
                .unwrap()
                .interrupted
                .is_empty()
        );
    }

    /// #2056: the owner of a staged deploy sees what it waits for: a replay
    /// during `landing_in_progress` names the gating queue entry (sequence and
    /// batch), a running local job names the job, its kind and owner, and the
    /// receipt says since when.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn a_replay_names_the_gating_entry_and_the_running_job() {
        let f = fixture(true);
        let row = stage(&f, "why-staged", 3600).await;
        let now = Utc::now();
        let blockers = |f: &Fixture| {
            let store = f.store.try_lock().unwrap();
            crate::session::deploy_verb::receipt_with_blockers(
                &store,
                &row,
                crate::store::agent_deploys::DeployInterrupt::default(),
                true,
            )
            .unwrap()
        };
        let quiet = blockers(&f);
        assert!(quiet.blockers.is_empty());
        assert!(
            quiet.waiting_since.is_some(),
            "staged: waiting since requested"
        );
        let entry = {
            let store = f.store.lock().await;
            store
                .enqueue_rolling_queue_source(
                    &NewQueueEntry {
                        project_id: None,
                        repo_path: "/tmp/repo".into(),
                        source_commit: "b".repeat(40),
                        source_session_id: f.owner,
                        owner_epic_id: None,
                        binding: RollingQueueBinding::Unbound,
                        work_key: None,
                        migration_version: None,
                        hot_files: vec![],
                        test_filters: vec![],
                        fixes_tests: Vec::new(),
                        idempotency_key: "land".into(),
                    },
                    now,
                )
                .unwrap();
            store.claim_next_rolling_queue_entry(now).unwrap().unwrap();
            store
                .conn
                .query_row(
                    "SELECT sequence, batch_id FROM rolling_queue_entries \
                     WHERE state IN ('admitted','gating')",
                    [],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)),
                )
                .unwrap()
        };
        let receipt = blockers(&f);
        assert_eq!(receipt.blockers.len(), 1);
        let landing = &receipt.blockers[0];
        assert_eq!(landing.kind, "landing_in_progress");
        assert_eq!(landing.subjects_total, 1);
        assert_eq!(landing.subjects[0].queue_sequence, Some(entry.0));
        assert_eq!(
            landing.subjects[0].batch_id.map(|id| id.to_string()),
            entry.1,
            "the gating batch is named"
        );
        assert!(landing.subjects[0].queue_state.is_some());

        let job = Uuid::new_v4();
        f.store
            .lock()
            .await
            .conn
            .execute(
                "INSERT INTO agent_jobs(id, owner_session_id, kind, params_json, cwd, \
                 unit_name, log_path, status_path, state, created_at, row_version) \
                 VALUES (?1,?2,'test','{}','/tmp','u','/tmp/l','/tmp/s','running',?3,1)",
                rusqlite::params![
                    job.to_string(),
                    f.owner.to_string(),
                    now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
        let receipt = blockers(&f);
        let kinds: Vec<_> = receipt.blockers.iter().map(|b| b.kind.as_str()).collect();
        assert_eq!(kinds, ["landing_in_progress", "job_running"]);
        let running = &receipt.blockers[1].subjects[0];
        assert_eq!(running.job_id, Some(job));
        assert_eq!(running.job_kind.as_deref(), Some("test"));
        assert_eq!(running.owner_session_id, Some(f.owner));
        assert_eq!(
            f.store
                .lock()
                .await
                .deploy_quiet_blockers_with(f.owner, true)
                .unwrap(),
            vec!["landing_in_progress", "job_running"],
            "names and details agree"
        );
    }

    /// #1461: with the operator's drain setting off nothing is ever held, so
    /// there is no hold to wait out.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn interrupt_workers_has_no_hold_to_wait_out_when_the_drain_setting_is_off() {
        let f = fixture(true);
        let worker = worker_mid_turn(&f).await;
        let row = stage_with(&f, "no-drain", 3600, true).await;
        f.drain.set_hold_cap_secs(600);
        let mut gate = GateState::default();
        let now = Utc::now();
        assert_eq!(
            poll_at(&f, &mut gate, now, false).await,
            PollOutcome::Waiting(vec![])
        );
        assert_eq!(
            poll_at(&f, &mut gate, now, false).await,
            PollOutcome::Restarting(row.id)
        );
        assert_eq!(
            f.store
                .lock()
                .await
                .agent_deploy_interrupt(row.id)
                .unwrap()
                .interrupted,
            vec![worker]
        );
    }

    /// #1461: the deadline still bounds the wait. A wait shorter than the hold
    /// ends at its deadline, with the worker named as the blocker, exactly as
    /// before: the manager keeps `max_wait_secs` above the hold.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn an_interrupt_deploy_whose_deadline_precedes_the_hold_end_times_out() {
        let f = fixture(true);
        worker_mid_turn(&f).await;
        let row = stage_with(&f, "short-wait", 30, true).await;
        f.drain.set_hold_cap_secs(600);
        let mut gate = GateState::default();
        let late = Utc::now() + ChronoDuration::seconds(40);
        assert_eq!(
            poll_at(&f, &mut gate, late, true).await,
            PollOutcome::Settled(row.id, DeployState::TimedOut)
        );
        let store = f.store.lock().await;
        let wake = wakes(&store, f.owner);
        assert!(
            wake[0].message.contains("worker_mid_turn"),
            "{}",
            wake[0].message
        );
        assert_eq!(f.restarts.load(Ordering::SeqCst), 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn restart_verification_succeeds_once_and_wakes_exactly_once() {
        let f = fixture(true);
        let row = stage(&f, "verify", 900).await;
        let mut gate = GateState::default();
        let now = Utc::now();
        for _ in 0..2 {
            poll_once(&f.store, &f.service, now, &mut gate, &f.drain, true)
                .await
                .unwrap();
        }
        // A deploy in flight: the marker and `.prev` sit beside the install path.
        let marker = f.install.join("rsid.deploy-inflight");
        assert!(marker.exists());
        assert!(f.install.join("rsid.prev").exists());
        let new_sha = binary_sha256(&f.install.join("rsid")).unwrap();
        let outcome = verify_after_restart(&f.store, &f.service, SHA, Some(&new_sha), now)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            Some(PollOutcome::Settled(row.id, DeployState::Succeeded))
        );
        // Verified: nothing is left that could restore a stale binary later.
        assert!(!marker.exists());
        assert!(!f.install.join("rsid.prev").exists());
        assert_eq!(installed(&f), NEW);
        // A second verification (or a second startup) finds nothing live.
        assert_eq!(
            verify_after_restart(&f.store, &f.service, SHA, Some(&new_sha), now)
                .await
                .unwrap(),
            None
        );
        let store = f.store.lock().await;
        let wake = wakes(&store, f.owner);
        assert_eq!(wake.len(), 1);
        assert_eq!(wake[0].wake_mode, rsi_common::types::WakeMode::Resume);
        assert!(wake[0].message.contains("succeeded"), "{}", wake[0].message);
        assert!(wake[0].message.contains(SHA), "{}", wake[0].message);
        assert!(
            wake[0].message.contains("Skipped (not in binaries_dir)")
                && wake[0].message.contains("rsi-rolling-land"),
            "{}",
            wake[0].message
        );
        assert_eq!(f.restarts.load(Ordering::SeqCst), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn migrated_database_rollback_waits_for_offline_supervisor_recovery() {
        let mut f = fixture(true);
        f.service.plan.probe = Arc::new(|path: &Path| {
            let schema = if path
                .extension()
                .is_some_and(|extension| extension == "prev")
            {
                0
            } else {
                999
            };
            Ok((SHA.to_string(), schema))
        });
        let row = stage(&f, "database-rollback", 900).await;
        swap_in(&row.manifest, row.id).unwrap();
        let database = f._dir.path().join("rsi.db");
        let store = Store::open(&database).unwrap();
        let backup = prepare_database_rollback(&store, &f.service, &row)
            .unwrap()
            .unwrap();
        assert_eq!(
            store.schema_version().unwrap(),
            crate::store::LATEST_SCHEMA_VERSION
        );
        assert_eq!(std::fs::read(f.install.join("rsid.failed")).unwrap(), NEW);
        assert_eq!(
            std::fs::read_to_string(f.install.join("rsid.db-rollback")).unwrap(),
            database.to_str().unwrap()
        );
        rollback(&row.manifest.iter().collect::<Vec<_>>());
        assert_eq!(installed(&f), OLD);
        assert!(backup.exists());
        drop(store);
        assert_eq!(
            crate::store::migration_backup::restore(&database, 0).unwrap(),
            Some(backup)
        );
        let restored = rusqlite::Connection::open(&database).unwrap();
        assert_eq!(
            restored
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                .unwrap(),
            0
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn failed_verification_restores_previous_binaries_and_wakes_once() {
        let f = fixture(true);
        let row = stage(&f, "rollback", 900).await;
        let mut gate = GateState::default();
        let now = Utc::now();
        for _ in 0..2 {
            poll_once(&f.store, &f.service, now, &mut gate, &f.drain, true)
                .await
                .unwrap();
        }
        assert_eq!(installed(&f), NEW);
        let other = "f".repeat(40);
        let outcome = verify_after_restart(&f.store, &f.service, &other, Some("0"), now)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            Some(PollOutcome::Settled(row.id, DeployState::Failed))
        );
        assert!(!f.install.join("rsid.deploy-inflight").exists());
        assert_eq!(installed(&f), OLD);
        let store = f.store.lock().await;
        let wake = wakes(&store, f.owner);
        assert_eq!(wake.len(), 1);
        assert!(wake[0].message.contains("failed"), "{}", wake[0].message);
        assert!(
            wake[0].message.contains("previous binaries restored"),
            "{}",
            wake[0].message
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_failed_check_names_itself_and_never_reports_equal_values_as_a_mismatch() {
        let same = "a".repeat(40);
        let other = "b".repeat(40);
        let BuildCheck::Failed(reason) = check_running_build(&same, &other, Some("h"), Some("h"))
        else {
            panic!("a different build_sha fails");
        };
        assert!(reason.starts_with("build_sha check: running "), "{reason}");
        assert!(
            reason.contains(&same) && reason.contains(&other),
            "{reason}"
        );
        let BuildCheck::Failed(reason) = check_running_build(&same, &same, Some("h1"), Some("h2"))
        else {
            panic!("a different binary hash fails");
        };
        assert!(
            reason.contains("binary_sha256 check: running h2, expected h1"),
            "{reason}"
        );
        assert!(
            reason.contains("build_sha") && reason.contains("matched"),
            "{reason}"
        );
        assert!(matches!(
            check_running_build(&same, &same, Some("h1"), None),
            BuildCheck::Unreadable(reason) if reason.contains("could not be hashed")
        ));
        assert_eq!(
            check_running_build(&same, &same, Some("h1"), Some("h1")),
            BuildCheck::Verified
        );
    }

    fn patience(stall_ms: u64, cap_ms: u64) -> VerifyPatience {
        VerifyPatience {
            stall: Duration::from_millis(stall_ms),
            hard_cap: Duration::from_millis(cap_ms),
            poll: Duration::from_millis(2),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn a_slow_but_progressing_startup_still_verifies() {
        let f = fixture(true);
        let row = stage(&f, "slow", 900).await;
        let mut gate = GateState::default();
        let now = Utc::now();
        for _ in 0..2 {
            poll_once(&f.store, &f.service, now, &mut gate, &f.drain, true)
                .await
                .unwrap();
        }
        let new_sha = binary_sha256(&f.install.join("rsid")).unwrap();
        let mut attempts = 0;
        let outcome = verify_with_patience(
            &f.store,
            &f.service,
            SHA,
            patience(60_000, 60_000),
            || {
                attempts += 1;
                (attempts > 5).then(|| new_sha.clone())
            },
            || Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            Some(PollOutcome::Settled(row.id, DeployState::Succeeded))
        );
        assert_eq!(attempts, 6);
        assert_eq!(installed(&f), NEW);
        assert_eq!(f.restarts.load(Ordering::SeqCst), 1, "no restore restart");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn an_unreadable_binary_fails_naming_the_check_once_progress_stalls_or_the_cap_hits() {
        for (label, patience, age) in [
            ("stalled", patience(10, 60_000), Duration::from_secs(1)),
            ("capped", patience(60_000, 30), Duration::ZERO),
        ] {
            let f = fixture(true);
            let row = stage(&f, label, 900).await;
            let mut gate = GateState::default();
            let now = Utc::now();
            for _ in 0..2 {
                poll_once(&f.store, &f.service, now, &mut gate, &f.drain, true)
                    .await
                    .unwrap();
            }
            let outcome =
                verify_with_patience(&f.store, &f.service, SHA, patience, || None, || age)
                    .await
                    .unwrap();
            assert_eq!(
                outcome,
                Some(PollOutcome::Settled(row.id, DeployState::Failed)),
                "{label}"
            );
            let store = f.store.lock().await;
            let message = wakes(&store, f.owner)[0].message.clone();
            assert!(
                message.contains("binary_sha256 check"),
                "{label}: {message}"
            );
            assert!(
                message.contains("could not be hashed"),
                "{label}: {message}"
            );
            drop(store);
            assert_eq!(installed(&f), OLD, "{label}");
            assert_eq!(
                f.restarts.load(Ordering::SeqCst),
                2,
                "{label}: the restored build is restarted into"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn a_failed_verification_restarts_unless_the_running_build_is_the_installed_one() {
        for (label, running_is_old) in [("new-running", false), ("old-running", true)] {
            let f = fixture(true);
            let row = stage(&f, label, 900).await;
            let mut gate = GateState::default();
            let now = Utc::now();
            for _ in 0..2 {
                poll_once(&f.store, &f.service, now, &mut gate, &f.drain, true)
                    .await
                    .unwrap();
            }
            let new_sha = binary_sha256(&f.install.join("rsid")).unwrap();
            let old_sha = binary_sha256(&f.install.join("rsid.prev")).unwrap();
            // The new build runs with a wrong build_sha, or the old one runs.
            let (build, running) = if running_is_old {
                ("f".repeat(40), old_sha)
            } else {
                ("e".repeat(40), new_sha)
            };
            let outcome = verify_after_restart(&f.store, &f.service, &build, Some(&running), now)
                .await
                .unwrap();
            assert_eq!(
                outcome,
                Some(PollOutcome::Settled(row.id, DeployState::Failed))
            );
            assert_eq!(installed(&f), OLD, "{label}");
            let expected_restarts = if running_is_old { 1 } else { 2 };
            assert_eq!(
                f.restarts.load(Ordering::SeqCst),
                expected_restarts,
                "{label}: running must equal installed afterwards"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn failed_verification_removes_an_optional_binary_that_was_absent_before() {
        let f = fixture(true);
        // `rsi-rolling-land` is installed for the first time; `rsi-remote`
        // already exists and must be restored, not removed.
        std::fs::write(f.source.join("rsi-rolling-land"), b"new-lander").unwrap();
        std::fs::write(f.source.join("rsi-remote"), b"new-remote").unwrap();
        std::fs::write(f.install.join("rsi-remote"), b"old-remote").unwrap();
        let row = stage(&f, "first-time", 900).await;
        let presence: Vec<(&str, bool)> = row
            .manifest
            .iter()
            .map(|entry| (entry.name.as_str(), entry.prior_present))
            .collect();
        assert_eq!(
            presence,
            vec![
                ("rsid", true),
                ("rsi-rolling-land", false),
                ("rsi-remote", true)
            ],
            "the deploy record carries each destination's prior presence"
        );
        let mut gate = GateState::default();
        let now = Utc::now();
        for _ in 0..2 {
            poll_once(&f.store, &f.service, now, &mut gate, &f.drain, true)
                .await
                .unwrap();
        }
        assert_eq!(
            std::fs::read(f.install.join("rsi-rolling-land")).unwrap(),
            b"new-lander"
        );
        let other = "f".repeat(40);
        let outcome = verify_after_restart(&f.store, &f.service, &other, Some("0"), now)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            Some(PollOutcome::Settled(row.id, DeployState::Failed))
        );
        assert_eq!(installed(&f), OLD);
        assert_eq!(
            std::fs::read(f.install.join("rsi-remote")).unwrap(),
            b"old-remote"
        );
        assert!(
            std::fs::symlink_metadata(f.install.join("rsi-rolling-land")).is_err(),
            "a binary absent before the deploy is absent after the failed deploy"
        );
    }

    #[cfg(target_os = "macos")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn mac_restore_moves_aside_to_vacant_destination() {
        let dir = tempfile::tempdir().unwrap();
        let aside = dir.path().join("aside");
        let dest = dir.path().join("dest");
        std::fs::write(&aside, b"replacement").unwrap();
        let identity = object_identity(&aside).unwrap();
        restore_without_replacing(&aside, &dest).unwrap();
        assert_eq!(object_identity(&dest), Some(identity));
        assert_eq!(std::fs::read(&dest).unwrap(), b"replacement");
    }

    #[cfg(target_os = "macos")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn mac_restore_preserves_occupied_destination_and_aside() {
        let dir = tempfile::tempdir().unwrap();
        let aside = dir.path().join("aside");
        let dest = dir.path().join("dest");
        std::fs::write(&aside, b"replacement").unwrap();
        std::fs::write(&dest, b"new install").unwrap();
        assert_eq!(
            restore_without_replacing(&aside, &dest).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"new install");
        assert_eq!(std::fs::read(&aside).unwrap(), b"replacement");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn rollback_leaves_a_file_it_did_not_install_at_an_absent_before_path() {
        let f = fixture(true);
        std::fs::write(f.source.join("rsi-rolling-land"), b"new-lander").unwrap();
        let id = Uuid::new_v4();
        let manifest = f
            .service
            .stage_plan()
            .stage_binaries(id, f.source.to_str().unwrap(), SHA, 1)
            .unwrap();
        // The deployed copy never landed and something else sits at the path.
        let lander = f.install.join("rsi-rolling-land");
        std::fs::write(&lander, b"operator-installed").unwrap();
        rollback(&manifest.iter().collect::<Vec<_>>());
        assert_eq!(std::fs::read(&lander).unwrap(), b"operator-installed");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn rollback_removes_only_the_object_it_installed_even_when_replaced_mid_removal() {
        let f = fixture(true);
        std::fs::write(f.source.join("rsi-rolling-land"), b"new-lander").unwrap();
        let id = Uuid::new_v4();
        let manifest = f
            .service
            .stage_plan()
            .stage_binaries(id, f.source.to_str().unwrap(), SHA, 1)
            .unwrap();
        let lander_entry: Vec<&DeployBinaryV1> = manifest
            .iter()
            .filter(|entry| entry.name == "rsi-rolling-land")
            .collect();
        swap_in(&manifest, id).unwrap();
        let lander = f.install.join("rsi-rolling-land");
        assert_eq!(std::fs::read(&lander).unwrap(), b"new-lander");

        // The deploy's own object is removed, and a replacement installed
        // right after it is moved aside (between the check and the removal) is
        // preserved untouched.
        let path = lander.clone();
        *AFTER_MOVE_ASIDE.lock().unwrap() = Some(Box::new(move || {
            std::fs::write(&path, b"operator-replacement").unwrap();
        }));
        rollback(&lander_entry);
        *AFTER_MOVE_ASIDE.lock().unwrap() = None;
        assert_eq!(std::fs::read(&lander).unwrap(), b"operator-replacement");
        let leftovers: Vec<_> = std::fs::read_dir(&f.install)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".rollback-"))
            .collect();
        assert!(leftovers.is_empty(), "our object is gone: {leftovers:?}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn rollback_preserves_a_replacement_installed_before_it_runs() {
        let f = fixture(true);
        std::fs::write(f.source.join("rsi-rolling-land"), b"new-lander").unwrap();
        let id = Uuid::new_v4();
        let manifest = f
            .service
            .stage_plan()
            .stage_binaries(id, f.source.to_str().unwrap(), SHA, 1)
            .unwrap();
        swap_in(&manifest, id).unwrap();
        let lander = f.install.join("rsi-rolling-land");
        // Same bytes, different object: identity, not content, decides. The
        // deployed object is kept aside so its inode is not reused.
        std::fs::rename(&lander, f.install.join("kept-aside")).unwrap();
        std::fs::write(&lander, b"new-lander").unwrap();
        let entries: Vec<&DeployBinaryV1> = manifest
            .iter()
            .filter(|entry| entry.name == "rsi-rolling-land")
            .collect();
        rollback(&entries);
        assert_eq!(std::fs::read(&lander).unwrap(), b"new-lander");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_manifest_recorded_before_prior_presence_existed_reads_as_present() {
        let old = r#"{"name":"rsid","dest":"/x/rsid","sha256":"ab"}"#;
        let entry: DeployBinaryV1 = serde_json::from_str(old).unwrap();
        assert!(entry.prior_present);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn a_staged_copy_changed_before_the_swap_is_never_installed() {
        let f = fixture(true);
        // A second binary, so an entry swapped before the bad one is rolled back.
        std::fs::write(f._dir.path().join("build/rsi"), b"new-rsi").unwrap();
        std::fs::write(f.install.join("rsi"), b"old-rsi").unwrap();
        let row = stage(&f, "toctou", 900).await;
        let tampered = row
            .manifest
            .iter()
            .find(|entry| entry.name == "rsi")
            .map(|entry| staged_path(Path::new(&entry.dest), row.id))
            .unwrap();
        std::fs::write(&tampered, b"tampered").unwrap();
        let mut gate = GateState::default();
        let now = Utc::now();
        poll_once(&f.store, &f.service, now, &mut gate, &f.drain, true)
            .await
            .unwrap();
        let outcome = poll_once(&f.store, &f.service, now, &mut gate, &f.drain, true)
            .await
            .unwrap();
        assert_eq!(outcome, PollOutcome::Settled(row.id, DeployState::Failed));
        // Nothing new is installed, the earlier swap is rolled back, no restart.
        assert_eq!(installed(&f), OLD);
        assert_eq!(std::fs::read(f.install.join("rsi")).unwrap(), b"old-rsi");
        assert_eq!(f.restarts.load(Ordering::SeqCst), 0);
        let store = f.store.lock().await;
        let wake = wakes(&store, f.owner);
        assert_eq!(wake.len(), 1);
        assert!(
            wake[0].message.contains(DEPLOY_STAGED_CHANGED),
            "{}",
            wake[0].message
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn an_unsupervised_daemon_fails_the_deploy_at_the_quiet_point() {
        let f = fixture(false);
        let row = stage(&f, "nosup", 900).await;
        let mut gate = GateState::default();
        let now = Utc::now();
        poll_once(&f.store, &f.service, now, &mut gate, &f.drain, true)
            .await
            .unwrap();
        let outcome = poll_once(&f.store, &f.service, now, &mut gate, &f.drain, true)
            .await
            .unwrap();
        assert_eq!(outcome, PollOutcome::Settled(row.id, DeployState::Failed));
        assert_eq!(installed(&f), OLD);
        assert_eq!(f.restarts.load(Ordering::SeqCst), 0);
        let store = f.store.lock().await;
        assert!(
            wakes(&store, f.owner)[0]
                .message
                .contains(DEPLOY_NEEDS_SUPERVISOR)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn restart_budget_refuses_after_two_deploy_restarts_in_an_hour() {
        let f = fixture(true);
        let now = Utc::now();
        {
            let store = f.store.lock().await;
            for n in 0..2 {
                let id = Uuid::new_v4();
                let manifest = [DeployBinaryV1 {
                    name: "rsid".into(),
                    dest: "/x/rsid".into(),
                    sha256: "0".repeat(64),
                    prior_present: true,
                    staged_identity: None,
                }];
                store
                    .insert_agent_deploy(
                        &crate::store::agent_deploys::NewDeploy {
                            id,
                            owner_session_id: f.owner,
                            idempotency_key: &format!("budget-{n}"),
                            sha: SHA,
                            fingerprint: deploy_fingerprint(SHA, "x", 60),
                            manifest: &manifest,
                            max_wait_secs: 60,
                            interrupt_workers: false,
                        },
                        now,
                    )
                    .unwrap();
                assert!(store.mark_agent_deploy_restarting(id, now).unwrap());
                store
                    .settle_agent_deploy(id, DeployState::Succeeded, None, now)
                    .unwrap();
            }
            let error = f.service.check_budget(&store, now).unwrap_err().to_string();
            assert!(error.contains(DEPLOY_RESTART_BUDGET), "{error}");
            // #1777: the refusal names when the oldest restart leaves the hour.
            let expected =
                (now + ChronoDuration::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
            assert!(
                error.ends_with(&format!("{DEPLOY_RETRY_AFTER_MARKER}{expected}")),
                "{error} vs {expected}"
            );
            // A higher operator limit leaves room; a lower one refuses sooner.
            f.service.set_restarts_per_hour(3);
            f.service.check_budget(&store, now).unwrap();
            f.service.set_restarts_per_hour(1);
            let error = f.service.check_budget(&store, now).unwrap_err().to_string();
            assert!(error.contains(DEPLOY_RESTART_BUDGET), "{error}");
            f.service
                .set_restarts_per_hour(DEPLOY_RESTARTS_PER_HOUR_DEFAULT);
            // Outside the hour the budget is clear again.
            f.service
                .check_budget(&store, now + ChronoDuration::hours(2))
                .unwrap();
        }
    }

    fn admission_service_reading(f: &Fixture, published: &str) -> DeployService {
        let path = f._dir.path().join("supervisor-restarts");
        std::fs::write(&path, published).unwrap();
        DeployService::new(
            f.install.clone(),
            vec![f._dir.path().to_path_buf()],
            Box::new(|| true),
            Arc::new(|_: &Path| Ok((SHA.to_string(), 999))),
        )
        .with_supervisor_restarts(path)
    }

    /// #3492: crash and watchdog restarts spend the same supervisor budget as
    /// deploys, so admission reads them from the supervisor's published file
    /// and refuses the deploy the supervisor would back off or refuse.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn deploy_admission_counts_supervisor_crash_restarts_3492() {
        let f = fixture(true);
        let now = Utc::now();
        let at = now.timestamp();
        let store = f.store.lock().await;
        // limit=3 (two deploys per hour + the rollback slot): one crash leaves
        // room for a deploy, two crashes take the room a deploy needs.
        let one_crash =
            admission_service_reading(&f, &format!("limit=3\nwindow=3600\nrestart={}\n", at - 120));
        one_crash.check_budget(&store, now).unwrap();
        let two_crashes = admission_service_reading(
            &f,
            &format!(
                "limit=3\nwindow=3600\nrestart={}\nrestart={}\n",
                at - 600,
                at - 120
            ),
        );
        let error = two_crashes
            .check_budget(&store, now)
            .unwrap_err()
            .to_string();
        assert!(error.contains(DEPLOY_RESTART_BUDGET), "{error}");
        // The refusal names when the oldest restart leaves the window.
        let frees = DateTime::from_timestamp(at - 600 + 3600, 0)
            .unwrap()
            .to_rfc3339_opts(SecondsFormat::Secs, true);
        assert!(
            error.ends_with(&format!("{DEPLOY_RETRY_AFTER_MARKER}{frees}")),
            "{error} vs {frees}"
        );
        // Once the older crash left the window the deploy is admitted again.
        two_crashes
            .check_budget(&store, now + ChronoDuration::seconds(3100))
            .unwrap();
    }

    /// #3492: deploys then a crash inside one window: the supervisor holds the
    /// spent budget, so a further deploy is refused until the window frees.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn deploy_admission_refuses_after_deploys_then_a_crash_and_during_backoff_3492() {
        let f = fixture(true);
        let now = Utc::now();
        let at = now.timestamp();
        let store = f.store.lock().await;
        // Two deploys and one crash: all of limit=3, though the store holds no deploy.
        let spent = admission_service_reading(
            &f,
            &format!(
                "limit=3\nwindow=3600\nrestart={}\nrestart={}\nrestart={}\n",
                at - 900,
                at - 600,
                at - 30
            ),
        );
        let error = spent.check_budget(&store, now).unwrap_err().to_string();
        assert!(error.contains(DEPLOY_RESTART_BUDGET), "{error}");
        // The supervisor waiting out an exhausted budget refuses until it restarts.
        let backing_off = admission_service_reading(
            &f,
            &format!("limit=3\nwindow=3600\nbackoff_until={}\n", at + 300),
        );
        let error = backing_off
            .check_budget(&store, now)
            .unwrap_err()
            .to_string();
        let until = DateTime::from_timestamp(at + 300, 0)
            .unwrap()
            .to_rfc3339_opts(SecondsFormat::Secs, true);
        assert!(
            error.ends_with(&format!("{DEPLOY_RETRY_AFTER_MARKER}{until}")),
            "{error}"
        );
        // No supervisor file: only the recorded deploys count, as before.
        DeployService::new(
            f.install.clone(),
            vec![f._dir.path().to_path_buf()],
            Box::new(|| true),
            Arc::new(|_: &Path| Ok((SHA.to_string(), 999))),
        )
        .with_supervisor_restarts(f._dir.path().join("absent"))
        .check_budget(&store, now)
        .unwrap();
    }

    /// Write `<stage>/MANIFEST.sha256` the way `install-release.sh` does: one
    /// `<sha256>  <name>` line per plain file, sorted.
    fn write_stage_manifest(stage: &Path) {
        let mut lines: Vec<String> = std::fs::read_dir(stage)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != STAGE_MANIFEST_NAME)
            .map(|name| format!("{}  {name}", binary_sha256(&stage.join(&name)).unwrap()))
            .collect();
        lines.sort();
        std::fs::write(stage.join(STAGE_MANIFEST_NAME), lines.join("\n") + "\n").unwrap();
    }

    /// hub #3856: a stage holding one skipped member, with `rsid` installed.
    fn upgrade_member_fixture(root: &Path) -> (StagePlan, PathBuf, PathBuf) {
        let (install, stage) = (root.join("install"), root.join("stage"));
        std::fs::create_dir_all(&install).unwrap();
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(install.join("rsid"), NEW).unwrap();
        std::fs::write(stage.join("rsid"), NEW).unwrap();
        std::fs::write(stage.join("rsi-eval"), b"fresh-eval").unwrap();
        let service = DeployService::new(
            install.clone(),
            vec![root.to_path_buf()],
            Box::new(|| true),
            Arc::new(|_: &Path| Ok((SHA.to_string(), 999))),
        );
        (service.stage_plan(), install, stage)
    }

    /// hub #3856: a skipped member altered after the stage manifest was written
    /// is not installed, and the reason names it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn upgrade_completion_refuses_a_skipped_member_altered_after_the_manifest() {
        let root = tempfile::tempdir().unwrap();
        let (plan, install, stage) = upgrade_member_fixture(root.path());
        write_stage_manifest(&stage);
        std::fs::write(stage.join("rsi-eval"), b"tampered-eval").unwrap();
        let report = plan.complete_upgrade_members(&stage, &[], Uuid::new_v4());
        assert!(report.installed.is_empty());
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert_eq!(report.failed[0].0, "rsi-eval");
        assert!(report.failed[0].1.contains("MANIFEST.sha256"), "{report:?}");
        assert!(!install.join("rsi-eval").exists());
        let note = report.note().expect("a note");
        assert!(
            note.contains("PARTIAL INSTALL") && note.contains("rsi-eval"),
            "{note}"
        );
    }

    /// hub #3856: a skipped member the stage manifest does not list, or a stage
    /// with no manifest at all, installs nothing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn upgrade_completion_refuses_a_skipped_member_without_a_manifest_entry() {
        let root = tempfile::tempdir().unwrap();
        let (plan, install, stage) = upgrade_member_fixture(root.path());
        write_stage_manifest(&stage);
        let manifest = std::fs::read_to_string(stage.join(STAGE_MANIFEST_NAME)).unwrap();
        let without: String = manifest
            .lines()
            .filter(|line| !line.ends_with("  rsi-eval"))
            .map(|line| format!("{line}\n"))
            .collect();
        std::fs::write(stage.join(STAGE_MANIFEST_NAME), without).unwrap();
        let report = plan.complete_upgrade_members(&stage, &[], Uuid::new_v4());
        assert!(report.installed.is_empty());
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert_eq!(report.failed[0].0, "rsi-eval");
        assert!(!install.join("rsi-eval").exists());

        std::fs::remove_file(stage.join(STAGE_MANIFEST_NAME)).unwrap();
        let report = plan.complete_upgrade_members(&stage, &[], Uuid::new_v4());
        assert!(report.installed.is_empty());
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert!(!install.join("rsi-eval").exists());
    }

    /// hub #3856: a member that matches its manifest entry still installs.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn upgrade_completion_installs_a_skipped_member_that_matches_the_manifest() {
        let root = tempfile::tempdir().unwrap();
        let (plan, install, stage) = upgrade_member_fixture(root.path());
        write_stage_manifest(&stage);
        let report = plan.complete_upgrade_members(&stage, &[], Uuid::new_v4());
        assert_eq!(report.installed, vec!["rsi-eval".to_string()]);
        assert!(report.failed.is_empty(), "{report:?}");
        assert_eq!(
            std::fs::read(install.join("rsi-eval")).unwrap(),
            b"fresh-eval"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn stage_manifest_parser_reads_text_and_binary_lines_and_rejects_garbage() {
        let digest = "ab".repeat(32);
        let parsed =
            parse_stage_manifest(&format!("{digest}  rsi-eval\n{digest} *rsi-cloud\n\n")).unwrap();
        assert_eq!(parsed.get("rsi-eval"), Some(&digest));
        assert_eq!(parsed.get("rsi-cloud"), Some(&digest));
        assert!(parse_stage_manifest("nothex  rsi-eval\n").is_err());
        assert!(parse_stage_manifest(&format!("{digest}\n")).is_err());
    }

    /// #587: a stage built by a daemon that knew fewer members still reaches
    /// the host. The new daemon installs a plain member the manifest skipped,
    /// beside the resolved `rsid` with an `install_dir` link, hash-verified.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_verified_deploy_installs_a_member_the_old_inventory_skipped() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let (real, links, staging) = (
            root.path().join("install"),
            root.path().join("bin"),
            root.path().join("staging"),
        );
        let stage = staging.join(format!("bin-{}", &SHA[..9]));
        for dir in [&real, &links, &stage] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(real.join("rsid"), NEW).unwrap();
        std::os::unix::fs::symlink(real.join("rsid"), links.join("rsid")).unwrap();
        std::fs::write(stage.join("rsid"), NEW).unwrap();
        std::fs::write(stage.join("rsi-known-failure"), b"known-failure-bin").unwrap();
        // Not a member of the allowlist: never installed.
        std::fs::write(stage.join("rsi-stray"), b"stray").unwrap();
        write_stage_manifest(&stage);
        let service = DeployService::new(
            links.clone(),
            vec![root.path().to_path_buf()],
            Box::new(|| true),
            Arc::new(|_: &Path| Ok((SHA.to_string(), 999))),
        )
        .with_staging_root(staging.clone());
        let manifest = vec![DeployBinaryV1 {
            name: "rsid".to_string(),
            dest: real.join("rsid").to_string_lossy().into_owned(),
            sha256: binary_sha256(&stage.join("rsid")).unwrap(),
            prior_present: true,
            staged_identity: None,
        }];
        let row = DeployRow {
            id: Uuid::new_v4(),
            owner_session_id: None,
            sha: SHA.to_string(),
            manifest,
            state: DeployState::Restarting,
            reason: None,
            deadline_at: Utc::now(),
            operator: false,
            forced: false,
        };
        let note = service.complete_upgrade_members(&row).expect("a note");
        assert!(note.contains("installed new members"), "{note}");
        assert!(note.contains("rsi-known-failure"), "{note}");
        let installed = real.join("rsi-known-failure");
        assert_eq!(std::fs::read(&installed).unwrap(), b"known-failure-bin");
        assert_eq!(
            std::fs::metadata(&installed).unwrap().permissions().mode() & 0o111,
            0o111
        );
        assert_eq!(
            std::fs::canonicalize(links.join("rsi-known-failure")).unwrap(),
            std::fs::canonicalize(&installed).unwrap()
        );
        assert!(!real.join("rsi-stray").exists() && !links.join("rsi-stray").exists());
        // Already current: a second pass changes nothing and files no note.
        assert_eq!(service.complete_upgrade_members(&row), None);
    }

    /// #587: a stale copy of a skipped member is replaced; a member the
    /// manifest carried is never touched by the completion pass.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn upgrade_completion_replaces_a_stale_member_and_leaves_manifest_members_alone() {
        let root = tempfile::tempdir().unwrap();
        let (install, stage) = (root.path().join("install"), root.path().join("stage"));
        std::fs::create_dir_all(&install).unwrap();
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(install.join("rsid"), OLD).unwrap();
        std::fs::write(install.join("rsi-eval"), b"stale-eval").unwrap();
        std::fs::write(install.join("rsi-remote"), b"installed-remote").unwrap();
        std::fs::write(stage.join("rsid"), NEW).unwrap();
        std::fs::write(stage.join("rsi-eval"), b"fresh-eval").unwrap();
        std::fs::write(stage.join("rsi-remote"), b"staged-remote").unwrap();
        write_stage_manifest(&stage);
        let service = DeployService::new(
            install.clone(),
            vec![root.path().to_path_buf()],
            Box::new(|| true),
            Arc::new(|_: &Path| Ok((SHA.to_string(), 999))),
        );
        let manifest = vec![DeployBinaryV1 {
            name: "rsi-remote".to_string(),
            dest: install.join("rsi-remote").to_string_lossy().into_owned(),
            sha256: binary_sha256(&stage.join("rsi-remote")).unwrap(),
            prior_present: true,
            staged_identity: None,
        }];
        let report =
            service
                .stage_plan()
                .complete_upgrade_members(&stage, &manifest, Uuid::new_v4());
        assert_eq!(report.installed, vec!["rsi-eval".to_string()]);
        assert!(report.failed.is_empty());
        assert_eq!(
            std::fs::read(install.join("rsi-eval")).unwrap(),
            b"fresh-eval"
        );
        assert_eq!(std::fs::read(install.join("rsid")).unwrap(), OLD);
        assert_eq!(
            std::fs::read(install.join("rsi-remote")).unwrap(),
            b"installed-remote"
        );
    }

    /// #2681: a verified deploy prunes the old `bin-<sha>` staging directories
    /// and keeps the deployed build, the one before it and a live deploy's.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn a_verified_deploy_prunes_old_staging_bins_and_keeps_current_previous_and_pending() {
        let f = fixture(true);
        let staging = tempfile::tempdir().unwrap();
        let service = DeployService::new(
            f.install.clone(),
            vec![f.install.clone()],
            Box::new(|| true),
            Arc::new(|_: &Path| Ok((SHA.to_string(), 999))),
        )
        .with_staging_root(staging.path().to_path_buf());
        let sha = |c: char| c.to_string().repeat(40);
        let (oldest, previous, current, pending, unrelated) =
            (sha('1'), sha('2'), sha('3'), sha('4'), sha('5'));
        let now = Utc::now();
        let insert = |store: &Store, id: Uuid, sha: &str, key: &str| {
            store
                .insert_agent_deploy(
                    &crate::store::agent_deploys::NewDeploy {
                        id,
                        owner_session_id: f.owner,
                        idempotency_key: key,
                        sha,
                        fingerprint: deploy_fingerprint(sha, "x", 60),
                        manifest: &[],
                        max_wait_secs: 60,
                        interrupt_workers: false,
                    },
                    now,
                )
                .unwrap();
        };
        let store = f.store.lock().await;
        let mut deployed = None;
        for (n, sha) in [&oldest, &previous, &current].into_iter().enumerate() {
            let id = Uuid::new_v4();
            insert(&store, id, sha, &format!("prune-{n}"));
            let at = now + ChronoDuration::seconds(n as i64);
            assert!(store.mark_agent_deploy_restarting(id, at).unwrap());
            store
                .settle_agent_deploy(id, DeployState::Succeeded, None, at)
                .unwrap();
            deployed = store.get_agent_deploy(id).unwrap();
        }
        // The next deploy is queued: its directory is referenced.
        insert(&store, Uuid::new_v4(), &pending, "prune-pending");

        let day_ago = std::time::SystemTime::now() - Duration::from_secs(24 * 60 * 60);
        let dir_of = |sha: &str| staging.path().join(format!("bin-{}", &sha[..9]));
        for sha in [&oldest, &previous, &current, &pending, &unrelated] {
            let dir = dir_of(sha);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("rsid"), vec![0_u8; 64]).unwrap();
            std::fs::File::open(&dir)
                .unwrap()
                .set_modified(day_ago)
                .unwrap();
        }

        let report = service
            .prune_staging_after_success(&store, &deployed.unwrap())
            .expect("a staging root is configured")
            .await
            .unwrap();
        for sha in [&previous, &current, &pending] {
            assert!(dir_of(sha).is_dir(), "kept: {sha}");
        }
        for sha in [&oldest, &unrelated] {
            assert!(!dir_of(sha).exists(), "pruned: {sha}");
        }
        assert_eq!(report.removed.len(), 2);
        assert_eq!(report.bytes_freed(), 128);
        assert_eq!(report.kept, 3);
        // No staging root, no prune.
        assert!(
            f.service
                .prune_staging_after_success(&store, &store.latest_agent_deploy().unwrap().unwrap())
                .is_none()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn build_info_line_and_parser_agree() {
        let line = build_info_line();
        let (sha, schema) = parse_build_info(&line).unwrap();
        assert_eq!(sha, BUILD_SHA);
        assert_eq!(schema, i64::from(crate::store::LATEST_SCHEMA_VERSION));
        assert!(parse_build_info("only-one-field").is_none());
        assert!(parse_build_info("a 1 extra").is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_supervisor_on_another_path_refuses_the_deploy_naming_both_paths() {
        let f = fixture(true);
        let elsewhere = f._dir.path().join("shared-target/release/rsid");
        std::fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
        std::fs::write(&elsewhere, OLD).unwrap();
        let running = elsewhere.clone();
        let service = f
            .service
            .with_supervisor_binary(Box::new(move || Some(running.clone())));
        let message = service.check_target().unwrap_err().to_string();
        assert!(message.contains(DEPLOY_TARGET_MISMATCH), "{message}");
        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap().display().to_string();
        assert!(message.contains(&canonical(&elsewhere)), "{message}");
        assert!(
            message.contains(&canonical(&f.install.join("rsid"))),
            "{message}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_supervisor_on_the_installed_path_passes_directly_or_through_a_link() {
        let f = fixture(true);
        let installed = f.install.join("rsid");
        let link_dir = f._dir.path().join("bin");
        std::fs::create_dir_all(&link_dir).unwrap();
        let link = link_dir.join("rsid");
        std::os::unix::fs::symlink(&installed, &link).unwrap();
        for running in [installed.clone(), link] {
            let service = DeployService::new(
                f.install.clone(),
                vec![f._dir.path().to_path_buf()],
                Box::new(|| true),
                Arc::new(|_: &Path| Ok((SHA.to_string(), 999))),
            )
            .with_supervisor_binary(Box::new(move || Some(running.clone())));
            service.check_target().unwrap();
        }
        // No readable supervisor argv: nothing to compare, nothing refused.
        f.service.check_target().unwrap();
    }
}
