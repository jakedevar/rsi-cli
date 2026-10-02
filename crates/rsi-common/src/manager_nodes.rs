//! Recursive manager-node grants. A seat executes a node's grant; it is not
//! the grant's identity. The daemon resolves dynamic Group membership before
//! authorizing an effect.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::harness_manager::{HARNESS_MANAGER_MAX_EPICS, HARNESS_MANAGER_MAX_GROUPS};
use crate::harness_manager_v2::{
    ManagerCapabilityV2, ManagerLaunchChoiceV2, ManagerPolicyV2, ManagerProviderLimitV2,
};

pub const MANAGER_NODE_DEFAULT_MAX_DIRECT_REPORTS: u16 = 5;

/// A project selector is explicit: an empty selected set never means an
/// entire project. Group membership is resolved against the live topology.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagerNodeSelectorV1 {
    Project,
    Selected {
        group_ids: Vec<Uuid>,
        epic_ids: Vec<Uuid>,
    },
}

impl ManagerNodeSelectorV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Project => Ok(()),
            Self::Selected {
                group_ids,
                epic_ids,
            } => {
                if group_ids.len() > HARNESS_MANAGER_MAX_GROUPS
                    || epic_ids.len() > HARNESS_MANAGER_MAX_EPICS
                    || (group_ids.is_empty() && epic_ids.is_empty())
                    || !unique_non_nil(group_ids)
                    || !unique_non_nil(epic_ids)
                {
                    return Err("manager_node_invalid_selector");
                }
                Ok(())
            }
        }
    }

    /// Resolve against the current project topology on every authority check.
    /// The caller must first verify the Epic belongs to the selected project.
    pub fn covers_epic(&self, epic_id: Uuid, group_id: Option<Uuid>) -> bool {
        match self {
            Self::Project => true,
            Self::Selected {
                group_ids,
                epic_ids,
            } => {
                epic_ids.contains(&epic_id)
                    || group_id.is_some_and(|group| group_ids.contains(&group))
            }
        }
    }

    /// The project topology is a live input, never a grant-time snapshot.
    /// Recheck this before admitting or moving an Epic into a selected Group.
    pub fn overlaps_on_live_epics(
        &self,
        other: &Self,
        live_epics: &[(Uuid, Option<Uuid>)],
    ) -> bool {
        match (self, other) {
            (Self::Project, _) | (_, Self::Project) => return true,
            (
                Self::Selected {
                    group_ids: left_groups,
                    epic_ids: left_epics,
                },
                Self::Selected {
                    group_ids: right_groups,
                    epic_ids: right_epics,
                },
            ) if left_groups.iter().any(|group| right_groups.contains(group))
                || left_epics.iter().any(|epic| right_epics.contains(epic)) =>
            {
                return true;
            }
            _ => {}
        }
        live_epics.iter().any(|(epic, group)| {
            self.covers_epic(*epic, *group) && other.covers_epic(*epic, *group)
        })
    }
}

fn unique_non_nil(ids: &[Uuid]) -> bool {
    ids.iter()
        .enumerate()
        .all(|(index, id)| !id.is_nil() && !ids[..index].contains(id))
}

