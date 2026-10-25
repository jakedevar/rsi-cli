//! Hub-owned polling of explicitly enabled satellite links.
//!
//! Only the daemon starts this loop. A TUI render reads the bounded cache and
//! never dials a peer or starts a local daemon on a peer socket.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rsi_common::satellite::{SatelliteLinkDirectionV1, SatellitePeerV1};
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

use crate::config::RuntimeConfig;
use crate::store::Store;
use crate::store::satellite_registry::SatelliteFailure;
use crate::store::satellite_reports::HubReportLinkStatus;

use super::poll::{MAX_IN_FLIGHT, PeerSnapshot, PollSchedule, probe_link};
use super::registry::{LinkDirection, PeerContinuity, RegistryLink, RegistryPeer};

#[cfg(not(test))]
const REGISTRY_TICK: Duration = Duration::from_secs(5);
/// Test runs use a short tick so poller tests finish in well under a second.
#[cfg(test)]
const REGISTRY_TICK: Duration = Duration::from_millis(50);
const PEER_ROUND_DEADLINE: Duration = Duration::from_secs(30);
/// Hard cap on the startup stale-marking retry. The poller must never loop
/// forever before its first registry tick.
const STARTUP_STALE_ATTEMPTS: usize = 5;

type PollOutcome = std::result::Result<PeerSnapshot, SatelliteFailure>;

/// Own the single-flight tasks rather than a flag cleared only on normal
/// return. A panic or cancellation is observed and retried on the next tick.
#[derive(Default)]
struct LinkRounds {
    dispatch: Option<tokio::task::JoinHandle<()>>,
    reports: Option<tokio::task::JoinHandle<()>>,
}

impl LinkRounds {
    async fn tick<D, R>(&mut self, dispatch: D, reports: R)
    where
        D: std::future::Future<Output = ()> + Send + 'static,
        R: std::future::Future<Output = ()> + Send + 'static,
    {
        Self::start(&mut self.dispatch, "delivery", dispatch).await;
        Self::start(&mut self.reports, "reports", reports).await;
    }

    async fn start<F>(slot: &mut Option<tokio::task::JoinHandle<()>>, round: &'static str, work: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        if slot.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        if let Some(task) = slot.take() {
            if let Err(error) = task.await {
                tracing::warn!(round, %error, "satellite link round exited; restarting");
            }
        }
        *slot = Some(tokio::spawn(work));
    }
}

impl Drop for LinkRounds {
    fn drop(&mut self) {
        for task in [&self.dispatch, &self.reports].into_iter().flatten() {
            task.abort();
        }
    }
}

pub(crate) fn registry_peer(value: &SatellitePeerV1) -> RegistryPeer {
    let config = &value.config;
    RegistryPeer {
        id: config.peer_id.0,
        label: config.label.clone(),
        expected_installation_id: config.expected_installation_id.map(|id| id.0),
        enabled: config.enabled,
        read_enabled: config.read_enabled,
        launch_enabled: false,
        dispatch_enabled: config.dispatch_enabled,
        links: value
            .links
            .iter()
            .map(|link| RegistryLink {
                id: link.link_id.0,
                direction: match link.direction {
                    SatelliteLinkDirectionV1::DialHomeReverse => LinkDirection::DialHomeReverse,
                    SatelliteLinkDirectionV1::DirectLocalForward => {
                        LinkDirection::DirectLocalForward
                    }
                },
                socket_path: PathBuf::from(&link.socket_path),
                ssh_target: link.ssh_target.clone(),
                trust_reference: link.trust_reference.clone(),
                enabled: link.enabled,
                priority: link.priority,
            })
            .collect(),
    }
}

fn classify(error: &io::Error) -> SatelliteFailure {
    match error.kind() {
        io::ErrorKind::PermissionDenied => SatelliteFailure::Insecure,
        io::ErrorKind::InvalidData => SatelliteFailure::Incompatible,
        io::ErrorKind::NotFound
        | io::ErrorKind::ConnectionRefused
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::TimedOut
        | io::ErrorKind::UnexpectedEof
        | io::ErrorKind::BrokenPipe => SatelliteFailure::Offline,
        _ => SatelliteFailure::Degraded,
    }
}

