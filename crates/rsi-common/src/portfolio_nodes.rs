//! Fractal manager hierarchy S2 (#1236): portfolio node identity.
//!
//! A manager above project level is a stable portfolio node: a row with an id,
//! a display-only `tier_label` ("global", "pinnacle", ...), an authority epoch
//! and a history of grants. Each node's coverage is an explicit project set;
//! sibling and root coverage is disjoint. Authority code never reads the
//! label. The operator creates, reconfigures and revokes nodes through the
//! operator-only RPCs below; they are not agent verbs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::global_manager::{
    GLOBAL_MANAGER_MAX_LAUNCHES, GLOBAL_MANAGER_MAX_PROJECTS, GlobalManagerGrantV1,
};
use crate::harness_manager_v2::{ManagerLaunchChoiceV2, ManagerPolicyV2};

/// The operator-only portfolio RPCs (never in an agent catalog).
pub const OPERATOR_METHODS: [&str; 5] = [
    "ListPortfolioNodes",
    "GetPortfolioNode",
    "ConfigurePortfolioNode",
    "ConfigurePortfolioNodeLaunches",
    "RevokePortfolioNode",
];

/// The tier label the `*GlobalManager` compatibility shims address.
pub const GLOBAL_TIER_LABEL: &str = "global";
/// Longest tier label.
pub const PORTFOLIO_TIER_LABEL_MAX: usize = 32;
/// Default and largest `max_direct_reports` of a portfolio grant.
pub const PORTFOLIO_DEFAULT_MAX_DIRECT_REPORTS: u16 = 5;
pub const PORTFOLIO_MAX_DIRECT_REPORTS: u16 = 64;

/// A project is already covered by another node at the same depth.
pub const MANAGER_SCOPE_OVERLAP: &str = "manager_scope_overlap";
/// An expected grant version or authority epoch is not the node's current one.
pub const MANAGER_NODE_STALE: &str = "manager_node_stale";
/// The seat session is missing, archived, a container, or holds another node.
pub const GLOBAL_MANAGER_SEAT_UNAVAILABLE: &str = "global_manager_seat_unavailable";
/// Only the operator creates a root node.
pub const MANAGER_NODE_ROOT_OPERATOR_ONLY: &str = "manager_node_root_operator_only";
/// The `*GlobalManager` shims found more than one active root labelled
/// `global`; address a node with the portfolio RPCs instead.
pub const GLOBAL_MANAGER_AMBIGUOUS: &str = "global_manager_ambiguous";
/// No portfolio node has this id.
pub const PORTFOLIO_NODE_NOT_FOUND: &str = "portfolio_node_not_found";
/// The request is malformed.
pub const PORTFOLIO_INVALID_REQUEST: &str = "portfolio_invalid_request";
/// A reused idempotency key carries a different request.
pub const PORTFOLIO_IDEMPOTENCY_CONFLICT: &str = "portfolio_idempotency_conflict";
/// A node's tier label is fixed at creation.
pub const PORTFOLIO_TIER_LABEL_IMMUTABLE: &str = "portfolio_tier_label_immutable";
/// `child_policy` grants a capability `policy` does not; also a child grant
/// holding a capability or launch its parent does not (#1237).
pub use crate::grant_narrowing::{
    MANAGER_ALLOWANCE_EXCEEDED, MANAGER_CAPABILITY_WIDENED, MANAGER_SCOPE_WIDENED,
};
/// #1237: an adopted node is not a current child of the adopting node's
/// parent (a root, when the adopting node is a root).
pub const PORTFOLIO_ADOPT_NOT_ROOT: &str = "portfolio_adopt_not_root";
/// #1237: the adopting node's projects omit part of an adopted node's
/// coverage.
pub const PORTFOLIO_COVERAGE_NOT_SUPERSET: &str = "portfolio_coverage_not_superset";
/// #1237: an edit names another parent; a node moves only by being adopted.
pub const PORTFOLIO_PARENT_IMMUTABLE: &str = "portfolio_parent_immutable";
/// #1237: the named parent is missing or revoked.
pub const MANAGER_ANCESTOR_REVOKED: &str = "manager_ancestor_revoked";
/// Most nodes one request adopts.
pub const PORTFOLIO_MAX_ADOPTED: usize = 64;

/// A configure request with the operator's explicit acknowledgment of lower
/// project resource caps. Older callers omit the flag and get a preview refusal.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PortfolioCapConfirmation<T> {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm_cap_reductions: bool,
    #[serde(flatten)]
    pub request: T,
}

