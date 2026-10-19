//! Hub side of #2120: start or poll one full-library QA run on a paired
//! satellite over the #1017 link.
//!
//! The same link discipline as a hub deploy: an owner-owned mode 0600 socket,
//! the versioned identity handshake and installation continuity against the
//! operator-paired identity before anything is sent. The identity read is
//! also the live qualification of a new placement: the satellite must
//! report the hub's OS, advertise `qa_run` and report a QA admission that
//! admits now (its build governor's floor included, #179). The hub stores the
//! binding in its own queue receipts.
//!
//! #180: an answer is either a receipt, a provable non-effect (nothing was
//! sent, or the satellite answered with a listed refusal that it raises before
//! it records a run) or uncertain (the request was sent and no valid answer
//! came back, or the satellite answered with an error that does not prove it
//! started nothing: the satellite may be running it, #2156). Callers never
//! place the run elsewhere on an uncertain answer.

use std::path::Path;
use std::time::Duration;

use rsi_common::satellite::SatelliteIdentityV1;
use rsi_common::satellite_qa::{
    REQUEST_HUB_QA_RUN_METHOD, SATELLITE_QA_DEPLOY_PENDING, SATELLITE_QA_INVALID,
    SATELLITE_QA_REFUSED, SATELLITE_QA_UNREACHABLE, SATELLITE_QA_UNSUPPORTED,
    SatelliteQaRunReceiptV1, SatelliteQaRunRequestV1, classify_refusal,
};

use super::deploy_link::{links, open_verified};
use super::poll::{MAX_RPC_ENVELOPE_BYTES, RPC_DEADLINE, exchange};
use super::registry::RegistryPeer;

/// A start fetches `origin/rolling` (when the commit is missing) and checks
/// out a snapshot before the satellite replies.
#[cfg(not(test))]
const START_DEADLINE: Duration = Duration::from_secs(120);
#[cfg(test)]
const START_DEADLINE: Duration = Duration::from_secs(10);

/// The satellite's live health carries no QA admission.
pub(crate) const SATELLITE_QA_NO_HEALTH: &str = "satellite_reports_no_health";
pub(crate) const SATELLITE_PLATFORM_UNKNOWN: &str = "satellite_platform_unknown";
pub(crate) const SATELLITE_PLATFORM_MISMATCH: &str = "satellite_platform_mismatch";

/// Missing platform fails closed, including identities from older builds.
fn platform_admission(identity: &SatelliteIdentityV1, hub_os: &str) -> Result<(), QaLinkError> {
    let platform = identity
        .health
        .as_ref()
        .and_then(|health| health.platform.as_ref())
        .ok_or(QaLinkError::NoEffect(SATELLITE_PLATFORM_UNKNOWN))?;
    if platform.os != hub_os {
        return Err(QaLinkError::NoEffect(SATELLITE_PLATFORM_MISMATCH));
    }
    Ok(())
}

/// Why a start or poll returned no receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QaLinkError {
    /// This call provably started nothing: no link verified, the satellite
    /// does not qualify (from its live identity), or it answered with a
    /// listed refusal it raises before any run exists.
    NoEffect(&'static str),
    /// The request was sent and no valid answer came back (a lost or
    /// malformed reply), or the satellite answered with an error that does not
    /// prove it started nothing (an unlisted or internal error, such as a
    /// launch error that left the job alive): the satellite may already be
    /// running it.
    Uncertain(&'static str),
}

impl QaLinkError {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::NoEffect(code) | Self::Uncertain(code) => code,
        }
    }
}

/// What a satellite's error answer proves (#2156). Only a listed refusal
/// (`SATELLITE_QA_PASS_THROUGH`) is raised before the satellite records a run
/// or after it confirmed nothing runs; the collapsed code of any other error
/// (internal, launch or unknown) leaves the run possibly alive there.
fn error_effect(message: &str) -> QaLinkError {
    match classify_refusal(message) {
        SATELLITE_QA_REFUSED => QaLinkError::Uncertain(SATELLITE_QA_REFUSED),
        code => QaLinkError::NoEffect(code),
    }
}

/// A new placement needs a matching platform and a QA admission that admits.
/// #2176: on that platform a staged or restarting deploy refuses it before
/// other admission gates, whatever an older satellite's admission says.
fn admission(identity: &SatelliteIdentityV1) -> Result<(), QaLinkError> {
    use rsi_common::agent_deploy::DeployState;
    platform_admission(identity, std::env::consts::OS)?;
    let health = identity
        .health
        .as_ref()
        .ok_or(QaLinkError::NoEffect(SATELLITE_QA_NO_HEALTH))?;
    if health
        .last_deploy
        .as_ref()
        .is_some_and(|deploy| matches!(deploy.state, DeployState::Staged | DeployState::Restarting))
    {
        return Err(QaLinkError::NoEffect(SATELLITE_QA_DEPLOY_PENDING));
    }
    let admission = health
        .qa_admission
        .as_ref()
        .ok_or(QaLinkError::NoEffect(SATELLITE_QA_NO_HEALTH))?;
    admission.admits().map_err(QaLinkError::NoEffect)
}

