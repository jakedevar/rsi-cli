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
use crate::store::agent_deploys::{DeployRow, deploy_fingerprint};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rsi_common::agent_deploy::{
    DEPLOY_BINARIES, DEPLOY_BINARY_MISSING, DEPLOY_DIR_NOT_ALLOWED, DEPLOY_MAX_RESTARTS_PER_HOUR,
    DEPLOY_NEEDS_SUPERVISOR, DEPLOY_RESTART_BUDGET, DEPLOY_SCHEMA_DOWNGRADE, DEPLOY_SHA_MISMATCH,
    DEPLOY_STAGE_FAILED, DEPLOY_STAGED_CHANGED, DeployBinaryV1, DeployState,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use uuid::Uuid;

const TICK: Duration = Duration::from_secs(5);
/// Consecutive quiet polls required before the swap.
pub const QUIET_POLLS_REQUIRED: u32 = 2;
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

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
    restart: Mutex<Option<Trigger>>,
}

/// What staging needs; cloneable so blocking file work leaves the runtime.
#[derive(Clone)]
pub struct StagePlan {
    install_dir: PathBuf,
    allowed_roots: Vec<PathBuf>,
    probe: BuildProbe,
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
                allowed_roots,
                probe,
            },
            supervised,
            restart: Mutex::new(None),
        }
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
    }

    /// Initialise the process-wide service with the configured sandbox base.
    pub fn init_global(sandbox_base: PathBuf) -> &'static Self {
        GLOBAL.get_or_init(|| Self::production(sandbox_base))
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
    /// `deploy_restart_budget` or a persistence error.
    pub(crate) fn check_budget(&self, store: &Store, now: DateTime<Utc>) -> Result<()> {
        let used = store.count_agent_deploy_restarts_since(now - ChronoDuration::hours(1))?;
        if used >= DEPLOY_MAX_RESTARTS_PER_HOUR {
            return Err(DaemonError::PolicyDenied(DEPLOY_RESTART_BUDGET.into()));
        }
        Ok(())
    }

    /// The cloneable staging inputs.
    #[must_use]
    pub fn stage_plan(&self) -> StagePlan {
        self.plan.clone()
    }

    /// Insert-side helper for the verb: build the row inputs.
    #[must_use]
    pub(crate) fn fingerprint(sha: &str, dir: &str, wait: u32) -> String {
        deploy_fingerprint(sha, dir, wait)
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
        let invalid = |code: &str| DaemonError::InvalidParam(code.into());
        let dir =
            std::fs::canonicalize(binaries_dir).map_err(|_| invalid(DEPLOY_DIR_NOT_ALLOWED))?;
        let allowed = self
            .allowed_roots
            .iter()
            .any(|root| std::fs::canonicalize(root).is_ok_and(|root| dir.starts_with(root)));
        if !allowed {
            return Err(invalid(DEPLOY_DIR_NOT_ALLOWED));
        }
        let mut manifest = Vec::new();
        let outcome = self.stage_all(id, &dir, &mut manifest, expected_sha, live_schema);
        if outcome.is_err() {
            remove_staged(&manifest, id);
        }
        outcome.map(|()| manifest)
    }

    fn stage_all(
        &self,
        id: Uuid,
        dir: &Path,
        manifest: &mut Vec<DeployBinaryV1>,
        expected_sha: &str,
        live_schema: i64,
    ) -> Result<()> {
        let invalid = |code: &str| DaemonError::InvalidParam(code.into());
        for name in DEPLOY_BINARIES {
            let source = dir.join(name);
            let is_file =
                std::fs::symlink_metadata(&source).is_ok_and(|meta| meta.file_type().is_file());
            if !is_file {
                if name == "rsid" {
                    return Err(invalid(DEPLOY_BINARY_MISSING));
                }
                continue;
            }
            let installed = self.install_dir.join(name);
            let dest = std::fs::canonicalize(&installed).unwrap_or(installed);
            let parent = dest.parent().ok_or_else(|| invalid(DEPLOY_STAGE_FAILED))?;
            let staged = staged_path(&dest, id);
            let source_hash = binary_sha256(&source).ok_or_else(|| invalid(DEPLOY_STAGE_FAILED))?;
            std::fs::create_dir_all(parent).map_err(|_| invalid(DEPLOY_STAGE_FAILED))?;
            std::fs::copy(&source, &staged).map_err(|_| invalid(DEPLOY_STAGE_FAILED))?;
            manifest.push(DeployBinaryV1 {
                name: name.to_string(),
                dest: dest.to_string_lossy().into_owned(),
                sha256: source_hash.clone(),
            });
            set_executable(&staged).map_err(|_| invalid(DEPLOY_STAGE_FAILED))?;
            if binary_sha256(&staged).as_deref() != Some(source_hash.as_str()) {
                return Err(invalid(DEPLOY_STAGE_FAILED));
            }
        }
        let rsid = manifest
            .iter()
            .find(|entry| entry.name == "rsid")
            .ok_or_else(|| invalid(DEPLOY_BINARY_MISSING))?;
        let (sha, schema) = (self.probe)(&staged_path(Path::new(&rsid.dest), id))
            .map_err(|_| invalid(DEPLOY_STAGE_FAILED))?;
        if sha != expected_sha {
            return Err(invalid(DEPLOY_SHA_MISMATCH));
        }
        if schema < live_schema {
            return Err(invalid(DEPLOY_SCHEMA_DOWNGRADE));
        }
        Ok(())
    }
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

