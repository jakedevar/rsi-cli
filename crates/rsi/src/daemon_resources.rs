//! rsid resource usage for the session browser's live-activity pane.
//!
//! Memory and queue depths come from `GetHealthStatus`, which the TUI already
//! reads on its bounded background cadence. CPU time, threads, open
//! descriptors and uptime come from the daemon's own OS counters
//! (`/proc/<pid>` on Linux, `proc_pidinfo` on macOS, #1229): the PID is the
//! Unix-socket peer of that same Health connection, so no new RPC is needed.
//! Every field is optional; another host (or a daemon in another PID
//! namespace) simply shows less.

use std::time::Instant;

use rsi_common::codegraph::CodegraphHealthNoticeV1;
use rsi_common::rpc::HealthStatusResponse;

/// `/proc/<pid>/stat` reports CPU times in `USER_HZ` clock ticks, which the
/// Linux ABI fixes at 100 on every mainstream architecture.
const CLOCK_TICKS_PER_SEC: f64 = 100.0;

/// Counters read from `/proc/<pid>/stat` (or `proc_pidinfo` on macOS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcCounters {
    /// `utime + stime`, in clock ticks.
    pub cpu_ticks: u64,
    pub threads: u32,
    /// Process start, in clock ticks after boot (after the Unix epoch on
    /// macOS). Only compared for equality: distinguishes a restarted daemon
    /// that reused the PID.
    pub start_ticks: u64,
}

/// Parse `/proc/<pid>/stat`. The command name may contain spaces and
/// parentheses, so fields are counted from the last `)`.
#[must_use]
pub fn parse_proc_stat(body: &str) -> Option<ProcCounters> {
    let rest = body.get(body.rfind(')')? + 1..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // `rest` starts at field 3 (state), so field N sits at index N - 3.
    let field = |number: usize| fields.get(number - 3)?.parse::<u64>().ok();
    Some(ProcCounters {
        cpu_ticks: field(14)?.checked_add(field(15)?)?,
        threads: u32::try_from(field(20)?).ok()?,
        start_ticks: field(22)?,
    })
}

/// Seconds since boot from `/proc/uptime` (`12345.67 54321.00`).
#[must_use]
pub fn parse_proc_uptime(body: &str) -> Option<f64> {
    body.split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
}

/// The OS-side counters of one process; every field optional.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct OsCounters {
    proc_counters: Option<ProcCounters>,
    open_fds: Option<u32>,
    uptime_secs: Option<u64>,
}

#[cfg(target_os = "linux")]
fn read_os_counters(pid: u32) -> OsCounters {
    let proc_counters = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|body| parse_proc_stat(&body));
    let open_fds = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .ok()
        .and_then(|entries| u32::try_from(entries.count()).ok());
    let uptime_secs = proc_counters.and_then(|counters| {
        let since_boot = parse_proc_uptime(&std::fs::read_to_string("/proc/uptime").ok()?)?;
        #[allow(clippy::cast_precision_loss)]
        let started = counters.start_ticks as f64 / CLOCK_TICKS_PER_SEC;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let elapsed = (since_boot - started) as u64;
        (since_boot >= started).then_some(elapsed)
    });
    OsCounters {
        proc_counters,
        open_fds,
        uptime_secs,
    }
}

