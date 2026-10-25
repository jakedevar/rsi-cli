//! `AgentGetDaemonInfo` (#1045 slice 1): assemble the hub daemon's identity and
//! health. Everything returned is a build identifier, hash, timestamp or number;
//! no environment value or credential is ever read into the result.

use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::agent_daemon_info::{
    DAEMON_SUPERVISOR_NONE, DAEMON_SUPERVISOR_SCRIPT, DaemonDiskFreeV1, DaemonInfoV1, DaemonLoadV1,
    InstalledToolV1,
};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

/// Commit SHA embedded by `build.rs` (`RSI_BUILD_SHA` or `git rev-parse HEAD`).
pub const BUILD_SHA: &str = env!("RSI_BUILD_SHA");

/// Harness source digest of `BUILD_SHA`, embedded by `build.rs` (#3192): a sha256 over the git
/// tree or blob ids of the paths in `rsi-common/data/harness_source_paths.txt`, or
/// `unavailable:<sha>` when git could not say. It changes only when the harness code does, not on
/// every redeploy, which keeps a benchmark baseline comparable across builds.
pub const HARNESS_SOURCE_DIGEST: &str = env!("RSI_HARNESS_SOURCE_DIGEST");

/// Process-lifetime facts, captured once at startup.
pub struct DaemonInfoService {
    started_at: DateTime<Utc>,
    started: Instant,
    data_dir: PathBuf,
    sandbox_base: PathBuf,
    /// Hashed once, off the request path when [`Self::warm`] runs at startup.
    binary_sha256: Arc<OnceLock<Option<String>>>,
    binary_path: PathBuf,
    local_build: Arc<Mutex<Option<rsi_common::agent_daemon_info::LocalBuildV1>>>,
    local_build_started: OnceLock<()>,
}

static GLOBAL: OnceLock<DaemonInfoService> = OnceLock::new();

impl DaemonInfoService {
    #[must_use]
    pub fn new(data_dir: PathBuf, sandbox_base: PathBuf, binary_path: PathBuf) -> Self {
        Self {
            started_at: Utc::now(),
            started: Instant::now(),
            data_dir,
            sandbox_base,
            binary_sha256: Arc::new(OnceLock::new()),
            binary_path,
            local_build: Arc::new(Mutex::new(None)),
            local_build_started: OnceLock::new(),
        }
    }

