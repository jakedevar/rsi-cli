//! Construction of worker commands beneath a bounded systemd user slice.
//!
//! This is a foundation for provider launch and reviewer tool subprocesses.
//! Callers must retain the returned command's direct `Child`, configure its
//! pipes as usual, and settle the process group before dropping that handle.
//! The supported daemon launchers provision the parent slice before provider
//! admission; each production command verifies it without mutating policy.

use crate::error::{DaemonError, Result};
use crate::process_control::{
    ProcessContainment, configure_tokio_process_group, signal_process_group,
};
use rsi_common::rpc::WorkerSliceMemoryPressure;
use std::ffi::OsStr;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;
#[cfg(target_os = "linux")]
use std::time::Instant;
use tokio::process::Command;
use uuid::Uuid;

const SYSTEMD_RUN: &str = "/usr/bin/systemd-run";
const SCOPE_SLICE: &str = "rsi-workers.slice";
const MIN_MEMORY_MIB: u64 = 256;
const MAX_MEMORY_MIB: u64 = 1024 * 1024;
const MAX_CPU_WEIGHT: u64 = 10_000;

/// Explicit limits are mandatory: moving a worker out of `rsid.scope` must
/// never also remove its memory bound. Values come from the operator-governed
/// launcher snapshot and must match the live aggregate parent slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WorkerScopeLimits {
    memory_high_mib: u64,
    memory_max_mib: u64,
    memory_swap_max_mib: u64,
    cpu_weight: u64,
}

impl WorkerScopeLimits {
    pub(crate) fn new(high: u64, max: u64, swap_max: u64, cpu_weight: u64) -> Result<Self> {
        if !(MIN_MEMORY_MIB..=MAX_MEMORY_MIB).contains(&high)
            || !(MIN_MEMORY_MIB..=MAX_MEMORY_MIB).contains(&max)
            || high >= max
            || swap_max > MAX_MEMORY_MIB
            || !(1..=MAX_CPU_WEIGHT).contains(&cpu_weight)
        {
            return Err(DaemonError::InvalidParam(
                "worker scope memory limits are invalid".into(),
            ));
        }
        Ok(Self {
            memory_high_mib: high,
            memory_max_mib: max,
            memory_swap_max_mib: swap_max,
            cpu_weight,
        })
    }

    /// The atomic launcher snapshot is refreshed by operator-only daemon
    /// config writes. A changed value refuses new work until the supported
    /// restart has reconciled the parent slice.
    pub(crate) fn from_launcher_snapshot() -> Result<Self> {
        #[cfg(not(target_os = "linux"))]
        {
            // macOS has no systemd memory controller; retain its established
            // direct process launch and group lifecycle.
            let (high, max) = rsi_common::worker_memory::default_limits_mib();
            return Self::new(high, max, 0, 20);
        }
        #[cfg(all(test, target_os = "linux"))]
        {
            // Unit tests use the hermetic runner below and never touch the
            // live user manager or its production slice.
            let (high, max) = rsi_common::worker_memory::default_limits_mib();
            return Self::new(high, max, 0, 20);
        }
        #[cfg(all(not(test), target_os = "linux"))]
        {
            let path = rsi_common::identity::data_path("rsid-scope.env", "rsid-scope");
            let contents = std::fs::read_to_string(&path).map_err(|error| {
            DaemonError::Process(format!(
                "worker scope settings snapshot {} unavailable: {error}; restart rsid through its supported launcher",
                path.display()
            ))
        })?;
            Self::parse_launcher_snapshot(&contents)
        }
    }

