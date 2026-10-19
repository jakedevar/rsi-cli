//! Operator setting `hub_build_cpu_percent` (#2119): one hub-wide CPU budget
//! for every cargo build and test unit RSI starts.
//!
//! Per-unit `CPUQuota` caps do not bound concurrency: N job units each taking
//! the host-derived ceiling can take N x that many cores and compete equally
//! with the operator's desktop. Every such unit therefore runs in one user
//! slice, [`SLICE`] (`rsi.slice/rsi-build.slice`; the dash in the name encodes
//! the parent), whose `CPUQuota` is `hub_build_cpu_percent` of the host's
//! logical CPUs and whose `CPUWeight` is [`CPU_WEIGHT`]. The slice is an
//! implicit systemd unit: this module writes its runtime properties
//! (idempotently, live when the unit is active) and each job's own ceiling is
//! derived from the same budget, so concurrent jobs share it.
//!
//! #2126: the parent slice [`PARENT_SLICE`] (`rsi.slice`, which holds the daemon,
//! `rsi-workers.slice` and the build slice) gets a low [`PARENT_CPU_WEIGHT`], the
//! same class as `background.slice`, so the operator's desktop (`app.slice`,
//! weight 100) wins CPU contention against everything RSI runs, and the SAME
//! hard `CPUQuota` as the build slice, so a cargo run in a worker's own scope
//! (`rsi-workers.slice`), which never enters the build slice, is bounded by the
//! budget too. The relative weights inside `rsi.slice` (workers 200, builds 20)
//! are unchanged.
//!
//! The store mirrors the live setting into this module; the daemon applies it
//! at start, on every accepted change and before it launches a build unit.
//! Linux (systemd user manager) only: elsewhere the report says no slice is
//! available and units run with their own limits. On Linux the build slice also
//! gets MemoryHigh at 60% and MemoryMax at 75% of physical host RAM (hub #3084).
//! These aggregate bounds leave room outside builds for the daemon/desktop;
//! the parent and daemon are never given the build memory ceiling. MemoryHigh
//! throttles reclaim; MemoryMax may OOM a build rather than exhaust the host.
//! This memory policy is a no-op on macOS/Windows (no systemd user slice).
//!
//! #2282: the slices are the user's, shared by every process on the host, so a
//! process writes them only when it is the hub daemon (its data directory is
//! the user's own, see [`owns_user_slices`]) and only after the live setting
//! was published into [`set_percent`]. A throwaway daemon with a fresh store (a
//! TUI end-to-end fixture) otherwise "applies" the default 50% over the
//! operator's value, and the hub runs twice as hot until someone notices.
//! The gate sits at the one place that executes `systemctl --user`
//! ([`user_manager_runner`]), so the daemon's own apply and the job launch
//! share it, and a test seam build refuses outright.

use serde_json::json;
use std::io;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// The systemd user slice every RSI cargo build and test unit runs in.
pub const SLICE: &str = "rsi-build.slice";
/// The absolute `systemctl` the daemon runs, so no `PATH` entry can stand in.
pub const SYSTEMCTL: &str = "/usr/bin/systemctl";
/// `CPUWeight` of the slice: builds yield to everything else under `rsi.slice`.
pub const CPU_WEIGHT: u32 = 20;
/// The parent of [`SLICE`] and `rsi-workers.slice` (the dash in a slice name
/// encodes its parent).
pub const PARENT_SLICE: &str = "rsi.slice";
/// `CPUWeight` of [`PARENT_SLICE`], below the desktop's `app.slice` (100) and
/// level with systemd's `background.slice` (30).
pub const PARENT_CPU_WEIGHT: u32 = 30;
/// Default share of the host's logical CPUs the slice may use.
pub const HUB_BUILD_CPU_PERCENT_DEFAULT: u32 = 50;
/// Smallest accepted value of the setting.
pub const HUB_BUILD_CPU_PERCENT_MIN: u32 = 10;
/// Largest accepted value of the setting (the whole host).
pub const HUB_BUILD_CPU_PERCENT_MAX: u32 = 100;

/// The published share; [`UNPUBLISHED`] until the store (or the daemon at start)
/// publishes one, because the default is not the operator's value (#2282).
static PERCENT: AtomicU32 = AtomicU32::new(UNPUBLISHED);
/// `PERCENT` before anything was published (the setting range starts at 10).
const UNPUBLISHED: u32 = 0;
/// Whether this process is the hub daemon that may write the user's slices.
static OWNS_USER_SLICES: AtomicBool = AtomicBool::new(false);
/// Serializes writers: boot, the setting change and every launch may race.
static APPLY_LOCK: Mutex<()> = Mutex::new(());

/// The share (percent of the host's logical CPUs) new units are budgeted from:
/// the published value, else the default (sizing only; see [`write_refusal`]).
#[must_use]
pub fn current_percent() -> u32 {
    published_percent().unwrap_or(HUB_BUILD_CPU_PERCENT_DEFAULT)
}

/// The operator's value once it was published, `None` before.
#[must_use]
pub fn published_percent() -> Option<u32> {
    Some(PERCENT.load(Ordering::Relaxed)).filter(|percent| *percent != UNPUBLISHED)
}

/// Publish the operator's value (the store calls this on every accepted write).
pub fn set_percent(percent: u32) {
    PERCENT.store(percent, Ordering::Relaxed);
}

/// Record whether this process is the hub daemon (see [`owns_user_slices`]).
pub fn bind_owner(owns: bool) {
    OWNS_USER_SLICES.store(owns, Ordering::Relaxed);
}

/// Whether the daemon whose data directory is `data_dir` is the user's own hub
/// daemon: `data_dir` is the passwd home's `.rsi`, not a `$HOME`-relative one
/// a fixture redirected (a throwaway daemon shares the user's systemd manager
/// but not the user's settings). Where the passwd entry cannot be read, or has
/// no home, the daemon is not the owner: it fails closed (#2342).
#[must_use]
pub fn owns_user_slices(data_dir: &Path) -> bool {
    #[cfg(unix)]
    {
        let home = match nix::unistd::User::from_uid(nix::unistd::Uid::current()) {
            Ok(Some(user)) if !user.dir.as_os_str().is_empty() => Some(user.dir),
            Ok(_) => {
                tracing::warn!("passwd entry has no home; not owning the user's build slices");
                None
            }
            Err(err) => {
                tracing::warn!(%err, "passwd lookup failed; not owning the user's build slices");
                None
            }
        };
        owns_user_slices_of(data_dir, home.as_deref())
    }
    #[cfg(not(unix))]
    {
        let _ = data_dir;
        true
    }
}

