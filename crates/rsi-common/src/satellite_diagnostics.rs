//! Closed, bounded observations of a hub's forwarded-socket probe. A local
//! connection or service state never establishes remote SSH authentication.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MAX_STAGE_ELAPSED_MS: u32 = 300_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SatelliteStageStateV1 {
    NotAttempted,
    /// Forwarding/SSH internals or a store read without a returned measurement.
    Unobserved,
    Succeeded,
    TimedOut,
    Refused,
    InvalidResponse,
    IoFailure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteStageV1 {
    pub state: SatelliteStageStateV1,
    /// Saturated at MAX_STAGE_ELAPSED_MS; absent if no measurement was made.
    pub elapsed_ms: Option<u32>,
}

impl SatelliteStageV1 {
    pub fn unmeasured(state: SatelliteStageStateV1) -> Self {
        Self {
            state,
            elapsed_ms: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SatelliteTunnelStateV1 {
    /// Reverse tunnel service runs on the satellite, outside hub observation.
    RemoteService,
    /// Registry socket has no canonical installer service mapping.
    Unmapped,
    Unsupported,
    Unavailable,
    TimedOut,
    Active,
    Inactive,
    Activating,
    Deactivating,
    Failed,
    NotFound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SatelliteProbeRpcV1 {
    Identity,
    SessionPage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteReachabilityV1 {
    pub link_id: Uuid,
    pub checked_at: DateTime<Utc>,
    pub elapsed_ms: u32,
    /// Includes local endpoint custody checks; success proves local connect only.
    pub connect: SatelliteStageV1,
    /// Bounded write to the forwarded socket, not a remote SSH/auth handshake.
    pub transport: SatelliteStageV1,
    /// Bounded response plus envelope/identity validation. A timeout cannot
    /// distinguish remote forwarding, scheduling and store-lock delays.
    pub daemon_rpc: SatelliteStageV1,
    pub rpc: SatelliteProbeRpcV1,
    /// Satellite-local health-count read only when the peer returns its timing.
    pub store_probe: SatelliteStageV1,
    /// Always unobserved: SSH authentication belongs to the tunnel service.
    pub auth: SatelliteStageV1,
    pub tunnel_service: SatelliteTunnelStateV1,
    pub tunnel_service_elapsed_ms: Option<u32>,
}