    /// Record the daemon's start time and paths, and start hashing the running
    /// binary on a background thread. Idempotent; the first call wins.
    pub fn init_global(sandbox_base: PathBuf) -> &'static Self {
        let service = GLOBAL.get_or_init(|| {
            Self::new(
                rsi_common::identity::data_dir(),
                sandbox_base,
                running_binary_path(),
            )
        });
        service.warm();
        service.local_build_started.get_or_init(|| {
            // A sandbox-built daemon still watches the stable repository's build.
            let main = Path::new(env!("RSI_BUILD_MAIN_WORKTREE"));
            let repo = if !main.as_os_str().is_empty() && main.is_dir() {
                Some(main)
            } else {
                crate::session::preamble::harness_root()
            };
            let paths = crate::local_build::candidates(repo, &service.data_dir);
            let mut detector =
                crate::local_build::Detector::new(paths, BUILD_SHA, &service.binary_path);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(crate::local_build::REFRESH_INTERVAL);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    service.refresh_local_build(&mut detector).await;
                }
            });
        });
        service
    }

    pub(crate) async fn refresh_local_build(&self, detector: &mut crate::local_build::Detector) {
        let build = detector.refresh().await;
        *self
            .local_build
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = build;
    }

    /// The process-wide service; initialised with the default sandbox base when
    /// `main` did not call [`Self::init_global`] (tests, tools).
    pub fn global() -> &'static Self {
        GLOBAL.get_or_init(|| {
            let base = dirs::home_dir().map_or_else(
                || PathBuf::from("/tmp/rsi-sandboxes"),
                |home| home.join(".rsi/sandboxes"),
            );
            Self::new(
                rsi_common::identity::data_dir(),
                base,
                running_binary_path(),
            )
        })
    }

    /// Hash the binary in the background so the first read never waits.
    pub fn warm(&self) {
        let cell = Arc::clone(&self.binary_sha256);
        let path = self.binary_path.clone();
        std::thread::spawn(move || {
            cache_binary_sha256(&cell, &path);
        });
    }

    /// The cached (or, if still pending, freshly computed) running-binary hash.
    #[must_use]
    pub fn running_binary_sha256(&self) -> Option<String> {
        cache_binary_sha256(&self.binary_sha256, &self.binary_path)
    }

    /// Daemon start time, RFC3339 with nanoseconds.
    #[must_use]
    pub fn started_at_rfc3339(&self) -> String {
        self.started_at.to_rfc3339_opts(SecondsFormat::Nanos, true)
    }

    /// Assemble the snapshot. `schema_version` comes from the live database.
    #[must_use]
    pub fn snapshot(&self, schema_version: i64) -> DaemonInfoV1 {
        let binary_sha256 = cache_binary_sha256(&self.binary_sha256, &self.binary_path);
        DaemonInfoV1 {
            build_sha: BUILD_SHA.to_string(),
            local_build: self
                .local_build
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            binary_sha256,
            started_at: self.started_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
            uptime_secs: self.started.elapsed().as_secs(),
            schema_version,
            disk_free: DaemonDiskFreeV1 {
                data_dir_free_bytes: disk_free_bytes(&self.data_dir),
                sandbox_base_free_bytes: disk_free_bytes(&self.sandbox_base),
            },
            load: load_average(),
            supervisor_mode: supervisor_mode(),
            supervisor_binary: supervisor_binary().map(|path| path.display().to_string()),
            installed_tools: installed_tools(),
            // The deploy record lives in the store; the verb fills it (hub #2980).
            last_deploy: None,
            build_cache_reclaim: Some(build_cache_reclaim().to_owned()),
        }
    }
}

/// `supported` when this build reclaims sandbox build caches (Linux, macOS),
/// else `unsupported` (the fail-closed shim; hub #3055 part 2).
#[must_use]
pub fn build_cache_reclaim() -> &'static str {
    if crate::sandbox::target_reclaim::RECLAIM_SUPPORTED {
        rsi_common::agent_daemon_info::BUILD_CACHE_RECLAIM_SUPPORTED
    } else {
        rsi_common::agent_daemon_info::BUILD_CACHE_RECLAIM_UNSUPPORTED
    }
}

/// Where the daemon looks for `cargo-slot` and `target-pool` (`~/.rsi/bin`).
#[must_use]
pub fn tools_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".rsi/bin"))
}

/// Install state and content hash of each deploy-managed script in `dir`.
#[must_use]
pub fn installed_tools_in(dir: &Path) -> Vec<InstalledToolV1> {
    use std::os::unix::fs::PermissionsExt;
    rsi_common::agent_deploy::DEPLOY_TOOL_SCRIPTS
        .iter()
        .map(|name| {
            let path = if *name == "cargo-slot" {
                dir.parent().unwrap_or(dir).join("lib/cargo-slot")
            } else {
                dir.join(name)
            };
            let mut installed = path
                .metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0);
            // Jobs run the `bin` entry, so the implementation only counts as
            // installed behind the daemon-managed wrapper (#2777).
            if *name == "cargo-slot" {
                installed = installed && crate::deploy::is_cargo_slot_wrapper(&dir.join(name));
            }
            InstalledToolV1 {
                name: (*name).to_string(),
                installed,
                sha256: if installed {
                    binary_sha256(&path)
                } else {
                    None
                },
            }
        })
        .collect()
}

fn installed_tools() -> Vec<InstalledToolV1> {
    tools_dir().map_or_else(Vec::new, |dir| installed_tools_in(&dir))
}