/// [`owns_user_slices`] for an explicit passwd home; no home is no ownership.
#[must_use]
pub fn owns_user_slices_of(data_dir: &Path, user_home: Option<&Path>) -> bool {
    let Some(home) = user_home else { return false };
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical(data_dir) == canonical(&home.join(".rsi"))
}

/// Why this process must not write the user's slices, `None` when it may.
#[must_use]
pub fn write_refusal() -> Option<&'static str> {
    refusal(
        OWNS_USER_SLICES.load(Ordering::Relaxed),
        published_percent(),
    )
}

fn refusal(owns: bool, published: Option<u32>) -> Option<&'static str> {
    if !owns {
        Some(
            "this daemon is not the user's hub daemon (its data directory is not the \
             user's own ~/.rsi); it leaves the shared slices alone",
        )
    } else if published.is_none() {
        Some(
            "hub_build_cpu_percent has not been loaded from the store yet; the default \
             is never written over the operator's value",
        )
    } else {
        None
    }
}

/// Why a test seam build never writes the user's slices.
const TEST_SEAM_REFUSAL: &str = "test seam: the real build slices are never written by tests";

/// What stops this process from writing the shared slices right now: the
/// ownership and publication rules of [`write_refusal`], and always a test seam
/// build (a test unit shares the user's systemd manager with the hub).
fn live_write_refusal() -> Option<&'static str> {
    if cfg!(feature = "test-seam") {
        Some(TEST_SEAM_REFUSAL)
    } else {
        write_refusal()
    }
}

/// `refusal` when `args` (a `systemctl --user` argument list) would write one
/// of the shared slices ([`SLICE`], [`PARENT_SLICE`]); `None` for everything
/// else, reads and a throwaway probe slice included.
fn shared_slice_write_refusal(
    args: &[&str],
    refusal: Option<&'static str>,
) -> Option<&'static str> {
    let writes = args.first() == Some(&"set-property")
        && args[1..]
            .iter()
            .any(|arg| *arg == SLICE || *arg == PARENT_SLICE);
    if writes { refusal } else { None }
}

/// The `systemctl --user` runner for the user's own systemd manager: runs
/// `systemctl` without the session token and refuses (a `PermissionDenied`
/// error that [`apply_with`] reports) to `set-property` a shared slice unless
/// this process may write them (see [`write_refusal`]). Every production caller
/// of [`apply_with`] takes its runner from here, so no path can write the
/// slices around the gate (#2282).
pub fn user_manager_runner(systemctl: &Path) -> impl Fn(&[&str]) -> io::Result<Output> + '_ {
    move |args| {
        if let Some(why) = shared_slice_write_refusal(args, live_write_refusal()) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, why));
        }
        Command::new(systemctl)
            .arg("--user")
            .args(args)
            .env_remove("RSI_SESSION_TOKEN")
            .output()
    }
}

/// Online logical CPUs of the host. On Unix this is `sysconf`, which a CPU
/// quota or affinity mask does not shrink: a process inside a quota-limited
/// cgroup (a test unit, say) must still size the hub budget from the whole
/// host, or it would write a tiny quota into the shared slice. Elsewhere, and
/// if `sysconf` fails, the parallelism std reports.
#[must_use]
pub fn host_threads() -> usize {
    #[cfg(unix)]
    {
        // SAFETY: sysconf has no preconditions and does not mutate state.
        let online = unsafe { nix::libc::sysconf(nix::libc::_SC_NPROCESSORS_ONLN) };
        if let Some(online) = usize::try_from(online).ok().filter(|online| *online > 0) {
            return online;
        }
    }
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

/// Physical host RAM, independent of this process's cgroup/affinity limits.
/// Non-Linux hosts have no systemd memory policy and return `None`.
#[must_use]
pub fn host_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: sysconf has no preconditions and does not mutate state.
        let pages = unsafe { nix::libc::sysconf(nix::libc::_SC_PHYS_PAGES) };
        let page_size = unsafe { nix::libc::sysconf(nix::libc::_SC_PAGESIZE) };
        u64::try_from(pages)
            .ok()?
            .checked_mul(u64::try_from(page_size).ok()?)
            .filter(|bytes| *bytes > 0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Aggregate build memory bounds. No per-job multiplication of host RAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryBudget {
    pub high_bytes: u64,
    pub max_bytes: u64,
}

impl MemoryBudget {
    /// Leave 25% of physical RAM outside admitted builds, starting reclaim at
    /// 60%. Round down to MiB so systemd's page rounding does not cause a
    /// rewrite on every apply. Division first avoids overflow on any host size.
    #[must_use]
    pub fn for_host(bytes: u64) -> Self {
        let mib = 1024 * 1024;
        let share = |percent| (bytes / 100 * percent + bytes % 100 * percent / 100) / mib * mib;
        Self {
            high_bytes: share(60),
            max_bytes: share(75),
        }
    }
}

/// The slice's `CPUQuota`, in percent of one CPU: `percent` of `host_threads`
/// (50% of 32 threads is 1600), never below one CPU.
#[must_use]
pub fn quota_percent(host_threads: usize, percent: u32) -> u64 {
    let percent = u64::from(percent.clamp(HUB_BUILD_CPU_PERCENT_MIN, HUB_BUILD_CPU_PERCENT_MAX));
    (host_threads.max(1) as u64 * percent).max(100)
}

/// Whole cores of the slice budget (at least one).
#[must_use]
pub fn budget_cores(host_threads: usize, percent: u32) -> u64 {
    (quota_percent(host_threads, percent) / 100).max(1)
}

/// One unit's ceiling (percent of one CPU): the unit's own `ceiling_percent`
/// capped by the slice budget, so a unit never sizes itself for the whole host.
#[must_use]
pub fn per_job_ceiling_percent(host_threads: usize, percent: u32, ceiling_percent: u64) -> u64 {
    ceiling_percent
        .min(budget_cores(host_threads, percent) * 100)
        .max(100)
}

/// [`per_job_ceiling_percent`] for the live setting on this host.
#[must_use]
pub fn current_job_ceiling_percent(ceiling_percent: u64) -> u64 {
    per_job_ceiling_percent(host_threads(), current_percent(), ceiling_percent)
}

/// What [`ensure_applied`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceState {
    /// The slice properties were written.
    Applied,
    /// The slice already carried the wanted properties.
    Unchanged,
    /// No slice could be configured; units run without a shared budget.
    Unavailable,
}

impl SliceState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Unchanged => "unchanged",
            Self::Unavailable => "unavailable",
        }
    }
}

