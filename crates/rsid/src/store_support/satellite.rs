//! Satellite limits and cached-read types the store and manager ledger use
//! (moved down from `satellite::{poll, registry}` / `satellite`, which
//! re-export them).

use chrono::{DateTime, Utc};
use rsi_common::satellite::{SatelliteHealthV1, SatelliteSessionSummaryV1};
use std::sync::LazyLock;
use uuid::Uuid;

pub(crate) const MAX_REGISTERED_PEERS: usize = 128;
pub(crate) const MAX_CACHED_ROWS: usize = 1_000;
pub(crate) const MAX_CACHED_BYTES: usize = 1_048_576;

#[derive(Debug)]
pub(crate) struct PeerSnapshot {
    pub(crate) installation_id: Uuid,
    pub(crate) incarnation_id: Uuid,
    pub(crate) sessions: Vec<SatelliteSessionSummaryV1>,
    /// Health the satellite reported on its identity, if it reports any.
    pub(crate) health: Option<SatelliteHealthV1>,
}

/// Last health each peer reported, with the time the hub received it. It is
/// process-local by design: after a hub restart the value is unknown until the
/// next successful poll, so no migration or stale row can present old health.
#[derive(Debug, Clone)]
pub(crate) struct ReportedHealth {
    pub(crate) received_at: DateTime<Utc>,
    pub(crate) health: SatelliteHealthV1,
}

static REPORTED_HEALTH: LazyLock<
    std::sync::Mutex<std::collections::HashMap<Uuid, ReportedHealth>>,
> = LazyLock::new(Default::default);

/// Record (or clear, for a satellite that reports none) a peer's health.
pub(crate) fn record_reported_health(peer_id: Uuid, health: Option<&SatelliteHealthV1>) {
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

pub(crate) fn reported_health(peer_id: Uuid) -> Option<ReportedHealth> {
    REPORTED_HEALTH.lock().ok()?.get(&peer_id).cloned()
}
