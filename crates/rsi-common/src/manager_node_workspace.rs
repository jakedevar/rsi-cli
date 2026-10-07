//! Fractal manager hierarchy S6 (#1240): one snapshot of any manager node.
//!
//! The operator and every manager see a node the same way, whatever its
//! tier: its grant and seat, its child nodes (each with seat status, model,
//! context fill and grant), the projects it covers (PM seat, Issue counts,
//! Running and WaitingApproval counts), its pending escalations and a fleet
//! rollup. There is no per-tier screen or query.
//!
//! - `GetManagerNodeWorkspace {node}` is operator-only (AGENTS.md rule 10):
//!   it lists the node's whole coverage. `GetGlobalManagerWorkspace` (#1213)
//!   is its shim for the single root labelled `global`.
//! - `AgentManagerOverview {}` is the caller's own node with the same shape,
//!   bounded for agents: it lists only the projects the node manages
//!   directly. A child portfolio node appears as one bounded digest (counts
//!   over its coverage), never with its projects or its inbox.
//!   `AgentGlobalOverview` stays as its alias with the v0 result shape.
//! - The fleet rollup is #1232's store aggregation filtered to the node's
//!   coverage; there is no second fleet query.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::fleet::{FleetGroup, FleetOverview, FleetUsage};
use crate::global_manager::{
    GlobalIssueCountsV1, GlobalManagerGrantV1, GlobalManagerWorkspaceV1, GlobalSeatSessionV1,
    GlobalWorkspaceProjectV1,
};
use crate::manager_nodes::{ManagerNodeGrantV1, ManagerNodeSelectorV1};
use crate::manager_tier_routing::ManagerNodeRefV1;

/// The operator-only node workspace RPC (never in an agent catalog).
pub const OPERATOR_METHODS: [&str; 1] = ["GetManagerNodeWorkspace"];

/// Most children one snapshot lists (a node may hold at most 64 direct
/// reports; a portfolio node's direct projects count separately).
pub const MANAGER_NODE_WORKSPACE_MAX_CHILDREN: usize = 128;
/// Most pending escalations one snapshot lists.
pub const MANAGER_NODE_WORKSPACE_MAX_ESCALATIONS: usize = 64;
/// Longest escalation reason one snapshot carries (bytes, on a char boundary).
pub const MANAGER_NODE_WORKSPACE_MAX_REASON_BYTES: usize = 512;

/// Operator-only `GetManagerNodeWorkspace {node}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetManagerNodeWorkspaceRequestV1 {
    pub node: ManagerNodeRefV1,
}

impl GetManagerNodeWorkspaceRequestV1 {
    /// # Errors
    /// `manager_tier_invalid_request` for a nil id.
    pub fn validate(&self) -> Result<(), &'static str> {
        let id = match self.node {
            ManagerNodeRefV1::Portfolio { node_id } | ManagerNodeRefV1::Area { node_id } => node_id,
            ManagerNodeRefV1::Project { project_id } => project_id,
        };
        if id.is_nil() {
            return Err(crate::manager_tier_routing::MANAGER_TIER_INVALID_REQUEST);
        }
        Ok(())
    }
}

/// `AgentManagerOverview {}`: the caller's own node.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerOverviewRequestV1 {}

/// Session-level counts summed over a node's coverage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerNodeCountsV1 {
    pub issues: GlobalIssueCountsV1,
    pub running_sessions: i64,
    pub waiting_approval_sessions: i64,
    pub pending_questions: i64,
    pub pending_approvals: i64,
}

/// An area node's grant as a workspace shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagerNodeAreaGrantV1 {
    pub project_id: Uuid,
    pub active: bool,
    pub grant_version: i64,
    pub authority_epoch: i64,
    pub selector: Option<ManagerNodeSelectorV1>,
    pub grant: Option<ManagerNodeGrantV1>,
}

/// One child of a node, as a bounded digest: its identity, grant, seat and
/// counts over its coverage. A child's own children, projects and inbox are
/// never included.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagerNodeChildV1 {
    pub node: ManagerNodeRefV1,
    /// A portfolio node's tier label, a project's name, or `area`.
    pub label: String,
    /// `active` or `revoked`; a project without a live PM is `vacant`.
    pub state: String,
    /// `operator` or `node:<uuid>` (portfolio children only).
    pub grantor: Option<String>,
    /// The child's grant version (a project's manager scope version), 0
    /// when it has none.
    pub grant_version: i64,
    /// The child's live seat (status, model, context fill).
    pub seat: Option<GlobalSeatSessionV1>,
    /// The child's coverage (a project child: its project; an area child:
    /// the project it lives in).
    pub project_ids: Vec<Uuid>,
    /// Counts summed over the child's coverage (`None` for an area child,
    /// whose coverage is part of one project).
    pub counts: Option<ManagerNodeCountsV1>,
    /// Escalations waiting on the child.
    pub pending_escalations: i64,
}

/// One escalation waiting on the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerNodePendingEscalationV1 {
    /// The in-project escalation (`manager_node_escalations.id`).
    pub escalation_id: Uuid,
    pub project_id: Uuid,
    pub subject_id: Uuid,
    /// Truncated to `MANAGER_NODE_WORKSPACE_MAX_REASON_BYTES`.
    pub reason: String,
    /// The hop above the project root that holds it, for a portfolio node.
    pub hop: Option<i64>,
    pub created_at: DateTime<Utc>,
}