/// The account of one apply, for the RPC response and the logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceReport {
    pub state: SliceState,
    pub slice: String,
    pub hub_build_cpu_percent: u32,
    pub cpu_quota_percent: u64,
    pub cpu_weight: u32,
    pub budget_cores: u64,
    /// Intended aggregate build memory bounds; absent when RAM is unavailable
    /// or the platform does not support the systemd policy.
    pub memory: Option<MemoryBudget>,
    /// What happened to [`PARENT_SLICE`]'s `CPUWeight` (#2126).
    pub parent_state: SliceState,
    /// Why the slice is unavailable, or what was refused.
    pub detail: Option<String>,
}

impl SliceReport {
    /// An unavailable report for the live setting with `detail` as the reason
    /// (a failed apply task, for example).
    #[must_use]
    pub fn failed(detail: String) -> Self {
        let (threads, percent) = (host_threads(), current_percent());
        Self {
            state: SliceState::Unavailable,
            slice: SLICE.to_string(),
            hub_build_cpu_percent: percent,
            cpu_quota_percent: quota_percent(threads, percent),
            cpu_weight: CPU_WEIGHT,
            budget_cores: budget_cores(threads, percent),
            memory: host_memory_bytes().map(MemoryBudget::for_host),
            parent_state: SliceState::Unavailable,
            detail: Some(detail),
        }
    }

    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "state": self.state.as_str(),
            "slice": self.slice,
            "hub_build_cpu_percent": self.hub_build_cpu_percent,
            "cpu_quota_percent": self.cpu_quota_percent,
            "cpu_weight": self.cpu_weight,
            "budget_cores": self.budget_cores,
            "memory_high_bytes": self.memory.map(|memory| memory.high_bytes),
            "memory_max_bytes": self.memory.map(|memory| memory.max_bytes),
            "parent_slice": PARENT_SLICE,
            "parent_cpu_weight": PARENT_CPU_WEIGHT,
            "parent_cpu_quota_percent": self.cpu_quota_percent,
            "parent_state": self.parent_state.as_str(),
            "detail": self.detail,
        })
    }

    #[must_use]
    pub fn is_unavailable(&self) -> bool {
        self.state == SliceState::Unavailable
    }
}

/// Microseconds of `systemctl show`'s timespan rendering (`16s`, `1min 4s`,
/// `12.5s`, `500ms`); `None` for `infinity` or anything unrecognised.
fn parse_timespan_usec(text: &str) -> Option<u64> {
    let mut total = 0.0_f64;
    let mut parts = 0;
    for token in text.split_whitespace() {
        let split = token.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
        let (number, unit) = token.split_at(split);
        let unit_usec = match unit {
            "us" => 1.0,
            "ms" => 1_000.0,
            "s" => 1_000_000.0,
            "min" => 60_000_000.0,
            "h" => 3_600_000_000.0,
            "d" => 86_400_000_000.0,
            "w" => 604_800_000_000.0,
            _ => return None,
        };
        total += number.parse::<f64>().ok()? * unit_usec;
        parts += 1;
    }
    // Truncation is the intent: systemd's resolution is a microsecond.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (parts > 0).then(|| total.round() as u64)
}

/// The slice's current `(CPUQuotaPerSecUSec as CPUQuota percent, CPUWeight)`.
fn parse_show(stdout: &str) -> (Option<u64>, Option<u32>) {
    let mut quota = None;
    let mut weight = None;
    for line in stdout.lines() {
        if let Some(value) = line.strip_prefix("CPUQuotaPerSecUSec=") {
            // One CPU is one second per second: 10_000 usec per percent.
            quota = parse_timespan_usec(value.trim()).map(|usec| usec / 10_000);
        } else if let Some(value) = line.strip_prefix("CPUWeight=") {
            weight = value.trim().parse().ok();
        }
    }
    (quota, weight)
}

fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    let start = text.len().saturating_sub(300);
    // Never cut inside a multi-byte character.
    let start = (start..=text.len())
        .find(|at| text.is_char_boundary(*at))
        .unwrap_or(text.len());
    text[start..].to_string()
}

/// Write `slice`'s runtime `CPUQuota`/`CPUWeight` for `percent` of
/// `host_threads` through `systemctl --user`, only when they differ. `run` is
/// the `systemctl --user` runner (replaced by tests).
pub fn apply_with(
    run: &dyn Fn(&[&str]) -> io::Result<Output>,
    slice: &str,
    host_threads: usize,
    percent: u32,
) -> SliceReport {
    apply_weighted_with(run, slice, host_threads, percent, CPU_WEIGHT)
}

/// [`apply_with`] for a slice with its own `CPUWeight` ([`PARENT_SLICE`] takes
/// the same quota as [`SLICE`] at [`PARENT_CPU_WEIGHT`], #2126).
pub fn apply_weighted_with(
    run: &dyn Fn(&[&str]) -> io::Result<Output>,
    slice: &str,
    host_threads: usize,
    percent: u32,
    weight: u32,
) -> SliceReport {
    apply_properties_with(run, slice, host_threads, percent, weight, None)
}