#[cfg(target_os = "macos")]
fn read_os_counters(pid: u32) -> OsCounters {
    macos::read(pid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_os_counters(_pid: u32) -> OsCounters {
    OsCounters::default()
}

/// Seconds from a process start (`start_secs` after the Unix epoch) to `now`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn uptime_since_epoch_start(start_secs: u64, now: std::time::SystemTime) -> Option<u64> {
    now.duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs()
        .checked_sub(start_secs)
}

/// macOS counters from `proc_pidinfo` (#1229).
#[cfg(target_os = "macos")]
mod macos {
    use super::{OsCounters, ProcCounters, uptime_since_epoch_start};
    use std::mem::{MaybeUninit, size_of};

    /// One clock tick (1/100 s, the unit of [`ProcCounters`]) in nanoseconds.
    const NANOS_PER_TICK: u64 = 10_000_000;

    pub(super) fn read(pid: u32) -> OsCounters {
        let Ok(pid) = libc::c_int::try_from(pid) else {
            return OsCounters::default();
        };
        let info = task_all_info(pid);
        OsCounters {
            proc_counters: info.as_ref().and_then(counters),
            open_fds: open_fd_count(pid),
            uptime_secs: info.as_ref().and_then(|info| {
                uptime_since_epoch_start(info.pbsd.pbi_start_tvsec, std::time::SystemTime::now())
            }),
        }
    }

    fn task_all_info(pid: libc::c_int) -> Option<libc::proc_taskallinfo> {
        let mut info = MaybeUninit::<libc::proc_taskallinfo>::zeroed();
        let size = libc::c_int::try_from(size_of::<libc::proc_taskallinfo>()).ok()?;
        // SAFETY: the buffer is a properly sized and aligned
        // `proc_taskallinfo` that outlives the call; proc_pidinfo writes at
        // most `size` bytes into it.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTASKALLINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        // SAFETY: proc_pidinfo filled the whole struct (checked first).
        (written == size).then(|| unsafe { info.assume_init() })
    }

    fn counters(info: &libc::proc_taskallinfo) -> Option<ProcCounters> {
        let cpu = info
            .ptinfo
            .pti_total_user
            .checked_add(info.ptinfo.pti_total_system)?;
        Some(ProcCounters {
            cpu_ticks: mach_time_to_nanos(cpu)? / NANOS_PER_TICK,
            threads: u32::try_from(info.ptinfo.pti_threadnum).ok()?,
            start_ticks: info
                .pbsd
                .pbi_start_tvsec
                .checked_mul(100)?
                .checked_add(info.pbsd.pbi_start_tvusec / 10_000)?,
        })
    }

    /// Task CPU times are in Mach absolute-time units, which are not
    /// nanoseconds on Apple silicon (24 MHz ticks there). `mach_timebase_info`
    /// is deprecated in libc in favour of the `mach2` crate; the symbol is
    /// stable in libSystem and one call needs no new dependency.
    #[allow(deprecated)]
    fn mach_time_to_nanos(time: u64) -> Option<u64> {
        let mut timebase = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: mach_timebase_info only writes the struct it is given.
        let status = unsafe { libc::mach_timebase_info(&raw mut timebase) };
        if status != 0 || timebase.denom == 0 {
            return None;
        }
        u64::try_from(u128::from(time) * u128::from(timebase.numer) / u128::from(timebase.denom))
            .ok()
    }

    fn open_fd_count(pid: libc::c_int) -> Option<u32> {
        // SAFETY: a null buffer asks only for the size the list needs.
        let needed =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        let entry = size_of::<libc::proc_fdinfo>();
        // A few descriptors may open between the two calls.
        let capacity = usize::try_from(needed).ok().filter(|&n| n > 0)? / entry + 16;
        let mut buffer = Vec::<libc::proc_fdinfo>::with_capacity(capacity);
        let bytes = libc::c_int::try_from(capacity.checked_mul(entry)?).ok()?;
        // SAFETY: the buffer holds `capacity` entries (`bytes` bytes) and
        // outlives the call; only the returned byte count is read.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                buffer.as_mut_ptr().cast(),
                bytes,
            )
        };
        u32::try_from(usize::try_from(written).ok()? / entry).ok()
    }
}

/// One Health read plus the daemon's OS counters at the same moment.
#[derive(Debug, Clone, PartialEq)]
pub struct DaemonResourceSample {
    pub sampled_at: Instant,
    pub pid: Option<u32>,
    pub proc_counters: Option<ProcCounters>,
    pub open_fds: Option<u32>,
    pub uptime_secs: Option<u64>,
    pub rss_bytes: Option<u64>,
    pub heap_in_use_bytes: Option<u64>,
    pub persistence_queue_depth: usize,
    pub persistence_queue_capacity: usize,
    pub last_command_duration_ms: u64,
    pub queue_pending: i64,
    pub queue_claimed: i64,
    pub queue_failed: i64,
    /// `rsid-supervisor.sh` or `none` (#1217); absent when the daemon cannot say.
    pub supervisor_mode: Option<String>,
    /// Projects whose codegraph has been Degraded or Failed for over 24 hours
    /// (#2246); empty when every graph is healthy.
    pub codegraph_unhealthy: Vec<CodegraphHealthNoticeV1>,
}

