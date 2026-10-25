//! Hub side of #1017 slice 2: custody-checked calls to a paired satellite for
//! a hub-initiated deploy and for reading the satellite's daemon info.
//!
//! Both use the same link discipline as queued delivery: an owner-owned mode
//! 0600 socket, the versioned identity handshake, and installation continuity
//! against the operator-paired identity before anything else is sent. The hub
//! stores nothing here; the satellite's own deploy row is the record.

use std::path::Path;
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use rsi_common::agent_daemon_info::SatelliteDaemonInfoV1;
use rsi_common::agent_deploy::{
    AgentRequestDeployReceiptV1, DEPLOY_BUILD_NOT_SUPPORTED, DEPLOY_CANCEL_TOO_LATE,
    DEPLOY_DIR_NOT_ALLOWED, DEPLOY_EXECUTE_REQUIRED, DEPLOY_IN_PROGRESS, DEPLOY_KEY_CONFLICT,
    DEPLOY_NEEDS_SUPERVISOR, DEPLOY_NOT_FOUND, DEPLOY_RESTART_BUDGET, DEPLOY_SCHEMA_DOWNGRADE,
    DEPLOY_SHA_INVALID, DEPLOY_SHA_MISMATCH, DEPLOY_STAGE_FAILED, DEPLOY_TARGET_MISMATCH,
    SATELLITE_DEPLOY_INTERRUPT_UNSUPPORTED, SATELLITE_DEPLOY_OWNER_REQUIRED,
    SATELLITE_DEPLOY_UNREACHABLE,
};
use rsi_common::satellite::{SatelliteIdentityV1, SatelliteUuidV1};
use rsi_common::satellite_dispatch::{SATELLITE_TARGET_NOT_AUTHORIZED, SatelliteDeployRequestV1};
use tokio::io::BufReader;
use tokio::net::UnixStream;

use super::link::connect_owned_socket;
use super::poll::{DeployReply, MAX_RPC_ENVELOPE_BYTES, RPC_DEADLINE, call_deploy_rpc};
use super::registry::{ContinuityResult, PeerContinuity, RegistryLink, RegistryPeer};
use crate::error::{DaemonError, Result};

/// Staging copies and hashes several binaries before the satellite replies.
#[cfg(not(test))]
const DEPLOY_RPC_DEADLINE: Duration = Duration::from_secs(90);
#[cfg(test)]
const DEPLOY_RPC_DEADLINE: Duration = Duration::from_secs(10);

/// #3291: a reseat launches a whole session on the satellite before it
/// replies, which took longer than `RPC_DEADLINE` (3 s) on every real reseat
/// while the satellite still completed it.
#[cfg(not(test))]
const RESEAT_RPC_DEADLINE: Duration = Duration::from_secs(60);
#[cfg(test)]
const RESEAT_RPC_DEADLINE: Duration = Duration::from_secs(10);

/// #3291: the stable reason appended to `satellite_reseat_uncertain`: the
/// request reached the satellite but no usable reply came back.
fn reseat_uncertain_reason(error: &std::io::Error) -> &'static str {
    if error.kind() == std::io::ErrorKind::TimedOut {
        "reply_timed_out"
    } else {
        "reply_lost"
    }
}

/// #3291: a failure AFTER the request was written may have been a success on
/// the satellite, so it is never reported as unreachable.
fn reseat_uncertain(reason: &str) -> DaemonError {
    DaemonError::PolicyDenied(format!(
        "{} reason={reason}",
        rsi_common::manager_reseat::SATELLITE_RESEAT_UNCERTAIN
    ))
}

/// Stable codes a satellite refusal may carry through to the manager; anything
/// else collapses to `satellite_deploy_refused` so remote text never leaks.
const PASS_THROUGH: &[&str] = &[
    SATELLITE_TARGET_NOT_AUTHORIZED,
    SATELLITE_DEPLOY_OWNER_REQUIRED,
    DEPLOY_NEEDS_SUPERVISOR,
    DEPLOY_TARGET_MISMATCH,
    DEPLOY_RESTART_BUDGET,
    DEPLOY_IN_PROGRESS,
    DEPLOY_KEY_CONFLICT,
    DEPLOY_DIR_NOT_ALLOWED,
    DEPLOY_SHA_INVALID,
    DEPLOY_SHA_MISMATCH,
    DEPLOY_SCHEMA_DOWNGRADE,
    DEPLOY_STAGE_FAILED,
    DEPLOY_BUILD_NOT_SUPPORTED,
    DEPLOY_EXECUTE_REQUIRED,
    DEPLOY_CANCEL_TOO_LATE,
    DEPLOY_NOT_FOUND,
    "deploy_binary_missing",
    "deploy_idempotency_key_invalid",
    "deploy_invalid_request",
    SATELLITE_DEPLOY_INTERRUPT_UNSUPPORTED,
    "satellite_message_invalid",
];