// Deserialize the inner request on its own so its deny_unknown_fields contract
// survives the envelope. Serde's derived flatten deserializer drops extras.
impl<'de, T: serde::de::DeserializeOwned> Deserialize<'de> for PortfolioCapConfirmation<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = serde_json::Value::deserialize(deserializer)?;
        let confirm_cap_reductions = value
            .as_object_mut()
            .and_then(|object| object.remove("confirm_cap_reductions"))
            .map(serde_json::from_value)
            .transpose()
            .map_err(serde::de::Error::custom)?
            .unwrap_or(false);
        let request = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            confirm_cap_reductions,
            request,
        })
    }
}

/// A configure would lower a covered project's effective resource caps.
pub const PORTFOLIO_CAP_REDUCTION_CONFIRMATION_REQUIRED: &str =
    "portfolio_cap_reduction_confirmation_required";

/// Configured ceilings, not remaining capacity: each is the minimum across
/// the live project policy and its portfolio ancestor chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagerResourceCapsV1 {
    pub max_active_sessions: u16,
    pub max_created_sessions: u16,
    pub max_created_containers: u16,
    pub max_spend_usd: Option<f64>,
    pub provider_limits: Vec<crate::harness_manager_v2::ManagerProviderLimitV2>,
}

impl ManagerResourceCapsV1 {
    #[must_use]
    pub fn from_policy(policy: &ManagerPolicyV2) -> Self {
        Self {
            max_active_sessions: policy.max_active_sessions,
            max_created_sessions: policy.max_created_sessions,
            max_created_containers: policy.max_created_containers,
            max_spend_usd: policy.max_spend_usd,
            provider_limits: policy.provider_limits.clone(),
        }
    }

    pub fn intersect(&mut self, policy: &ManagerPolicyV2) {
        self.max_active_sessions = self.max_active_sessions.min(policy.max_active_sessions);
        self.max_created_sessions = self.max_created_sessions.min(policy.max_created_sessions);
        self.max_created_containers = self
            .max_created_containers
            .min(policy.max_created_containers);
        self.max_spend_usd = match (self.max_spend_usd, policy.max_spend_usd) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        for limit in &policy.provider_limits {
            if let Some(own) = self
                .provider_limits
                .iter_mut()
                .find(|own| own.provider == limit.provider)
            {
                own.max_active = own.max_active.min(limit.max_active);
            } else {
                self.provider_limits.push(limit.clone());
            }
        }
    }

    /// Human-readable changes requiring confirmation (only decreases).
    #[must_use]
    pub fn reductions_from(&self, before: &Self) -> Vec<String> {
        let mut changes = Vec::new();
        for (name, old, new) in [
            (
                "active sessions",
                before.max_active_sessions,
                self.max_active_sessions,
            ),
            (
                "created sessions",
                before.max_created_sessions,
                self.max_created_sessions,
            ),
            (
                "created containers",
                before.max_created_containers,
                self.max_created_containers,
            ),
        ] {
            if new < old {
                changes.push(format!("{name}: {old} → {new}"));
            }
        }
        if let Some(new) = self.max_spend_usd
            && before.max_spend_usd.is_none_or(|old| new < old)
        {
            changes.push(format!(
                "spend USD: {} → {new}",
                before
                    .max_spend_usd
                    .map_or_else(|| "uncapped".into(), |old| old.to_string())
            ));
        }
        for limit in &self.provider_limits {
            // A provider also pays the total-session ceiling. Adding a looser
            // provider limit does not lower the project's effective capacity.
            let old = before
                .provider_limits
                .iter()
                .find(|old| old.provider == limit.provider)
                .map_or(before.max_active_sessions, |old| {
                    old.max_active.min(before.max_active_sessions)
                });
            let new = limit.max_active.min(self.max_active_sessions);
            if new < old {
                changes.push(format!("{:?} active: {old} → {new}", limit.provider));
            }
        }
        changes
    }
}

fn valid_key(key: &str) -> bool {
    !key.trim().is_empty() && key.len() <= 128
}

/// A tier label is 1..=32 characters with no surrounding whitespace.
#[must_use]
pub fn valid_tier_label(label: &str) -> bool {
    !label.is_empty()
        && label.chars().count() <= PORTFOLIO_TIER_LABEL_MAX
        && label.trim() == label
        && !label.chars().any(char::is_control)
}

