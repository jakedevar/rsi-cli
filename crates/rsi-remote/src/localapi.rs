//! Pure decoders and predicates for the three read-only Tailscale `LocalAPI`
//! responses the gateway depends on.
//!
//! Nothing here performs I/O; callers cap and read the body, then hand the
//! bytes to these functions.

use crate::config::Config;
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use std::net::IpAddr;
use std::path::Path;

/// Canonical Go zero time used by tailcfg for a non-expiring key.
const ZERO_EXPIRY: &str = "0001-01-01T00:00:00Z";
/// Unix seconds of `0001-01-01T00:00:00Z`.
const ZERO_EXPIRY_UNIX: i64 = -62_135_596_800;

/// Why a `LocalAPI` observation is refused. Codes are stable `snake_case` strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    Malformed,
    NotRunning,
    MissingSelf,
    ServerMismatch,
    NotAuthorized,
    Expired,
    KeyExpiring,
    WrongOwner,
    NodeNotAllowed,
    Tagged,
    Shared,
    SourceMismatch,
    RouteMissing,
    RouteMismatch,
    FunnelOn,
}

impl Denial {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::NotRunning => "not_running",
            Self::MissingSelf => "missing_self",
            Self::ServerMismatch => "server_mismatch",
            Self::NotAuthorized => "not_authorized",
            Self::Expired => "expired",
            Self::KeyExpiring => "key_expiring",
            Self::WrongOwner => "wrong_owner",
            Self::NodeNotAllowed => "node_not_allowed",
            Self::Tagged => "tagged",
            Self::Shared => "shared",
            Self::SourceMismatch => "source_mismatch",
            Self::RouteMissing => "route_missing",
            Self::RouteMismatch => "route_mismatch",
            Self::FunnelOn => "funnel_on",
        }
    }
}

/// The bound server identity taken from `GET /localapi/v0/status?peers=false`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerBinding {
    pub node_id: String,
    pub user_id: u64,
    pub dns_name: String,
    pub magic_dns_suffix: String,
    pub tailscale_ips: Vec<IpAddr>,
}

/// The client identity taken from `GET /localapi/v0/whois?addr=<ip>&proto=tcp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    pub node_stable_id: String,
    pub user_id: u64,
}

fn parse(bytes: &[u8]) -> Result<Value, Denial> {
    serde_json::from_slice(bytes).map_err(|_| Denial::Malformed)
}

/// Numeric IDs must be lossless integers: floats, negatives and non-numbers deny.
fn strict_u64(value: &Value) -> Result<u64, Denial> {
    value.as_u64().ok_or(Denial::Malformed)
}

fn nonempty_str(value: Option<&Value>) -> Result<&str, Denial> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(Denial::Malformed)
}

const fn strict_bool_opt(value: Option<&Value>) -> Result<Option<bool>, Denial> {
    match value {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(Denial::Malformed),
    }
}

fn is_zero_expiry(s: &str) -> bool {
    if s == ZERO_EXPIRY {
        return true;
    }
    DateTime::parse_from_rfc3339(s).is_ok_and(|t| t.timestamp() == ZERO_EXPIRY_UNIX)
}

/// D4b `KeyExpiry` rule: omitted or canonical zero is non-expiring locally; any
/// other value must parse and be strictly later than `now + 30s`.
fn key_expiry_ok(value: Option<&Value>, now: DateTime<Utc>) -> Result<(), Denial> {
    let Some(value) = value else { return Ok(()) };
    match value {
        Value::String(s) if is_zero_expiry(s) => Ok(()),
        Value::String(s) => {
            let expiry = DateTime::parse_from_rfc3339(s).map_err(|_| Denial::Malformed)?;
            if expiry > now + Duration::seconds(30) {
                Ok(())
            } else {
                Err(Denial::KeyExpiring)
            }
        }
        _ => Err(Denial::Malformed),
    }
}

/// D4b Expired rule for a tailcfg object: `true` denies, omitted/null-free false allows.
fn expired_ok(object: &Value) -> Result<(), Denial> {
    if strict_bool_opt(object.get("Expired"))? == Some(true) {
        Err(Denial::Expired)
    } else {
        Ok(())
    }
}

