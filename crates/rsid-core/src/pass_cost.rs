//! Per-pass cost accounting for background maintenance lanes (hub #2940).
//!
//! A maintenance pass (archived-sandbox purge, absent-root adoption, session
//! retention) used to log at most a duration, so a burst of daemon CPU could
//! not be attributed to a lane or to a proof stage. [`ThreadMeter`] measures
//! one span of work on ONE thread (wall clock, thread CPU time and the bytes
//! the thread handed to `read(2)`); [`StageCosts`] sums spans per named stage
//! so a lane logs a single bounded summary line per pass, never a line per
//! candidate.
//!
//! The meter is thread-bound on purpose: a pass that hops threads (`.await`
//! across `spawn_blocking`) meters each blocking closure and adds the
//! results, so the sum is the pass's own work and never another lane's
//! concurrent CPU. Thread CPU time and `/proc/thread-self/io` exist on Linux
//! only; every other platform reports `None` for those and keeps the wall
//! clock.

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::time::{Duration, Instant};

/// Most distinct stage names one [`StageCosts`] keeps. Names are `&'static`
/// literals chosen by the lane, so this only guards a future caller that
/// would key by something unbounded.
pub const MAX_STAGES: usize = 24;

/// What one span of work cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cost {
    pub wall: Duration,
    /// Thread CPU time; `None` where the platform does not report it.
    pub cpu: Option<Duration>,
    /// Bytes the thread handed to `read(2)`-family calls (`rchar`).
    pub read_bytes: Option<u64>,
    /// `read(2)`-family calls the thread made (`syscr`).
    pub read_calls: Option<u64>,
}