/// The cached running-binary hash, computing it when absent. A failed read is
/// never cached: a transient error under load must not make the binary look
/// unreadable for the rest of the process (#2095).
fn cache_binary_sha256(cell: &OnceLock<Option<String>>, path: &Path) -> Option<String> {
    if let Some(Some(hash)) = cell.get() {
        return Some(hash.clone());
    }
    let hash = binary_sha256(path);
    if hash.is_some() {
        let _ = cell.set(hash.clone());
    }
    hash
}

/// Lowercase hex sha256 of a file, streamed. `None` when it cannot be read.
#[must_use]
pub fn binary_sha256(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Some(hex::encode(hasher.finalize()))
}

/// Free bytes available to an unprivileged writer on the filesystem holding
/// `path` (or its nearest existing ancestor).
#[must_use]
pub fn disk_free_bytes(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|candidate| candidate.exists())?;
    let stats = nix::sys::statvfs::statvfs(existing).ok()?;
    Some(u64::from(stats.blocks_available()).saturating_mul(u64::from(stats.fragment_size())))
}

/// Parse `/proc/loadavg` (`0.42 0.30 0.25 1/300 12345`).
#[must_use]
pub fn parse_loadavg(body: &str) -> Option<DaemonLoadV1> {
    let mut fields = body.split_whitespace();
    let mut next = || fields.next()?.parse::<f64>().ok().filter(|v| v.is_finite());
    Some(DaemonLoadV1 {
        one: next()?,
        five: next()?,
        fifteen: next()?,
    })
}

/// The host's load averages: `/proc/loadavg` on Linux.
#[cfg(target_os = "linux")]
pub(crate) fn load_average() -> Option<DaemonLoadV1> {
    parse_loadavg(&std::fs::read_to_string("/proc/loadavg").ok()?)
}

/// The host's load averages from `getloadavg(3)` on macOS and other unix
/// systems that have no `/proc/loadavg` (#2937).
#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn load_average() -> Option<DaemonLoadV1> {
    let mut samples = [0.0_f64; 3];
    // SAFETY: getloadavg writes at most `nelem` (3) doubles into the buffer.
    let filled = unsafe { nix::libc::getloadavg(samples.as_mut_ptr(), 3) };
    load_from_getloadavg(samples, filled)
}

/// No load average on this platform.
#[cfg(not(unix))]
pub(crate) fn load_average() -> Option<DaemonLoadV1> {
    None
}

/// Validate one `getloadavg(3)` result: all three samples must be filled
/// (`filled == 3`, it returns -1 or fewer on failure), finite and non-negative.
#[cfg(any(test, all(unix, not(target_os = "linux"))))]
#[must_use]
pub(crate) fn load_from_getloadavg(samples: [f64; 3], filled: i32) -> Option<DaemonLoadV1> {
    if filled != 3 || samples.iter().any(|v| !v.is_finite() || *v < 0.0) {
        return None;
    }
    Some(DaemonLoadV1 {
        one: samples[0],
        five: samples[1],
        fifteen: samples[2],
    })
}

/// Classify a parent process's command line: the supervisor script, or not.
#[must_use]
pub fn classify_supervisor(parent_cmdline: &str) -> &'static str {
    if parent_cmdline
        .split('\0')
        .any(|arg| arg.rsplit('/').next() == Some(DAEMON_SUPERVISOR_SCRIPT))
    {
        DAEMON_SUPERVISOR_SCRIPT
    } else {
        DAEMON_SUPERVISOR_NONE
    }
}

/// The rsid path a supervisor command line relaunches: the argument after the
/// supervisor script (`rsid-supervisor.sh /path/to/rsid`).
#[must_use]
pub fn parse_supervisor_binary(parent_cmdline: &str) -> Option<PathBuf> {
    let mut args = parent_cmdline.split('\0');
    args.find(|arg| arg.rsplit('/').next() == Some(DAEMON_SUPERVISOR_SCRIPT))?;
    args.next().filter(|arg| !arg.is_empty()).map(PathBuf::from)
}

