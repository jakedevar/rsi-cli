//! Bounded scheduling for one hub-owned satellite poller.
//!
//! S2 keeps this scheduler inert. S3 will attach its RPC client and operator
//! controls; a TUI render must never start a peer request itself.

use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use uuid::Uuid;

use rsi_common::rpc::{RpcRequest, RpcResponse};
use serde_json::Value;

use super::link::connect_owned_socket;
use super::registry::{ContinuityResult, PeerContinuity, RegistryLink, RegistryPeer};
use rsi_common::satellite::{
    SATELLITE_WIRE_VERSION_V1, SatelliteCompatibility, SatelliteIdentityV1,
    SatelliteSessionPageRequestV1, SatelliteSessionPageV1, SatelliteSessionSummaryV1,
};

pub(crate) const MAX_REGISTERED_PEERS: usize = 128;
pub(crate) const MAX_IN_FLIGHT: usize = 4;
const MIN_INTERVAL: Duration = Duration::from_secs(15);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
const MAX_JITTER_MS: u64 = 3_000;
pub(crate) const RPC_DEADLINE: Duration = Duration::from_secs(3);
pub(crate) const MAX_RPC_ENVELOPE_BYTES: usize =
    rsi_common::satellite::SATELLITE_MAX_RESPONSE_BYTES + 4_096;
pub(crate) const MAX_CACHED_ROWS: usize = 1_000;
pub(crate) const MAX_CACHED_BYTES: usize = 1_048_576;
const MAX_PAGES: usize = 20;

fn protocol_error(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

async fn bounded_line(reader: &mut BufReader<UnixStream>, max_bytes: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "satellite closed RPC response",
            ));
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let used = newline.map_or(available.len(), |index| index + 1);
        if bytes.len().saturating_add(used) > max_bytes {
            return Err(protocol_error("satellite RPC response exceeds byte limit"));
        }
        bytes.extend_from_slice(&available[..used]);
        reader.consume(used);
        if newline.is_some() {
            return Ok(bytes);
        }
    }
}

/// Fixed, read-only RPC allowlist. The caller owns the validated link and
/// must check the returned installation/incarnation against registry custody.
pub(crate) async fn call_read_rpc(
    reader: &mut BufReader<UnixStream>,
    method: &'static str,
    params: Value,
    max_response_bytes: usize,
    deadline: Duration,
) -> io::Result<Value> {
    if !matches!(method, "GetSatelliteIdentity" | "ListSatelliteSessions") {
        return Err(protocol_error(
            "satellite RPC method is outside read allowlist",
        ));
    }
    let request = RpcRequest::new(method, params);
    let mut encoded = serde_json::to_vec(&request).map_err(io::Error::other)?;
    encoded.push(b'\n');
    tokio::time::timeout(deadline, reader.get_mut().write_all(&encoded))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "satellite RPC write timed out"))??;
    let cap = max_response_bytes.min(MAX_RPC_ENVELOPE_BYTES);
    let line = tokio::time::timeout(deadline, bounded_line(reader, cap))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "satellite RPC read timed out"))??;
    let response: RpcResponse = serde_json::from_slice(&line).map_err(io::Error::other)?;
    if response.jsonrpc != "2.0"
        || response.id != request.id
        || response.error.is_some()
        || response.result.is_none()
    {
        return Err(protocol_error("invalid satellite RPC envelope"));
    }
    Ok(response.result.expect("checked result"))
}

#[derive(Debug)]
pub(crate) struct PeerSnapshot {
    pub(crate) installation_id: Uuid,
    pub(crate) incarnation_id: Uuid,
    pub(crate) sessions: Vec<SatelliteSessionSummaryV1>,
}