/// D4b `MachineAuthorized` rule: anything other than an explicit `true` denies.
fn machine_authorized_ok(object: &Value) -> Result<(), Denial> {
    match strict_bool_opt(object.get("MachineAuthorized"))? {
        Some(true) => Ok(()),
        _ => Err(Denial::NotAuthorized),
    }
}

/// Host prefix for an address family: `/32` for v4, `/128` for v6.
const fn host_prefix(ip: &IpAddr) -> u8 {
    match ip {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    }
}

/// Parse a CIDR entry and report whether it names `source` as a whole host.
fn address_matches(entry: &str, source: IpAddr) -> bool {
    let Some((addr, prefix)) = entry.rsplit_once('/') else {
        return false;
    };
    let Ok(ip) = addr.parse::<IpAddr>() else {
        return false;
    };
    let Ok(prefix) = prefix.parse::<u8>() else {
        return false;
    };
    ip == source && prefix == host_prefix(&ip)
}

/// Decode `GET /localapi/v0/status?peers=false`.
///
/// # Errors
/// Returns the [`Denial`] matching the first failed binding requirement.
pub fn server_binding(
    status: &[u8],
    policy: &Config,
    now: DateTime<Utc>,
) -> Result<ServerBinding, Denial> {
    let root = parse(status)?;

    if root.get("BackendState").and_then(Value::as_str) != Some("Running") {
        return Err(Denial::NotRunning);
    }

    let self_node = match root.get("Self") {
        None | Some(Value::Null) => return Err(Denial::MissingSelf),
        Some(node) => node,
    };

    // `Self.ID` is the node's stable ID (a string); `Self.StableID` is unrelated.
    let node_id = nonempty_str(self_node.get("ID"))?;

    let user_id = strict_u64(self_node.get("UserID").ok_or(Denial::Malformed)?)?;
    if user_id == 0 || user_id != policy.owner_user_id {
        return Err(Denial::WrongOwner);
    }

    let raw_ips = self_node
        .get("TailscaleIPs")
        .and_then(Value::as_array)
        .filter(|ips| !ips.is_empty())
        .ok_or(Denial::Malformed)?;
    let mut tailscale_ips = Vec::with_capacity(raw_ips.len());
    for raw in raw_ips {
        let addr = raw.as_str().ok_or(Denial::Malformed)?;
        tailscale_ips.push(addr.parse::<IpAddr>().map_err(|_| Denial::Malformed)?);
    }

    let suffix = nonempty_str(
        root.get("CurrentTailnet")
            .and_then(|tailnet| tailnet.get("MagicDNSSuffix")),
    )?;

    let dns_name = self_node
        .get("DNSName")
        .and_then(Value::as_str)
        .ok_or(Denial::Malformed)?;
    let trimmed = dns_name.strip_suffix('.').unwrap_or(dns_name);

    if trimmed != policy.canonical_host || !policy.canonical_host.ends_with(&format!(".{suffix}")) {
        return Err(Denial::ServerMismatch);
    }

    // `Self` carries no MachineAuthorized field under the pinned tailcfg.
    expired_ok(self_node)?;
    key_expiry_ok(self_node.get("KeyExpiry"), now)?;

    Ok(ServerBinding {
        node_id: node_id.to_string(),
        user_id,
        dns_name: trimmed.to_string(),
        magic_dns_suffix: suffix.to_string(),
        tailscale_ips,
    })
}

