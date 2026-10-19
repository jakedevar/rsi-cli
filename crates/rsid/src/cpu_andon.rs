//! #1337: the CPU-time andon per agent-owned process tree.
//!
//! On 2026-10-06 one worker ran the full `rsid-store` suite and a broad `rsid`
//! run for about two hours at 12-14 cores each (host load 60-83 on 32 cores)
//! while its manager watched. The daemon now samples CPU time per agent
//! process tree once a minute and, when one tree passes the operator's
//! CPU-minute threshold (`cpu_andon_cpu_minutes`) or dominates a loaded host
//! (`cpu_andon_host_load`), it:
//!
//! - records one `runaway_process:<tree>:<reason>` friction event (#1333);
//! - tells the owning manager (a manager-inbox notice, else the Epic lead by
//!   mail) with the session id and a suggested halt
//!   ([`crate::store::runaway_process`]);
//! - publishes a warning system message for the operator.
//!
//! It never stops a tree: the manager decides. Each tree trips at most once
//! per daemon lifetime, and the notice is idempotent per tree across restarts.
//!
//! Trees are cgroups, so measurement is cheap and exact: a session's provider
//! scope (`rsi-workers.slice/rsi-worker-<invocation>-<n>.scope`, which holds
//! every command the agent runs) and a job unit (`rsi-job-<id>.service`, in
//! `rsi-build.slice` for cargo units, #2119). `scripts/scoped-test` runs its
//! build and tests in a nested `rsi-scoped-test-<job id>-<hex>.service` beside
//! the job unit (a separate cgroup, so the job unit holds only the driver); it
//! is attributed to that job (#2127).
//! Linux reads `cgroup v2 cpu.stat`. macOS (#2951) has no cgroups: launchd
//! owns each job as the service `rsi-job-<id>`, so the sampler lists the
//! services (`launchctl list`: pid and label), takes each job's root process
//! and reads its cumulative CPU time from `proc_pid_rusage` (its own plus the
//! reaped descendants', so a tree of short-lived `rustc` children still
//! accumulates). Provider scopes are not launchd services and have no
//! invocation-to-pid map there, so only job trees are sampled on macOS.
//! Elsewhere the sampler reports [`ANDON_UNSUPPORTED`] once and the loop ends.

use crate::session::SessionManager;
use crate::store::runaway_process::{RunawayNoticeRoute, RunawayProcessReport};
use rsi_common::friction::{FrictionKind, NewFrictionEventV1};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How often the sampler reads the trees.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(60);
/// Most trees one sample reads (bounds the walk and the state maps).
const MAX_TREES: usize = 512;
/// A tree "dominates" a loaded host when it uses at least this many cores...
const DOMINANT_MIN_CORES: f64 = 4.0;
/// ...and at least this share of the 1-minute load.
const DOMINANT_LOAD_SHARE: f64 = 0.15;
/// The explicit refusal on platforms with no tree CPU source.
pub const ANDON_UNSUPPORTED: &str = "cpu_andon_unsupported";

/// Who owns a sampled tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TreeOwner {
    /// A provider scope; the model invocation names the session.
    Invocation(Uuid),
    /// An `AgentSubmitJob` unit.
    Job(Uuid),
}

impl TreeOwner {
    #[must_use]
    pub const fn id(self) -> Uuid {
        match self {
            Self::Invocation(id) | Self::Job(id) => id,
        }
    }
}

/// One tree's cumulative CPU time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSample {
    pub unit: String,
    pub owner: TreeOwner,
    pub usage_usec: u64,
}

/// The operator thresholds; 0 turns a trigger off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AndonThresholds {
    pub cpu_minutes: u32,
    pub host_load: u32,
}

impl AndonThresholds {
    #[must_use]
    pub const fn is_off(self) -> bool {
        self.cpu_minutes == 0 && self.host_load == 0
    }
}

/// Why a tree tripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TripReason {
    CpuMinutes,
    HostLoad,
}

impl TripReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CpuMinutes => "cpu_minutes",
            Self::HostLoad => "host_load",
        }
    }
}

/// One tree that crossed a threshold.
#[derive(Debug, Clone, PartialEq)]
pub struct Trip {
    pub sample: TreeSample,
    pub reason: TripReason,
    pub cpu_minutes: f64,
    pub cores: Option<f64>,
    pub host_load: Option<f64>,
    pub threshold: u32,
}