impl DaemonResourceSample {
    /// The daemon positively reports it is not under `rsid-supervisor.sh`, so
    /// managed deploys are refused until `make release-install` restarts it.
    #[must_use]
    pub fn is_unsupervised(&self) -> bool {
        self.supervisor_mode.as_deref()
            == Some(rsi_common::agent_daemon_info::DAEMON_SUPERVISOR_NONE)
    }

    /// Combine a Health response with the OS counters of `pid`.
    #[must_use]
    pub fn from_health(health: &HealthStatusResponse, pid: Option<u32>) -> Self {
        let OsCounters {
            proc_counters,
            open_fds,
            uptime_secs,
        } = pid.map(read_os_counters).unwrap_or_default();
        let memory = health.process_memory.as_ref();
        Self {
            sampled_at: Instant::now(),
            pid,
            proc_counters,
            open_fds,
            uptime_secs,
            rss_bytes: memory.map(|memory| memory.rss_bytes),
            heap_in_use_bytes: memory.and_then(|memory| memory.heap_in_use_bytes),
            persistence_queue_depth: health.persistence_queue_depth,
            persistence_queue_capacity: health.persistence_queue_capacity,
            last_command_duration_ms: health.last_command_duration_ms,
            queue_pending: health.queue_pending,
            queue_claimed: health.queue_claimed,
            queue_failed: health.queue_failed,
            supervisor_mode: health.supervisor_mode.clone(),
            codegraph_unhealthy: health.codegraph_unhealthy.clone(),
        }
    }
}

/// What the activity pane renders: the latest sample plus CPU use over the
/// interval since the previous sample of the same daemon process.
#[derive(Debug, Clone, PartialEq)]
pub struct DaemonResourceView {
    pub sample: DaemonResourceSample,
    /// Percent of one core (may exceed 100 on several cores); `None` until
    /// two samples of one daemon process exist.
    pub cpu_percent: Option<f64>,
    /// #1122: the operator's pending quiet-point restart, one line.
    pub restart_pending: Option<String>,
}

impl DaemonResourceView {
    #[must_use]
    pub fn next(previous: Option<&Self>, sample: DaemonResourceSample) -> Self {
        let cpu_percent = previous.and_then(|previous| {
            let before = previous.sample.proc_counters?;
            let after = sample.proc_counters?;
            if previous.sample.pid != sample.pid || before.start_ticks != after.start_ticks {
                return None;
            }
            let wall = sample
                .sampled_at
                .checked_duration_since(previous.sample.sampled_at)?
                .as_secs_f64();
            if wall < 0.5 {
                return None;
            }
            #[allow(clippy::cast_precision_loss)]
            let ticks = after.cpu_ticks.checked_sub(before.cpu_ticks)? as f64;
            Some(ticks / CLOCK_TICKS_PER_SEC / wall * 100.0)
        });
        Self {
            sample,
            cpu_percent,
            restart_pending: previous.and_then(|previous| previous.restart_pending.clone()),
        }
    }

    /// The next refresh can come sooner while the CPU reading still needs
    /// its second sample.
    #[must_use]
    pub const fn awaiting_cpu(&self) -> bool {
        self.cpu_percent.is_none() && self.sample.proc_counters.is_some()
    }
}

/// `412M`, `1.2G`, `980K`.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        #[allow(clippy::cast_precision_loss)]
        let gib = bytes as f64 / GIB as f64;
        format!("{gib:.1}G")
    } else if bytes >= MIB {
        format!("{}M", bytes / MIB)
    } else {
        format!("{}K", bytes.div_ceil(KIB))
    }
}