fn apply_properties_with(
    run: &dyn Fn(&[&str]) -> io::Result<Output>,
    slice: &str,
    host_threads: usize,
    percent: u32,
    weight: u32,
    memory: Option<MemoryBudget>,
) -> SliceReport {
    let quota = quota_percent(host_threads, percent);
    let mut report = SliceReport {
        state: SliceState::Unavailable,
        slice: slice.to_string(),
        hub_build_cpu_percent: percent,
        cpu_quota_percent: quota,
        cpu_weight: weight,
        budget_cores: budget_cores(host_threads, percent),
        memory,
        parent_state: SliceState::Unavailable,
        detail: None,
    };
    let _serialized = APPLY_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let shown = match run(&[
        "show",
        slice,
        "--property=CPUQuotaPerSecUSec",
        "--property=CPUWeight",
        "--property=MemoryHigh",
        "--property=MemoryMax",
    ]) {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            report.detail = Some(format!(
                "systemctl --user show {slice} exited {}: {}",
                output.status,
                tail(&output.stderr)
            ));
            return report;
        }
        Err(error) => {
            report.detail = Some(format!("cannot run systemctl --user: {error}"));
            return report;
        }
    };
    let (have_quota, have_weight) = parse_show(&String::from_utf8_lossy(&shown.stdout));
    let shown_memory = |name: &str| {
        String::from_utf8_lossy(&shown.stdout)
            .lines()
            .find_map(|line| {
                line.strip_prefix(name)
                    .and_then(|value| value.parse::<u64>().ok())
            })
    };
    let memory_matches = memory.is_none_or(|budget| {
        shown_memory("MemoryHigh=") == Some(budget.high_bytes)
            && shown_memory("MemoryMax=") == Some(budget.max_bytes)
    });
    if have_quota == Some(quota) && have_weight == Some(weight) && memory_matches {
        report.state = SliceState::Unchanged;
        return report;
    }
    let quota_property = format!("CPUQuota={quota}%");
    let weight_property = format!("CPUWeight={weight}");
    let mut args = vec![
        "set-property",
        "--runtime",
        slice,
        &quota_property,
        &weight_property,
    ];
    let high_property;
    let max_property;
    if let Some(budget) = memory {
        high_property = format!("MemoryHigh={}", budget.high_bytes);
        max_property = format!("MemoryMax={}", budget.max_bytes);
        args.extend([high_property.as_str(), max_property.as_str()]);
    }
    match run(&args) {
        Ok(output) if output.status.success() => report.state = SliceState::Applied,
        Ok(output) => {
            report.detail = Some(format!(
                "systemctl --user set-property {slice} exited {}: {}",
                output.status,
                tail(&output.stderr)
            ));
        }
        Err(error) => report.detail = Some(format!("cannot run systemctl --user: {error}")),
    }
    report
}

/// Configure [`SLICE`] for the live setting on this host. Blocking (it runs
/// `systemctl`); async callers use `spawn_blocking`. Never fails: the report
/// says whether the budget is in force.
#[must_use]
pub fn ensure_applied() -> SliceReport {
    // A test seam build reports its own, more specific, reason below.
    #[cfg(not(feature = "test-seam"))]
    if let Some(why) = write_refusal() {
        return SliceReport::failed(why.to_string());
    }
    ensure_applied_for(host_threads(), current_percent())
}

/// Test builds (the `test-seam` feature, enabled only through dev-dependency
/// edges) never write the operator's real slice: the slice belongs to the
/// user's systemd manager, and a daemon fixture or test unit that did would
/// overwrite the running daemon's budget (and could throttle its own run).
#[cfg(feature = "test-seam")]
#[must_use]
pub fn ensure_applied_for(host_threads: usize, percent: u32) -> SliceReport {
    SliceReport {
        state: SliceState::Unavailable,
        slice: SLICE.to_string(),
        hub_build_cpu_percent: percent,
        cpu_quota_percent: quota_percent(host_threads, percent),
        cpu_weight: CPU_WEIGHT,
        budget_cores: budget_cores(host_threads, percent),
        memory: None,
        parent_state: SliceState::Unavailable,
        detail: Some(TEST_SEAM_REFUSAL.to_string()),
    }
}

/// [`ensure_applied`] for an explicit host size and share.
#[cfg(all(target_os = "linux", not(feature = "test-seam")))]
#[must_use]
pub fn ensure_applied_for(host_threads: usize, percent: u32) -> SliceReport {
    apply_pair_with(
        &user_manager_runner(Path::new(SYSTEMCTL)),
        host_threads,
        percent,
    )
}

/// Both slices for `percent` of `host_threads`: [`SLICE`] and, at the low
/// weight, its parent [`PARENT_SLICE`] with the same quota (#2126). The parent's
/// failure never marks the build budget unavailable. `run` is the
/// `systemctl --user` runner (replaced by tests).
pub fn apply_pair_with(
    run: &dyn Fn(&[&str]) -> io::Result<Output>,
    host_threads: usize,
    percent: u32,
) -> SliceReport {
    apply_pair_with_memory(run, host_threads, percent, host_memory_bytes())
}

/// Both slices with explicit physical RAM for fixture-only policy verification.
/// `None` keeps CPU budgeting and reports the missing memory protection.
pub fn apply_pair_with_memory(
    run: &dyn Fn(&[&str]) -> io::Result<Output>,
    host_threads: usize,
    percent: u32,
    host_memory: Option<u64>,
) -> SliceReport {
    let memory = host_memory
        .filter(|bytes| *bytes > 0)
        .map(MemoryBudget::for_host);
    let mut report = apply_properties_with(run, SLICE, host_threads, percent, CPU_WEIGHT, memory);
    if memory.is_none() && !report.is_unavailable() {
        report.detail =
            Some("host RAM unavailable; build memory bounds were not configured".into());
    }
    let parent = apply_weighted_with(run, PARENT_SLICE, host_threads, percent, PARENT_CPU_WEIGHT);
    report.parent_state = parent.state;
    if let Some(detail) = parent.detail {
        report.detail = Some(match report.detail.take() {
            Some(first) => format!("{first}; {detail}"),
            None => detail,
        });
    }
    report
}