/// Operator policy has three distinct states. Merely omitting a row never
/// promotes a manager to the granted state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerNodeGrantStateV1 {
    Absent,
    Granted,
    Revoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerNodeStateV1 {
    Active,
    Revoked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListManagerNodesRequestV1 {
    pub project_id: Uuid,
    #[serde(default)]
    pub after_node_id: Option<Uuid>,
    pub limit: u16,
}

impl ListManagerNodesRequestV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.project_id.is_nil()
            || self.after_node_id.is_some_and(|id| id.is_nil())
            || !(1..=64).contains(&self.limit)
        {
            return Err("manager_node_invalid_page");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetManagerNodeRequestV1 {
    pub project_id: Uuid,
    pub node_id: Uuid,
}

impl GetManagerNodeRequestV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.project_id.is_nil() || self.node_id.is_nil() {
            return Err("manager_node_invalid_lookup");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigureManagerNodeRequestV1 {
    /// None creates a node; Some replaces its grant with a new version.
    pub node_id: Option<Uuid>,
    pub parent_node_id: Uuid,
    pub project_id: Uuid,
    pub seat_root_session_id: Uuid,
    pub selector: ManagerNodeSelectorV1,
    pub grant: ManagerNodeGrantV1,
    pub policy: ManagerPolicyV2,
    pub expected_parent_grant_version: i64,
    pub expected_parent_policy_version: i64,
    pub expected_parent_authority_epoch: i64,
    pub expected_node_grant_version: i64,
    pub idempotency_key: String,
}

/// Manager delegation names the parent node as a routing target. The daemon
/// derives its project and authenticates the parent's current seat.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegateManagerNodeRequestV1 {
    pub node_id: Option<Uuid>,
    pub parent_node_id: Uuid,
    pub seat_root_session_id: Uuid,
    pub selector: ManagerNodeSelectorV1,
    pub grant: ManagerNodeGrantV1,
    pub policy: ManagerPolicyV2,
    pub expected_parent_grant_version: i64,
    pub expected_parent_policy_version: i64,
    pub expected_parent_authority_epoch: i64,
    pub expected_node_grant_version: i64,
    pub idempotency_key: String,
}

impl DelegateManagerNodeRequestV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.parent_node_id.is_nil() {
            return Err("manager_node_invalid_delegation");
        }
        self.clone()
            .into_configuration(Uuid::from_u128(1))
            .validate()
    }

    pub fn into_configuration(self, project_id: Uuid) -> ConfigureManagerNodeRequestV1 {
        ConfigureManagerNodeRequestV1 {
            node_id: self.node_id,
            parent_node_id: self.parent_node_id,
            project_id,
            seat_root_session_id: self.seat_root_session_id,
            selector: self.selector,
            grant: self.grant,
            policy: self.policy,
            expected_parent_grant_version: self.expected_parent_grant_version,
            expected_parent_policy_version: self.expected_parent_policy_version,
            expected_parent_authority_epoch: self.expected_parent_authority_epoch,
            expected_node_grant_version: self.expected_node_grant_version,
            idempotency_key: self.idempotency_key,
        }
    }
}

