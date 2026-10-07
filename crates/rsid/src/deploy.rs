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
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rsi_common::agent_deploy::{
    DEPLOY_BINARIES, DEPLOY_BINARY_MISSING, DEPLOY_DIR_NOT_ALLOWED, DEPLOY_MAX_RESTARTS_PER_HOUR,
    DEPLOY_NEEDS_SUPERVISOR, DEPLOY_RESTART_BUDGET, DEPLOY_SCHEMA_DOWNGRADE, DEPLOY_SHA_MISMATCH,
    DEPLOY_STAGE_FAILED, DEPLOY_STAGED_CHANGED, DEPLOY_TARGET_MISMATCH, DeployBinaryV1,
    DeployState, ObjectIdentityV1,
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
    /// The rsid binary the supervisor relaunches (#1164); `None` skips the
    /// target check (tests, or a supervisor argv that cannot be read).
    supervisor_binary: Box<dyn Fn() -> Option<PathBuf> + Send + Sync>,
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
            supervisor_binary: Box::new(|| None),
            restart: Mutex::new(None),
        }
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
        match self.stage_all(id, &dir, &mut manifest, expected_sha, live_schema) {
            Ok(sha) => Ok((manifest, sha)),
            Err(error) => {
                remove_staged(&manifest, id);
                Err(error)
            }
        }
    }

    fn stage_all(
        &self,
        id: Uuid,
        dir: &Path,
        manifest: &mut Vec<DeployBinaryV1>,
        expected_sha: Option<&str>,
        live_schema: i64,
    ) -> Result<String> {
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
            let prior_present = std::fs::symlink_metadata(&dest).is_ok();
            std::fs::copy(&source, &staged).map_err(|_| invalid(DEPLOY_STAGE_FAILED))?;
            manifest.push(DeployBinaryV1 {
                name: name.to_string(),
                dest: dest.to_string_lossy().into_owned(),
                sha256: source_hash.clone(),
                prior_present,
                staged_identity: None,
            });
            set_executable(&staged).map_err(|_| invalid(DEPLOY_STAGE_FAILED))?;
            if binary_sha256(&staged).as_deref() != Some(source_hash.as_str()) {
                return Err(invalid(DEPLOY_STAGE_FAILED));
            }
            if name == "rsid-supervisor.sh"
                && !std::process::Command::new("bash")
                    .arg("-n")
                    .arg(&staged)
                    .status()
                    .is_ok_and(|status| status.success())
            {
                return Err(invalid(DEPLOY_STAGE_FAILED));
            }
            let identity = object_identity(&staged).ok_or_else(|| invalid(DEPLOY_STAGE_FAILED))?;
            if let Some(entry) = manifest.last_mut() {
                entry.staged_identity = Some(identity);
            }
        }
        let rsid = manifest
            .iter()
            .find(|entry| entry.name == "rsid")
            .ok_or_else(|| invalid(DEPLOY_BINARY_MISSING))?;
        let (sha, schema) = (self.probe)(&staged_path(Path::new(&rsid.dest), id))
            .map_err(|_| invalid(DEPLOY_STAGE_FAILED))?;
        if expected_sha.is_some_and(|expected| sha != expected) {
            return Err(invalid(DEPLOY_SHA_MISMATCH));
        }
        if schema < live_schema {
            return Err(invalid(DEPLOY_SCHEMA_DOWNGRADE));
        }
        Ok(sha)
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
    let synced = {
        let store = store.lock().await;
        store.live_agent_deploy().and_then(|live| {
            let end = match &live {
                Some(row) => hold_end(&store, row, drain, gate.quiet_polls > 0)?,
                None => None,
            };
            drain.sync_until(live.as_ref(), end, drain_enabled, now);
            Ok(())
        })
    };
    match synced {
        Ok(()) => {}
        Err(error) if outcome.is_ok() => return Err(error),
        Err(_) => {}
    }
    outcome
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
        drain.set_blockers(&[]);
        drain.sync(None, drain_enabled, now);
        return Ok(PollOutcome::Idle);
    };
    // Hold new work before judging the quiet point, so nothing new arrives
    // between the read of the blockers and the swap (#1320: within the
    // operator's hold window, or from the first quiet poll on).
    let end = hold_end(&store, &row, drain, gate.quiet_polls > 0)?;
    drain.sync_until(Some(&row), end, drain_enabled, now);
    if row.state != DeployState::Staged {
        // `restarting`: the drain is in flight; startup verification settles it.
        drain.set_blockers(&[]);
        return Ok(PollOutcome::Idle);
    }
    // #1461: past the drain hold, an `interrupt_workers` deploy no longer waits
    // for workers mid-turn; a landing and a local job still block.
    let mut interrupting = false;
    let blockers = match row.owner_session_id {
        Some(owner) => {
            interrupting = store.agent_deploy_interrupt(row.id)?.requested
                && drain_hold_over(&store, &row, drain, drain_enabled, now)?;
            store.deploy_quiet_blockers_with(owner, !interrupting)?
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
        if !store.mark_agent_deploy_restarting(row.id, now)? {
            return Ok(PollOutcome::Idle);
        }
        if let Err(error) = swap_in(&row.manifest, row.id) {
            return settle(&store, &row, DeployState::Failed, &error.to_string(), now);
        }
        if let Err(error) = write_marker(&row.manifest, store.database_path()) {
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
        if interrupting {
            note_deploy_interrupts(&store, &row, &interrupted);
        }
        return Ok(PollOutcome::Restarting(row.id));
    }
    if now >= row.deadline_at {
        note_deploy_timeout(&store, &row, &blockers);
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
    let backup = prepare_database_rollback(&store, service, &row)?;
    // The new build did not come up as deployed: put the old binaries back and,
    // if a wrong-but-new build is what runs, restart once into them.
    remove_marker(&row.manifest);
    rollback(&row.manifest.iter().collect::<Vec<_>>());
    let new_bits_running = running_sha256.is_some() && expected == running_sha256;
    let mut reason = format!(
        "verification failed: running build {build_sha}, expected {}; previous binaries restored",
        row.sha
    );
    if let Some(path) = &backup {
        reason.push_str(&format!(
            "; offline database restore queued from {}",
            path.display()
        ));
    }
    store.settle_agent_deploy(row.id, DeployState::Failed, Some(&reason), now)?;
    if new_bits_running || backup.is_some() {
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
        drain.set_hold_cap_secs(config.deploy_drain_hold_secs.load(Ordering::Relaxed));
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
    use crate::store::agent_deploys::deploy_fingerprint;
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
                "rsid-supervisor.sh"
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
        let release_by = DateTime::parse_from_rfc3339(status.release_by.as_deref().unwrap())
            .unwrap()
            .with_timezone(&Utc);
        assert!(
            release_by <= start + ChronoDuration::seconds(61),
            "released by the window, not the {}s deadline: {release_by}",
            3600
        );

        // Past the window: still waiting on the worker, nothing held.
        let past = start + ChronoDuration::seconds(120);
        let unheld = poll_once(&f.store, &f.service, past, &mut gate, &f.drain, true)
            .await
            .unwrap();
        assert_eq!(unheld, PollOutcome::Waiting(vec!["worker_mid_turn"]));
        assert!(!f.drain.holds(other, true), "launches run past the window");
        let status = f.drain.status();
        assert!(!status.draining);
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
        assert_eq!(
            poll_once(&f.store, &f.service, now, &mut gate, &f.drain, true)
                .await
                .unwrap(),
            PollOutcome::Waiting(vec![]),
            "running cloud jobs are not blockers"
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
        let row = stage_with(&f, "interrupt", 3600, true).await;
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
        let row = stage(&f, "plain", 3600).await;
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

    /// #1461: only workers stop blocking. A landing in progress and a running
    /// local job still hold the deploy back past the hold, so a landing or a
    /// test/build run is never cut off.
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
