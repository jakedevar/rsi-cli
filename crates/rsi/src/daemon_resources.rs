//! rsid resource usage for the session browser's live-activity pane.
//!
//! Memory and queue depths come from `GetHealthStatus`, which the TUI already
//! reads on its bounded background cadence. CPU time, threads, open
//! descriptors and uptime come from the daemon's own `/proc/<pid>` counters:
//! the PID is the Unix-socket peer of that same Health connection, so no new
//! RPC is needed. Every field is optional; a host without `/proc` (or a daemon
//! in another PID namespace) simply shows less.

use std::time::Instant;

use rsi_common::rpc::HealthStatusResponse;

/// `/proc/<pid>/stat` reports CPU times in `USER_HZ` clock ticks, which the
/// Linux ABI fixes at 100 on every mainstream architecture.
const CLOCK_TICKS_PER_SEC: f64 = 100.0;

/// Counters read from `/proc/<pid>/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcCounters {
    /// `utime + stime`, in clock ticks.
    pub cpu_ticks: u64,
    pub threads: u32,
    /// Process start, in clock ticks after boot. Distinguishes a restarted
    /// daemon that reused the PID.
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

/// One Health read plus the daemon's `/proc` counters at the same moment.
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
}

impl DaemonResourceSample {
    /// Combine a Health response with the `/proc` counters of `pid`.
    #[must_use]
    pub fn from_health(health: &HealthStatusResponse, pid: Option<u32>) -> Self {
        let proc_counters = pid
            .and_then(|pid| std::fs::read_to_string(format!("/proc/{pid}/stat")).ok())
            .and_then(|body| parse_proc_stat(&body));
        let open_fds = pid
            .and_then(|pid| std::fs::read_dir(format!("/proc/{pid}/fd")).ok())
            .and_then(|entries| u32::try_from(entries.count()).ok());
        let uptime_secs = proc_counters.and_then(|counters| {
            let since_boot = parse_proc_uptime(&std::fs::read_to_string("/proc/uptime").ok()?)?;
            #[allow(clippy::cast_precision_loss)]
            let started = counters.start_ticks as f64 / CLOCK_TICKS_PER_SEC;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let elapsed = (since_boot - started) as u64;
            (since_boot >= started).then_some(elapsed)
        });
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
}