/// One portfolio node as the operator sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortfolioNodeV1 {
    pub node_id: Uuid,
    /// Display only ("global", "pinnacle", ...).
    pub tier_label: String,
    /// `active` or `revoked`.
    pub state: String,
    /// Bumps on operator (or grantor) edits, never on seat succession.
    pub authority_epoch: i64,
    /// `None` for a root.
    pub parent_node_id: Option<Uuid>,
    /// `operator` or `node:<uuid>`.
    pub grantor: String,
    /// The seat of the grant that opened the current epoch: the V2 ledger
    /// principal the node acts under in each covered project.
    pub seat_root_session_id: Uuid,
    pub max_direct_reports: u16,
    /// The policy the node hands to the managers it appoints (`None`: its own
    /// `grant.project_policy`).
    pub child_policy: Option<ManagerPolicyV2>,
    /// The active grant, or a revoked node's last grant. Its `project_ids`
    /// are the node's coverage.
    pub grant: GlobalManagerGrantV1,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One coverage row: `node_id` covers `project_id` at `depth` (0 is a root).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortfolioCoverageV1 {
    pub project_id: Uuid,
    pub depth: u16,
    pub node_id: Uuid,
    pub grant_version: i64,
}

/// Operator-only `ListPortfolioNodes {include_revoked?}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListPortfolioNodesRequestV1 {
    #[serde(default)]
    pub include_revoked: bool,
}

/// `ListPortfolioNodes` result, ordered by creation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ListPortfolioNodesResultV1 {
    pub nodes: Vec<PortfolioNodeV1>,
}

/// Operator-only `GetPortfolioNode {node_id}` (`null` when unknown).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetPortfolioNodeRequestV1 {
    pub node_id: Uuid,
}

const fn default_max_direct_reports() -> u16 {
    PORTFOLIO_DEFAULT_MAX_DIRECT_REPORTS
}

/// Operator-only: create a node (`node_id: null`) or write a new grant
/// version of an existing one (seat, projects, launches, policy). Every edit
/// is CAS-fenced on the node's current grant version and authority epoch
/// (both 0 when creating) and bumps the epoch.
///
/// #1237 (S3): `parent_node_id` names an existing active node to nest under
/// (the child must narrow it, `grant_narrows`); `adopt_node_ids` names
/// current children of that parent (roots, for a root) that move under this
/// node in the same transaction, keeping their grantor and epoch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigurePortfolioNodeRequestV1 {
    #[serde(default)]
    pub node_id: Option<Uuid>,
    /// `null` for a root. Fixed for an existing node (it moves only by being
    /// adopted).
    #[serde(default)]
    pub parent_node_id: Option<Uuid>,
    /// Current children of `parent_node_id` (roots, for a root) that become
    /// this node's children.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adopt_node_ids: Vec<Uuid>,
    /// When set, the parent's current grant version (CAS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_parent_grant_version: Option<i64>,
    pub tier_label: String,
    pub seat_session_id: Uuid,
    pub project_ids: Vec<Uuid>,
    pub allowed_launches: Vec<ManagerLaunchChoiceV2>,
    /// The node's own in-project policy (`GlobalManagerGrantV1.project_policy`).
    pub policy: ManagerPolicyV2,
    #[serde(default)]
    pub child_policy: Option<ManagerPolicyV2>,
    #[serde(default = "default_max_direct_reports")]
    pub max_direct_reports: u16,
    pub expected_node_grant_version: i64,
    pub expected_authority_epoch: i64,
    pub idempotency_key: String,
}

/// #2103: operator-only one-launch edit of a node's grant. Adds and removes
/// launches (provider, model, effort) and rewrites nothing else: the daemon
/// rebuilds the full configure request from the node's current grant, CAS-fenced
/// on `expected_node_grant_version`, so the operator never re-types the grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigurePortfolioNodeLaunchesRequestV1 {
    pub node_id: Uuid,
    #[serde(default)]
    pub add: Vec<ManagerLaunchChoiceV2>,
    #[serde(default)]
    pub remove: Vec<ManagerLaunchChoiceV2>,
    pub expected_node_grant_version: i64,
    pub idempotency_key: String,
}