fn classify_refusal(message: &str) -> &'static str {
    // The satellite's error text is `<kind>: <code>` (DaemonError's Display,
    // for example "Policy denied: deploy_in_progress"). Match the code after the
    // last ": " exactly, so a longer remote string that merely contains a listed
    // code collapses to the generic refusal.
    // #3423: the directory refusal carries detail after its code (the received
    // path and the allowed roots), so it is read from the front.
    // Hub #3884: the stage and missing-member refusals carry the member and the
    // failed step after their code, so they are read from the front too.
    let after_kind = message.split_once(": ").map_or(message, |(_, rest)| rest);
    for detailed_code in [
        DEPLOY_DIR_NOT_ALLOWED,
        DEPLOY_STAGE_FAILED,
        "deploy_binary_missing",
    ] {
        let detailed = format!("{detailed_code}: ");
        if message.starts_with(&detailed) || after_kind.starts_with(&detailed) {
            return detailed_code;
        }
    }
    let code = message
        .rsplit_once(": ")
        .map_or(message, |(_, code)| code)
        .trim();
    // #1777: the budget refusal appends ` retry_after=<RFC3339>` to its code.
    let code = code
        .split_once(rsi_common::agent_deploy::DEPLOY_RETRY_AFTER_MARKER)
        .map_or(code, |(code, _)| code);
    PASS_THROUGH
        .iter()
        .find(|known| code == **known)
        .copied()
        .unwrap_or("satellite_deploy_refused")
}

pub(crate) fn links(peer: &RegistryPeer) -> Vec<&RegistryLink> {
    let mut links: Vec<_> = peer.links.iter().filter(|link| link.enabled).collect();
    links.sort_by_key(|link| (link.priority, link.id));
    links
}

/// One verified stream to the peer: owner-owned socket, identity handshake and
/// installation continuity against the operator-paired identity.
pub(crate) async fn open_verified(
    root: &Path,
    peer: &RegistryPeer,
    link: &RegistryLink,
) -> std::io::Result<(BufReader<UnixStream>, SatelliteIdentityV1)> {
    let mut attempt = super::diagnostics::Attempt::new(peer.id, link);
    attempt.observe_tunnel(link).await;
    peer.validate(root).map_err(std::io::Error::other)?;
    if !peer.enabled || !peer.read_enabled || peer.expected_installation_id.is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "satellite peer is disabled or unpaired",
        ));
    }
    let start = std::time::Instant::now();
    let socket = connect_owned_socket(root, &link.socket_path, RPC_DEADLINE).await;
    attempt.value.connect = super::diagnostics::measured(start, &socket);
    let mut rpc = BufReader::new(socket?);
    let identity =
        super::poll::read_identity_diagnosed(&mut rpc, &mut attempt, RPC_DEADLINE).await?;
    let mut continuity = PeerContinuity::new(peer.expected_installation_id);
    continuity.begin_round();
    if continuity.observe(
        link.id,
        identity.installation_id.0,
        identity.daemon_incarnation_id.0,
    ) != ContinuityResult::Matched
    {
        attempt.value.daemon_rpc.state =
            rsi_common::satellite_diagnostics::SatelliteStageStateV1::Refused;
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "satellite installation identity requires review",
        ));
    }
    attempt.identity(&identity);
    super::record_reported_health(peer.id, identity.health.as_ref());
    Ok((rpc, identity))
}