/// One bounded peer read, invoked only after S3 operator policy enables it.
/// Every page must belong to one identity, incarnation and snapshot.
pub(crate) async fn probe_link(
    satellite_root: &Path,
    peer: &RegistryPeer,
    link: &RegistryLink,
    continuity: &mut PeerContinuity,
) -> io::Result<PeerSnapshot> {
    peer.validate(satellite_root).map_err(protocol_error)?;
    if !peer.enabled
        || !peer.read_enabled
        || !link.enabled
        || peer.expected_installation_id.is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "satellite peer read is disabled or unpaired",
        ));
    }
    if !peer.links.contains(link)
        || continuity.expected_installation_id() != peer.expected_installation_id
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "satellite link is not registered for peer",
        ));
    }
    let socket = connect_owned_socket(satellite_root, &link.socket_path, RPC_DEADLINE).await?;
    let mut rpc = BufReader::new(socket);
    let identity_value = call_read_rpc(
        &mut rpc,
        "GetSatelliteIdentity",
        Value::Null,
        MAX_RPC_ENVELOPE_BYTES,
        RPC_DEADLINE,
    )
    .await?;
    let identity: SatelliteIdentityV1 =
        serde_json::from_value(identity_value).map_err(io::Error::other)?;
    identity.validate().map_err(io::Error::other)?;
    if continuity.observe(
        link.id,
        identity.installation_id.0,
        identity.daemon_incarnation_id.0,
    ) != ContinuityResult::Matched
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "satellite installation identity requires review or is quarantined",
        ));
    }
    if !matches!(
        identity.read_compatibility(),
        SatelliteCompatibility::Compatible
    ) {
        return Err(protocol_error("satellite read protocol is incompatible"));
    }
    let limits = &identity.capabilities.limits;
    let mut sessions = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = None;
    let mut snapshot_id = None;
    let mut observed_at = None;
    let mut total = None;
    for _ in 0..MAX_PAGES {
        let request = SatelliteSessionPageRequestV1 {
            wire_version: SATELLITE_WIRE_VERSION_V1,
            limit: limits.max_page_size.min(100),
            cursor,
        };
        request.validate(limits).map_err(io::Error::other)?;
        let params = serde_json::to_value(request).map_err(io::Error::other)?;
        let page_value = call_read_rpc(
            &mut rpc,
            "ListSatelliteSessions",
            params,
            usize::try_from(limits.max_response_bytes)
                .unwrap_or(MAX_RPC_ENVELOPE_BYTES)
                .saturating_add(4_096),
            RPC_DEADLINE,
        )
        .await?;
        let page: SatelliteSessionPageV1 =
            serde_json::from_value(page_value).map_err(io::Error::other)?;
        page.validate(limits).map_err(io::Error::other)?;
        if page.installation_id != identity.installation_id
            || page.daemon_incarnation_id != identity.daemon_incarnation_id
            || page.snapshot_offset as usize != sessions.len()
            || page.snapshot_total_sessions as usize > MAX_CACHED_ROWS
            || snapshot_id.is_some_and(|previous| previous != page.snapshot_id)
            || observed_at.is_some_and(|previous| previous != page.observed_at)
            || total.is_some_and(|previous| previous != page.snapshot_total_sessions)
        {
            return Err(protocol_error(
                "satellite pages disagree on snapshot identity or bounds",
            ));
        }
        snapshot_id = Some(page.snapshot_id);
        observed_at = Some(page.observed_at);
        total = Some(page.snapshot_total_sessions);
        for session in page.sessions {
            if !seen.insert(session.session_id) {
                return Err(protocol_error("satellite snapshot has duplicate sessions"));
            }
            sessions.push(session);
        }
        if serde_json::to_vec(&sessions)
            .map_err(io::Error::other)?
            .len()
            > MAX_CACHED_BYTES
        {
            return Err(protocol_error(
                "satellite snapshot exceeds cache byte limit",
            ));
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(PeerSnapshot {
                installation_id: identity.installation_id.0,
                incarnation_id: identity.daemon_incarnation_id.0,
                sessions,
            });
        }
    }
    Err(protocol_error("satellite snapshot exceeds page limit"))
}

#[derive(Debug, Clone)]
struct PeerPoll {
    enabled: bool,
    in_flight: bool,
    failures: u8,
    next_due: Instant,
}

/// One process-local schedule. Registry rows, not this map, survive restart.
#[derive(Debug, Default)]
pub(crate) struct PollSchedule {
    peers: HashMap<Uuid, PeerPoll>,
    in_flight: usize,
}

fn jitter(peer_id: Uuid) -> Duration {
    let bytes = peer_id.as_bytes();
    let seed = u64::from_be_bytes(bytes[..8].try_into().expect("UUID has 16 bytes"));
    Duration::from_millis(seed % (MAX_JITTER_MS + 1))
}

fn delay(peer_id: Uuid, failures: u8) -> Duration {
    let multiplier = 1u32 << failures.min(5);
    (MIN_INTERVAL * multiplier).min(MAX_BACKOFF) + jitter(peer_id)
}

impl PollSchedule {
    /// Register a peer without scheduling an immediate dial. Newly loaded
    /// observations are stale until an explicit S3 activation starts polling.
    pub(crate) fn register(&mut self, peer_id: Uuid, enabled: bool, now: Instant) -> bool {
        if peer_id.is_nil()
            || (!self.peers.contains_key(&peer_id) && self.peers.len() >= MAX_REGISTERED_PEERS)
        {
            return false;
        }
        let state = self.peers.entry(peer_id).or_insert_with(|| PeerPoll {
            enabled,
            in_flight: false,
            failures: 0,
            next_due: now + delay(peer_id, 0),
        });
        state.enabled = enabled;
        true
    }

