//! #1235 (fractal manager hierarchy S1, plan §2.3): a portfolio node's seat
//! holds the project-manager verb set inside every project of its grant.
//! Since #1236 (S2) the arm resolves through the caller's own node: several
//! disjoint nodes may be active, and each seat reaches only its coverage.
//!
//! The arm copies the area-node storage principal precedent
//! (`manager_node_authority.rs`): the V2 ledger principal of a node acting
//! in project `p` is `(p, seat root, authority epoch)`.
//! - The authority epoch is `manager_portfolio_nodes.authority_epoch`: the
//!   `grant_version` of the operator grant that opened it. It bumps on an
//!   operator edit and never on a #1005 context-cap seat transfer, so the
//!   successor controls its predecessor's workers; an operator re-grant or a
//!   revoke starts a new epoch.
//! - The seat root is that epoch grant's seat. Every context-cap successor
//!   continues from it, so this is the lineage root of the transfer chain,
//!   read from immutable grant history rather than a lineage walk.
//! - The fence is `{scope_version: epoch, policy_version: grant_version}`.
//!   Every effect re-resolves the arm, so a revoke, replacement or transfer
//!   between admission and effect refuses the effect.
//!
//! Identity always comes from the transport-bound caller; `project_id` is only
//! a target checked against the grant.

use rsi_common::global_manager::{GlobalManagerGrantV1, MANAGER_PROJECT_NOT_IN_SCOPE};
use rsi_common::grant_narrowing::portfolio_effective_launches;
use rsi_common::harness_manager::{HarnessManagerConfigV1, HarnessManagerScopeModeV1};
use rsi_common::harness_manager_v2::{
    HarnessManagerPolicyConfigV2, ManagerCapabilityV2, ManagerFenceV2, ManagerLaunchChoiceV2,
    ManagerOperatingModeV2, ManagerPolicyV2,
};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use super::{ManagerAuthorityV2, Store, refused};
use crate::error::{DaemonError, Result};
use crate::store::portfolio_nodes::{
    PORTFOLIO_DEPTH_LIMIT, chain_seat_node_on, latest_node_grant_on, node_grant_on,
    portfolio_head_on, retired_chain_seat_on, seat_grant_on,
};

/// Capabilities the global arm never exercises in S1: root succession belongs
/// to the project manager seat, and Git effects and delegated operator calls
/// stay with the project manager (and the operator) until a later slice
/// re-proves them for a portfolio principal.
const WITHHELD: &[ManagerCapabilityV2] = &[
    ManagerCapabilityV2::SelfSuccession,
    ManagerCapabilityV2::GitEffect,
    ManagerCapabilityV2::OperatorDelegation,
    ManagerCapabilityV2::StorageControl,
    ManagerCapabilityV2::DaemonSettings,
];

/// Bound on the parent and rotation walks of the ownership check.
const OWNER_WALK_LIMIT: usize = 32;

/// A portfolio node's active seat acting in one covered project.
#[derive(Debug, Clone)]
pub struct GlobalProjectPrincipal {
    /// The portfolio node the seat holds.
    pub node_id: Uuid,
    pub grant: GlobalManagerGrantV1,
    pub project_id: Uuid,
    /// V2 storage principal: the seat of the epoch's operator grant.
    pub seat_root: Uuid,
    pub authority_epoch: i64,
}

impl GlobalProjectPrincipal {
    /// The daemon's current fence for this principal.
    pub fn fence(&self) -> ManagerFenceV2 {
        ManagerFenceV2 {
            scope_version: self.authority_epoch,
            policy_version: self.grant.grant_version,
        }
    }

    /// Whether `config` is this principal's ledger config.
    pub fn owns_config(&self, config: &HarnessManagerConfigV1) -> bool {
        config.project_id == self.project_id
            && config.manager_session_id == self.seat_root
            && config.row_version == self.authority_epoch
    }
}