/// Decode `GET /localapi/v0/whois?addr=<ip>&proto=tcp` for a client.
///
/// # Errors
/// Returns the [`Denial`] matching the first failed identity requirement.
pub fn client_identity(
    whois: &[u8],
    policy: &Config,
    source: IpAddr,
    now: DateTime<Utc>,
) -> Result<ClientIdentity, Denial> {
    let root = parse(whois)?;

    let profile_id = strict_u64(
        root.get("UserProfile")
            .and_then(|profile| profile.get("ID"))
            .ok_or(Denial::Malformed)?,
    )?;
    let node = match root.get("Node") {
        None | Some(Value::Null) => return Err(Denial::Malformed),
        Some(node) => node,
    };
    let node_user = strict_u64(node.get("User").ok_or(Denial::Malformed)?)?;

    if profile_id == 0
        || node_user == 0
        || profile_id != node_user
        || node_user != policy.owner_user_id
    {
        return Err(Denial::WrongOwner);
    }

    let stable_id = nonempty_str(node.get("StableID"))?;
    if !policy.allowed_node_ids.iter().any(|id| id == stable_id) {
        return Err(Denial::NodeNotAllowed);
    }

    let tagged = match node.get("Tags") {
        None | Some(Value::Null) => false,
        Some(Value::Array(tags)) => !tags.is_empty(),
        Some(_) => return Err(Denial::Malformed),
    };
    if tagged {
        return Err(Denial::Tagged);
    }

    let sharer = match node.get("Sharer") {
        None | Some(Value::Null) => 0,
        Some(value) => strict_u64(value)?,
    };
    if sharer != 0 {
        return Err(Denial::Shared);
    }

    let addresses = node
        .get("Addresses")
        .and_then(Value::as_array)
        .ok_or(Denial::Malformed)?;
    let mut source_seen = false;
    for entry in addresses {
        let entry = entry.as_str().ok_or(Denial::Malformed)?;
        if address_matches(entry, source) {
            source_seen = true;
        }
    }
    if !source_seen {
        return Err(Denial::SourceMismatch);
    }

    machine_authorized_ok(node)?;
    expired_ok(node)?;
    key_expiry_ok(node.get("KeyExpiry"), now)?;

    Ok(ClientIdentity {
        node_stable_id: stable_id.to_string(),
        user_id: node_user,
    })
}

/// Bind a Self `WhoIs` observation to the previously observed server binding.
///
/// # Errors
/// Returns the [`Denial`] matching the first failed binding requirement.
pub fn self_whois_matches(
    whois: &[u8],
    binding: &ServerBinding,
    now: DateTime<Utc>,
) -> Result<(), Denial> {
    let root = parse(whois)?;
    let node = match root.get("Node") {
        None | Some(Value::Null) => return Err(Denial::Malformed),
        Some(node) => node,
    };

    let stable_id = nonempty_str(node.get("StableID"))?;
    if stable_id != binding.node_id {
        return Err(Denial::ServerMismatch);
    }

    let user_id = strict_u64(node.get("User").ok_or(Denial::Malformed)?)?;
    if user_id != binding.user_id {
        return Err(Denial::WrongOwner);
    }

    machine_authorized_ok(node)?;
    expired_ok(node)?;
    key_expiry_ok(node.get("KeyExpiry"), now)?;
    Ok(())
}

fn require_object<'a>(parent: &'a Value, key: &str) -> Result<&'a Value, Denial> {
    match parent.get(key) {
        None | Some(Value::Null) => Err(Denial::RouteMissing),
        Some(Value::Object(_)) => Ok(&parent[key]),
        Some(_) => Err(Denial::RouteMismatch),
    }
}