    pub(crate) fn remove(&mut self, peer_id: Uuid) {
        if let Some(state) = self.peers.remove(&peer_id) {
            if state.in_flight {
                self.in_flight -= 1;
            }
        }
    }

    /// A manual refresh uses the same due time and capacity gate as a tick.
    pub(crate) fn begin(&mut self, peer_id: Uuid, now: Instant) -> bool {
        if self.in_flight >= MAX_IN_FLIGHT {
            return false;
        }
        let Some(state) = self.peers.get_mut(&peer_id) else {
            return false;
        };
        if !state.enabled || state.in_flight || now < state.next_due {
            return false;
        }
        state.in_flight = true;
        self.in_flight += 1;
        true
    }

    /// Complete one claimed probe. Late completions after removal are ignored.
    pub(crate) fn finish(&mut self, peer_id: Uuid, success: bool, now: Instant) {
        let Some(state) = self.peers.get_mut(&peer_id) else {
            return;
        };
        if !state.in_flight {
            return;
        }
        state.in_flight = false;
        self.in_flight -= 1;
        state.failures = if success {
            0
        } else {
            state.failures.saturating_add(1)
        };
        state.next_due = now + delay(peer_id, state.failures);
    }
}

#[cfg(test)]
mod tests {
    use super::super::registry::LinkDirection;
    use super::*;
    use chrono::Utc;
    use rsi_common::rpc::RpcResponse;
    use rsi_common::satellite::{
        SATELLITE_PROTOCOL_MAJOR, SATELLITE_PROTOCOL_MINOR, SatelliteCapabilitiesV1,
        SatelliteProtocolVersionV1, SatelliteReadLimitsV1, SatelliteUuidV1,
    };
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn disabled_peers_never_claim_and_failures_back_off() {
        let now = Instant::now();
        let peer = Uuid::new_v4();
        let mut schedule = PollSchedule::default();
        assert!(schedule.register(peer, false, now));
        assert!(!schedule.begin(peer, now + Duration::from_secs(60)));
        assert!(schedule.register(peer, true, now));
        assert!(schedule.begin(peer, now + Duration::from_secs(60)));
        assert!(!schedule.begin(peer, now + Duration::from_secs(60)));
        schedule.finish(peer, false, now + Duration::from_secs(60));
        assert!(!schedule.begin(peer, now + Duration::from_secs(85)));
        assert!(schedule.begin(peer, now + Duration::from_secs(94)));
        schedule.finish(peer, true, now + Duration::from_secs(94));
        assert!(!schedule.begin(peer, now + Duration::from_secs(108)));
        assert!(schedule.begin(peer, now + Duration::from_secs(113)));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn global_capacity_and_registration_are_bounded() {
        let now = Instant::now();
        let mut schedule = PollSchedule::default();
        let peers: Vec<_> = (0..MAX_REGISTERED_PEERS).map(|_| Uuid::new_v4()).collect();
        for peer in &peers {
            assert!(schedule.register(*peer, true, now));
        }
        assert!(!schedule.register(Uuid::new_v4(), true, now));
        let due = now + Duration::from_secs(20);
        for peer in peers.iter().take(MAX_IN_FLIGHT) {
            assert!(schedule.begin(*peer, due));
        }
        assert!(!schedule.begin(peers[MAX_IN_FLIGHT], due));
        schedule.remove(peers[0]);
        assert!(schedule.begin(peers[MAX_IN_FLIGHT], due));
        assert!(schedule.register(Uuid::new_v4(), true, now));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn read_rpc_is_bounded_and_never_sends_agent_authority() {
        let (client, server) = UnixStream::pair().unwrap();
        let responder = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            let mut request_line = String::new();
            server.read_line(&mut request_line).await.unwrap();
            let request: RpcRequest = serde_json::from_str(&request_line).unwrap();
            assert_eq!(request.method, "GetSatelliteIdentity");
            assert!(request.session_token.is_none());
            server
                .get_mut()
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n")
                .await
                .unwrap();
        });
        let mut client = BufReader::new(client);
        assert_eq!(
            call_read_rpc(
                &mut client,
                "GetSatelliteIdentity",
                Value::Null,
                256,
                RPC_DEADLINE,
            )
            .await
            .unwrap(),
            json!({"ok":true})
        );
        responder.await.unwrap();

        let (client, mut server) = UnixStream::pair().unwrap();
        let mut client = BufReader::new(client);
        let oversized = tokio::spawn(async move {
            server.write_all(&vec![b'x'; 1024]).await.unwrap();
        });
        assert_eq!(
            call_read_rpc(
                &mut client,
                "ListSatelliteSessions",
                Value::Null,
                32,
                RPC_DEADLINE,
            )
            .await
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
        oversized.await.unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn silent_peer_times_out_and_mutating_method_is_refused() {
        let (client, _silent_server) = UnixStream::pair().unwrap();
        let mut client = BufReader::new(client);
        assert_eq!(
            call_read_rpc(
                &mut client,
                "GetSatelliteIdentity",
                Value::Null,
                256,
                Duration::from_millis(20),
            )
            .await
            .unwrap_err()
            .kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            call_read_rpc(&mut client, "LaunchSession", Value::Null, 256, RPC_DEADLINE,)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn registered_peer_collects_one_bounded_snapshot() {
        let temp = tempfile::Builder::new()
            .prefix("sat-probe-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = temp.path().join("satellites");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = root.join("peer.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        let installation_id = Uuid::new_v4();
        let incarnation_id = Uuid::new_v4();
        let link = RegistryLink {
            id: Uuid::new_v4(),
            direction: LinkDirection::DialHomeReverse,
            socket_path: socket,
            ssh_target: None,
            trust_reference: "ssh-config:peer".into(),
            enabled: true,
            priority: 0,
        };
        let mut peer = RegistryPeer {
            id: Uuid::new_v4(),
            label: "peer".into(),
            expected_installation_id: Some(installation_id),
            enabled: true,
            read_enabled: true,
            launch_enabled: false,
            dispatch_enabled: false,
            links: vec![link.clone()],
        };
        let responder = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut request = String::new();
            stream.read_line(&mut request).await.unwrap();
            assert_eq!(
                serde_json::from_str::<RpcRequest>(&request).unwrap().method,
                "GetSatelliteIdentity"
            );
            let identity = SatelliteIdentityV1 {
                wire_version: SATELLITE_WIRE_VERSION_V1,
                installation_id: SatelliteUuidV1(installation_id),
                daemon_incarnation_id: SatelliteUuidV1(incarnation_id),
                protocol: SatelliteProtocolVersionV1 {
                    major: SATELLITE_PROTOCOL_MAJOR,
                    minor: SATELLITE_PROTOCOL_MINOR,
                },
                capabilities: SatelliteCapabilitiesV1 {
                    session_read: true,
                    limits: SatelliteReadLimitsV1::default(),
                },
            };
            let response =
                RpcResponse::success(Some(json!(1)), serde_json::to_value(identity).unwrap());
            let mut encoded = serde_json::to_vec(&response).unwrap();
            encoded.push(b'\n');
            stream.get_mut().write_all(&encoded).await.unwrap();
            request.clear();
            stream.read_line(&mut request).await.unwrap();
            assert_eq!(
                serde_json::from_str::<RpcRequest>(&request).unwrap().method,
                "ListSatelliteSessions"
            );
            let page = SatelliteSessionPageV1 {
                wire_version: SATELLITE_WIRE_VERSION_V1,
                installation_id: SatelliteUuidV1(installation_id),
                daemon_incarnation_id: SatelliteUuidV1(incarnation_id),
                snapshot_id: SatelliteUuidV1(Uuid::new_v4()),
                snapshot_total_sessions: 0,
                snapshot_offset: 0,
                observed_at: Utc::now(),
                sessions: Vec::new(),
                next_cursor: None,
            };
            let response =
                RpcResponse::success(Some(json!(1)), serde_json::to_value(page).unwrap());
            let mut encoded = serde_json::to_vec(&response).unwrap();
            encoded.push(b'\n');
            stream.get_mut().write_all(&encoded).await.unwrap();
        });
        let mut continuity = PeerContinuity::new(Some(installation_id));
        let snapshot = probe_link(&root, &peer, &link, &mut continuity)
            .await
            .unwrap();
        assert_eq!(snapshot.installation_id, installation_id);
        assert_eq!(snapshot.incarnation_id, incarnation_id);
        assert!(snapshot.sessions.is_empty());
        responder.await.unwrap();
        let mut forged = link.clone();
        forged.socket_path = root.join("forged.sock");
        assert_eq!(
            probe_link(&root, &peer, &forged, &mut continuity)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        peer.enabled = false;
        peer.read_enabled = false;
        assert_eq!(
            probe_link(&root, &peer, &link, &mut continuity)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