impl ConfigurePortfolioNodeLaunchesRequestV1 {
    /// # Errors
    /// A stable refusal code.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.node_id.is_nil()
            || self.expected_node_grant_version <= 0
            || !valid_key(&self.idempotency_key)
            || (self.add.is_empty() && self.remove.is_empty())
            || self.add.len() > GLOBAL_MANAGER_MAX_LAUNCHES
            || self.remove.len() > GLOBAL_MANAGER_MAX_LAUNCHES
            || self
                .add
                .iter()
                .chain(&self.remove)
                .any(|launch| launch.validate().is_err())
            // A launch both added and removed is ambiguous.
            || self.add.iter().any(|launch| self.remove.contains(launch))
        {
            return Err(PORTFOLIO_INVALID_REQUEST);
        }
        Ok(())
    }

    /// The full configure request that applies this edit to `node`: every
    /// other field is the node's current value. `add` extends the grant's
    /// `allowed_launches` (and the policy's list when it is non-empty, since a
    /// non-empty policy list narrows the grant); `remove` drops the exact
    /// entries from the grant, the policy and the child policy.
    ///
    /// # Errors
    /// `portfolio_invalid_request` for a malformed edit or one that would leave
    /// a launch list empty (an empty policy list means "any", which widens);
    /// `manager_node_stale` when `expected_node_grant_version` is not the
    /// node's current grant version.
    pub fn apply_to(
        &self,
        node: &PortfolioNodeV1,
    ) -> Result<ConfigurePortfolioNodeRequestV1, &'static str> {
        self.validate()?;
        if node.state != "active" || node.grant.grant_version != self.expected_node_grant_version {
            return Err(MANAGER_NODE_STALE);
        }
        fn edit(
            list: &[ManagerLaunchChoiceV2],
            add: &[ManagerLaunchChoiceV2],
            remove: &[ManagerLaunchChoiceV2],
        ) -> Vec<ManagerLaunchChoiceV2> {
            let mut out: Vec<ManagerLaunchChoiceV2> = list
                .iter()
                .filter(|launch| !remove.contains(launch))
                .cloned()
                .collect();
            for launch in add {
                if !out.contains(launch) {
                    out.push(launch.clone());
                }
            }
            out
        }
        let mut policy = node.grant.project_policy.clone();
        if !policy.allowed_launches.is_empty() {
            let edited = edit(&policy.allowed_launches, &self.add, &self.remove);
            if edited.is_empty() {
                return Err(PORTFOLIO_INVALID_REQUEST);
            }
            policy.allowed_launches = edited;
        }
        let mut child_policy = node.child_policy.clone();
        if let Some(child) = child_policy.as_mut()
            && !child.allowed_launches.is_empty()
        {
            let edited = edit(&child.allowed_launches, &[], &self.remove);
            if edited.is_empty() {
                return Err(PORTFOLIO_INVALID_REQUEST);
            }
            child.allowed_launches = edited;
        }
        let allowed_launches = edit(&node.grant.allowed_launches, &self.add, &self.remove);
        if allowed_launches.is_empty() {
            return Err(PORTFOLIO_INVALID_REQUEST);
        }
        Ok(ConfigurePortfolioNodeRequestV1 {
            node_id: Some(node.node_id),
            parent_node_id: node.parent_node_id,
            adopt_node_ids: Vec::new(),
            expected_parent_grant_version: None,
            tier_label: node.tier_label.clone(),
            seat_session_id: node.grant.seat_session_id,
            project_ids: node.grant.project_ids.clone(),
            allowed_launches,
            policy,
            child_policy,
            max_direct_reports: node.max_direct_reports,
            expected_node_grant_version: node.grant.grant_version,
            expected_authority_epoch: node.authority_epoch,
            idempotency_key: self.idempotency_key.clone(),
        })
    }
}

/// #1902: what a new root node's first grant starts from when the operator
/// gives no policy: the most recent revoked root grant over the same
/// coverage, so replacing a revoked seat does not silently drop its
/// capabilities, caps and launches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortfolioGrantPrefillV1 {
    pub source_node_id: Uuid,
    pub source_tier_label: String,
    pub source_grant_version: i64,
    /// The source covers exactly the new coverage; otherwise it overlaps it.
    pub exact_coverage: bool,
    pub allowed_launches: Vec<ManagerLaunchChoiceV2>,
    pub policy: ManagerPolicyV2,
    pub child_policy: Option<ManagerPolicyV2>,
    pub max_direct_reports: u16,
}

/// The prefill for a new root covering `coverage`, from `nodes` (any mix of
/// active and revoked): the most recently updated revoked root whose coverage
/// equals `coverage`, else the most recent one that shares a project with it.
/// `None` (use defaults) when no revoked root overlaps: a grant over other
/// projects is not the operator's policy for these.
#[must_use]
pub fn prefill_from_revoked_root(
    nodes: &[PortfolioNodeV1],
    coverage: &[Uuid],
) -> Option<PortfolioGrantPrefillV1> {
    let same = |node: &PortfolioNodeV1| {
        let ids = &node.grant.project_ids;
        ids.len() == coverage.len() && coverage.iter().all(|id| ids.contains(id))
    };
    let overlaps = |node: &PortfolioNodeV1| {
        node.grant
            .project_ids
            .iter()
            .any(|id| coverage.contains(id))
    };
    let recency = |node: &&PortfolioNodeV1| (node.updated_at, node.grant.grant_version);
    let revoked_roots = || {
        nodes
            .iter()
            .filter(|node| node.state == "revoked" && node.parent_node_id.is_none())
    };
    let (source, exact_coverage) = match revoked_roots().filter(|n| same(n)).max_by_key(recency) {
        Some(node) => (node, true),
        None => (
            revoked_roots()
                .filter(|n| overlaps(n))
                .max_by_key(recency)?,
            false,
        ),
    };
    Some(PortfolioGrantPrefillV1 {
        source_node_id: source.node_id,
        source_tier_label: source.tier_label.clone(),
        source_grant_version: source.grant.grant_version,
        exact_coverage,
        allowed_launches: source.grant.allowed_launches.clone(),
        policy: source.grant.project_policy.clone(),
        child_policy: source.child_policy.clone(),
        max_direct_reports: source.max_direct_reports,
    })
}

