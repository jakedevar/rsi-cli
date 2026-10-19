//! Process-start clock for the startup milestones (#2375).
//!
//! `total_elapsed_ms` counts from the first line of `prepare_daemon`, which
//! already ran after the kernel exec and the dynamic loader. The kernel's own
//! process start time is the one clock a supervisor regression (a slow relaunch
//! after `Shutdown complete`) cannot hide in, so the `request_ready` milestone
//! also logs `process_elapsed_ms` from it. Linux reads `/proc/self/stat`;
//! elsewhere the value is absent and the field is omitted.

/// Milliseconds since this process started, or `None` where it cannot be read.
#[must_use]
pub fn process_elapsed_ms() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        let uptime = std::fs::read_to_string("/proc/uptime").ok()?;
        let ticks_per_second = nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK)
            .ok()
            .flatten()?;
        elapsed_ms_from(&stat, &uptime, u64::try_from(ticks_per_second).ok()?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Names the awaited startup steps that cost time (#2375). Every `mark` logs
/// the milliseconds since the previous mark under `startup_span`; a step under
/// [`SPAN_LOG_FLOOR_MS`] is logged at debug so the log names only what the
/// critical path actually spends. The spans tile `run_daemon`, so a gap that
/// has no span is a step nobody has labelled yet.
pub struct StartupSpans {
    last: std::time::Instant,
}

/// Spans at or above this are logged at info.
pub const SPAN_LOG_FLOOR_MS: u64 = 10;

impl StartupSpans {
    #[must_use]
    pub fn new() -> Self {
        Self {
            last: std::time::Instant::now(),
        }
    }

    /// Ends the span `name` now and returns its length in milliseconds.
    pub fn mark(&mut self, name: &'static str) -> u64 {
        let now = std::time::Instant::now();
        let elapsed_ms = now
            .duration_since(self.last)
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        self.last = now;
        if elapsed_ms >= SPAN_LOG_FLOOR_MS {
            tracing::info!(startup_span = name, elapsed_ms, "Startup step finished");
        } else {
            tracing::debug!(startup_span = name, elapsed_ms, "Startup step finished");
        }
        elapsed_ms
    }
}

impl Default for StartupSpans {
    fn default() -> Self {
        Self::new()
    }
}

/// Pure core: `/proc/<pid>/stat` field 22 is the start time in clock ticks
/// since boot; `/proc/uptime` is the seconds since boot.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn elapsed_ms_from(stat: &str, uptime: &str, ticks_per_second: u64) -> Option<u64> {
    if ticks_per_second == 0 {
        return None;
    }
    let start_ticks = crate::governor::parse_start_ticks(stat)?;
    let uptime_secs: f64 = uptime.split_whitespace().next()?.parse().ok()?;
    let start_secs = start_ticks as f64 / ticks_per_second as f64;
    let elapsed_ms = (uptime_secs - start_secs) * 1000.0;
    (elapsed_ms.is_finite() && elapsed_ms >= 0.0).then_some(elapsed_ms as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Field 22 (starttime) is 1200 ticks at 100 Hz = 12 s after boot.
    const STAT: &str = "42 (rsid (x) y) S 1 42 42 0 -1 4194560 100 0 0 0 1 1 0 0 20 0 5 0 1200 1000 100 18446744073709551615";

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn spans_measure_from_the_previous_mark() {
        let mut spans = StartupSpans::new();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(spans.mark("first") >= 30);
        assert!(
            spans.mark("second") < 30,
            "a span starts at the previous mark"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn elapsed_is_uptime_minus_start_time() {
        assert_eq!(elapsed_ms_from(STAT, "14.25 99.00\n", 100), Some(2250));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn unreadable_inputs_give_none() {
        assert_eq!(elapsed_ms_from(STAT, "14.25 1", 0), None);
        assert_eq!(elapsed_ms_from("garbage", "14.25 1", 100), None);
        assert_eq!(elapsed_ms_from(STAT, "x", 100), None);
        // A start time after the uptime sample is clock skew, never negative.
        assert_eq!(elapsed_ms_from(STAT, "3.0 1", 100), None);
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn this_process_has_a_start_time() {
        assert!(process_elapsed_ms().is_some());
    }
}
