//! Fractal manager hierarchy S6 (#1240): one bounded snapshot of any manager
//! node (portfolio of any tier, project or area).
//!
//! The snapshot is assembled from the reads each surface already owns, not
//! from a per-tier query:
//! - coverage, children and parents come from the portfolio grant rows
//!   (`portfolio_nodes`), the area node rows and `parent_of`
//!   (`tier_parent_of`);
//! - each covered project's row is #1213's `global_project_row`;
//! - escalations are the open in-project escalations and the open hops above
//!   the project root (#1238), counted the way the manager tree counts them;
//! - the fleet rollup is #1232's aggregation (`fleet_overview_scoped`)
//!   filtered to the node's coverage; an area's coverage is its selected
//!   Epics' session tree, bounded by `AREA_FLEET_MAX_SESSIONS` (a clipped
//!   tree marks the rollup truncated, #1329).
//!
//! An area manages no project directly, so its agent overview
//! (`ProjectSpan::Direct`) carries no project row: the row is the whole
//! project's operator summary, beyond an Epic-scoped grant (#1328).
//!
//! The session layer adds the live seat sessions.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use rsi_common::global_manager::GlobalManagerGrantV1;
use rsi_common::manager_node_workspace::{
    MANAGER_NODE_WORKSPACE_MAX_CHILDREN, MANAGER_NODE_WORKSPACE_MAX_ESCALATIONS,
    MANAGER_NODE_WORKSPACE_MAX_REASON_BYTES, ManagerNodeAreaGrantV1, ManagerNodeCountsV1,
    ManagerNodeFleetV1, ManagerNodePendingEscalationV1, bounded_reason,
};
use rsi_common::manager_nodes::ManagerNodeSelectorV1;
use rsi_common::manager_tier_routing::{MANAGER_TIER_TARGET_UNKNOWN, ManagerNodeRefV1};
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use super::Store;
use super::fleet::FleetScope;
use super::global_manager::GlobalProjectRow;
use super::portfolio_nodes::{self, GrantRecord};
use crate::error::{DaemonError, Result};

/// The most sessions an area's fleet coverage enumerates; past it the
/// rollup is marked truncated (#1329).
const AREA_FLEET_MAX_SESSIONS: usize = 65_536;

/// Which covered projects a snapshot lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectSpan {
    /// The node's whole coverage (the operator workspace).
    Coverage,
    /// Only the projects the node manages directly: a portfolio node's
    /// covered projects that no deeper node covers, none for an area node
    /// (the agent overview).
    Direct,
}

/// One child digest before the session layer adds its seat.
#[derive(Debug, Clone)]
pub struct NodeChildRaw {
    pub node: ManagerNodeRefV1,
    pub label: String,
    pub state: String,
    pub grantor: Option<String>,
    pub grant_version: i64,
    pub seat_session_id: Option<Uuid>,
    pub project_ids: Vec<Uuid>,
    pub counts: Option<ManagerNodeCountsV1>,
    pub pending_escalations: i64,
}

/// A node snapshot before the session layer adds the seat sessions.
pub struct NodeWorkspaceRaw {
    pub node: ManagerNodeRefV1,
    pub label: String,
    pub state: String,
    pub parent: Option<ManagerNodeRefV1>,
    pub grant: Option<GlobalManagerGrantV1>,
    pub grantor: Option<String>,
    pub max_direct_reports: Option<u16>,
    pub area: Option<ManagerNodeAreaGrantV1>,
    pub seat_session_id: Option<Uuid>,
    pub children: Vec<NodeChildRaw>,
    pub children_truncated: bool,
    pub project_rows: Vec<GlobalProjectRow>,
    pub missing_project_ids: Vec<Uuid>,
    pub escalations: Vec<ManagerNodePendingEscalationV1>,
    pub escalations_truncated: bool,
    /// `None` when the caller skipped the fleet rollup.
    pub fleet: Option<ManagerNodeFleetV1>,
}

fn refused(code: &str) -> DaemonError {
    DaemonError::InvalidParam(code.into())
}

fn parse_uuid(text: &str) -> Result<Uuid> {
    Uuid::parse_str(text).map_err(|_| DaemonError::Store("invalid manager node uuid".into()))
}

