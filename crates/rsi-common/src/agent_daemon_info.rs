//! `AgentGetDaemonInfo` (#1045 slice 1): a read-only, secret-free description of
//! the running hub daemon, so a manager can decide whether a deploy is needed
//! without `pgrep`, `readlink` or `sha256sum` shell hacks.
//!
//! Nothing here can carry an environment value or a credential: every field is
//! a build identifier, a hash, a timestamp, a number or a closed enum string.

use serde::{Deserialize, Serialize};

/// Supervisor mode: the parent process is `rsid-supervisor.sh`, so an exit
/// code 75 restarts the daemon in place with its environment intact.
pub const DAEMON_SUPERVISOR_SCRIPT: &str = "rsid-supervisor.sh";
/// Supervisor mode: no supervising parent was recognised.
pub const DAEMON_SUPERVISOR_NONE: &str = "none";

/// Strict empty request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentGetDaemonInfoRequestV1 {}

/// Free space, in bytes, for the two trees the daemon writes to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonDiskFreeV1 {
    /// Free bytes on the filesystem holding the data dir (`~/.rsi`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir_free_bytes: Option<u64>,
    /// Free bytes on the filesystem holding the sandbox base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_base_free_bytes: Option<u64>,
}

/// 1, 5 and 15 minute load averages.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DaemonLoadV1 {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

/// One daemon's identity and health.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonInfoV1 {
    /// Full commit SHA embedded at build time (`unknown` when the build had no
    /// git checkout and no `RSI_BUILD_SHA`).
    pub build_sha: String,
    /// Newest known local release artifact; absent when no build can be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_build: Option<LocalBuildV1>,
    /// Lowercase hex sha256 of the running executable, computed once at start.
    /// Absent when the binary could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_sha256: Option<String>,
    /// Daemon start time, RFC3339 with nanoseconds.
    pub started_at: String,
    pub uptime_secs: u64,
    /// `PRAGMA user_version` of the daemon's database.
    pub schema_version: i64,
    pub disk_free: DaemonDiskFreeV1,
    /// Absent when the host exposes no load average.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<DaemonLoadV1>,
    /// `rsid-supervisor.sh` or `none`; absent when it cannot be determined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_mode: Option<String>,
    /// The rsid binary the supervisor relaunches (#1164); a deploy installs
    /// there or is refused as `deploy_target_mismatch`. Absent when the daemon
    /// is not under the supervisor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_binary: Option<String>,
    /// The scripts the daemon runs from `~/.rsi/bin` (`cargo-slot`,
    /// `target-pool`): whether each is installed and its content hash, so a
    /// missing or stale copy is visible after a deploy (#2722). Absent from
    /// older daemons.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub installed_tools: Vec<InstalledToolV1>,
    /// This daemon's own newest deploy (hub #2980): `deploy_id`, `sha`,
    /// `state` and, once it settled, `finished_at`, from the same
    /// `agent_deploys` row the hub reads as `satellites[].last_deploy`.
    /// A satellite confirms its own deploy here. Absent before any deploy and
    /// from older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_deploy: Option<crate::satellite::SatelliteDeployStatusV1>,
    /// Whether this daemon's build can reclaim sandbox build caches
    /// (hub #3055 part 2): [`BUILD_CACHE_RECLAIM_SUPPORTED`] on Linux and
    /// macOS, [`BUILD_CACHE_RECLAIM_UNSUPPORTED`] on a platform that compiles
    /// the fail-closed shim, where finished workers' `target/` directories are
    /// never removed automatically and the `reclaim_unsupported` health
    /// condition holds. Absent from older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_cache_reclaim: Option<String>,
}

/// A local build is informational only: detecting it never installs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalBuildV1 {
    pub sha: String,
    pub path: String,
    /// Artifact modification time, RFC3339 with nanoseconds (not checkout HEAD).
    pub built_at: String,
    /// Different SHA and strictly newer than the running executable.
    pub differs: bool,
}

impl DaemonInfoV1 {
    /// Persistent operator guidance, shared by every TUI refresh.
    #[must_use]
    pub fn local_build_warning(&self) -> Option<String> {
        let build = self.local_build.as_ref().filter(|build| build.differs)?;
        Some(format!(
            "running build {}, built build {}: run make release-install",
            self.build_sha.chars().take(9).collect::<String>(),
            build.sha.chars().take(9).collect::<String>(),
        ))
    }
}

