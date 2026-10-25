//! Versioned, bounded wire DTOs for operator-only satellite reads.
//!
//! These types describe identity and read data only. They do not authenticate a
//! peer; the configured transport and its local socket custody remain the
//! security boundary.

use crate::{SessionProvider, SessionStatus};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashSet;
use thiserror::Error;
use uuid::Uuid;

pub const SATELLITE_WIRE_VERSION_V1: u16 = 1;
pub const SATELLITE_PROTOCOL_MAJOR: u16 = 1;
pub const SATELLITE_PROTOCOL_MINOR: u16 = 0;
pub const SATELLITE_MAX_PAGE_SIZE: u16 = 100;
pub const SATELLITE_MAX_SESSIONS: u32 = 10_000;
pub const SATELLITE_MAX_CURSOR_BYTES: u16 = 512;
pub const SATELLITE_MAX_TITLE_BYTES: u16 = 512;
pub const SATELLITE_MAX_WORKING_DIR_BYTES: u16 = 4_096;
pub const SATELLITE_MAX_RESPONSE_BYTES: usize = 1_048_576;

/// UUID that must use the canonical lowercase hyphenated wire representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SatelliteUuidV1(pub Uuid);

impl Serialize for SatelliteUuidV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for SatelliteUuidV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let parsed = Uuid::parse_str(&value).map_err(serde::de::Error::custom)?;
        if parsed.to_string() != value {
            return Err(serde::de::Error::custom(
                "UUID must be canonical lowercase hyphenated text",
            ));
        }
        Ok(Self(parsed))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteProtocolVersionV1 {
    pub major: u16,
    pub minor: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteReadLimitsV1 {
    pub max_page_size: u16,
    pub max_sessions: u32,
    pub max_cursor_bytes: u16,
    pub max_title_bytes: u16,
    pub max_working_dir_bytes: u16,
    pub max_response_bytes: u32,
}

impl Default for SatelliteReadLimitsV1 {
    fn default() -> Self {
        Self {
            max_page_size: SATELLITE_MAX_PAGE_SIZE,
            max_sessions: SATELLITE_MAX_SESSIONS,
            max_cursor_bytes: SATELLITE_MAX_CURSOR_BYTES,
            max_title_bytes: SATELLITE_MAX_TITLE_BYTES,
            max_working_dir_bytes: SATELLITE_MAX_WORKING_DIR_BYTES,
            max_response_bytes: 1_048_576,
        }
    }
}

impl SatelliteReadLimitsV1 {
    /// # Errors
    /// Returns an error when any advertised limit exceeds the local ceiling or is zero.
    pub fn validate(&self) -> Result<(), SatelliteValidationError> {
        let default = Self::default();
        if self.max_page_size == 0
            || self.max_page_size > default.max_page_size
            || self.max_sessions == 0
            || self.max_sessions > default.max_sessions
            || self.max_cursor_bytes == 0
            || self.max_cursor_bytes > default.max_cursor_bytes
            || self.max_title_bytes == 0
            || self.max_title_bytes > default.max_title_bytes
            || self.max_working_dir_bytes == 0
            || self.max_working_dir_bytes > default.max_working_dir_bytes
            || self.max_response_bytes == 0
            || self.max_response_bytes > default.max_response_bytes
        {
            return Err(SatelliteValidationError::InvalidLimits);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteCapabilitiesV1 {
    pub session_read: bool,
    pub limits: SatelliteReadLimitsV1,
    /// #2120: serves the hub's full-library QA runs
    /// (`satellite_qa::REQUEST_HUB_QA_RUN_METHOD`). Absent (false) from older
    /// builds, which the hub treats as an incompatible build for QA.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub qa_run: bool,
    /// Accepts the hub's `outcomes` on the report pull (why a pulled report
    /// was held or refused). Absent (false) from older builds, which would
    /// refuse the unknown field, so the hub then sends acknowledgements only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub report_outcomes: bool,
    /// #2587: accepts `interrupt_workers` on a hub deploy request. Absent
    /// (false) from older builds, which would refuse the unknown field, so the
    /// hub refuses an interrupting deploy for them instead of sending it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deploy_interrupt: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteIdentityV1 {
    pub wire_version: u16,
    pub installation_id: SatelliteUuidV1,
    pub daemon_incarnation_id: SatelliteUuidV1,
    pub protocol: SatelliteProtocolVersionV1,
    pub capabilities: SatelliteCapabilitiesV1,
    /// Bounded operational health (#1017). Absent from older satellites.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<SatelliteHealthV1>,
}

pub const SATELLITE_MAX_HEALTH_TEXT_BYTES: usize = 64;
/// Bound on the provider CLI names a satellite health report may list.
pub const SATELLITE_MAX_MISSING_PROVIDER_CLIS: usize = 8;

/// Host target reported by `GetSatelliteIdentity` in its health. QA placement
/// compares OS only; architecture is operational information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatellitePlatformV1 {
    pub os: String,
    pub arch: String,
}

/// Disk admission facts sampled by the satellite's job governor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteGovernorDiskV1 {
    pub free_bytes: Option<u64>,
    pub floor_bytes: u64,
    /// Longest continuous disk hold among live governor waiters. `Some(0)`
    /// means holding now; `None` means no disk waiter, not an unknown duration.
    #[serde(default)]
    pub oldest_wait_seconds: Option<u64>,
}

/// Non-secret operational facts a satellite reports beside its identity.
/// Every field is optional or bounded; QA placement also consumes admission
/// and platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteHealthV1 {
    /// #2935: absent on older builds, which cannot take a new hub QA run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<SatellitePlatformV1>,
    /// Cargo package version of the satellite's rsid.
    pub daemon_version: String,
    /// Lowercase hex SHA-256 of the running rsid binary, once computed.
    #[serde(default)]
    pub binary_sha256: Option<String>,
    pub uptime_seconds: u64,
    /// SQLite `user_version` of the satellite database.
    #[serde(default)]
    pub schema_version: Option<i32>,
    /// Sessions currently in a live provider turn.
    pub sessions_running: u32,
    /// Completed workers parked by the same derivation as AgentWorkerRoster.
    /// Absent on older satellites; a reported zero means measured idle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workers_parked: Option<u32>,
    /// Non-terminal daemon jobs (queued or running), including jobs of live turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_jobs: Option<u32>,
    pub sessions_waiting_approval: u32,
    /// Free bytes on the filesystem holding the rsi data directory.
    #[serde(default)]
    pub disk_free_bytes: Option<u64>,
    /// The job governor's disk reading and continuous disk wait; absent on old peers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub governor_disk: Option<SatelliteGovernorDiskV1>,
    /// One-minute load average times 1000 (Linux `/proc/loadavg`).
    #[serde(default)]
    pub load_avg_1m_milli: Option<u32>,
    /// Commit SHA embedded in the running rsid (#1017 slice 2).
    #[serde(default)]
    pub build_sha: Option<String>,
    /// Daemon start time, RFC3339 with nanoseconds.
    #[serde(default)]
    pub started_at: Option<String>,
    /// `rsid-supervisor.sh` or `none`.
    #[serde(default)]
    pub supervisor_mode: Option<String>,
    /// The satellite's most recent deploy (staged, restarting or settled).
    #[serde(default)]
    pub last_deploy: Option<SatelliteDeployStatusV1>,
    /// Provider CLIs (`claude`, `codex`, `agy`) the satellite cannot find
    /// (#1087). Names only, never paths; absent when all are found.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_provider_clis: Vec<String>,
    /// Effective launch choices per manager seat (#1941); absent when the
    /// satellite seats no manager or is older.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manager_launches: Vec<crate::satellite_manager_policy::SatelliteManagerLaunchesV1>,
    /// Each declared project's seat and saved policy, including missing seats.
    /// Separate from the legacy eight-row summary so old hub validators work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager_policies: Option<Vec<crate::satellite_manager_policy::SatelliteManagerLaunchesV1>>,
    /// #2120: whether a hub full-library QA run would enter now, from the
    /// same gates its job meets (build governor floor included). Absent from
    /// satellites that do not serve hub QA runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qa_admission: Option<crate::satellite_qa::SatelliteQaAdmissionV1>,
    /// #2753: the satellite's own provider CLI login probes (`claude`,
    /// `codex`): state, method and check time only, never account text. The
    /// hub shows them per host. Absent from older satellites and while no
    /// probe finished.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_logins: Vec<crate::provider_login::ProviderLoginHealthV1>,
    /// #2747: provider faults the satellite's own launches hit (an auth
    /// rejection, a refused model), for the hub's `when.health_anomaly`.
    /// Bounded by [`SATELLITE_MAX_PROVIDER_FAULTS`]; absent from older satellites.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_faults: Vec<SatelliteProviderFaultV1>,
    /// Latest persisted watchdog trip, including after the daemon restarts.
    /// Historical evidence, not a statement of current reachability; absent
    /// on older satellites or when no watchdog trip has been recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_watchdog_trip: Option<SatelliteWatchdogTripV1>,
    /// Successful satellite-local health-count store read (excludes lock wait).
    /// Absent on old peers; no independent store evidence on an RPC timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_probe_elapsed_ms: Option<u32>,
    /// #4008: this host's model invocations and token totals by provider over
    /// the last hour and 24 hours, so a capacity owner can attribute a shared
    /// account's burn to a host. Counts only; bounded by
    /// [`SATELLITE_MAX_USAGE_ROWS`]; absent from older satellites.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub usage: Vec<ProviderUsageRollupV1>,
}

/// Rollup window label: the last hour.
pub const USAGE_WINDOW_1H: &str = "1h";
/// Rollup window label: the last 24 hours.
pub const USAGE_WINDOW_24H: &str = "24h";

/// Most usage rows a host reports: 16 providers times the two windows.
pub const SATELLITE_MAX_USAGE_ROWS: usize = 32;

/// One provider's invocations and token totals over one window on one host
/// (#4008). Secret-free: a provider name, a window label and counts, never an
/// account, key, prompt or path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUsageRollupV1 {
    pub provider: String,
    /// [`USAGE_WINDOW_1H`] or [`USAGE_WINDOW_24H`].
    pub window: String,
    pub invocations: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub reasoning_tokens: u64,
}

