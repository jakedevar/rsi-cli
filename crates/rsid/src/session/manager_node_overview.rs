//! Fractal manager hierarchy S6 (#1240): the node workspace and overview.
//!
//! `GetManagerNodeWorkspace {node}` (operator-only) and `AgentManagerOverview
//! {}` (the caller's own node) return one `ManagerNodeWorkspaceV1` built
//! from the store snapshot (`Store::manager_node_workspace_raw`) plus the
//! live seat sessions. `GetGlobalManagerWorkspace` and `AgentGlobalOverview`
//! are their v0-shaped shims (`global_manager_verbs`).

use chrono::Utc;
use rsi_common::manager_node_workspace::{
    AgentManagerOverviewRequestV1, ManagerNodeChildV1, ManagerNodeFleetV1, ManagerNodeWorkspaceV1,
};
use rsi_common::manager_tier_routing::{MANAGER_TIER_NOT_NODE_SEAT, ManagerNodeRefV1};
use uuid::Uuid;

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};
use crate::store::manager_node_workspace::{NodeWorkspaceRaw, ProjectSpan};

impl AgentControlHandle {
    /// Operator-only `GetManagerNodeWorkspace`: `node`'s whole coverage. No
    /// caller check: the RPC is default-denied to tokened callers because it
    /// is not in the attributed verb registry.
    ///
    /// # Errors
    /// `manager_tier_target_unknown` or a persistence error.
    pub async fn operator_manager_node_workspace(
        &self,
        node: ManagerNodeRefV1,
    ) -> Result<ManagerNodeWorkspaceV1> {
        self.manager_node_workspace(node, ProjectSpan::Coverage, true)
            .await
    }

    /// `AgentManagerOverview`: the caller's own node, bounded for agents
    /// (only the projects it manages directly; each child as a digest).
    ///
    /// # Errors
    /// `manager_tier_not_node_seat` for a caller that holds no manager node
    /// seat, or a persistence error.
    pub async fn agent_manager_overview(
        &self,
        caller: Uuid,
        _request: AgentManagerOverviewRequestV1,
    ) -> Result<ManagerNodeWorkspaceV1> {
        let node = self
            .store
            .lock()
            .await
            .manager_caller_node(caller)?
            .ok_or_else(|| DaemonError::InvalidParam(MANAGER_TIER_NOT_NODE_SEAT.into()))?;
        self.manager_node_workspace(node, ProjectSpan::Direct, true)
            .await
    }

    /// One snapshot of `node`; `with_fleet: false` skips the fleet rollup
    /// (the v0 shim drops it).
    pub(super) async fn manager_node_workspace(
        &self,
        node: ManagerNodeRefV1,
        span: ProjectSpan,
        with_fleet: bool,
    ) -> Result<ManagerNodeWorkspaceV1> {
        let raw = self.store.lock().await.manager_node_workspace_raw(
            node,
            span,
            Utc::now(),
            with_fleet,
        )?;
        Ok(self.assemble_node_workspace(raw).await)
    }

    async fn assemble_node_workspace(&self, raw: NodeWorkspaceRaw) -> ManagerNodeWorkspaceV1 {
        let mut children = Vec::with_capacity(raw.children.len());
        for child in raw.children {
            children.push(ManagerNodeChildV1 {
                seat: self.seat_view(child.seat_session_id).await,
                node: child.node,
                label: child.label,
                state: child.state,
                grantor: child.grantor,
                grant_version: child.grant_version,
                project_ids: child.project_ids,
                counts: child.counts,
                pending_escalations: child.pending_escalations,
            });
        }
        ManagerNodeWorkspaceV1 {
            seat: self.seat_view(raw.seat_session_id).await,
            node: raw.node,
            label: raw.label,
            state: raw.state,
            parent: raw.parent,
            grant: raw.grant,
            grantor: raw.grantor,
            max_direct_reports: raw.max_direct_reports,
            area: raw.area,
            children,
            children_truncated: raw.children_truncated,
            projects: self.workspace_projects(raw.project_rows).await,
            missing_project_ids: raw.missing_project_ids,
            escalations: raw.escalations,
            escalations_truncated: raw.escalations_truncated,
            fleet: raw
                .fleet
                .unwrap_or_else(|| ManagerNodeFleetV1::empty(Utc::now())),
        }
    }
}

#[cfg(test)]
#[path = "manager_node_overview_tests.rs"]
mod tests;