fn parse_time(text: &str) -> Result<DateTime<Utc>> {
    super::parse_timestamp(text)
        .map_err(|_| DaemonError::Store("invalid manager node timestamp".into()))
}

/// Counts summed over project rows.
fn sum_counts<'a>(rows: impl IntoIterator<Item = &'a GlobalProjectRow>) -> ManagerNodeCountsV1 {
    let mut counts = ManagerNodeCountsV1::default();
    for row in rows {
        counts.issues.open += row.issues.open;
        counts.issues.in_progress += row.issues.in_progress;
        counts.issues.open_operator_requests += row.issues.open_operator_requests;
        counts.running_sessions += row.running_sessions;
        counts.waiting_approval_sessions += row.waiting_approval_sessions;
        counts.pending_questions += row.pending_questions;
        counts.pending_approvals += row.pending_approvals;
    }
    counts
}

fn project_state(row: &GlobalProjectRow) -> &'static str {
    if row.scope_revoked {
        "revoked"
    } else if row.manager_session_id.is_none() {
        "vacant"
    } else {
        "active"
    }
}

/// An area node row: its project and parent, and whether it is live.
struct AreaIdentity {
    project: Uuid,
    parent: Option<Uuid>,
}

impl Store {
    /// The manager node whose seat `caller` holds (#1238's resolver): a
    /// portfolio seat, a project's live PM, or an area node's seat tip.
    ///
    /// # Errors
    /// A persistence error.
    pub fn manager_caller_node(&self, caller: Uuid) -> Result<Option<ManagerNodeRefV1>> {
        self.tier_caller_node(caller)
    }

    /// One snapshot of `node` (see the module docs).
    ///
    /// # Errors
    /// `manager_tier_target_unknown` when the node does not exist, or a
    /// persistence error.
    pub fn manager_node_workspace_raw(
        &self,
        node: ManagerNodeRefV1,
        span: ProjectSpan,
        now: DateTime<Utc>,
        with_fleet: bool,
    ) -> Result<NodeWorkspaceRaw> {
        let now = with_fleet.then_some(now);
        match node {
            ManagerNodeRefV1::Portfolio { node_id } => self.portfolio_workspace(node_id, span, now),
            ManagerNodeRefV1::Project { project_id } => self.project_workspace(project_id, now),
            ManagerNodeRefV1::Area { node_id } => {
                let identity = self
                    .area_identity(node_id)?
                    .ok_or_else(|| refused(MANAGER_TIER_TARGET_UNKNOWN))?;
                if identity.parent.is_none() {
                    // A project's root area is addressed as its project.
                    return self.project_workspace(identity.project, now);
                }
                self.area_workspace(node_id, identity.project, span, now)
            }
        }
    }