/// Send one deploy request over the first link that verifies.
///
/// # Errors
/// `satellite_deploy_unreachable` when no link verifies (or the reply is lost;
/// the caller retries with the same idempotency key), or the satellite's stable
/// refusal code.
pub(crate) async fn request_hub_deploy(
    root: &Path,
    peer: &RegistryPeer,
    wire: &SatelliteDeployRequestV1,
) -> Result<AgentRequestDeployReceiptV1> {
    for link in links(peer) {
        let Ok((mut rpc, identity)) = open_verified(root, peer, link).await else {
            continue;
        };
        // #2587: an older satellite refuses the unknown field. Say so rather
        // than send nothing useful or drop the option and time out waiting.
        if wire.interrupt_workers == Some(true) && !identity.capabilities.deploy_interrupt {
            return Err(DaemonError::PolicyDenied(
                SATELLITE_DEPLOY_INTERRUPT_UNSUPPORTED.into(),
            ));
        }
        return match call_deploy_rpc(&mut rpc, wire, DEPLOY_RPC_DEADLINE).await {
            Ok(DeployReply::Accepted(receipt)) => Ok(receipt),
            Ok(DeployReply::Refused(message)) => {
                Err(DaemonError::PolicyDenied(classify_refusal(&message).into()))
            }
            Err(_) => Err(DaemonError::PolicyDenied(
                SATELLITE_DEPLOY_UNREACHABLE.into(),
            )),
        };
    }
    Err(DaemonError::PolicyDenied(
        SATELLITE_DEPLOY_UNREACHABLE.into(),
    ))
}

/// Policy writes use the same custody and paired-identity handshake as deploy.
pub(crate) async fn push_manager_policy(
    root: &Path,
    peer: &RegistryPeer,
    wire: &rsi_common::satellite_manager_policy::SatelliteManagerPolicyRequestV1,
) -> Result<rsi_common::satellite_manager_policy::SatelliteManagerPolicyReceiptV1> {
    use rsi_common::satellite_manager_policy::*;
    for link in links(peer) {
        let Ok((mut rpc, _)) = open_verified(root, peer, link).await else {
            continue;
        };
        let response = super::poll::exchange(
            &mut rpc,
            REQUEST_HUB_MANAGER_POLICY_METHOD,
            serde_json::to_value(wire)?,
            MAX_RPC_ENVELOPE_BYTES,
            RPC_DEADLINE,
        )
        .await
        .map_err(|_| DaemonError::PolicyDenied(POLICY_UNREACHABLE.into()))?;
        if let Some(error) = response.error {
            if let Some(data) = error.data.as_ref()
                && data.get("code").and_then(serde_json::Value::as_str) == Some(POLICY_STALE)
                && let Ok(detail) =
                    serde_json::from_value::<SatelliteManagerPolicyStaleV1>(data.clone())
            {
                let safe = detail.rpc_error();
                return Err(DaemonError::StructuredRpc {
                    rpc_code: safe.code,
                    message: safe.message,
                    data: safe.data.unwrap_or_default(),
                });
            }
            let code = error
                .message
                .rsplit_once(": ")
                .map_or(error.message.as_str(), |(_, code)| code)
                .trim();
            let safe = [
                SATELLITE_TARGET_NOT_AUTHORIZED,
                POLICY_INVALID,
                POLICY_STALE,
                POLICY_KEY_CONFLICT,
            ]
            .into_iter()
            .find(|known| *known == code)
            .unwrap_or(POLICY_REFUSED);
            return Err(DaemonError::PolicyDenied(safe.into()));
        }
        return serde_json::from_value(
            response
                .result
                .ok_or_else(|| DaemonError::PolicyDenied(POLICY_UNREACHABLE.into()))?,
        )
        .map_err(|_| DaemonError::PolicyDenied(POLICY_UNREACHABLE.into()));
    }
    Err(DaemonError::PolicyDenied(POLICY_UNREACHABLE.into()))
}

