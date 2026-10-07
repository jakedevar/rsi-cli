//! Fractal manager hierarchy S5 (#1239, plan §2.4, §3, §5 M3): delegation at
//! every portfolio level.
//!
//! Any portfolio seat may stand up, replace or retire a child inside its own
//! grant: a project manager (PM) for a covered project, or a child portfolio
//! node over a strict subset of its coverage. A child never widens its parent
//! (`grant_narrows`), and a node never reaches a sibling or an ancestor. Only
//! the operator creates roots, adopts or re-parents: no request here carries
//! a parent or adopt field, and `deny_unknown_fields` refuses a forged one.
//!
//! `AgentManagerAppointChild` generalizes `AgentGlobalAppointManager` (kept as
//! an alias for a `project` target). `AgentManagerRevokeChild` revokes only a
//! child the caller's node granted, with its node-granted subtree; operator
//! granted children stay the operator's (operator decision, 2026-10-05).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::global_manager::{
    AgentGlobalAppointManagerRequestV1, GLOBAL_MANAGER_MAX_LAUNCHES, GLOBAL_MANAGER_MAX_PROJECTS,
    GLOBAL_MANAGER_MAX_QUERY_BYTES,
};
use crate::harness_manager_v2::{ManagerLaunchChoiceV2, ManagerPolicyV2};
use crate::portfolio_nodes::{
    MANAGER_CAPABILITY_WIDENED, PORTFOLIO_INVALID_REQUEST, PORTFOLIO_MAX_DIRECT_REPORTS,
    valid_tier_label,
};

/// Creating a child would exceed the grantor node's `max_direct_reports`
/// (its active child nodes plus the live PMs of the projects it covers
/// deepest).
pub const MANAGER_DIRECT_REPORT_CAP: &str = "manager_direct_report_cap";
/// The child was granted by the operator: only the operator revokes or
/// re-seats it.
pub const MANAGER_CHILD_OPERATOR_GRANTED: &str = "manager_child_operator_granted";
/// A new child's coverage equals the caller's: a child covers a strict
/// subset of its parent's projects.
pub const MANAGER_SCOPE_NOT_NARROWED: &str = "manager_scope_not_narrowed";
/// The named node is not a descendant the caller's node granted (a sibling,
/// an ancestor, another subtree, or a grandchild granted by another node).
pub const MANAGER_NODE_NOT_IN_SCOPE: &str = "manager_node_not_in_scope";

fn valid_key(key: &str) -> bool {
    !key.trim().is_empty() && key.len() <= 128
}

/// Who an appointment seats.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AppointChildTargetV1 {
    /// Appoint or replace the PM of a covered project. The project is saved
    /// with the caller node's `child_policy` (its own policy when unset).
    Project { project_id: Uuid },
    /// `node_id: null` creates a child portfolio node under the caller's
    /// node (grantor `node:<caller node>`); `tier_label`, `project_ids` (a
    /// strict subset of the caller's coverage) and `policy` are required.
    /// `node_id: <id>` replaces the seat of that child (one the caller's node
    /// granted) and takes no other field: the child's grant, epoch, ledger
    /// and workers stay; the predecessor seat loses authority in the same
    /// commit.
    Portfolio {
        #[serde(default)]
        node_id: Option<Uuid>,
        #[serde(default)]
        tier_label: Option<String>,
        #[serde(default)]
        project_ids: Vec<Uuid>,
        /// Defaults to the caller's effective launches.
        #[serde(default)]
        allowed_launches: Vec<ManagerLaunchChoiceV2>,
        #[serde(default)]
        policy: Option<ManagerPolicyV2>,
        #[serde(default)]
        child_policy: Option<ManagerPolicyV2>,
        /// Defaults to the smaller of 5 and the caller's limit.
        #[serde(default)]
        max_direct_reports: Option<u16>,
        /// The project the new seat runs in (defaults to the first of
        /// `project_ids`; for a replacement, the child's first project).
        #[serde(default)]
        launch_project_id: Option<Uuid>,
        /// Replacement only: the child's current grant version (CAS).
        #[serde(default)]
        expected_grant_version: Option<i64>,
    },
}

/// `AgentManagerAppointChild {target, launch, query, idempotency_key,
/// sandbox?}`: one idempotent call that launches a Standard root session and
/// appoints it to `target`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerAppointChildRequestV1 {
    pub target: AppointChildTargetV1,
    pub launch: ManagerLaunchChoiceV2,
    pub query: String,
    pub idempotency_key: String,
    /// Launch the seat in its own git worktree sandbox (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<bool>,
}

impl From<&AgentGlobalAppointManagerRequestV1> for AgentManagerAppointChildRequestV1 {
    fn from(request: &AgentGlobalAppointManagerRequestV1) -> Self {
        Self {
            target: AppointChildTargetV1::Project {
                project_id: request.project_id,
            },
            launch: request.launch.clone(),
            query: request.query.clone(),
            idempotency_key: request.idempotency_key.clone(),
            sandbox: request.sandbox,
        }
    }
}

