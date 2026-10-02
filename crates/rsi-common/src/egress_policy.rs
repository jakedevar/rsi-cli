//! Network egress policy for Harness tools (#774).
//!
//! Pure types and address classification; no I/O. rsid enforces the policy at
//! connect time (`session::harness::egress`); `offline` additionally isolates
//! the shell tool's network namespace, while `deny_private` does not restrict
//! the shell. The mode is operator-only: a daemon default
//! (`harness_egress_mode`) layered under the per-session `HarnessToolPolicy`.
//! No agent verb carries or widens it.
//!
//! Every mode fails closed. There is deliberately no "allow everything" mode
//! and no host allow-list: a network-capable tool reaches the public web and
//! never the host's private surfaces.

use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Stable refusal codes: the `Error: <code>: ...` prefix of a settled tool row.
pub const EGRESS_DENIED: &str = "egress_denied";
pub const EGRESS_LIMIT_EXCEEDED: &str = "egress_limit_exceeded";
pub const EGRESS_FAILED: &str = "egress_failed";

pub const DEFAULT_MAX_REDIRECTS: u8 = 5;
/// Response body cap for one fetch.
pub const DEFAULT_MAX_RESPONSE_BYTES: u64 = 5 * 1024 * 1024;
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

/// Ordered from most to least restrictive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressMode {
    /// No network for Harness tools: fetch is refused and the shell runs in an
    /// empty network namespace.
    Offline,
    /// Public addresses only (the default).
    DenyPrivate,
}

impl Default for EgressMode {
    fn default() -> Self {
        Self::DenyPrivate
    }
}

impl EgressMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::DenyPrivate => "deny_private",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "offline" => Self::Offline,
            "deny_private" => Self::DenyPrivate,
            _ => return None,
        })
    }
}

/// The policy a network-capable tool consults through `ToolContext`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EgressPolicy {
    pub mode: EgressMode,
    pub max_redirects: u8,
    pub max_response_bytes: u64,
    pub request_timeout_secs: u64,
}

impl Default for EgressPolicy {
    fn default() -> Self {
        Self::for_mode(EgressMode::DenyPrivate)
    }
}

impl EgressPolicy {
    #[must_use]
    pub const fn for_mode(mode: EgressMode) -> Self {
        Self {
            mode,
            max_redirects: DEFAULT_MAX_REDIRECTS,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
        }
    }
}

/// Why an address is not reachable by a Harness network tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedClass {
    Loopback,
    /// `169.254.169.254` and the IPv6 metadata address `fd00:ec2::254`.
    CloudMetadata,
    LinkLocal,
    /// RFC 1918, carrier-grade NAT (RFC 6598) and unique-local IPv6.
    Private,
    Unspecified,
    Multicast,
    /// Everything else that is not global unicast: documentation, benchmarking,
    /// IETF protocol assignments, 6to4 relay, discard-only, Teredo, ORCHID,
    /// future use, broadcast and unallocated IPv6 space.
    Reserved,
}

impl BlockedClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Loopback => "loopback",
            Self::CloudMetadata => "cloud metadata",
            Self::LinkLocal => "link-local",
            Self::Private => "private range",
            Self::Unspecified => "unspecified",
            Self::Multicast => "multicast",
            Self::Reserved => "reserved",
        }
    }
}

const AWS_METADATA_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254);

/// `None` means the address is global unicast and publicly routable. The
/// policy is deny-by-default: an address is allowed only when it is not in any
/// special-purpose block of the IANA IPv4 and IPv6 special-purpose address
/// registries and, for IPv6, lies in the global-unicast block `2000::/3`.
/// Embedded IPv4 forms (`::ffff:a.b.c.d`, `::ffff:0:a.b.c.d`, `::a.b.c.d`,
/// NAT64, 6to4) classify by the IPv4 address they carry.
#[must_use]
pub fn blocked_class(ip: IpAddr) -> Option<BlockedClass> {
    match ip {
        IpAddr::V4(v4) => blocked_class_v4(v4),
        IpAddr::V6(v6) => blocked_class_v6(v6),
    }
}

/// One special-purpose IPv4 block: network address, prefix length, class.
type V4Block = ([u8; 4], u8, BlockedClass);

