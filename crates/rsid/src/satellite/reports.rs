//! Hub-side pull of satellite reports (#1103).
//!
//! Runs in the same paced background round as queued manager delivery, never
//! as its own socket loop. For each enabled, paired, read- and dispatch-enabled
//! peer the hub connects over the custody-checked link, re-verifies the pinned
//! installation, and fetches the reports the satellite's manager queued. The
//! sender is the authenticated peer; the payload carries no identity. Each
//! report is authorized and rate-limited by the store before it becomes one
//! manager notice. Reports are informational and carry no authority.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use rsi_common::satellite::{SATELLITE_WIRE_VERSION_V1, SatelliteIdentityV1, SatelliteUuidV1};
use rsi_common::satellite_dispatch::{
    SATELLITE_REPORT_MAX_PER_FETCH, SatelliteFetchReportsRequestV1, SatelliteReportV1,
};
use tokio::io::BufReader;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::hub::registry_peer;
use super::link::connect_owned_socket;
use super::poll::{
    FetchReportsReply, MAX_RPC_ENVELOPE_BYTES, RPC_DEADLINE, call_fetch_reports_rpc, call_read_rpc,
};
use super::registry::RegistryPeer;
use crate::store::Store;
use crate::store::satellite_reports::{INSTALLATION_CHANGED, ReportRecord};

#[cfg(not(test))]
const POLL_EVERY: Duration = Duration::from_secs(15);
#[cfg(test)]
const POLL_EVERY: Duration = Duration::from_millis(20);
const FETCH_DEADLINE: Duration = Duration::from_secs(10);

/// Process-local pacing and acknowledgement memory. Notices, dedup and the rate
/// limit live in the store, so losing this only repeats a fetch.
#[derive(Debug, Default)]
pub(crate) struct ReportState {
    next_poll: HashMap<Uuid, Instant>,
    /// Acknowledgements per peer, tagged with the installation they were
    /// recorded from; never sent to a satellite with a different identity.
    pending_acks: HashMap<Uuid, (Uuid, Vec<Uuid>)>,
}

/// One authenticated fetch: the verified installation and the reports. `None`
/// on any failure (retried next round).
pub(crate) async fn fetch_reports(
    root: &Path,
    peer: &RegistryPeer,
    hub_installation: Uuid,
    acked: Vec<Uuid>,
) -> Option<(Uuid, Vec<SatelliteReportV1>)> {
    let expected = peer.expected_installation_id?;
    let mut links: Vec<_> = peer.links.iter().filter(|link| link.enabled).collect();
    links.sort_by_key(|link| (link.priority, link.id));
    for link in links {
        let Ok(socket) = connect_owned_socket(root, &link.socket_path, RPC_DEADLINE).await else {
            continue;
        };
        let mut rpc = BufReader::new(socket);
        let Ok(value) = call_read_rpc(
            &mut rpc,
            "GetSatelliteIdentity",
            serde_json::Value::Null,
            MAX_RPC_ENVELOPE_BYTES,
            RPC_DEADLINE,
        )
        .await
        else {
            continue;
        };
        let Ok(identity) = serde_json::from_value::<SatelliteIdentityV1>(value) else {
            continue;
        };
        // The sender identity is the pinned installation, not the payload.
        if identity.installation_id.0 != expected {
            return None;
        }
        let request = SatelliteFetchReportsRequestV1 {
            wire_version: SATELLITE_WIRE_VERSION_V1,
            hub_installation_id: SatelliteUuidV1(hub_installation),
            acked,
        };
        return match call_fetch_reports_rpc(&mut rpc, &request, FETCH_DEADLINE).await {
            Ok(FetchReportsReply::Accepted(reply)) => Some((expected, reply.reports)),
            Ok(FetchReportsReply::Refused(_)) | Err(_) => None,
        };
    }
    None
}

/// Fetch and record once per due peer.
pub(crate) async fn report_round(store: &Arc<Mutex<Store>>, root: &Path, state: &mut ReportState) {
    let (peers, hub_installation) = {
        let guard = store.lock().await;
        let Ok(view) = guard.satellite_registry_view() else {
            return;
        };
        let Ok(hub_installation) = guard.satellite_installation_id() else {
            return;
        };
        let peers: Vec<RegistryPeer> = view
            .peers
            .iter()
            .filter(|value| {
                let config = &value.config;
                config.enabled
                    && config.read_enabled
                    && config.dispatch_enabled
                    && config.expected_installation_id.is_some()
            })
            .map(registry_peer)
            .collect();
        (peers, hub_installation)
    };
    let live: std::collections::HashSet<Uuid> = peers.iter().map(|peer| peer.id).collect();
    state.next_poll.retain(|id, _| live.contains(id));
    state.pending_acks.retain(|id, _| live.contains(id));
    for peer in peers {
        let now = Instant::now();
        if state.next_poll.get(&peer.id).is_some_and(|due| *due > now) {
            continue;
        }
        state.next_poll.insert(peer.id, now + POLL_EVERY);
        let acked = state
            .pending_acks
            .get(&peer.id)
            .filter(|(installation, _)| Some(*installation) == peer.expected_installation_id)
            .map(|(_, acks)| acks.clone())
            .unwrap_or_default();
        let Some((installation, reports)) =
            fetch_reports(root, &peer, hub_installation, acked).await
        else {
            continue;
        };
        let acks = record_reports(store, peer.id, installation, reports).await;
        state.pending_acks.insert(peer.id, (installation, acks));
    }
}