/// #1232's fleet aggregation filtered to a node's coverage: active agents
/// and usage windows, without the per-agent rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagerNodeFleetV1 {
    pub as_of: DateTime<Utc>,
    /// Active (Starting, Running, WaitingApproval) agents in the coverage.
    pub active: u64,
    /// Usage over the `FLEET_WINDOWS` (5m, 1h, 24h).
    pub totals: [FleetUsage; 3],
    /// Project, provider and model groups of the covered rows.
    pub groups: Vec<FleetGroup>,
    /// The aggregation's row bounds were hit; counts may be partial.
    pub agents_truncated: bool,
    pub usage_truncated: bool,
}

impl ManagerNodeFleetV1 {
    /// No rows (a snapshot taken without the rollup).
    #[must_use]
    pub fn empty(as_of: DateTime<Utc>) -> Self {
        Self {
            as_of,
            active: 0,
            totals: Default::default(),
            groups: Vec::new(),
            agents_truncated: false,
            usage_truncated: false,
        }
    }

    /// The rollup of a (coverage-filtered) fleet snapshot.
    #[must_use]
    pub fn from_overview(overview: FleetOverview) -> Self {
        Self {
            as_of: overview.as_of,
            active: overview.agents.len() as u64,
            totals: overview.totals,
            groups: overview.groups,
            agents_truncated: overview.agents_truncated,
            usage_truncated: overview.usage_truncated,
        }
    }
}

/// `GetManagerNodeWorkspace` and `AgentManagerOverview` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagerNodeWorkspaceV1 {
    pub node: ManagerNodeRefV1,
    /// A portfolio node's tier label, a project's name, or `area`.
    pub label: String,
    /// `active` or `revoked`; a project without a live PM is `vacant`.
    pub state: String,
    /// `parent_of(node)`; `None` is the operator.
    pub parent: Option<ManagerNodeRefV1>,
    /// A portfolio node's active grant, else its last (revoked) one. Its
    /// `project_ids` are the node's coverage.
    pub grant: Option<GlobalManagerGrantV1>,
    /// `operator` or `node:<uuid>` (portfolio nodes).
    pub grantor: Option<String>,
    pub max_direct_reports: Option<u16>,
    /// An area node's grant.
    pub area: Option<ManagerNodeAreaGrantV1>,
    /// The node's seat session; `None` when vacant or gone.
    pub seat: Option<GlobalSeatSessionV1>,
    /// Child nodes: child portfolio nodes, then the projects this node
    /// manages directly, then child areas.
    pub children: Vec<ManagerNodeChildV1>,
    pub children_truncated: bool,
    /// Covered projects. The operator workspace lists the whole coverage;
    /// the agent overview lists only the projects the node manages directly
    /// (a child portfolio node's projects stay in its digest).
    pub projects: Vec<GlobalWorkspaceProjectV1>,
    /// Granted project ids whose project no longer exists.
    pub missing_project_ids: Vec<Uuid>,
    pub escalations: Vec<ManagerNodePendingEscalationV1>,
    pub escalations_truncated: bool,
    pub fleet: ManagerNodeFleetV1,
}

impl ManagerNodeWorkspaceV1 {
    /// The `GetGlobalManagerWorkspace` (#1213) projection of a portfolio
    /// node's operator workspace.
    #[must_use]
    pub fn into_global(self) -> GlobalManagerWorkspaceV1 {
        GlobalManagerWorkspaceV1 {
            grant: self.grant,
            seat: self.seat,
            projects: self.projects,
            missing_project_ids: self.missing_project_ids,
        }
    }
}

/// Cut `text` to at most `max` bytes on a char boundary.
#[must_use]
pub fn bounded_reason(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_request_names_one_node_and_refuses_extra_fields() {
        let id = Uuid::new_v4();
        let request: GetManagerNodeWorkspaceRequestV1 = serde_json::from_value(
            serde_json::json!({"node": {"kind": "portfolio", "node_id": id}}),
        )
        .unwrap();
        assert_eq!(request.node, ManagerNodeRefV1::Portfolio { node_id: id });
        assert_eq!(request.validate(), Ok(()));
        assert!(
            serde_json::from_value::<GetManagerNodeWorkspaceRequestV1>(serde_json::json!({
                "node": {"kind": "project", "project_id": id}, "extra": 1
            }))
            .is_err()
        );
        let nil = GetManagerNodeWorkspaceRequestV1 {
            node: ManagerNodeRefV1::Area {
                node_id: Uuid::nil(),
            },
        };
        assert!(nil.validate().is_err());
        assert!(
            serde_json::from_value::<AgentManagerOverviewRequestV1>(serde_json::json!({"x": 1}))
                .is_err()
        );
    }

    #[test]
    fn reason_is_cut_on_a_char_boundary() {
        assert_eq!(bounded_reason("abc", 8), "abc");
        assert_eq!(bounded_reason("ééé", 3), "é");
    }
}