/// IANA IPv4 Special-Purpose Address Registry, plus the whole of 224.0.0.0/3
/// (multicast and reserved-for-future-use, which includes the limited
/// broadcast address). More specific entries precede broader ones.
/// Registry entries that are globally reachable (192.31.196.0/24 AS112,
/// 192.52.193.0/24 AMT, 192.175.48.0/24 AS112) are deliberately not listed.
const V4_BLOCKS: &[V4Block] = &[
    ([169, 254, 169, 254], 32, BlockedClass::CloudMetadata),
    ([0, 0, 0, 0], 8, BlockedClass::Unspecified), // "this network" (RFC 791)
    ([10, 0, 0, 0], 8, BlockedClass::Private),    // RFC 1918
    ([100, 64, 0, 0], 10, BlockedClass::Private), // shared CGNAT (RFC 6598)
    ([127, 0, 0, 0], 8, BlockedClass::Loopback),
    ([169, 254, 0, 0], 16, BlockedClass::LinkLocal),
    ([172, 16, 0, 0], 12, BlockedClass::Private), // RFC 1918
    ([192, 0, 0, 0], 24, BlockedClass::Reserved), // IETF protocol assignments
    ([192, 0, 2, 0], 24, BlockedClass::Reserved), // documentation (TEST-NET-1)
    ([192, 88, 99, 0], 24, BlockedClass::Reserved), // 6to4 relay anycast (deprecated)
    ([192, 168, 0, 0], 16, BlockedClass::Private), // RFC 1918
    ([198, 18, 0, 0], 15, BlockedClass::Reserved), // benchmarking
    ([198, 51, 100, 0], 24, BlockedClass::Reserved), // documentation (TEST-NET-2)
    ([203, 0, 113, 0], 24, BlockedClass::Reserved), // documentation (TEST-NET-3)
    ([224, 0, 0, 0], 4, BlockedClass::Multicast),
    ([240, 0, 0, 0], 4, BlockedClass::Reserved), // future use + limited broadcast
];

fn v4_in(ip: Ipv4Addr, net: [u8; 4], len: u8) -> bool {
    let mask = u32::MAX.checked_shl(32 - u32::from(len)).unwrap_or(0);
    u32::from(ip) & mask == u32::from(Ipv4Addr::from(net)) & mask
}

fn blocked_class_v4(ip: Ipv4Addr) -> Option<BlockedClass> {
    V4_BLOCKS
        .iter()
        .find(|(net, len, _)| v4_in(ip, *net, *len))
        .map(|(_, _, class)| *class)
}

/// One special-purpose IPv6 block: network address (as 16-bit groups), prefix
/// length, class.
type V6Block = ([u16; 8], u8, BlockedClass);

/// IANA IPv6 Special-Purpose Address Registry entries that live inside the
/// global-unicast block `2000::/3`, plus the non-global blocks that need a
/// typed class. Everything outside `2000::/3` and not listed here is denied as
/// `Reserved` by [`blocked_class_v6`].
const V6_BLOCKS: &[V6Block] = &[
    // 2001::/23 IETF protocol assignments: Teredo (2001::/32), benchmarking
    // (2001:2::/48), deprecated ORCHID (2001:10::/28), ORCHIDv2
    // (2001:20::/28), AS112, AMT and the anycast hosts. None is a general
    // public web destination.
    ([0x2001, 0, 0, 0, 0, 0, 0, 0], 23, BlockedClass::Reserved),
    // 2001:db8::/32 documentation.
    (
        [0x2001, 0x0db8, 0, 0, 0, 0, 0, 0],
        32,
        BlockedClass::Reserved,
    ),
    // 3fff::/20 documentation (RFC 9637).
    ([0x3fff, 0, 0, 0, 0, 0, 0, 0], 20, BlockedClass::Reserved),
    // fe80::/10 link-local unicast.
    ([0xfe80, 0, 0, 0, 0, 0, 0, 0], 10, BlockedClass::LinkLocal),
    // fec0::/10 deprecated site-local.
    ([0xfec0, 0, 0, 0, 0, 0, 0, 0], 10, BlockedClass::Private),
    // fc00::/7 unique-local.
    ([0xfc00, 0, 0, 0, 0, 0, 0, 0], 7, BlockedClass::Private),
    // ff00::/8 multicast.
    ([0xff00, 0, 0, 0, 0, 0, 0, 0], 8, BlockedClass::Multicast),
];

fn v6_in(ip: Ipv6Addr, net: [u16; 8], len: u8) -> bool {
    let mask = u128::MAX.checked_shl(128 - u32::from(len)).unwrap_or(0);
    u128::from(ip) & mask == u128::from(Ipv6Addr::from(net)) & mask
}