fn add_optional<T: std::ops::Add<Output = T>>(left: Option<T>, right: Option<T>) -> Option<T> {
    match (left, right) {
        (Some(a), Some(b)) => Some(a + b),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

impl Cost {
    /// Add `other` into `self`; a side with no measurement adds nothing.
    pub fn add(&mut self, other: Self) {
        self.wall += other.wall;
        self.cpu = add_optional(self.cpu, other.cpu);
        self.read_bytes = add_optional(self.read_bytes, other.read_bytes);
        self.read_calls = add_optional(self.read_calls, other.read_calls);
    }

    /// Thread CPU in whole milliseconds (0 where unreported).
    #[must_use]
    pub fn cpu_ms(&self) -> u64 {
        self.cpu.map_or(0, |cpu| cpu.as_millis() as u64)
    }

    /// Wall clock in whole milliseconds.
    #[must_use]
    pub fn wall_ms(&self) -> u64 {
        self.wall.as_millis() as u64
    }

    /// Bytes read, in KiB (0 where unreported).
    #[must_use]
    pub fn read_kib(&self) -> u64 {
        self.read_bytes.map_or(0, |bytes| bytes / 1024)
    }
}

/// One thread's cumulative I/O counters.
#[derive(Debug, Clone, Copy)]
struct ThreadIo {
    rchar: u64,
    syscr: u64,
}

/// Measures a span of work on the thread that created it.
///
/// `!Send`: moving the meter to another thread would subtract two different
/// threads' counters.
pub struct ThreadMeter {
    wall: Instant,
    cpu: Option<Duration>,
    io: Option<ThreadIo>,
    _thread_bound: PhantomData<*const ()>,
}

thread_local! {
    static METER_ENDPOINTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many [`ThreadMeter`] endpoints (a start or a finish, each a thread CPU
/// clock call and a `/proc` read) this thread has taken. A test reads it around
/// a guarded section to prove the section took none.
#[must_use]
pub fn thread_meter_endpoints() -> u64 {
    METER_ENDPOINTS.with(std::cell::Cell::get)
}

fn count_meter_endpoint() {
    METER_ENDPOINTS.with(|count| count.set(count.get() + 1));
}

impl ThreadMeter {
    #[must_use]
    pub fn start() -> Self {
        count_meter_endpoint();
        Self {
            wall: Instant::now(),
            cpu: thread_cpu_time(),
            io: thread_io(),
            _thread_bound: PhantomData,
        }
    }

    /// The cost since [`ThreadMeter::start`].
    #[must_use]
    pub fn finish(self) -> Cost {
        count_meter_endpoint();
        let wall = self.wall.elapsed();
        let cpu = match (self.cpu, thread_cpu_time()) {
            (Some(before), Some(after)) => Some(after.saturating_sub(before)),
            _ => None,
        };
        let (read_bytes, read_calls) = match (self.io, thread_io()) {
            (Some(before), Some(after)) => (
                Some(after.rchar.saturating_sub(before.rchar)),
                Some(after.syscr.saturating_sub(before.syscr)),
            ),
            _ => (None, None),
        };
        Cost {
            wall,
            cpu,
            read_bytes,
            read_calls,
        }
    }

    /// Run `work` on this thread and return its result with its cost.
    pub fn measure<T>(work: impl FnOnce() -> T) -> (T, Cost) {
        let meter = Self::start();
        let output = work();
        (output, meter.finish())
    }
}

/// Meters work that runs while a lock guard is held without adding a single
/// meter endpoint (thread CPU clock call, `/proc` read) to the critical
/// section (hub #2973).
///
/// [`LockedSection::start`] takes the meter's start endpoint BEFORE the lock is
/// taken; inside the lock only plain [`Instant`] reads happen
/// ([`LockedSection::acquired`], [`LockedSection::work`]); and
/// [`LockedSection::finish`] takes the finish endpoint AFTER the guard is
/// dropped. Wall time is exact per span. Thread CPU and reads are the section's
/// total, from before the lock was requested to after it was released (a thread
/// blocked on the lock spends none), and are attributed to the work stage.
pub struct LockedSection {
    meter: ThreadMeter,
    requested: Instant,
    wait: Option<Duration>,
    work: Option<Duration>,
}

impl LockedSection {
    /// Take the start endpoint. Call before requesting the lock.
    #[must_use]
    pub fn start() -> Self {
        Self {
            meter: ThreadMeter::start(),
            requested: Instant::now(),
            wait: None,
            work: None,
        }
    }

    /// The lock was just granted: record the wait (plain clock read only).
    pub fn acquired(&mut self) {
        self.wait = Some(self.requested.elapsed());
    }

    /// Run `work` (under the guard) and add its wall time to the work stage.
    pub fn work<T>(&mut self, work: impl FnOnce() -> T) -> T {
        let begun = Instant::now();
        let output = work();
        self.work = Some(self.work.unwrap_or_default() + begun.elapsed());
        output
    }

    /// Take the finish endpoint, with the guard already dropped, and record
    /// `wait_stage` (wall only) and `work_stage` into `stages`. A stage whose
    /// span never ran (the lock was refused) records nothing.
    pub fn finish(
        self,
        stages: &mut StageCosts,
        wait_stage: &'static str,
        work_stage: &'static str,
    ) {
        let total = self.meter.finish();
        if let Some(wait) = self.wait {
            stages.record(
                wait_stage,
                Cost {
                    wall: wait,
                    ..Cost::default()
                },
            );
        }
        if let Some(work) = self.work {
            stages.record(
                work_stage,
                Cost {
                    wall: work,
                    ..total
                },
            );
        }
    }
}

/// Measures a span of work across threads and `.await` points: the whole
/// daemon process's CPU and reads between [`ProcessMeter::start`] and
/// [`ProcessMeter::finish`]. It is an UPPER BOUND for the pass it brackets,
/// since every other lane running at the same time is counted too; use it for
/// an async pass that cannot be pinned to one thread, and read it against the
/// idle baseline.
pub struct ProcessMeter {
    wall: Instant,
    cpu: Option<Duration>,
    io: Option<ThreadIo>,
}

impl ProcessMeter {
    #[must_use]
    pub fn start() -> Self {
        Self {
            wall: Instant::now(),
            cpu: process_cpu_time(),
            io: process_io(),
        }
    }

    /// The process-wide cost since [`ProcessMeter::start`].
    #[must_use]
    pub fn finish(self) -> Cost {
        let wall = self.wall.elapsed();
        let cpu = match (self.cpu, process_cpu_time()) {
            (Some(before), Some(after)) => Some(after.saturating_sub(before)),
            _ => None,
        };
        let (read_bytes, read_calls) = match (self.io, process_io()) {
            (Some(before), Some(after)) => (
                Some(after.rchar.saturating_sub(before.rchar)),
                Some(after.syscr.saturating_sub(before.syscr)),
            ),
            _ => (None, None),
        };
        Cost {
            wall,
            cpu,
            read_bytes,
            read_calls,
        }
    }
}

#[cfg(target_os = "linux")]
fn clock_time(clock: nix::time::ClockId) -> Option<Duration> {
    let spec = nix::time::clock_gettime(clock).ok()?;
    let seconds = u64::try_from(spec.tv_sec()).ok()?;
    let nanos = u32::try_from(spec.tv_nsec()).ok()?;
    Some(Duration::new(seconds, nanos))
}

#[cfg(target_os = "linux")]
fn thread_cpu_time() -> Option<Duration> {
    clock_time(nix::time::ClockId::CLOCK_THREAD_CPUTIME_ID)
}

#[cfg(target_os = "linux")]
fn process_cpu_time() -> Option<Duration> {
    clock_time(nix::time::ClockId::CLOCK_PROCESS_CPUTIME_ID)
}

#[cfg(not(target_os = "linux"))]
fn process_cpu_time() -> Option<Duration> {
    None
}

#[cfg(target_os = "linux")]
fn process_io() -> Option<ThreadIo> {
    parse_thread_io(&std::fs::read_to_string("/proc/self/io").ok()?)
}

#[cfg(not(target_os = "linux"))]
fn process_io() -> Option<ThreadIo> {
    None
}

#[cfg(not(target_os = "linux"))]
fn thread_cpu_time() -> Option<Duration> {
    None
}

#[cfg(target_os = "linux")]
fn thread_io() -> Option<ThreadIo> {
    parse_thread_io(&std::fs::read_to_string("/proc/thread-self/io").ok()?)
}

#[cfg(not(target_os = "linux"))]
fn thread_io() -> Option<ThreadIo> {
    None
}

#[cfg(any(target_os = "linux", test))]
fn parse_thread_io(text: &str) -> Option<ThreadIo> {
    let (mut rchar, mut syscr) = (None, None);
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "rchar" => rchar = value.trim().parse::<u64>().ok(),
            "syscr" => syscr = value.trim().parse::<u64>().ok(),
            _ => {}
        }
    }
    Some(ThreadIo {
        rchar: rchar?,
        syscr: syscr?,
    })
}

/// One stage's total across a pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageCost {
    pub runs: u64,
    pub cost: Cost,
}

/// Cost summed per named stage; one bounded summary per pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StageCosts {
    stages: BTreeMap<&'static str, StageCost>,
}

