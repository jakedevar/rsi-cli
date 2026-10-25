//! Fractal manager hierarchy S6 (#1240): the node workspace and overview.
//!
//! `GetManagerNodeWorkspace {node}` (operator-only) and `AgentManagerOverview
//! {}` (the caller's own node) return one `ManagerNodeWorkspaceV1` built
//! from the store snapshot (`Store::manager_node_workspace_raw`) plus the
//! live seat sessions. `GetGlobalManagerWorkspace` and `AgentGlobalOverview`
//! are their v0-shaped shims (`global_manager_verbs`).

use std::path::PathBuf;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use rsi_common::manager_node_workspace::{
    AgentManagerOverviewRequestV1, ManagerNodeChildV1, ManagerNodeFleetV1,
    ManagerNodeSeatHandoffV1, ManagerNodeWorkspaceV1, ManagerOverviewFleetV1,
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
        request: AgentManagerOverviewRequestV1,
    ) -> Result<ManagerNodeWorkspaceV1> {
        let node = self
            .store
            .lock()
            .await
            .manager_caller_node(caller)?
            .ok_or_else(|| DaemonError::InvalidParam(MANAGER_TIER_NOT_NODE_SEAT.into()))?;
        let with_fleet = request.fleet != ManagerOverviewFleetV1::None;
        self.manager_node_workspace(node, ProjectSpan::Direct, with_fleet)
            .await
    }

    /// #2052, #3531: `AgentManagerOverview` as the agent sees it: the
    /// workspace with the fleet rollup cut to the requested detail (default
    /// `summary`) and the repeated per-seat data compacted (default
    /// `detail: compact`).
    ///
    /// # Errors
    /// As [`Self::agent_manager_overview`], or a serialization error.
    pub async fn agent_manager_overview_value(
        &self,
        caller: Uuid,
        request: AgentManagerOverviewRequestV1,
    ) -> Result<serde_json::Value> {
        let workspace = self.agent_manager_overview(caller, request.clone()).await?;
        Ok(workspace.into_overview_value(&request)?)
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
        let standing_directives = self.active_standing_directives(raw.node).await;
        ManagerNodeWorkspaceV1 {
            seat: self.seat_view(raw.seat_session_id).await,
            standing_directives,
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
            last_seat_handoff: flag_handoff_changed(raw.last_seat_handoff).await,
        }
    }
}

/// #2749: the file a seat handoff names, when it names one the daemon can
/// find on this host: an absolute path or one under `~/`. A commit, a
/// repository-relative path or anything else is `None` (not checkable).
fn handoff_file(reference: &str) -> Option<PathBuf> {
    let reference = reference.trim();
    let path = match reference.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()?.join(rest),
        None => PathBuf::from(reference),
    };
    path.is_absolute().then_some(path)
}

/// #2749: set `handoff_changed_after_appointment` when the handoff file was
/// modified after the appointment completed. The retired seat has no token
/// once the successor holds the seat, so its only channel is to edit this
/// file; a successor that read it earlier would never learn of the edit. The
/// flag is `false` for a handoff that names no checkable file or whose file
/// cannot be read.
async fn flag_handoff_changed(
    handoff: Option<ManagerNodeSeatHandoffV1>,
) -> Option<ManagerNodeSeatHandoffV1> {
    let mut handoff = handoff?;
    let modified = match handoff.handoff.as_deref().and_then(handoff_file) {
        Some(path) => tokio::fs::metadata(path)
            .await
            .ok()
            .and_then(|meta| meta.modified().ok()),
        None => None,
    };
    apply_handoff_modified(&mut handoff, modified);
    Some(handoff)
}

fn apply_handoff_modified(handoff: &mut ManagerNodeSeatHandoffV1, modified: Option<SystemTime>) {
    let modified = modified.map(DateTime::<Utc>::from);
    handoff.handoff_changed_after_appointment = modified.is_some_and(|at| at > handoff.handed_at);
    handoff.handoff_modified_at = modified.filter(|at| *at > handoff.handed_at);
}

#[cfg(test)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[path = "manager_node_overview_handoff_tests.rs"]
mod handoff_tests;

#[cfg(test)]
#[path = "manager_node_overview_tests.rs"]
mod tests;
