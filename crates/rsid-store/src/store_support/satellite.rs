//! Satellite limits and cached-read types the store and manager ledger use
//! (moved down from `satellite::{poll, registry}` / `satellite`, which
//! re-export them).

use chrono::{DateTime, Utc};
use rsi_common::satellite::{SatelliteHealthV1, SatellitePlatformV1, SatelliteSessionSummaryV1};
use std::sync::LazyLock;
use uuid::Uuid;

pub const MAX_REGISTERED_PEERS: usize = 128;
pub const MAX_CACHED_ROWS: usize = 1_000;
pub const MAX_CACHED_BYTES: usize = 1_048_576;

#[derive(Debug)]
pub struct PeerSnapshot {
    pub installation_id: Uuid,
    pub incarnation_id: Uuid,
    pub sessions: Vec<SatelliteSessionSummaryV1>,
    /// Health the satellite reported on its identity, if it reports any.
    pub health: Option<SatelliteHealthV1>,
}

/// Last health each peer reported, with the time the hub received it. It is
/// process-local by design: after a hub restart the value is unknown until the
/// next successful poll, so no migration or stale row can present old health.
#[derive(Debug, Clone)]
pub struct ReportedHealth {
    pub(crate) received_at: DateTime<Utc>,
    pub(crate) health: SatelliteHealthV1,
}

static REPORTED_HEALTH: LazyLock<
    std::sync::Mutex<std::collections::HashMap<Uuid, ReportedHealth>>,
> = LazyLock::new(Default::default);

/// Record (or clear, for a satellite that reports none) a peer's health.
pub fn record_reported_health(peer_id: Uuid, health: Option<&SatelliteHealthV1>) {
    let Ok(mut cache) = REPORTED_HEALTH.lock() else {
        return;
    };
    match health {
        Some(health) => {
            cache.insert(
                peer_id,
                ReportedHealth {
                    received_at: Utc::now(),
                    health: health.clone(),
                },
            );
        }
        None => {
            cache.remove(&peer_id);
        }
    }
}

pub fn reported_health(peer_id: Uuid) -> Option<ReportedHealth> {
    REPORTED_HEALTH.lock().ok()?.get(&peer_id).cloned()
}

static REACHABILITY: LazyLock<
    std::sync::Mutex<
        std::collections::HashMap<Uuid, rsi_common::satellite_diagnostics::SatelliteReachabilityV1>,
    >,
> = LazyLock::new(Default::default);

/// One latest attempt per peer, bounded independently of transient registry churn.
pub fn record_reachability(
    peer_id: Uuid,
    diagnostics: rsi_common::satellite_diagnostics::SatelliteReachabilityV1,
) {
    let Ok(mut cache) = REACHABILITY.lock() else {
        return;
    };
    if cache
        .get(&peer_id)
        .is_some_and(|current| current.checked_at > diagnostics.checked_at)
    {
        return;
    }
    if cache.len() >= MAX_REGISTERED_PEERS && !cache.contains_key(&peer_id) {
        if let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, value)| value.checked_at)
            .map(|(id, _)| *id)
        {
            cache.remove(&oldest);
        }
    }
    cache.insert(peer_id, diagnostics);
}

pub fn reachability(
    peer_id: Uuid,
) -> Option<rsi_common::satellite_diagnostics::SatelliteReachabilityV1> {
    REACHABILITY.lock().ok()?.get(&peer_id).cloned()
}

pub fn latest_watchdog_trip(
    peer_id: Uuid,
) -> Option<rsi_common::satellite::SatelliteWatchdogTripV1> {
    reported_health(peer_id)?.health.latest_watchdog_trip
}

/// Last reported platform for QA reasons and peer displays, without exposing
/// the store's private health-cache representation across crate boundaries.
pub fn reported_platform(peer_id: Uuid) -> Option<SatellitePlatformV1> {
    REPORTED_HEALTH
        .lock()
        .ok()?
        .get(&peer_id)?
        .health
        .platform
        .clone()
}
