//! `AgentGetDaemonInfo` (#1045 slice 1): assemble the hub daemon's identity and
//! health. Everything returned is a build identifier, hash, timestamp or number;
//! no environment value or credential is ever read into the result.

use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::agent_daemon_info::{
    DAEMON_SUPERVISOR_NONE, DAEMON_SUPERVISOR_SCRIPT, DaemonDiskFreeV1, DaemonInfoV1, DaemonLoadV1,
};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

/// Commit SHA embedded by `build.rs` (`RSI_BUILD_SHA` or `git rev-parse HEAD`).
pub const BUILD_SHA: &str = env!("RSI_BUILD_SHA");

/// Process-lifetime facts, captured once at startup.
pub struct DaemonInfoService {
    started_at: DateTime<Utc>,
    started: Instant,
    data_dir: PathBuf,
    sandbox_base: PathBuf,
    /// Hashed once, off the request path when [`Self::warm`] runs at startup.
    binary_sha256: Arc<OnceLock<Option<String>>>,
    binary_path: PathBuf,
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
        }
    }

    /// Record the daemon's start time and paths, and start hashing the running
    /// binary on a background thread. Idempotent; the first call wins.
    pub fn init_global(sandbox_base: PathBuf) -> &'static Self {
        let service = GLOBAL.get_or_init(|| {
            Self::new(
                rsi_common::identity::data_dir(),
                sandbox_base,
                PathBuf::from("/proc/self/exe"),
            )
        });
        service.warm();
        service
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
                PathBuf::from("/proc/self/exe"),
            )
        })
    }

    /// Hash the binary in the background so the first read never waits.
    pub fn warm(&self) {
        let cell = Arc::clone(&self.binary_sha256);
        let path = self.binary_path.clone();
        std::thread::spawn(move || {
            cell.get_or_init(|| binary_sha256(&path));
        });
    }

    /// The cached (or, if still pending, freshly computed) running-binary hash.
    #[must_use]
    pub fn running_binary_sha256(&self) -> Option<String> {
        self.binary_sha256
            .get_or_init(|| binary_sha256(&self.binary_path))
            .clone()
    }

    /// Daemon start time, RFC3339 with nanoseconds.
    #[must_use]
    pub fn started_at_rfc3339(&self) -> String {
        self.started_at.to_rfc3339_opts(SecondsFormat::Nanos, true)
    }

    /// Assemble the snapshot. `schema_version` comes from the live database.
    #[must_use]
    pub fn snapshot(&self, schema_version: i64) -> DaemonInfoV1 {
        let binary_sha256 = self
            .binary_sha256
            .get_or_init(|| binary_sha256(&self.binary_path))
            .clone();
        DaemonInfoV1 {
            build_sha: BUILD_SHA.to_string(),
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
        }
    }
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

fn load_average() -> Option<DaemonLoadV1> {
    parse_loadavg(&std::fs::read_to_string("/proc/loadavg").ok()?)
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

/// The binary the supervising `rsid-supervisor.sh` relaunches (#1164); `None`
/// when the daemon is not under the supervisor or its argv cannot be read.
pub(crate) fn supervisor_binary() -> Option<PathBuf> {
    let parent = std::os::unix::process::parent_id();
    let cmdline = std::fs::read(format!("/proc/{parent}/cmdline")).ok()?;
    parse_supervisor_binary(&String::from_utf8_lossy(&cmdline))
}

pub(crate) fn supervisor_mode() -> Option<String> {
    let parent = std::os::unix::process::parent_id();
    let cmdline = std::fs::read(format!("/proc/{parent}/cmdline")).ok()?;
    Some(classify_supervisor(&String::from_utf8_lossy(&cmdline)).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn daemon_info_parses_loadavg_and_classifies_supervisor() {
        let load = parse_loadavg("0.42 0.30 0.25 1/300 12345\n").unwrap();
        assert_eq!((load.one, load.five, load.fifteen), (0.42, 0.30, 0.25));
        assert!(parse_loadavg("garbage").is_none());
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
