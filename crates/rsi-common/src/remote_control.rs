//! Operator wire types for configuring RSI Remote from the TUI (#1096).
//!
//! These cross the operator-only `RemoteGetStatus` / `RemoteSetConfig` RPCs.
//! They carry ids and display names only: never credentials, cookies or paths
//! to secrets. Agents have no verb for them.

use serde::{Deserialize, Serialize};

/// Highest-trust Tailscale line the gateway was qualified against. Patch
/// builds `>=` [`QUALIFIED_TAILSCALE_MIN_PATCH`] in this line are accepted.
pub const QUALIFIED_TAILSCALE_LINE: &str = "1.102";
pub const QUALIFIED_TAILSCALE_MIN_PATCH: u32 = 3;
/// Hard cap on exposed projects, matching the gateway policy validator.
pub const MAX_REMOTE_PROJECTS: usize = 32;

/// Whether a `tailscale version` string is inside the qualified line.
#[must_use]
pub fn tailscale_version_qualified(version: &str) -> bool {
    let core = version.split(['-', '+', ' ']).next().unwrap_or("");
    let mut parts = core.split('.');
    let (Some(major), Some(minor), Some(patch)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    format!("{major}.{minor}") == QUALIFIED_TAILSCALE_LINE
        && patch
            .parse::<u32>()
            .is_ok_and(|patch| patch >= QUALIFIED_TAILSCALE_MIN_PATCH)
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemotePeerV1 {
    /// Stable node id (what the policy's `allowed_node_ids` holds).
    pub id: String,
    pub name: String,
    pub os: String,
    pub online: bool,
    pub allowed: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteProjectV1 {
    pub id: String,
    pub name: String,
    pub exposed: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteTailscaleV1 {
    pub reachable: bool,
    pub backend_state: String,
    pub version: String,
    pub pinned_version: String,
    pub version_qualified: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteStatusV1 {
    pub enabled: bool,
    /// Tailnet host the phone opens, from the policy or live detection.
    pub canonical_host: String,
    /// `https://<host>/` once a host is known.
    pub url: Option<String>,
    pub unit_installed: bool,
    pub gateway_running: bool,
    pub serve_route_present: bool,
    /// True unless Funnel is on for the host. Never enabled by RSI.
    pub funnel_off: bool,
    /// The exact one-time command the operator must run, when serve could not
    /// be applied without privilege.
    pub serve_pending_command: Option<String>,
    pub tailscale: RemoteTailscaleV1,
    pub peers: Vec<RemotePeerV1>,
    pub projects: Vec<RemoteProjectV1>,
    /// Human-readable problems (validation failures, missing binary...).
    pub errors: Vec<String>,
}

/// Partial update: absent fields keep their current value. Owner id and
/// canonical host are always auto-detected, never supplied.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteSetConfigRequestV1 {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub allowed_node_ids: Option<Vec<String>>,
    #[serde(default)]
    pub project_ids: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_line_accepts_patch_builds_at_or_above_the_pin() {
        assert!(tailscale_version_qualified("1.102.3"));
        assert!(tailscale_version_qualified("1.102.4"));
        assert!(tailscale_version_qualified("1.102.4-t3caf7d9e7-g1234"));
        assert!(!tailscale_version_qualified("1.102.2"));
        assert!(!tailscale_version_qualified("1.103.0"));
        assert!(!tailscale_version_qualified("1.10.23"));
        assert!(!tailscale_version_qualified(""));
    }

    #[test]
    fn set_config_request_rejects_unknown_fields() {
        let ok: RemoteSetConfigRequestV1 =
            serde_json::from_str(r#"{"enabled":true,"project_ids":[]}"#).unwrap();
        assert_eq!(ok.enabled, Some(true));
        assert!(
            serde_json::from_str::<RemoteSetConfigRequestV1>(r#"{"canonical_host":"x.ts.net"}"#)
                .is_err()
        );
    }
}