/// [`DaemonInfoV1::build_cache_reclaim`] on a build that reclaims build caches.
pub const BUILD_CACHE_RECLAIM_SUPPORTED: &str = "supported";
/// [`DaemonInfoV1::build_cache_reclaim`] on a build with the fail-closed shim.
pub const BUILD_CACHE_RECLAIM_UNSUPPORTED: &str = "unsupported";

/// One script the daemon executes from `~/.rsi/bin` (#2722).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledToolV1 {
    /// File name under `~/.rsi/bin` (`cargo-slot` or `target-pool`).
    pub name: String,
    /// A regular, executable file is there. A missing tool leaves the daemon
    /// without it (no warm-pool seeding without `target-pool`).
    pub installed: bool,
    /// Lowercase hex sha256 of the installed file; absent when not installed or
    /// unreadable. Compare with the hash of `scripts/<name>` at the deployed SHA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// One paired satellite's daemon, read over the hub link at call time
/// (#1017 slice 2). An unreachable satellite (down, mid-restart, or its
/// identity no longer matches) is `reachable:false` with only a stable
/// `error` code, never a stale identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SatelliteDaemonInfoV1 {
    /// Latest bounded hub observation; absent on older hubs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<crate::satellite_diagnostics::SatelliteReachabilityV1>,
    /// Last reported historical trip, retained during a current outage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_watchdog_trip: Option<crate::satellite::SatelliteWatchdogTripV1>,
    /// Live host platform; absent for older or unreachable satellites.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<crate::satellite::SatellitePlatformV1>,
    pub peer_id: crate::satellite::SatelliteUuidV1,
    pub label: String,
    pub reachable: bool,
    /// RFC3339 with nanoseconds: when the hub read this over the link.
    pub checked_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_mode: Option<String>,
    /// The satellite's newest deploy: confirm a deploy with
    /// `state == succeeded` and `build_sha` equal to the deployed SHA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_deploy: Option<crate::satellite::SatelliteDeployStatusV1>,
    /// `none` when a manager seat has no effective launch choices (#1941),
    /// else the smallest count; absent when the satellite seats no manager.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launches: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Result: the hub, plus every enabled, paired satellite for the appointed
/// manager (empty for an Epic lead and when no satellite is paired).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentGetDaemonInfoResultV1 {
    pub hub: DaemonInfoV1,
    #[serde(default)]
    pub satellites: Vec<SatelliteDaemonInfoV1>,
    /// Deploy drain (#1073): whether new work is held for a waiting deploy,
    /// and what is held. Absent from older daemons.
    #[serde(default)]
    pub deploy_drain: DeployDrainV1,
    /// Host-load admission (#1417): whether manager launches are held while
    /// the host's 1-minute load is above the operator's threshold, and what
    /// is held. Absent from older daemons.
    #[serde(default)]
    pub host_load: HostLoadAdmissionV1,
    /// Reports this daemon's reporting seat queued for a hub manager and what
    /// became of each (#2242). Present only for the satellite seat that may
    /// call `AgentReportToHub`; absent on a hub and for any other caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub_reports: Option<crate::satellite_dispatch::SatelliteReportStatusV1>,
    /// Warm target pool state (#2773): the newest pool and, when no pool was built, the
    /// typed reason (`pool_skipped_disk_headroom` with the free, floor and expected GiB,
    /// `pool_aborted_disk_floor`), plus idle pools reclaimed for a worker waiting on the
    /// disk floor. Absent when `target-pool` is not installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_pool: Option<crate::target_pool::TargetPoolStatus>,
    /// Topology handoff counters since this daemon started (#2759):
    /// `handoff_observed_total` (a finished node's last message was parsed) and
    /// `handoff_invalid_total` (the parser refused it), each by `provider` and
    /// whether the turn that produced the message carried the handoff schema
    /// (`structured`). Absent until the first handoff is read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handoff_counters: Vec<HandoffCounterV1>,
    /// Aggregate event-bus counters since the bus was created (#332): emitted and
    /// delivered per event kind plus the lagged total. Two reads a known time
    /// apart give exact events/s. Absent from older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bus: Option<BusStatsV1>,
}

/// One event kind's bus counters. `kind` is the closed set of `BusEvent`
/// type names (one per `DaemonEvent` variant), never a payload or a session id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusKindStatsV1 {
    pub kind: String,
    /// `publish` calls of this kind, with or without a subscriber.
    pub emitted: u64,
    /// Receiver enqueues: the sum over `publish` calls of the receivers that
    /// held a slot for the event (an emit with no subscriber adds 0).
    pub delivered: u64,
}