/// Closed watchdog causes: never carry logs, paths or arbitrary error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SatelliteWatchdogProbeV1 {
    RpcTimeout,
    StoreProbeTimeout,
    SchedulerTickStale,
    ReconciliationTickStale,
}

/// One durable watchdog restart, bounded to the four independent probes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteWatchdogTripV1 {
    pub restart_id: SatelliteUuidV1,
    #[serde(with = "canonical_timestamp")]
    pub observed_at: DateTime<Utc>,
    #[serde(with = "canonical_timestamp")]
    pub last_healthy_at: DateTime<Utc>,
    pub failed_probes: Vec<SatelliteWatchdogProbeV1>,
}

impl SatelliteWatchdogTripV1 {
    fn validate(&self) -> Result<(), SatelliteValidationError> {
        if self.restart_id.0.is_nil()
            || self.last_healthy_at > self.observed_at
            || self.failed_probes.is_empty()
            || self.failed_probes.len() > 4
            || self
                .failed_probes
                .iter()
                .enumerate()
                .any(|(index, probe)| self.failed_probes[..index].contains(probe))
        {
            return Err(SatelliteValidationError::InvalidIdentity);
        }
        Ok(())
    }
}

impl TryFrom<crate::rpc::DaemonRestartRecordV1> for SatelliteWatchdogTripV1 {
    type Error = SatelliteValidationError;