    fn portfolio_workspace(
        &self,
        node_id: Uuid,
        span: ProjectSpan,
        now: Option<DateTime<Utc>>,
    ) -> Result<NodeWorkspaceRaw> {
        let row = portfolio_nodes::node_row_on(&self.conn, node_id)?
            .ok_or_else(|| refused(MANAGER_TIER_TARGET_UNKNOWN))?;
        let active = portfolio_nodes::node_grant_on(&self.conn, node_id)?;
        let live = row.active && active.is_some();
        let record = match active {
            Some(record) => record,
            None => portfolio_nodes::latest_node_grant_on(&self.conn, node_id)?
                .ok_or_else(|| refused(MANAGER_TIER_TARGET_UNKNOWN))?,
        };
        let node = ManagerNodeRefV1::Portfolio { node_id };
        let coverage = record.grant.project_ids.clone();
        let mut direct = Vec::new();
        if live {
            for project in &coverage {
                if portfolio_nodes::covering_node_on(&self.conn, *project)? == Some(node_id) {
                    direct.push(*project);
                }
            }
        }
        let project_rows = match span {
            ProjectSpan::Coverage => self.global_project_rows(&record.grant)?,
            ProjectSpan::Direct => self.project_rows_of(&direct)?,
        };
        let mut missing_project_ids = Vec::new();
        for project in &coverage {
            if self.get_project(*project)?.is_none() {
                missing_project_ids.push(*project);
            }
        }
        let mut children = Vec::new();
        if live {
            for child in portfolio_nodes::active_children_on(&self.conn, node_id)? {
                children.push(self.portfolio_child(&child)?);
            }
            for project in &direct {
                if let Some(child) = self.project_child(*project)? {
                    children.push(child);
                }
            }
        }
        let children_truncated = children.len() > MANAGER_NODE_WORKSPACE_MAX_CHILDREN;
        children.truncate(MANAGER_NODE_WORKSPACE_MAX_CHILDREN);
        let (escalations, escalations_truncated) = self.portfolio_escalations(node_id)?;
        let fleet = self.node_fleet(now, &FleetScope::Projects(coverage.into_iter().collect()))?;
        Ok(NodeWorkspaceRaw {
            node,
            label: row.tier_label,
            state: if live { "active" } else { "revoked" }.into(),
            parent: if live {
                self.tier_parent_of(node)?
            } else {
                None
            },
            seat_session_id: Some(record.grant.seat_session_id),
            grantor: Some(record.grantor.clone()),
            max_direct_reports: Some(record.max_direct_reports),
            grant: Some(record.grant),
            area: None,
            children,
            children_truncated,
            project_rows,
            missing_project_ids,
            escalations,
            escalations_truncated,
            fleet,
        })
    }

    fn project_workspace(
        &self,
        project_id: Uuid,
        now: Option<DateTime<Utc>>,
    ) -> Result<NodeWorkspaceRaw> {
        let project = self
            .get_project(project_id)?
            .ok_or_else(|| refused(MANAGER_TIER_TARGET_UNKNOWN))?;
        let row = self.global_project_row(project)?;
        let root = self.area_root(project_id)?;
        let (mut children, mut escalations, mut escalations_truncated) =
            (Vec::new(), Vec::new(), false);
        if let Some(root) = root {
            children = self.area_children(root, project_id)?;
            (escalations, escalations_truncated) = self.area_escalations(root)?;
        }
        let children_truncated = children.len() > MANAGER_NODE_WORKSPACE_MAX_CHILDREN;
        children.truncate(MANAGER_NODE_WORKSPACE_MAX_CHILDREN);
        let node = ManagerNodeRefV1::Project { project_id };
        let fleet = self.node_fleet(now, &FleetScope::Projects(BTreeSet::from([project_id])))?;
        Ok(NodeWorkspaceRaw {
            node,
            label: row.name.clone(),
            state: project_state(&row).into(),
            parent: self.tier_parent_of(node)?,
            grant: None,
            grantor: None,
            max_direct_reports: None,
            area: None,
            seat_session_id: row.manager_session_id,
            children,
            children_truncated,
            project_rows: vec![row],
            missing_project_ids: Vec::new(),
            escalations,
            escalations_truncated,
            fleet,
        })
    }