    fn parse_launcher_snapshot(contents: &str) -> Result<Self> {
        let mut values = [None; 4];
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(DaemonError::Process(
                    "malformed scope settings snapshot".into(),
                ));
            };
            let index = match key.trim() {
                "worker_scope_memory_high_mib" => Some(0),
                "worker_scope_memory_max_mib" => Some(1),
                "worker_scope_memory_swap_max_mib" => Some(2),
                "worker_scope_cpu_weight" => Some(3),
                _ => None,
            };
            if let Some(index) = index {
                if values[index].is_some() {
                    return Err(DaemonError::Process(format!(
                        "duplicate worker scope setting {key}"
                    )));
                }
                values[index] = Some(value.trim().parse::<u64>().map_err(|_| {
                    DaemonError::Process(format!("invalid worker scope setting {key}"))
                })?);
            }
        }
        // Legacy four-key daemon snapshot: the new worker keys did not exist
        // before this feature, so all four default together on first upgrade.
        if values.iter().all(Option::is_none) {
            let (high, max) = rsi_common::worker_memory::default_limits_mib();
            return Self::new(high, max, 0, 20);
        }
        let [Some(high), Some(max), Some(swap), Some(cpu)] = values else {
            return Err(DaemonError::Process(
                "incomplete worker scope settings snapshot".into(),
            ));
        };
        Self::new(high, max, swap, cpu)
    }

    /// A finite parent must be present before the provider inherits ambient
    /// credentials or begins any work. Read four cgroup files directly rather
    /// than waiting for the user-manager D-Bus path on a Tokio runtime thread.
    fn verify_parent(self) -> Result<()> {
        let cgroup = std::fs::read_to_string("/proc/self/cgroup").map_err(|error| {
            DaemonError::Process(format!("cannot inspect daemon cgroup: {error}"))
        })?;
        // SAFETY: geteuid has no preconditions and does not mutate state.
        let uid = unsafe { nix::libc::geteuid() };
        let path = worker_slice_cgroup_path(&cgroup, uid)?;
        self.verify_parent_files(&path)
    }

    fn verify_parent_files(self, path: &Path) -> Result<()> {
        let number = |name: &str| -> Option<u64> {
            std::fs::read_to_string(path.join(name))
                .ok()?
                .trim()
                .parse::<u64>()
                .ok()
        };
        let mib = 1024 * 1024;
        let matches = number("memory.high") == Some(self.memory_high_mib * mib)
            && number("memory.max") == Some(self.memory_max_mib * mib)
            && number("memory.swap.max") == Some(self.memory_swap_max_mib * mib)
            && number("cpu.weight") == Some(self.cpu_weight);
        if !matches {
            return Err(DaemonError::Process(
                "rsi-workers.slice is missing or its aggregate limits differ from settings; restart rsid through its supported launcher".into(),
            ));
        }
        Ok(())
    }
}

fn worker_slice_cgroup_path(cgroup: &str, uid: u32) -> Result<std::path::PathBuf> {
    let own = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| DaemonError::Process("cgroup v2 placement is required".into()))?;
    let components: Vec<&str> = own
        .split('/')
        .filter(|component| !component.is_empty())
        .collect();
    let manager = format!("user@{uid}.service");
    let Some(index) = components
        .iter()
        .position(|component| *component == manager)
    else {
        return Err(DaemonError::Process(
            "daemon is not in the expected systemd user manager cgroup".into(),
        ));
    };
    if components.first() != Some(&"user.slice") || index < 2 {
        return Err(DaemonError::Process(
            "daemon user manager cgroup is outside user.slice".into(),
        ));
    }
    let mut path = std::path::PathBuf::from("/sys/fs/cgroup");
    for component in &components[..=index] {
        path.push(component);
    }
    // systemd.slice(5): each dash in a slice name encodes one parent level.
    Ok(path.join("rsi.slice").join(SCOPE_SLICE))
}

#[cfg(target_os = "linux")]
static HIGH_EVENT_SAMPLE: OnceLock<tokio::sync::Mutex<Option<(u64, Instant)>>> = OnceLock::new();

#[cfg(target_os = "linux")]
fn parse_high_events(contents: &str) -> Option<u64> {
    let mut high = None;
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() == Some("high") {
            if high.is_some() {
                return None;
            }
            high = Some(fields.next()?.parse().ok()?);
            if fields.next().is_some() {
                return None;
            }
        }
    }
    high
}

#[cfg(target_os = "linux")]
fn parse_full_avg60(contents: &str) -> Option<f64> {
    let mut full = None;
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() == Some("full") {
            if full.is_some() {
                return None;
            }
            full = fields
                .find_map(|field| field.strip_prefix("avg60="))
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite() && (0.0..=100.0).contains(value));
            if full.is_none() {
                return None;
            }
        }
    }
    full
}