/// Check every enabled route before accepting a snapshot. A conflicting
/// identity on a lower-priority route quarantines the whole peer.
async fn read_peer(root: &Path, mut peer: RegistryPeer) -> PollOutcome {
    if peer.validate(root).is_err() {
        return Err(SatelliteFailure::Insecure);
    }
    peer.links.sort_by_key(|link| (link.priority, link.id));
    let mut continuity = PeerContinuity::new(peer.expected_installation_id);
    continuity.begin_round();
    let mut first_good = None;
    let mut incompatible = false;
    let mut degraded = false;
    for link in peer.links.iter().filter(|link| link.enabled) {
        match probe_link(root, &peer, link, &mut continuity).await {
            Ok(snapshot) => {
                if first_good.is_none() {
                    first_good = Some(snapshot);
                }
            }
            Err(error) => {
                let failure = classify(&error);
                if continuity.is_quarantined() || matches!(failure, SatelliteFailure::Insecure) {
                    return Err(SatelliteFailure::Insecure);
                }
                incompatible |= matches!(failure, SatelliteFailure::Incompatible);
                degraded |= matches!(failure, SatelliteFailure::Degraded);
            }
        }
    }
    if incompatible {
        Err(SatelliteFailure::Incompatible)
    } else if degraded {
        Err(SatelliteFailure::Degraded)
    } else {
        first_good.ok_or(SatelliteFailure::Offline)
    }
}