impl ConfigureManagerNodeRequestV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.selector.validate()?;
        self.grant.validate()?;
        self.policy.validate()?;
        let root_edit = self.parent_node_id.is_nil();
        if (root_edit
            && (self.node_id.is_none()
                || self.expected_parent_grant_version != 0
                || self.expected_parent_policy_version != 0
                || self.expected_parent_authority_epoch != 0))
            || (!root_edit
                && (self.expected_parent_grant_version <= 0
                    || self.expected_parent_policy_version < 0
                    || self.expected_parent_authority_epoch <= 0))
            || self.project_id.is_nil()
            || self.seat_root_session_id.is_nil()
            || self
                .node_id
                .is_some_and(|id| id.is_nil() || (!root_edit && id == self.parent_node_id))
            || self.expected_node_grant_version < 0
            || self.node_id.is_none() != (self.expected_node_grant_version == 0)
            || !(1..=128).contains(&self.idempotency_key.len())
            || self.grant.capabilities.len() != self.policy.capabilities.len()
            || !self
                .grant
                .capabilities
                .iter()
                .all(|capability| self.policy.capabilities.contains(capability))
            || self.grant.allowance.max_created_containers != self.policy.max_created_containers
            || self.grant.allowance.max_created_sessions != self.policy.max_created_sessions
            || self.grant.allowance.max_active_sessions != self.policy.max_active_sessions
            || self.grant.allowance.max_spend_usd != self.policy.max_spend_usd
            || self.grant.allowance.provider_limits.len() != self.policy.provider_limits.len()
            || !self
                .grant
                .allowance
                .provider_limits
                .iter()
                .all(|limit| self.policy.provider_limits.contains(limit))
            || self.grant.allowed_launches.len() != self.policy.allowed_launches.len()
            || !self
                .grant
                .allowed_launches
                .iter()
                .all(|choice| self.policy.allowed_launches.contains(choice))
        {
            return Err("manager_node_invalid_configuration");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeManagerNodeRequestV1 {
    pub project_id: Uuid,
    pub node_id: Uuid,
    pub expected_grant_version: i64,
    pub expected_authority_epoch: i64,
    pub idempotency_key: String,
}

impl RevokeManagerNodeRequestV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.project_id.is_nil()
            || self.node_id.is_nil()
            || self.expected_grant_version <= 0
            || self.expected_authority_epoch <= 0
            || !(1..=128).contains(&self.idempotency_key.len())
        {
            return Err("manager_node_invalid_revocation");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagerNodeViewV1 {
    pub node_id: Uuid,
    pub parent_node_id: Option<Uuid>,
    pub seat_root_session_id: Uuid,
    pub project_id: Uuid,
    pub selector: Option<ManagerNodeSelectorV1>,
    pub state: ManagerNodeStateV1,
    pub grant_state: ManagerNodeGrantStateV1,
    pub grant: Option<ManagerNodeGrantV1>,
    pub policy: Option<ManagerPolicyV2>,
    pub grant_version: i64,
    pub policy_version: i64,
    pub authority_epoch: i64,
    pub direct_reports: u16,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListManagerNodesResultV1 {
    pub rows: Vec<ManagerNodeViewV1>,
    pub next_after_node_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerNodeAllowanceV1 {
    pub max_created_containers: u16,
    pub max_created_sessions: u16,
    pub max_active_sessions: u16,
    pub max_build_slots: u16,
    pub max_disk_gib: u32,
    pub provider_limits: Vec<ManagerProviderLimitV2>,
    /// None is an uncapped operator choice, not an implicit spend grant.
    pub max_spend_usd: Option<f64>,
}

impl ManagerNodeAllowanceV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.max_created_containers > 64
            || self.max_created_sessions > 1024
            || self.max_active_sessions == 0
            || self.max_active_sessions > 100
            || self.max_build_slots > 64
            || self.max_disk_gib > 1_000_000
            || self.provider_limits.len() > 8
            || self
                .provider_limits
                .iter()
                .enumerate()
                .any(|(index, limit)| {
                    !(1..=64).contains(&limit.max_active)
                        || self.provider_limits[..index]
                            .iter()
                            .any(|prior| prior.provider == limit.provider)
                })
            || self
                .max_spend_usd
                .is_some_and(|value| !value.is_finite() || value <= 0.0)
        {
            return Err("manager_node_invalid_allowance");
        }
        Ok(())
    }

    /// Every available finite dimension is carved down. A zero dimension
    /// carries no capacity to delegate. Optional spend may remain uncapped
    /// when the operator chose no spend cap (D4).
    pub fn strictly_narrower_than(&self, parent: &Self) -> bool {
        self.validate().is_ok()
            && parent.validate().is_ok()
            && narrower_u16(self.max_created_containers, parent.max_created_containers)
            && narrower_u16(self.max_created_sessions, parent.max_created_sessions)
            && narrower_u16(self.max_active_sessions, parent.max_active_sessions)
            && narrower_u16(self.max_build_slots, parent.max_build_slots)
            && narrower_u32(self.max_disk_gib, parent.max_disk_gib)
            && parent.provider_limits.iter().all(|parent_limit| {
                self.provider_limits.iter().any(|child_limit| {
                    child_limit.provider == parent_limit.provider
                        && child_limit.max_active < parent_limit.max_active
                })
            })
            && match (self.max_spend_usd, parent.max_spend_usd) {
                (Some(child), Some(parent)) => child < parent,
                (Some(_), None) | (None, None) => true,
                (None, Some(_)) => false,
            }
    }
}

fn narrower_u16(child: u16, parent: u16) -> bool {
    (parent == 0 && child == 0) || (parent > 0 && child < parent)
}

fn narrower_u32(child: u32, parent: u32) -> bool {
    (parent == 0 && child == 0) || (parent > 0 && child < parent)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerNodeGrantV1 {
    pub capabilities: Vec<ManagerCapabilityV2>,
    pub allowed_launches: Vec<ManagerLaunchChoiceV2>,
    pub allowance: ManagerNodeAllowanceV1,
    pub max_direct_reports: u16,
}

impl ManagerNodeGrantV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.allowance.validate()?;
        if self.capabilities.len() > 16
            || self.allowed_launches.len() > 32
            || self
                .allowed_launches
                .iter()
                .any(|choice| choice.validate().is_err())
            || self
                .allowed_launches
                .iter()
                .enumerate()
                .any(|(index, choice)| self.allowed_launches[..index].contains(choice))
            || self
                .capabilities
                .iter()
                .enumerate()
                .any(|(index, capability)| self.capabilities[..index].contains(capability))
            || !(1..=64).contains(&self.max_direct_reports)
        {
            return Err("manager_node_invalid_grant");
        }
        Ok(())
    }

    pub fn strictly_narrower_than(&self, parent: &Self) -> bool {
        self.validate().is_ok()
            && parent.validate().is_ok()
            && self.capabilities.len() < parent.capabilities.len()
            && self
                .capabilities
                .iter()
                .all(|capability| parent.capabilities.contains(capability))
            && self
                .allowed_launches
                .iter()
                .all(|choice| parent.allowed_launches.contains(choice))
            && self.allowance.strictly_narrower_than(&parent.allowance)
            && self.max_direct_reports < parent.max_direct_reports
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SessionProvider;

    fn grant() -> ManagerNodeGrantV1 {
        ManagerNodeGrantV1 {
            capabilities: vec![
                ManagerCapabilityV2::WorkPlan,
                ManagerCapabilityV2::LeadControl,
            ],
            allowed_launches: vec![],
            allowance: ManagerNodeAllowanceV1 {
                max_created_containers: 8,
                max_created_sessions: 16,
                max_active_sessions: 8,
                max_build_slots: 3,
                max_disk_gib: 90,
                provider_limits: vec![],
                max_spend_usd: None,
            },
            max_direct_reports: MANAGER_NODE_DEFAULT_MAX_DIRECT_REPORTS,
        }
    }

    #[test]
    fn selected_scope_is_distinct_from_project_scope() {
        let project: ManagerNodeSelectorV1 = serde_json::from_str(r#"{"mode":"project"}"#).unwrap();
        assert_eq!(project, ManagerNodeSelectorV1::Project);
        assert!(project.validate().is_ok());

        let selected: ManagerNodeSelectorV1 = serde_json::from_str(&format!(
            r#"{{"mode":"selected","group_ids":[],"epic_ids":["{}"]}}"#,
            Uuid::new_v4()
        ))
        .unwrap();
        assert!(selected.validate().is_ok());
        assert!(
            ManagerNodeSelectorV1::Selected {
                group_ids: vec![],
                epic_ids: vec![],
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn live_group_membership_can_create_a_sibling_overlap() {
        let group = Uuid::new_v4();
        let epic = Uuid::new_v4();
        let selected_group = ManagerNodeSelectorV1::Selected {
            group_ids: vec![group],
            epic_ids: vec![],
        };
        let selected_epic = ManagerNodeSelectorV1::Selected {
            group_ids: vec![],
            epic_ids: vec![epic],
        };
        assert!(!selected_group.overlaps_on_live_epics(&selected_epic, &[(epic, None)]));
        assert!(selected_group.overlaps_on_live_epics(&selected_epic, &[(epic, Some(group))]));
        assert!(selected_group.overlaps_on_live_epics(&selected_group, &[]));
        assert!(ManagerNodeSelectorV1::Project.overlaps_on_live_epics(&selected_group, &[]));
    }

    #[test]
    fn child_grant_carves_each_finite_capacity_and_capabilities() {
        let parent = grant();
        let child = ManagerNodeGrantV1 {
            capabilities: vec![ManagerCapabilityV2::LeadControl],
            allowed_launches: vec![],
            allowance: ManagerNodeAllowanceV1 {
                max_created_containers: 2,
                max_created_sessions: 4,
                max_active_sessions: 2,
                max_build_slots: 1,
                max_disk_gib: 20,
                provider_limits: vec![],
                max_spend_usd: None,
            },
            max_direct_reports: 3,
        };
        assert!(child.strictly_narrower_than(&parent));
        assert!(!parent.strictly_narrower_than(&child));
    }

    #[test]
    fn child_cannot_widen_provider_ceiling_or_launch_choices() {
        let mut parent = grant();
        parent
            .allowance
            .provider_limits
            .push(ManagerProviderLimitV2 {
                provider: SessionProvider::Codex,
                max_active: 4,
            });
        let mut child = grant();
        child.capabilities.pop();
        child.allowance.max_created_containers = 4;
        child.allowance.max_created_sessions = 8;
        child.allowance.max_active_sessions = 4;
        child.allowance.max_build_slots = 1;
        child.allowance.max_disk_gib = 40;
        child.max_direct_reports = 3;
        child
            .allowance
            .provider_limits
            .push(ManagerProviderLimitV2 {
                provider: SessionProvider::Codex,
                max_active: 3,
            });
        assert!(child.strictly_narrower_than(&parent));
        child.allowance.provider_limits[0].max_active = 4;
        assert!(!child.strictly_narrower_than(&parent));
        child.allowance.provider_limits[0].max_active = 3;
        child.allowed_launches.push(ManagerLaunchChoiceV2 {
            provider: SessionProvider::Codex,
            model: "gpt-6-sol".into(),
            effort: Some("high".into()),
        });
        assert!(!child.strictly_narrower_than(&parent));
    }

    #[test]
    fn absent_grant_has_distinct_wire_state() {
        let state: ManagerNodeGrantStateV1 = serde_json::from_str("\"absent\"").unwrap();
        assert_eq!(state, ManagerNodeGrantStateV1::Absent);
        assert_eq!(
            serde_json::to_string(&ManagerNodeGrantStateV1::Revoked).unwrap(),
            "\"revoked\""
        );
    }

    #[test]
    fn operator_configuration_requires_matching_policy_and_fences() {
        let mut request = ConfigureManagerNodeRequestV1 {
            node_id: None,
            parent_node_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            seat_root_session_id: Uuid::new_v4(),
            selector: ManagerNodeSelectorV1::Selected {
                group_ids: vec![],
                epic_ids: vec![Uuid::new_v4()],
            },
            grant: grant(),
            policy: ManagerPolicyV2 {
                capabilities: vec![
                    ManagerCapabilityV2::WorkPlan,
                    ManagerCapabilityV2::LeadControl,
                ],
                max_created_containers: 8,
                max_created_sessions: 16,
                max_active_sessions: 8,
                ..Default::default()
            },
            expected_parent_grant_version: 1,
            expected_parent_policy_version: 1,
            expected_parent_authority_epoch: 1,
            expected_node_grant_version: 0,
            idempotency_key: "area-one".into(),
        };
        assert!(request.validate().is_ok());
        request.policy.max_active_sessions = 9;
        assert!(request.validate().is_err());
        request.policy.max_active_sessions = 8;
        request.expected_node_grant_version = 1;
        assert!(request.validate().is_err());
        request.node_id = Some(Uuid::new_v4());
        request.parent_node_id = Uuid::nil();
        request.expected_parent_grant_version = 0;
        request.expected_parent_policy_version = 0;
        request.expected_parent_authority_epoch = 0;
        assert!(request.validate().is_ok());
    }
}