fn unique(ids: &[Uuid]) -> bool {
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    sorted.len() == ids.len() && !sorted.iter().any(Uuid::is_nil)
}

impl AgentManagerAppointChildRequestV1 {
    /// # Errors
    /// A stable refusal code.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.query.trim().is_empty()
            || self.query.len() > GLOBAL_MANAGER_MAX_QUERY_BYTES
            || !valid_key(&self.idempotency_key)
            || self.launch.validate().is_err()
        {
            return Err(PORTFOLIO_INVALID_REQUEST);
        }
        match &self.target {
            AppointChildTargetV1::Project { project_id } => {
                if project_id.is_nil() {
                    return Err(PORTFOLIO_INVALID_REQUEST);
                }
            }
            AppointChildTargetV1::Portfolio {
                node_id: Some(node_id),
                tier_label,
                project_ids,
                allowed_launches,
                policy,
                child_policy,
                max_direct_reports,
                launch_project_id,
                expected_grant_version,
            } => {
                // A replacement moves the seat only.
                if node_id.is_nil()
                    || tier_label.is_some()
                    || !project_ids.is_empty()
                    || !allowed_launches.is_empty()
                    || policy.is_some()
                    || child_policy.is_some()
                    || max_direct_reports.is_some()
                    || launch_project_id.is_some_and(|id| id.is_nil())
                    || expected_grant_version.is_some_and(|version| version <= 0)
                {
                    return Err(PORTFOLIO_INVALID_REQUEST);
                }
            }
            AppointChildTargetV1::Portfolio {
                node_id: None,
                tier_label,
                project_ids,
                allowed_launches,
                policy,
                child_policy,
                max_direct_reports,
                launch_project_id,
                expected_grant_version,
            } => {
                let Some(policy) = policy else {
                    return Err(PORTFOLIO_INVALID_REQUEST);
                };
                if !tier_label.as_deref().is_some_and(valid_tier_label)
                    || project_ids.is_empty()
                    || project_ids.len() > GLOBAL_MANAGER_MAX_PROJECTS
                    || !unique(project_ids)
                    || allowed_launches.len() > GLOBAL_MANAGER_MAX_LAUNCHES
                    || allowed_launches
                        .iter()
                        .any(|launch| launch.validate().is_err())
                    || max_direct_reports.is_some_and(|limit| limit > PORTFOLIO_MAX_DIRECT_REPORTS)
                    || launch_project_id.is_some_and(|id| !project_ids.contains(&id))
                    || expected_grant_version.is_some()
                    || policy.validate().is_err()
                {
                    return Err(PORTFOLIO_INVALID_REQUEST);
                }
                if let Some(child) = child_policy {
                    child.validate().map_err(|_| PORTFOLIO_INVALID_REQUEST)?;
                    if child
                        .capabilities
                        .iter()
                        .any(|capability| !policy.capabilities.contains(capability))
                    {
                        return Err(MANAGER_CAPABILITY_WIDENED);
                    }
                }
            }
        }
        Ok(())
    }
}

/// `AgentManagerAppointChild` result. For a project target the versions are
/// the project's manager scope and policy versions; for a portfolio target
/// they are the child node's authority epoch and grant version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentManagerAppointChildResultV1 {
    pub appointment_id: Uuid,
    pub session_id: Uuid,
    /// `project:<id>` or `portfolio:<id>`.
    pub target_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<Uuid>,
    pub scope_version: i64,
    pub policy_version: i64,
    pub deduplicated: bool,
}

/// `AgentManagerRevokeChild {node_id, expected_grant_version,
/// idempotency_key}`: revoke a child portfolio node the caller's node
/// granted, with every node whose authority came from it. Operator-granted
/// descendants re-parent instead of dying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerRevokeChildRequestV1 {
    pub node_id: Uuid,
    pub expected_grant_version: i64,
    pub idempotency_key: String,
}

impl AgentManagerRevokeChildRequestV1 {
    /// # Errors
    /// A stable refusal code.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.node_id.is_nil()
            || self.expected_grant_version <= 0
            || !valid_key(&self.idempotency_key)
        {
            return Err(PORTFOLIO_INVALID_REQUEST);
        }
        Ok(())
    }
}