/// Mark cached observations stale at startup with a bounded retry.
///
/// The marker is called at most `STARTUP_STALE_ATTEMPTS` times with a
/// `REGISTRY_TICK` backoff between failures. Returns `true` once marking
/// succeeds, `false` when the attempts are exhausted and the caller must carry
/// a `stale_pending` flag into the poll loop.
async fn mark_stale_bounded<F, Fut>(mut mark: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = crate::error::Result<()>>,
{
    for attempt in 1..=STARTUP_STALE_ATTEMPTS {
        if mark().await.is_ok() {
            return true;
        }
        if attempt < STARTUP_STALE_ATTEMPTS {
            tokio::time::sleep(REGISTRY_TICK).await;
        }
    }
    false
}

/// How often the satellite re-homes failure notices a re-appointment retired.
const FAILURE_ADOPTION_EVERY: Duration = Duration::from_secs(15);

/// Remember only link state, so normal pulls and repeated outage ticks are
/// silent. Removing pairing forgets its state without inventing a recovery.
#[derive(Default)]
struct HubLinkTransitions {
    stale: HashMap<Uuid, bool>,
}

impl HubLinkTransitions {
    fn observe(
        &mut self,
        link: Option<&HubReportLinkStatus>,
        now: DateTime<Utc>,
    ) -> Vec<(Uuid, bool)> {
        let Some(link) = link else {
            self.stale.clear();
            return Vec::new();
        };
        self.stale
            .retain(|id, _| link.hub_installations.contains(id));
        let stale = crate::health_anomaly::hub_pull_stale(&link.reports, now);
        link.hub_installations
            .iter()
            .filter_map(|id| {
                let previous = self.stale.insert(*id, stale);
                if previous.is_some_and(|previous| previous != stale)
                    || (previous.is_none() && stale)
                {
                    Some((*id, stale))
                } else {
                    None
                }
            })
            .collect()
    }
}

fn sweep_hub_link(store: &Store, transitions: &mut HubLinkTransitions, now: DateTime<Utc>) {
    let link = match store.hub_report_link_status(now) {
        Ok(link) => link,
        Err(error) => {
            tracing::warn!("satellite hub link status read failed: {error}");
            return;
        }
    };
    let changes = transitions.observe(link.as_ref(), now);
    let Some(link) = link else { return };
    for (hub, stale) in changes {
        let last_hub_pull_at = link.reports.last_hub_pull_at.as_deref().unwrap_or("never");
        let backlog_depth = link.reports.backlog.depth;
        let oldest_age_secs = link.reports.backlog.oldest_age_secs;
        if stale {
            tracing::warn!(hub_installation_id = %hub, last_hub_pull_at, backlog_depth,
                ?oldest_age_secs, "satellite hub link stopped pulling reports");
        } else {
            tracing::info!(hub_installation_id = %hub, last_hub_pull_at, backlog_depth,
                ?oldest_age_secs, "satellite hub link resumed pulling reports");
        }
    }
}

/// #2453: record the satellite's due report failures as durable notices for
/// its manager, and (paced) re-home the ones a re-appointment retired
/// unretrieved. A failure leaves the notice due, so the next tick retries.
async fn sweep_report_failures(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    next_adoption: &mut Instant,
) {
    let guard = store.lock().await;
    match guard.sweep_hub_report_failures() {
        Ok(0) => {}
        Ok(recorded) => {
            tracing::info!(
                recorded,
                "satellite report failures recorded for the manager"
            );
        }
        Err(error) => tracing::warn!("satellite report failure notice not recorded: {error}"),
    }
    let now = Instant::now();
    if *next_adoption > now {
        return;
    }
    *next_adoption = now + FAILURE_ADOPTION_EVERY;
    match guard.adopt_orphaned_report_failures() {
        Ok(0) => {}
        Ok(adopted) => {
            tracing::info!(
                adopted,
                "satellite report failures re-homed onto the current manager"
            );
        }
        Err(error) => tracing::warn!("satellite report failure re-homing failed: {error}"),
    }
}

/// Run once for the hub daemon. Registry revision fences prevent an in-flight
/// read from publishing after an operator edits policy or disables a peer.
pub(crate) async fn run_hub_poller(
    store: Arc<rsid_core::store_lock::Mutex<Store>>,
    runtime_config: Arc<RuntimeConfig>,
    satellite_root: PathBuf,
) {
    let store_mark = Arc::clone(&store);
    let marked = mark_stale_bounded(move || {
        let store = Arc::clone(&store_mark);
        async move { store.lock().await.mark_satellite_observations_stale() }
    })
    .await;
    let mut stale_pending = !marked;
    if stale_pending {
        tracing::warn!(
            "satellite poller: startup stale marking failed after {STARTUP_STALE_ATTEMPTS} attempts; retrying on the next successful poll"
        );
    }

    #[cfg(not(test))]
    let mut schedule = PollSchedule::default();
    #[cfg(test)]
    let mut schedule = PollSchedule::with_test_timing(Duration::from_millis(25), 0);
    let mut registered = HashSet::new();
    let dispatch_state = Arc::new(Mutex::new(super::dispatch::DispatchState::default()));
    let reports = super::reports::ReportState::default();
    let report_health = Arc::clone(&reports.health);
    let report_state = Arc::new(Mutex::new(reports));
    let mut link_rounds = LinkRounds::default();
    let (done_tx, mut done_rx) = mpsc::channel::<(Uuid, u64, PollOutcome)>(MAX_IN_FLIGHT * 2);
    let mut ticker = tokio::time::interval(REGISTRY_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut next_adoption = Instant::now();
    let mut hub_link_transitions = HubLinkTransitions::default();
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                // #2453: a satellite's failed reports reach its manager on
                // this tick whether or not the hub polls it or this daemon
                // polls peers.
                sweep_report_failures(&store, &mut next_adoption).await;
                {
                    let guard = store.lock().await;
                    sweep_hub_link(&guard, &mut hub_link_transitions, Utc::now());
                }
                if !runtime_config.satellite_polling_enabled.load(Ordering::Relaxed) {
                    // An intentional pause is not a failed link. Start a new
                    // observation window when polling is enabled again.
                    report_health.lock().unwrap_or_else(|e| e.into_inner()).clear();
                    continue;
                }
                let view = store.lock().await.satellite_registry_view();
                let view = match view {
                    Ok(view) => view,
                    Err(error) => {
                        tracing::warn!("satellite registry read failed: {error}");
                        continue;
                    }
                };
                // This monitor does not need the round's state lock: even a
                // round waiting on I/O cannot hide a stale pull.
                report_health.lock().unwrap_or_else(|e| e.into_inner())
                    .watch(&view.peers, Instant::now());
                if stale_pending {
                    let marked = store.lock().await.mark_satellite_observations_stale();
                    match marked {
                        Ok(()) => stale_pending = false,
                        Err(error) => {
                            tracing::warn!(
                                "satellite poller: stale marking still pending: {error}"
                            );
                            continue;
                        }
                    }
                }
                // Delivery can spend 16 * 30 seconds on queued rows. Pulls
                // schedule their own per-peer single flights, so neither that
                // backlog nor another peer's stalled fetch can starve them.
                let dispatch = {
                    let store = Arc::clone(&store);
                    let root = satellite_root.clone();
                    let state = Arc::clone(&dispatch_state);
                    async move {
                        let mut state = state.lock().await;
                        super::dispatch::dispatch_round(&store, &root, &mut state).await;
                    }
                };
                let reports = {
                    let store = Arc::clone(&store);
                    let root = satellite_root.clone();
                    let reports = Arc::clone(&report_state);
                    async move {
                        let mut reports = reports.lock().await;
                        super::reports::report_tick(&store, &root, &mut reports).await;
                    }
                };
                link_rounds.tick(dispatch, reports).await;
                let current: HashSet<Uuid> = view.peers.iter().map(|peer| peer.config.peer_id.0).collect();
                for id in registered.difference(&current) {
                    schedule.remove(*id);
                }
                registered = current;
                let now = Instant::now();
                for value in view.peers {
                    let id = value.config.peer_id.0;
                    let active = value.config.enabled
                        && value.config.read_enabled
                        && value.config.expected_installation_id.is_some()
                        && value.links.iter().any(|link| link.enabled)
                        && !value.observation.as_ref().is_some_and(|observation| observation.state == "insecure");
                    if !schedule.register(id, active, now) || !schedule.begin(id, now) {
                        continue;
                    }
                    let root = satellite_root.clone();
                    let peer = registry_peer(&value);
                    let sender = done_tx.clone();
                    let revision = view.revision;
                    tokio::spawn(async move {
                        let read = tokio::spawn(async move {
                            tokio::time::timeout(PEER_ROUND_DEADLINE, read_peer(&root, peer)).await
                                .unwrap_or(Err(SatelliteFailure::Degraded))
                        });
                        let outcome = read.await.unwrap_or(Err(SatelliteFailure::Degraded));
                        let _ = sender.send((id, revision, outcome)).await;
                    });
                }
            }
            Some((id, revision, outcome)) = done_rx.recv() => {
                let mut success = false;
                let guard = store.lock().await;
                match guard.satellite_registry_revision() {
                    Ok(current) if current == revision => {
                        let result = match &outcome {
                            Ok(snapshot) => {
                                let recorded = guard.record_satellite_snapshot(id, snapshot);
                                if recorded.is_ok() {
                                    crate::satellite::record_reported_health(
                                        id,
                                        snapshot.health.as_ref(),
                                    );
                                }
                                recorded
                            }
                            Err(failure) => guard.record_satellite_failure(id, *failure),
                        };
                        if let Err(error) = result {
                            tracing::warn!(peer_id = %id, "satellite observation persistence failed: {error}");
                        } else {
                            success = outcome.is_ok();
                        }
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!("satellite registry revision read failed: {error}"),
                }
                drop(guard);
                schedule.finish(id, success, Instant::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rsi_common::rpc::{RpcRequest, RpcResponse};
    use rsi_common::satellite::{
        SATELLITE_WIRE_VERSION_V1, SatelliteLinkConfigV1, SatelliteLinkDirectionV1,
        SatellitePeerConfigV1, SatellitePutLinkRequestV1, SatellitePutPeerRequestV1,
        SatelliteSessionPageV1, SatelliteUuidV1,
    };
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    async fn finished(task: &Option<tokio::task::JoinHandle<()>>) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !task.as_ref().unwrap().is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("link round settled");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_stalled_delivery_round_does_not_starve_report_pulls() {
        let mut rounds = LinkRounds::default();
        let (release, stalled) = tokio::sync::oneshot::channel::<()>();
        let (pulled, pull) = tokio::sync::oneshot::channel();
        rounds
            .tick(
                async move {
                    let _ = stalled.await;
                },
                async move {
                    pulled.send(()).unwrap();
                },
            )
            .await;
        tokio::time::timeout(Duration::from_secs(2), pull)
            .await
            .unwrap()
            .unwrap();
        finished(&rounds.reports).await;

        // The next registry tick pulls again while delivery still owns its
        // single flight. A second delivery future must never start.
        let (pulled, pull) = tokio::sync::oneshot::channel();
        rounds
            .tick(
                async {
                    panic!("overlapping delivery");
                },
                async move {
                    pulled.send(()).unwrap();
                },
            )
            .await;
        tokio::time::timeout(Duration::from_secs(2), pull)
            .await
            .unwrap()
            .unwrap();
        assert!(!rounds.dispatch.as_ref().unwrap().is_finished());
        release.send(()).unwrap();
        finished(&rounds.dispatch).await;

        let (delivered, delivery) = tokio::sync::oneshot::channel();
        rounds
            .tick(
                async move {
                    delivered.send(()).unwrap();
                },
                async {},
            )
            .await;
        tokio::time::timeout(Duration::from_secs(2), delivery)
            .await
            .unwrap()
            .unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_panicked_link_round_restarts_on_the_next_tick() {
        let mut rounds = LinkRounds::default();
        rounds
            .tick(
                async {
                    panic!("injected delivery panic");
                },
                async {
                    panic!("injected pull panic");
                },
            )
            .await;
        finished(&rounds.dispatch).await;
        finished(&rounds.reports).await;
        let (delivered, delivery) = tokio::sync::oneshot::channel();
        let (pulled, pull) = tokio::sync::oneshot::channel();
        rounds
            .tick(
                async move {
                    delivered.send(()).unwrap();
                },
                async move {
                    pulled.send(()).unwrap();
                },
            )
            .await;
        tokio::time::timeout(Duration::from_secs(2), async {
            delivery.await.unwrap();
            pull.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_cancelled_link_round_restarts_on_the_next_tick() {
        let mut rounds = LinkRounds::default();
        rounds
            .tick(std::future::pending(), std::future::pending())
            .await;
        rounds.dispatch.as_ref().unwrap().abort();
        rounds.reports.as_ref().unwrap().abort();
        finished(&rounds.dispatch).await;
        finished(&rounds.reports).await;
        let (delivered, delivery) = tokio::sync::oneshot::channel();
        let (pulled, pull) = tokio::sync::oneshot::channel();
        rounds
            .tick(
                async move {
                    delivered.send(()).unwrap();
                },
                async move {
                    pulled.send(()).unwrap();
                },
            )
            .await;
        tokio::time::timeout(Duration::from_secs(2), async {
            delivery.await.unwrap();
            pull.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn hub_link_logs_only_outage_and_recovery_transitions() {
        use rsi_common::health_anomaly::HUB_PULL_STALE_SECS;
        use rsi_common::satellite_dispatch::SatelliteReportStatusV1;
        let start = Utc::now();
        let hub = Uuid::new_v4();
        let mut link = HubReportLinkStatus {
            hub_installations: vec![hub],
            reports: SatelliteReportStatusV1 {
                last_hub_pull_at: Some(start.to_rfc3339()),
                ..Default::default()
            },
        };
        let mut transitions = HubLinkTransitions::default();
        assert!(transitions.observe(None, start).is_empty());
        assert!(transitions.observe(Some(&link), start).is_empty());
        let stale = start + chrono::Duration::seconds(HUB_PULL_STALE_SECS + 1);
        assert_eq!(transitions.observe(Some(&link), stale), vec![(hub, true)]);
        assert!(transitions.observe(Some(&link), stale).is_empty());
        link.reports.last_hub_pull_at = Some(stale.to_rfc3339());
        assert_eq!(transitions.observe(Some(&link), stale), vec![(hub, false)]);
        for tick in 1..=3 {
            let at = stale + chrono::Duration::seconds(tick);
            link.reports.last_hub_pull_at = Some(at.to_rfc3339());
            assert!(transitions.observe(Some(&link), at).is_empty());
        }
        let down_again = stale + chrono::Duration::seconds(HUB_PULL_STALE_SECS + 5);
        assert_eq!(
            transitions.observe(Some(&link), down_again),
            vec![(hub, true)]
        );
        assert!(transitions.observe(None, down_again).is_empty());
        assert!(transitions.stale.is_empty());
        // A startup with an already stale link emits one warning, too.
        assert_eq!(
            transitions.observe(Some(&link), down_again),
            vec![(hub, true)]
        );
        assert!(transitions.observe(Some(&link), down_again).is_empty());
    }

    fn socket_root() -> (tempfile::TempDir, PathBuf) {
        let temp = crate::test_support::socket_tempdir("sat-hub-");
        let root = temp.path().join("satellites");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        (temp, root)
    }

    fn link(root: &Path, name: &str, priority: u8) -> RegistryLink {
        RegistryLink {
            id: Uuid::new_v4(),
            direction: LinkDirection::DialHomeReverse,
            socket_path: root.join(name),
            ssh_target: None,
            trust_reference: "ssh-config:work-laptop".into(),
            enabled: true,
            priority,
        }
    }

    fn peer(installation: Uuid, links: Vec<RegistryLink>) -> RegistryPeer {
        RegistryPeer {
            id: Uuid::new_v4(),
            label: "work laptop".into(),
            expected_installation_id: Some(installation),
            enabled: true,
            read_enabled: true,
            launch_enabled: false,
            dispatch_enabled: false,
            links,
        }
    }

    async fn serve(listener: UnixListener, installation: Uuid, incarnation: Uuid, send_page: bool) {
        serve_with_major(
            listener,
            installation,
            incarnation,
            send_page,
            rsi_common::satellite::SATELLITE_PROTOCOL_MAJOR,
        )
        .await;
    }

    async fn serve_with_major(
        listener: UnixListener,
        installation: Uuid,
        incarnation: Uuid,
        send_page: bool,
        major: u16,
    ) {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        stream.read_line(&mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<RpcRequest>(&line).unwrap().method,
            "GetSatelliteIdentity"
        );
        let mut identity = super::super::identity(installation, incarnation);
        identity.protocol.major = major;
        let mut response = serde_json::to_vec(&RpcResponse::success(
            Some(json!(1)),
            serde_json::to_value(identity).unwrap(),
        ))
        .unwrap();
        response.push(b'\n');
        stream.get_mut().write_all(&response).await.unwrap();
        if !send_page {
            return;
        }
        line.clear();
        stream.read_line(&mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<RpcRequest>(&line).unwrap().method,
            "ListSatelliteSessions"
        );
        let page = SatelliteSessionPageV1 {
            wire_version: SATELLITE_WIRE_VERSION_V1,
            installation_id: SatelliteUuidV1(installation),
            daemon_incarnation_id: SatelliteUuidV1(incarnation),
            snapshot_id: SatelliteUuidV1(Uuid::new_v4()),
            snapshot_total_sessions: 0,
            snapshot_offset: 0,
            observed_at: Utc::now(),
            sessions: vec![],
            next_cursor: None,
        };
        let mut response = serde_json::to_vec(&RpcResponse::success(
            Some(json!(1)),
            serde_json::to_value(page).unwrap(),
        ))
        .unwrap();
        response.push(b'\n');
        stream.get_mut().write_all(&response).await.unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn second_link_with_restored_clone_quarantines_first_good_snapshot() {
        let (_temp, root) = socket_root();
        let installation = Uuid::new_v4();
        let first = link(&root, "first.sock", 0);
        let second = link(&root, "second.sock", 1);
        let first_listener = UnixListener::bind(&first.socket_path).unwrap();
        let second_listener = UnixListener::bind(&second.socket_path).unwrap();
        for link in [&first, &second] {
            fs::set_permissions(&link.socket_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let first_responder =
            tokio::spawn(serve(first_listener, installation, Uuid::new_v4(), true));
        let second_responder =
            tokio::spawn(serve(second_listener, installation, Uuid::new_v4(), false));
        let result = read_peer(&root, peer(installation, vec![first, second])).await;
        assert!(matches!(result, Err(SatelliteFailure::Insecure)));
        first_responder.await.unwrap();
        second_responder.await.unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn offline_first_link_uses_healthy_second_link() {
        let (_temp, root) = socket_root();
        let installation = Uuid::new_v4();
        let first = link(&root, "missing.sock", 0);
        let second = link(&root, "healthy.sock", 1);
        let listener = UnixListener::bind(&second.socket_path).unwrap();
        fs::set_permissions(&second.socket_path, fs::Permissions::from_mode(0o600)).unwrap();
        let responder = tokio::spawn(serve(listener, installation, Uuid::new_v4(), true));
        let snapshot = read_peer(&root, peer(installation, vec![first, second]))
            .await
            .unwrap();
        assert_eq!(snapshot.installation_id, installation);
        responder.await.unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn protocol_major_skew_is_reported_as_incompatible() {
        let (_temp, root) = socket_root();
        let installation = Uuid::new_v4();
        let route = link(&root, "skewed.sock", 0);
        let listener = UnixListener::bind(&route.socket_path).unwrap();
        fs::set_permissions(&route.socket_path, fs::Permissions::from_mode(0o600)).unwrap();
        let responder = tokio::spawn(serve_with_major(
            listener,
            installation,
            Uuid::new_v4(),
            false,
            rsi_common::satellite::SATELLITE_PROTOCOL_MAJOR + 1,
        ));
        let result = read_peer(&root, peer(installation, vec![route])).await;
        assert!(matches!(result, Err(SatelliteFailure::Incompatible)));
        responder.await.unwrap();
    }

    /// One registered, enabled, read-enabled peer on a real in-memory store, its
    /// socket path not yet bound. A healthy observation is recorded so the startup
    /// stale marking has something to flip.
    struct RegisteredPeer {
        _temp: tempfile::TempDir,
        root: PathBuf,
        socket: PathBuf,
        store: Arc<rsid_core::store_lock::Mutex<Store>>,
        peer_id: SatelliteUuidV1,
    }

    async fn registered_peer() -> RegisteredPeer {
        let temp = crate::test_support::socket_tempdir("sat-hub-poll-");
        let root = temp.path().join("satellites");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = root.join("peer.sock");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let installation = Uuid::new_v4();
        let peer_id = SatelliteUuidV1(Uuid::new_v4());
        let link_id = SatelliteUuidV1(Uuid::new_v4());
        {
            let guard = store.lock().await;
            let revision = guard.satellite_registry_revision().unwrap();
            guard
                .put_satellite_peer(
                    &SatellitePutPeerRequestV1 {
                        expected_registry_revision: revision,
                        peer: SatellitePeerConfigV1 {
                            peer_id,
                            label: "peer".into(),
                            expected_installation_id: Some(SatelliteUuidV1(installation)),
                            enabled: true,
                            read_enabled: true,
                            dispatch_enabled: false,
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
                        peer_id,
                        link: SatelliteLinkConfigV1 {
                            link_id,
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
            guard
                .record_satellite_snapshot(
                    peer_id.0,
                    &PeerSnapshot {
                        installation_id: installation,
                        incarnation_id: Uuid::new_v4(),
                        sessions: vec![],
                        health: None,
                    },
                )
                .unwrap();
        }
        RegisteredPeer {
            _temp: temp,
            root,
            socket,
            store,
            peer_id,
        }
    }

    async fn observation_state(
        store: &Arc<rsid_core::store_lock::Mutex<Store>>,
        peer_id: SatelliteUuidV1,
    ) -> String {
        store
            .lock()
            .await
            .satellite_registry_view()
            .unwrap()
            .peers
            .iter()
            .find(|peer| peer.config.peer_id == peer_id)
            .and_then(|peer| peer.observation.as_ref())
            .map(|observation| observation.state.clone())
            .unwrap_or_default()
    }

    /// #925: with the switch off, a registered healthy peer is never dialled and
    /// its registry row survives; flipping the switch back on resumes polling
    /// without a daemon restart.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn polling_switch_gates_probes_and_retains_rows() {
        let peer = registered_peer().await;
        let listener = UnixListener::bind(&peer.socket).unwrap();
        fs::set_permissions(&peer.socket, fs::Permissions::from_mode(0o600)).unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let accept_hits = Arc::clone(&hits);
        let acceptor = tokio::spawn(async move {
            loop {
                if listener.accept().await.is_ok() {
                    accept_hits.fetch_add(1, Ordering::SeqCst);
                }
            }
        });

        let config = RuntimeConfig::from_config(&crate::config::Config::default());
        config
            .update_field("satellite_polling_enabled", &json!(false))
            .unwrap();
        let runtime = Arc::new(config);
        let poller = tokio::spawn(run_hub_poller(
            Arc::clone(&peer.store),
            Arc::clone(&runtime),
            peer.root.clone(),
        ));

        tokio::time::sleep(REGISTRY_TICK * 4).await;
        assert_eq!(hits.load(Ordering::SeqCst), 0, "polling off dials nothing");
        assert_eq!(
            observation_state(&peer.store, peer.peer_id).await,
            "degraded",
            "the registry row and its observation are retained"
        );

        runtime
            .update_field("satellite_polling_enabled", &json!(true))
            .unwrap();
        let mut probed = false;
        for _ in 0..40 {
            tokio::time::sleep(REGISTRY_TICK).await;
            if hits.load(Ordering::SeqCst) > 0 {
                probed = true;
                break;
            }
        }
        assert!(probed, "polling on probes the peer");

        poller.abort();
        acceptor.abort();
    }

    /// #925: the startup stale marking is bounded. An always-failing marker is
    /// called exactly the cap and reports exhaustion; a later success marks the
    /// store stale so the caller clears its pending flag.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn bounded_stale_marking_exhausts_then_a_later_success_clears() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let exhausted = mark_stale_bounded(move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(crate::error::DaemonError::Store("injected".into()))
            }
        })
        .await;
        assert!(!exhausted, "exhausted attempts leave the flag pending");
        assert_eq!(calls.load(Ordering::SeqCst), STARTUP_STALE_ATTEMPTS);

        let peer = registered_peer().await;
        assert_eq!(
            observation_state(&peer.store, peer.peer_id).await,
            "healthy"
        );
        let store = Arc::clone(&peer.store);
        let failures_left = Arc::new(AtomicUsize::new(1));
        let marked = mark_stale_bounded(move || {
            let store = Arc::clone(&store);
            let failures_left = Arc::clone(&failures_left);
            async move {
                if failures_left.fetch_sub(1, Ordering::SeqCst) > 0 {
                    return Err(crate::error::DaemonError::Store("injected".into()));
                }
                store.lock().await.mark_satellite_observations_stale()
            }
        })
        .await;
        assert!(marked, "a later success clears the pending flag");
        let view = peer.store.lock().await.satellite_registry_view().unwrap();
        let observation = view
            .peers
            .iter()
            .find(|entry| entry.config.peer_id == peer.peer_id)
            .and_then(|peer| peer.observation.as_ref())
            .expect("observation");
        assert_eq!(observation.state, "degraded");
        assert_eq!(
            observation.last_error.as_deref(),
            Some("awaiting identity recheck")
        );
    }
}