    fn try_from(record: crate::rpc::DaemonRestartRecordV1) -> Result<Self, Self::Error> {
        let failed_probes = record
            .failed_probes
            .iter()
            .map(|code| match code.as_str() {
                "rpc_timeout" => Ok(SatelliteWatchdogProbeV1::RpcTimeout),
                "store_probe_timeout" => Ok(SatelliteWatchdogProbeV1::StoreProbeTimeout),
                "scheduler_tick_stale" => Ok(SatelliteWatchdogProbeV1::SchedulerTickStale),
                "reconciliation_tick_stale" => {
                    Ok(SatelliteWatchdogProbeV1::ReconciliationTickStale)
                }
                _ => Err(SatelliteValidationError::InvalidIdentity),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let trip = Self {
            restart_id: SatelliteUuidV1(record.id),
            observed_at: record.observed_at,
            last_healthy_at: record.last_healthy_at,
            failed_probes,
        };
        trip.validate()?;
        Ok(trip)
    }
}

/// Most provider faults a satellite health report carries.
pub const SATELLITE_MAX_PROVIDER_FAULTS: usize = 8;
/// [`SatelliteProviderFaultV1::kind`]: the provider rejected the credential.
pub const SATELLITE_FAULT_AUTH: &str = "auth_failure";
/// [`SatelliteProviderFaultV1::kind`]: the provider refused a model.
pub const SATELLITE_FAULT_MODEL_UNAVAILABLE: &str = "model_unavailable";

/// One provider fault on a satellite (#2747): which provider, which fault and,
/// for a refused model, which model. Names only, never a message or a key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteProviderFaultV1 {
    pub provider: String,
    /// [`SATELLITE_FAULT_AUTH`] or [`SATELLITE_FAULT_MODEL_UNAVAILABLE`].
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// The newest `agent_deploys` row of a satellite, without any path or reason
/// text: id, state and the commit it was built from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteDeployStatusV1 {
    pub deploy_id: SatelliteUuidV1,
    pub state: crate::agent_deploy::DeployState,
    pub sha: String,
    /// #2056: while the deploy is `staged`, what holds its quiet point (ids
    /// only, no text; at most `SATELLITE_MAX_DEPLOY_BLOCKERS` kinds).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<crate::agent_deploy::DeployBlockerV1>,
    /// #2056: RFC3339 nanos the staged deploy was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_since: Option<String>,
    /// RFC3339 nanos the deploy settled (`succeeded`, `failed` or `timed_out`);
    /// absent while it is `staged` or `restarting`, and from an older daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
}

/// Most blocker kinds a satellite deploy status carries.
pub const SATELLITE_MAX_DEPLOY_BLOCKERS: usize = 8;

impl SatelliteHealthV1 {
    /// # Errors
    /// Returns an error when a text field is unbounded or not printable ASCII.
    pub fn validate(&self) -> Result<(), SatelliteValidationError> {
        let text_ok = |value: &str| {
            !value.is_empty()
                && value.len() <= SATELLITE_MAX_HEALTH_TEXT_BYTES
                && value.bytes().all(|byte| byte.is_ascii_graphic())
        };
        if self
            .store_probe_elapsed_ms
            .is_some_and(|ms| ms > crate::satellite_diagnostics::MAX_STAGE_ELAPSED_MS)
        {
            return Err(SatelliteValidationError::InvalidIdentity);
        }
        if let Some(trip) = &self.latest_watchdog_trip {
            trip.validate()?;
        }
        if !text_ok(&self.daemon_version) {
            return Err(SatelliteValidationError::InvalidIdentity);
        }
        if let Some(platform) = &self.platform
            && (!text_ok(&platform.os) || !text_ok(&platform.arch))
        {
            return Err(SatelliteValidationError::InvalidIdentity);
        }
        for text in [&self.build_sha, &self.started_at, &self.supervisor_mode]
            .into_iter()
            .flatten()
        {
            if !text_ok(text) {
                return Err(SatelliteValidationError::InvalidIdentity);
            }
        }
        if self.missing_provider_clis.len() > SATELLITE_MAX_MISSING_PROVIDER_CLIS
            || !self.missing_provider_clis.iter().all(|name| text_ok(name))
        {
            return Err(SatelliteValidationError::InvalidIdentity);
        }
        if self.provider_logins.len() > crate::provider_login::PROVIDER_LOGIN_NAMES.len()
            || !self.provider_logins.iter().all(|login| login.is_valid())
        {
            return Err(SatelliteValidationError::InvalidIdentity);
        }
        if self.provider_faults.len() > SATELLITE_MAX_PROVIDER_FAULTS
            || self.provider_faults.iter().any(|fault| {
                !text_ok(&fault.provider)
                    || ![SATELLITE_FAULT_AUTH, SATELLITE_FAULT_MODEL_UNAVAILABLE]
                        .contains(&fault.kind.as_str())
                    || fault.model.as_deref().is_some_and(|model| !text_ok(model))
            })
        {
            return Err(SatelliteValidationError::InvalidIdentity);
        }
        if self.usage.len() > SATELLITE_MAX_USAGE_ROWS
            || self.usage.iter().any(|row| {
                !text_ok(&row.provider)
                    || ![USAGE_WINDOW_1H, USAGE_WINDOW_24H].contains(&row.window.as_str())
            })
        {
            return Err(SatelliteValidationError::InvalidIdentity);
        }
        if self.manager_launches.len()
            > crate::satellite_manager_policy::SATELLITE_MAX_MANAGER_LAUNCH_ROWS
            || self.manager_policies.as_ref().is_some_and(|rows| {
                rows.len() > crate::satellite_dispatch::SATELLITE_INBOUND_MAX_ENTRIES
            })
        {
            return Err(SatelliteValidationError::InvalidIdentity);
        }
        if let Some(deploy) = &self.last_deploy
            && !(deploy.sha.len() == 40
                && deploy
                    .sha
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        {
            return Err(SatelliteValidationError::InvalidIdentity);
        }
        if let Some(deploy) = &self.last_deploy
            && (deploy.blockers.len() > SATELLITE_MAX_DEPLOY_BLOCKERS
                || deploy.blockers.iter().any(|blocker| {
                    !text_ok(&blocker.kind)
                        || blocker.subjects.len() > crate::agent_deploy::DEPLOY_BLOCKER_SUBJECTS_MAX
                        || blocker.subjects.iter().any(|subject| {
                            [&subject.queue_state, &subject.job_kind, &subject.since]
                                .into_iter()
                                .flatten()
                                .any(|text| !text_ok(text))
                        })
                })
                || deploy
                    .waiting_since
                    .as_deref()
                    .is_some_and(|at| !text_ok(at)))
        {
            return Err(SatelliteValidationError::InvalidIdentity);
        }
        if let Some(digest) = &self.binary_sha256
            && !(digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        {
            return Err(SatelliteValidationError::InvalidIdentity);
        }
        Ok(())
    }
}

impl SatelliteIdentityV1 {
    /// # Errors
    /// Returns an error for an unsupported wire version, nil identity, or invalid read limits.
    pub fn validate(&self) -> Result<(), SatelliteValidationError> {
        if self.wire_version != SATELLITE_WIRE_VERSION_V1 {
            return Err(SatelliteValidationError::UnsupportedWireVersion(
                self.wire_version,
            ));
        }
        if self.installation_id.0.is_nil() || self.daemon_incarnation_id.0.is_nil() {
            return Err(SatelliteValidationError::InvalidIdentity);
        }
        self.capabilities.limits.validate()?;
        if let Some(health) = &self.health {
            health.validate()?;
        }
        Ok(())
    }

    /// Minor versions are additive; a major mismatch or absent read feature is
    /// incompatible for the bounded session-list operation.
    #[must_use]
    pub const fn read_compatibility(&self) -> SatelliteCompatibility {
        if self.protocol.major != SATELLITE_PROTOCOL_MAJOR {
            SatelliteCompatibility::IncompatibleMajor {
                found: self.protocol.major,
                supported: SATELLITE_PROTOCOL_MAJOR,
            }
        } else if !self.capabilities.session_read {
            SatelliteCompatibility::MissingSessionRead
        } else {
            SatelliteCompatibility::Compatible
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SatelliteCompatibility {
    Compatible,
    MissingSessionRead,
    IncompatibleMajor { found: u16, supported: u16 },
}

/// Hub-owned registration identity combined with a peer-local session UUID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SatelliteSessionKeyV1 {
    pub peer_id: SatelliteUuidV1,
    pub remote_session_id: SatelliteUuidV1,
}

/// Operator-owned registry data. The socket path names a listener on this hub;
/// it does not grant the remote daemon any authority by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SatelliteLinkDirectionV1 {
    DialHomeReverse,
    DirectLocalForward,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatelliteLinkConfigV1 {
    pub link_id: SatelliteUuidV1,
    pub direction: SatelliteLinkDirectionV1,
    pub socket_path: String,
    pub ssh_target: Option<String>,
    pub trust_reference: String,
    pub enabled: bool,
    pub priority: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatellitePeerConfigV1 {
    pub peer_id: SatelliteUuidV1,
    pub label: String,
    pub expected_installation_id: Option<SatelliteUuidV1>,
    pub enabled: bool,
    pub read_enabled: bool,
    /// Operator switch for queued hub-manager delivery (#1017 slice 3).
    /// Default off; it requires an enabled, paired, read-enabled peer.
    #[serde(default)]
    pub dispatch_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatellitePutPeerRequestV1 {
    pub expected_registry_revision: u64,
    pub peer: SatellitePeerConfigV1,
    /// An explicit operator acknowledgement after repairing SSH trust or
    /// replacing a peer. Metadata and link edits preserve quarantine.
    #[serde(default)]
    pub repair_quarantine: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatellitePutLinkRequestV1 {
    pub expected_registry_revision: u64,
    pub peer_id: SatelliteUuidV1,
    pub link: SatelliteLinkConfigV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatelliteObservationV1 {
    /// Activity from the last reported health, unknown until a successful poll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions_running: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workers_parked: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_jobs: Option<u32>,
    /// Process-local platform from the last successful identity poll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<SatellitePlatformV1>,
    pub state: String,
    pub installation_id: Option<SatelliteUuidV1>,
    pub incarnation_id: Option<SatelliteUuidV1>,
    #[serde(with = "canonical_optional_timestamp")]
    pub last_observed_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub cached_session_count: u32,
    /// Cached data is stale after a hub restart until a fresh identity check.
    pub stale: bool,
    /// Effective launch choices per satellite manager seat, from the satellite's
    /// last reported health (#1941). Empty when none is known.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manager_launches: Vec<crate::satellite_manager_policy::SatelliteManagerLaunchesV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatellitePeerV1 {
    pub config: SatellitePeerConfigV1,
    pub row_version: u64,
    pub links: Vec<SatelliteLinkConfigV1>,
    pub observation: Option<SatelliteObservationV1>,
    /// Operator-declared remote sessions the hub manager may message.
    #[serde(default)]
    pub dispatch_scope: Vec<SatelliteUuidV1>,
    /// Operator-declared remote projects (#1928): the project's current
    /// appointed manager (and its descendants) is the target, resolved live by
    /// the satellite. The hub verifies only the declared project id.
    #[serde(default)]
    pub dispatch_project_scope: Vec<SatelliteUuidV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatelliteRegistryV1 {
    pub revision: u64,
    pub peers: Vec<SatellitePeerV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatelliteHubSessionV1 {
    pub key: SatelliteSessionKeyV1,
    pub summary: SatelliteSessionSummaryV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatelliteHubSessionsPageV1 {
    pub peer_id: SatelliteUuidV1,
    pub observation: SatelliteObservationV1,
    pub offset: u32,
    pub sessions: Vec<SatelliteHubSessionV1>,
    pub next_offset: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatelliteHubSessionsRequestV1 {
    pub peer_id: SatelliteUuidV1,
    pub offset: u32,
    pub limit: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatelliteProbeLinkRequestV1 {
    pub peer_id: SatelliteUuidV1,
    pub link_id: SatelliteUuidV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatelliteProbeLinkResultV1 {
    pub peer_id: SatelliteUuidV1,
    pub link_id: SatelliteUuidV1,
    pub direction: SatelliteLinkDirectionV1,
    pub trust_reference: String,
    pub identity: SatelliteIdentityV1,
    /// None when the peer is not paired yet; false is a continuity conflict.
    pub matches_expected: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteSessionSummaryV1 {
    pub session_id: SatelliteUuidV1,
    pub title: Option<String>,
    pub provider: SessionProvider,
    pub status: SessionStatus,
    /// Display metadata from the remote host; never a local filesystem path.
    pub working_dir: Option<String>,
    #[serde(with = "canonical_timestamp")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "canonical_timestamp")]
    pub updated_at: DateTime<Utc>,
}

impl SatelliteSessionSummaryV1 {
    /// # Errors
    /// Returns an error when remote display metadata exceeds a limit or contains controls.
    pub fn validate(&self, limits: &SatelliteReadLimitsV1) -> Result<(), SatelliteValidationError> {
        if self.session_id.0.is_nil() {
            return Err(SatelliteValidationError::InvalidIdentity);
        }
        if self.updated_at < self.created_at {
            return Err(SatelliteValidationError::InvalidTimestampOrder);
        }
        validate_display_text(self.title.as_deref(), usize::from(limits.max_title_bytes))?;
        validate_display_text(
            self.working_dir.as_deref(),
            usize::from(limits.max_working_dir_bytes),
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteSessionPageRequestV1 {
    pub wire_version: u16,
    pub limit: u16,
    pub cursor: Option<String>,
}

impl SatelliteSessionPageRequestV1 {
    /// # Errors
    /// Returns an error for an unsupported version, invalid limit, or invalid cursor.
    pub fn validate(&self, limits: &SatelliteReadLimitsV1) -> Result<(), SatelliteValidationError> {
        if self.wire_version != SATELLITE_WIRE_VERSION_V1 {
            return Err(SatelliteValidationError::UnsupportedWireVersion(
                self.wire_version,
            ));
        }
        if self.limit == 0 || self.limit > limits.max_page_size {
            return Err(SatelliteValidationError::InvalidPageLimit);
        }
        if self.cursor.as_ref().is_some_and(|cursor| {
            cursor.is_empty() || cursor.len() > usize::from(limits.max_cursor_bytes)
        }) {
            return Err(SatelliteValidationError::InvalidCursor);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteSessionPageV1 {
    pub wire_version: u16,
    pub installation_id: SatelliteUuidV1,
    pub daemon_incarnation_id: SatelliteUuidV1,
    pub snapshot_id: SatelliteUuidV1,
    pub snapshot_total_sessions: u32,
    pub snapshot_offset: u32,
    #[serde(with = "canonical_timestamp")]
    pub observed_at: DateTime<Utc>,
    pub sessions: Vec<SatelliteSessionSummaryV1>,
    pub next_cursor: Option<String>,
}

impl SatelliteSessionPageV1 {
    /// # Errors
    /// Returns an error for invalid pagination, limits, duplicate rows, or unsafe display text.
    pub fn validate(&self, limits: &SatelliteReadLimitsV1) -> Result<(), SatelliteValidationError> {
        if self.wire_version != SATELLITE_WIRE_VERSION_V1 {
            return Err(SatelliteValidationError::UnsupportedWireVersion(
                self.wire_version,
            ));
        }
        if self.installation_id.0.is_nil()
            || self.daemon_incarnation_id.0.is_nil()
            || self.snapshot_id.0.is_nil()
        {
            return Err(SatelliteValidationError::InvalidIdentity);
        }
        if self.sessions.len() > usize::from(limits.max_page_size) {
            return Err(SatelliteValidationError::PageTooLarge);
        }
        let rows = u32::try_from(self.sessions.len())
            .map_err(|_| SatelliteValidationError::InvalidSnapshotCount)?;
        let page_end = self
            .snapshot_offset
            .checked_add(rows)
            .ok_or(SatelliteValidationError::InvalidSnapshotCount)?;
        if self.snapshot_total_sessions > limits.max_sessions
            || page_end > self.snapshot_total_sessions
        {
            return Err(SatelliteValidationError::InvalidSnapshotCount);
        }
        if self.sessions.is_empty() && page_end < self.snapshot_total_sessions {
            return Err(SatelliteValidationError::InvalidPagination);
        }
        if self.next_cursor.as_ref().is_some_and(|cursor| {
            cursor.is_empty() || cursor.len() > usize::from(limits.max_cursor_bytes)
        }) {
            return Err(SatelliteValidationError::InvalidCursor);
        }
        if self.next_cursor.is_some() == (page_end == self.snapshot_total_sessions) {
            return Err(SatelliteValidationError::InvalidPagination);
        }
        let mut session_ids = HashSet::with_capacity(self.sessions.len());
        for session in &self.sessions {
            if !session_ids.insert(session.session_id) {
                return Err(SatelliteValidationError::DuplicateSessionId);
            }
            session.validate(limits)?;
        }
        Ok(())
    }
}

/// Reject an oversized payload before JSON parsing allocates its value tree.
/// # Errors
/// Returns an error when the payload exceeds the local byte ceiling or is invalid JSON.
pub fn decode_satellite_json<T: for<'de> Deserialize<'de>>(
    bytes: &[u8],
) -> Result<T, SatelliteDecodeError> {
    decode_satellite_json_with_limit(bytes, SATELLITE_MAX_RESPONSE_BYTES)
}

/// Decode with the peer-advertised response limit, capped by the local ceiling.
/// # Errors
/// Returns an error when the payload exceeds either byte ceiling or is invalid JSON.
pub fn decode_satellite_json_with_limit<T: for<'de> Deserialize<'de>>(
    bytes: &[u8],
    peer_limit: usize,
) -> Result<T, SatelliteDecodeError> {
    if bytes.len() > peer_limit.min(SATELLITE_MAX_RESPONSE_BYTES) {
        return Err(SatelliteDecodeError::PayloadTooLarge);
    }
    serde_json::from_slice(bytes).map_err(SatelliteDecodeError::Json)
}

fn validate_display_text(
    value: Option<&str>,
    max_bytes: usize,
) -> Result<(), SatelliteValidationError> {
    if let Some(value) = value
        && (value.len() > max_bytes || value.chars().any(char::is_control))
    {
        return Err(SatelliteValidationError::InvalidDisplayText);
    }
    Ok(())
}

mod canonical_timestamp {
    use chrono::{DateTime, SecondsFormat, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S>(value: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_rfc3339_opts(SecondsFormat::Nanos, true))
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let parsed = DateTime::parse_from_rfc3339(&value).map_err(serde::de::Error::custom)?;
        let utc = parsed.with_timezone(&Utc);
        if utc.to_rfc3339_opts(SecondsFormat::Nanos, true) != value {
            return Err(serde::de::Error::custom(
                "timestamp must be canonical RFC3339 UTC with nanoseconds",
            ));
        }
        Ok(utc)
    }
}

mod canonical_optional_timestamp {
    use chrono::{DateTime, SecondsFormat, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S>(
        value: &Option<DateTime<Utc>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(time) => {
                serializer.serialize_some(&time.to_rfc3339_opts(SecondsFormat::Nanos, true))
            }
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Option<DateTime<Utc>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let Some(value) = Option::<String>::deserialize(deserializer)? else {
            return Ok(None);
        };
        let parsed = DateTime::parse_from_rfc3339(&value).map_err(serde::de::Error::custom)?;
        let utc = parsed.with_timezone(&Utc);
        if utc.to_rfc3339_opts(SecondsFormat::Nanos, true) != value {
            return Err(serde::de::Error::custom(
                "timestamp must be canonical RFC3339 UTC with nanoseconds",
            ));
        }
        Ok(Some(utc))
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SatelliteValidationError {
    #[error("unsupported satellite wire version {0}")]
    UnsupportedWireVersion(u16),
    #[error("satellite identity must be a non-nil UUID")]
    InvalidIdentity,
    #[error("satellite read limits are outside supported bounds")]
    InvalidLimits,
    #[error("satellite page limit is outside supported bounds")]
    InvalidPageLimit,
    #[error("satellite cursor is empty or exceeds its byte limit")]
    InvalidCursor,
    #[error("satellite page exceeds its declared row limit")]
    PageTooLarge,
    #[error("satellite snapshot count is outside supported bounds")]
    InvalidSnapshotCount,
    #[error("satellite snapshot offset and cursor do not describe a valid page")]
    InvalidPagination,
    #[error("satellite page contains duplicate session ids")]
    DuplicateSessionId,
    #[error("satellite display text exceeds its byte limit or contains controls")]
    InvalidDisplayText,
    #[error("satellite session timestamps are out of order")]
    InvalidTimestampOrder,
}

#[derive(Debug, Error)]
pub enum SatelliteDecodeError {
    #[error("satellite payload exceeds the response byte limit")]
    PayloadTooLarge,
    #[error("invalid satellite JSON: {0}")]
    Json(#[source] serde_json::Error),
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn uuid(value: &str) -> SatelliteUuidV1 {
        SatelliteUuidV1(Uuid::parse_str(value).unwrap())
    }

    fn identity() -> SatelliteIdentityV1 {
        SatelliteIdentityV1 {
            wire_version: SATELLITE_WIRE_VERSION_V1,
            installation_id: uuid("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
            daemon_incarnation_id: uuid("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"),
            protocol: SatelliteProtocolVersionV1 {
                major: SATELLITE_PROTOCOL_MAJOR,
                minor: SATELLITE_PROTOCOL_MINOR,
            },
            capabilities: SatelliteCapabilitiesV1 {
                session_read: true,
                limits: SatelliteReadLimitsV1::default(),
                qa_run: false,
                report_outcomes: false,
                deploy_interrupt: false,
            },
            health: None,
        }
    }

    fn session(id: SatelliteUuidV1) -> SatelliteSessionSummaryV1 {
        SatelliteSessionSummaryV1 {
            session_id: id,
            title: Some("build task".into()),
            provider: SessionProvider::Codex,
            status: SessionStatus::Running,
            working_dir: Some("/remote/work/tree".into()),
            created_at: DateTime::from_timestamp(1_790_000_000, 0).unwrap(),
            updated_at: DateTime::from_timestamp(1_790_000_001, 0).unwrap(),
        }
    }

    fn page() -> SatelliteSessionPageV1 {
        SatelliteSessionPageV1 {
            wire_version: SATELLITE_WIRE_VERSION_V1,
            installation_id: identity().installation_id,
            daemon_incarnation_id: identity().daemon_incarnation_id,
            snapshot_id: uuid("cccccccc-cccc-4ccc-8ccc-cccccccccccc"),
            snapshot_total_sessions: 2,
            snapshot_offset: 0,
            observed_at: DateTime::from_timestamp(1_790_000_002, 0).unwrap(),
            sessions: vec![session(uuid("dddddddd-dddd-4ddd-8ddd-dddddddddddd"))],
            next_cursor: Some("opaque-v1".into()),
        }
    }

    #[test]
    fn governor_disk_health_is_optional_and_round_trips() {
        let mut wire = json!({
            "daemon_version": "1.0.0", "uptime_seconds": 1,
            "sessions_running": 0, "sessions_waiting_approval": 0
        });
        let old: SatelliteHealthV1 = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(old.governor_disk, None);
        wire["governor_disk"] = json!({
            "free_bytes": 27_u64 << 30, "floor_bytes": 30_u64 << 30,
            "oldest_wait_seconds": 2400
        });
        let health: SatelliteHealthV1 = serde_json::from_value(wire.clone()).unwrap();
        health.validate().unwrap();
        assert_eq!(
            health.governor_disk.as_ref().unwrap().oldest_wait_seconds,
            Some(2400)
        );
        assert_eq!(
            serde_json::to_value(health).unwrap()["governor_disk"],
            wire["governor_disk"]
        );
    }

    #[test]
    fn activity_counts_preserve_unknown_and_round_trip_reported_counts() {
        let legacy = json!({
            "daemon_version": "1.0.0", "uptime_seconds": 1,
            "sessions_running": 1, "sessions_waiting_approval": 0
        });
        let old: SatelliteHealthV1 = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(old.workers_parked, None);
        assert_eq!(old.open_jobs, None);
        for count in [0, 2] {
            let mut wire = legacy.clone();
            wire["workers_parked"] = json!(count);
            wire["open_jobs"] = json!(count);
            let health: SatelliteHealthV1 = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(health.sessions_running, 1);
            assert_eq!(health.workers_parked, Some(count));
            assert_eq!(health.open_jobs, Some(count));
            assert_eq!(
                serde_json::to_value(health).unwrap()["workers_parked"],
                count
            );
        }
    }

    #[test]
    fn usage_rollup_is_optional_bounded_and_validated_4008() {
        let mut wire = serde_json::to_value(identity()).unwrap();
        wire["health"] = json!({
            "daemon_version": "1.0.0", "uptime_seconds": 1,
            "sessions_running": 0, "sessions_waiting_approval": 0
        });
        let legacy: SatelliteIdentityV1 = serde_json::from_value(wire.clone()).unwrap();
        legacy.validate().unwrap();
        let legacy_health = legacy.health.unwrap();
        assert!(legacy_health.usage.is_empty());
        assert!(
            serde_json::to_value(&legacy_health)
                .unwrap()
                .get("usage")
                .is_none()
        );
        let row = |provider: &str, window: &str| {
            json!({"provider": provider, "window": window, "invocations": 9,
                   "input_tokens": 1, "output_tokens": 2, "cache_creation_tokens": 3,
                   "cache_read_tokens": 6_300_000, "reasoning_tokens": 4})
        };
        wire["health"]["usage"] = json!([row("Claude", "1h"), row("Claude", "24h")]);
        let reported: SatelliteIdentityV1 = serde_json::from_value(wire.clone()).unwrap();
        reported.validate().unwrap();
        let usage = reported.health.unwrap().usage;
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].cache_read_tokens, 6_300_000);
        for bad in [row("Claude", "7d"), row("", "1h"), row("has space", "1h")] {
            wire["health"]["usage"] = json!([bad]);
            let invalid: SatelliteIdentityV1 = serde_json::from_value(wire.clone()).unwrap();
            assert!(invalid.validate().is_err());
        }
        wire["health"]["usage"] = json!(vec![row("Claude", "1h"); SATELLITE_MAX_USAGE_ROWS + 1]);
        let excessive: SatelliteIdentityV1 = serde_json::from_value(wire).unwrap();
        assert!(excessive.validate().is_err());
    }

    #[test]
    fn watchdog_health_round_trips_and_old_peers_remain_compatible() {
        let mut wire = serde_json::to_value(identity()).unwrap();
        wire["health"] = json!({
            "daemon_version": "1.0.0", "uptime_seconds": 1,
            "sessions_running": 0, "sessions_waiting_approval": 0
        });
        let legacy: SatelliteIdentityV1 = serde_json::from_value(wire.clone()).unwrap();
        legacy.validate().unwrap();
        let legacy_health = legacy.health.unwrap();
        assert_eq!(legacy_health.latest_watchdog_trip, None);
        assert_eq!(legacy_health.store_probe_elapsed_ms, None);
        wire["health"]["store_probe_elapsed_ms"] = json!(17);
        let measured: SatelliteIdentityV1 = serde_json::from_value(wire.clone()).unwrap();
        measured.validate().unwrap();
        assert_eq!(measured.health.unwrap().store_probe_elapsed_ms, Some(17));
        let mut excessive = wire.clone();
        excessive["health"]["store_probe_elapsed_ms"] = json!(300_001);
        assert!(
            serde_json::from_value::<SatelliteIdentityV1>(excessive)
                .unwrap()
                .validate()
                .is_err()
        );
        wire["health"]["latest_watchdog_trip"] = json!({
            "restart_id": "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
            "observed_at": "2026-10-19T04:17:24.000000000Z",
            "last_healthy_at": "2026-10-19T04:15:24.000000000Z",
            "failed_probes": ["rpc_timeout", "store_probe_timeout"]
        });
        let decoded: SatelliteIdentityV1 = serde_json::from_value(wire.clone()).unwrap();
        decoded.validate().unwrap();
        assert_eq!(
            serde_json::to_value(decoded).unwrap()["health"]["latest_watchdog_trip"],
            wire["health"]["latest_watchdog_trip"]
        );
        // Only closed causes and canonical identity/time enter the hub cache.
        for (key, value) in [
            ("restart_id", json!("CCCCCCCC-CCCC-4CCC-8CCC-CCCCCCCCCCCC")),
            ("observed_at", json!("2026-10-19T04:17:24Z")),
            ("failed_probes", json!(["arbitrary remote error text"])),
        ] {
            let mut invalid = wire.clone();
            invalid["health"]["latest_watchdog_trip"][key] = value;
            assert!(serde_json::from_value::<SatelliteIdentityV1>(invalid).is_err());
        }
        for (key, value) in [
            ("restart_id", json!("00000000-0000-0000-0000-000000000000")),
            ("last_healthy_at", json!("2026-10-19T04:18:24.000000000Z")),
            ("failed_probes", json!([])),
            ("failed_probes", json!(["rpc_timeout", "rpc_timeout"])),
            ("failed_probes", json!(vec!["rpc_timeout"; 5])),
        ] {
            let mut invalid = wire.clone();
            invalid["health"]["latest_watchdog_trip"][key] = value;
            let invalid: SatelliteIdentityV1 = serde_json::from_value(invalid).unwrap();
            assert_eq!(
                invalid.validate(),
                Err(SatelliteValidationError::InvalidIdentity)
            );
        }
    }

    #[test]
    fn identity_and_page_round_trip_with_canonical_identity() {
        let identity = identity();
        identity.validate().unwrap();
        let encoded = serde_json::to_vec(&identity).unwrap();
        let decoded: SatelliteIdentityV1 = decode_satellite_json(&encoded).unwrap();
        assert_eq!(decoded, identity);
        assert_eq!(
            decoded.read_compatibility(),
            SatelliteCompatibility::Compatible
        );

        let page = page();
        page.validate(&identity.capabilities.limits).unwrap();
        let encoded = serde_json::to_vec(&page).unwrap();
        let decoded: SatelliteSessionPageV1 = decode_satellite_json(&encoded).unwrap();
        assert_eq!(decoded, page);
    }

    #[test]
    fn rejects_noncanonical_uuid_and_unknown_enum_values() {
        let mut upper = serde_json::to_value(identity()).unwrap();
        upper["installation_id"] = json!("AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA");
        assert!(serde_json::from_value::<SatelliteIdentityV1>(upper).is_err());

        let mut unknown = serde_json::to_value(page()).unwrap();
        unknown["sessions"][0]["status"] = json!("paused");
        assert!(serde_json::from_value::<SatelliteSessionPageV1>(unknown).is_err());

        let mut noncanonical_time = serde_json::to_value(page()).unwrap();
        noncanonical_time["observed_at"] = json!("2026-09-27T04:00:00Z");
        assert!(serde_json::from_value::<SatelliteSessionPageV1>(noncanonical_time).is_err());
    }

    #[test]
    fn rejects_invalid_versions_capability_limits_and_request_bounds() {
        let mut old_wire = identity();
        old_wire.wire_version = 0;
        assert_eq!(
            old_wire.validate(),
            Err(SatelliteValidationError::UnsupportedWireVersion(0))
        );

        let mut incompatible = identity();
        incompatible.protocol.major = SATELLITE_PROTOCOL_MAJOR + 1;
        assert!(matches!(
            incompatible.read_compatibility(),
            SatelliteCompatibility::IncompatibleMajor { .. }
        ));
        incompatible.protocol.major = SATELLITE_PROTOCOL_MAJOR;
        incompatible.protocol.minor += 1;
        assert_eq!(
            incompatible.read_compatibility(),
            SatelliteCompatibility::Compatible
        );
        incompatible.capabilities.session_read = false;
        assert_eq!(
            incompatible.read_compatibility(),
            SatelliteCompatibility::MissingSessionRead
        );

        let limits = SatelliteReadLimitsV1 {
            max_page_size: SATELLITE_MAX_PAGE_SIZE + 1,
            ..SatelliteReadLimitsV1::default()
        };
        assert_eq!(
            limits.validate(),
            Err(SatelliteValidationError::InvalidLimits)
        );
        let request = SatelliteSessionPageRequestV1 {
            wire_version: SATELLITE_WIRE_VERSION_V1,
            limit: SATELLITE_MAX_PAGE_SIZE + 1,
            cursor: None,
        };
        assert_eq!(
            request.validate(&SatelliteReadLimitsV1::default()),
            Err(SatelliteValidationError::InvalidPageLimit)
        );
    }

    #[test]
    fn rejects_duplicate_rows_bad_display_fields_and_oversized_payloads() {
        let mut nil_identity = identity();
        nil_identity.installation_id = SatelliteUuidV1(Uuid::nil());
        assert_eq!(
            nil_identity.validate(),
            Err(SatelliteValidationError::InvalidIdentity)
        );

        let mut empty_page = page();
        empty_page.sessions.clear();
        assert_eq!(
            empty_page.validate(&SatelliteReadLimitsV1::default()),
            Err(SatelliteValidationError::InvalidPagination)
        );

        let mut duplicate_page = page();
        duplicate_page
            .sessions
            .push(duplicate_page.sessions[0].clone());
        duplicate_page.next_cursor = None;
        assert_eq!(
            duplicate_page.validate(&SatelliteReadLimitsV1::default()),
            Err(SatelliteValidationError::DuplicateSessionId)
        );

        let mut impossible_snapshot = page();
        impossible_snapshot.snapshot_total_sessions = 0;
        assert_eq!(
            impossible_snapshot.validate(&SatelliteReadLimitsV1::default()),
            Err(SatelliteValidationError::InvalidSnapshotCount)
        );

        let mut page = page();
        page.sessions[0].title = Some("bad\nlabel".into());
        assert_eq!(
            page.validate(&SatelliteReadLimitsV1::default()),
            Err(SatelliteValidationError::InvalidDisplayText)
        );

        let oversized = vec![b' '; SATELLITE_MAX_RESPONSE_BYTES + 1];
        assert!(matches!(
            decode_satellite_json::<SatelliteSessionPageV1>(&oversized),
            Err(SatelliteDecodeError::PayloadTooLarge)
        ));
        let small_payload = b"{}";
        assert!(matches!(
            decode_satellite_json_with_limit::<SatelliteIdentityV1>(small_payload, 1),
            Err(SatelliteDecodeError::PayloadTooLarge)
        ));
    }

    #[test]
    fn identity_platform_round_trips_new_and_older_health() {
        let mut old = identity();
        old.health = Some(
            serde_json::from_value(json!({
                "daemon_version": "1.0.0",
                "uptime_seconds": 1,
                "sessions_running": 0,
                "sessions_waiting_approval": 0
            }))
            .unwrap(),
        );
        let old_wire = serde_json::to_value(&old).unwrap();
        assert!(old_wire["health"].get("platform").is_none());
        let decoded: SatelliteIdentityV1 = serde_json::from_value(old_wire.clone()).unwrap();
        decoded.validate().unwrap();
        assert_eq!(decoded, old);
        assert_eq!(serde_json::to_value(decoded).unwrap(), old_wire);

        let mut new_wire = old_wire;
        new_wire["health"]["platform"] = json!({"os": "linux", "arch": "aarch64"});
        let decoded: SatelliteIdentityV1 = serde_json::from_value(new_wire.clone()).unwrap();
        decoded.validate().unwrap();
        assert_eq!(
            decoded.health.as_ref().unwrap().platform,
            Some(SatellitePlatformV1 {
                os: "linux".into(),
                arch: "aarch64".into()
            })
        );
        assert_eq!(serde_json::to_value(decoded).unwrap(), new_wire);
    }

    #[test]
    fn report_outcomes_capability_is_absent_when_false_and_defaults_off() {
        let mut older = identity();
        let value = serde_json::to_value(&older).unwrap();
        assert!(
            value["capabilities"]
                .as_object()
                .unwrap()
                .get("report_outcomes")
                .is_none()
        );
        // An older satellite's identity (no key) decodes as not advertising it.
        let back: SatelliteIdentityV1 = serde_json::from_value(value).unwrap();
        assert!(!back.capabilities.report_outcomes);
        older.capabilities.report_outcomes = true;
        let advertised = serde_json::to_value(&older).unwrap();
        assert_eq!(advertised["capabilities"]["report_outcomes"], true);
    }

    #[test]
    fn deploy_interrupt_capability_is_absent_when_false_and_defaults_off() {
        let mut older = identity();
        let value = serde_json::to_value(&older).unwrap();
        assert!(
            value["capabilities"]
                .as_object()
                .unwrap()
                .get("deploy_interrupt")
                .is_none()
        );
        let back: SatelliteIdentityV1 = serde_json::from_value(value).unwrap();
        assert!(!back.capabilities.deploy_interrupt);
        older.capabilities.deploy_interrupt = true;
        let advertised = serde_json::to_value(&older).unwrap();
        assert_eq!(advertised["capabilities"]["deploy_interrupt"], true);
    }
}