/// `3d4h`, `3h12m`, `12m`, `<1m`.
#[must_use]
pub fn format_uptime(secs: u64) -> String {
    let (days, hours, minutes) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        "<1m".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_resources_parse_proc_stat_past_a_parenthesised_command_name() {
        let body = "4242 (rsid (main) x) S 1 4242 4242 0 -1 4194560 100 0 0 0 \
                    1500 250 0 0 20 0 38 0 987654 1000000 2000 18446744073709551615";
        let counters = parse_proc_stat(body).expect("stat fields");
        assert_eq!(counters.cpu_ticks, 1750);
        assert_eq!(counters.threads, 38);
        assert_eq!(counters.start_ticks, 987_654);
        assert_eq!(parse_proc_uptime("12345.67 54321.00\n"), Some(12345.67));
    }

    #[test]
    fn daemon_resources_flag_only_a_daemon_that_reports_no_supervisor() {
        let sample = |mode: Option<&str>| DaemonResourceSample {
            sampled_at: Instant::now(),
            pid: None,
            proc_counters: None,
            open_fds: None,
            uptime_secs: None,
            rss_bytes: None,
            heap_in_use_bytes: None,
            persistence_queue_depth: 0,
            persistence_queue_capacity: 0,
            last_command_duration_ms: 0,
            queue_pending: 0,
            queue_claimed: 0,
            queue_failed: 0,
            supervisor_mode: mode.map(str::to_string),
            codegraph_unhealthy: Vec::new(),
        };
        assert!(sample(Some("none")).is_unsupervised());
        assert!(!sample(Some("rsid-supervisor.sh")).is_unsupervised());
        // Unknown (older daemon, no /proc) is not a warning.
        assert!(!sample(None).is_unsupervised());
    }

    #[test]
    fn daemon_resources_cpu_percent_needs_two_samples_of_one_process() {
        let start = Instant::now();
        let sample = |offset_ms: u64, ticks: u64, pid: u32| DaemonResourceSample {
            sampled_at: start + std::time::Duration::from_millis(offset_ms),
            pid: Some(pid),
            proc_counters: Some(ProcCounters {
                cpu_ticks: ticks,
                threads: 4,
                start_ticks: 10,
            }),
            open_fds: None,
            uptime_secs: None,
            rss_bytes: None,
            heap_in_use_bytes: None,
            persistence_queue_depth: 0,
            persistence_queue_capacity: 0,
            last_command_duration_ms: 0,
            queue_pending: 0,
            queue_claimed: 0,
            queue_failed: 0,
            supervisor_mode: None,
            codegraph_unhealthy: Vec::new(),
        };
        let first = DaemonResourceView::next(None, sample(0, 100, 7));
        assert!(first.awaiting_cpu());
        let second = DaemonResourceView::next(Some(&first), sample(10_000, 150, 7));
        let cpu = second.cpu_percent.expect("cpu over ten seconds");
        assert!((cpu - 5.0).abs() < 1e-9, "50 ticks over 10 s is 5%: {cpu}");
        let restarted = DaemonResourceView::next(Some(&second), sample(20_000, 10, 8));
        assert!(restarted.awaiting_cpu());
        assert_eq!(format_bytes(412 * 1024 * 1024), "412M");
        assert_eq!(format_uptime(3 * 3600 + 12 * 60), "3h12m");
    }

    #[test]
    fn daemon_resources_uptime_counts_from_an_epoch_start() {
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        assert_eq!(uptime_since_epoch_start(999_000, now), Some(1000));
        assert_eq!(uptime_since_epoch_start(2_000_000, now), None);
    }

    /// #1229: the counters come from the OS on Linux and macOS alike; this
    /// test process stands in for the daemon.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[allow(clippy::expect_used)]
    fn daemon_resources_read_this_process_counters_from_the_os() {
        let pid = std::process::id();
        let before = read_os_counters(pid);
        let counters = before.proc_counters.expect("cpu and thread counters");
        assert!(counters.threads >= 1);
        assert!(before.open_fds.is_some_and(|fds| fds >= 1), "{before:?}");
        assert!(
            before.uptime_secs.is_some_and(|secs| secs < 86_400),
            "{before:?}"
        );
        // Burn 250 ms of CPU on this thread: the counter is in 1/100 s, so a
        // unit error (Mach ticks read as nanoseconds) would show ~0 here.
        let spin = Instant::now();
        let mut acc = 0_u64;
        while spin.elapsed() < std::time::Duration::from_millis(250) {
            acc = std::hint::black_box(acc.wrapping_add(1));
        }
        let after = read_os_counters(pid)
            .proc_counters
            .expect("counters after the spin");
        assert_eq!(after.start_ticks, counters.start_ticks);
        let spent = after.cpu_ticks - counters.cpu_ticks;
        assert!(spent >= 15, "250 ms of CPU is ~25 ticks, read {spent}");
    }
}