/// Record each pulled report; the returned ids are safe to acknowledge
/// (recorded, a replay, or permanently refused). A report with no live manager
/// to receive it, at the peer's retention cap, or fetched from an installation
/// that is no longer the peer's pin stays unacknowledged. The pin is rechecked
/// per report under the store lock (`verified_installation` came from the
/// fetch).
pub(crate) async fn record_reports(
    store: &Arc<Mutex<Store>>,
    peer_id: Uuid,
    verified_installation: Uuid,
    reports: Vec<SatelliteReportV1>,
) -> Vec<Uuid> {
    let guard = store.lock().await;
    let mut acks = Vec::new();
    for report in reports.into_iter().take(SATELLITE_REPORT_MAX_PER_FETCH) {
        match guard.record_satellite_report(peer_id, verified_installation, &report, Utc::now()) {
            Ok(ReportRecord::Recorded | ReportRecord::Duplicate) => acks.push(report.report_id),
            Ok(ReportRecord::Refused(INSTALLATION_CHANGED)) => {
                tracing::warn!(peer_id = %peer_id, "satellite report dropped: installation changed");
            }
            Ok(ReportRecord::Refused(class)) => {
                tracing::warn!(peer_id = %peer_id, class, "satellite report refused");
                acks.push(report.report_id);
            }
            Ok(ReportRecord::Retry) => {}
            Err(error) => {
                tracing::warn!(peer_id = %peer_id, "satellite report record failed: {error}");
            }
        }
    }
    acks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::manager_coordinator::tests::fixture;
    use rsi_common::harness_manager_v2::{ManagerCapabilityV2, ManagerPolicyV2};
    use rsi_common::rpc::{RpcRequest, RpcResponse};
    use rsi_common::satellite::{
        SatelliteLinkConfigV1, SatelliteLinkDirectionV1, SatellitePeerConfigV1,
        SatellitePutLinkRequestV1, SatellitePutPeerRequestV1,
    };
    use rsi_common::satellite_dispatch::{SatelliteFetchReportsReplyV1, SatelliteReportKindV1};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    /// A fake satellite that serves identity and the report pull. With
    /// `honor_acks` false it re-offers every report forever (a satellite that
    /// lost the hub's acknowledgements).
    struct Fake {
        fetches: Arc<AtomicUsize>,
        reports: Arc<StdMutex<Vec<SatelliteReportV1>>>,
        acks_seen: Arc<StdMutex<Vec<Uuid>>>,
    }

    struct Rig {
        _temp: tempfile::TempDir,
        root: std::path::PathBuf,
        store: Arc<Mutex<Store>>,
        peer: Uuid,
        fake: Fake,
    }

    async fn rig(dispatch_enabled: bool, honor_acks: bool) -> Rig {
        let temp = tempfile::Builder::new()
            .prefix("sat-rep-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = temp.path().join("satellites");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = root.join("peer.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        let installation = Uuid::new_v4();
        let fake = Fake {
            fetches: Arc::new(AtomicUsize::new(0)),
            reports: Arc::new(StdMutex::new(vec![SatelliteReportV1 {
                report_id: Uuid::new_v4(),
                kind: SatelliteReportKindV1::Enqueue,
                text: "ENQUEUE deadbeef issue=#1".into(),
            }])),
            acks_seen: Arc::new(StdMutex::new(Vec::new())),
        };
        let (fetches, reports, acks_seen) = (
            Arc::clone(&fake.fetches),
            Arc::clone(&fake.reports),
            Arc::clone(&fake.acks_seen),
        );
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (fetches, reports, acks_seen) = (
                    Arc::clone(&fetches),
                    Arc::clone(&reports),
                    Arc::clone(&acks_seen),
                );
                tokio::spawn(async move {
                    let mut stream = tokio::io::BufReader::new(stream);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let request: RpcRequest = serde_json::from_str(&line).unwrap();
                        let result = match request.method.as_str() {
                            "GetSatelliteIdentity" => serde_json::to_value(
                                crate::satellite::identity(installation, Uuid::new_v4()),
                            )
                            .unwrap(),
                            "FetchHubReports" => {
                                fetches.fetch_add(1, Ordering::SeqCst);
                                let params: SatelliteFetchReportsRequestV1 =
                                    serde_json::from_value(request.params.clone()).unwrap();
                                acks_seen.lock().unwrap().extend(params.acked.iter());
                                let mut held = reports.lock().unwrap();
                                if honor_acks {
                                    held.retain(|report| !params.acked.contains(&report.report_id));
                                }
                                serde_json::to_value(SatelliteFetchReportsReplyV1 {
                                    reports: held.clone(),
                                })
                                .unwrap()
                            }
                            other => panic!("hub called a method outside the allowlist: {other}"),
                        };
                        let response =
                            RpcResponse::success(Some(request.id.clone().into()), result);
                        let mut bytes = serde_json::to_vec(&response).unwrap();
                        bytes.push(b'\n');
                        stream.get_mut().write_all(&bytes).await.unwrap();
                    }
                });
            }
        });
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let peer = Uuid::new_v4();
        {
            let guard = store.lock().await;
            fixture(
                &guard,
                ManagerPolicyV2 {
                    capabilities: vec![ManagerCapabilityV2::WorkPlan],
                    ..Default::default()
                },
            );
            let revision = guard.satellite_registry_revision().unwrap();
            guard
                .put_satellite_peer(
                    &SatellitePutPeerRequestV1 {
                        expected_registry_revision: revision,
                        peer: SatellitePeerConfigV1 {
                            peer_id: SatelliteUuidV1(peer),
                            label: "peer".into(),
                            expected_installation_id: Some(SatelliteUuidV1(installation)),
                            enabled: true,
                            read_enabled: true,
                            dispatch_enabled,
                        },
                        repair_quarantine: false,
                    },
                    &root,
                )
                .unwrap();
            let revision = guard.satellite_registry_revision().unwrap();
            guard
                .put_satellite_link(
                    &SatellitePutLinkRequestV1 {
                        expected_registry_revision: revision,
                        peer_id: SatelliteUuidV1(peer),
                        link: SatelliteLinkConfigV1 {
                            link_id: SatelliteUuidV1(Uuid::new_v4()),
                            direction: SatelliteLinkDirectionV1::DialHomeReverse,
                            socket_path: socket.to_string_lossy().into_owned(),
                            ssh_target: None,
                            trust_reference: "ssh-config:peer".into(),
                            enabled: true,
                            priority: 0,
                        },
                    },
                    &root,
                )
                .unwrap();
            let revision = guard.satellite_registry_revision().unwrap();
            guard
                .put_satellite_peer_scope(revision, peer, &[SatelliteUuidV1(Uuid::new_v4())])
                .unwrap();
        }
        Rig {
            _temp: temp,
            root,
            store,
            peer,
            fake,
        }
    }

    async fn notices(rig: &Rig) -> i64 {
        rig.store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices WHERE subject_id='satellite_report'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn the_hub_pulls_records_one_notice_and_acknowledges_next_round() {
        let rig = rig(true, true).await;
        let mut state = ReportState::default();
        report_round(&rig.store, &rig.root, &mut state).await;
        assert_eq!(notices(&rig).await, 1);
        assert!(rig.fake.acks_seen.lock().unwrap().is_empty());
        tokio::time::sleep(Duration::from_millis(40)).await;
        report_round(&rig.store, &rig.root, &mut state).await;
        assert_eq!(notices(&rig).await, 1);
        assert_eq!(rig.fake.acks_seen.lock().unwrap().len(), 1);
        assert!(rig.fake.reports.lock().unwrap().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_satellite_that_re_offers_a_report_never_duplicates_the_notice() {
        let rig = rig(true, false).await;
        let mut state = ReportState::default();
        for _ in 0..3 {
            report_round(&rig.store, &rig.root, &mut state).await;
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        assert!(rig.fake.fetches.load(Ordering::SeqCst) >= 3);
        assert_eq!(notices(&rig).await, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_peer_without_dispatch_is_never_asked_for_reports() {
        let rig = rig(false, true).await;
        let mut state = ReportState::default();
        report_round(&rig.store, &rig.root, &mut state).await;
        assert_eq!(rig.fake.fetches.load(Ordering::SeqCst), 0);
        assert_eq!(notices(&rig).await, 0);
        let _ = rig.peer;
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn reports_fetched_before_a_re_pair_are_dropped_not_recorded_or_acked() {
        let rig = rig(true, true).await;
        let fetched_from = Uuid::new_v4();
        let reports = rig.fake.reports.lock().unwrap().clone();
        // The peer's pin is a different installation by ingest time.
        let acks = record_reports(&rig.store, rig.peer, fetched_from, reports.clone()).await;
        assert!(acks.is_empty());
        assert_eq!(notices(&rig).await, 0);
        // The verified pin is accepted.
        let pin: String = rig
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT expected_installation_id FROM satellite_peers WHERE id=?1",
                [rig.peer.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let acks = record_reports(
            &rig.store,
            rig.peer,
            Uuid::parse_str(&pin).unwrap(),
            reports.clone(),
        )
        .await;
        assert_eq!(acks.len(), reports.len());
        assert_eq!(notices(&rig).await, 1);
    }
}
