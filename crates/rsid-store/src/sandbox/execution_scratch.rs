//! Authenticated, disk-backed execution scratch beneath a sandbox custody root.
//!
//! This is deliberately a descriptor, not a caller-selectable path.  The
//! provider launch funnel receives it only after a custody ContextRead permit
//! has authenticated the sandbox root.

use crate::config::RuntimeConfig;
use crate::error::{DaemonError, Result};
use crate::sandbox::custody::CustodyEffectPermit;
use nix::fcntl::{OFlag, open, openat};
use nix::sys::stat::{Mode, SFlag, fstat, mkdirat};
use std::ffi::CStr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use tokio::process::Command;

const TARGET_NAME: &CStr = c"target";
const TEMP_NAME: &CStr = c".rsi-tmp";
// `mode_t` is `u32` on Linux but `u16` on macOS/BSD; type this from the
// platform's actual `libc::mode_t` alias instead of hardcoding a width so it
// matches both `fchmod`'s parameter type and `stat.st_mode`'s field type on
// every target.
const PRIVATE_DIRECTORY_MODE: nix::libc::mode_t = 0o700;
const TMPFS_MAGIC: i64 = 0x0102_1994;
/// The build settings a session's shell received, read back for its job
/// units (`agent_jobs`). Kept at the authenticated sandbox root, outside
/// disposable Cargo output. Only names in [`RECORDED_BUILD_ENV`] are honoured.
pub const BUILD_ENV_RECORD: &str = ".rsi-build-env.json";
/// Every variable [`SandboxExecutionScratch::stamp_build_env`] may set.
pub const RECORDED_BUILD_ENV: &[&str] = &[
    "CARGO_BUILD_JOBS",
    "CARGO_PROFILE_DEV_DEBUG",
    "CARGO_INCREMENTAL",
    "RUSTC_WRAPPER",
    "RSI_BUILD_SLOTS",
    "RSI_BUILD_SLOT_ROOT",
    "RSI_BUILD_SCCACHE",
    "SCCACHE_CACHE_SIZE",
    "SCCACHE_DIR",
    "SCCACHE_SERVER_UDS",
    "SCCACHE_CLIENT_SIDE",
];
const RAMFS_MAGIC: i64 = 0x8584_58f6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxExecutionScratch {
    root: PathBuf,
    target: PathBuf,
    temp: PathBuf,
    root_identity: FileIdentity,
    target_identity: FileIdentity,
    temp_identity: FileIdentity,
    build_env: Option<AgentBuildEnvironment>,
    /// The custody-recorded Git common directory of the sandbox's repository
    /// (#2746); the trusted anchor for the Git paths a provider may write.
    git_common_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentBuildEnvironment {
    jobs: u32,
    line_tables_only: bool,
    worker: bool,
    wrapper: Option<PathBuf>,
    slots: u32,
    sccache: Option<PathBuf>,
    cache_gib: u32,
    /// Operator setting `target_pool_keep`: warm pools `target-pool` keeps.
    pool_keep: u64,
}

impl AgentBuildEnvironment {
    fn from_runtime(runtime: &RuntimeConfig, worker: bool) -> Self {
        let cache_enabled = runtime.agent_build_sccache_enabled.load(Ordering::Relaxed);
        Self {
            jobs: runtime.agent_build_jobs.load(Ordering::Relaxed),
            line_tables_only: runtime.agent_build_line_tables_only.load(Ordering::Relaxed),
            worker,
            wrapper: installed_build_wrapper(),
            slots: runtime.agent_build_slots.load(Ordering::Relaxed),
            sccache: cache_enabled
                .then(|| executable_on_path("sccache"))
                .flatten(),
            cache_gib: runtime
                .agent_build_sccache_cache_gib
                .load(Ordering::Relaxed),
            pool_keep: runtime.target_pool_keep.load(Ordering::Relaxed),
        }
    }

    /// The daemon-owned build settings: `Some(value)` is set, `None` is removed from
    /// an inherited environment.
    fn env_values(&self) -> Vec<(&'static str, Option<String>)> {
        let build = self;
        let path = |path: &Path| Some(path.display().to_string());
        let home = dirs::home_dir();
        let mut values = vec![
            ("CARGO_BUILD_JOBS", Some(build.jobs.to_string())),
            (
                "CARGO_PROFILE_DEV_DEBUG",
                build
                    .line_tables_only
                    .then(|| "line-tables-only".to_string()),
            ),
            (
                "CARGO_INCREMENTAL",
                (build.worker || build.sccache.is_some()).then(|| "0".to_string()),
            ),
        ];
        if let Some(wrapper) = &build.wrapper {
            values.push(("RUSTC_WRAPPER", path(wrapper)));
            values.push(("RSI_BUILD_SLOTS", Some(build.slots.to_string())));
            if let Some(home) = &home {
                values.push(("RSI_BUILD_SLOT_ROOT", path(&home.join(".rsi/build-slots"))));
            }
            values.push(("RSI_BUILD_SCCACHE", build.sccache.as_deref().and_then(path)));
        } else {
            values.push(("RUSTC_WRAPPER", build.sccache.as_deref().and_then(path)));
        }
        if build.sccache.is_some() {
            values.push(("SCCACHE_CACHE_SIZE", Some(format!("{}G", build.cache_gib))));
            if let Some(home) = &home {
                // #2745: the server socket lives inside the cache directory, so
                // the one directory a Codex `workspace-write` launch grants is
                // enough for the server to create both its socket and its cache.
                // A socket beside the directory under `~/.rsi` would stay on
                // the read-only side of the sandbox.
                let cache_dir = home.join(AGENT_SCCACHE_DIR);
                values.push(("SCCACHE_DIR", path(&cache_dir)));
                values.push((
                    "SCCACHE_SERVER_UDS",
                    path(&cache_dir.join(SCCACHE_SOCKET_NAME)),
                ));
            }
            // Keep compilation in the wrapper-held client process; the cache
            // daemon only serves artifacts and never consumes a build slot.
            values.push(("SCCACHE_CLIENT_SIDE", Some("1".to_string())));
        }
        values
    }
}

/// The shared worker compiler-cache directory, relative to the home directory.
const AGENT_SCCACHE_DIR: &str = ".rsi/agent-sccache";
/// The cache server's socket inside [`AGENT_SCCACHE_DIR`] (#2745).
const SCCACHE_SOCKET_NAME: &str = "server.sock";

fn executable_on_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// How long a launch waits for `target-pool seed` (a real seed takes ~10 s).
const SEED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[cfg(any(test, feature = "test-seam"))]
fn target_pool_scripts_for_test()
-> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, PathBuf>> {
    static SCRIPTS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, PathBuf>>,
    > = std::sync::OnceLock::new();
    SCRIPTS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Substitute one root's next seed command without changing process-global
/// HOME/PATH or invoking the installed pool builder in a launch test.
#[cfg(any(test, feature = "test-seam"))]
pub fn install_target_pool_script_for_test(root: PathBuf, script: PathBuf) {
    target_pool_scripts_for_test()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(root, script);
}

/// A target the seeder may fill: absent, or holding only the daemon's own
/// scratch and records.
fn target_is_unseeded(target: &Path) -> bool {
    match std::fs::read_dir(target) {
        Ok(entries) => entries.filter_map(|entry| entry.ok()).all(|entry| {
            matches!(
                entry.file_name().to_str(),
                Some(".rsi-tmp" | BUILD_ENV_RECORD | "CACHEDIR.TAG")
            )
        }),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

/// `~/.rsi/bin/target-pool`, installed by `make release-install`.
fn installed_target_pool() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let candidate = dirs::home_dir()?.join(".rsi/bin/target-pool");
    candidate
        .metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .then_some(candidate)
}

/// The warm pools' disk and age, as `target-pool summary` reports them, for the
/// operator's Sandbox Storage settings (#1882). Blocking; None when the script
/// is not installed or fails.
pub fn target_pool_summary() -> Option<serde_json::Value> {
    let script = installed_target_pool()?;
    let output = std::process::Command::new(script)
        .arg("summary")
        .stdin(std::process::Stdio::null())
        .env_remove("RSI_SESSION_TOKEN")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| serde_json::from_slice(&output.stdout).ok())
        .flatten()
}

/// One pool garbage-collection pass: keep the newest `keep` pools, or only the
/// newest one under disk `pressure`. A pool a seed is reading is skipped by the
/// script, never waited on. Blocking; returns the script's JSON report
/// (`removed`, `skipped_in_use`) or None when it did not run.
pub fn run_target_pool_gc(keep: u64, pressure: bool) -> Option<serde_json::Value> {
    let script = installed_target_pool()?;
    let mut command = std::process::Command::new(script);
    command
        .arg("gc")
        .arg("--keep")
        .arg(keep.to_string())
        .arg("--json")
        .stdin(std::process::Stdio::null())
        .env_remove("RSI_SESSION_TOKEN");
    if pressure {
        command.arg("--pressure");
    }
    let output = command.output().ok()?;
    output
        .status
        .success()
        .then(|| serde_json::from_slice(&output.stdout).ok())
        .flatten()
}

/// One pool refresh pass (#2707): `target-pool refresh` starts a detached build at
/// the `origin/rolling` tip when no pool is within half the seed distance of it. The
/// script checks the free-space floor itself and the build runs in the build slice,
/// so this never waits for a build. Blocking; returns the script's JSON report
/// (`refresh`: one entry per pool directory) or None when it did not run.
pub fn run_target_pool_refresh(runtime: &RuntimeConfig, keep: u64) -> Option<serde_json::Value> {
    let script = installed_target_pool()?;
    let output = target_pool_refresh_command(&script, runtime, keep)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| serde_json::from_slice(&output.stdout).ok())
        .flatten()
}

/// The `target-pool refresh` command with the build settings a worker's seed gets.
/// The pool key hashes `CARGO_INCREMENTAL` and the `CARGO_PROFILE_*` settings, and
/// the build the refresh starts inherits what it is given, so a refresh in the
/// daemon's own environment would build a pool no worker seed matches (#2716).
fn target_pool_refresh_command(
    script: &Path,
    runtime: &RuntimeConfig,
    keep: u64,
) -> std::process::Command {
    let mut command = std::process::Command::new(script);
    command
        .arg("refresh")
        .env("RSI_TARGET_POOL_KEEP", keep.to_string())
        // A pool build must leave the workers the floor their cargo-slot admission holds
        // (the operator's governor_min_free_disk_gb), whatever its own floor is (#2773).
        .env(
            "RSI_TARGET_POOL_WORKER_FLOOR_GIB",
            runtime.governor_policy().min_free_disk_gb.to_string(),
        )
        .stdin(std::process::Stdio::null())
        .env_remove("RSI_SESSION_TOKEN");
    for (name, value) in AgentBuildEnvironment::from_runtime(runtime, true).env_values() {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
    command
}

fn installed_build_wrapper() -> Option<PathBuf> {
    let sibling = std::env::current_exe().ok().and_then(|executable| {
        executable
            .parent()
            .map(|parent| parent.join("rsi-build-rustc"))
    });
    sibling
        .filter(|wrapper| wrapper.is_file())
        .or_else(|| executable_on_path("rsi-build-rustc"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl SandboxExecutionScratch {
    /// Prepare the fixed scratch layout beneath an already authenticated
    /// custody root.  All child creation and inspection is descriptor-relative
    /// and no caller-controlled path enters this API.
    fn prepare(root: &Path) -> Result<Self> {
        let root_fd = open_directory(root, None)?;
        let root_identity = inspect_directory(root_fd.as_raw_fd(), "sandbox root")?;
        reject_memory_filesystem(root_fd.as_raw_fd(), "sandbox root")?;

        ensure_directory(root_fd.as_raw_fd(), TARGET_NAME, "target")?;
        let target_fd = open_directory(Path::new("target"), Some(root_fd.as_raw_fd()))?;
        let target_identity = inspect_directory(target_fd.as_raw_fd(), "target")?;
        require_private_mode(target_fd.as_raw_fd(), "target")?;
        reject_memory_filesystem(target_fd.as_raw_fd(), "target")?;
        require_same_device(root_identity, target_identity, "target")?;

        ensure_directory(target_fd.as_raw_fd(), TEMP_NAME, ".rsi-tmp")?;
        let temp_fd = open_directory(Path::new(".rsi-tmp"), Some(target_fd.as_raw_fd()))?;
        let temp_identity = inspect_directory(temp_fd.as_raw_fd(), ".rsi-tmp")?;
        reject_memory_filesystem(temp_fd.as_raw_fd(), ".rsi-tmp")?;
        require_private_mode(temp_fd.as_raw_fd(), ".rsi-tmp")?;
        require_same_device(root_identity, temp_identity, ".rsi-tmp")?;
        Ok(Self {
            root: root.to_path_buf(),
            target: root.join("target"),
            temp: root.join("target").join(".rsi-tmp"),
            root_identity,
            target_identity,
            temp_identity,
            build_env: None,
            git_common_dir: None,
        })
    }

    /// Derive scratch only from a typed, authenticated ContextRead permit.
    /// Ordinary custody has no daemon-selected target and therefore receives
    /// no execution scratch or TMPDIR stamp.
    pub fn from_context_permit(permit: &CustodyEffectPermit) -> Result<Option<Self>> {
        if !permit.is_context_read() {
            return Err(rejected(
                "execution scratch requires a ContextRead effect permit",
            ));
        }
        let Some(target) = permit.cargo_target_dir() else {
            return Ok(None);
        };
        let expected = permit.effective_cwd().join("target");
        if target != expected {
            return Err(rejected(
                "ContextRead target does not match authenticated custody root",
            ));
        }
        let mut scratch = Self::prepare(permit.effective_cwd())?;
        scratch.git_common_dir = permit.git_common_dir().map(Path::to_path_buf);
        Ok(Some(scratch))
    }

    /// The custody-recorded Git common directory, when the sandbox is a Git
    /// worktree.
    pub fn git_common_dir(&self) -> Option<&Path> {
        self.git_common_dir.as_deref()
    }

    /// Record a Git common directory the way custody does (tests only).
    #[cfg(any(test, feature = "test-seam"))]
    pub fn with_git_common_dir_for_test(mut self, common: PathBuf) -> Self {
        self.git_common_dir = Some(common);
        self
    }

    #[cfg(any(test, feature = "test-seam"))]
    pub fn prepare_for_test(root: &Path) -> Result<Self> {
        Self::prepare(root)
    }

    pub fn target(&self) -> &Path {
        &self.target
    }
    pub fn temp(&self) -> &Path {
        &self.temp
    }

    /// Attach daemon-owned build defaults only after the custody permit has
    /// established this sandbox's scratch directory.
    pub fn with_build_env(mut self, runtime: &RuntimeConfig, worker: bool) -> Self {
        self.build_env = Some(AgentBuildEnvironment::from_runtime(runtime, worker));
        self
    }

    /// The daemon-owned build settings for this sandbox: `Some(value)` is set,
    /// `None` is removed from an inherited environment.
    fn build_env_values(&self) -> Vec<(&'static str, Option<String>)> {
        self.build_env
            .as_ref()
            .map(AgentBuildEnvironment::env_values)
            .unwrap_or_default()
    }

    /// Stamp the build settings onto a provider command and record them in
    /// `<sandbox>/.rsi-build-env.json`, so a job unit the session submits builds
    /// exactly as its shell does, even after `cargo clean` (#1741, #2401).
    pub fn stamp_build_env(&self, cmd: &mut Command) {
        let values = self.build_env_values();
        for (name, value) in &values {
            match value {
                Some(value) => cmd.env(name, value),
                None => cmd.env_remove(name),
            };
        }
        if self.build_env.is_some() {
            if let Err(error) = self.record_build_env(&values) {
                tracing::warn!(
                    target = %self.target.display(),
                    %error,
                    "cannot record the sandbox build environment for job units"
                );
            }
        }
    }

    fn record_build_env(&self, values: &[(&'static str, Option<String>)]) -> std::io::Result<()> {
        use std::io::Write;

        let set: serde_json::Map<String, serde_json::Value> = values
            .iter()
            .filter_map(|(name, value)| {
                value
                    .as_ref()
                    .map(|value| (name.to_string(), serde_json::Value::String(value.clone())))
            })
            .collect();
        let body = serde_json::json!({ "version": 1, "env": set });
        // Create a private, unique file without following an existing symlink.
        // Both paths stay outside target, so cleaning it cannot interrupt a stamp.
        let mut temporary = tempfile::Builder::new()
            .prefix(".rsi-build-env.")
            .tempfile_in(&self.root)?;
        temporary.write_all(&serde_json::to_vec_pretty(&body)?)?;
        temporary
            .persist(self.root.join(BUILD_ENV_RECORD))
            .map(|_| ())
            .map_err(|error| error.error)
    }

    /// The `target-pool seed` command with exactly the build settings the session
    /// receives, so it picks (and, when none matches, builds) a pool keyed the same way.
    fn target_pool_seed_command(&self, script: &Path, build: &AgentBuildEnvironment) -> Command {
        let mut command = Command::new(script);
        command
            .arg("seed")
            .arg("--sandbox")
            .arg(&self.root)
            .arg("--target")
            .arg(&self.target)
            .arg("--build-missing")
            // The pool build this seed may start garbage-collects with the
            // operator's keep count (#1882).
            .env("RSI_TARGET_POOL_KEEP", build.pool_keep.to_string())
            .env("CARGO_TARGET_DIR", &self.target)
            .env("TMPDIR", &self.temp)
            .env_remove("RSI_SESSION_TOKEN")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        for (name, value) in self.build_env_values() {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
        command
    }

    /// Seed this worker's empty target from the warm target pool (#1741):
    /// run the installed `target-pool seed` with exactly the build settings
    /// the session receives, so it picks a pool built the same way, and let it
    /// start a pool build for this commit when none matches. Best effort: a
    /// missing script, a timeout or a failure leaves an empty target and the
    /// session builds cold as before. Returns the seeder's one-line summary.
    pub async fn seed_target_from_pool(&self) -> Option<String> {
        let build = self.build_env.as_ref()?;
        if !build.worker
            || !self.root.join("Cargo.toml").is_file()
            || !target_is_unseeded(&self.target)
        {
            return None;
        }
        #[cfg(any(test, feature = "test-seam"))]
        let script = target_pool_scripts_for_test()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.root)
            .or_else(installed_target_pool)?;
        #[cfg(not(any(test, feature = "test-seam")))]
        let script = installed_target_pool()?;
        let mut command = self.target_pool_seed_command(&script, build);
        let outcome = tokio::time::timeout(SEED_TIMEOUT, command.output()).await;
        let summary = match outcome {
            Ok(Ok(output)) if output.status.success() => {
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            }
            Ok(Ok(output)) => format!(
                "target-pool seed exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Ok(Err(error)) => format!("target-pool seed could not start: {error}"),
            Err(_) => format!(
                "target-pool seed timed out after {}s",
                SEED_TIMEOUT.as_secs()
            ),
        };
        tracing::info!(root = %self.root.display(), %summary, "warm target pool seed");
        Some(summary)
    }

    /// Reopen all fixed names below the pinned root and compare their exact
    /// identities immediately before command construction.  Replacement,
    /// symlinks, device drift, and tmpfs/ramfs all fail closed.
    pub fn revalidate(&self) -> Result<()> {
        let root_fd = open_directory(&self.root, None)?;
        let root_identity = inspect_directory(root_fd.as_raw_fd(), "sandbox root")?;
        reject_memory_filesystem(root_fd.as_raw_fd(), "sandbox root")?;
        let target_fd = open_directory(Path::new("target"), Some(root_fd.as_raw_fd()))?;
        let target_identity = inspect_directory(target_fd.as_raw_fd(), "target")?;
        require_private_mode(target_fd.as_raw_fd(), "target")?;
        reject_memory_filesystem(target_fd.as_raw_fd(), "target")?;
        let temp_fd = open_directory(Path::new(".rsi-tmp"), Some(target_fd.as_raw_fd()))?;
        let temp_identity = inspect_directory(temp_fd.as_raw_fd(), ".rsi-tmp")?;
        require_private_mode(temp_fd.as_raw_fd(), ".rsi-tmp")?;
        reject_memory_filesystem(temp_fd.as_raw_fd(), ".rsi-tmp")?;
        if root_identity != self.root_identity
            || target_identity != self.target_identity
            || temp_identity != self.temp_identity
        {
            return Err(rejected(
                "execution scratch identity drifted after preparation",
            ));
        }
        require_same_device(root_identity, target_identity, "target")?;
        require_same_device(root_identity, temp_identity, ".rsi-tmp")?;
        Ok(())
    }
}

fn rejected(reason: &str) -> DaemonError {
    DaemonError::ExecutionScratchUnavailable(format!(
        "sandbox execution scratch rejected: {reason}"
    ))
}

fn open_directory(path: &Path, parent: Option<RawFd>) -> Result<OwnedFd> {
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW;
    match parent {
        Some(fd) => openat(Some(fd), path, flags, Mode::empty()),
        None => open(path, flags, Mode::empty()),
    }
    .map(|fd| {
        // SAFETY: `open`/`openat` returned a new owned descriptor and this is
        // its sole conversion into an RAII owner.
        unsafe { OwnedFd::from_raw_fd(fd) }
    })
    .map_err(|error| {
        rejected(&format!(
            "cannot open directory {}: {error}",
            path.display()
        ))
    })
}

fn ensure_directory(parent: RawFd, name: &CStr, label: &str) -> Result<()> {
    match mkdirat(Some(parent), name, Mode::from_bits_truncate(0o700)) {
        Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
        Err(error) => return Err(rejected(&format!("cannot create {label}: {error}"))),
    }
    let fd = openat(
        Some(parent),
        name,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| rejected(&format!("cannot reopen {label}: {error}")))?;
    // SAFETY: `openat` returned a new owned descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: `fd` is the live no-follow directory descriptor opened above.
    if unsafe { nix::libc::fchmod(fd.as_raw_fd(), PRIVATE_DIRECTORY_MODE) } == 0 {
        Ok(())
    } else {
        Err(rejected(&format!("cannot set {label} mode")))
    }
}

fn require_private_mode(fd: RawFd, label: &str) -> Result<()> {
    let stat = fstat(fd).map_err(|error| rejected(&format!("cannot stat {label}: {error}")))?;
    if (stat.st_mode & 0o7777) != PRIVATE_DIRECTORY_MODE {
        return Err(rejected(&format!("{label} mode is not 0700")));
    }
    Ok(())
}

fn require_same_device(
    root_identity: FileIdentity,
    child_identity: FileIdentity,
    label: &str,
) -> Result<()> {
    if child_identity.device != root_identity.device {
        return Err(rejected(&format!(
            "{label} crosses the sandbox-root device"
        )));
    }
    Ok(())
}

fn inspect_directory(fd: RawFd, label: &str) -> Result<FileIdentity> {
    let stat = fstat(fd).map_err(|error| rejected(&format!("cannot stat {label}: {error}")))?;
    if SFlag::from_bits_truncate(stat.st_mode) != SFlag::S_IFDIR {
        return Err(rejected(&format!("{label} is not a directory")));
    }
    Ok(FileIdentity {
        // `dev_t` is `u64` on Linux but `i32` on macOS/BSD. `as` is a valid
        // conversion for both source types (unlike `From`/`TryFrom`, which
        // would need a per-platform impl); device ids are always small
        // non-negative numbers in practice, so the reinterpretation is safe.
        device: stat.st_dev as u64,
        inode: stat.st_ino,
    })
}

fn reject_memory_filesystem(fd: RawFd, label: &str) -> Result<()> {
    let mut stat: nix::libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is a live directory descriptor and `stat` is writable.
    if unsafe { nix::libc::fstatfs(fd, &mut stat) } != 0 {
        return Err(rejected(&format!("cannot inspect filesystem for {label}")));
    }
    let filesystem_type = stat.f_type as i64;
    if filesystem_type == TMPFS_MAGIC || filesystem_type == RAMFS_MAGIC {
        return Err(rejected(&format!("{label} is on tmpfs/ramfs")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::custody::EffectKind;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;

    /// Per-process open-descriptor directory (`/proc` is Linux-only).
    #[cfg(target_os = "linux")]
    const OPEN_DESCRIPTOR_DIR: &str = "/proc/self/fd";
    #[cfg(not(target_os = "linux"))]
    const OPEN_DESCRIPTOR_DIR: &str = "/dev/fd";

    /// Permission bits as `mode_t` (`u32` on Linux, `u16` on macOS).
    fn permission_bits(path: &Path) -> nix::libc::mode_t {
        nix::sys::stat::stat(path).unwrap().st_mode & 0o7777
    }

    fn disk_backed_fixture() -> tempfile::TempDir {
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap().join("target"))
            .join("slice8-execution-scratch-fixtures");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix("scratch-")
            .tempdir_in(base)
            .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn sandbox_build_env_distinguishes_worker_and_lead() {
        use crate::config::Config;
        let fixture = disk_backed_fixture();
        let runtime = RuntimeConfig::from_config(&Config::default());
        let env_value = |cmd: &Command, name: &str| {
            cmd.as_std().get_envs().find_map(|(key, value)| {
                (key == name)
                    .then(|| value.map(|v| v.to_string_lossy().into_owned()))
                    .flatten()
            })
        };

        // #1883: sccache is off by default; a worker still gets the wrapper
        // and the other build settings, but no cache client or cache paths.
        let default_worker = SandboxExecutionScratch::prepare(fixture.path())
            .unwrap()
            .with_build_env(&runtime, true);
        let mut default_cmd = Command::new("true");
        default_worker.stamp_build_env(&mut default_cmd);
        assert_eq!(env_value(&default_cmd, "RSI_BUILD_SCCACHE"), None);
        assert_eq!(env_value(&default_cmd, "SCCACHE_DIR"), None);
        assert_eq!(env_value(&default_cmd, "SCCACHE_CACHE_SIZE"), None);
        assert_eq!(
            env_value(&default_cmd, "RUSTC_WRAPPER").is_some(),
            installed_build_wrapper().is_some()
        );

        // The operator setting still turns the cache on for new workers.
        runtime
            .update_field("agent_build_sccache_enabled", &serde_json::json!(true))
            .unwrap();
        let worker = SandboxExecutionScratch::prepare(fixture.path())
            .unwrap()
            .with_build_env(&runtime, true);
        let mut worker_cmd = Command::new("true");
        worker.stamp_build_env(&mut worker_cmd);
        assert_eq!(
            env_value(&worker_cmd, "CARGO_BUILD_JOBS").as_deref(),
            Some("4")
        );
        assert_eq!(
            env_value(&worker_cmd, "CARGO_PROFILE_DEV_DEBUG").as_deref(),
            Some("line-tables-only")
        );
        assert_eq!(
            env_value(&worker_cmd, "CARGO_INCREMENTAL").as_deref(),
            Some("0")
        );
        assert_eq!(
            env_value(&worker_cmd, "RUSTC_WRAPPER").is_some(),
            executable_on_path("sccache").is_some() || installed_build_wrapper().is_some()
        );
        if executable_on_path("sccache").is_some() {
            assert_eq!(
                env_value(&worker_cmd, "SCCACHE_CACHE_SIZE").as_deref(),
                Some("10G")
            );
            assert_eq!(
                env_value(&worker_cmd, "SCCACHE_CLIENT_SIDE").as_deref(),
                Some("1")
            );
        }

        // #1741: the same values are recorded for the session's job units,
        // and only names the job launcher honours are written.
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixture.path().join(BUILD_ENV_RECORD)).unwrap())
                .unwrap();
        let recorded = record["env"].as_object().unwrap();
        assert_eq!(recorded["CARGO_INCREMENTAL"], "0");
        assert_eq!(recorded["CARGO_PROFILE_DEV_DEBUG"], "line-tables-only");
        for (name, value) in recorded {
            assert!(RECORDED_BUILD_ENV.contains(&name.as_str()), "{name}");
            assert_eq!(env_value(&worker_cmd, name).as_deref(), value.as_str());
        }
        let stamped_unset = worker_cmd
            .as_std()
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned());
        for name in stamped_unset {
            assert!(
                !recorded.contains_key(&name),
                "{name} is removed, not recorded"
            );
        }

        let lead = SandboxExecutionScratch::prepare(fixture.path())
            .unwrap()
            .with_build_env(&runtime, false);
        let mut lead_cmd = Command::new("true");
        lead.stamp_build_env(&mut lead_cmd);
        assert_eq!(
            env_value(&lead_cmd, "CARGO_BUILD_JOBS").as_deref(),
            Some("4")
        );
        assert_eq!(
            env_value(&lead_cmd, "CARGO_INCREMENTAL"),
            executable_on_path("sccache").map(|_| "0".to_string())
        );
        assert_eq!(
            env_value(&lead_cmd, "RUSTC_WRAPPER").is_some(),
            installed_build_wrapper().is_some() || executable_on_path("sccache").is_some()
        );
    }

    /// #2745: the cache server socket sits inside the cache directory, the one
    /// directory a Codex `workspace-write` launch grants, so a cold start can
    /// create both.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn sccache_socket_is_inside_the_cache_directory() {
        let build = AgentBuildEnvironment {
            jobs: 4,
            line_tables_only: true,
            worker: true,
            wrapper: Some(PathBuf::from("/w/rsi-build-rustc")),
            slots: 16,
            sccache: Some(PathBuf::from("/usr/bin/sccache")),
            cache_gib: 10,
            pool_keep: 1,
        };
        let values = build.env_values();
        let get = |name: &str| {
            values
                .iter()
                .find_map(|(key, value)| (*key == name).then(|| value.clone()).flatten())
        };
        let cache = PathBuf::from(get("SCCACHE_DIR").expect("cache dir stamped"));
        let socket = PathBuf::from(get("SCCACHE_SERVER_UDS").expect("socket stamped"));
        assert_eq!(socket.parent(), Some(cache.as_path()));
    }

    /// #2716: the pool a reclaim-triggered refresh builds must carry the build key the
    /// worker seeds look for. The key hashes `CARGO_INCREMENTAL` and `CARGO_PROFILE_*`,
    /// so a daemon environment without the worker overrides yields a different one.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn target_pool_refresh_and_worker_seed_commands_yield_the_same_build_key() {
        use crate::config::Config;
        use std::collections::BTreeMap;

        let fixture = disk_backed_fixture();
        let runtime = RuntimeConfig::from_config(&Config::default());
        let worker = SandboxExecutionScratch::prepare(fixture.path())
            .unwrap()
            .with_build_env(&runtime, true);
        let build = worker.build_env.clone().unwrap();
        let script = Path::new("target-pool");
        let seed = worker.target_pool_seed_command(script, &build);
        let refresh = target_pool_refresh_command(script, &runtime, 3);

        // A daemon environment that lacks the worker overrides and carries others.
        let mut daemon: BTreeMap<String, String> = std::env::vars()
            .filter(|(name, _)| name != "CARGO_INCREMENTAL" && !name.starts_with("CARGO_PROFILE_"))
            .collect();
        daemon.insert("CARGO_INCREMENTAL".into(), "1".into());

        let effective = |overrides: Vec<(String, Option<String>)>| {
            let mut env = daemon.clone();
            for (name, value) in overrides {
                match value {
                    Some(value) => env.insert(name, value),
                    None => env.remove(&name),
                };
            }
            env
        };
        let overrides_of = |envs: std::process::CommandEnvs<'_>| {
            envs.map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<Vec<_>>()
        };
        let pool_script = Path::new(rsi_common::manifest_dir!()).join("../../scripts/target-pool");
        let key_under = |env: BTreeMap<String, String>| {
            let output = std::process::Command::new("python3")
                .arg(&pool_script)
                .arg("key")
                .arg("--repo")
                .arg(fixture.path())
                .env_clear()
                .envs(&env)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };

        let seed_key = key_under(effective(overrides_of(seed.as_std().get_envs())));
        let refresh_key = key_under(effective(overrides_of(refresh.get_envs())));
        assert_eq!(refresh_key, seed_key);
        assert_ne!(
            key_under(daemon.clone()),
            seed_key,
            "the daemon environment alone must not already match, or the test proves nothing"
        );
        let refresh_env: BTreeMap<_, _> = overrides_of(refresh.get_envs()).into_iter().collect();
        assert_eq!(
            refresh_env.get("RSI_TARGET_POOL_KEEP"),
            Some(&Some("3".to_string()))
        );
        assert_eq!(
            refresh_env.get("CARGO_INCREMENTAL"),
            Some(&Some("0".to_string()))
        );
        // #2773: the pool build is told the floor the workers' cargo-slot holds.
        assert_eq!(
            refresh_env.get("RSI_TARGET_POOL_WORKER_FLOOR_GIB"),
            Some(&Some(
                runtime.governor_policy().min_free_disk_gb.to_string()
            ))
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn only_a_target_holding_daemon_scratch_is_seeded_from_the_pool() {
        // #1741: the warm target pool fills a fresh worker target only; a
        // target with any build output is the session's own and stays.
        let fixture = disk_backed_fixture();
        let scratch = SandboxExecutionScratch::prepare(fixture.path()).unwrap();
        std::fs::write(scratch.target().join(BUILD_ENV_RECORD), b"{}").unwrap();
        assert!(target_is_unseeded(scratch.target()));
        assert!(target_is_unseeded(&fixture.path().join("absent")));
        std::fs::create_dir(scratch.target().join("debug")).unwrap();
        assert!(!target_is_unseeded(scratch.target()));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn lead_sessions_and_non_cargo_roots_are_never_seeded() {
        use crate::config::Config;
        let fixture = disk_backed_fixture();
        let runtime = RuntimeConfig::from_config(&Config::default());
        let lead = SandboxExecutionScratch::prepare(fixture.path())
            .unwrap()
            .with_build_env(&runtime, false);
        assert_eq!(lead.seed_target_from_pool().await, None);
        // No Cargo.toml at the root: nothing to seed, no pool build requested.
        let worker = SandboxExecutionScratch::prepare(fixture.path())
            .unwrap()
            .with_build_env(&runtime, true);
        assert_eq!(worker.seed_target_from_pool().await, None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn execution_scratch_prepares_fixed_private_layout() {
        let fixture = disk_backed_fixture();
        let scratch = SandboxExecutionScratch::prepare(fixture.path()).unwrap();
        assert_eq!(scratch.target(), fixture.path().join("target"));
        assert_eq!(scratch.temp(), fixture.path().join("target/.rsi-tmp"));
        assert_eq!(permission_bits(scratch.target()), PRIVATE_DIRECTORY_MODE);
        assert_eq!(permission_bits(scratch.temp()), PRIVATE_DIRECTORY_MODE);
        scratch.revalidate().unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn execution_scratch_accepts_only_context_read_permits_and_preserves_ordinary_none() {
        let fixture = disk_backed_fixture();
        let target = fixture.path().join("target");
        let context = CustodyEffectPermit::for_execution_scratch_test(
            fixture.path().to_path_buf(),
            Some(target),
            EffectKind::ContextRead,
        );
        assert!(
            SandboxExecutionScratch::from_context_permit(&context)
                .unwrap()
                .is_some()
        );

        for kind in [
            EffectKind::ProviderLaunch,
            EffectKind::ProviderTurn,
            EffectKind::ToolExecution,
            EffectKind::BuildCacheReclaim,
        ] {
            let permit = CustodyEffectPermit::for_execution_scratch_test(
                fixture.path().to_path_buf(),
                Some(fixture.path().join("target")),
                kind,
            );
            assert!(matches!(
                SandboxExecutionScratch::from_context_permit(&permit),
                Err(DaemonError::ExecutionScratchUnavailable(_))
            ));
        }

        let ordinary = CustodyEffectPermit::for_execution_scratch_test(
            fixture.path().to_path_buf(),
            None,
            EffectKind::ContextRead,
        );
        assert_eq!(
            SandboxExecutionScratch::from_context_permit(&ordinary).unwrap(),
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn execution_scratch_rejects_target_and_temp_symlinks_and_replacement() {
        let fixture = disk_backed_fixture();
        let target = fixture.path().join("target");
        symlink(fixture.path(), &target).unwrap();
        assert!(SandboxExecutionScratch::prepare(fixture.path()).is_err());
        std::fs::remove_file(&target).unwrap();
        let scratch = SandboxExecutionScratch::prepare(fixture.path()).unwrap();
        std::fs::remove_dir_all(scratch.target()).unwrap();
        std::fs::create_dir(scratch.target()).unwrap();
        assert!(scratch.revalidate().is_err());

        let temp_fixture = disk_backed_fixture();
        let scratch = SandboxExecutionScratch::prepare(temp_fixture.path()).unwrap();
        std::fs::remove_dir(scratch.temp()).unwrap();
        symlink(temp_fixture.path(), scratch.temp()).unwrap();
        assert!(scratch.revalidate().is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn execution_scratch_revalidation_rejects_mode_drift() {
        let fixture = disk_backed_fixture();
        let scratch = SandboxExecutionScratch::prepare(fixture.path()).unwrap();
        std::fs::set_permissions(scratch.target(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(scratch.revalidate().is_err());

        std::fs::set_permissions(scratch.target(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(scratch.temp(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(scratch.revalidate().is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn repeated_execution_scratch_refusal_keeps_file_descriptors_bounded() {
        let fixture = disk_backed_fixture();
        let scratch = SandboxExecutionScratch::prepare(fixture.path()).unwrap();
        std::fs::set_permissions(scratch.temp(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let descriptor_count = || std::fs::read_dir(OPEN_DESCRIPTOR_DIR).unwrap().count();
        let before = descriptor_count();
        for _ in 0..512 {
            assert!(matches!(
                scratch.revalidate(),
                Err(DaemonError::ExecutionScratchUnavailable(_))
            ));
        }
        let after = descriptor_count();
        assert!(
            // The lib-test process runs unrelated descriptor-using tests in
            // parallel. A fixed allowance absorbs that ambient churn while a
            // per-refusal leak (512 target/temp descriptors) remains decisive.
            after <= before + 64,
            "repeated refusal leaked descriptors: before={before}, after={after}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn repeated_prepare_refusal_after_target_open_keeps_file_descriptors_bounded() {
        let fixture = disk_backed_fixture();
        let target = fixture.path().join("target");
        std::fs::create_dir(&target).unwrap();
        symlink(fixture.path(), target.join(".rsi-tmp")).unwrap();

        let descriptor_count = || std::fs::read_dir(OPEN_DESCRIPTOR_DIR).unwrap().count();
        let before = descriptor_count();
        let attempts = 512;
        let mut refusals = 0;
        for _ in 0..attempts {
            let error = SandboxExecutionScratch::prepare(fixture.path())
                .expect_err("symlinked .rsi-tmp must fail after target opens");
            assert!(
                error.to_string().contains("cannot reopen .rsi-tmp"),
                "refusal must occur after target_fd opens: {error}"
            );
            refusals += 1;
        }
        let after = descriptor_count();
        assert_eq!(refusals, attempts, "prepare refusal loop must be nonzero");
        assert!(
            // The lib-test process may run unrelated descriptor-using tests in
            // parallel. A fixed allowance still makes the original leak of
            // one target descriptor per refusal decisive.
            after <= before + 64,
            "repeated prepare refusal leaked descriptors: refusals={refusals}, before={before}, after={after}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn execution_scratch_rejects_cross_device_identity() {
        let root = FileIdentity {
            device: 1,
            inode: 10,
        };
        let child = FileIdentity {
            device: 2,
            inode: 11,
        };
        assert!(require_same_device(root, child, "target").is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn execution_scratch_rejects_tmpfs_root_when_available() {
        let tmpfs = Path::new("/dev/shm");
        if tmpfs.is_dir() {
            assert!(SandboxExecutionScratch::prepare(tmpfs).is_err());
        }
    }
}