#[cfg(target_os = "linux")]
fn high_events_per_minute(
    previous: Option<(u64, Instant)>,
    current: (u64, Instant),
) -> (Option<f64>, (u64, Instant)) {
    let Some((previous_count, previous_at)) = previous else {
        return (None, current);
    };
    if current.0 < previous_count {
        return (None, current);
    }
    let Some(elapsed) = current.1.checked_duration_since(previous_at) else {
        return (None, current);
    };
    let seconds = elapsed.as_secs_f64();
    if seconds == 0.0 {
        return (None, (previous_count, previous_at));
    }
    (
        Some((current.0 - previous_count) as f64 * 60.0 / seconds),
        current,
    )
}

#[cfg(target_os = "linux")]
async fn read_worker_pressure_at(path: &Path) -> Option<(u64, f64)> {
    let (events, pressure) = tokio::join!(
        tokio::fs::read_to_string(path.join("memory.events")),
        tokio::fs::read_to_string(path.join("memory.pressure"))
    );
    Some((
        parse_high_events(&events.ok()?)?,
        parse_full_avg60(&pressure.ok()?)?,
    ))
}

/// Read cgroup files directly; no systemd D-Bus call is made on the health path.
#[cfg(target_os = "linux")]
pub(crate) async fn worker_slice_memory_pressure() -> Option<WorkerSliceMemoryPressure> {
    let reading = async {
        let cgroup = tokio::fs::read_to_string("/proc/self/cgroup").await.ok()?;
        // SAFETY: geteuid has no preconditions and does not mutate state.
        let uid = unsafe { nix::libc::geteuid() };
        let path = worker_slice_cgroup_path(&cgroup, uid).ok()?;
        read_worker_pressure_at(&path).await
    }
    .await;
    let mut previous = HIGH_EVENT_SAMPLE
        .get_or_init(|| tokio::sync::Mutex::new(None))
        .lock()
        .await;
    let Some((high_events, full_avg60)) = reading else {
        *previous = None;
        return None;
    };
    let (high_events_per_minute, next) =
        high_events_per_minute(*previous, (high_events, Instant::now()));
    *previous = Some(next);
    Some(WorkerSliceMemoryPressure {
        high_events,
        high_events_per_minute,
        full_avg60,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) async fn worker_slice_memory_pressure() -> Option<WorkerSliceMemoryPressure> {
    None
}

/// Kill the exact private process group and reap its direct child. Provider
/// children use this for forced termination; tool capture has its own bounded
/// group settlement path.
pub(crate) async fn kill_worker_child(child: &mut tokio::process::Child) -> Result<()> {
    if let Some(pid) = child.id() {
        let pgid = nix::unistd::Pid::from_raw(pid as i32);
        if let Err(error) = signal_process_group(pgid, nix::sys::signal::Signal::SIGKILL) {
            child.kill().await?;
            return Err(DaemonError::Process(format!(
                "worker process group kill failed: {error}"
            )));
        }
    }
    let _ = child.wait().await?;
    Ok(())
}

/// A configured command whose direct child is the synchronous `systemd-run`
/// scope launcher. Its process group is private, so the caller can signal the
/// wrapper and its descendants as one owned cohort. A distinct unit is used
/// for each command, including concurrent tools in one model invocation.
pub(crate) struct ScopedWorkerCommand {
    command: Command,
    unit_name: String,
}

impl ScopedWorkerCommand {
    pub(crate) fn new(
        program: &OsStr,
        invocation_id: Uuid,
        limits: WorkerScopeLimits,
    ) -> Result<Self> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (invocation_id, limits);
            return Ok(Self {
                command: Command::new(program),
                unit_name: String::new(),
            });
        }
        #[cfg(target_os = "linux")]
        {
            #[cfg(test)]
            {
                return Self::with_runner(test_runner(), program, invocation_id, limits);
            }
            #[cfg(not(test))]
            {
                limits.verify_parent()?;
                Self::with_runner(Path::new(SYSTEMD_RUN), program, invocation_id, limits)
            }
        }
    }

    fn with_runner(
        runner: &Path,
        program: &OsStr,
        invocation_id: Uuid,
        limits: WorkerScopeLimits,
    ) -> Result<Self> {
        if program.is_empty() {
            return Err(DaemonError::InvalidParam(
                "worker scope program is empty".into(),
            ));
        }
        let unit_name = format!("rsi-worker-{invocation_id}-{}.scope", Uuid::new_v4());
        let mut command = Command::new(runner);
        command
            .arg("--user")
            .arg("--scope")
            .arg("--collect")
            .arg("--quiet")
            // systemd-run otherwise expands `${NAME}` inside a provider
            // argument. Prompts and tool commands must cross byte-for-byte.
            .arg("--expand-environment=no")
            .arg(format!("--slice={SCOPE_SLICE}"))
            .arg(format!("--unit={unit_name}"))
            .arg(format!("--property=MemoryHigh={}M", limits.memory_high_mib))
            .arg(format!("--property=MemoryMax={}M", limits.memory_max_mib))
            .arg(format!(
                "--property=MemorySwapMax={}M",
                limits.memory_swap_max_mib
            ))
            .arg(format!("--property=CPUWeight={}", limits.cpu_weight))
            .arg("--")
            .arg(program);
        configure_tokio_process_group(&mut command, ProcessContainment::Group)?;
        Ok(Self { command, unit_name })
    }

    pub(crate) fn command_mut(&mut self) -> &mut Command {
        &mut self.command
    }

    pub(crate) fn unit_name(&self) -> &str {
        &self.unit_name
    }

    pub(crate) fn into_command(self) -> Command {
        self.command
    }

    /// Wrap an already constructed provider command before its stdio is set.
    /// Preserve its program, argv, cwd, and explicit environment operations.
    pub(crate) fn wrap_unspawned(inner: &Command, invocation_id: Uuid) -> Result<Command> {
        let source = inner.as_std();
        let limits = WorkerScopeLimits::from_launcher_snapshot()?;
        let mut scoped = Self::new(source.get_program(), invocation_id, limits)?.into_command();
        scoped.args(source.get_args());
        if let Some(dir) = source.get_current_dir() {
            scoped.current_dir(dir);
        }
        for (key, value) in source.get_envs() {
            match value {
                Some(value) => {
                    scoped.env(key, value);
                }
                None => {
                    scoped.env_remove(key);
                }
            }
        }
        Ok(scoped)
    }
}