fn blocked_class_v6(ip: Ipv6Addr) -> Option<BlockedClass> {
    if ip == Ipv6Addr::UNSPECIFIED {
        return Some(BlockedClass::Unspecified);
    }
    if ip == Ipv6Addr::LOCALHOST {
        return Some(BlockedClass::Loopback);
    }
    if ip == AWS_METADATA_V6 {
        return Some(BlockedClass::CloudMetadata);
    }
    let seg = ip.segments();
    let embedded =
        |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    // ::ffff:0:0/96 IPv4-mapped: a real IPv4 peer on a dual-stack socket, so
    // the carried address decides.
    if seg[..5] == [0; 5] && seg[5] == 0xffff {
        return blocked_class_v4(embedded(seg[6], seg[7]));
    }
    // ::ffff:0:0:0/96 IPv4-translated (SIIT) and ::/96 IPv4-compatible
    // (deprecated): never a usable public destination. A carried non-global
    // address names its own class; a carried global one is still Reserved.
    if (seg[..4] == [0; 4] && seg[4] == 0xffff && seg[5] == 0) || seg[..6] == [0; 6] {
        return blocked_class_v4(embedded(seg[6], seg[7])).or(Some(BlockedClass::Reserved));
    }
    // 64:ff9b::/96 well-known NAT64: the carried address decides. The rest of
    // 64:ff9b::/32, including 64:ff9b:1::/48 local-use, is Reserved below.
    if seg[..6] == [0x0064, 0xff9b, 0, 0, 0, 0] {
        return blocked_class_v4(embedded(seg[6], seg[7]));
    }
    // 2002::/16 6to4 embeds the IPv4 address in bits 16..48. Not global per
    // the registry, so a carried global address is still Reserved.
    if seg[0] == 0x2002 {
        return blocked_class_v4(embedded(seg[1], seg[2])).or(Some(BlockedClass::Reserved));
    }
    if let Some((_, _, class)) = V6_BLOCKS.iter().find(|(net, len, _)| v6_in(ip, *net, *len)) {
        return Some(*class);
    }
    // Only 2000::/3 is allocated for global unicast; every other block
    // (::/8 remnants, 64:ff9b::/32, 100::/8 including the discard-only
    // 100::/64 and dummy 100:0:0:1::/64, 5f00::/16 SRv6 SIDs, ...) is denied.
    if seg[0] & 0xe000 != 0x2000 {
        return Some(BlockedClass::Reserved);
    }
    None
}

/// A refusal or failure of one fetch. The text always starts with a stable
/// code so a settled tool row carries a machine-readable prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressError {
    /// The destination (or a redirect target) is refused by policy.
    Denied(String),
    /// A response-size, redirect-count or timeout cap was hit.
    LimitExceeded(String),
    /// A transport failure that is not a policy decision.
    Failed(String),
}

impl EgressError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Denied(_) => EGRESS_DENIED,
            Self::LimitExceeded(_) => EGRESS_LIMIT_EXCEEDED,
            Self::Failed(_) => EGRESS_FAILED,
        }
    }

    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Denied(m) | Self::LimitExceeded(m) | Self::Failed(m) => m,
        }
    }
}