impl ConfigurePortfolioNodeRequestV1 {
    /// # Errors
    /// A stable refusal code.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.seat_session_id.is_nil()
            || self.node_id.is_some_and(|id| id.is_nil())
            || self.parent_node_id.is_some_and(|id| id.is_nil())
            || self.expected_node_grant_version < 0
            || self.expected_authority_epoch < 0
            || (self.node_id.is_none()
                && (self.expected_node_grant_version != 0 || self.expected_authority_epoch != 0))
            || (self.node_id.is_some()
                && (self.expected_node_grant_version == 0 || self.expected_authority_epoch == 0))
            || !valid_tier_label(&self.tier_label)
            || !valid_key(&self.idempotency_key)
            || self.project_ids.is_empty()
            || self.project_ids.len() > GLOBAL_MANAGER_MAX_PROJECTS
            || self.allowed_launches.is_empty()
            || self.allowed_launches.len() > GLOBAL_MANAGER_MAX_LAUNCHES
            || self.max_direct_reports > PORTFOLIO_MAX_DIRECT_REPORTS
            || self.adopt_node_ids.len() > PORTFOLIO_MAX_ADOPTED
            || self
                .expected_parent_grant_version
                .is_some_and(|version| version <= 0)
            || (self.parent_node_id.is_none() && self.expected_parent_grant_version.is_some())
            || self.parent_node_id.is_some() && self.parent_node_id == self.node_id
        {
            return Err(PORTFOLIO_INVALID_REQUEST);
        }
        let mut adopted = self.adopt_node_ids.clone();
        adopted.sort_unstable();
        adopted.dedup();
        if adopted.len() != self.adopt_node_ids.len()
            || adopted.iter().any(|id| {
                id.is_nil() || Some(*id) == self.node_id || Some(*id) == self.parent_node_id
            })
        {
            return Err(PORTFOLIO_INVALID_REQUEST);
        }
        let mut projects = self.project_ids.clone();
        projects.sort_unstable();
        projects.dedup();
        if projects.len() != self.project_ids.len() || projects.iter().any(Uuid::is_nil) {
            return Err(PORTFOLIO_INVALID_REQUEST);
        }
        for launch in &self.allowed_launches {
            if launch.validate().is_err() {
                return Err(PORTFOLIO_INVALID_REQUEST);
            }
        }
        self.policy
            .validate()
            .map_err(|_| PORTFOLIO_INVALID_REQUEST)?;
        if let Some(child) = &self.child_policy {
            child.validate().map_err(|_| PORTFOLIO_INVALID_REQUEST)?;
            if child
                .capabilities
                .iter()
                .any(|capability| !self.policy.capabilities.contains(capability))
            {
                return Err(MANAGER_CAPABILITY_WIDENED);
            }
        }
        Ok(())
    }
}

/// Operator-only: revoke one node. Its grant is revoked, its coverage
/// released and its queued mail retired; nothing is deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokePortfolioNodeRequestV1 {
    pub node_id: Uuid,
    pub expected_grant_version: i64,
    pub expected_authority_epoch: i64,
    pub idempotency_key: String,
}