#[cfg(test)]
fn test_runner() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/process_scope_direct_runner.sh"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_control::signal_process_group;
    use nix::sys::signal::Signal;
    use std::ffi::OsString;
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    fn limits() -> WorkerScopeLimits {
        WorkerScopeLimits::new(512, 1024, 0, 50).unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn parent_slice_mismatch_refuses_worker() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("memory.high"), "536870912\n").unwrap();
        std::fs::write(dir.path().join("memory.max"), "1073741824\n").unwrap();
        std::fs::write(dir.path().join("memory.swap.max"), "0\n").unwrap();
        std::fs::write(dir.path().join("cpu.weight"), "50\n").unwrap();
        assert!(limits().verify_parent_files(dir.path()).is_ok());
        std::fs::write(dir.path().join("memory.max"), "max\n").unwrap();
        assert!(limits().verify_parent_files(dir.path()).is_err());
        std::fs::remove_file(dir.path().join("memory.high")).unwrap();
        assert!(limits().verify_parent_files(dir.path()).is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn parent_path_is_bound_to_this_user_manager() {
        let cgroup =
            "0::/user.slice/user-1000.slice/user@1000.service/user.slice/rsid-tui-a.scope\n";
        assert_eq!(
            worker_slice_cgroup_path(cgroup, 1000).unwrap(),
            Path::new(
                "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/rsi.slice/rsi-workers.slice"
            )
        );
        assert!(worker_slice_cgroup_path(cgroup, 1001).is_err());
        assert!(worker_slice_cgroup_path("1:memory:/user.slice\n", 1000).is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn launcher_snapshot_requires_complete_worker_policy() {
        let legacy = "rsid_scope_memory_high_mib=6144\nrsid_scope_memory_max_mib=8192\nrsid_scope_memory_swap_max_mib=0\nrsid_scope_cpu_weight=20\n";
        assert_eq!(
            WorkerScopeLimits::parse_launcher_snapshot(legacy).unwrap(),
            {
                let (high, max) = rsi_common::worker_memory::default_limits_mib();
                WorkerScopeLimits::new(high, max, 0, 20).unwrap()
            }
        );
        let partial = format!("{legacy}worker_scope_memory_high_mib=512\n");
        assert!(WorkerScopeLimits::parse_launcher_snapshot(&partial).is_err());
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn worker_pressure_parsers_select_high_and_full_avg60() {
        assert_eq!(
            parse_high_events("low 9\nhigh 42\nmax 3\noom 0\n"),
            Some(42)
        );
        assert_eq!(parse_high_events("high 1\nhigh 2\n"), None);
        assert_eq!(parse_high_events("low 1\n"), None);
        assert_eq!(parse_high_events("high nope\n"), None);

        let pressure = "some avg10=1.00 avg60=2.00 avg300=3.00 total=10\nfull avg10=0.10 avg60=0.25 avg300=0.30 total=2\n";
        assert_eq!(parse_full_avg60(pressure), Some(0.25));
        assert_eq!(parse_full_avg60("some avg60=2.00\n"), None);
        assert_eq!(parse_full_avg60("full avg60=NaN\n"), None);
        assert_eq!(parse_full_avg60("full avg60=101.00\n"), None);
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn worker_high_event_rate_uses_monotonic_samples_and_reseeds_on_reset() {
        let start = Instant::now();
        let first = (10, start);
        assert_eq!(high_events_per_minute(None, first), (None, first));
        let second = (13, start + std::time::Duration::from_secs(30));
        assert_eq!(
            high_events_per_minute(Some(first), second),
            (Some(6.0), second)
        );
        assert_eq!(
            high_events_per_minute(Some(second), (14, second.1)),
            (None, second)
        );
        let reset = (2, second.1 + std::time::Duration::from_secs(60));
        assert_eq!(high_events_per_minute(Some(second), reset), (None, reset));
        let after_reset = (4, reset.1 + std::time::Duration::from_secs(60));
        assert_eq!(
            high_events_per_minute(Some(reset), after_reset),
            (Some(2.0), after_reset)
        );
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn worker_pressure_files_are_optional() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_worker_pressure_at(dir.path()).await, None);
        std::fs::write(dir.path().join("memory.events"), "high 7\n").unwrap();
        std::fs::write(dir.path().join("memory.pressure"), "full avg60=0.50\n").unwrap();
        assert_eq!(read_worker_pressure_at(dir.path()).await, Some((7, 0.5)));
        std::fs::write(dir.path().join("memory.pressure"), "full avg60=NaN\n").unwrap();
        assert_eq!(read_worker_pressure_at(dir.path()).await, None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn wrapped_provider_preserves_argv_environment_and_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let mut inner = Command::new("sh");
        inner
            .args(["-c", "printf '%s|%s' \"$RSI_TEST_SCOPE_ARG\" \"$PWD\""])
            .env("RSI_TEST_SCOPE_ARG", "alpha beta")
            .current_dir(dir.path());
        let output = ScopedWorkerCommand::wrap_unspawned(&inner, Uuid::new_v4())
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("alpha beta|{}", dir.path().display())
        );
    }

    fn fake_runner() -> &'static Path {
        Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/process_scope_group_runner.sh"
        ))
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn explicit_limits_and_unique_sibling_units() {
        assert!(WorkerScopeLimits::new(512, 512, 0, 50).is_err());
        assert!(WorkerScopeLimits::new(0, 1024, 0, 50).is_err());
        assert!(WorkerScopeLimits::new(512, 1024, MAX_MEMORY_MIB + 1, 50).is_err());
        assert!(WorkerScopeLimits::new(512, 1024, 0, 0).is_err());
        let invocation = Uuid::new_v4();
        let first = ScopedWorkerCommand::with_runner(
            Path::new("/fake/systemd-run"),
            OsStr::new("fake-provider"),
            invocation,
            limits(),
        )
        .unwrap();
        let second = ScopedWorkerCommand::with_runner(
            Path::new("/fake/systemd-run"),
            OsStr::new("fake-provider"),
            invocation,
            limits(),
        )
        .unwrap();
        assert_ne!(first.unit_name(), second.unit_name());
        assert!(
            first
                .unit_name()
                .starts_with(&format!("rsi-worker-{invocation}-"))
        );
        assert!(first.unit_name().ends_with(".scope"));
        let args: Vec<OsString> = first
            .command
            .as_std()
            .get_args()
            .map(OsStr::to_os_string)
            .collect();
        let args: Vec<&str> = args.iter().map(|arg| arg.to_str().unwrap()).collect();
        assert_eq!(
            &args[..6],
            [
                "--user",
                "--scope",
                "--collect",
                "--quiet",
                "--expand-environment=no",
                "--slice=rsi-workers.slice"
            ]
        );
        assert_eq!(args[7], "--property=MemoryHigh=512M");
        assert_eq!(args[8], "--property=MemoryMax=1024M");
        assert_eq!(args[9], "--property=MemorySwapMax=0M");
        assert_eq!(args[10], "--property=CPUWeight=50");
        assert_eq!(&args[11..], ["--", "fake-provider"]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn fake_scope_keeps_direct_child_streams_and_exit_status() {
        let dir = tempfile::tempdir().unwrap();
        let runner = fake_runner();
        let mut scoped =
            ScopedWorkerCommand::with_runner(runner, OsStr::new("sh"), Uuid::new_v4(), limits())
                .unwrap();
        scoped
            .command_mut()
            .args([
                "-c",
                "read line; printf 'out:%s\\n' \"$line\"; printf 'err\\n' >&2; exit 7",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = scoped.into_command().spawn().unwrap();
        let pid = child.id().unwrap();
        assert_eq!(
            nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(pid as i32)))
                .unwrap()
                .as_raw(),
            pid as i32
        );
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"hello\n").await.unwrap();
        drop(stdin);
        let output = child.wait_with_output().await.unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, b"out:hello\n");
        assert_eq!(output.stderr, b"err\n");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn failed_scope_launcher_does_not_run_the_provider() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("provider-ran");
        // A stable failing executable keeps this test about provider admission.
        let mut scoped = ScopedWorkerCommand::with_runner(
            Path::new("/usr/bin/false"),
            OsStr::new("sh"),
            Uuid::new_v4(),
            limits(),
        )
        .unwrap();
        scoped.command_mut().args([
            "-c",
            "printf ran > \"$1\"",
            "provider",
            marker.to_str().unwrap(),
        ]);
        let status = scoped.into_command().spawn().unwrap().wait().await.unwrap();
        assert_eq!(status.code(), Some(1));
        assert!(!marker.exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn fake_scope_interrupts_the_owned_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let runner = fake_runner();
        let marker = dir.path().join("provider-ready");
        let child_pid_file = dir.path().join("provider-pid");
        let mut scoped =
            ScopedWorkerCommand::with_runner(runner, OsStr::new("sh"), Uuid::new_v4(), limits())
                .unwrap();
        scoped.command_mut().args([
            "-c",
            "printf ready > \"$1\"; exec sleep 30",
            "provider",
            marker.to_str().unwrap(),
        ]);
        scoped
            .command_mut()
            .env("FAKE_CHILD_PID_FILE", &child_pid_file);
        let mut child = scoped.into_command().spawn().unwrap();
        let pgid = nix::unistd::Pid::from_raw(child.id().unwrap() as i32);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !marker.exists() || !child_pid_file.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let child_pid: i32 = std::fs::read_to_string(&child_pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        signal_process_group(pgid, Signal::SIGTERM).unwrap();
        let status = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
        assert_eq!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(child_pid), None),
            Err(nix::errno::Errno::ESRCH)
        );
    }
}