/// The absolute path of the `rsid-supervisor.sh` a supervisor command line
/// runs (#2368). A relative argument cannot be resolved from the daemon's cwd.
#[must_use]
pub fn parse_supervisor_script(parent_cmdline: &str) -> Option<PathBuf> {
    parent_cmdline
        .split('\0')
        .find(|arg| arg.rsplit('/').next() == Some(DAEMON_SUPERVISOR_SCRIPT))
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// The script the supervising bash executes; `None` when the daemon is not
/// under the supervisor or its argv cannot be read.
pub(crate) fn supervisor_script() -> Option<PathBuf> {
    parse_supervisor_script(&parent_cmdline()?)
}

/// The binary the supervising `rsid-supervisor.sh` relaunches (#1164); `None`
/// when the daemon is not under the supervisor or its argv cannot be read.
pub(crate) fn supervisor_binary() -> Option<PathBuf> {
    parse_supervisor_binary(&parent_cmdline()?)
}

pub(crate) fn supervisor_mode() -> Option<String> {
    Some(classify_supervisor(&parent_cmdline()?).to_string())
}

/// The running executable [`DaemonInfoService`] hashes. Linux reads
/// `/proc/self/exe`, which names the running inode even after a deploy replaced
/// the installed path. macOS has no `/proc` (#2936): it hashes the path this
/// process was started from, which `warm` reads right at startup, before any
/// later deploy can swap it; without it a deploy's verification never saw the
/// running build and stayed `restarting`.
fn running_binary_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    if let Ok(exe) = std::env::current_exe() {
        return exe;
    }
    PathBuf::from("/proc/self/exe")
}

/// The parent process's argv, NUL-separated like Linux `/proc/<pid>/cmdline`;
/// `None` when it cannot be read.
fn parent_cmdline() -> Option<String> {
    process_cmdline(std::os::unix::process::parent_id())
}

#[cfg(not(target_os = "macos"))]
fn process_cmdline(pid: u32) -> Option<String> {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(String::from_utf8_lossy(&cmdline).into_owned())
}

/// macOS has no `/proc` (#2936): read the argv with `sysctl` `KERN_PROCARGS2`,
/// without which a launchd-run satellite never counts as supervised and every
/// deploy is refused `deploy_needs_supervisor`.
#[cfg(target_os = "macos")]
fn process_cmdline(pid: u32) -> Option<String> {
    use nix::libc;
    let pid = libc::c_int::try_from(pid).ok()?;
    let mut argmax: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    // SAFETY: `mib` names two levels, and `argmax`/`size` describe a writable
    // c_int that outlives the call; no new value is written.
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&raw mut argmax).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 || argmax <= 0 {
        return None;
    }
    let mut buffer = vec![0_u8; usize::try_from(argmax).ok()?];
    let mut size = buffer.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: `mib` names three levels, and `buffer`/`size` describe a
    // writable allocation of `size` bytes that outlives the call.
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    buffer.truncate(size);
    parse_procargs2(&buffer)
}

