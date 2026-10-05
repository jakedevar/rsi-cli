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
    AgentRequestDeployReceiptV1, DEPLOY_BUILD_NOT_SUPPORTED, DEPLOY_DIR_NOT_ALLOWED,
    DEPLOY_EXECUTE_REQUIRED, DEPLOY_IN_PROGRESS, DEPLOY_KEY_CONFLICT, DEPLOY_NEEDS_SUPERVISOR,
    DEPLOY_RESTART_BUDGET, DEPLOY_SCHEMA_DOWNGRADE, DEPLOY_SHA_INVALID, DEPLOY_SHA_MISMATCH,
    DEPLOY_STAGE_FAILED, DEPLOY_TARGET_MISMATCH, SATELLITE_DEPLOY_OWNER_REQUIRED,
    SATELLITE_DEPLOY_UNREACHABLE,
};
use rsi_common::satellite::{SatelliteIdentityV1, SatelliteUuidV1};
use rsi_common::satellite_dispatch::{SATELLITE_TARGET_NOT_AUTHORIZED, SatelliteDeployRequestV1};
use tokio::io::BufReader;
use tokio::net::UnixStream;

use super::link::connect_owned_socket;
use super::poll::{
    DeployReply, MAX_RPC_ENVELOPE_BYTES, RPC_DEADLINE, call_deploy_rpc, call_read_rpc,
};
use super::registry::{ContinuityResult, PeerContinuity, RegistryLink, RegistryPeer};
use crate::error::{DaemonError, Result};

/// Staging copies and hashes several binaries before the satellite replies.
#[cfg(not(test))]
const DEPLOY_RPC_DEADLINE: Duration = Duration::from_secs(90);
#[cfg(test)]
const DEPLOY_RPC_DEADLINE: Duration = Duration::from_secs(10);

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
    "deploy_binary_missing",
    "deploy_idempotency_key_invalid",
    "deploy_invalid_request",
    "satellite_message_invalid",
];

fn classify_refusal(message: &str) -> &'static str {
    // The satellite's error text is `<kind>: <code>` (DaemonError's Display,
    // for example "Policy denied: deploy_in_progress"). Match the code after the
    // last ": " exactly, so a longer remote string that merely contains a listed
    // code collapses to the generic refusal.
    let code = message
        .rsplit_once(": ")
        .map_or(message, |(_, code)| code)
        .trim();
    PASS_THROUGH
        .iter()
        .find(|known| code == **known)
        .copied()
        .unwrap_or("satellite_deploy_refused")
}

fn links(peer: &RegistryPeer) -> Vec<&RegistryLink> {
    let mut links: Vec<_> = peer.links.iter().filter(|link| link.enabled).collect();
    links.sort_by_key(|link| (link.priority, link.id));
    links
}

/// One verified stream to the peer: owner-owned socket, identity handshake and
/// installation continuity against the operator-paired identity.
async fn open_verified(
    root: &Path,
    peer: &RegistryPeer,
    link: &RegistryLink,
) -> std::io::Result<(BufReader<UnixStream>, SatelliteIdentityV1)> {
    peer.validate(root).map_err(std::io::Error::other)?;
    if !peer.enabled || !peer.read_enabled || peer.expected_installation_id.is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "satellite peer is disabled or unpaired",
        ));
    }
    let socket = connect_owned_socket(root, &link.socket_path, RPC_DEADLINE).await?;
    let mut rpc = BufReader::new(socket);
    let value = call_read_rpc(
        &mut rpc,
        "GetSatelliteIdentity",
        serde_json::Value::Null,
        MAX_RPC_ENVELOPE_BYTES,
        RPC_DEADLINE,
    )
    .await?;
    let identity: SatelliteIdentityV1 =
        serde_json::from_value(value).map_err(std::io::Error::other)?;
    identity.validate().map_err(std::io::Error::other)?;
    let mut continuity = PeerContinuity::new(peer.expected_installation_id);
    continuity.begin_round();
    if continuity.observe(
        link.id,
        identity.installation_id.0,
        identity.daemon_incarnation_id.0,
    ) != ContinuityResult::Matched
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "satellite installation identity requires review",
        ));
    }
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
        let Ok((mut rpc, _identity)) = open_verified(root, peer, link).await else {
            continue;
        };
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

/// Read one peer's daemon info over the link now. Any failure is a
/// `reachable:false` row with a stable error code, never a stale identity.
pub(crate) async fn read_peer_daemon_info(
    root: &Path,
    peer: &RegistryPeer,
) -> SatelliteDaemonInfoV1 {
    let checked_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    let unreachable = |error: &str| SatelliteDaemonInfoV1 {
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
            error: None,
        };
    }
    unreachable("satellite_unreachable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn refusal_codes_pass_through_only_as_the_exact_code_after_the_error_prefix() {
        let prefixed = format!("Policy denied: {DEPLOY_IN_PROGRESS}");
        assert_eq!(classify_refusal(&prefixed), DEPLOY_IN_PROGRESS);
        assert_eq!(classify_refusal(DEPLOY_SHA_MISMATCH), DEPLOY_SHA_MISMATCH);
        let embedded = format!("Policy denied: remote said {DEPLOY_IN_PROGRESS} and more");
        assert_eq!(classify_refusal(&embedded), "satellite_deploy_refused");
        assert_eq!(
            classify_refusal("Invalid parameter: invalid hub deploy: missing field"),
            "satellite_deploy_refused"
        );
    }
}