/// Bounded aggregate counters of the daemon event bus (#332). Memory is the
/// number of event kinds (a compile-time closed set), whatever the traffic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusStatsV1 {
    /// Seconds since the counters started (the bus was created).
    pub window_secs: u64,
    /// Live subscribers now.
    pub subscribers: u64,
    pub emitted_total: u64,
    pub delivered_total: u64,
    /// Events receivers skipped because they fell behind the channel capacity,
    /// summed over receivers that report it. The channel does not say which
    /// kinds were skipped, so this has no per-kind split.
    pub lagged_total: u64,
    /// Times a receiver observed a lag (each skips `n >= 1` events).
    pub lag_events_total: u64,
    /// Kinds seen since start, sorted by name.
    #[serde(default)]
    pub by_kind: Vec<BusKindStatsV1>,
}

/// One labelled counter of the topology handoff metrics (#2759).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffCounterV1 {
    /// `handoff_observed_total` or `handoff_invalid_total`.
    pub name: String,
    /// The session provider of the node (`Claude`, `Codex`, ...).
    pub provider: String,
    /// The turn carried the handoff schema (`--json-schema` / `--output-schema`).
    pub structured: bool,
    pub value: u64,
}

/// Counter name: a finished topology node's last message was parsed as a handoff.
pub const HANDOFF_OBSERVED_TOTAL: &str = "handoff_observed_total";
/// Counter name: the handoff parser refused a finished node's last message.
pub const HANDOFF_INVALID_TOTAL: &str = "handoff_invalid_total";

/// The typed reason a manager launch is held while the host's 1-minute load
/// average is above the operator's `host_load_admission_threshold` (#1417).
pub const HOST_LOAD: &str = "host_load";

/// Default of the operator setting `host_load_admission_threshold`: the
/// 1-minute load above which manager launches are held (0 disables).
pub const HOST_LOAD_THRESHOLD_DEFAULT: u32 = 40;

/// Upper bound of `host_load_admission_threshold`.
pub const HOST_LOAD_THRESHOLD_MAX: u32 = 1024;

/// Host-load admission status (#1417). The daemon holds a manager's
/// `create_session` (queued, never refused; Issue-worker launches and
/// topology node launches included) while `load + recent_admissions` is above
/// `threshold`, and releases held launches oldest-first as the load drops.
/// A worker's own spawns, retries, successors, lead recovery and operator
/// sessions are never held.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostLoadAdmissionV1 {
    /// Operator setting `host_load_admission_threshold`; 0 disables the hold.
    #[serde(default)]
    pub threshold: u32,
    /// False on a platform that reports no load average: everything is
    /// admitted and `load` is absent.
    #[serde(default)]
    pub supported: bool,
    /// The host's 1-minute load average when last read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<f64>,
    /// Launches admitted in the last minute that `load` has not absorbed
    /// yet; each counts one toward the comparison with `threshold`.
    #[serde(default)]
    pub recent_admissions: u32,
    /// True while new manager launches are held.
    #[serde(default)]
    pub holding: bool,
    /// Held launches, oldest first (bounded): queued manager `create_session`
    /// actions (`manager_create_session`) and waiting topology nodes
    /// (`topology_node`). Each carries `reason: host_load`.
    #[serde(default)]
    pub held: Vec<HeldWorkV1>,
    /// The build disk floor now (#163): what `scripts/cargo-slot` enforces for
    /// every build/test job, read before dispatch. Absent from older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_disk: Option<BuildDiskFloorV1>,
}

/// The effective disk floor of build admission, separate from any session
/// launch policy. A build/test job's unit may launch with the host below this
/// floor, but its governor slot waits (within the job's admission budget, ten
/// minutes for a scoped test) until `/` holds at least `min_free_gb`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildDiskFloorV1 {
    /// Operator setting `governor_min_free_disk_gb`, in GiB; 0 disables the gate.
    #[serde(default)]
    pub min_free_gb: u64,
    /// Free GiB on `/` (where the governor measures) when read; absent where
    /// it cannot be read, in which case nothing holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_gb: Option<f64>,
    /// Whether a build would pass the disk gate right now.
    #[serde(default)]
    pub open: bool,
}