/// Decode a macOS `KERN_PROCARGS2` buffer (a native-endian `argc`, the exec
/// path, NUL padding, then `argc` NUL-terminated arguments and the
/// environment) into the argv alone, NUL-separated like `/proc/<pid>/cmdline`.
/// The environment is never read.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn parse_procargs2(buffer: &[u8]) -> Option<String> {
    let argc = i32::from_ne_bytes(buffer.get(..4)?.try_into().ok()?);
    let argc = usize::try_from(argc).ok().filter(|argc| *argc > 0)?;
    let rest = &buffer[4..];
    let exec_end = rest.iter().position(|byte| *byte == 0)?;
    let mut rest = &rest[exec_end..];
    while let [0, tail @ ..] = rest {
        rest = tail;
    }
    let mut args = Vec::with_capacity(argc);
    for _ in 0..argc {
        let end = rest.iter().position(|byte| *byte == 0)?;
        args.push(String::from_utf8_lossy(&rest[..end]).into_owned());
        rest = &rest[end + 1..];
    }
    let mut cmdline = args.join("\0");
    cmdline.push('\0');
    Some(cmdline)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn daemon_info_reports_build_cache_reclaim_for_the_compiled_platform() {
        // Hub #3055 part 2: Linux and macOS reclaim; any other platform runs
        // the fail-closed shim and says so.
        let dir = tempfile::tempdir().unwrap();
        let info = DaemonInfoService::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            dir.path().join("rsid"),
        )
        .snapshot(1);
        let expected = if cfg!(any(target_os = "linux", target_os = "macos")) {
            "supported"
        } else {
            "unsupported"
        };
        assert_eq!(info.build_cache_reclaim.as_deref(), Some(expected));
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["build_cache_reclaim"], expected);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn daemon_info_hashes_a_file_with_known_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bin");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            binary_sha256(&path).as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert_eq!(binary_sha256(&dir.path().join("missing")), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn installed_tools_report_presence_and_content_hash_and_name_a_missing_one() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        // cargo-slot's implementation lives in the sibling `lib` dir; the `bin`
        // entry is only a wrapper.
        let dir = root.path().join("bin");
        let lib = root.path().join("lib");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        let slot = lib.join("cargo-slot");
        std::fs::write(&slot, b"abc").unwrap();
        std::fs::set_permissions(&slot, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Jobs run the `bin` entry: a lib copy with no wrapper is not installed.
        assert!(!installed_tools_in(&dir)[0].installed);
        let wrapper = dir.join("cargo-slot");
        std::fs::write(&wrapper, crate::deploy::CARGO_SLOT_WRAPPER).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tools = installed_tools_in(&dir);
        let by_name = |name: &str| tools.iter().find(|tool| tool.name == name).unwrap();
        assert!(by_name("cargo-slot").installed);
        assert_eq!(
            by_name("cargo-slot").sha256.as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        // target-pool is absent: listed, not installed, no hash (never omitted).
        assert!(!by_name("target-pool").installed);
        assert_eq!(by_name("target-pool").sha256, None);
        // A non-executable copy is not usable by the daemon either.
        let pool = dir.join("target-pool");
        std::fs::write(&pool, b"x").unwrap();
        std::fs::set_permissions(&pool, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!installed_tools_in(&dir)[1].installed);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_failed_binary_read_is_not_cached_and_a_later_read_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bin");
        let cell = OnceLock::new();
        assert_eq!(cache_binary_sha256(&cell, &path), None);
        std::fs::write(&path, b"abc").unwrap();
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(cache_binary_sha256(&cell, &path).as_deref(), Some(expected));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(cache_binary_sha256(&cell, &path).as_deref(), Some(expected));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn daemon_info_parses_loadavg_and_classifies_supervisor() {
        let load = parse_loadavg("0.42 0.30 0.25 1/300 12345\n").unwrap();
        assert_eq!((load.one, load.five, load.fifteen), (0.42, 0.30, 0.25));
        assert!(parse_loadavg("garbage").is_none());
        // #2937: the getloadavg(3) result is trusted only when all three
        // samples were filled and are sane.
        let ok = load_from_getloadavg([0.5, 1.5, 2.5], 3).unwrap();
        assert_eq!((ok.one, ok.five, ok.fifteen), (0.5, 1.5, 2.5));
        assert!(load_from_getloadavg([0.5, 1.5, 2.5], -1).is_none());
        assert!(load_from_getloadavg([0.5, 1.5, 2.5], 1).is_none());
        assert!(load_from_getloadavg([f64::NAN, 1.5, 2.5], 3).is_none());
        assert!(load_from_getloadavg([0.5, -1.0, 2.5], 3).is_none());
        assert_eq!(
            classify_supervisor("/usr/bin/bash\0/home/u/.rsi/bin/rsid-supervisor.sh\0/x/rsid\0"),
            DAEMON_SUPERVISOR_SCRIPT
        );
        assert_eq!(
            classify_supervisor("/usr/lib/systemd/systemd\0--user\0"),
            "none"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn daemon_info_names_the_binary_the_supervisor_relaunches() {
        assert_eq!(
            parse_supervisor_binary(
                "/usr/bin/bash\0/r/scripts/rsid-supervisor.sh\0/h/.rsi/install/rsid\0"
            ),
            Some(PathBuf::from("/h/.rsi/install/rsid"))
        );
        assert_eq!(
            parse_supervisor_binary("/usr/lib/systemd/systemd\0--user\0"),
            None
        );
        assert_eq!(parse_supervisor_binary("bash\0rsid-supervisor.sh\0"), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn daemon_info_names_the_script_the_supervisor_executes() {
        assert_eq!(
            parse_supervisor_script(
                "/usr/bin/bash\0/h/.rsi/install/rsid-supervisor.sh\0/h/.rsi/install/rsid\0"
            ),
            Some(PathBuf::from("/h/.rsi/install/rsid-supervisor.sh"))
        );
        assert_eq!(parse_supervisor_script("bash\0rsid-supervisor.sh\0"), None);
        assert_eq!(parse_supervisor_script("systemd\0--user\0"), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn daemon_info_reads_the_supervisor_argv_from_a_macos_procargs_buffer() {
        // #2936: argc, the exec path, NUL padding, argv, then the environment.
        let mut buffer = 3_i32.to_ne_bytes().to_vec();
        buffer.extend_from_slice(b"/bin/bash\0\0\0\0");
        buffer
            .extend_from_slice(b"bash\0/h/.rsi/install/rsid-supervisor.sh\0/h/.rsi/install/rsid\0");
        buffer.extend_from_slice(b"HOME=/h\0SECRET_TOKEN=x\0");
        let cmdline = parse_procargs2(&buffer).unwrap();
        assert_eq!(
            cmdline,
            "bash\0/h/.rsi/install/rsid-supervisor.sh\0/h/.rsi/install/rsid\0"
        );
        assert_eq!(classify_supervisor(&cmdline), DAEMON_SUPERVISOR_SCRIPT);
        assert_eq!(
            parse_supervisor_binary(&cmdline),
            Some(PathBuf::from("/h/.rsi/install/rsid"))
        );
        assert_eq!(
            parse_supervisor_script(&cmdline),
            Some(PathBuf::from("/h/.rsi/install/rsid-supervisor.sh"))
        );
        // A truncated buffer reads as unknown, never as a shorter argv.
        assert_eq!(parse_procargs2(&buffer[..20]), None);
        assert_eq!(parse_procargs2(&[1, 0]), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn daemon_info_snapshot_is_well_formed() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("rsid");
        std::fs::write(&exe, b"abc").unwrap();
        let service = DaemonInfoService::new(
            dir.path().to_path_buf(),
            dir.path().join("sandboxes-not-created-yet"),
            exe,
        );
        let info = service.snapshot(145);
        assert!(!info.build_sha.is_empty());
        assert_eq!(info.binary_sha256.as_ref().map(String::len), Some(64));
        assert_eq!(info.schema_version, 145);
        let started = chrono::DateTime::parse_from_rfc3339(&info.started_at).unwrap();
        assert!(started.timestamp() > 0);
        // Nanosecond precision: nine fractional digits before the `Z`.
        let fraction = info.started_at.split('.').nth(1).unwrap();
        assert_eq!(fraction.trim_end_matches('Z').len(), 9);
        assert!(info.disk_free.data_dir_free_bytes.unwrap() > 0);
        // A not-yet-created sandbox base resolves through its ancestor.
        assert!(info.disk_free.sandbox_base_free_bytes.unwrap() > 0);
        if let Some(load) = info.load {
            assert!(load.one >= 0.0 && load.five >= 0.0 && load.fifteen >= 0.0);
        }
    }
}