impl std::fmt::Display for EgressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for EgressError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(text: &str) -> Option<BlockedClass> {
        blocked_class(text.parse().unwrap())
    }

    #[test]
    fn every_blocked_class_is_named() {
        for (text, want) in [
            ("127.0.0.1", BlockedClass::Loopback),
            ("127.255.255.254", BlockedClass::Loopback),
            ("::1", BlockedClass::Loopback),
            ("169.254.169.254", BlockedClass::CloudMetadata),
            ("fd00:ec2::254", BlockedClass::CloudMetadata),
            ("169.254.1.1", BlockedClass::LinkLocal),
            ("fe80::1", BlockedClass::LinkLocal),
            ("10.1.2.3", BlockedClass::Private),
            ("172.16.0.1", BlockedClass::Private),
            ("172.31.255.255", BlockedClass::Private),
            ("192.168.1.1", BlockedClass::Private),
            ("100.64.0.1", BlockedClass::Private),
            ("100.100.100.200", BlockedClass::Private),
            ("fc00::1", BlockedClass::Private),
            ("fd12:3456::1", BlockedClass::Private),
            ("0.0.0.0", BlockedClass::Unspecified),
            ("::", BlockedClass::Unspecified),
            ("224.0.0.1", BlockedClass::Multicast),
            ("ff02::1", BlockedClass::Multicast),
            ("255.255.255.255", BlockedClass::Reserved),
            ("198.18.0.1", BlockedClass::Reserved),
            ("203.0.113.9", BlockedClass::Reserved),
            ("2001:db8::1", BlockedClass::Reserved),
        ] {
            assert_eq!(class(text), Some(want), "{text}");
        }
    }

    #[test]
    fn embedded_ipv4_forms_classify_by_the_carried_address() {
        for (text, want) in [
            ("::ffff:127.0.0.1", BlockedClass::Loopback),
            ("::ffff:169.254.169.254", BlockedClass::CloudMetadata),
            ("::ffff:10.0.0.1", BlockedClass::Private),
            ("64:ff9b::7f00:1", BlockedClass::Loopback),
            ("2002:7f00:1::1", BlockedClass::Loopback),
            ("2002:a9fe:a9fe::1", BlockedClass::CloudMetadata),
        ] {
            assert_eq!(class(text), Some(want), "{text}");
        }
    }

    #[test]
    fn public_addresses_are_reachable() {
        for text in [
            "93.184.216.34",
            "8.8.8.8",
            "1.1.1.1",
            "172.15.0.1",
            "172.32.0.1",
            "100.63.0.1",
            "100.128.0.1",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert_eq!(class(text), None, "{text}");
        }
    }

    /// `(first, last, class, global address just below, global address just
    /// above)`; a neighbour is `None` where it is itself another blocked range.
    /// Neighbours are global per the IANA special-purpose registries.
    #[allow(clippy::type_complexity)]
    const RANGES: &[(&str, &str, BlockedClass, Option<&str>, Option<&str>)] = &[
        (
            "0.0.0.0",
            "0.255.255.255",
            BlockedClass::Unspecified,
            None,
            Some("1.0.0.0"),
        ),
        (
            "10.0.0.0",
            "10.255.255.255",
            BlockedClass::Private,
            Some("9.255.255.255"),
            Some("11.0.0.0"),
        ),
        (
            "100.64.0.0",
            "100.127.255.255",
            BlockedClass::Private,
            Some("100.63.255.255"),
            Some("100.128.0.0"),
        ),
        (
            "127.0.0.0",
            "127.255.255.255",
            BlockedClass::Loopback,
            Some("126.255.255.255"),
            Some("128.0.0.0"),
        ),
        (
            "169.254.0.0",
            "169.254.255.255",
            BlockedClass::LinkLocal,
            Some("169.253.255.255"),
            Some("169.255.0.0"),
        ),
        (
            "172.16.0.0",
            "172.31.255.255",
            BlockedClass::Private,
            Some("172.15.255.255"),
            Some("172.32.0.0"),
        ),
        (
            "192.0.0.0",
            "192.0.0.255",
            BlockedClass::Reserved,
            Some("191.255.255.255"),
            Some("192.0.1.0"),
        ),
        (
            "192.0.2.0",
            "192.0.2.255",
            BlockedClass::Reserved,
            Some("192.0.1.255"),
            Some("192.0.3.0"),
        ),
        (
            "192.88.99.0",
            "192.88.99.255",
            BlockedClass::Reserved,
            Some("192.88.98.255"),
            Some("192.88.100.0"),
        ),
        (
            "192.168.0.0",
            "192.168.255.255",
            BlockedClass::Private,
            Some("192.167.255.255"),
            Some("192.169.0.0"),
        ),
        (
            "198.18.0.0",
            "198.19.255.255",
            BlockedClass::Reserved,
            Some("198.17.255.255"),
            Some("198.20.0.0"),
        ),
        (
            "198.51.100.0",
            "198.51.100.255",
            BlockedClass::Reserved,
            Some("198.51.99.255"),
            Some("198.51.101.0"),
        ),
        (
            "203.0.113.0",
            "203.0.113.255",
            BlockedClass::Reserved,
            Some("203.0.112.255"),
            Some("203.0.114.0"),
        ),
        (
            "224.0.0.0",
            "239.255.255.255",
            BlockedClass::Multicast,
            Some("223.255.255.255"),
            None,
        ),
        (
            "240.0.0.0",
            "255.255.255.255",
            BlockedClass::Reserved,
            None,
            None,
        ),
        // IPv6: the six ranges the review named, then the rest of the registry.
        (
            "100::",
            "100::ffff:ffff:ffff:ffff",
            BlockedClass::Reserved,
            None,
            None,
        ),
        (
            "100:0:0:1::",
            "100:0:0:1:ffff:ffff:ffff:ffff",
            BlockedClass::Reserved,
            None,
            None,
        ),
        (
            "2001:2::",
            "2001:2:0:ffff:ffff:ffff:ffff:ffff",
            BlockedClass::Reserved,
            None,
            None,
        ),
        (
            "2001:10::",
            "2001:1f:ffff:ffff:ffff:ffff:ffff:ffff",
            BlockedClass::Reserved,
            None,
            None,
        ),
        (
            "2001::",
            "2001:1ff:ffff:ffff:ffff:ffff:ffff:ffff",
            BlockedClass::Reserved,
            Some("2000::"),
            Some("2001:200::"),
        ),
        (
            "3fff::",
            "3fff:fff:ffff:ffff:ffff:ffff:ffff:ffff",
            BlockedClass::Reserved,
            Some("3ffe:ffff::"),
            Some("3fff:1000::"),
        ),
        (
            "5f00::",
            "5f00:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            BlockedClass::Reserved,
            None,
            None,
        ),
        (
            "2001:db8::",
            "2001:db8:ffff:ffff:ffff:ffff:ffff:ffff",
            BlockedClass::Reserved,
            Some("2001:db7:ffff::"),
            Some("2001:db9::"),
        ),
        (
            "64:ff9b:1::",
            "64:ff9b:1:ffff:ffff:ffff:ffff:ffff",
            BlockedClass::Reserved,
            None,
            None,
        ),
        (
            "fc00::",
            "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            BlockedClass::Private,
            None,
            None,
        ),
        (
            "fe80::",
            "febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            BlockedClass::LinkLocal,
            None,
            None,
        ),
        (
            "fec0::",
            "feff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            BlockedClass::Private,
            None,
            None,
        ),
        (
            "ff00::",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            BlockedClass::Multicast,
            None,
            None,
        ),
    ];

    #[test]
    fn every_special_purpose_range_is_denied_at_both_edges() {
        for (first, last, want, below, above) in RANGES {
            assert_eq!(class(first), Some(*want), "first {first}");
            assert_eq!(class(last), Some(*want), "last {last}");
            for (label, neighbour) in [("below", below), ("above", above)] {
                if let Some(n) = neighbour {
                    assert_eq!(class(n), None, "{label} neighbour {n} of {first}");
                }
            }
        }
    }

    #[test]
    fn addresses_outside_global_unicast_are_denied_by_default() {
        for text in [
            "::2",
            "1::",
            "100::1",
            "200::",
            "4000::",
            "5f00::1",
            "8000::",
            "a000::",
            "e000::1",
            "64:ff9b::1:0:0",
            "64:ff9b:2::1",
            "fe00::1",
        ] {
            assert!(class(text).is_some(), "{text}");
        }
    }

    #[test]
    fn ipv4_translated_and_compatible_forms_follow_the_carried_address() {
        for (text, want) in [
            ("::ffff:0:a00:1", BlockedClass::Private),
            ("::ffff:0:10.0.0.1", BlockedClass::Private),
            ("::ffff:0:7f00:1", BlockedClass::Loopback),
            ("::ffff:0:a9fe:a9fe", BlockedClass::CloudMetadata),
            // A carried global address is still not a usable destination.
            ("::ffff:0:808:808", BlockedClass::Reserved),
            ("::10.0.0.1", BlockedClass::Private),
            ("::8.8.8.8", BlockedClass::Reserved),
            ("2002:808:808::1", BlockedClass::Reserved),
            ("2002:a00:1::1", BlockedClass::Private),
        ] {
            assert_eq!(class(text), Some(want), "{text}");
        }
    }

    #[test]
    fn mode_round_trips_and_defaults_to_deny_private() {
        for mode in [EgressMode::Offline, EgressMode::DenyPrivate] {
            assert_eq!(EgressMode::parse(mode.as_str()), Some(mode));
        }
        assert!(EgressMode::Offline < EgressMode::DenyPrivate);
        assert_eq!(EgressPolicy::default().mode, EgressMode::DenyPrivate);
        assert_eq!(EgressMode::parse("allow"), None);
    }

    #[test]
    fn errors_carry_stable_codes() {
        assert_eq!(
            EgressError::Denied("x".into()).to_string(),
            "egress_denied: x"
        );
        assert_eq!(
            EgressError::LimitExceeded("y".into()).code(),
            EGRESS_LIMIT_EXCEEDED
        );
    }
}