/// `AgentManagerRevokeChild` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentManagerRevokeChildResultV1 {
    pub node_id: Uuid,
    /// `revoked` after the call.
    pub state: String,
    pub grant_version: i64,
    /// Every node revoked by this call (the child first).
    pub revoked: Vec<Uuid>,
    /// Operator-granted descendants moved up one level.
    pub reparented: Vec<Uuid>,
    /// The child was already revoked at this grant version.
    pub deduplicated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness_manager_v2::ManagerCapabilityV2;
    use crate::types::SessionProvider;

    fn launch() -> ManagerLaunchChoiceV2 {
        ManagerLaunchChoiceV2 {
            provider: SessionProvider::Claude,
            model: "claude-opus-5-5".into(),
            effort: Some("high".into()),
        }
    }

    fn create(projects: Vec<Uuid>) -> AgentManagerAppointChildRequestV1 {
        AgentManagerAppointChildRequestV1 {
            target: AppointChildTargetV1::Portfolio {
                node_id: None,
                tier_label: Some("area-lead".into()),
                project_ids: projects,
                allowed_launches: Vec::new(),
                policy: Some(ManagerPolicyV2::default()),
                child_policy: None,
                max_direct_reports: None,
                launch_project_id: None,
                expected_grant_version: None,
            },
            launch: launch(),
            query: "You manage B and C.".into(),
            idempotency_key: "k".into(),
            sandbox: None,
        }
    }

    #[test]
    fn create_needs_label_projects_and_policy() {
        let b = Uuid::new_v4();
        assert_eq!(create(vec![b]).validate(), Ok(()));
        assert_eq!(create(vec![]).validate(), Err(PORTFOLIO_INVALID_REQUEST));
        assert_eq!(
            create(vec![b, b]).validate(),
            Err(PORTFOLIO_INVALID_REQUEST)
        );
        let mut no_policy = create(vec![b]);
        if let AppointChildTargetV1::Portfolio { policy, .. } = &mut no_policy.target {
            *policy = None;
        }
        assert_eq!(no_policy.validate(), Err(PORTFOLIO_INVALID_REQUEST));
        let mut widened = create(vec![b]);
        if let AppointChildTargetV1::Portfolio { child_policy, .. } = &mut widened.target {
            *child_policy = Some(ManagerPolicyV2 {
                capabilities: vec![ManagerCapabilityV2::IssueCoordinate],
                ..ManagerPolicyV2::default()
            });
        }
        assert_eq!(widened.validate(), Err(MANAGER_CAPABILITY_WIDENED));
        let mut outside_launch_project = create(vec![b]);
        if let AppointChildTargetV1::Portfolio {
            launch_project_id, ..
        } = &mut outside_launch_project.target
        {
            *launch_project_id = Some(Uuid::new_v4());
        }
        assert_eq!(
            outside_launch_project.validate(),
            Err(PORTFOLIO_INVALID_REQUEST)
        );
    }

    #[test]
    fn a_replacement_moves_the_seat_only() {
        let mut replace = create(Vec::new());
        replace.target = AppointChildTargetV1::Portfolio {
            node_id: Some(Uuid::new_v4()),
            tier_label: None,
            project_ids: Vec::new(),
            allowed_launches: Vec::new(),
            policy: None,
            child_policy: None,
            max_direct_reports: None,
            launch_project_id: None,
            expected_grant_version: Some(4),
        };
        assert_eq!(replace.validate(), Ok(()));
        if let AppointChildTargetV1::Portfolio { project_ids, .. } = &mut replace.target {
            project_ids.push(Uuid::new_v4());
        }
        assert_eq!(replace.validate(), Err(PORTFOLIO_INVALID_REQUEST));
    }

    #[test]
    fn forged_root_and_adopt_fields_fail_the_schema() {
        let mut value = serde_json::to_value(create(vec![Uuid::new_v4()])).unwrap();
        value["target"]["parent_node_id"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<AgentManagerAppointChildRequestV1>(value).is_err());
        let mut value = serde_json::to_value(create(vec![Uuid::new_v4()])).unwrap();
        value["target"]["adopt_node_ids"] = serde_json::json!([Uuid::new_v4()]);
        assert!(serde_json::from_value::<AgentManagerAppointChildRequestV1>(value).is_err());
        let mut value = serde_json::to_value(create(vec![Uuid::new_v4()])).unwrap();
        value["caller"] = serde_json::json!(Uuid::new_v4());
        assert!(serde_json::from_value::<AgentManagerAppointChildRequestV1>(value).is_err());
        let revoke = serde_json::json!({
            "node_id": Uuid::new_v4(), "expected_grant_version": 3,
            "idempotency_key": "r", "cascade": true
        });
        assert!(serde_json::from_value::<AgentManagerRevokeChildRequestV1>(revoke).is_err());
    }

    #[test]
    fn the_alias_maps_to_a_project_target() {
        let project_id = Uuid::new_v4();
        let alias = AgentGlobalAppointManagerRequestV1 {
            project_id,
            launch: launch(),
            query: "q".into(),
            idempotency_key: "k".into(),
            sandbox: Some(false),
        };
        let mapped = AgentManagerAppointChildRequestV1::from(&alias);
        assert_eq!(mapped.target, AppointChildTargetV1::Project { project_id });
        assert_eq!(mapped.sandbox, Some(false));
        assert_eq!(mapped.validate(), Ok(()));
    }
}
