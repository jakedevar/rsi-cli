//! Live inputs of a `when.health_anomaly` wake (#2747): the facts only the
//! running session manager can read. No model turn is started here.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use uuid::Uuid;

use super::SessionManager;
use crate::health_anomaly::{LiveHealthInputs, PmContext, SatelliteReading, Scope};

/// How long a satellite read over its link is reused. The poll is once a
/// minute; dialing every satellite that often would cost more than it finds.
const SATELLITE_READ_TTL: Duration = Duration::from_secs(5 * 60);

fn satellite_cache() -> &'static Mutex<Option<(Instant, Vec<SatelliteReading>)>> {
    static CACHE: OnceLock<Mutex<Option<(Instant, Vec<SatelliteReading>)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

impl SessionManager {
    /// Each PM seat's context fill against its rotation threshold (scoped), and
    /// every satellite's reported disk and provider faults (only for a wake that
    /// watches everything).
    pub(super) async fn health_live_inputs_for(&self, scope: &Scope) -> LiveHealthInputs {
        LiveHealthInputs {
            pm_contexts: self.pm_contexts(scope).await,
            satellites: if scope.is_fleet_wide() {
                self.satellite_readings().await
            } else {
                Vec::new()
            },
        }
    }

    async fn pm_contexts(&self, scope: &Scope) -> Vec<PmContext> {
        let seats: Vec<(Uuid, Uuid, bool)> = {
            let store = self.store.lock().await;
            let projects: Vec<String> = match store
                .conn
                .prepare("SELECT project_id FROM harness_manager_scopes ORDER BY project_id")
                .and_then(|mut statement| {
                    statement
                        .query_map([], |row| row.get(0))?
                        .collect::<std::result::Result<_, _>>()
                }) {
                Ok(projects) => projects,
                Err(error) => {
                    tracing::warn!(%error, "health predicate: PM seats unreadable");
                    return Vec::new();
                }
            };
            projects
                .into_iter()
                .filter_map(|text| Uuid::parse_str(&text).ok())
                .filter(|project| scope.allows(*project))
                .filter_map(|project| {
                    let seat = match scope.area_seat() {
                        Some(seat) => seat,
                        None => {
                            store
                                .get_harness_manager_notice_config(project)
                                .ok()??
                                .current_session_id?
                        }
                    };
                    let queued = store
                        .session_has_unresolved_manager_succession(seat)
                        .unwrap_or(false);
                    Some((project, seat, queued))
                })
                .collect()
        };
        let mut contexts = Vec::new();
        for (project_id, session_id, succession_queued) in seats {
            let Some(session) = self.get_session(session_id).await else {
                continue;
            };
            let threshold =
                super::context_cap::rotation_threshold_for(&self.runtime_config, &session, true);
            if let (Some(fill_pct), Some(threshold)) = (session.context_fill_pct, threshold) {
                contexts.push(PmContext {
                    project_id,
                    session_id,
                    fill_pct,
                    threshold_pct: threshold.pct,
                    succession_queued,
                });
            }
        }
        contexts
    }

    async fn satellite_readings(&self) -> Vec<SatelliteReading> {
        if let Some((read_at, readings)) = satellite_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            && read_at.elapsed() < SATELLITE_READ_TTL
        {
            return readings.clone();
        }
        let peers = {
            let store = self.store.lock().await;
            match store.satellite_registry_view() {
                Ok(view) => view
                    .peers
                    .iter()
                    .filter(|peer| {
                        peer.config.enabled
                            && peer.config.read_enabled
                            && peer.config.expected_installation_id.is_some()
                    })
                    .map(crate::satellite::hub::registry_peer)
                    .collect::<Vec<_>>(),
                Err(error) => {
                    tracing::warn!(%error, "health predicate: satellite registry unreadable");
                    return Vec::new();
                }
            }
        };
        let root = rsi_common::identity::data_path("satellites", "satellites");
        let reads = futures::future::join_all(peers.iter().map(|peer| async {
            crate::satellite::deploy_link::read_peer_health(&root, peer)
                .await
                .map(|health| SatelliteReading::from_health(peer.id, &peer.label, &health))
        }))
        .await;
        let readings: Vec<SatelliteReading> = reads.into_iter().flatten().collect();
        *satellite_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((Instant::now(), readings.clone()));
        readings
    }
}