/// The portfolio arm for `caller` in `project`, read through `conn` (an open
/// transaction sees its own snapshot). The caller's node is the one whose
/// active grant names it as the seat (one active grant per seat).
/// - `Ok(None)`: the caller holds no portfolio seat.
/// - `manager_node_custody_changed`: the caller is a retired seat of an
///   active node's current transfer chain; its token never borrows the
///   successor's grant.
/// - `manager_project_not_in_scope`: the node's grant does not cover
///   `project`.
pub(crate) fn global_project_principal_on(
    conn: &Connection,
    caller: Uuid,
    project: Uuid,
) -> Result<Option<GlobalProjectPrincipal>> {
    let Some(record) = seat_grant_on(conn, caller)? else {
        if retired_chain_seat_on(conn, caller)? {
            return Err(refused("manager_node_custody_changed"));
        }
        return Ok(None);
    };
    let node_id = record
        .node_id
        .ok_or_else(|| DaemonError::Store("an active global manager grant has no node".into()))?;
    let (authority_epoch, seat_root, _) = portfolio_head_on(conn, node_id)?;
    if !record.grant.project_ids.contains(&project) {
        return Err(refused(MANAGER_PROJECT_NOT_IN_SCOPE));
    }
    Ok(Some(GlobalProjectPrincipal {
        node_id,
        grant: record.grant,
        project_id: project,
        seat_root,
        authority_epoch,
    }))
}

/// `Ok(None)` when the grant does not cover the project, so a caller's own
/// project keeps its legacy and area denial codes. Other refusals and Store
/// errors propagate.
pub(crate) fn uncovered_is_none<T>(result: Result<Option<T>>) -> Result<Option<T>> {
    match result {
        Err(DaemonError::InvalidParam(code)) if code == MANAGER_PROJECT_NOT_IN_SCOPE => Ok(None),
        other => other,
    }
}

/// The global arm of the guarded Issue authority: the active seat whose grant
/// covers `project` and whose project policy holds `IssueCoordinate`. A
/// mutation also needs an Execute, unpaused policy (a Status-mode global
/// reads only). Returns the authority epoch recorded as the actor's scope.
pub(crate) fn global_issue_authority_on(
    conn: &Connection,
    caller: Uuid,
    project: Uuid,
    mutation: bool,
) -> Result<Option<i64>> {
    let Some(principal) = global_project_principal_on(conn, caller, project)? else {
        return Ok(None);
    };
    let policy = &principal.grant.project_policy;
    if !policy
        .capabilities
        .contains(&ManagerCapabilityV2::IssueCoordinate)
        || (mutation && !policy_executes(policy))
    {
        return Err(refused("manager_v2_capability_denied"));
    }
    Ok(Some(principal.authority_epoch))
}

fn policy_executes(policy: &ManagerPolicyV2) -> bool {
    policy.mode == ManagerOperatingModeV2::Execute && !policy.paused
}

/// The launches the global may make in a project (#1237: shared with the
/// narrowing rule).
fn effective_launches(
    grant: &GlobalManagerGrantV1,
) -> Vec<rsi_common::harness_manager_v2::ManagerLaunchChoiceV2> {
    rsi_common::grant_narrowing::portfolio_effective_launches(
        &grant.allowed_launches,
        &grant.project_policy,
    )
}

impl Store {
    /// See [`global_project_principal_on`].
    pub(crate) fn global_project_principal(
        &self,
        caller: Uuid,
        project: Uuid,
    ) -> Result<Option<GlobalProjectPrincipal>> {
        global_project_principal_on(&self.conn, caller, project)
    }