fn write_marker(manifest: &[DeployBinaryV1]) -> std::io::Result<()> {
    let path = marker_path(manifest)
        .ok_or_else(|| std::io::Error::other("deploy manifest has no rsid entry"))?;
    std::fs::write(path, b"deploy in flight\n")
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

/// Rename staged copies over the install paths, keeping `<name>.prev`. Each
/// staged copy is re-hashed immediately before its rename, so a copy changed
/// since staging is never installed. Undoes any partial swap on failure.
fn swap_in(manifest: &[DeployBinaryV1], id: Uuid) -> std::result::Result<(), SwapError> {
    let mut done: Vec<&DeployBinaryV1> = Vec::new();
    for entry in manifest {
        let dest = Path::new(&entry.dest);
        let staged = staged_path(dest, id);
        if binary_sha256(&staged).as_deref() != Some(entry.sha256.as_str()) {
            rollback(&done);
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
            rollback(&done);
            return Err(SwapError::Io(error));
        }
        done.push(entry);
    }
    Ok(())
}

/// Restore `<name>.prev` over each install path that has one.
fn rollback(manifest: &[&DeployBinaryV1]) {
    for entry in manifest {
        let dest = Path::new(&entry.dest);
        let prev = prev_path(dest);
        if prev.exists() {
            let _ = std::fs::rename(&prev, dest);
        }
    }
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

/// One pass of the deploy runner over the single live deploy.
///
/// # Errors
/// A persistence error; the caller logs and retries next tick.
pub async fn poll_once(
    store: &tokio::sync::Mutex<Store>,
    service: &DeployService,
    now: DateTime<Utc>,
    gate: &mut GateState,
    drain: &DeployDrain,
    drain_enabled: bool,
) -> Result<PollOutcome> {
    let outcome = poll_step(store, service, now, gate, drain, drain_enabled).await;
    // Re-derive the hold from the durable row after every step: a settle or a
    // timeout releases it, a restart keeps it until the process exits.
    let live = store.lock().await.live_agent_deploy();
    match live {
        Ok(live) => drain.sync(live.as_ref(), drain_enabled, now),
        Err(error) if outcome.is_ok() => return Err(error),
        Err(_) => {}
    }
    outcome
}

async fn poll_step(
    store: &tokio::sync::Mutex<Store>,
    service: &DeployService,
    now: DateTime<Utc>,
    gate: &mut GateState,
    drain: &DeployDrain,
    drain_enabled: bool,
) -> Result<PollOutcome> {
    let store = store.lock().await;
    let Some(row) = store.live_agent_deploy()? else {
        gate.quiet_polls = 0;
        drain.sync(None, drain_enabled, now);
        return Ok(PollOutcome::Idle);
    };
    // Hold new work before judging the quiet point, so nothing new arrives
    // between the read of the blockers and the swap.
    drain.sync(Some(&row), drain_enabled, now);
    if row.state != DeployState::Staged {
        // `restarting`: the drain is in flight; startup verification settles it.
        return Ok(PollOutcome::Idle);
    }
    let blockers = store.deploy_quiet_blockers(row.owner_session_id)?;
    gate.last_blockers.clone_from(&blockers);
    if blockers.is_empty() {
        gate.quiet_polls += 1;
    } else {
        gate.quiet_polls = 0;
    }
    if gate.quiet_polls >= QUIET_POLLS_REQUIRED {
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
        if !store.mark_agent_deploy_restarting(row.id, now)? {
            return Ok(PollOutcome::Idle);
        }
        if let Err(error) = swap_in(&row.manifest, row.id) {
            return settle(&store, &row, DeployState::Failed, &error.to_string(), now);
        }
        if let Err(error) = write_marker(&row.manifest) {
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
        return Ok(PollOutcome::Restarting(row.id));
    }
    if now >= row.deadline_at {
        let reason = format!("quiet point not reached: {}", blockers.join(","));
        return settle(&store, &row, DeployState::TimedOut, &reason, now);
    }
    Ok(PollOutcome::Waiting(blockers))
}

/// Startup verification of a `restarting` deploy against the running build.
///
/// # Errors
/// A persistence error.
pub async fn verify_after_restart(
    store: &tokio::sync::Mutex<Store>,
    service: &DeployService,
    build_sha: &str,
    running_sha256: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<PollOutcome>> {
    let store = store.lock().await;
    let Some(row) = store.live_agent_deploy()? else {
        return Ok(None);
    };
    if row.state != DeployState::Restarting {
        return Ok(None);
    }
    let expected = row
        .manifest
        .iter()
        .find(|entry| entry.name == "rsid")
        .map(|entry| entry.sha256.as_str());
    let verified = build_sha == row.sha && expected.is_some() && expected == running_sha256;
    if verified {
        store.settle_agent_deploy(row.id, DeployState::Succeeded, None, now)?;
        remove_marker(&row.manifest);
        remove_prev_files(&row.manifest);
        return Ok(Some(PollOutcome::Settled(row.id, DeployState::Succeeded)));
    }
    // The new build did not come up as deployed: put the old binaries back and,
    // if a wrong-but-new build is what runs, restart once into them.
    remove_marker(&row.manifest);
    rollback(&row.manifest.iter().collect::<Vec<_>>());
    let new_bits_running = running_sha256.is_some() && expected == running_sha256;
    let reason = format!(
        "verification failed: running build {build_sha}, expected {}; previous binaries restored",
        row.sha
    );
    store.settle_agent_deploy(row.id, DeployState::Failed, Some(&reason), now)?;
    if new_bits_running {
        service.fire_restart();
    }
    Ok(Some(PollOutcome::Settled(row.id, DeployState::Failed)))
}

/// The daemon-owned runner: verify once at startup, then poll.
pub async fn run_deploy_loop(
    store: Arc<tokio::sync::Mutex<Store>>,
    service: &'static DeployService,
    drain: Arc<DeployDrain>,
    config: Arc<crate::config::RuntimeConfig>,
) {
    let running_sha =
        tokio::task::spawn_blocking(|| DaemonInfoService::global().running_binary_sha256())
            .await
            .ok()
            .flatten();
    match verify_after_restart(
        &store,
        service,
        BUILD_SHA,
        running_sha.as_deref(),
        Utc::now(),
    )
    .await
    {
        Ok(Some(outcome)) => tracing::info!(?outcome, "deploy verified after restart"),
        Ok(None) => {}
        Err(error) => tracing::warn!(%error, "deploy verification deferred"),
    }
    let mut gate = GateState::default();
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let drain_enabled = config.deploy_drain_enabled.load(Ordering::Relaxed);
        match poll_once(
            &store,
            service,
            Utc::now(),
            &mut gate,
            &drain,
            drain_enabled,
        )
        .await
        {
            Ok(PollOutcome::Idle | PollOutcome::Waiting(_)) => {}
            Ok(outcome) => tracing::info!(?outcome, "deploy poll"),
            Err(error) => tracing::warn!(%error, "deploy poll deferred"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::rolling_queue::NewQueueEntry;
    use rsi_common::rolling_queue::RollingQueueBinding;
    use std::sync::atomic::AtomicUsize;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const OLD: &[u8] = b"old-rsid";
    const NEW: &[u8] = b"new-rsid";

    struct Fixture {
        _dir: tempfile::TempDir,
        source: PathBuf,
        install: PathBuf,
        service: DeployService,
        restarts: Arc<AtomicUsize>,
        store: tokio::sync::Mutex<Store>,
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
            store: tokio::sync::Mutex::new(store),
            owner,
            drain: DeployDrain::new(),
        }
    }

    /// Stage the fixture's build and record a `staged` deploy.
    async fn stage(f: &Fixture, key: &str, wait: u32) -> DeployRow {
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
                    fingerprint: deploy_fingerprint(SHA, "x", wait),
                    manifest: &manifest,
                    max_wait_secs: wait,
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
                     VALUES (?1,?2,'test','{}','/tmp','u','/tmp/l','/tmp/s','running',?3,1)",
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
        assert_eq!(f.restarts.load(Ordering::SeqCst), 1);
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
            // Outside the hour the budget is clear again.
            f.service
                .check_budget(&store, now + ChronoDuration::hours(2))
                .unwrap();
        }
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
}