    fn area_workspace(
        &self,
        node_id: Uuid,
        project_id: Uuid,
        span: ProjectSpan,
        now: Option<DateTime<Utc>>,
    ) -> Result<NodeWorkspaceRaw> {
        let area = self
            .get_area_node(project_id, node_id)?
            .ok_or_else(|| refused(MANAGER_TIER_TARGET_UNKNOWN))?;
        let live = area.active && area.grant.is_some();
        let node = ManagerNodeRefV1::Area { node_id };
        let mut children = self.area_children(node_id, project_id)?;
        let children_truncated = children.len() > MANAGER_NODE_WORKSPACE_MAX_CHILDREN;
        children.truncate(MANAGER_NODE_WORKSPACE_MAX_CHILDREN);
        let (escalations, escalations_truncated) = self.area_escalations(node_id)?;
        // The project row is the whole project's operator summary (Issue,
        // question and approval counts, every Epic's sessions, the PM's
        // policy and seat): the operator workspace shows the containing
        // project, the area's own agent overview does not (#1328).
        let project_rows = match span {
            ProjectSpan::Coverage => self.project_rows_of(&[project_id])?,
            ProjectSpan::Direct => Vec::new(),
        };
        let fleet = match now {
            Some(_) => {
                let (scope, clipped) = self.area_fleet_scope(
                    project_id,
                    area.selector.as_ref(),
                    AREA_FLEET_MAX_SESSIONS,
                )?;
                self.node_fleet(now, &scope)?.map(|mut fleet| {
                    fleet.agents_truncated |= clipped;
                    fleet.usage_truncated |= clipped;
                    fleet
                })
            }
            None => None,
        };
        Ok(NodeWorkspaceRaw {
            node,
            label: "area".into(),
            state: if live { "active" } else { "revoked" }.into(),
            parent: self.tier_parent_of(node)?,
            grant: None,
            grantor: None,
            max_direct_reports: area.grant.as_ref().map(|grant| grant.max_direct_reports),
            seat_session_id: if live {
                Some(self.manager_lineage_tip(area.seat_root_session_id)?)
            } else {
                None
            },
            area: Some(ManagerNodeAreaGrantV1 {
                project_id,
                active: live,
                grant_version: area.grant_version,
                authority_epoch: area.authority_epoch,
                selector: area.selector,
                grant: area.grant,
            }),
            children,
            children_truncated,
            project_rows,
            missing_project_ids: Vec::new(),
            escalations,
            escalations_truncated,
            fleet,
        })
    }

    /// Project rows of `projects` that still exist, in order.
    fn project_rows_of(&self, projects: &[Uuid]) -> Result<Vec<GlobalProjectRow>> {
        let mut rows = Vec::with_capacity(projects.len());
        for id in projects {
            if let Some(project) = self.get_project(*id)? {
                rows.push(self.global_project_row(project)?);
            }
        }
        Ok(rows)
    }

    /// A child portfolio node's digest: its grant, seat and counts summed
    /// over its coverage. Its own children and projects stay out.
    fn portfolio_child(&self, child: &GrantRecord) -> Result<NodeChildRaw> {
        let node_id = child
            .node_id
            .ok_or_else(|| DaemonError::Store("active portfolio grant without a node".into()))?;
        let label = portfolio_nodes::node_row_on(&self.conn, node_id)?
            .map(|row| row.tier_label)
            .unwrap_or_default();
        let rows = self.global_project_rows(&child.grant)?;
        Ok(NodeChildRaw {
            node: ManagerNodeRefV1::Portfolio { node_id },
            label,
            state: "active".into(),
            grantor: Some(child.grantor.clone()),
            grant_version: child.grant.grant_version,
            seat_session_id: Some(child.grant.seat_session_id),
            project_ids: child.grant.project_ids.clone(),
            counts: Some(sum_counts(&rows)),
            pending_escalations: self.tier_open_hops_for_node(node_id).unwrap_or(0),
        })
    }

    /// A covered project's digest: its PM seat and counts.
    fn project_child(&self, project_id: Uuid) -> Result<Option<NodeChildRaw>> {
        let Some(project) = self.get_project(project_id)? else {
            return Ok(None);
        };
        let row = self.global_project_row(project)?;
        let pending_escalations = match self.area_root(project_id)? {
            Some(root) => self.open_area_escalations(root)?,
            None => 0,
        };
        Ok(Some(NodeChildRaw {
            node: ManagerNodeRefV1::Project { project_id },
            label: row.name.clone(),
            state: project_state(&row).into(),
            grantor: None,
            grant_version: row.scope_version.unwrap_or(0),
            seat_session_id: row.manager_session_id,
            project_ids: vec![project_id],
            counts: Some(sum_counts([&row])),
            pending_escalations,
        }))
    }