    /// Live Epics of `project` under a live root Group, ordered by id (the
    /// order progress paging relies on).
    pub(crate) fn global_project_epics(&self, project: Uuid) -> Result<Vec<Uuid>> {
        let mut statement = self.conn.prepare(
            "SELECT e.id FROM sessions e JOIN sessions g ON g.id=e.parent_id
             WHERE e.project_id=?1 AND e.session_kind='Epic'
               AND e.status NOT IN ('Archived','Deleted') AND g.project_id=e.project_id
               AND g.session_kind='Group' AND g.parent_id IS NULL
               AND g.status NOT IN ('Archived','Deleted')
             ORDER BY e.id",
        )?;
        let ids = statement
            .query_map([project.to_string()], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter()
            .map(|id| {
                Uuid::parse_str(&id).map_err(|_| refused("manager_v2_invalid_stored_identity"))
            })
            .collect()
    }

    /// The synthetic project-scope authority of one global principal:
    /// project `p`, scope `Project`, every live Epic of `p`, the seat root as
    /// the storage principal and the epoch as the scope version.
    pub(crate) fn global_manager_authority_for(
        &self,
        principal: &GlobalProjectPrincipal,
        caller: Uuid,
    ) -> Result<ManagerAuthorityV2> {
        let epics = self.global_project_epics(principal.project_id)?;
        let mut policy = principal.grant.project_policy.clone();
        policy.allowed_launches = effective_launches(&principal.grant);
        Ok(ManagerAuthorityV2 {
            config: HarnessManagerConfigV1 {
                project_id: principal.project_id,
                manager_session_id: principal.seat_root,
                current_session_id: Some(caller),
                epic_ids: epics.clone(),
                scope_mode: HarnessManagerScopeModeV1::Project,
                selected_epic_ids: Some(epics),
                group_ids: Vec::new(),
                row_version: principal.authority_epoch,
                updated_at: principal.grant.updated_at,
            },
            grant: HarnessManagerPolicyConfigV2 {
                project_id: principal.project_id,
                manager_session_id: principal.seat_root,
                scope_version: principal.authority_epoch,
                row_version: principal.grant.grant_version,
                policy,
                updated_at: principal.grant.updated_at,
                revoked: false,
            },
            caller,
            is_manager: true,
        })
    }

    /// The current (unfenced) global authority of `caller` in `project`, or
    /// `None` when the caller is not the active global seat. Refuses
    /// `manager_project_not_in_scope` outside the grant.
    pub(crate) fn global_manager_authority_current(
        &self,
        caller: Uuid,
        project: Uuid,
    ) -> Result<Option<ManagerAuthorityV2>> {
        self.global_project_principal(caller, project)?
            .map(|principal| self.global_manager_authority_for(&principal, caller))
            .transpose()
    }

    /// The fenced global arm used by admission and by every effect-time
    /// recheck. A fence from another epoch or grant version refuses
    /// `manager_node_authority_changed`.
    pub(crate) fn global_manager_authorize(
        &self,
        caller: Uuid,
        project: Uuid,
        fence: &ManagerFenceV2,
        capability: Option<ManagerCapabilityV2>,
    ) -> Result<Option<ManagerAuthorityV2>> {
        let Some(principal) = self.global_project_principal(caller, project)? else {
            return Ok(None);
        };
        let current = principal.fence();
        if fence.scope_version != current.scope_version
            || fence.policy_version != current.policy_version
        {
            return Err(refused("manager_node_authority_changed"));
        }
        if let Some(required) = capability
            && (WITHHELD.contains(&required)
                || !principal
                    .grant
                    .project_policy
                    .capabilities
                    .contains(&required))
        {
            return Err(refused("manager_v2_capability_denied"));
        }
        self.global_manager_authority_for(&principal, caller)
            .map(Some)
    }

    /// The global authority whose ledger config is exactly `config`, if any.
    /// Used where a config is re-resolved without its caller's request
    /// (policy, direct Epics, mail route).
    pub(crate) fn global_authority_for_config(
        &self,
        config: &HarnessManagerConfigV1,
    ) -> Result<Option<ManagerAuthorityV2>> {
        let Some(caller) = config.current_session_id else {
            return Ok(None);
        };
        // A retired seat or an uncovered project simply is not this config.
        let principal = match self.global_project_principal(caller, config.project_id) {
            Ok(principal) => principal,
            Err(DaemonError::InvalidParam(_)) => None,
            Err(error) => return Err(error),
        };
        principal
            .filter(|principal| principal.owns_config(config))
            .map(|principal| self.global_manager_authority_for(&principal, caller))
            .transpose()
    }

    /// #1237: every active node covering `project`, root first: one line of
    /// authority (sibling coverage is disjoint at every depth).
    pub(crate) fn portfolio_chain_heads(&self, project: Uuid) -> Result<Vec<PortfolioHead>> {
        let mut heads = Vec::new();
        for coverage in self.portfolio_chain_for_project(project)? {
            let Some(record) = node_grant_on(&self.conn, coverage.node_id)? else {
                continue;
            };
            let (epoch, seat_root, started) = portfolio_head_on(&self.conn, coverage.node_id)?;
            heads.push(PortfolioHead {
                node_id: coverage.node_id,
                epoch,
                seat_root,
                started,
                record,
            });
        }
        Ok(heads)
    }

    /// The index in `chain` of the node whose ledger principal `config` is.
    fn chain_position(chain: &[PortfolioHead], config: &HarnessManagerConfigV1) -> Option<usize> {
        chain.iter().position(|head| {
            config.manager_session_id == head.seat_root && config.row_version == head.epoch
        })
    }

    /// The nodes above the principal of `config` in its project's chain: the
    /// whole chain for a project or area principal, the strict ancestors for
    /// a portfolio principal.
    pub(crate) fn portfolio_ancestors_of(
        &self,
        config: &HarnessManagerConfigV1,
    ) -> Result<(Vec<PortfolioHead>, Option<PortfolioHead>)> {
        let mut chain = self.portfolio_chain_heads(config.project_id)?;
        match Self::chain_position(&chain, config) {
            Some(position) => {
                let own = chain.swap_remove(position);
                chain.truncate(position);
                Ok((chain, Some(own)))
            }
            None => Ok((chain, None)),
        }
    }

    /// Resolve a principal's launch list against every live ancestor grant.
    /// An empty own list inherits a finite ancestor list. With no ancestor
    /// and no own list, the result stays empty: launch admission, topology
    /// and automatic recovery fail closed.
    pub fn manager_effective_launches(
        &self,
        config: &HarnessManagerConfigV1,
        own: &[ManagerLaunchChoiceV2],
    ) -> Result<Vec<ManagerLaunchChoiceV2>> {
        let (ancestors, _) = self.portfolio_ancestors_of(config)?;
        let mut launches = (!own.is_empty()).then(|| own.to_vec());
        for head in &ancestors {
            let grant = &head.record.grant;
            let effective =
                portfolio_effective_launches(&grant.allowed_launches, &grant.project_policy);
            launches = Some(match launches {
                Some(current) => current
                    .into_iter()
                    .filter(|launch| effective.contains(launch))
                    .collect(),
                None => effective,
            });
        }
        Ok(launches.unwrap_or_default())
    }

    /// Admission and inspection use the same live intersection. An empty
    /// resolved list never grants a launch, including an independent PM.
    pub(crate) fn manager_launch_policy_gate(
        &self,
        config: &HarnessManagerConfigV1,
        own: &[ManagerLaunchChoiceV2],
        launch: &ManagerLaunchChoiceV2,
    ) -> Result<()> {
        let allowed = self.manager_effective_launches(config, own)?;
        if !allowed.contains(launch) {
            return Err(refused(&format!(
                "manager_v2_launch_not_granted: allowed_launches={}",
                serde_json::to_string(&allowed)?
            )));
        }
        self.manager_ancestor_launch_gate(config, launch)
    }

    /// #1412: the launches a principal may make are the intersection of its
    /// own policy list (checked by the caller) and, live, every ancestor
    /// node's effective launches: the grant's `allowed_launches` narrowed by
    /// its project policy. An appointed PM's policy keeps no copy of the
    /// grant's list, so a grant widened (or narrowed) after the appointment
    /// applies to the PM at its next launch.
    pub(crate) fn manager_ancestor_launch_gate(
        &self,
        config: &HarnessManagerConfigV1,
        launch: &ManagerLaunchChoiceV2,
    ) -> Result<()> {
        let (ancestors, _) = self.portfolio_ancestors_of(config)?;
        // A delegated empty list means inherit, even after the operator
        // removes its last ancestor. It must not turn into an unrestricted
        // root policy. This ancestry-only check is also used by storage
        // diagnostics; production admission first checks the resolved policy
        // in manager_launch_policy_gate, including independent PMs.
        if ancestors.is_empty() {
            let orphaned: bool = self.conn.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM manager_portfolio_appointments a
                    JOIN harness_manager_v2_policies p ON p.project_id=?1
                    WHERE a.target_ref='project:'||?1 AND a.state='appointed'
                      AND a.session_id=?2 AND a.scope_version=?3
                      AND p.manager_session_id=?2 AND p.scope_version=?3
                      AND json_array_length(p.policy_json,'$.allowed_launches')=0
                )",
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version
                ],
                |row| row.get(0),
            )?;
            if orphaned {
                return Err(refused("manager_v2_launch_not_granted"));
            }
        }
        for head in &ancestors {
            let grant = &head.record.grant;
            let granted =
                portfolio_effective_launches(&grant.allowed_launches, &grant.project_policy)
                    .iter()
                    .any(|l| {
                        l.provider == launch.provider
                            && l.model == launch.model
                            && l.effort == launch.effort
                    });
            if !granted {
                return Err(refused("manager_v2_launch_not_granted"));
            }
        }
        Ok(())
    }

    /// #1301: the portfolio node a V2 ledger principal acts for, if any.
    /// `session` is the principal's seat root (a lifecycle operation) or the
    /// requesting seat (a topology execution, possibly a context-cap
    /// successor); `version` is its scope version, a node's authority epoch:
    /// the `grant_version` of the grant that opened it. The principal is that
    /// grant's node when `session` seats a grant of the same node at or after
    /// the epoch. A pre-M1 epoch grant carries no node id (only the
    /// backfilled root's chain reaches that far back) and, like a project
    /// manager, area node or Epic lead, resolves to `None`.
    pub(crate) fn portfolio_origin_node(
        &self,
        session: &str,
        version: i64,
    ) -> Result<Option<Uuid>> {
        let node: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT g0.node_id FROM global_manager_grants g0 WHERE g0.grant_version=?2
                   AND EXISTS(SELECT 1 FROM global_manager_grants g WHERE g.seat_session_id=?1
                     AND g.grant_version>=g0.grant_version AND g.node_id IS g0.node_id)",
                params![session, version],
                |row| row.get(0),
            )
            .optional()?;
        node.flatten()
            .map(|id| {
                Uuid::parse_str(&id).map_err(|_| DaemonError::Store("invalid node id".into()))
            })
            .transpose()
    }

    /// #1301: `node` and the nodes above it, by each one's newest grant
    /// (active, else its last revoked one), so a revoked origin is still
    /// charged to the ancestors it had, never to a node that was below it.
    pub(crate) fn portfolio_lineage(&self, node: Uuid) -> Result<Vec<Uuid>> {
        let mut lineage = vec![node];
        let mut cursor = latest_node_grant_on(&self.conn, node)?.and_then(|r| r.parent_node_id);
        while let Some(parent) = cursor {
            if lineage.contains(&parent) || lineage.len() > PORTFOLIO_DEPTH_LIMIT {
                return Err(DaemonError::Store(
                    "portfolio parent chain is too deep or cyclic".into(),
                ));
            }
            lineage.push(parent);
            cursor = latest_node_grant_on(&self.conn, parent)?.and_then(|r| r.parent_node_id);
        }
        Ok(lineage)
    }

    /// Whether `anchor` is the storage principal (seat root) of any node in
    /// `project`'s chain (#1237: any depth).
    pub(crate) fn is_global_principal_anchor(&self, project: Uuid, anchor: Uuid) -> Result<bool> {
        Ok(self
            .portfolio_chain_heads(project)?
            .iter()
            .any(|head| head.seat_root == anchor))
    }

    /// #1235 rule (c), mutations flow down: `target` is owned by a principal
    /// `acting` may not mutate when it (or a parent below its Epic, or a
    /// rotation predecessor) is a seat of another active node's transfer
    /// chain, or was created by the ledger of a node above `acting` in the
    /// project's chain. #1236: the target's portfolio identity is resolved
    /// before the acting principal's, so a node never mutates the seat of a
    /// disjoint node hosted in its project. #1237: at any depth; a node is
    /// exempt only for its own seats and the sessions its own ledger or a
    /// descendant's created, never for an ancestor's or a sibling's.
    pub(crate) fn manager_target_owned_by_ancestor(
        &self,
        acting: &HarnessManagerConfigV1,
        target: Uuid,
    ) -> Result<bool> {
        let (ancestors, own) = self.portfolio_ancestors_of(acting)?;
        let acting_node = own.as_ref().map(|head| head.node_id);
        let ancestor_roots: Vec<String> = ancestors
            .iter()
            .map(|head| head.seat_root.to_string())
            .collect();
        let ancestor_roots = serde_json::to_string(&ancestor_roots)?;
        let mut cursor = Some(target);
        for _ in 0..OWNER_WALK_LIMIT {
            let Some(id) = cursor else { break };
            let mut lineage = Some(id);
            for _ in 0..OWNER_WALK_LIMIT {
                let Some(member) = lineage else { break };
                let created: bool = !ancestors.is_empty()
                    && self.conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_operations
                           WHERE project_id=?2 AND manager_session_id IN (SELECT value FROM json_each(?3))
                             AND kind='lifecycle_action' AND target_session_id=?1
                             AND json_extract(payload_json,'$.request.operation.action')='create_session')",
                        params![member.to_string(), acting.project_id.to_string(), ancestor_roots],
                        |row| row.get(0),
                    )?;
                if created
                    || chain_seat_node_on(&self.conn, member)?
                        .is_some_and(|node| Some(node) != acting_node)
                {
                    return Ok(true);
                }
                lineage = self.get_session(member)?.and_then(|row| row.continued_from);
            }
            let Some(row) = self.get_session(id)? else {
                break;
            };
            if !rsi_common::is_leaf_kind(row.session_kind) {
                break;
            }
            cursor = row.parent_id;
        }
        Ok(false)
    }
}

