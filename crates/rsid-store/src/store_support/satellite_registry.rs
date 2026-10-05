//! Inert hub registry model. S3 supplies operator CRUD and starts connections.
//!
//! A peer is one installation with several routes. A route never becomes a
//! second peer, and a conflicting installation ID quarantines the whole peer.

use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use uuid::Uuid;

pub(crate) const MAX_LINKS_PER_PEER: usize = 4;
pub(crate) const MAX_PEER_LABEL_BYTES: usize = 128;
pub(crate) const MAX_TRUST_REFERENCE_BYTES: usize = 256;
pub(crate) const MAX_SSH_TARGET_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkDirection {
    DialHomeReverse,
    DirectLocalForward,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryLink {
    pub id: Uuid,
    pub direction: LinkDirection,
    pub socket_path: PathBuf,
    pub ssh_target: Option<String>,
    pub trust_reference: String,
    pub enabled: bool,
    pub priority: u8,
}

fn safe_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

impl RegistryLink {
    pub fn validate(&self, satellite_root: &std::path::Path) -> Result<(), &'static str> {
        if self.id.is_nil() {
            return Err("link ID is nil");
        }
        if !satellite_root.is_absolute()
            || self.socket_path.parent() != Some(satellite_root)
            || self.socket_path.file_name().is_none()
            || self.socket_path.as_os_str().as_bytes().len() > 107
            || self.socket_path.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        {
            return Err("link socket is outside satellite directory");
        }
        if !safe_text(&self.trust_reference, MAX_TRUST_REFERENCE_BYTES) {
            return Err("invalid directional trust reference");
        }
        if self.ssh_target.as_ref().is_some_and(|target| {
            !safe_text(target, MAX_SSH_TARGET_BYTES)
                || target.starts_with('-')
                || target.chars().any(char::is_whitespace)
        }) {
            return Err("invalid SSH target");
        }
        if self.direction == LinkDirection::DirectLocalForward && self.ssh_target.is_none() {
            return Err("direct link requires a configured SSH target");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct RegistryPeer {
    pub id: Uuid,
    pub label: String,
    /// Set only after operator review of transport trust and the first probe.
    pub expected_installation_id: Option<Uuid>,
    pub enabled: bool,
    pub read_enabled: bool,
    pub launch_enabled: bool,
    pub dispatch_enabled: bool,
    pub links: Vec<RegistryLink>,
}

impl RegistryPeer {
    pub fn validate(&self, satellite_root: &std::path::Path) -> Result<(), &'static str> {
        if self.id.is_nil() {
            return Err("peer ID is nil");
        }
        if !safe_text(&self.label, MAX_PEER_LABEL_BYTES) {
            return Err("invalid peer label");
        }
        if self.expected_installation_id.is_some_and(|id| id.is_nil()) {
            return Err("expected installation ID is nil");
        }
        if self.links.len() > MAX_LINKS_PER_PEER {
            return Err("too many peer links");
        }
        if self.read_enabled || self.launch_enabled || self.dispatch_enabled {
            if !self.enabled || self.expected_installation_id.is_none() {
                return Err("active policy requires an enabled paired peer");
            }
        }
        let mut ids = std::collections::HashSet::new();
        let mut paths = std::collections::HashSet::new();
        for link in &self.links {
            link.validate(satellite_root)?;
            if !ids.insert(link.id) || !paths.insert(&link.socket_path) {
                return Err("duplicate peer link");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuityResult {
    Matched,
    PairingRequired,
    Quarantined,
}

/// Bounded, process-local continuity check shared by all links of one peer.
/// A new daemon incarnation is a fresh observation; it never silently clears
/// an installation mismatch. Explicit operator repair must replace this state.
#[derive(Debug)]
pub struct PeerContinuity {
    expected_installation_id: Option<Uuid>,
    observed_links: HashMap<Uuid, (Uuid, Uuid)>,
    quarantined: bool,
}

impl PeerContinuity {
    pub fn new(expected_installation_id: Option<Uuid>) -> Self {
        Self {
            expected_installation_id,
            observed_links: HashMap::new(),
            quarantined: false,
        }
    }

    pub fn expected_installation_id(&self) -> Option<Uuid> {
        self.expected_installation_id
    }

    /// Compare links only within one poll round. A daemon may legitimately
    /// restart between rounds, while concurrent clones disagree within one.
    pub fn begin_round(&mut self) {
        self.observed_links.clear();
    }

    pub fn is_quarantined(&self) -> bool {
        self.quarantined
    }

    pub fn observe(
        &mut self,
        link_id: Uuid,
        installation_id: Uuid,
        incarnation_id: Uuid,
    ) -> ContinuityResult {
        if self.quarantined {
            return ContinuityResult::Quarantined;
        }
        if link_id.is_nil() || installation_id.is_nil() || incarnation_id.is_nil() {
            self.quarantined = true;
            return ContinuityResult::Quarantined;
        }
        if self
            .expected_installation_id
            .is_some_and(|expected| expected != installation_id)
            || self
                .observed_links
                .values()
                .any(|(observed_installation, observed_incarnation)| {
                    *observed_installation != installation_id
                        || *observed_incarnation != incarnation_id
                })
        {
            self.quarantined = true;
            self.observed_links.clear();
            return ContinuityResult::Quarantined;
        }
        if !self.observed_links.contains_key(&link_id)
            && self.observed_links.len() >= MAX_LINKS_PER_PEER
        {
            self.quarantined = true;
            self.observed_links.clear();
            return ContinuityResult::Quarantined;
        }
        self.observed_links
            .insert(link_id, (installation_id, incarnation_id));
        if self.expected_installation_id.is_none() {
            ContinuityResult::PairingRequired
        } else {
            ContinuityResult::Matched
        }
    }
}