/// No systemd user manager here: the explicit "no slice available" report.
#[cfg(all(not(target_os = "linux"), not(feature = "test-seam")))]
#[must_use]
pub fn ensure_applied_for(host_threads: usize, percent: u32) -> SliceReport {
    SliceReport {
        state: SliceState::Unavailable,
        slice: SLICE.to_string(),
        hub_build_cpu_percent: percent,
        cpu_quota_percent: quota_percent(host_threads, percent),
        cpu_weight: CPU_WEIGHT,
        budget_cores: budget_cores(host_threads, percent),
        memory: None,
        parent_state: SliceState::Unavailable,
        detail: Some(format!(
            "{SLICE} needs a Linux systemd user manager; build and test units run \
             without a shared CPU budget on {}",
            std::env::consts::OS
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[cfg(unix)]
    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn memory_budget_leaves_headroom_even_on_small_hosts() {
        for gib in [1, 4, 8, 30, 64, 256] {
            let ram = gib * 1024 * 1024 * 1024;
            let budget = MemoryBudget::for_host(ram);
            assert_eq!(
                budget.high_bytes,
                ram * 60 / 100 / (1024 * 1024) * (1024 * 1024)
            );
            assert_eq!(
                budget.max_bytes,
                ram * 75 / 100 / (1024 * 1024) * (1024 * 1024)
            );
            assert!(budget.high_bytes < budget.max_bytes);
            assert!(ram - budget.max_bytes >= ram / 4);
        }
        let largest = MemoryBudget::for_host(u64::MAX);
        assert!(largest.high_bytes < largest.max_bytes && largest.max_bytes < u64::MAX);
    }

    #[cfg(unix)]
    #[test]
    fn shared_memory_bounds_apply_with_cpu_and_repair_memory_only_drift() {
        let ram = 30 * 1024 * 1024 * 1024;
        let budget = MemoryBudget::for_host(ram);
        let calls = RefCell::new(Vec::<Vec<String>>::new());
        let properties = RefCell::new(std::collections::BTreeMap::<String, Vec<String>>::new());
        let run = |args: &[&str]| -> io::Result<Output> {
            calls
                .borrow_mut()
                .push(args.iter().map(ToString::to_string).collect());
            if args[0] == "show" {
                let properties = properties.borrow();
                let shown = properties
                    .get(args[1])
                    .map(|rows| rows.join("\n"))
                    .unwrap_or_default();
                return Ok(output(0, &shown, ""));
            }
            let rows = args[3..]
                .iter()
                .map(|property| {
                    if let Some(quota) = property.strip_prefix("CPUQuota=") {
                        let quota: u64 = quota.trim_end_matches('%').parse().unwrap();
                        format!("CPUQuotaPerSecUSec={}s", quota / 100)
                    } else {
                        property.to_string()
                    }
                })
                .collect();
            properties.borrow_mut().insert(args[2].to_string(), rows);
            Ok(output(0, "", ""))
        };
        let report = apply_pair_with_memory(&run, 8, 50, Some(ram));
        assert_eq!(report.state, SliceState::Applied);
        assert_eq!(report.parent_state, SliceState::Applied);
        assert_eq!(report.memory, Some(budget));
        assert_eq!(report.to_json()["memory_max_bytes"], budget.max_bytes);
        let writes: Vec<_> = calls
            .borrow()
            .iter()
            .filter(|call| call[0] == "set-property")
            .cloned()
            .collect();
        assert_eq!(
            writes,
            vec![
                vec![
                    "set-property".into(),
                    "--runtime".into(),
                    SLICE.into(),
                    "CPUQuota=400%".into(),
                    "CPUWeight=20".into(),
                    format!("MemoryHigh={}", budget.high_bytes),
                    format!("MemoryMax={}", budget.max_bytes)
                ],
                vec![
                    "set-property".into(),
                    "--runtime".into(),
                    PARENT_SLICE.into(),
                    "CPUQuota=400%".into(),
                    "CPUWeight=30".into()
                ],
            ]
        );
        calls.borrow_mut().clear();
        assert_eq!(
            apply_pair_with_memory(&run, 8, 50, Some(ram)).state,
            SliceState::Unchanged
        );
        assert!(calls.borrow().iter().all(|call| call[0] == "show"));
        // Old/unbounded memory settings must be repaired even when CPU matches.
        properties
            .borrow_mut()
            .get_mut(SLICE)
            .unwrap()
            .retain(|row| !row.starts_with("Memory"));
        assert_eq!(
            apply_pair_with_memory(&run, 8, 50, Some(ram)).state,
            SliceState::Applied
        );
        assert_eq!(
            apply_pair_with_memory(&run, 8, 50, Some(ram)).state,
            SliceState::Unchanged
        );
    }

    #[cfg(unix)]
    #[test]
    fn refused_memory_bounds_are_reported_and_parent_cpu_is_still_applied() {
        let run = |args: &[&str]| -> io::Result<Output> {
            if args[0] == "set-property" && args[2] == SLICE {
                return Ok(output(1, "", "memory controller unavailable"));
            }
            Ok(output(0, "", ""))
        };
        let report = apply_pair_with_memory(&run, 8, 50, Some(8 * 1024 * 1024 * 1024));
        assert!(report.is_unavailable());
        assert_eq!(report.parent_state, SliceState::Applied);
        assert!(
            report
                .detail
                .unwrap()
                .contains("memory controller unavailable")
        );
        let missing_ram = apply_pair_with_memory(&run, 8, 50, None);
        assert!(missing_ram.memory.is_none());
    }

    #[test]
    fn host_threads_is_not_shrunk_by_a_cpu_quota_or_affinity() {
        // std's parallelism honours the cgroup quota and the affinity mask of
        // the running process; the online CPU count never undershoots it.
        let visible = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        assert!(host_threads() >= visible.max(1));
    }

    #[test]
    fn quota_is_the_share_of_the_hosts_logical_cpus() {
        // The hub: 50% of 32 threads is 16 cores.
        assert_eq!(quota_percent(32, 50), 1600);
        assert_eq!(budget_cores(32, 50), 16);
        assert_eq!(quota_percent(32, 100), 3200);
        assert_eq!(quota_percent(32, 10), 320);
        // Out-of-range values clamp; a tiny host keeps one CPU.
        assert_eq!(quota_percent(32, 5), 320);
        assert_eq!(quota_percent(32, 500), 3200);
        assert_eq!(quota_percent(1, 10), 100);
        assert_eq!(quota_percent(0, 50), 100);
        assert_eq!(budget_cores(4, 10), 1);
    }

    #[test]
    fn a_job_never_sizes_itself_above_the_slice_budget() {
        // The #2087 ceiling is 12 cores on a 32-thread host; the 16-core
        // budget leaves it alone.
        assert_eq!(per_job_ceiling_percent(32, 50, 1200), 1200);
        assert_eq!(per_job_ceiling_percent(32, 50, 1600), 1600);
        // A smaller share caps it: 25% of 32 threads is 8 cores.
        assert_eq!(per_job_ceiling_percent(32, 25, 1200), 800);
        assert_eq!(per_job_ceiling_percent(32, 10, 1600), 300);
        // A small host: 50% of 8 threads is 4 cores.
        assert_eq!(per_job_ceiling_percent(8, 50, 400), 400);
        assert_eq!(per_job_ceiling_percent(8, 50, 1200), 400);
        // Never below one CPU.
        assert_eq!(per_job_ceiling_percent(2, 10, 1200), 100);
    }

    #[test]
    fn timespans_parse_as_systemctl_prints_them() {
        assert_eq!(parse_timespan_usec("16s"), Some(16_000_000));
        assert_eq!(parse_timespan_usec("12.5s"), Some(12_500_000));
        assert_eq!(parse_timespan_usec("500ms"), Some(500_000));
        assert_eq!(parse_timespan_usec("1min 4s"), Some(64_000_000));
        assert_eq!(parse_timespan_usec("infinity"), None);
        assert_eq!(parse_timespan_usec(""), None);
        assert_eq!(
            parse_show("CPUWeight=20\nCPUQuotaPerSecUSec=16s\n"),
            (Some(1600), Some(20))
        );
        assert_eq!(
            parse_show("CPUWeight=[not set]\nCPUQuotaPerSecUSec=infinity\n"),
            (None, None)
        );
    }

    #[cfg(unix)]
    #[test]
    fn apply_writes_runtime_properties_only_when_they_differ() {
        let calls = RefCell::new(Vec::<Vec<String>>::new());
        let shown = RefCell::new(String::from(
            "CPUWeight=[not set]\nCPUQuotaPerSecUSec=infinity\n",
        ));
        let run = |args: &[&str]| -> io::Result<Output> {
            calls
                .borrow_mut()
                .push(args.iter().map(ToString::to_string).collect());
            if args[0] == "show" {
                return Ok(output(0, &shown.borrow(), ""));
            }
            // The write lands in what `show` reports next.
            *shown.borrow_mut() = "CPUWeight=20\nCPUQuotaPerSecUSec=16s\n".into();
            Ok(output(0, "", ""))
        };
        let first = apply_with(&run, SLICE, 32, 50);
        assert_eq!(first.state, SliceState::Applied);
        assert_eq!(first.cpu_quota_percent, 1600);
        assert_eq!(first.budget_cores, 16);
        assert_eq!(
            calls.borrow().last().unwrap(),
            &[
                "set-property",
                "--runtime",
                "rsi-build.slice",
                "CPUQuota=1600%",
                "CPUWeight=20"
            ]
        );
        // The second apply finds the properties in place and writes nothing.
        calls.borrow_mut().clear();
        let second = apply_with(&run, SLICE, 32, 50);
        assert_eq!(second.state, SliceState::Unchanged);
        assert_eq!(calls.borrow().len(), 1);
        assert_eq!(calls.borrow()[0][0], "show");
        // A changed setting rewrites it.
        let third = apply_with(&run, SLICE, 32, 25);
        assert_eq!(third.state, SliceState::Applied);
        assert!(
            calls
                .borrow()
                .last()
                .unwrap()
                .contains(&"CPUQuota=800%".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_parent_gets_the_low_weight_and_the_same_quota_from_the_setting() {
        // The desktop's app.slice is 100; the parent sits at the
        // background.slice class (#2126).
        assert!(PARENT_CPU_WEIGHT < 100 && PARENT_CPU_WEIGHT >= 30);
        assert_eq!(PARENT_SLICE, "rsi.slice");
        let calls = RefCell::new(Vec::<Vec<String>>::new());
        let shown = RefCell::new(String::from(
            "CPUWeight=[not set]\nCPUQuotaPerSecUSec=infinity\n",
        ));
        let run = |args: &[&str]| -> io::Result<Output> {
            calls
                .borrow_mut()
                .push(args.iter().map(ToString::to_string).collect());
            if args[0] == "show" {
                return Ok(output(0, &shown.borrow(), ""));
            }
            // Mirror the write into what `show` reports next.
            let quota: u64 = args[3]
                .trim_start_matches("CPUQuota=")
                .trim_end_matches('%')
                .parse()
                .unwrap();
            *shown.borrow_mut() = format!(
                "CPUWeight={PARENT_CPU_WEIGHT}\nCPUQuotaPerSecUSec={}s\n",
                quota / 100
            );
            Ok(output(0, "", ""))
        };
        let first = apply_weighted_with(&run, PARENT_SLICE, 32, 50, PARENT_CPU_WEIGHT);
        assert_eq!(first.state, SliceState::Applied);
        assert_eq!(first.cpu_weight, 30);
        assert_eq!(
            calls.borrow().last().unwrap(),
            &[
                "set-property",
                "--runtime",
                "rsi.slice",
                "CPUQuota=1600%",
                "CPUWeight=30"
            ]
        );
        // A restart finds both in place and writes nothing.
        calls.borrow_mut().clear();
        let second = apply_weighted_with(&run, PARENT_SLICE, 32, 50, PARENT_CPU_WEIGHT);
        assert_eq!(second.state, SliceState::Unchanged);
        assert_eq!(calls.borrow().len(), 1);
        // A changed setting rewrites the quota, keeping the weight.
        let third = apply_weighted_with(&run, PARENT_SLICE, 32, 25, PARENT_CPU_WEIGHT);
        assert_eq!(third.state, SliceState::Applied);
        assert_eq!(calls.borrow().last().unwrap()[3], "CPUQuota=800%");
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_or_missing_systemctl_is_reported_unavailable() {
        let refuse = |args: &[&str]| -> io::Result<Output> {
            if args[0] == "show" {
                return Ok(output(
                    0,
                    "CPUWeight=[not set]\nCPUQuotaPerSecUSec=infinity\n",
                    "",
                ));
            }
            Ok(output(1, "", "Failed to connect to bus: No medium found"))
        };
        let report = apply_with(&refuse, SLICE, 32, 50);
        assert_eq!(report.state, SliceState::Unavailable);
        assert!(
            report
                .detail
                .as_deref()
                .unwrap()
                .contains("No medium found")
        );
        assert!(report.is_unavailable());

        let missing = |_: &[&str]| -> io::Result<Output> {
            Err(io::Error::new(io::ErrorKind::NotFound, "no systemctl"))
        };
        let report = apply_with(&missing, SLICE, 32, 50);
        assert_eq!(report.state, SliceState::Unavailable);
        assert!(report.detail.as_deref().unwrap().contains("no systemctl"));
        assert_eq!(report.to_json()["state"], "unavailable");
        assert_eq!(report.to_json()["cpu_quota_percent"], 1600);
    }

    /// A user manager with both slices, modelled just far enough for `apply`:
    /// `show` reports what was last written, `set-property` records it.
    #[cfg(unix)]
    struct FakeManager {
        calls: RefCell<Vec<Vec<String>>>,
        state: RefCell<std::collections::BTreeMap<String, (u64, u32)>>,
    }

    #[cfg(unix)]
    impl FakeManager {
        /// Both slices carrying `quota` percent, as after a hand fix or a restart.
        fn with_quota(quota: u64) -> Self {
            let state = [(SLICE, CPU_WEIGHT), (PARENT_SLICE, PARENT_CPU_WEIGHT)]
                .into_iter()
                .map(|(slice, weight)| (slice.to_string(), (quota, weight)))
                .collect();
            Self {
                calls: RefCell::default(),
                state: RefCell::new(state),
            }
        }

        fn run(&self, args: &[&str]) -> io::Result<Output> {
            self.calls
                .borrow_mut()
                .push(args.iter().map(ToString::to_string).collect());
            let slice = args[if args[0] == "show" { 1 } else { 2 }].to_string();
            if args[0] == "show" {
                let (quota, weight) = self.state.borrow()[&slice];
                return Ok(output(
                    0,
                    &format!("CPUWeight={weight}\nCPUQuotaPerSecUSec={}s\n", quota / 100),
                    "",
                ));
            }
            let quota = args[3]
                .trim_start_matches("CPUQuota=")
                .trim_end_matches('%');
            let weight = args[4].trim_start_matches("CPUWeight=");
            self.state
                .borrow_mut()
                .insert(slice, (quota.parse().unwrap(), weight.parse().unwrap()));
            Ok(output(0, "", ""))
        }

        fn quotas(&self) -> Vec<u64> {
            [SLICE, PARENT_SLICE]
                .iter()
                .map(|slice| self.state.borrow()[*slice].0)
                .collect()
        }

        fn written_quotas(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .filter(|call| call[0] == "set-property")
                .map(|call| call[3].clone())
                .collect()
        }
    }

    /// #2282: a daemon start with the operator's 25% on a 32-thread host
    /// configures 8 CPUs on both slices and never writes the 16-CPU default,
    /// whatever the slices carried before.
    #[cfg(unix)]
    #[test]
    fn start_with_25_percent_on_32_threads_applies_8_cpus_to_both_slices() {
        for before in [1600, 800, 3200] {
            let manager = FakeManager::with_quota(before);
            let report = apply_pair_with_memory(&|args: &[&str]| manager.run(args), 32, 25, None);
            assert_eq!(manager.quotas(), [800, 800], "from {before}%");
            assert_eq!(report.cpu_quota_percent, 800);
            assert_eq!(report.budget_cores, 8);
            assert_eq!(
                report.parent_state,
                if before == 800 {
                    SliceState::Unchanged
                } else {
                    SliceState::Applied
                }
            );
            assert!(
                manager
                    .written_quotas()
                    .iter()
                    .all(|quota| quota == "CPUQuota=800%"),
                "{:?}",
                manager.written_quotas()
            );
        }
    }

    #[test]
    fn nothing_is_written_before_the_setting_is_published_by_the_hub_daemon() {
        // The default is not the operator's value, and a throwaway daemon is
        // not the hub's: either alone refuses the write.
        assert!(refusal(true, None).unwrap().contains("not been loaded"));
        assert!(
            refusal(false, Some(25))
                .unwrap()
                .contains("not the user's hub")
        );
        assert!(refusal(false, None).is_some());
        assert_eq!(refusal(true, Some(25)), None);
    }

    /// #2282: the process-wide state (the published setting, the ownership) and
    /// the runner every production caller takes. The only test in this crate
    /// that touches that state, so parallel tests cannot see a half-open gate.
    #[test]
    fn the_published_setting_and_ownership_open_the_slices_to_writes() {
        set_percent(UNPUBLISHED);
        bind_owner(false);
        assert_eq!(published_percent(), None);
        assert_eq!(current_percent(), HUB_BUILD_CPU_PERCENT_DEFAULT);
        assert!(write_refusal().is_some());
        // `ensure_applied` reports the refusal instead of touching systemd.
        let refused = ensure_applied();
        assert!(refused.is_unavailable(), "{refused:?}");
        // The runner never reaches systemctl for a shared slice write, whoever
        // the caller is (the daemon's apply, a job launch, a test unit).
        #[cfg(unix)]
        {
            // `true` stands in for systemctl: a call past the gate succeeds, a
            // refused one never starts it.
            let run = user_manager_runner(Path::new("/usr/bin/true"));
            let write = |slice: &str| run(&["set-property", "--runtime", slice, "CPUQuota=1600%"]);
            let kind = |result: io::Result<Output>| result.err().map(|error| error.kind());
            assert_eq!(kind(write(SLICE)), Some(io::ErrorKind::PermissionDenied));
            assert_eq!(
                kind(write(PARENT_SLICE)),
                Some(io::ErrorKind::PermissionDenied)
            );
            // Reads and a throwaway probe slice stay open.
            assert!(run(&["show", SLICE, "--property=CPUWeight"]).is_ok());
            assert!(write("rsi-probe-build-1.slice").is_ok());
        }
        set_percent(25);
        assert_eq!(published_percent(), Some(25));
        assert_eq!(current_percent(), 25);
        assert!(
            write_refusal().is_some(),
            "a published value alone is not ownership"
        );
        bind_owner(true);
        assert_eq!(write_refusal(), None);
        // The hub daemon with the operator's value may write the slices; a test
        // seam build never may.
        #[cfg(unix)]
        {
            let run = user_manager_runner(Path::new("/usr/bin/true"));
            let written = run(&["set-property", "--runtime", SLICE, "CPUQuota=800%"]);
            assert_eq!(written.is_ok(), !cfg!(feature = "test-seam"));
        }
        bind_owner(false);
        set_percent(UNPUBLISHED);
    }

    #[test]
    fn only_shared_slice_writes_carry_the_refusal() {
        let why = refusal(false, Some(25));
        fn set(slice: &str) -> [&str; 4] {
            ["set-property", "--runtime", slice, "CPUQuota=1600%"]
        }
        assert_eq!(shared_slice_write_refusal(&set(SLICE), why), why);
        assert_eq!(shared_slice_write_refusal(&set(PARENT_SLICE), why), why);
        assert_eq!(
            shared_slice_write_refusal(&set("rsi-probe.slice"), why),
            None
        );
        assert_eq!(
            shared_slice_write_refusal(&set("rsi-workers.slice"), why),
            None
        );
        assert_eq!(
            shared_slice_write_refusal(&["show", SLICE, "--property=CPUWeight"], why),
            None
        );
        // An owner that loaded the setting is never refused.
        assert_eq!(shared_slice_write_refusal(&set(SLICE), None), None);
    }

    #[test]
    fn only_the_daemon_in_the_users_own_data_dir_owns_the_slices() {
        let home = tempfile::tempdir().unwrap();
        let data = home.path().join(".rsi");
        std::fs::create_dir(&data).unwrap();
        assert!(owns_user_slices_of(&data, Some(home.path())));
        // A symlinked spelling of the same directory is the same directory.
        #[cfg(unix)]
        {
            let link = home.path().join("link");
            std::os::unix::fs::symlink(&data, &link).unwrap();
            assert!(owns_user_slices_of(&link, Some(home.path())));
        }
        // A fixture's `$HOME`-relative data dir is another daemon's.
        let fixture = tempfile::tempdir().unwrap();
        assert!(!owns_user_slices_of(
            &fixture.path().join("home/.rsi"),
            Some(home.path())
        ));
        // No passwd home to compare with (lookup failed or no entry): fail closed (#2342).
        assert!(!owns_user_slices_of(fixture.path(), None));
        assert!(!owns_user_slices_of(&data, None));
    }

    /// Drop a probe slice's runtime drop-ins. `systemctl revert` does the same
    /// but reloads the user's whole systemd manager, which the journal shows as
    /// "Reload requested" from the test unit, indistinguishable from a write to
    /// the hub's real slices. The probe is inactive and unreferenced, so
    /// systemd unloads it by itself.
    #[cfg(target_os = "linux")]
    fn discard_probe_slice(slice: &str) {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(format!("/run/user/{}", nix::unistd::Uid::current()))
            });
        let _ = std::fs::remove_dir_all(
            runtime
                .join("systemd/user.control")
                .join(format!("{slice}.d")),
        );
    }

    /// The real systemd user manager, on a throwaway slice (never the
    /// operator's `rsi-build.slice`): a write shows in `systemctl show`, an
    /// identical apply writes nothing and a new share rewrites it live.
    #[cfg(target_os = "linux")]
    #[test]
    fn live_apply_reaches_the_systemd_user_manager() {
        let systemctl = |args: &[&str]| Command::new(SYSTEMCTL).arg("--user").args(args).output();
        if systemctl(&["show", "--property=Version"])
            .map(|output| !output.status.success())
            .unwrap_or(true)
        {
            eprintln!("skipped: no systemd user manager");
            return;
        }
        let slice = format!("rsi-probe-build-{}.slice", std::process::id());
        let run = |args: &[&str]| systemctl(args);
        let first = apply_with(&run, &slice, 32, 50);
        let shown = |slice: &str| {
            let output = systemctl(&[
                "show",
                slice,
                "--property=CPUQuotaPerSecUSec",
                "--property=CPUWeight",
            ])
            .unwrap();
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        let before = shown(&slice);
        let again = apply_with(&run, &slice, 32, 50);
        let changed = apply_with(&run, &slice, 32, 25);
        let after = shown(&slice);
        discard_probe_slice(&slice);
        assert_eq!(first.state, SliceState::Applied, "{first:?}");
        assert!(before.contains("CPUQuotaPerSecUSec=16s"), "{before}");
        assert!(before.contains("CPUWeight=20"), "{before}");
        assert_eq!(again.state, SliceState::Unchanged, "{again:?}");
        assert_eq!(changed.state, SliceState::Applied, "{changed:?}");
        assert!(after.contains("CPUQuotaPerSecUSec=8s"), "{after}");
    }

    /// The parent weight and quota on a throwaway slice (never the operator's `rsi.slice`).
    #[cfg(target_os = "linux")]
    #[test]
    fn live_parent_weight_and_quota_reach_the_systemd_user_manager() {
        let systemctl = |args: &[&str]| Command::new(SYSTEMCTL).arg("--user").args(args).output();
        if systemctl(&["show", "--property=Version"])
            .map(|output| !output.status.success())
            .unwrap_or(true)
        {
            eprintln!("skipped: no systemd user manager");
            return;
        }
        let slice = format!("rsi-probe-parent-{}.slice", std::process::id());
        let run = |args: &[&str]| systemctl(args);
        let first = apply_weighted_with(&run, &slice, 32, 50, PARENT_CPU_WEIGHT);
        let shown = systemctl(&[
            "show",
            &slice,
            "--property=CPUWeight",
            "--property=CPUQuotaPerSecUSec",
        ])
        .unwrap();
        let shown = String::from_utf8_lossy(&shown.stdout).into_owned();
        let again = apply_weighted_with(&run, &slice, 32, 50, PARENT_CPU_WEIGHT);
        discard_probe_slice(&slice);
        assert_eq!(first.state, SliceState::Applied, "{first:?}");
        assert!(shown.contains("CPUWeight=30"), "{shown}");
        assert!(shown.contains("CPUQuotaPerSecUSec=16s"), "{shown}");
        assert_eq!(again.state, SliceState::Unchanged, "{again:?}");
    }
}