    /// The active child areas of area node `parent`.
    fn area_children(&self, parent: Uuid, project_id: Uuid) -> Result<Vec<NodeChildRaw>> {
        let ids: Vec<String> = {
            let mut statement = self.conn.prepare(
                "SELECT id FROM manager_nodes WHERE parent_node_id=?1 AND state='active' ORDER BY created_at,id LIMIT ?2",
            )?;
            statement
                .query_map(
                    params![
                        parent.to_string(),
                        MANAGER_NODE_WORKSPACE_MAX_CHILDREN as i64 + 1
                    ],
                    |row| row.get(0),
                )?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut children = Vec::with_capacity(ids.len());
        for id in ids {
            let id = parse_uuid(&id)?;
            let Some(area) = self.get_area_node(project_id, id)? else {
                continue;
            };
            let live = area.active && area.grant.is_some();
            children.push(NodeChildRaw {
                node: ManagerNodeRefV1::Area { node_id: id },
                label: "area".into(),
                state: if live { "active" } else { "revoked" }.into(),
                grantor: None,
                grant_version: area.grant_version,
                seat_session_id: if live {
                    Some(self.manager_lineage_tip(area.seat_root_session_id)?)
                } else {
                    None
                },
                project_ids: vec![project_id],
                counts: None,
                pending_escalations: self.open_area_escalations(id)?,
            });
        }
        Ok(children)
    }

    /// The project's root area node, when one exists.
    fn area_root(&self, project_id: Uuid) -> Result<Option<Uuid>> {
        self.conn
            .query_row(
                "SELECT id FROM manager_nodes WHERE legacy_project_id=?1",
                [project_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .as_deref()
            .map(parse_uuid)
            .transpose()
    }

    fn area_identity(&self, node_id: Uuid) -> Result<Option<AreaIdentity>> {
        let row: Option<(Option<String>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT COALESCE(n.legacy_project_id,(SELECT s.project_id FROM manager_node_scopes s WHERE s.node_id=n.id LIMIT 1)),n.parent_node_id
                 FROM manager_nodes n WHERE n.id=?1",
                [node_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((Some(project), parent)) = row else {
            return Ok(None);
        };
        Ok(Some(AreaIdentity {
            project: parse_uuid(&project)?,
            parent: parent.as_deref().map(parse_uuid).transpose()?,
        }))
    }

    /// Open escalations addressed to area node `node` that are not held
    /// above the project root (the manager tree's count, #1238).
    fn open_area_escalations(&self, node: Uuid) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM manager_node_escalations e WHERE e.target_node_id=?1 AND e.state='open'
               AND NOT EXISTS(SELECT 1 FROM manager_tier_escalations h WHERE h.escalation_id=e.id AND h.state='open')",
            [node.to_string()],
            |row| row.get(0),
        )?)
    }

    fn area_escalations(&self, node: Uuid) -> Result<(Vec<ManagerNodePendingEscalationV1>, bool)> {
        let mut statement = self.conn.prepare(
            "SELECT e.id,e.project_id,e.subject_id,e.reason,e.created_at FROM manager_node_escalations e
              WHERE e.target_node_id=?1 AND e.state='open'
                AND NOT EXISTS(SELECT 1 FROM manager_tier_escalations h WHERE h.escalation_id=e.id AND h.state='open')
              ORDER BY e.created_at,e.id LIMIT ?2",
        )?;
        let rows = statement
            .query_map(
                params![
                    node.to_string(),
                    MANAGER_NODE_WORKSPACE_MAX_ESCALATIONS as i64 + 1
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let truncated = rows.len() > MANAGER_NODE_WORKSPACE_MAX_ESCALATIONS;
        let mut escalations = Vec::with_capacity(rows.len());
        for (id, project, subject, reason, created) in rows
            .into_iter()
            .take(MANAGER_NODE_WORKSPACE_MAX_ESCALATIONS)
        {
            escalations.push(ManagerNodePendingEscalationV1 {
                escalation_id: parse_uuid(&id)?,
                project_id: parse_uuid(&project)?,
                subject_id: parse_uuid(&subject)?,
                reason: bounded_reason(&reason, MANAGER_NODE_WORKSPACE_MAX_REASON_BYTES),
                hop: None,
                created_at: parse_time(&created)?,
            });
        }
        Ok((escalations, truncated))
    }

    /// The open hops above the project root addressed to portfolio `node`.
    fn portfolio_escalations(
        &self,
        node: Uuid,
    ) -> Result<(Vec<ManagerNodePendingEscalationV1>, bool)> {
        let mut statement = self.conn.prepare(
            "SELECT h.escalation_id,h.project_id,e.subject_id,e.reason,h.hop,h.created_at
               FROM manager_tier_escalations h JOIN manager_node_escalations e ON e.id=h.escalation_id
              WHERE h.target_ref=?1 AND h.state='open' ORDER BY h.created_at,h.id LIMIT ?2",
        )?;
        let rows = statement
            .query_map(
                params![
                    format!("portfolio:{node}"),
                    MANAGER_NODE_WORKSPACE_MAX_ESCALATIONS as i64 + 1
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let truncated = rows.len() > MANAGER_NODE_WORKSPACE_MAX_ESCALATIONS;
        let mut escalations = Vec::with_capacity(rows.len());
        for (id, project, subject, reason, hop, created) in rows
            .into_iter()
            .take(MANAGER_NODE_WORKSPACE_MAX_ESCALATIONS)
        {
            escalations.push(ManagerNodePendingEscalationV1 {
                escalation_id: parse_uuid(&id)?,
                project_id: parse_uuid(&project)?,
                subject_id: parse_uuid(&subject)?,
                reason: bounded_reason(&reason, MANAGER_NODE_WORKSPACE_MAX_REASON_BYTES),
                hop: Some(hop),
                created_at: parse_time(&created)?,
            });
        }
        Ok((escalations, truncated))
    }

    /// The fleet rows an area node covers: its whole project for a
    /// project-wide selector, else the sessions under its selected Epics
    /// (each Epic, every descendant and each Epic's lead). One recursive
    /// query walks every selected Epic's tree; it stops after `max` sessions
    /// and the second value says the set was clipped, so the rollup is
    /// marked truncated instead of silently partial (#1329).
    fn area_fleet_scope(
        &self,
        project_id: Uuid,
        selector: Option<&ManagerNodeSelectorV1>,
        max: usize,
    ) -> Result<(FleetScope, bool)> {
        let selector = match selector {
            None | Some(ManagerNodeSelectorV1::Project) => {
                return Ok((FleetScope::Projects(BTreeSet::from([project_id])), false));
            }
            Some(selector) => selector,
        };
        let epics = super::harness_manager_v2::manager_node_authority::live_selected_epics(
            self, project_id, selector,
        )?;
        let epic_json =
            serde_json::to_string(&epics.iter().map(Uuid::to_string).collect::<Vec<_>>())
                .map_err(|error| DaemonError::Store(format!("area fleet epics: {error}")))?;
        let ids = {
            let mut statement = self.conn.prepare(
                "WITH RECURSIVE tree(id) AS (
                    SELECT value FROM json_each(?1)
                    UNION SELECT s.id FROM sessions s JOIN tree ON s.parent_id=tree.id
                    LIMIT ?2)
                 SELECT id FROM tree",
            )?;
            statement
                .query_map(params![epic_json, max as i64 + 1], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let clipped = ids.len() > max;
        let mut sessions = BTreeSet::new();
        for id in ids.iter().take(max) {
            sessions.insert(parse_uuid(id)?);
        }
        let leads = {
            let mut statement = self.conn.prepare(
                "SELECT lead_session_id FROM sessions
                  WHERE id IN (SELECT value FROM json_each(?1)) AND lead_session_id IS NOT NULL",
            )?;
            statement
                .query_map([epic_json], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for id in leads {
            sessions.insert(parse_uuid(&id)?);
        }
        Ok((FleetScope::Sessions(sessions), clipped))
    }

    fn node_fleet(
        &self,
        now: Option<DateTime<Utc>>,
        scope: &FleetScope,
    ) -> Result<Option<ManagerNodeFleetV1>> {
        now.map(|now| {
            self.fleet_overview_scoped(now, scope)
                .map(ManagerNodeFleetV1::from_overview)
        })
        .transpose()
    }

    /// The `*GlobalManager` shims' node: the single active root labelled
    /// `global` (`global_manager_ambiguous` when several exist).
    ///
    /// # Errors
    /// `global_manager_ambiguous` or a persistence error.
    pub fn global_shim_node(&self) -> Result<Option<Uuid>> {
        portfolio_nodes::global_shim_node_on(&self.conn)
    }
}

#[cfg(test)]
#[path = "manager_node_workspace_tests.rs"]
mod tests;