/// Decode `GET /localapi/v0/serve-config` and require exactly the pinned route.
///
/// # Errors
/// Returns the [`Denial`] matching the first failed route requirement.
pub fn serve_route_ok(
    serve_config: &[u8],
    canonical_host: &str,
    socket_path: &Path,
) -> Result<(), Denial> {
    let root = parse(serve_config)?;
    let host_port = format!("{canonical_host}:443");

    let tcp = require_object(&root, "TCP")?;
    let listener = require_object(tcp, "443")?;
    match listener.get("HTTPS") {
        None | Some(Value::Null) => return Err(Denial::RouteMissing),
        Some(Value::Bool(true)) => {}
        Some(_) => return Err(Denial::RouteMismatch),
    }

    let web = require_object(&root, "Web")?;
    let host = require_object(web, &host_port)?;
    let handlers = match host.get("Handlers") {
        None | Some(Value::Null) => return Err(Denial::RouteMissing),
        Some(Value::Object(handlers)) => handlers,
        Some(_) => return Err(Denial::RouteMismatch),
    };
    if handlers.len() != 1 {
        return Err(Denial::RouteMismatch);
    }
    let Some((path, handler)) = handlers.iter().next() else {
        return Err(Denial::RouteMismatch);
    };
    if path != "/" {
        return Err(Denial::RouteMismatch);
    }
    let handler = match handler {
        Value::Object(_) => handler,
        _ => return Err(Denial::RouteMismatch),
    };
    let expected_proxy = format!("unix:{}", socket_path.display());
    match handler.get("Proxy") {
        None | Some(Value::Null) => return Err(Denial::RouteMissing),
        Some(Value::String(proxy)) if *proxy == expected_proxy => {}
        Some(_) => return Err(Denial::RouteMismatch),
    }

    match root.get("AllowFunnel") {
        None | Some(Value::Null) => {}
        Some(Value::Object(funnel)) => match funnel.get(&host_port) {
            None | Some(Value::Null | Value::Bool(false)) => {}
            Some(Value::Bool(true)) => return Err(Denial::FunnelOn),
            Some(_) => return Err(Denial::RouteMismatch),
        },
        // An unreadable Funnel map cannot prove Funnel is off.
        Some(_) => return Err(Denial::RouteMismatch),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const SOCKET: &str = "/run/rsi-remote/ingress.sock";
    const HOST: &str = "host.example.ts.net";
    const NODE: &str = "nABCD123";
    const OWNER: u64 = 12_345;

    fn policy() -> Config {
        Config {
            enabled: true,
            canonical_host: HOST.into(),
            owner_user_id: OWNER,
            allowed_node_ids: vec![NODE.into()],
            project_ids: vec![],
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-28T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn encode(value: &Value) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }

    fn status() -> Value {
        json!({
            "BackendState": "Running",
            "Self": {
                "ID": NODE,
                "StableID": "nSelfStableIgnored",
                "UserID": OWNER,
                "DNSName": "host.example.ts.net.",
                "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0::1"],
                "Expired": false,
                "KeyExpiry": "2027-01-01T00:00:00Z"
            },
            "CurrentTailnet": { "MagicDNSSuffix": "example.ts.net" },
            "Peer": { "ignored": true }
        })
    }

    fn whois() -> Value {
        json!({
            "UserProfile": { "ID": OWNER, "LoginName": "owner@example.com" },
            "Node": {
                "StableID": NODE,
                "User": OWNER,
                "MachineAuthorized": true,
                "Expired": false,
                "KeyExpiry": "2027-01-01T00:00:00Z",
                "Addresses": ["100.101.102.103/32", "fd7a:115c:a1e0::1/128"],
                "Tags": [],
                "Sharer": 0
            }
        })
    }

    fn serve() -> Value {
        json!({
            "TCP": { "443": { "HTTPS": true, "HTTP": false } },
            "Web": {
                "host.example.ts.net:443": {
                    "Handlers": { "/": { "Proxy": "unix:/run/rsi-remote/ingress.sock" } }
                }
            },
            "AllowFunnel": { "host.example.ts.net:443": false }
        })
    }

    fn source() -> IpAddr {
        "100.101.102.103".parse().unwrap()
    }

    #[test]
    fn denial_codes_are_stable_snake_case() {
        assert_eq!(Denial::Malformed.code(), "malformed");
        assert_eq!(Denial::NotRunning.code(), "not_running");
        assert_eq!(Denial::MissingSelf.code(), "missing_self");
        assert_eq!(Denial::ServerMismatch.code(), "server_mismatch");
        assert_eq!(Denial::NotAuthorized.code(), "not_authorized");
        assert_eq!(Denial::Expired.code(), "expired");
        assert_eq!(Denial::KeyExpiring.code(), "key_expiring");
        assert_eq!(Denial::WrongOwner.code(), "wrong_owner");
        assert_eq!(Denial::NodeNotAllowed.code(), "node_not_allowed");
        assert_eq!(Denial::Tagged.code(), "tagged");
        assert_eq!(Denial::Shared.code(), "shared");
        assert_eq!(Denial::SourceMismatch.code(), "source_mismatch");
        assert_eq!(Denial::RouteMissing.code(), "route_missing");
        assert_eq!(Denial::RouteMismatch.code(), "route_mismatch");
        assert_eq!(Denial::FunnelOn.code(), "funnel_on");
    }

    #[test]
    fn status_accepts_pinned_binding() {
        let binding = server_binding(&encode(&status()), &policy(), now()).unwrap();
        assert_eq!(binding.node_id, NODE);
        assert_eq!(binding.user_id, OWNER);
        assert_eq!(binding.dns_name, HOST);
        assert_eq!(binding.magic_dns_suffix, "example.ts.net");
        assert_eq!(
            binding.tailscale_ips,
            vec![
                "100.101.102.103".parse::<IpAddr>().unwrap(),
                "fd7a:115c:a1e0::1".parse::<IpAddr>().unwrap(),
            ]
        );
    }

    #[test]
    fn status_zero_key_expiry_is_accepted() {
        let mut value = status();
        value["Self"]["KeyExpiry"] = json!("0001-01-01T00:00:00Z");
        assert!(server_binding(&encode(&value), &policy(), now()).is_ok());
    }

    #[test]
    fn status_missing_key_expiry_is_accepted() {
        let mut value = status();
        value["Self"].as_object_mut().unwrap().remove("KeyExpiry");
        assert!(server_binding(&encode(&value), &policy(), now()).is_ok());
    }

    #[test]
    fn status_denials_are_exact() {
        let cases: Vec<(Value, Denial)> = vec![
            (
                json!({"BackendState": "Stopped", "Self": null}),
                Denial::NotRunning,
            ),
            (
                {
                    let mut v = status();
                    v["Self"] = Value::Null;
                    v
                },
                Denial::MissingSelf,
            ),
            (
                {
                    let mut v = status();
                    v["Self"].as_object_mut().unwrap().remove("ID");
                    v
                },
                Denial::Malformed,
            ),
            (
                {
                    let mut v = status();
                    v["Self"]["ID"] = json!("");
                    v
                },
                Denial::Malformed,
            ),
            (
                {
                    let mut v = status();
                    v["Self"]["UserID"] = json!(999);
                    v
                },
                Denial::WrongOwner,
            ),
            (
                {
                    let mut v = status();
                    v["Self"]["UserID"] = json!(0);
                    v
                },
                Denial::WrongOwner,
            ),
            (
                {
                    let mut v = status();
                    v["Self"]["UserID"] = json!(12_345.5);
                    v
                },
                Denial::Malformed,
            ),
            (
                {
                    let mut v = status();
                    v["Self"]["UserID"] = json!(-1);
                    v
                },
                Denial::Malformed,
            ),
            (
                {
                    let mut v = status();
                    v["Self"]["TailscaleIPs"] = json!([]);
                    v
                },
                Denial::Malformed,
            ),
            (
                {
                    let mut v = status();
                    v["Self"]["TailscaleIPs"] = json!(["not-an-ip"]);
                    v
                },
                Denial::Malformed,
            ),
            (
                {
                    let mut v = status();
                    v["CurrentTailnet"]["MagicDNSSuffix"] = json!("");
                    v
                },
                Denial::Malformed,
            ),
            (
                {
                    let mut v = status();
                    v["Self"]["DNSName"] = json!("other.example.ts.net.");
                    v
                },
                Denial::ServerMismatch,
            ),
            (
                {
                    let mut v = status();
                    v["CurrentTailnet"]["MagicDNSSuffix"] = json!("other.ts.net");
                    v
                },
                Denial::ServerMismatch,
            ),
            (
                {
                    let mut v = status();
                    v["Self"]["Expired"] = json!(true);
                    v
                },
                Denial::Expired,
            ),
            (
                {
                    let mut v = status();
                    v["Self"]["KeyExpiry"] = json!("2026-09-28T12:00:10Z");
                    v
                },
                Denial::KeyExpiring,
            ),
            (
                {
                    let mut v = status();
                    v["Self"]["KeyExpiry"] = json!("2026-09-28T11:59:00Z");
                    v
                },
                Denial::KeyExpiring,
            ),
            (
                {
                    let mut v = status();
                    v["Self"]["KeyExpiry"] = json!(null);
                    v
                },
                Denial::Malformed,
            ),
            (
                {
                    let mut v = status();
                    v["Self"]["KeyExpiry"] = json!("not-a-timestamp");
                    v
                },
                Denial::Malformed,
            ),
            (
                {
                    let mut v = status();
                    v["Self"]["KeyExpiry"] = json!(1_700_000_000);
                    v
                },
                Denial::Malformed,
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(
                server_binding(&encode(&value), &policy(), now()),
                Err(expected)
            );
        }
        assert_eq!(
            server_binding(b"{not json", &policy(), now()),
            Err(Denial::Malformed)
        );
    }

    #[test]
    fn whois_accepts_pinned_client() {
        let identity = client_identity(&encode(&whois()), &policy(), source(), now()).unwrap();
        assert_eq!(
            identity,
            ClientIdentity {
                node_stable_id: NODE.into(),
                user_id: OWNER,
            }
        );
    }

    #[test]
    fn whois_zero_key_expiry_is_accepted() {
        let mut value = whois();
        value["Node"]["KeyExpiry"] = json!("0001-01-01T00:00:00Z");
        assert!(client_identity(&encode(&value), &policy(), source(), now()).is_ok());
    }

    #[test]
    fn whois_denials_are_exact() {
        let cases: Vec<(Value, Denial)> = vec![
            (
                {
                    let mut v = whois();
                    v["UserProfile"]["ID"] = json!(1);
                    v
                },
                Denial::WrongOwner,
            ),
            (
                {
                    let mut v = whois();
                    v["Node"]["User"] = json!(1);
                    v
                },
                Denial::WrongOwner,
            ),
            (
                {
                    let mut v = whois();
                    v["UserProfile"]["ID"] = json!(0);
                    v
                },
                Denial::WrongOwner,
            ),
            (
                {
                    let mut v = whois();
                    v["UserProfile"]["ID"] = json!(12_345.5);
                    v
                },
                Denial::Malformed,
            ),
            (
                {
                    let mut v = whois();
                    v["Node"]["StableID"] = json!("nOTHER");
                    v
                },
                Denial::NodeNotAllowed,
            ),
            (
                {
                    let mut v = whois();
                    v["Node"]["Tags"] = json!(["tag:server"]);
                    v
                },
                Denial::Tagged,
            ),
            (
                {
                    let mut v = whois();
                    v["Node"]["Sharer"] = json!(777);
                    v
                },
                Denial::Shared,
            ),
            (
                {
                    let mut v = whois();
                    v["Node"]["Sharer"] = json!(-3);
                    v
                },
                Denial::Malformed,
            ),
            (
                {
                    let mut v = whois();
                    v["Node"]["Addresses"] = json!(["100.101.102.104/32"]);
                    v
                },
                Denial::SourceMismatch,
            ),
            (
                {
                    let mut v = whois();
                    v["Node"]["Addresses"] = json!(["100.101.102.0/24"]);
                    v
                },
                Denial::SourceMismatch,
            ),
            (
                {
                    let mut v = whois();
                    v["Node"]["Addresses"] = json!(["100.101.102.103/128"]);
                    v
                },
                Denial::SourceMismatch,
            ),
            (
                {
                    let mut v = whois();
                    v["Node"]
                        .as_object_mut()
                        .unwrap()
                        .remove("MachineAuthorized");
                    v
                },
                Denial::NotAuthorized,
            ),
            (
                {
                    let mut v = whois();
                    v["Node"]["MachineAuthorized"] = json!(false);
                    v
                },
                Denial::NotAuthorized,
            ),
            (
                {
                    let mut v = whois();
                    v["Node"]["Expired"] = json!(true);
                    v
                },
                Denial::Expired,
            ),
            (
                {
                    let mut v = whois();
                    v["Node"]["KeyExpiry"] = json!("2026-09-28T12:00:10Z");
                    v
                },
                Denial::KeyExpiring,
            ),
            (
                {
                    let mut v = whois();
                    v["Node"]["KeyExpiry"] = json!(null);
                    v
                },
                Denial::Malformed,
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(
                client_identity(&encode(&value), &policy(), source(), now()),
                Err(expected)
            );
        }
    }

    #[test]
    fn self_whois_matches_binding() {
        let binding = server_binding(&encode(&status()), &policy(), now()).unwrap();
        assert_eq!(
            self_whois_matches(&encode(&whois()), &binding, now()),
            Ok(())
        );
    }

    #[test]
    fn self_whois_denials_are_exact() {
        let binding = server_binding(&encode(&status()), &policy(), now()).unwrap();
        let cases: Vec<(Value, Denial)> = vec![
            (
                {
                    let mut v = whois();
                    v["Node"]["StableID"] = json!("nOTHER");
                    v
                },
                Denial::ServerMismatch,
            ),
            (
                {
                    let mut v = whois();
                    v["Node"]["User"] = json!(1);
                    v
                },
                Denial::WrongOwner,
            ),
            (
                {
                    let mut v = whois();
                    v["Node"]
                        .as_object_mut()
                        .unwrap()
                        .remove("MachineAuthorized");
                    v
                },
                Denial::NotAuthorized,
            ),
            (
                {
                    let mut v = whois();
                    v["Node"]["Expired"] = json!(true);
                    v
                },
                Denial::Expired,
            ),
            (
                {
                    let mut v = whois();
                    v["Node"]["KeyExpiry"] = json!("2026-09-28T12:00:10Z");
                    v
                },
                Denial::KeyExpiring,
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(
                self_whois_matches(&encode(&value), &binding, now()),
                Err(expected)
            );
        }
    }

    #[test]
    fn serve_route_accepts_pinned_route() {
        let path = Path::new(SOCKET);
        assert_eq!(serve_route_ok(&encode(&serve()), HOST, path), Ok(()));
    }

    #[test]
    fn serve_route_denials_are_exact() {
        let path = Path::new(SOCKET);
        let cases: Vec<(Value, Denial)> = vec![
            (
                {
                    let mut v = serve();
                    v["TCP"]["443"]["HTTPS"] = json!(false);
                    v
                },
                Denial::RouteMismatch,
            ),
            (
                {
                    let mut v = serve();
                    v["TCP"].as_object_mut().unwrap().remove("443");
                    v
                },
                Denial::RouteMissing,
            ),
            (
                {
                    let mut v = serve();
                    v.as_object_mut().unwrap().remove("Web");
                    v
                },
                Denial::RouteMissing,
            ),
            (
                {
                    let mut v = serve();
                    v["Web"]["host.example.ts.net:443"]["Handlers"]["/"]["Proxy"] =
                        json!("unix:/tmp/other.sock");
                    v
                },
                Denial::RouteMismatch,
            ),
            (
                {
                    let mut v = serve();
                    v["Web"]["host.example.ts.net:443"]["Handlers"]["//other"] =
                        json!({ "Proxy": "unix:/run/rsi-remote/ingress.sock" });
                    v
                },
                Denial::RouteMismatch,
            ),
            (
                {
                    let mut v = serve();
                    v["Web"]["host.example.ts.net:443"]["Handlers"]
                        .as_object_mut()
                        .unwrap()
                        .remove("/");
                    v
                },
                Denial::RouteMismatch,
            ),
            (
                {
                    let mut v = serve();
                    v["AllowFunnel"]["host.example.ts.net:443"] = json!(true);
                    v
                },
                Denial::FunnelOn,
            ),
            (
                {
                    let mut v = serve();
                    v["AllowFunnel"] = json!(true);
                    v
                },
                Denial::RouteMismatch,
            ),
            (json!({ "TCP": null }), Denial::RouteMissing),
            (json!({ "TCP": [] }), Denial::RouteMismatch),
        ];
        for (value, expected) in cases {
            assert_eq!(serve_route_ok(&encode(&value), HOST, path), Err(expected));
        }
        assert_eq!(serve_route_ok(b"nope", HOST, path), Err(Denial::Malformed));
    }

    #[test]
    fn serve_route_funnel_absent_is_accepted() {
        let mut value = serve();
        value.as_object_mut().unwrap().remove("AllowFunnel");
        assert_eq!(
            serve_route_ok(&encode(&value), HOST, Path::new(SOCKET)),
            Ok(())
        );
    }
}