/// #2750: a manager reseat uses the same custody and paired-identity
/// handshake as a policy push. Only the stable refusal codes the satellite
/// can raise pass through; anything else is `satellite_reseat_refused`.
/// `satellite_reseat_unreachable` means the request never reached the
/// satellite; once it was sent, a lost or late reply is
/// `satellite_reseat_uncertain` (#3291), because the satellite may have
/// completed the reseat.
pub(crate) async fn reseat_manager(
    root: &Path,
    peer: &RegistryPeer,
    wire: &rsi_common::manager_reseat::SatelliteManagerReseatRequestV1,
) -> Result<rsi_common::manager_reseat::SatelliteManagerReseatReceiptV1> {
    use rsi_common::manager_reseat::*;
    for link in links(peer) {
        let Ok((mut rpc, _)) = open_verified(root, peer, link).await else {
            continue;
        };
        let response = super::poll::exchange(
            &mut rpc,
            REQUEST_HUB_MANAGER_RESEAT_METHOD,
            serde_json::to_value(wire)?,
            MAX_RPC_ENVELOPE_BYTES,
            RESEAT_RPC_DEADLINE,
        )
        .await
        .map_err(|error| reseat_uncertain(reseat_uncertain_reason(&error)))?;
        if let Some(error) = response.error {
            let code = error
                .message
                .rsplit_once(": ")
                .map_or(error.message.as_str(), |(_, code)| code)
                .trim();
            let safe = [
                rsi_common::satellite_dispatch::SATELLITE_TARGET_NOT_AUTHORIZED,
                SATELLITE_RESEAT_INVALID,
                RESEAT_NOT_CONFIGURED,
                RESEAT_REVOKED,
                RESEAT_NO_SEAT,
                RESEAT_SEAT_LIVE,
                RESEAT_SEAT_NOT_FAILED,
                RESEAT_ROOT_REQUIRED,
            ]
            .into_iter()
            .find(|known| *known == code)
            .unwrap_or(SATELLITE_RESEAT_REFUSED);
            return Err(DaemonError::PolicyDenied(safe.into()));
        }
        return serde_json::from_value(
            response
                .result
                .ok_or_else(|| reseat_uncertain("reply_empty"))?,
        )
        .map_err(|_| reseat_uncertain("reply_malformed"));
    }
    Err(DaemonError::PolicyDenied(
        SATELLITE_RESEAT_UNREACHABLE.into(),
    ))
}

/// The health a peer reports on its identity over the first link that
/// verifies; `None` when none does or the peer reports none (#2747).
pub(crate) async fn read_peer_health(
    root: &Path,
    peer: &RegistryPeer,
) -> Option<rsi_common::satellite::SatelliteHealthV1> {
    for link in links(peer) {
        let Ok((_rpc, identity)) = open_verified(root, peer, link).await else {
            continue;
        };
        return identity.health;
    }
    None
}