/// The sampler's memory: the previous reading per unit (for the window
/// rate) and the units that already tripped. Both are pruned to the units
/// present in the latest sample, so they stay within [`MAX_TREES`].
#[derive(Debug, Default)]
pub struct CpuAndon {
    previous: HashMap<String, (u64, Instant)>,
    tripped: HashSet<String>,
}

impl CpuAndon {
    /// Evaluate one sample. Pure: no I/O.
    pub fn observe(
        &mut self,
        samples: Vec<TreeSample>,
        host_load: Option<f64>,
        now: Instant,
        thresholds: AndonThresholds,
    ) -> Vec<Trip> {
        let present: HashSet<String> = samples.iter().map(|s| s.unit.clone()).collect();
        self.previous.retain(|unit, _| present.contains(unit));
        self.tripped.retain(|unit| present.contains(unit));
        let loaded = host_load
            .filter(|load| thresholds.host_load > 0 && *load >= f64::from(thresholds.host_load));
        let mut trips = Vec::new();
        for sample in samples.into_iter().take(MAX_TREES) {
            let cores = self.previous.get(&sample.unit).and_then(|(usage, at)| {
                let seconds = now.checked_duration_since(*at)?.as_secs_f64();
                (seconds > 0.0 && sample.usage_usec >= *usage)
                    .then(|| (sample.usage_usec - usage) as f64 / 1e6 / seconds)
            });
            self.previous
                .insert(sample.unit.clone(), (sample.usage_usec, now));
            if self.tripped.contains(&sample.unit) {
                continue;
            }
            let cpu_minutes = sample.usage_usec as f64 / 60e6;
            let reason =
                if thresholds.cpu_minutes > 0 && cpu_minutes >= f64::from(thresholds.cpu_minutes) {
                    Some((TripReason::CpuMinutes, thresholds.cpu_minutes))
                } else if let (Some(load), Some(cores)) = (loaded, cores)
                    && cores >= DOMINANT_MIN_CORES
                    && cores >= load * DOMINANT_LOAD_SHARE
                {
                    Some((TripReason::HostLoad, thresholds.host_load))
                } else {
                    None
                };
            if let Some((reason, threshold)) = reason {
                self.tripped.insert(sample.unit.clone());
                trips.push(Trip {
                    sample,
                    reason,
                    cpu_minutes,
                    cores,
                    host_load,
                    threshold,
                });
            }
        }
        trips
    }
}

/// `rsi-worker-<invocation>-<n>.scope`, `rsi-job-<id>.service` or, for the
/// nested unit `scripts/scoped-test` starts beside its job unit (#2127),
/// `rsi-scoped-test-<job id>-<hex>.service`. Without a job id the scoped-test
/// unit belongs to an operator run and is no agent tree.
#[must_use]
pub fn tree_owner(unit: &str) -> Option<TreeOwner> {
    if let Some(rest) = unit
        .strip_prefix("rsi-scoped-test-")
        .and_then(|rest| rest.strip_suffix(".service"))
    {
        let (id, tag) = (rest.get(..36)?, rest.get(36..)?);
        return (tag.len() > 1 && tag.starts_with('-'))
            .then(|| Uuid::parse_str(id).ok())
            .flatten()
            .map(TreeOwner::Job);
    }
    if let Some(rest) = unit
        .strip_prefix("rsi-worker-")
        .and_then(|rest| rest.strip_suffix(".scope"))
    {
        return rest
            .get(..36)
            .and_then(|id| Uuid::parse_str(id).ok())
            .map(TreeOwner::Invocation);
    }
    unit.strip_prefix("rsi-job-")
        .and_then(|rest| rest.strip_suffix(".service"))
        .and_then(|id| Uuid::parse_str(id).ok())
        .map(TreeOwner::Job)
}

/// `usage_usec` from a cgroup v2 `cpu.stat`.
#[must_use]
pub fn parse_usage_usec(cpu_stat: &str) -> Option<u64> {
    cpu_stat.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next() == Some("usage_usec"))
            .then(|| fields.next()?.parse().ok())
            .flatten()
    })
}

