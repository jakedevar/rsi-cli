//! Hub registry model; the definitions live in `store_support::satellite_registry`
//! (below `store`) and are re-exported here.

pub(crate) use crate::store_support::satellite_registry::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::path::PathBuf;
    use uuid::Uuid;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn second_link_with_wrong_identity_quarantines_both_paths() {
        let expected = Uuid::new_v4();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let mut continuity = PeerContinuity::new(Some(expected));
        assert_eq!(
            continuity.observe(first, expected, Uuid::new_v4()),
            ContinuityResult::Matched
        );
        assert_eq!(
            continuity.observe(second, Uuid::new_v4(), Uuid::new_v4()),
            ContinuityResult::Quarantined
        );
        assert_eq!(
            continuity.observe(first, expected, Uuid::new_v4()),
            ContinuityResult::Quarantined
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn same_installation_concurrent_clone_quarantines_but_later_restart_is_allowed() {
        let installation = Uuid::new_v4();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let mut continuity = PeerContinuity::new(Some(installation));
        assert_eq!(
            continuity.observe(first, installation, Uuid::new_v4()),
            ContinuityResult::Matched
        );
        continuity.begin_round();
        let restarted_incarnation = Uuid::new_v4();
        assert_eq!(
            continuity.observe(first, installation, restarted_incarnation),
            ContinuityResult::Matched
        );
        assert_eq!(
            continuity.observe(second, installation, Uuid::new_v4()),
            ContinuityResult::Quarantined
        );
        continuity.begin_round();
        assert_eq!(
            continuity.observe(first, installation, restarted_incarnation),
            ContinuityResult::Quarantined
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn unpaired_peer_stays_inert_until_operator_pins_identity() {
        let mut continuity = PeerContinuity::new(None);
        let link = Uuid::new_v4();
        assert_eq!(
            continuity.observe(link, Uuid::new_v4(), Uuid::new_v4()),
            ContinuityResult::PairingRequired
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn peer_and_link_bounds_reject_unsafe_metadata() {
        let root = Path::new("/tmp/satellites");
        let peer = RegistryPeer {
            id: Uuid::new_v4(),
            label: "work laptop".into(),
            expected_installation_id: None,
            enabled: false,
            read_enabled: false,
            launch_enabled: false,
            dispatch_enabled: false,
            links: vec![RegistryLink {
                id: Uuid::new_v4(),
                direction: LinkDirection::DialHomeReverse,
                socket_path: root.join("work.sock"),
                ssh_target: None,
                trust_reference: "ssh-config:work-laptop".into(),
                enabled: true,
                priority: 0,
            }],
        };
        assert!(peer.validate(root).is_ok());
        let mut unsafe_peer = peer.clone();
        unsafe_peer.links[0].socket_path = PathBuf::from("/tmp/other.sock");
        assert!(unsafe_peer.validate(root).is_err());
        unsafe_peer = peer.clone();
        unsafe_peer.label = "bad\nlabel".into();
        assert!(unsafe_peer.validate(root).is_err());
        unsafe_peer = peer.clone();
        unsafe_peer.enabled = true;
        unsafe_peer.read_enabled = true;
        assert!(unsafe_peer.validate(root).is_err());
        unsafe_peer = peer.clone();
        unsafe_peer.links[0].socket_path = root.join("..");
        assert!(unsafe_peer.validate(root).is_err());
        unsafe_peer = peer;
        unsafe_peer.links[0].direction = LinkDirection::DirectLocalForward;
        assert!(unsafe_peer.validate(root).is_err());
    }
}
