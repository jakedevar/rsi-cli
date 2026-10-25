//! Hub-side pull of satellite reports (#1103).
//!
//! Runs on the registry tick independently of queued manager delivery, never
//! as its own socket loop. For each enabled, paired, read- and dispatch-enabled
//! peer the hub connects over the custody-checked link, re-verifies the pinned
//! installation, and fetches the reports the satellite's manager queued. The
//! sender is the authenticated peer; the payload carries no identity. Each
//! report is authorized and rate-limited by the store before it becomes one
//! manager notice. Reports are informational and carry no authority.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rsi_common::satellite::{
    SATELLITE_WIRE_VERSION_V1, SatelliteIdentityV1, SatellitePeerV1, SatelliteUuidV1,
};
use rsi_common::satellite_dispatch::{
    SATELLITE_REPORT_MAX_ACKS, SATELLITE_REPORT_MAX_PER_FETCH, SatelliteFetchReportsRequestV1,
    SatelliteReportOutcomeV1, SatelliteReportV1, SatelliteReportVerdictV1,
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
use crate::store::satellite_reports::{INSTALLATION_CHANGED, RETRY_RECORD_FAILED, ReportRecord};

#[cfg(not(test))]
const POLL_EVERY: Duration = Duration::from_secs(15);
#[cfg(test)]
const POLL_EVERY: Duration = Duration::from_millis(20);
const FETCH_DEADLINE: Duration = Duration::from_secs(10);
/// Match the satellite's no-pull deadline, including a newly paired peer
/// that has never answered. Warn at most once per window until recovery.
const STALLED_AFTER: Duration = Duration::from_secs(
    rsi_common::satellite_dispatch::SATELLITE_REPORT_UNDELIVERED_AFTER_SECS as u64,
);

#[derive(Debug)]
struct PullProgress {
    installation: Uuid,
    since: Instant,
    last_success: Option<DateTime<Utc>>,
    next_warning: Instant,
    warned: bool,
}

/// Separate from the round's async state lock so the registry tick can warn
/// even while a pull is still waiting. No report contents are logged.
#[derive(Debug, Default)]
pub(super) struct PullHealth(HashMap<Uuid, PullProgress>);

impl PullHealth {
    pub(super) fn clear(&mut self) {
        self.0.clear();
    }

    pub(super) fn watch(&mut self, peers: &[SatellitePeerV1], now: Instant) {
        let mut live = std::collections::HashSet::new();
        for peer in peers {
            let config = &peer.config;
            let Some(installation) = config.expected_installation_id else {
                continue;
            };
            if !config.enabled || !config.read_enabled || !config.dispatch_enabled {
                continue;
            }
            let id = config.peer_id.0;
            live.insert(id);
            let progress = self.0.entry(id).or_insert_with(|| PullProgress {
                installation: installation.0,
                since: now,
                last_success: None,
                next_warning: now + STALLED_AFTER,
                warned: false,
            });
            if progress.installation != installation.0 {
                *progress = PullProgress {
                    installation: installation.0,
                    since: now,
                    last_success: None,
                    next_warning: now + STALLED_AFTER,
                    warned: false,
                };
            }
            if now >= progress.next_warning {
                tracing::warn!(
                    peer_id = %id,
                    peer = %config.label,
                    last_success = %progress.last_success.map(|at| at.to_rfc3339()).unwrap_or_else(|| "never".into()),
                    stalled_secs = now.saturating_duration_since(progress.since).as_secs(),
                    "satellite report pull stalled"
                );
                progress.warned = true;
                progress.next_warning = now + STALLED_AFTER;
            }
        }
        self.0.retain(|id, _| live.contains(id));
    }

    fn succeeded(&mut self, peer: &RegistryPeer, now: Instant, at: DateTime<Utc>) {
        let Some(progress) = self.0.get_mut(&peer.id) else {
            return;
        };
        if Some(progress.installation) != peer.expected_installation_id {
            return;
        }
        if progress.warned {
            tracing::info!(peer_id = %peer.id, peer = %peer.label, "satellite report pull recovered");
        }
        progress.since = now;
        progress.last_success = Some(at);
        progress.next_warning = now + STALLED_AFTER;
        progress.warned = false;
    }
}

/// Process-local pacing and acknowledgement memory. Notices, dedup and the rate
/// limit live in the store, so losing this only repeats a fetch.
#[derive(Debug, Default)]
pub(crate) struct ReportState {
    pub(super) health: Arc<StdMutex<PullHealth>>,
    peers: HashMap<Uuid, PeerPull>,
}

#[derive(Debug, Default)]
struct PeerPull {
    state: Arc<Mutex<PeerReportState>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for PeerPull {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[derive(Debug, Default)]
struct PeerReportState {
    next_poll: Option<Instant>,
    // Retain acknowledgements across failed pulls, but never across identities.
    pending_acks: Option<(Uuid, Vec<Uuid>)>,
    pending_outcomes: Option<(Uuid, Vec<SatelliteReportOutcomeV1>)>,
}

/// What recording one pulled page decided: the ids safe to acknowledge, and
/// why the rest (and the permanently refused) were not recorded.
#[derive(Debug, Default)]
pub(crate) struct ReportVerdicts {
    pub(crate) acks: Vec<Uuid>,
    pub(crate) outcomes: Vec<SatelliteReportOutcomeV1>,
}

impl ReportVerdicts {
    fn outcome(&mut self, report_id: Uuid, verdict: SatelliteReportVerdictV1, reason: &str) {
        if self.outcomes.len() < SATELLITE_REPORT_MAX_ACKS {
            self.outcomes.push(SatelliteReportOutcomeV1 {
                report_id,
                verdict,
                reason: reason.to_string(),
            });
        }
    }
}

/// One authenticated fetch: the verified installation and the reports. `None`
/// on any failure (retried next round).
pub(crate) async fn fetch_reports(
    root: &Path,
    peer: &RegistryPeer,
    hub_installation: Uuid,
    acked: Vec<Uuid>,
    outcomes: Vec<SatelliteReportOutcomeV1>,
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
            // An older satellite refuses the unknown field: verdicts only to
            // a build that says it accepts them.
            outcomes: if identity.capabilities.report_outcomes {
                outcomes
            } else {
                Vec::new()
            },
        };
        return match call_fetch_reports_rpc(&mut rpc, &request, FETCH_DEADLINE).await {
            Ok(FetchReportsReply::Accepted(reply)) => Some((expected, reply.reports)),
            Ok(FetchReportsReply::Refused(_)) | Err(_) => None,
        };
    }
    None
}

/// Start at most one pull per peer. A slow peer never holds another peer's
/// next pull (including the pull carrying its acknowledgements). Completed or
/// panicked tasks are reaped on the next tick; dropping a peer cancels its pull.
pub(crate) async fn report_tick(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    root: &Path,
    state: &mut ReportState,
) {
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
    state.peers.retain(|id, _| live.contains(id));
    for peer in peers {
        let pull = state.peers.entry(peer.id).or_default();
        if pull.task.as_ref().is_some_and(|task| !task.is_finished()) {
            continue;
        }
        if let Some(task) = pull.task.take() {
            if let Err(error) = task.await {
                tracing::warn!(peer_id = %peer.id, %error, "satellite report pull exited; retrying");
            }
        }
        let store = Arc::clone(store);
        let root = root.to_path_buf();
        let health = Arc::clone(&state.health);
        let state = Arc::clone(&pull.state);
        pull.task = Some(tokio::spawn(async move {
            let mut state = state.lock().await;
            report_peer(&store, &root, &peer, hub_installation, &health, &mut state).await;
        }));
    }
}

async fn report_peer(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    root: &Path,
    peer: &RegistryPeer,
    hub_installation: Uuid,
    health: &StdMutex<PullHealth>,
    state: &mut PeerReportState,
) {
    let now = Instant::now();
    if state.next_poll.is_some_and(|due| due > now) {
        return;
    }
    state.next_poll = Some(now + POLL_EVERY);
    // Re-homing needs no link, so a dark satellite does not hold it back.
    adopt_orphaned_reports(store, peer.id).await;
    let acked = state
        .pending_acks
        .as_ref()
        .filter(|(installation, _)| Some(*installation) == peer.expected_installation_id)
        .map(|(_, acks)| acks.clone())
        .unwrap_or_default();
    let outcomes = state
        .pending_outcomes
        .as_ref()
        .filter(|(installation, _)| Some(*installation) == peer.expected_installation_id)
        .map(|(_, outcomes)| outcomes.clone())
        .unwrap_or_default();
    let Some((installation, reports)) =
        fetch_reports(root, peer, hub_installation, acked, outcomes).await
    else {
        return;
    };
    health
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .succeeded(peer, Instant::now(), Utc::now());
    // A full page means more may be queued: pull again next tick.
    let full_page = reports.len() >= SATELLITE_REPORT_MAX_PER_FETCH;
    let verdicts = record_reports(store, peer.id, installation, reports).await;
    if full_page && !verdicts.acks.is_empty() {
        state.next_poll = Some(Instant::now());
    }
    state.pending_acks = Some((installation, verdicts.acks));
    state.pending_outcomes = Some((installation, verdicts.outcomes));
}

/// Re-home the peer's retired, never-retrieved reports onto the current
/// manager of its project (see `Store::adopt_orphaned_satellite_reports_for_peer`).
async fn adopt_orphaned_reports(store: &Arc<rsid_core::store_lock::Mutex<Store>>, peer_id: Uuid) {
    match store
        .lock()
        .await
        .adopt_orphaned_satellite_reports_for_peer(peer_id)
    {
        Ok(0) => {}
        Ok(adopted) => tracing::info!(
            peer_id = %peer_id,
            adopted,
            "satellite reports re-homed onto the project's current manager"
        ),
        Err(error) => {
            tracing::warn!(peer_id = %peer_id, "satellite report re-homing failed: {error}");
        }
    }
}

/// Record each pulled report; `acks` are safe to acknowledge (recorded, a
/// replay, or permanently refused). A report with no live manager to receive
/// it, at the peer's retention cap, or fetched from an installation that is no
/// longer the peer's pin stays unacknowledged. `outcomes` names why a report
/// was held or refused, so the satellite's manager can read it (#2242). The
/// pin is rechecked per report under the store lock (`verified_installation`
/// came from the fetch).
pub(crate) async fn record_reports(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    peer_id: Uuid,
    verified_installation: Uuid,
    reports: Vec<SatelliteReportV1>,
) -> ReportVerdicts {
    let guard = store.lock().await;
    let mut verdicts = ReportVerdicts::default();
    let mut retries: Vec<&'static str> = Vec::new();
    for report in reports.into_iter().take(SATELLITE_REPORT_MAX_PER_FETCH) {
        match guard.record_satellite_report(peer_id, verified_installation, &report, Utc::now()) {
            Ok(ReportRecord::Recorded | ReportRecord::Duplicate) => {
                verdicts.acks.push(report.report_id);
            }
            Ok(ReportRecord::Refused(INSTALLATION_CHANGED)) => {
                tracing::warn!(peer_id = %peer_id, "satellite report dropped: installation changed");
            }
            Ok(ReportRecord::Refused(class)) => {
                tracing::warn!(peer_id = %peer_id, class, "satellite report refused");
                verdicts.outcome(report.report_id, SatelliteReportVerdictV1::Refused, class);
                verdicts.acks.push(report.report_id);
            }
            Ok(ReportRecord::Retry(reason)) => {
                verdicts.outcome(report.report_id, SatelliteReportVerdictV1::Held, reason);
                retries.push(reason);
            }
            Err(error) => {
                tracing::warn!(peer_id = %peer_id, "satellite report record failed: {error}");
                verdicts.outcome(
                    report.report_id,
                    SatelliteReportVerdictV1::Held,
                    RETRY_RECORD_FAILED,
                );
            }
        }
    }
    // A stalled pull is never silent: the reports stay on the satellite and
    // the hub names why (one line per round and peer).
    if let Some(reason) = retries.first() {
        tracing::warn!(
            peer_id = %peer_id,
            reason = *reason,
            held = retries.len(),
            "satellite reports held on the satellite: not recorded"
        );
    }
    verdicts
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
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn watched_peer() -> SatellitePeerV1 {
        SatellitePeerV1 {
            config: SatellitePeerConfigV1 {
                peer_id: SatelliteUuidV1(Uuid::new_v4()),
                label: "test satellite".into(),
                expected_installation_id: Some(SatelliteUuidV1(Uuid::new_v4())),
                enabled: true,
                read_enabled: true,
                dispatch_enabled: true,
            },
            row_version: 1,
            links: Vec::new(),
            observation: None,
            dispatch_scope: Vec::new(),
            dispatch_project_scope: Vec::new(),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn stalled_pulls_log_the_peer_last_success_and_paced_recovery() {
        #[derive(Clone, Default)]
        struct LogBuffer(Arc<StdMutex<Vec<u8>>>);
        impl std::io::Write for LogBuffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let logs = LogBuffer::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let peer = watched_peer();
        let now = Instant::now();
        let mut health = PullHealth::default();
        let peers = std::slice::from_ref(&peer);
        health.watch(peers, now);
        health.watch(peers, now + STALLED_AFTER - Duration::from_secs(1));
        assert_eq!(logs.0.lock().unwrap().len(), 0);
        // A peer that never answers is observable even with no reports or
        // usable link. The first warning is bounded from initial observation.
        health.watch(peers, now + STALLED_AFTER);
        health.watch(peers, now + STALLED_AFTER + Duration::from_secs(1));
        let captured = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert_eq!(captured.matches("satellite report pull stalled").count(), 1);
        assert!(captured.contains(&peer.config.peer_id.0.to_string()));
        assert!(captured.contains("test satellite"));
        assert!(captured.contains("last_success=never"));

        let at = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let recovered = now + STALLED_AFTER + Duration::from_secs(2);
        health.succeeded(&registry_peer(&peer), recovered, at);
        health.watch(peers, recovered + STALLED_AFTER - Duration::from_secs(1));
        assert!(!health.0[&peer.config.peer_id.0].warned);
        health.watch(peers, recovered + STALLED_AFTER);
        health.watch(peers, recovered + STALLED_AFTER * 2);
        let captured = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            captured.matches("satellite report pull recovered").count(),
            1
        );
        assert_eq!(captured.matches("satellite report pull stalled").count(), 3);
        assert!(captured.contains(&format!("last_success={}", at.to_rfc3339())));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn pull_monitor_resets_for_policy_pause_unpair_and_new_installation() {
        let peer = watched_peer();
        let now = Instant::now();
        for change in 0..4 {
            let mut health = PullHealth::default();
            health.watch(std::slice::from_ref(&peer), now);
            let mut disabled = peer.clone();
            match change {
                0 => disabled.config.enabled = false,
                1 => disabled.config.read_enabled = false,
                2 => disabled.config.dispatch_enabled = false,
                _ => disabled.config.expected_installation_id = None,
            }
            health.watch(&[disabled], now + STALLED_AFTER);
            assert_eq!(health.0.len(), 0);
            health.watch(std::slice::from_ref(&peer), now + STALLED_AFTER);
            assert_eq!(health.0[&peer.config.peer_id.0].since, now + STALLED_AFTER);
            assert!(!health.0[&peer.config.peer_id.0].warned);
        }
        let mut health = PullHealth::default();
        health.watch(std::slice::from_ref(&peer), now);
        let mut repaired = peer.clone();
        repaired.config.expected_installation_id = Some(SatelliteUuidV1(Uuid::new_v4()));
        health.watch(std::slice::from_ref(&repaired), now + STALLED_AFTER);
        assert!(!health.0[&peer.config.peer_id.0].warned);
        // A fetch started before a re-pair cannot clear the new pin's clock.
        health.succeeded(&registry_peer(&peer), now + STALLED_AFTER, Utc::now());
        assert_eq!(health.0[&peer.config.peer_id.0].last_success, None);
        health.clear();
        assert_eq!(health.0.len(), 0);
        health.watch(&[peer], now + STALLED_AFTER * 2);
        assert_eq!(health.0.len(), 1);
        health.watch(&[], now + STALLED_AFTER * 3);
        assert_eq!(health.0.len(), 0);
    }

    /// A fake satellite that serves identity and the report pull. With
    /// `honor_acks` false it re-offers every report forever (a satellite that
    /// lost the hub's acknowledgements).
    struct Fake {
        fetches: Arc<AtomicUsize>,
        reports: Arc<StdMutex<Vec<SatelliteReportV1>>>,
        acks_seen: Arc<StdMutex<Vec<Uuid>>>,
        /// The verdicts the hub sent with its pulls.
        outcomes_seen: Arc<StdMutex<Vec<SatelliteReportOutcomeV1>>>,
        /// Whether the fake's identity advertises `report_outcomes` (false is
        /// an older satellite, which would refuse the unknown field).
        outcomes_capable: Arc<AtomicBool>,
    }

    struct Rig {
        _temp: tempfile::TempDir,
        root: std::path::PathBuf,
        store: Arc<rsid_core::store_lock::Mutex<Store>>,
        peer: Uuid,
        fake: Fake,
    }

    async fn rig(dispatch_enabled: bool, honor_acks: bool) -> Rig {
        rig_with(dispatch_enabled, honor_acks, 1).await
    }

    async fn rig_with(dispatch_enabled: bool, honor_acks: bool, queued: usize) -> Rig {
        let temp = crate::test_support::socket_tempdir("sat-rep-");
        let root = temp.path().join("satellites");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = root.join("peer.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        let installation = Uuid::new_v4();
        let fake = Fake {
            fetches: Arc::new(AtomicUsize::new(0)),
            reports: Arc::new(StdMutex::new(
                (0..queued)
                    .map(|index| SatelliteReportV1 {
                        binaries_dir: None,
                        report_id: Uuid::new_v4(),
                        kind: SatelliteReportKindV1::Enqueue,
                        text: format!("ENQUEUE deadbeef issue=#{index}"),
                    })
                    .collect(),
            )),
            acks_seen: Arc::new(StdMutex::new(Vec::new())),
            outcomes_seen: Arc::new(StdMutex::new(Vec::new())),
            outcomes_capable: Arc::new(AtomicBool::new(true)),
        };
        let (fetches, reports, acks_seen, outcomes_seen, outcomes_capable) = (
            Arc::clone(&fake.fetches),
            Arc::clone(&fake.reports),
            Arc::clone(&fake.acks_seen),
            Arc::clone(&fake.outcomes_seen),
            Arc::clone(&fake.outcomes_capable),
        );
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (fetches, reports, acks_seen, outcomes_seen, outcomes_capable) = (
                    Arc::clone(&fetches),
                    Arc::clone(&reports),
                    Arc::clone(&acks_seen),
                    Arc::clone(&outcomes_seen),
                    Arc::clone(&outcomes_capable),
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
                            "GetSatelliteIdentity" => {
                                let mut identity =
                                    crate::satellite::identity(installation, Uuid::new_v4());
                                identity.capabilities.report_outcomes =
                                    outcomes_capable.load(Ordering::SeqCst);
                                serde_json::to_value(identity).unwrap()
                            }
                            "FetchHubReports" => {
                                fetches.fetch_add(1, Ordering::SeqCst);
                                let params: SatelliteFetchReportsRequestV1 =
                                    serde_json::from_value(request.params.clone()).unwrap();
                                acks_seen.lock().unwrap().extend(params.acked.iter());
                                outcomes_seen
                                    .lock()
                                    .unwrap()
                                    .extend(params.outcomes.iter().cloned());
                                let mut held = reports.lock().unwrap();
                                if honor_acks {
                                    held.retain(|report| !params.acked.contains(&report.report_id));
                                }
                                serde_json::to_value(SatelliteFetchReportsReplyV1 {
                                    reports: held
                                        .iter()
                                        .take(SATELLITE_REPORT_MAX_PER_FETCH)
                                        .cloned()
                                        .collect(),
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
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

    async fn report_round(
        store: &Arc<rsid_core::store_lock::Mutex<Store>>,
        root: &Path,
        state: &mut ReportState,
    ) {
        report_tick(store, root, state).await;
        for pull in state.peers.values_mut() {
            if let Some(task) = pull.task.take() {
                task.await.unwrap();
            }
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
    async fn a_stalled_peer_does_not_delay_another_peers_fetch_or_acknowledgement() {
        let rig = rig(true, true).await;
        let slow = Uuid::new_v4();
        let installation = Uuid::new_v4();
        let socket = rig.root.join("slow.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        {
            let guard = rig.store.lock().await;
            guard
                .put_satellite_peer(
                    &SatellitePutPeerRequestV1 {
                        expected_registry_revision: guard.satellite_registry_revision().unwrap(),
                        peer: SatellitePeerConfigV1 {
                            peer_id: SatelliteUuidV1(slow),
                            label: "slow".into(),
                            expected_installation_id: Some(SatelliteUuidV1(installation)),
                            enabled: true,
                            read_enabled: true,
                            dispatch_enabled: true,
                        },
                        repair_quarantine: false,
                    },
                    &rig.root,
                )
                .unwrap();
            guard
                .put_satellite_link(
                    &SatellitePutLinkRequestV1 {
                        expected_registry_revision: guard.satellite_registry_revision().unwrap(),
                        peer_id: SatelliteUuidV1(slow),
                        link: SatelliteLinkConfigV1 {
                            link_id: SatelliteUuidV1(Uuid::new_v4()),
                            direction: SatelliteLinkDirectionV1::DialHomeReverse,
                            socket_path: socket.to_string_lossy().into_owned(),
                            ssh_target: None,
                            trust_reference: "ssh-config:slow".into(),
                            enabled: true,
                            priority: 0,
                        },
                    },
                    &rig.root,
                )
                .unwrap();
        }
        let (started, fetching) = tokio::sync::oneshot::channel();
        let (release, stalled) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            let request: RpcRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(request.method, "GetSatelliteIdentity");
            let response = RpcResponse::success(
                Some(request.id.into()),
                serde_json::to_value(crate::satellite::identity(installation, Uuid::new_v4()))
                    .unwrap(),
            );
            let mut bytes = serde_json::to_vec(&response).unwrap();
            bytes.push(b'\n');
            stream.get_mut().write_all(&bytes).await.unwrap();
            line.clear();
            stream.read_line(&mut line).await.unwrap();
            let request: RpcRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(request.method, "FetchHubReports");
            let params: SatelliteFetchReportsRequestV1 =
                serde_json::from_value(request.params).unwrap();
            assert!(params.acked.is_empty());
            started.send(()).unwrap();
            let _ = stalled.await;
        });
        let mut state = ReportState::default();
        report_tick(&rig.store, &rig.root, &mut state).await;
        tokio::time::timeout(Duration::from_secs(2), fetching)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                report_tick(&rig.store, &rig.root, &mut state).await;
                if !rig.fake.acks_seen.lock().unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("healthy peer fetched and acknowledged while slow peer is held");
        assert!(!state.peers[&slow].task.as_ref().unwrap().is_finished());
        assert!(rig.fake.fetches.load(Ordering::SeqCst) >= 2);
        assert_eq!(rig.fake.acks_seen.lock().unwrap().len(), 1);
        assert!(rig.fake.reports.lock().unwrap().is_empty());
        assert_eq!(notices(&rig).await, 1);
        release.send(()).unwrap();
        server.await.unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn the_hub_pulls_records_one_notice_and_acknowledges_next_round() {
        let rig = rig(true, true).await;
        let mut state = ReportState::default();
        let peers = rig
            .store
            .lock()
            .await
            .satellite_registry_view()
            .unwrap()
            .peers;
        state.health.lock().unwrap().watch(&peers, Instant::now());
        report_round(&rig.store, &rig.root, &mut state).await;
        assert!(
            state.health.lock().unwrap().0[&rig.peer]
                .last_success
                .is_some()
        );
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
        let verdicts = record_reports(&rig.store, rig.peer, fetched_from, reports.clone()).await;
        assert!(verdicts.acks.is_empty());
        assert!(verdicts.outcomes.is_empty());
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
        let verdicts = record_reports(
            &rig.store,
            rig.peer,
            Uuid::parse_str(&pin).unwrap(),
            reports.clone(),
        )
        .await;
        assert_eq!(verdicts.acks.len(), reports.len());
        assert_eq!(notices(&rig).await, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_queue_larger_than_one_page_drains_over_successive_pulls() {
        // Within the per-peer rate limit, so only paging is in play.
        let queued = SATELLITE_REPORT_MAX_PER_FETCH + SATELLITE_REPORT_MAX_PER_FETCH / 2;
        let rig = rig_with(true, true, queued).await;
        let mut state = ReportState::default();
        // No pacing sleep: a full page is followed by an immediate pull.
        for _ in 0..2 {
            report_round(&rig.store, &rig.root, &mut state).await;
        }
        assert_eq!(notices(&rig).await, i64::try_from(queued).unwrap());
        // The last short page's acknowledgement rides the next paced pull.
        tokio::time::sleep(Duration::from_millis(40)).await;
        report_round(&rig.store, &rig.root, &mut state).await;
        assert_eq!(notices(&rig).await, i64::try_from(queued).unwrap());
        assert!(rig.fake.reports.lock().unwrap().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_hub_with_two_managers_still_records_for_the_dispatching_project() {
        let rig = rig_with(true, true, 3).await;
        let owner = {
            let guard = rig.store.lock().await;
            let (_second, lead) = fixture(
                &guard,
                ManagerPolicyV2 {
                    capabilities: vec![ManagerCapabilityV2::WorkPlan],
                    ..Default::default()
                },
            );
            let stamp = crate::store::harness_manager_v2::now();
            guard
                .conn
                .execute(
                    "INSERT INTO satellite_messages(id,owner_session_id,peer_id,remote_session_id,
                       idempotency_digest,request_fingerprint,payload,created_at,expires_at,updated_at)
                     VALUES(?1,?2,?3,?4,?5,?5,'x',?6,?6,?6)",
                    rusqlite::params![
                        Uuid::new_v4().to_string(),
                        lead.id.to_string(),
                        rig.peer.to_string(),
                        Uuid::new_v4().to_string(),
                        "0".repeat(64),
                        stamp,
                    ],
                )
                .unwrap();
            lead.id
        };
        let _ = owner;
        let mut state = ReportState::default();
        report_round(&rig.store, &rig.root, &mut state).await;
        assert_eq!(notices(&rig).await, 3);
    }

    /// #2242: the hub tells the satellite why it did not record a report, in
    /// the pull that follows, so the seat reads `held` with a typed reason
    /// instead of a bare `queued`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_report_the_hub_cannot_record_is_explained_to_the_satellite() {
        let rig = rig(true, true).await;
        // The project's manager scope is revoked: nobody can receive it.
        rig.store
            .lock()
            .await
            .conn
            .execute("UPDATE harness_manager_scopes SET epic_ids_json='[]'", [])
            .unwrap();
        let mut state = ReportState::default();
        report_round(&rig.store, &rig.root, &mut state).await;
        assert_eq!(notices(&rig).await, 0);
        assert!(rig.fake.outcomes_seen.lock().unwrap().is_empty());
        tokio::time::sleep(Duration::from_millis(40)).await;
        report_round(&rig.store, &rig.root, &mut state).await;
        let seen = rig.fake.outcomes_seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].verdict, SatelliteReportVerdictV1::Held);
        assert_eq!(seen[0].reason, "manager_unavailable");
        assert!(rig.fake.acks_seen.lock().unwrap().is_empty());
        assert_eq!(notices(&rig).await, 0);
    }

    /// An older satellite refuses the unknown `outcomes` field and its pull
    /// would stall: the hub sends verdicts only to a build that advertises it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_satellite_that_does_not_advertise_outcomes_never_receives_them() {
        let rig = rig(true, true).await;
        rig.fake.outcomes_capable.store(false, Ordering::SeqCst);
        rig.store
            .lock()
            .await
            .conn
            .execute("UPDATE harness_manager_scopes SET epic_ids_json='[]'", [])
            .unwrap();
        let mut state = ReportState::default();
        for _ in 0..3 {
            report_round(&rig.store, &rig.root, &mut state).await;
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        assert!(rig.fake.fetches.load(Ordering::SeqCst) >= 3);
        assert!(rig.fake.outcomes_seen.lock().unwrap().is_empty());
    }

    /// #2242, end to end through the pull round: a report recorded for the
    /// manager seat that is then replaced reaches the new manager without the
    /// satellite resending anything.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_pull_round_re_homes_a_report_recorded_for_a_replaced_manager() {
        use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
        use rsi_common::harness_manager_v2::{
            ConfigureHarnessManagerPolicyRequestV2, ManagerPolicyV2,
        };
        let rig = rig(true, true).await;
        let mut state = ReportState::default();
        report_round(&rig.store, &rig.root, &mut state).await;
        assert_eq!(notices(&rig).await, 1);
        let live = |rig: &Rig| {
            let store = Arc::clone(&rig.store);
            async move {
                store
                    .lock()
                    .await
                    .conn
                    .query_row(
                        "SELECT manager_session_id FROM harness_manager_notices
                         WHERE subject_id='satellite_report' AND retired_at IS NULL
                         ORDER BY sequence DESC LIMIT 1",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .unwrap()
            }
        };
        let first = live(&rig).await;

        // The portfolio manager appoints the next project manager; the old
        // scope's unretrieved notice is retired.
        let new_manager = {
            let guard = rig.store.lock().await;
            let project: String = guard
                .conn
                .query_row("SELECT project_id FROM harness_manager_scopes", [], |row| {
                    row.get(0)
                })
                .unwrap();
            let project = Uuid::parse_str(&project).unwrap();
            let mut session = rsid_store::test_support::make_test_session();
            session.project_id = Some(project);
            session.status = rsi_common::types::SessionStatus::Completed;
            guard.insert_session(&session).unwrap();
            let scope = guard.get_harness_manager(project).unwrap().unwrap();
            let config = guard
                .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                    project_id: project,
                    session_id: session.id,
                    epic_ids: None,
                    group_ids: Vec::new(),
                    expected_row_version: scope.row_version,
                })
                .unwrap();
            let policy = guard.get_harness_manager_policy(project).unwrap().unwrap();
            guard
                .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                    project_id: project,
                    expected_scope_version: config.row_version,
                    expected_policy_version: policy.row_version,
                    idempotency_key: "regrant".into(),
                    policy: ManagerPolicyV2::default(),
                })
                .unwrap();
            guard.reconcile_harness_manager_watches().unwrap();
            session.id
        };
        assert_ne!(first, new_manager.to_string());

        tokio::time::sleep(Duration::from_millis(40)).await;
        report_round(&rig.store, &rig.root, &mut state).await;
        assert_eq!(live(&rig).await, new_manager.to_string());
    }
}