impl StageCosts {
    /// Run `work` on this thread as stage `name` and record its cost.
    pub fn stage<T>(&mut self, name: &'static str, work: impl FnOnce() -> T) -> T {
        let (output, cost) = ThreadMeter::measure(work);
        self.record(name, cost);
        output
    }

    /// Record one run of stage `name`. A stage beyond [`MAX_STAGES`] distinct
    /// names is folded into `other` so a summary stays bounded.
    pub fn record(&mut self, name: &'static str, cost: Cost) {
        let key = if self.stages.contains_key(name) || self.stages.len() < MAX_STAGES {
            name
        } else {
            "other"
        };
        let entry = self.stages.entry(key).or_default();
        entry.runs += 1;
        entry.cost.add(cost);
    }

    /// Fold another pass's (or candidate's) stages into this one.
    pub fn merge(&mut self, other: &Self) {
        for (name, stage) in &other.stages {
            let key = if self.stages.contains_key(name) || self.stages.len() < MAX_STAGES {
                *name
            } else {
                "other"
            };
            let entry = self.stages.entry(key).or_default();
            entry.runs += stage.runs;
            entry.cost.add(stage.cost);
        }
    }

    /// Sum of every stage.
    #[must_use]
    pub fn total(&self) -> Cost {
        let mut total = Cost::default();
        for stage in self.stages.values() {
            total.add(stage.cost);
        }
        total
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&StageCost> {
        self.stages.get(name)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }

    /// `name=cpu_ms/wall_ms/runs` for every stage, in name order, for one log
    /// field. Bounded by [`MAX_STAGES`].
    #[must_use]
    pub fn summary(&self) -> String {
        self.stages
            .iter()
            .map(|(name, stage)| {
                format!(
                    "{name}={}/{}/{}",
                    stage.cost.cpu_ms(),
                    stage.cost.wall_ms(),
                    stage.runs
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn costs_add_and_a_missing_measurement_adds_nothing() {
        let mut total = Cost {
            wall: Duration::from_millis(5),
            cpu: Some(Duration::from_millis(2)),
            read_bytes: None,
            read_calls: None,
        };
        total.add(Cost {
            wall: Duration::from_millis(7),
            cpu: None,
            read_bytes: Some(2048),
            read_calls: Some(3),
        });
        assert_eq!(total.wall, Duration::from_millis(12));
        assert_eq!(total.cpu, Some(Duration::from_millis(2)));
        assert_eq!(total.read_bytes, Some(2048));
        assert_eq!(total.read_calls, Some(3));
        assert_eq!(total.read_kib(), 2);
    }

    #[test]
    fn thread_io_parses_the_counters_and_refuses_a_partial_file() {
        let text = "rchar: 10\nwchar: 2\nsyscr: 4\nsyscw: 1\nread_bytes: 0\n";
        let parsed = parse_thread_io(text).expect("both counters present");
        assert_eq!((parsed.rchar, parsed.syscr), (10, 4));
        assert!(parse_thread_io("wchar: 2\n").is_none());
    }

    #[test]
    fn stage_summary_orders_by_name_and_counts_runs() {
        let mut stages = StageCosts::default();
        stages.record("b_stage", Cost::default());
        stages.record("a_stage", Cost::default());
        stages.record("a_stage", Cost::default());
        assert_eq!(stages.summary(), "a_stage=0/0/2,b_stage=0/0/1");
        let mut merged = StageCosts::default();
        merged.merge(&stages);
        merged.merge(&stages);
        assert_eq!(merged.get("a_stage").map(|stage| stage.runs), Some(4));
    }

    #[test]
    fn stage_names_are_bounded_by_folding_into_other() {
        let names: [&'static str; MAX_STAGES + 3] = [
            "s00", "s01", "s02", "s03", "s04", "s05", "s06", "s07", "s08", "s09", "s10", "s11",
            "s12", "s13", "s14", "s15", "s16", "s17", "s18", "s19", "s20", "s21", "s22", "s23",
            "s24", "s25", "s26",
        ];
        let mut stages = StageCosts::default();
        for name in names {
            stages.record(name, Cost::default());
        }
        assert_eq!(stages.get("other").map(|stage| stage.runs), Some(3));
        assert!(stages.get("s25").is_none());
        assert_eq!(stages.summary().split(',').count(), MAX_STAGES + 1);
    }

    /// A guard that records the thread's meter endpoint count when it drops.
    struct ProbeGuard<'a> {
        at_drop: &'a std::cell::Cell<u64>,
    }

    impl Drop for ProbeGuard<'_> {
        fn drop(&mut self) {
            self.at_drop.set(thread_meter_endpoints());
        }
    }

    #[test]
    fn a_locked_section_takes_no_meter_endpoint_while_the_guard_is_held() {
        let at_drop = std::cell::Cell::new(0);
        let mut stages = StageCosts::default();
        let mut section = LockedSection::start();
        let before_lock = thread_meter_endpoints();
        let at_acquire;
        let after_work;
        {
            let _guard = ProbeGuard { at_drop: &at_drop };
            section.acquired();
            at_acquire = thread_meter_endpoints();
            section.work(|| std::hint::black_box(1 + 1));
            after_work = thread_meter_endpoints();
        }
        assert_eq!(at_acquire, before_lock, "the lock grant read no meter");
        assert_eq!(
            after_work, before_lock,
            "the work under the guard read no meter"
        );
        assert_eq!(
            at_drop.get(),
            before_lock,
            "no endpoint until the guard dropped"
        );
        section.finish(&mut stages, "store_lock", "dependency_proof");
        assert_eq!(thread_meter_endpoints(), before_lock + 1, "finish is after");
        assert_eq!(stages.get("store_lock").map(|stage| stage.runs), Some(1));
        assert_eq!(
            stages.get("dependency_proof").map(|stage| stage.runs),
            Some(1)
        );
    }

    #[test]
    fn a_refused_lock_records_no_stage() {
        let mut stages = StageCosts::default();
        LockedSection::start().finish(&mut stages, "store_lock", "dependency_proof");
        assert!(stages.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_span_counts_cpu_spent_on_other_threads() {
        let meter = ProcessMeter::start();
        std::thread::spawn(|| {
            let begin = thread_cpu_time().expect("linux thread cpu clock");
            while thread_cpu_time().expect("clock").saturating_sub(begin) < Duration::from_millis(5)
            {
                std::hint::black_box(0_u64);
            }
        })
        .join()
        .expect("busy thread");
        let cost = meter.finish();
        assert!(cost.cpu.expect("process cpu measured") >= Duration::from_millis(5));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_busy_span_reports_thread_cpu_and_a_file_read_reports_bytes() {
        use std::io::Read;
        let (_, busy) = ThreadMeter::measure(|| {
            let started = Instant::now();
            let begin = thread_cpu_time().expect("linux thread cpu clock");
            while thread_cpu_time().expect("clock").saturating_sub(begin) < Duration::from_millis(3)
                && started.elapsed() < Duration::from_secs(30)
            {
                std::hint::black_box(0_u64);
            }
        });
        assert!(busy.cpu.expect("cpu measured") >= Duration::from_millis(3));
        assert!(busy.wall >= Duration::from_millis(3));

        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("payload");
        std::fs::write(&path, vec![7_u8; 64 * 1024]).expect("write payload");
        let (_, read) = ThreadMeter::measure(|| {
            let mut buffer = Vec::new();
            std::fs::File::open(&path)
                .expect("open payload")
                .read_to_end(&mut buffer)
                .expect("read payload");
            buffer.len()
        });
        // `/proc/thread-self/io` needs task I/O accounting; where the kernel
        // lacks it the counters are unreported, which is not a failure here.
        if let Some(bytes) = read.read_bytes {
            assert!(bytes >= 64 * 1024, "read {bytes} bytes");
            assert!(read.read_calls.unwrap_or(0) >= 1);
        }
    }
}