/// Read every agent tree's CPU time beneath this user's systemd manager.
///
/// # Errors
/// [`ANDON_UNSUPPORTED`] without cgroup v2 under a systemd user manager.
#[cfg(target_os = "linux")]
pub fn sample_trees() -> Result<(Vec<TreeSample>, Option<f64>), &'static str> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").map_err(|_| ANDON_UNSUPPORTED)?;
    // SAFETY: geteuid has no preconditions and does not mutate state.
    let uid = unsafe { nix::libc::geteuid() };
    let root = crate::process_scope::user_manager_cgroup_root(&cgroup, uid)
        .map_err(|_| ANDON_UNSUPPORTED)?;
    let dirs = [
        root.join("rsi.slice")
            .join(crate::process_scope::SCOPE_SLICE),
        // #2119: cargo build and test job units run in the hub build slice.
        root.join("rsi.slice").join(rsid_core::build_slice::SLICE),
        // Other transient `systemd-run --user` services land in app.slice
        // (and units started before the build slice existed stay there).
        root.join("app.slice"),
        root.clone(),
    ];
    let mut samples = Vec::new();
    'dirs: for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if samples.len() >= MAX_TREES {
                break 'dirs;
            }
            let Some(unit) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Some(owner) = tree_owner(&unit) else {
                continue;
            };
            let Some(usage_usec) = std::fs::read_to_string(entry.path().join("cpu.stat"))
                .ok()
                .as_deref()
                .and_then(parse_usage_usec)
            else {
                continue;
            };
            samples.push(TreeSample {
                unit,
                owner,
                usage_usec,
            });
        }
    }
    let load = crate::daemon_info::load_average().map(|load| load.one);
    Ok((samples, load))
}

/// Running launchd services as `(pid, label)` from `launchctl list` output
/// (`PID<tab>Status<tab>Label`, `-` for a service with no process).
#[cfg(any(test, target_os = "macos"))]
#[must_use]
fn parse_launchctl_list(output: &str) -> Vec<(i32, String)> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let pid: i32 = fields.next()?.trim().parse().ok()?;
            let _status = fields.next()?;
            let label = fields.next()?.trim();
            (pid > 0 && !label.is_empty()).then(|| (pid, label.to_string()))
        })
        .collect()
}

/// `proc_pid_rusage` reports CPU time in mach absolute-time ticks (1:1 on an
/// Intel Mac, 125:3 on Apple silicon): microseconds for `ticks` at the given
/// timebase.
#[cfg(any(test, target_os = "macos"))]
#[must_use]
fn mach_ticks_to_usec(ticks: u64, numer: u32, denom: u32) -> u64 {
    if denom == 0 {
        return 0;
    }
    let nanos = u128::from(ticks) * u128::from(numer) / u128::from(denom);
    u64::try_from(nanos / 1_000).unwrap_or(u64::MAX)
}

/// Cumulative CPU time of one process and every descendant it has reaped, in
/// microseconds; `None` when the process is gone or unreadable.
// libc marks `mach_timebase_info` deprecated in favour of the `mach2` crate;
// one call is not worth a dependency.
#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn process_tree_usage_usec(pid: i32) -> Option<u64> {
    use nix::libc;
    // SAFETY: an all-zero `rusage_info_v2` is a valid value (plain integers).
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    // SAFETY: `proc_pid_rusage` fills at most one `rusage_info_v2` through the
    // pointer for flavor RUSAGE_INFO_V2, and `info` outlives the call.
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V2,
            std::ptr::addr_of_mut!(info).cast::<libc::rusage_info_t>(),
        )
    };
    if rc != 0 {
        return None;
    }
    let mut timebase = libc::mach_timebase_info { numer: 0, denom: 0 };
    // SAFETY: `timebase` is a valid out-pointer for the call.
    let rc = unsafe { libc::mach_timebase_info(&mut timebase) };
    if rc != 0 {
        return None;
    }
    let ticks = info
        .ri_user_time
        .saturating_add(info.ri_system_time)
        .saturating_add(info.ri_child_user_time)
        .saturating_add(info.ri_child_system_time);
    Some(mach_ticks_to_usec(ticks, timebase.numer, timebase.denom))
}

/// Read every agent job tree's CPU time from launchd (#2951). The sampled
/// unit is named `rsi-job-<id>.service`, the shape [`tree_owner`] reads on
/// Linux, so the trip routing is shared.
///
/// # Errors
/// [`ANDON_UNSUPPORTED`] when `launchctl` cannot be started. A failed listing
/// is an empty sample (the next minute retries), not the end of the loop.
#[cfg(target_os = "macos")]
pub fn sample_trees() -> Result<(Vec<TreeSample>, Option<f64>), &'static str> {
    let output = std::process::Command::new("/bin/launchctl")
        .arg("list")
        .env_remove("RSI_SESSION_TOKEN")
        .output()
        .map_err(|_| ANDON_UNSUPPORTED)?;
    let mut samples = Vec::new();
    if output.status.success() {
        for (pid, label) in parse_launchctl_list(&String::from_utf8_lossy(&output.stdout)) {
            if samples.len() >= MAX_TREES {
                break;
            }
            let unit = format!("{label}.service");
            let Some(owner @ TreeOwner::Job(_)) = tree_owner(&unit) else {
                continue;
            };
            let Some(usage_usec) = process_tree_usage_usec(pid) else {
                continue;
            };
            samples.push(TreeSample {
                unit,
                owner,
                usage_usec,
            });
        }
    } else {
        tracing::debug!(status = %output.status, "launchctl list failed; CPU andon sampled no trees");
    }
    let load = crate::daemon_info::load_average().map(|load| load.one);
    Ok((samples, load))
}

