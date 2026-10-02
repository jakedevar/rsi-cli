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
}

/// One paired satellite's daemon, read over the hub link at call time
/// (#1017 slice 2). An unreachable satellite (down, mid-restart, or its
/// identity no longer matches) is `reachable:false` with only a stable
/// `error` code, never a stale identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SatelliteDaemonInfoV1 {
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
}

/// The typed reason a launch, continuation, wake or job is held or refused
/// while a deploy waits for its quiet point.
pub const DEPLOY_DRAINING: &str = "deploy_draining";

/// One piece of work parked behind a draining deploy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldWorkV1 {
    /// `child_spawn`, `topology_node`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<uuid::Uuid>,
    /// RFC3339 with nanoseconds.
    pub since: String,
    /// Always [`DEPLOY_DRAINING`].
    pub reason: String,
}

/// Deploy drain status (#1073). Held work runs after the deploy settles;
/// refused work (`child_launch`, `continuation`, `agent_job`) was answered with
/// `deploy_draining` and is retried by its caller. Scheduled resume wakes stay
/// as due rows and are counted in `wakes_held`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployDrainV1 {
    pub draining: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy_id: Option<uuid::Uuid>,
    /// RFC3339 with nanoseconds: the hold is released no later than this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_by: Option<String>,
    #[serde(default)]
    pub held: Vec<HeldWorkV1>,
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