/// One active node of a project's chain (#1237).
#[derive(Debug, Clone)]
pub(crate) struct PortfolioHead {
    pub node_id: Uuid,
    pub epoch: i64,
    /// The V2 storage principal of the node in every covered project.
    pub seat_root: Uuid,
    /// `created_at` of the epoch's operator grant: budget counts start here.
    pub started: String,
    pub record: crate::store::portfolio_nodes::GrantRecord,
}

/// The manager principal one call resolves to: plan §2.3's arms in order.
#[derive(Debug, Clone)]
pub enum ManagerCallerV1 {
    /// The legacy appointment (`manager_config_for_caller`): the project's
    /// current manager (`is_manager`) or a managed Epic lead. Unchanged.
    Legacy {
        config: HarnessManagerConfigV1,
        is_manager: bool,
    },
    /// An area node (`authorize_area_node`). Unchanged.
    Area(ManagerAuthorityV2),
    /// The active global seat inside its grant (#1235).
    Global(ManagerAuthorityV2),
}

impl Store {
    /// `project` when it names a project other than the caller's own; only
    /// the global arm can serve such a target.
    fn manager_foreign_target(&self, caller: Uuid, project: Option<Uuid>) -> Result<Option<Uuid>> {
        let Some(project) = project else {
            return Ok(None);
        };
        let own = self
            .get_session(caller)?
            .and_then(|session| session.project_id);
        Ok((own != Some(project)).then_some(project))
    }