/// Start (`wire.start`) or report one run over the first link that verifies.
/// `require_admission` qualifies a new placement from the satellite's live
/// platform and QA admission first; a poll or the replayed start that reconciles an
/// uncertain dispatch skips it (its own run makes the satellite busy).
///
/// # Errors
/// [`QaLinkError::NoEffect`] with `satellite_qa_unsupported`,
/// `satellite_platform_unknown`, `satellite_platform_mismatch`,
/// `satellite_reports_no_health`, the satellite's admission or listed refusal
/// code, or `satellite_qa_unreachable` (no link verified);
/// [`QaLinkError::Uncertain`] with `satellite_qa_unreachable` (the reply was
/// lost), `satellite_qa_invalid` (the reply was malformed) or
/// `satellite_qa_refused` (the satellite answered with an unlisted error).
pub(crate) async fn request_qa_run(
    root: &Path,
    peer: &RegistryPeer,
    wire: &SatelliteQaRunRequestV1,
    require_admission: bool,
) -> Result<SatelliteQaRunReceiptV1, QaLinkError> {
    for link in links(peer) {
        let Ok((mut rpc, identity)) = open_verified(root, peer, link).await else {
            continue;
        };
        // Keep the displayed platform/rejection reason tied to this live
        // handshake, including an older peer that reports no health.
        super::record_reported_health(peer.id, identity.health.as_ref());
        if !identity.capabilities.qa_run {
            return Err(QaLinkError::NoEffect(SATELLITE_QA_UNSUPPORTED));
        }
        if require_admission {
            admission(&identity)?;
        }
        let params =
            serde_json::to_value(wire).map_err(|_| QaLinkError::NoEffect(SATELLITE_QA_INVALID))?;
        let deadline = if wire.start {
            START_DEADLINE
        } else {
            RPC_DEADLINE
        };
        // From here the request may have reached the satellite.
        let response = exchange(
            &mut rpc,
            REQUEST_HUB_QA_RUN_METHOD,
            params,
            MAX_RPC_ENVELOPE_BYTES,
            deadline,
        )
        .await
        .map_err(|_| QaLinkError::Uncertain(SATELLITE_QA_UNREACHABLE))?;
        // #2156: a listed refusal is raised before the satellite records a
        // run; any other error (a launch error whose unit could not be
        // stopped leaves the job running) is not a proof that nothing started.
        if let Some(error) = response.error {
            return Err(error_effect(&error.message));
        }
        let receipt: SatelliteQaRunReceiptV1 = response
            .result
            .and_then(|value| serde_json::from_value(value).ok())
            .ok_or(QaLinkError::Uncertain(SATELLITE_QA_INVALID))?;
        receipt
            .validate(wire.run_id)
            .map_err(QaLinkError::Uncertain)?;
        // Satellite text reaches a hub QA Issue: keep display-safe shapes only.
        return Ok(receipt.sanitized());
    }
    Err(QaLinkError::NoEffect(SATELLITE_QA_UNREACHABLE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::satellite_qa::{
        SATELLITE_QA_BUSY, SATELLITE_QA_LAUNCH_FAILED, SATELLITE_QA_NOT_FOUND,
    };

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn linux_qa_requires_a_reported_linux_platform() {
        let mut identity = crate::satellite::identity(uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        assert_eq!(
            platform_admission(&identity, "linux"),
            Err(QaLinkError::NoEffect(SATELLITE_PLATFORM_UNKNOWN))
        );
        identity.health = Some(
            serde_json::from_value(serde_json::json!({
                "daemon_version": "1.0.0", "uptime_seconds": 1,
                "sessions_running": 0, "sessions_waiting_approval": 0
            }))
            .unwrap(),
        );
        assert_eq!(
            platform_admission(&identity, "linux"),
            Err(QaLinkError::NoEffect(SATELLITE_PLATFORM_UNKNOWN))
        );
        for (os, expected) in [
            ("linux", Ok(())),
            (
                "macos",
                Err(QaLinkError::NoEffect(SATELLITE_PLATFORM_MISMATCH)),
            ),
            (
                "windows",
                Err(QaLinkError::NoEffect(SATELLITE_PLATFORM_MISMATCH)),
            ),
        ] {
            identity.health.as_mut().unwrap().platform =
                Some(rsi_common::satellite::SatellitePlatformV1 {
                    os: os.into(),
                    arch: "aarch64".into(),
                });
            assert_eq!(platform_admission(&identity, "linux"), expected);
        }
    }

    /// #2156: only the listed refusals prove a non-effect. The raw error of a
    /// launch whose unit could not be stopped (the job stays running on the
    /// satellite), an internal error and any unknown text are uncertain.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn only_listed_refusals_prove_a_non_effect() {
        assert_eq!(
            error_effect("Policy denied: satellite_qa_busy"),
            QaLinkError::NoEffect(SATELLITE_QA_BUSY)
        );
        assert_eq!(
            error_effect("Policy denied: satellite_qa_launch_failed"),
            QaLinkError::NoEffect(SATELLITE_QA_LAUNCH_FAILED)
        );
        assert_eq!(
            error_effect("Policy denied: satellite_qa_not_found"),
            QaLinkError::NoEffect(SATELLITE_QA_NOT_FOUND)
        );
        for message in [
            "Process error: job_launch_failed: controller unavailable after dispatch; job j remains running because service stop could not be confirmed",
            "Process error: hub QA run prepare: task panicked",
            "Internal error",
            "",
        ] {
            assert_eq!(
                error_effect(message),
                QaLinkError::Uncertain(SATELLITE_QA_REFUSED),
                "{message}"
            );
        }
    }
}