/// No tree CPU source here: the andon is explicitly unsupported.
///
/// # Errors
/// Always [`ANDON_UNSUPPORTED`].
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn sample_trees() -> Result<(Vec<TreeSample>, Option<f64>), &'static str> {
    Err(ANDON_UNSUPPORTED)
}

/// The friction event for one trip.
#[must_use]
pub fn trip_friction(trip: &Trip, tree: &str, session: Option<Uuid>) -> NewFrictionEventV1 {
    let event =
        NewFrictionEventV1::new(FrictionKind::RunawayProcess, &[tree, trip.reason.as_str()])
            .session(session);
    match trip.sample.owner {
        TreeOwner::Job(id) => event.evidence("job", id),
        TreeOwner::Invocation(_) => match session {
            Some(id) => event.evidence("session", id),
            None => event,
        },
    }
}

/// Resolve, record and route one trip. Best-effort: every failure is logged.
async fn report_trip(manager: &SessionManager, trip: Trip) {
    let observed_at = chrono::Utc::now();
    let store = manager.store().lock().await;
    let (session, tree) = match trip.sample.owner {
        TreeOwner::Invocation(id) => (
            store.model_invocation_session(id).ok().flatten(),
            "session".to_string(),
        ),
        TreeOwner::Job(id) => match store.get_agent_job(id).ok().flatten() {
            Some(row) => (
                Some(row.job.owner_session_id),
                format!("job_{}", row.job.kind.as_str()),
            ),
            None => (None, "job".to_string()),
        },
    };
    crate::friction::note_locked(&store, &trip_friction(&trip, &tree, session));
    let route = match session {
        Some(session_id) => store
            .record_runaway_process_notice(&RunawayProcessReport {
                session_id,
                tree: tree.clone(),
                tree_id: trip.sample.owner.id(),
                unit: trip.sample.unit.clone(),
                reason: trip.reason.as_str().into(),
                cpu_minutes: trip.cpu_minutes,
                cores: trip.cores,
                host_load: trip.host_load,
                threshold: trip.threshold,
                observed_at,
            })
            .unwrap_or_else(|error| {
                tracing::warn!(%session_id, %error, "runaway process notice deferred");
                RunawayNoticeRoute::Unrouted
            }),
        None => RunawayNoticeRoute::Unrouted,
    };
    drop(store);
    let who = session.map_or_else(|| "an unknown session".to_string(), |id| id.to_string());
    tracing::warn!(
        unit = %trip.sample.unit,
        session = %who,
        reason = trip.reason.as_str(),
        cpu_minutes = trip.cpu_minutes,
        cores = ?trip.cores,
        host_load = ?trip.host_load,
        route = ?route,
        "CPU andon: runaway agent process tree"
    );
    manager
        .event_bus()
        .publish(crate::bus::DaemonEvent::SystemMessage {
            level: "warn".into(),
            message: format!(
                "CPU andon (#1337): {} ({tree}, session {who}) used {:.0} CPU-minutes{}; {}. Halt it with AgentHalt or stop the job if it is not about to finish.",
                trip.sample.unit,
                trip.cpu_minutes,
                trip.cores
                    .map(|cores| format!(", {cores:.1} cores now"))
                    .unwrap_or_default(),
                match route {
                    RunawayNoticeRoute::ManagerNotice(_) => "its manager was notified",
                    RunawayNoticeRoute::LeadMail(_) => "its Epic lead was mailed",
                    RunawayNoticeRoute::Unrouted => "no manager to notify",
                },
            ),
        });
}