/// Read one peer's daemon info over the link now. Any failure is a
/// `reachable:false` row with a stable error code, never a stale identity.
pub(crate) async fn read_peer_daemon_info(
    root: &Path,
    peer: &RegistryPeer,
) -> SatelliteDaemonInfoV1 {
    let checked_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    let unreachable = |error: &str| SatelliteDaemonInfoV1 {
        sessions_running: None,
        workers_parked: None,
        open_jobs: None,
        diagnostics: crate::store_support::satellite::reachability(peer.id),
        latest_watchdog_trip: crate::store_support::satellite::latest_watchdog_trip(peer.id),
        platform: None,
        peer_id: SatelliteUuidV1(peer.id),
        label: peer.label.clone(),
        reachable: false,
        checked_at: checked_at.clone(),
        build_sha: None,
        binary_sha256: None,
        started_at: None,
        uptime_secs: None,
        schema_version: None,
        supervisor_mode: None,
        last_deploy: None,
        launches: None,
        manager_policies: None,
        usage: Vec::new(),
        error: Some(error.to_owned()),
    };
    for link in links(peer) {
        let Ok((_rpc, identity)) = open_verified(root, peer, link).await else {
            continue;
        };
        let Some(health) = identity.health else {
            return unreachable("satellite_reports_no_health");
        };
        return SatelliteDaemonInfoV1 {
            sessions_running: Some(health.sessions_running),
            workers_parked: health.workers_parked,
            open_jobs: health.open_jobs,
            diagnostics: crate::store_support::satellite::reachability(peer.id),
            latest_watchdog_trip: health.latest_watchdog_trip,
            manager_policies: health.manager_policies,
            usage: health.usage,
            platform: health.platform,
            peer_id: SatelliteUuidV1(peer.id),
            label: peer.label.clone(),
            reachable: true,
            checked_at,
            build_sha: health.build_sha,
            binary_sha256: health.binary_sha256,
            started_at: health.started_at,
            uptime_secs: Some(health.uptime_seconds),
            schema_version: health.schema_version,
            supervisor_mode: health.supervisor_mode,
            last_deploy: health.last_deploy,
            launches: rsi_common::satellite_manager_policy::launches_summary(
                &health.manager_launches,
            ),
            error: None,
        };
    }
    unreachable("satellite_unreachable")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #3291: a reply timeout after the request was sent names the timeout and
    /// is never the "never reached" code.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn reseat_reply_failures_are_uncertain_and_name_the_timeout() {
        let timed_out = std::io::Error::new(std::io::ErrorKind::TimedOut, "read timed out");
        let lost = std::io::Error::from(std::io::ErrorKind::UnexpectedEof);
        assert_eq!(reseat_uncertain_reason(&timed_out), "reply_timed_out");
        assert_eq!(reseat_uncertain_reason(&lost), "reply_lost");
        let text = reseat_uncertain("reply_timed_out").to_string();
        assert!(
            text.contains("satellite_reseat_uncertain reason=reply_timed_out"),
            "{text}"
        );
        assert!(!text.contains("unreachable"), "{text}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn delayed_daemon_and_connection_refusal_have_distinct_diagnostics_through_recovery() {
        use rsi_common::satellite_diagnostics::{
            SatelliteStageStateV1 as State, SatelliteTunnelStateV1 as Tunnel,
        };
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        use tokio::net::UnixListener;
        use uuid::Uuid;

        let temp = crate::test_support::socket_tempdir("sat-diag-");
        let root = temp.path().join("satellites");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = root.join("peer.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let installation = Uuid::new_v4();
        let peer = RegistryPeer {
            id: Uuid::new_v4(),
            label: "fixture".into(),
            expected_installation_id: Some(installation),
            enabled: true,
            read_enabled: true,
            launch_enabled: false,
            dispatch_enabled: false,
            links: vec![RegistryLink {
                id: Uuid::new_v4(),
                direction: super::super::registry::LinkDirection::DialHomeReverse,
                socket_path: socket.clone(),
                ssh_target: None,
                trust_reference: "fixture".into(),
                enabled: true,
                priority: 0,
            }],
        };
        let restart_id = Uuid::new_v4();
        let mut identity = crate::satellite::identity(installation, Uuid::new_v4());
        identity.health = Some(
            serde_json::from_value(serde_json::json!({
                "daemon_version":"1.0.0", "uptime_seconds":1,
                "sessions_running":1, "sessions_waiting_approval":0,
                "workers_parked":2, "open_jobs":2,
                "store_probe_elapsed_ms":17,
                "usage":[{"provider":"Claude","window":"1h","invocations":9,
                          "input_tokens":1,"output_tokens":2,"cache_creation_tokens":3,
                          "cache_read_tokens":6300000,"reasoning_tokens":0}],
                "latest_watchdog_trip": {
                    "restart_id":restart_id,
                    "observed_at":"2026-10-19T04:17:24.000000000Z",
                    "last_healthy_at":"2026-10-19T04:15:24.000000000Z",
                    "failed_probes":["rpc_timeout", "store_probe_timeout"]
                }
            }))
            .unwrap(),
        );
        let good_identity = identity.clone();
        let server = tokio::spawn(async move {
            // Healthy response, then a connection whose daemon reply is delayed.
            let (stream, _) = listener.accept().await.unwrap();
            let mut rpc = BufReader::new(stream);
            let mut line = String::new();
            rpc.read_line(&mut line).await.unwrap();
            let request: rsi_common::rpc::RpcRequest = serde_json::from_str(&line).unwrap();
            let response = rsi_common::rpc::RpcResponse::success(
                request.id,
                serde_json::to_value(good_identity).unwrap(),
            );
            let mut bytes = serde_json::to_vec(&response).unwrap();
            bytes.push(b'\n');
            rpc.get_mut().write_all(&bytes).await.unwrap();
            let (delayed, _) = listener.accept().await.unwrap();
            tokio::time::sleep(RPC_DEADLINE + Duration::from_millis(100)).await;
            drop(delayed);
            // Dropping listener leaves a refused socket for the next attempt.
        });
        let healthy = read_peer_daemon_info(&root, &peer).await;
        assert!(healthy.reachable);
        assert_eq!(
            (
                healthy.sessions_running,
                healthy.workers_parked,
                healthy.open_jobs
            ),
            (Some(1), Some(2), Some(2))
        );
        // #4008: the satellite's per-provider usage reaches the hub's row.
        assert_eq!(healthy.usage.len(), 1);
        assert_eq!(
            (
                healthy.usage[0].provider.as_str(),
                healthy.usage[0].invocations
            ),
            ("Claude", 9)
        );
        assert_eq!(healthy.usage[0].cache_read_tokens, 6_300_000);
        let diagnostics = healthy.diagnostics.unwrap();
        assert_eq!(diagnostics.connect.state, State::Succeeded);
        assert_eq!(diagnostics.daemon_rpc.state, State::Succeeded);
        assert_eq!(diagnostics.store_probe.elapsed_ms, Some(17));
        assert_eq!(diagnostics.auth.state, State::Unobserved);
        assert_eq!(diagnostics.tunnel_service, Tunnel::RemoteService);
        let trip = healthy.latest_watchdog_trip.unwrap();

        let delayed = read_peer_daemon_info(&root, &peer).await;
        assert!(!delayed.reachable);
        assert_eq!(
            (
                delayed.sessions_running,
                delayed.workers_parked,
                delayed.open_jobs
            ),
            (None, None, None)
        );
        let diagnostics = delayed.diagnostics.unwrap();
        assert_eq!(diagnostics.connect.state, State::Succeeded);
        assert_eq!(diagnostics.transport.state, State::Succeeded);
        assert_eq!(diagnostics.daemon_rpc.state, State::TimedOut);
        assert!(diagnostics.daemon_rpc.elapsed_ms.unwrap() >= 2_900);
        assert_eq!(diagnostics.store_probe.state, State::Unobserved);
        assert_eq!(diagnostics.auth.state, State::Unobserved);
        assert_eq!(delayed.latest_watchdog_trip, Some(trip.clone()));
        server.await.unwrap();

        let refused = read_peer_daemon_info(&root, &peer).await;
        assert!(!refused.reachable);
        let diagnostics = refused.diagnostics.unwrap();
        assert_eq!(diagnostics.connect.state, State::Refused);
        assert_eq!(diagnostics.daemon_rpc.state, State::NotAttempted);
        assert_eq!(refused.latest_watchdog_trip, Some(trip.clone()));

        std::fs::remove_file(&socket).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        // A live older satellite has unknown additive counts, rather than zero.
        identity.health.as_mut().unwrap().workers_parked = None;
        identity.health.as_mut().unwrap().open_jobs = None;
        let recovered = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut rpc = BufReader::new(stream);
            let mut line = String::new();
            rpc.read_line(&mut line).await.unwrap();
            let request: rsi_common::rpc::RpcRequest = serde_json::from_str(&line).unwrap();
            let response = rsi_common::rpc::RpcResponse::success(
                request.id,
                serde_json::to_value(identity).unwrap(),
            );
            let mut bytes = serde_json::to_vec(&response).unwrap();
            bytes.push(b'\n');
            rpc.get_mut().write_all(&bytes).await.unwrap();
        });
        let recovered_info = read_peer_daemon_info(&root, &peer).await;
        recovered.await.unwrap();
        assert!(recovered_info.reachable);
        assert_eq!(
            (
                recovered_info.sessions_running,
                recovered_info.workers_parked,
                recovered_info.open_jobs
            ),
            (Some(1), None, None)
        );
        assert_eq!(
            recovered_info.diagnostics.unwrap().daemon_rpc.state,
            State::Succeeded
        );
        assert_eq!(recovered_info.latest_watchdog_trip, Some(trip));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn refusal_codes_pass_through_only_as_the_exact_code_after_the_error_prefix() {
        let prefixed = format!("Policy denied: {DEPLOY_IN_PROGRESS}");
        assert_eq!(classify_refusal(&prefixed), DEPLOY_IN_PROGRESS);
        let detailed = format!(
            "Invalid parameter: {DEPLOY_STAGE_FAILED}: member target-pool: syntax check failed: x: y"
        );
        assert_eq!(classify_refusal(&detailed), DEPLOY_STAGE_FAILED);
        assert_eq!(classify_refusal(DEPLOY_SHA_MISMATCH), DEPLOY_SHA_MISMATCH);
        assert_eq!(
            classify_refusal("Invalid parameter: deploy_not_found"),
            DEPLOY_NOT_FOUND
        );
        // #1777: the budget refusal's retry_after suffix keeps its code.
        assert_eq!(
            classify_refusal(
                "Policy denied: deploy_restart_budget retry_after=2026-10-08T10:12:00Z"
            ),
            DEPLOY_RESTART_BUDGET
        );
        // #3423: the directory refusal names the path and roots after its code.
        assert_eq!(
            classify_refusal(
                "Invalid parameter: deploy_directory_not_allowed: /x/target/release is outside every allowed root; allowed roots: /a, /b"
            ),
            DEPLOY_DIR_NOT_ALLOWED
        );
        let embedded = format!("Policy denied: remote said {DEPLOY_IN_PROGRESS} and more");
        assert_eq!(classify_refusal(&embedded), "satellite_deploy_refused");
        assert_eq!(
            classify_refusal("Invalid parameter: invalid hub deploy: missing field"),
            "satellite_deploy_refused"
        );
    }
}