/// The typed reason a launch, continuation, wake or job is held or refused
/// while a deploy waits for its quiet point.
pub const DEPLOY_DRAINING: &str = "deploy_draining";

/// One piece of work parked behind a draining deploy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldWorkV1 {
    /// `child_spawn`, `topology_node`, `queue_batch` (merge queue held, #1128);
    /// under host-load admission (#1417) `manager_create_session` or
    /// `topology_node`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<uuid::Uuid>,
    /// RFC3339 with nanoseconds.
    pub since: String,
    /// [`DEPLOY_DRAINING`], or [`HOST_LOAD`] under host-load admission.
    pub reason: String,
}

/// Deploy drain status (#1073). Held work runs after the deploy settles;
/// refused work (`child_launch`, `continuation`, `agent_job`) was answered with
/// `deploy_draining` and is retried by its caller. Scheduled resume wakes stay
/// as due rows and are counted in `wakes_held`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployDrainV1 {
    /// True while worker starts, job starts or queue admission are held. Worker
    /// starts are held for at most `deploy_drain_hold_secs` from the deploy
    /// request; queue admission (#1806) and, for an `interrupt_workers` deploy,
    /// `test`/`build` job starts (#2190) are held for the whole staged deploy,
    /// to its deadline. After the worker window the deploy keeps waiting without
    /// holding worker starts, until its first quiet poll holds again through the swap.
    pub draining: bool,
    /// The deploy waiting for its quiet point, held or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy_id: Option<uuid::Uuid>,
    /// RFC3339 with nanoseconds: the hold is released no later than this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_by: Option<String>,
    /// True while a deploy waits for its quiet point (held or not).
    #[serde(default)]
    pub waiting: bool,
    /// RFC3339 with nanoseconds: the waiting deploy gives up (settles
    /// `timed_out`) at this time if no quiet point came.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy_deadline_at: Option<String>,
    #[serde(default)]
    pub held: Vec<HeldWorkV1>,
    /// Why the waiting deploy has not restarted yet (#1177): the latest
    /// quiet-point blockers (`landing_in_progress`, `job_running`,
    /// `worker_mid_turn`, ...). Empty when no deploy waits or none blocks.
    /// A worker mid-turn blocks because a restart interrupts it: provider
    /// processes are not re-adopted, the interrupted turn is resumed.
    #[serde(default)]
    pub blockers: Vec<String>,
    /// Launches, continuations and jobs refused with `deploy_draining` since
    /// the daemon started.
    #[serde(default)]
    pub refused_total: u64,
    /// Scheduled resume wakes deferred (left due, not consumed) since start.
    #[serde(default)]
    pub wakes_held_total: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_build_is_optional_and_warning_tracks_the_latest_artifact() {
        let mut info: DaemonInfoV1 = serde_json::from_value(serde_json::json!({
            "build_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "started_at": "2026-10-19T00:00:00.000000000Z",
            "uptime_secs": 1, "schema_version": 1, "disk_free": {}
        }))
        .unwrap();
        assert_eq!(info.local_build, None);
        assert_eq!(info.local_build_warning(), None);
        assert!(
            serde_json::to_value(&info)
                .unwrap()
                .get("local_build")
                .is_none()
        );
        info.local_build = Some(LocalBuildV1 {
            sha: "b".repeat(40),
            path: "/fixture/rsid".into(),
            built_at: "2026-10-19T01:00:00.000000000Z".into(),
            differs: true,
        });
        assert_eq!(
            info.local_build_warning().as_deref(),
            Some("running build aaaaaaaaa, built build bbbbbbbbb: run make release-install")
        );
        let roundtrip: DaemonInfoV1 =
            serde_json::from_value(serde_json::to_value(&info).unwrap()).unwrap();
        assert_eq!(roundtrip, info);
        info.local_build.as_mut().unwrap().sha = "c".repeat(40);
        assert!(
            info.local_build_warning()
                .unwrap()
                .contains("built build ccccccccc")
        );
        info.local_build.as_mut().unwrap().differs = false;
        assert_eq!(info.local_build_warning(), None);
    }

    #[test]
    fn request_is_strictly_empty() {
        assert!(
            serde_json::from_value::<AgentGetDaemonInfoRequestV1>(serde_json::json!({})).is_ok()
        );
        assert!(
            serde_json::from_value::<AgentGetDaemonInfoRequestV1>(serde_json::json!({"x": 1}))
                .is_err()
        );
    }
}