/// Sample every minute; read the thresholds at each sample (live settings).
pub async fn run_cpu_andon_loop(
    manager: Arc<SessionManager>,
    settings: Arc<crate::config::RuntimeConfig>,
) {
    let mut interval = tokio::time::interval(SAMPLE_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut andon = CpuAndon::default();
    loop {
        interval.tick().await;
        let thresholds = AndonThresholds {
            cpu_minutes: settings.cpu_andon_cpu_minutes.load(Ordering::Relaxed),
            host_load: settings.cpu_andon_host_load.load(Ordering::Relaxed),
        };
        if thresholds.is_off() {
            andon = CpuAndon::default();
            continue;
        }
        let sampled = match tokio::task::spawn_blocking(sample_trees).await {
            Ok(sampled) => sampled,
            Err(error) => {
                tracing::warn!(%error, "CPU andon sample join failed");
                continue;
            }
        };
        let (samples, load) = match sampled {
            Ok(sampled) => sampled,
            Err(code) => {
                // #2937: a visible state, not silence: this host has no tree
                // CPU source (no cgroup v2 on Linux without a user manager, no
                // launchctl on macOS, #2951), so runaway cargo trees are
                // bounded by the capacity gate (`host_memory`) and the job
                // timeout instead.
                tracing::warn!(
                    code,
                    "CPU andon is unsupported on this host (no tree CPU source); \
                     runaway process trees are not sampled"
                );
                return;
            }
        };
        for trip in andon.observe(samples, load, Instant::now(), thresholds) {
            report_trip(&manager, trip).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ON: AndonThresholds = AndonThresholds {
        cpu_minutes: 240,
        host_load: 40,
    };

    fn sample(unit: &str, cpu_minutes: f64) -> TreeSample {
        TreeSample {
            unit: unit.into(),
            owner: tree_owner(unit).unwrap(),
            usage_usec: (cpu_minutes * 60e6) as u64,
        }
    }

    const JOB: &str = "rsi-job-00000000-0000-0000-0000-000000000001.service";
    const SCOPE: &str = "rsi-worker-00000000-0000-0000-0000-000000000002-00000000-0000-0000-0000-000000000003.scope";

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn tree_names_map_to_their_owner() {
        assert_eq!(tree_owner(JOB), Some(TreeOwner::Job(Uuid::from_u128(1))));
        assert_eq!(
            tree_owner(SCOPE),
            Some(TreeOwner::Invocation(Uuid::from_u128(2)))
        );
        // #2127: a scoped test's nested unit belongs to the job named in it.
        assert_eq!(
            tree_owner("rsi-scoped-test-00000000-0000-0000-0000-000000000001-0123abcd.service"),
            Some(TreeOwner::Job(Uuid::from_u128(1)))
        );
        for other in [
            "rsi-scoped-test-abc.service",
            "rsi-scoped-test-0123456789abcdef0123456789abcdef.service",
            "rsi-scoped-test-00000000-0000-0000-0000-000000000001.service",
            "rsi-scoped-test-00000000-0000-0000-0000-000000000001-.service",
            "rsi-scoped-test-nope-0000-0000-0000-0000000000001-ab.service",
            "rsid.scope",
            "rsi-job-nope.service",
            "rsi-worker-x.scope",
        ] {
            assert_eq!(tree_owner(other), None, "{other}");
        }
        assert_eq!(
            parse_usage_usec("usage_usec 123\nuser_usec 100\nsystem_usec 23\n"),
            Some(123)
        );
        assert_eq!(parse_usage_usec("user_usec 100\n"), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_tree_past_its_cpu_minutes_trips_once() {
        let mut andon = CpuAndon::default();
        let t0 = Instant::now();
        assert!(
            andon
                .observe(vec![sample(JOB, 239.0)], Some(5.0), t0, ON)
                .is_empty()
        );
        let trips = andon.observe(
            vec![sample(JOB, 241.0)],
            Some(5.0),
            t0 + Duration::from_secs(60),
            ON,
        );
        assert_eq!(trips.len(), 1);
        assert_eq!(trips[0].reason, TripReason::CpuMinutes);
        assert_eq!(trips[0].threshold, 240);
        assert!((trips[0].cores.unwrap() - 2.0).abs() < 1e-6);
        // The same tree never trips again while it lives.
        assert!(
            andon
                .observe(
                    vec![sample(JOB, 900.0)],
                    Some(80.0),
                    t0 + Duration::from_secs(120),
                    ON
                )
                .is_empty()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_tree_dominating_a_loaded_host_trips_and_a_quiet_host_does_not() {
        let t0 = Instant::now();
        let later = t0 + Duration::from_secs(60);
        // 13 cores for a minute on a host at load 70 (the 2026-10-06 run).
        let run = |load| {
            let mut andon = CpuAndon::default();
            andon.observe(vec![sample(SCOPE, 10.0)], Some(load), t0, ON);
            andon.observe(vec![sample(SCOPE, 23.0)], Some(load), later, ON)
        };
        let trips = run(70.0);
        assert_eq!(trips.len(), 1);
        assert_eq!(trips[0].reason, TripReason::HostLoad);
        assert_eq!(trips[0].threshold, 40);
        // Below the load threshold the same tree is ordinary work.
        assert!(run(30.0).is_empty());
        // A light tree on a loaded host is not dominant.
        let mut andon = CpuAndon::default();
        andon.observe(vec![sample(SCOPE, 10.0)], Some(90.0), t0, ON);
        assert!(
            andon
                .observe(vec![sample(SCOPE, 12.0)], Some(90.0), later, ON)
                .is_empty()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn zero_thresholds_are_off_and_gone_trees_are_forgotten() {
        let off = AndonThresholds {
            cpu_minutes: 0,
            host_load: 0,
        };
        assert!(off.is_off());
        let mut andon = CpuAndon::default();
        assert!(
            andon
                .observe(vec![sample(JOB, 5000.0)], Some(99.0), Instant::now(), off)
                .is_empty()
        );
        andon.observe(vec![sample(JOB, 5000.0)], None, Instant::now(), ON);
        assert_eq!(andon.tripped.len(), 1);
        andon.observe(Vec::new(), None, Instant::now(), ON);
        assert!(andon.tripped.is_empty() && andon.previous.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn trip_friction_signature_names_tree_and_reason() {
        let mut andon = CpuAndon::default();
        let trip = andon
            .observe(vec![sample(JOB, 300.0)], None, Instant::now(), ON)
            .remove(0);
        let session = Uuid::from_u128(9);
        let event = trip_friction(&trip, "job_test", Some(session));
        assert_eq!(event.signature, "runaway_process:job_test:cpu_minutes");
        assert_eq!(event.session_id, Some(session));
        assert_eq!(
            event.evidence_ref.as_deref(),
            Some("job:00000000-0000-0000-0000-000000000001")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn other_platforms_are_explicitly_unsupported() {
        // Linux reads the live cgroup tree and macOS asks launchd; every other
        // platform refuses.
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        assert_eq!(sample_trees().unwrap_err(), ANDON_UNSUPPORTED);
        assert_eq!(ANDON_UNSUPPORTED, "cpu_andon_unsupported");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn launchctl_list_yields_running_services_only() {
        let listing = "PID\tStatus\tLabel\n\
-\t0\tcom.apple.SafariHistoryServiceAgent\n\
965\t0\tcom.apple.progressd\n\
4242\t-9\trsi-job-00000000-0000-0000-0000-000000000001\n\
garbage\n\
0\t0\tcom.example.zero\n";
        let services = parse_launchctl_list(listing);
        assert_eq!(
            services,
            vec![
                (965, "com.apple.progressd".to_string()),
                (
                    4242,
                    "rsi-job-00000000-0000-0000-0000-000000000001".to_string()
                ),
            ]
        );
        // Only a job label is an agent tree; the unit shape is the Linux one.
        let owned: Vec<_> = services
            .iter()
            .filter_map(|(_, label)| tree_owner(&format!("{label}.service")))
            .collect();
        assert_eq!(
            owned,
            vec![TreeOwner::Job(
                Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap()
            )]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn mach_ticks_convert_through_the_timebase() {
        // Apple silicon: 125/3 ns per tick, so 24 ticks are 1 us.
        assert_eq!(mach_ticks_to_usec(24, 125, 3), 1);
        assert_eq!(mach_ticks_to_usec(24_000_000 * 60, 125, 3), 60_000_000);
        // Intel: 1/1, ticks are nanoseconds.
        assert_eq!(mach_ticks_to_usec(5_000_000, 1, 1), 5_000);
        // An unreadable timebase reads zero, never a division by zero.
        assert_eq!(mach_ticks_to_usec(10, 1, 0), 0);
    }

    // The live sampler reads this very process through `proc_pid_rusage`.
    #[cfg(target_os = "macos")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn macos_reads_this_processs_cpu_time() {
        let mut burn = 0_u64;
        for i in 0..20_000_000_u64 {
            burn = burn.wrapping_add(i ^ (burn >> 3));
        }
        assert_ne!(burn, 1);
        let usec = process_tree_usage_usec(std::process::id() as i32).expect("own rusage");
        assert!(usec > 0, "usec {usec}");
        assert!(process_tree_usage_usec(i32::MAX).is_none());
    }
}