    /// Resolve the current (unfenced) principal of `caller` for a
    /// project-bound read or admission. `project: None` (or the caller's own
    /// project) keeps the legacy then area order, then tries the global arm
    /// for the caller's own project. Another project is served only by the
    /// global arm and is otherwise refused `manager_project_not_in_scope`.
    pub fn resolve_manager_caller(
        &self,
        caller: Uuid,
        project: Option<Uuid>,
    ) -> Result<ManagerCallerV1> {
        if let Some(project) = self.manager_foreign_target(caller, project)? {
            return self
                .global_manager_authority_current(caller, project)?
                .map(ManagerCallerV1::Global)
                .ok_or_else(|| refused(MANAGER_PROJECT_NOT_IN_SCOPE));
        }
        match self.manager_config_for_caller(caller) {
            Ok((config, is_manager)) => Ok(ManagerCallerV1::Legacy { config, is_manager }),
            Err(denial) => {
                if let Some(authority) = self.manager_area_authority_current(caller)? {
                    return Ok(ManagerCallerV1::Area(authority));
                }
                let own = self
                    .get_session(caller)?
                    .and_then(|session| session.project_id);
                if let Some(own) = own
                    && let Some(authority) =
                        uncovered_is_none(self.global_manager_authority_current(caller, own))?
                {
                    return Ok(ManagerCallerV1::Global(authority));
                }
                Err(denial)
            }
        }
    }

    /// [`Store::manager_v2_authorize`] with an optional target project: the
    /// legacy and area arms for the caller's own project, then the global arm.
    pub fn manager_v2_authorize_in(
        &self,
        caller: Uuid,
        project: Option<Uuid>,
        fence: &ManagerFenceV2,
        capability: Option<ManagerCapabilityV2>,
    ) -> Result<ManagerAuthorityV2> {
        fence.validate().map_err(refused)?;
        if let Some(project) = self.manager_foreign_target(caller, project)? {
            return self
                .global_manager_authorize(caller, project, fence, capability)?
                .ok_or_else(|| refused(MANAGER_PROJECT_NOT_IN_SCOPE));
        }
        match self.manager_v2_authorize_own(caller, fence, capability) {
            Ok(authority) => Ok(authority),
            Err(denial) => {
                let own = self
                    .get_session(caller)?
                    .and_then(|session| session.project_id);
                let Some(own) = own else {
                    return Err(denial);
                };
                uncovered_is_none(self.global_manager_authorize(caller, own, fence, capability))?
                    .ok_or(denial)
            }
        }
    }
}