impl RevokePortfolioNodeRequestV1 {
    /// # Errors
    /// A stable refusal code.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.node_id.is_nil()
            || self.expected_grant_version <= 0
            || self.expected_authority_epoch <= 0
            || !valid_key(&self.idempotency_key)
        {
            return Err(PORTFOLIO_INVALID_REQUEST);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness_manager_v2::ManagerCapabilityV2;
    use crate::types::SessionProvider;

    fn request() -> ConfigurePortfolioNodeRequestV1 {
        ConfigurePortfolioNodeRequestV1 {
            node_id: None,
            parent_node_id: None,
            adopt_node_ids: Vec::new(),
            expected_parent_grant_version: None,
            tier_label: "global".into(),
            seat_session_id: Uuid::new_v4(),
            project_ids: vec![Uuid::new_v4()],
            allowed_launches: vec![ManagerLaunchChoiceV2 {
                provider: SessionProvider::Claude,
                model: "claude-opus-5-5".into(),
                effort: Some("high".into()),
            }],
            policy: ManagerPolicyV2::default(),
            child_policy: None,
            max_direct_reports: PORTFOLIO_DEFAULT_MAX_DIRECT_REPORTS,
            expected_node_grant_version: 0,
            expected_authority_epoch: 0,
            idempotency_key: "k".into(),
        }
    }

    fn node(
        projects: &[Uuid],
        state: &str,
        parent: Option<Uuid>,
        version: i64,
        age_minutes: i64,
        active_cap: u16,
    ) -> PortfolioNodeV1 {
        let at = chrono::DateTime::<Utc>::from_timestamp(1_800_000_000 + age_minutes * 60, 0)
            .expect("timestamp");
        let mut policy = ManagerPolicyV2::default();
        policy.max_active_sessions = active_cap;
        PortfolioNodeV1 {
            node_id: Uuid::new_v4(),
            tier_label: "global".into(),
            state: state.into(),
            authority_epoch: 1,
            parent_node_id: parent,
            grantor: "operator".into(),
            seat_root_session_id: Uuid::new_v4(),
            max_direct_reports: 9,
            child_policy: None,
            grant: GlobalManagerGrantV1 {
                grant_id: Uuid::new_v4(),
                grant_version: version,
                seat_session_id: Uuid::new_v4(),
                state: state.into(),
                project_ids: projects.to_vec(),
                allowed_launches: request().allowed_launches,
                project_policy: policy,
                operator_origin: "test".into(),
                created_at: at,
                updated_at: at,
            },
            created_at: at,
            updated_at: at,
        }
    }

    fn fable() -> ManagerLaunchChoiceV2 {
        ManagerLaunchChoiceV2 {
            provider: SessionProvider::Claude,
            model: "claude-fable-5-1".into(),
            effort: Some("max".into()),
        }
    }

    fn launches_edit(
        add: Vec<ManagerLaunchChoiceV2>,
        remove: Vec<ManagerLaunchChoiceV2>,
        version: i64,
    ) -> ConfigurePortfolioNodeLaunchesRequestV1 {
        ConfigurePortfolioNodeLaunchesRequestV1 {
            node_id: Uuid::new_v4(),
            add,
            remove,
            expected_node_grant_version: version,
            idempotency_key: "edit".into(),
        }
    }

    #[test]
    fn launch_edit_adds_one_launch_and_keeps_every_other_field() {
        let node = node(&[Uuid::new_v4()], "active", None, 13, 1, 7);
        let full = launches_edit(vec![fable()], Vec::new(), 13)
            .apply_to(&node)
            .expect("applies");
        let mut expected_launches = node.grant.allowed_launches.clone();
        expected_launches.push(fable());
        assert_eq!(full.allowed_launches, expected_launches);
        assert_eq!(full.node_id, Some(node.node_id));
        assert_eq!(full.seat_session_id, node.grant.seat_session_id);
        assert_eq!(full.project_ids, node.grant.project_ids);
        assert_eq!(full.policy, node.grant.project_policy);
        assert_eq!(full.max_direct_reports, node.max_direct_reports);
        assert_eq!(full.expected_node_grant_version, 13);
        assert_eq!(full.expected_authority_epoch, node.authority_epoch);
        assert_eq!(full.idempotency_key, "edit");
        full.validate().expect("the derived request is valid");
    }

    #[test]
    fn launch_edit_extends_a_restricting_policy_list_and_is_idempotent_on_repeat() {
        let mut node = node(&[Uuid::new_v4()], "active", None, 2, 1, 7);
        node.grant.project_policy.allowed_launches = node.grant.allowed_launches.clone();
        let once = launches_edit(vec![fable()], Vec::new(), 2)
            .apply_to(&node)
            .expect("applies");
        assert!(once.policy.allowed_launches.contains(&fable()));
        node.grant.allowed_launches = once.allowed_launches.clone();
        node.grant.project_policy = once.policy.clone();
        node.grant.grant_version = 3;
        let twice = launches_edit(vec![fable()], Vec::new(), 3)
            .apply_to(&node)
            .expect("applies");
        assert_eq!(twice.allowed_launches, once.allowed_launches);
        assert_eq!(twice.policy, once.policy);
    }

    #[test]
    fn launch_edit_removes_from_grant_policy_and_child_policy() {
        let mut node = node(&[Uuid::new_v4()], "active", None, 4, 1, 7);
        node.grant.allowed_launches.push(fable());
        node.grant.project_policy.allowed_launches = node.grant.allowed_launches.clone();
        let mut child = ManagerPolicyV2::default();
        child.allowed_launches = vec![fable(), node.grant.allowed_launches[0].clone()];
        node.child_policy = Some(child);
        let full = launches_edit(Vec::new(), vec![fable()], 4)
            .apply_to(&node)
            .expect("applies");
        assert!(!full.allowed_launches.contains(&fable()));
        assert!(!full.policy.allowed_launches.contains(&fable()));
        let child = full.child_policy.expect("child policy kept");
        assert_eq!(
            child.allowed_launches,
            vec![node.grant.allowed_launches[0].clone()]
        );
    }

    #[test]
    fn launch_edit_refuses_a_stale_version_an_empty_result_and_a_bad_edit() {
        let node = node(&[Uuid::new_v4()], "active", None, 5, 1, 7);
        assert_eq!(
            launches_edit(vec![fable()], Vec::new(), 4).apply_to(&node),
            Err(MANAGER_NODE_STALE)
        );
        let only = node.grant.allowed_launches.clone();
        assert_eq!(
            launches_edit(Vec::new(), only, 5).apply_to(&node),
            Err(PORTFOLIO_INVALID_REQUEST)
        );
        assert_eq!(
            launches_edit(Vec::new(), Vec::new(), 5).validate(),
            Err(PORTFOLIO_INVALID_REQUEST)
        );
        assert_eq!(
            launches_edit(vec![fable()], vec![fable()], 5).validate(),
            Err(PORTFOLIO_INVALID_REQUEST)
        );
        let revoked = self::node(&[Uuid::new_v4()], "revoked", None, 5, 1, 7);
        assert_eq!(
            launches_edit(vec![fable()], Vec::new(), 5).apply_to(&revoked),
            Err(MANAGER_NODE_STALE)
        );
    }

    #[test]
    fn prefill_takes_the_latest_revoked_root_over_the_same_coverage() {
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let nodes = [
            node(&[a, b], "revoked", None, 7, 1, 11),
            node(&[a, b], "revoked", None, 8, 5, 20),
            node(&[a, b], "active", None, 1, 9, 3),
            node(&[a, b], "revoked", Some(Uuid::new_v4()), 2, 9, 4),
            node(&[c], "revoked", None, 3, 9, 5),
        ];
        let prefill = prefill_from_revoked_root(&nodes, &[b, a]).expect("prefill");
        assert_eq!(prefill.source_grant_version, 8);
        assert_eq!(prefill.policy.max_active_sessions, 20);
        assert_eq!(prefill.max_direct_reports, 9);
        assert!(prefill.exact_coverage);
    }

    #[test]
    fn prefill_falls_back_to_overlap_and_then_to_defaults() {
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let nodes = [
            node(&[a, b], "revoked", None, 8, 5, 20),
            node(&[c], "revoked", None, 9, 9, 30),
        ];
        let overlap = prefill_from_revoked_root(&nodes, &[a]).expect("overlap");
        assert_eq!(
            (overlap.source_grant_version, overlap.exact_coverage),
            (8, false)
        );
        assert_eq!(prefill_from_revoked_root(&nodes, &[Uuid::new_v4()]), None);
        assert_eq!(prefill_from_revoked_root(&[], &[a]), None);
    }

    #[test]
    fn portfolio_grant_accepts_64_launches_and_refuses_65() {
        let mut request = request();
        request.allowed_launches = (0..64)
            .map(|i| ManagerLaunchChoiceV2 {
                provider: SessionProvider::Claude,
                model: format!("model-{i}"),
                effort: None,
            })
            .collect();
        assert_eq!(request.validate(), Ok(()));
        request.allowed_launches.push(ManagerLaunchChoiceV2 {
            provider: SessionProvider::Claude,
            model: "model-64".into(),
            effort: None,
        });
        assert_eq!(request.validate(), Err(PORTFOLIO_INVALID_REQUEST));
    }

    #[test]
    fn configure_validates_label_versions_and_child_policy() {
        assert_eq!(request().validate(), Ok(()));
        for label in ["", " global", &"x".repeat(33)] {
            let mut bad = request();
            bad.tier_label = label.into();
            assert_eq!(bad.validate(), Err(PORTFOLIO_INVALID_REQUEST), "{label:?}");
        }
        let mut create_with_version = request();
        create_with_version.expected_node_grant_version = 1;
        assert_eq!(
            create_with_version.validate(),
            Err(PORTFOLIO_INVALID_REQUEST)
        );
        let mut edit_without_version = request();
        edit_without_version.node_id = Some(Uuid::new_v4());
        assert_eq!(
            edit_without_version.validate(),
            Err(PORTFOLIO_INVALID_REQUEST)
        );
        let mut widened = request();
        widened.child_policy = Some(ManagerPolicyV2 {
            capabilities: vec![ManagerCapabilityV2::IssueCoordinate],
            ..ManagerPolicyV2::default()
        });
        assert_eq!(widened.validate(), Err(MANAGER_CAPABILITY_WIDENED));
    }

    #[test]
    fn configure_rejects_unknown_fields() {
        let mut value = serde_json::to_value(request()).unwrap();
        value["caller"] = serde_json::json!(Uuid::new_v4());
        assert!(serde_json::from_value::<ConfigurePortfolioNodeRequestV1>(value).is_err());
    }

    #[test]
    fn adoption_fields_default_and_validate() {
        let value = serde_json::to_value(request()).unwrap();
        assert!(value.get("adopt_node_ids").is_none());
        let parsed: ConfigurePortfolioNodeRequestV1 = serde_json::from_value(value).unwrap();
        assert!(parsed.adopt_node_ids.is_empty());
        let mut adopt = request();
        let child = Uuid::new_v4();
        adopt.adopt_node_ids = vec![child];
        assert_eq!(adopt.validate(), Ok(()));
        adopt.adopt_node_ids = vec![child, child];
        assert_eq!(adopt.validate(), Err(PORTFOLIO_INVALID_REQUEST));
        let mut parent_version_without_parent = request();
        parent_version_without_parent.expected_parent_grant_version = Some(3);
        assert_eq!(
            parent_version_without_parent.validate(),
            Err(PORTFOLIO_INVALID_REQUEST)
        );
        let mut self_adopt = request();
        let node = Uuid::new_v4();
        self_adopt.node_id = Some(node);
        self_adopt.expected_node_grant_version = 1;
        self_adopt.expected_authority_epoch = 1;
        self_adopt.adopt_node_ids = vec![node];
        assert_eq!(self_adopt.validate(), Err(PORTFOLIO_INVALID_REQUEST));
    }
    #[test]
    fn cap_confirmation_defaults_preserve_strict_request_decoding() {
        let mut value = serde_json::to_value(request()).unwrap();
        let parsed: PortfolioCapConfirmation<ConfigurePortfolioNodeRequestV1> =
            serde_json::from_value(value.clone()).unwrap();
        assert!(!parsed.confirm_cap_reductions);
        value["confirm_cap_reductions"] = serde_json::json!(true);
        let parsed: PortfolioCapConfirmation<ConfigurePortfolioNodeRequestV1> =
            serde_json::from_value(value.clone()).unwrap();
        assert!(parsed.confirm_cap_reductions);
        assert_eq!(serde_json::to_value(parsed).unwrap(), value);
        value["caller"] = serde_json::json!(Uuid::new_v4());
        assert!(
            serde_json::from_value::<PortfolioCapConfirmation<ConfigurePortfolioNodeRequestV1>>(
                value
            )
            .is_err()
        );
    }

    #[test]
    fn resource_caps_intersect_and_report_only_decreases() {
        use crate::harness_manager_v2::ManagerProviderLimitV2;
        use crate::types::SessionProvider;
        let own = ManagerPolicyV2 {
            max_active_sessions: 20,
            max_created_sessions: 100,
            max_created_containers: 10,
            provider_limits: vec![ManagerProviderLimitV2 {
                provider: SessionProvider::Claude,
                max_active: 15,
            }],
            ..ManagerPolicyV2::default()
        };
        let before = ManagerResourceCapsV1::from_policy(&own);
        let mut caps = before.clone();
        caps.intersect(&ManagerPolicyV2 {
            max_active_sessions: 4,
            max_created_sessions: 120,
            max_created_containers: 8,
            max_spend_usd: Some(25.0),
            provider_limits: vec![ManagerProviderLimitV2 {
                provider: SessionProvider::Claude,
                max_active: 3,
            }],
            ..ManagerPolicyV2::default()
        });
        assert_eq!(caps.max_active_sessions, 4);
        assert_eq!(caps.max_created_sessions, 100);
        assert_eq!(caps.max_created_containers, 8);
        assert_eq!(caps.max_spend_usd, Some(25.0));
        assert_eq!(caps.provider_limits[0].max_active, 3);
        assert_eq!(
            caps.reductions_from(&before),
            [
                "active sessions: 20 → 4",
                "created containers: 10 → 8",
                "spend USD: uncapped → 25",
                "Claude active: 15 → 3"
            ]
        );
        caps.intersect(&own);
        assert!(caps.reductions_from(&caps).is_empty());
        let mut unbounded_provider = before.clone();
        unbounded_provider.provider_limits.clear();
        let mut looser_provider = unbounded_provider.clone();
        looser_provider
            .provider_limits
            .push(ManagerProviderLimitV2 {
                provider: SessionProvider::Claude,
                max_active: 30,
            });
        assert!(
            looser_provider
                .reductions_from(&unbounded_provider)
                .is_empty()
        );
        looser_provider.provider_limits[0].max_active = 10;
        assert_eq!(
            looser_provider.reductions_from(&unbounded_provider),
            ["Claude active: 20 → 10"]
        );
    }
}
