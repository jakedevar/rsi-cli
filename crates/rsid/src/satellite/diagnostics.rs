//! Hub observations only: no error text, paths, SSH targets or credentials.

use super::registry::{LinkDirection, RegistryLink};
use rsi_common::satellite::SatelliteIdentityV1;
use rsi_common::satellite_diagnostics::{
    MAX_STAGE_ELAPSED_MS, SatelliteReachabilityV1, SatelliteStageStateV1 as State,
    SatelliteStageV1 as Stage, SatelliteTunnelStateV1 as Tunnel,
};
use std::io;
use std::time::Instant;
use uuid::Uuid;

pub(crate) fn elapsed_ms(start: Instant) -> u32 {
    start
        .elapsed()
        .as_millis()
        .min(u128::from(MAX_STAGE_ELAPSED_MS)) as u32
}

pub(crate) fn measured<T>(start: Instant, result: &io::Result<T>) -> Stage {
    Stage {
        state: match result {
            Ok(_) => State::Succeeded,
            Err(error) => match error.kind() {
                io::ErrorKind::TimedOut => State::TimedOut,
                io::ErrorKind::PermissionDenied | io::ErrorKind::ConnectionRefused => {
                    State::Refused
                }
                io::ErrorKind::InvalidData => State::InvalidResponse,
                _ => State::IoFailure,
            },
        },
        elapsed_ms: Some(elapsed_ms(start)),
    }
}

/// RAII publishes even failed attempts without changing the caller's errors.
pub(crate) struct Attempt {
    peer_id: Uuid,
    start: Instant,
    pub(crate) value: SatelliteReachabilityV1,
}

impl Attempt {
    pub(crate) fn new(peer_id: Uuid, link: &RegistryLink) -> Self {
        Self {
            peer_id,
            start: Instant::now(),
            value: SatelliteReachabilityV1 {
                link_id: link.id,
                checked_at: chrono::Utc::now(),
                elapsed_ms: 0,
                connect: Stage::unmeasured(State::NotAttempted),
                transport: Stage::unmeasured(State::NotAttempted),
                daemon_rpc: Stage::unmeasured(State::NotAttempted),
                rpc: rsi_common::satellite_diagnostics::SatelliteProbeRpcV1::Identity,
                store_probe: Stage::unmeasured(State::Unobserved),
                auth: Stage::unmeasured(State::Unobserved),
                tunnel_service: if link.direction == LinkDirection::DialHomeReverse {
                    Tunnel::RemoteService
                } else {
                    Tunnel::Unmapped
                },
                tunnel_service_elapsed_ms: None,
            },
        }
    }

    pub(crate) async fn observe_tunnel(&mut self, link: &RegistryLink) {
        let start = Instant::now();
        self.value.tunnel_service = tunnel_state(link).await;
        self.value.tunnel_service_elapsed_ms = Some(elapsed_ms(start));
    }

    pub(crate) fn identity(&mut self, identity: &SatelliteIdentityV1) {
        if let Some(ms) = identity
            .health
            .as_ref()
            .and_then(|h| h.store_probe_elapsed_ms)
        {
            self.value.store_probe = Stage {
                state: State::Succeeded,
                elapsed_ms: Some(ms),
            };
        }
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        self.value.elapsed_ms = elapsed_ms(self.start);
        crate::store_support::satellite::record_reachability(self.peer_id, self.value.clone());
    }
}

// The installer fixes both the socket suffix and service instance alphabet.
// Arbitrary registry endpoints cannot be mapped safely to a local service.
fn direct_unit(link: &RegistryLink) -> Option<String> {
    let name = link
        .socket_path
        .file_name()?
        .to_str()?
        .strip_suffix("-direct.sock")?;
    if name.is_empty()
        || name.len() > 32
        || !name.as_bytes()[0].is_ascii_alphanumeric()
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        return None;
    }
    Some(format!("rsi-satellite-direct@{name}.service"))
}

#[cfg(any(target_os = "linux", test))]
fn parse_service(bytes: &[u8]) -> Tunnel {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Tunnel::Unavailable;
    };
    if text.lines().any(|line| line == "LoadState=not-found") {
        return Tunnel::NotFound;
    }
    if !text.lines().any(|line| line == "LoadState=loaded") {
        return Tunnel::Unavailable;
    }
    match text
        .lines()
        .find_map(|line| line.strip_prefix("ActiveState="))
    {
        Some("active") => Tunnel::Active,
        Some("inactive") => Tunnel::Inactive,
        Some("activating") => Tunnel::Activating,
        Some("deactivating") => Tunnel::Deactivating,
        Some("failed") => Tunnel::Failed,
        _ => Tunnel::Unavailable,
    }
}

async fn tunnel_state(link: &RegistryLink) -> Tunnel {
    if link.direction == LinkDirection::DialHomeReverse {
        return Tunnel::RemoteService;
    }
    let Some(unit) = direct_unit(link) else {
        return Tunnel::Unmapped;
    };
    #[cfg(target_os = "linux")]
    {
        use std::process::Stdio;
        use tokio::io::AsyncReadExt;
        // Query only. One attempt, 250 ms, 256 output bytes; never journal text.
        let query = async {
            let mut child = tokio::process::Command::new("systemctl")
                .args(["--user", "show", &unit, "--property=LoadState,ActiveState"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            let mut bytes = Vec::new();
            child
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("missing pipe"))?
                .take(257)
                .read_to_end(&mut bytes)
                .await?;
            if bytes.len() > 256 {
                return Ok(Tunnel::Unavailable);
            }
            let status = child.wait().await?;
            let state = parse_service(&bytes);
            Ok::<_, io::Error>(if status.success() || state == Tunnel::NotFound {
                state
            } else {
                Tunnel::Unavailable
            })
        };
        match tokio::time::timeout(std::time::Duration::from_millis(250), query).await {
            Ok(Ok(state)) => state,
            Ok(Err(_)) => Tunnel::Unavailable,
            Err(_) => Tunnel::TimedOut,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = unit;
        Tunnel::Unsupported
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn tunnel_service_mapping_and_output_are_closed_and_bounded() {
        let mut link = RegistryLink {
            id: Uuid::new_v4(),
            direction: LinkDirection::DirectLocalForward,
            socket_path: "/fixture/laptop-direct.sock".into(),
            ssh_target: Some("fixture".into()),
            trust_reference: "fixture".into(),
            enabled: true,
            priority: 0,
        };
        assert_eq!(
            direct_unit(&link).as_deref(),
            Some("rsi-satellite-direct@laptop.service")
        );
        link.socket_path = "/fixture/custom.sock".into();
        assert_eq!(direct_unit(&link), None);
        link.socket_path = "/fixture/--bad-direct.sock".into();
        assert_eq!(direct_unit(&link), None);
        for (bytes, state) in [
            (
                &b"ActiveState=active\nLoadState=loaded\n"[..],
                Tunnel::Active,
            ),
            (
                &b"LoadState=loaded\nActiveState=failed\n"[..],
                Tunnel::Failed,
            ),
            (
                &b"LoadState=not-found\nActiveState=inactive\n"[..],
                Tunnel::NotFound,
            ),
            (
                &b"LoadState=loaded\nActiveState=unexpected secret text\n"[..],
                Tunnel::Unavailable,
            ),
        ] {
            assert_eq!(parse_service(bytes), state);
        }
        assert_eq!(
            measured::<()>(
                Instant::now(),
                &Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "private path"
                ))
            )
            .state,
            State::Refused
        );
    }
}
