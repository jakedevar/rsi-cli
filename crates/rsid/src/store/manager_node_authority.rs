//! Admission-time resolution for area-node manager principals.
//!
//! The legacy appointment remains the V1 compatibility path. Area nodes use
//! their immutable seat-root ID as the legacy V2 storage principal (those
//! tables still reference `sessions`) and their authority epoch as the V2
//! scope fence. The grant itself retains the stable node ID. This resolver is
//! called again at effect time,
//! so a grant, seat lineage, or selected project area change invalidates an
//! admitted operation before it can take effect.

use chrono::Utc;
use rsi_common::harness_manager::{HarnessManagerConfigV1, HarnessManagerScopeModeV1};
use rsi_common::harness_manager_v2::{
    HarnessManagerPolicyConfigV2, ManagerCapabilityV2, ManagerFenceV2, ManagerPolicyV2,
};
use rsi_common::manager_nodes::{ManagerNodeGrantV1, ManagerNodeSelectorV1};
use rsi_common::types::{SessionKind, SessionStatus};
use rusqlite::params;
use uuid::Uuid;

use super::{ManagerAuthorityV2, Store, refused};
use crate::error::Result;

type NodeAuthorityRow = (
    String,
    String,
    i64,
    i64,
    i64,
    String,
    String,
    Option<String>,
    Option<String>,
);

pub(super) fn authorize_area_node(
    store: &Store,
    caller: Uuid,
    fence: &ManagerFenceV2,
    capability: Option<ManagerCapabilityV2>,
) -> Result<Option<ManagerAuthorityV2>> {
    let Ok(tip) = store.manager_lineage_tip(caller) else {
        return Ok(None);
    };
    let Some((node_id, project_id, row)) = node_for_seat(store, tip)? else {
        return Ok(None);
    };
    // A retired predecessor still resolves to the successor's lineage tip.
    // Its own transport token must never borrow that successor's grant.
    if caller != tip {
        return Err(refused("manager_node_custody_changed"));
    }
    let node_id = parse_uuid(&node_id)?;
    let project_id = parse_uuid(&project_id)?;
    let seat_id = parse_uuid(&row.0)?;
    let state = &row.1;
    if state != "active"
        || row.6 != "granted"
        || fence.scope_version != row.4
        || fence.policy_version != row.3
    {
        return Err(refused("manager_node_authority_changed"));
    }

    let seat = store
        .get_session(tip)?
        .ok_or_else(|| refused("manager_node_seat_unavailable"))?;
    if seat.project_id != Some(project_id) {
        return Err(refused("manager_node_project_denied"));
    }
    if seat.session_kind != SessionKind::Standard
        || matches!(
            seat.status,
            SessionStatus::Archived | SessionStatus::Deleted
        )
        || store.manager_lineage_tip(seat_id)? != tip
    {
        return Err(refused("manager_node_custody_changed"));
    }

    let root_config = store
        .get_harness_manager(project_id)?
        .ok_or_else(|| refused("manager_node_project_denied"))?;
    if root_config.current_session_id.is_none() || root_config.is_revoked() {
        return Err(refused("manager_node_parent_authority_changed"));
    }
    let ancestry_active: bool = store.conn.query_row(
        "WITH RECURSIVE ancestry(id,parent_node_id,state,legacy_project_id,grant_state,scope_current) AS (
             SELECT n.id,n.parent_node_id,n.state,n.legacy_project_id,g.state,
                    EXISTS(SELECT 1 FROM manager_node_scopes s WHERE s.node_id=n.id AND s.project_id=?2 AND s.grant_version=n.grant_version)
             FROM manager_nodes n JOIN manager_node_grants g ON g.node_id=n.id AND g.grant_version=n.grant_version
             WHERE n.id=?1
             UNION ALL
             SELECT p.id,p.parent_node_id,p.state,p.legacy_project_id,g.state,
                    EXISTS(SELECT 1 FROM manager_node_scopes s WHERE s.node_id=p.id AND s.project_id=?2 AND s.grant_version=p.grant_version)
             FROM manager_nodes p JOIN manager_node_grants g ON g.node_id=p.id AND g.grant_version=p.grant_version
             JOIN ancestry c ON c.parent_node_id=p.id
         )
         SELECT count(*)>0 AND min(state='active') AND min(grant_state='granted') AND min(scope_current) AND
                (SELECT legacy_project_id FROM ancestry WHERE parent_node_id IS NULL)=?2
         FROM ancestry",
        params![node_id.to_string(), project_id.to_string()],
        |row| row.get(0),
    )?;
    if !ancestry_active {
        return Err(refused("manager_node_parent_authority_changed"));
    }

    let grant: ManagerNodeGrantV1 = serde_json::from_str(
        row.7
            .as_deref()
            .ok_or_else(|| refused("manager_node_grant_required"))?,
    )
    .map_err(|_| refused("manager_node_invalid_stored_grant"))?;
    let policy: ManagerPolicyV2 = serde_json::from_str(
        row.8
            .as_deref()
            .ok_or_else(|| refused("manager_node_grant_required"))?,
    )
    .map_err(|_| refused("manager_node_invalid_stored_policy"))?;
    if !grant_matches_policy(&grant, &policy) {
        return Err(refused("manager_node_invalid_stored_grant"));
    }
    if capability.is_some_and(|required| !grant.capabilities.contains(&required)) {
        return Err(refused("manager_v2_capability_denied"));
    }

    let selector: ManagerNodeSelectorV1 = serde_json::from_str(&row.5)
        .map_err(|_| refused("manager_node_invalid_stored_selector"))?;
    let epic_ids = live_selected_epics(store, project_id, &selector)?;
    let mut config: HarnessManagerConfigV1 = root_config;
    // Existing V2 record/event/operation tables require a Session FK. The
    // root of this node's seat lineage is stable across succession; Work facts
    // retain their independent (project, kind, record_key) identity.
    config.manager_session_id = seat_id;
    config.current_session_id = Some(tip);
    config.epic_ids = epic_ids.clone();
    config.selected_epic_ids = Some(epic_ids);
    config.group_ids = match &selector {
        ManagerNodeSelectorV1::Project => Vec::new(),
        ManagerNodeSelectorV1::Selected { group_ids, .. } => group_ids.clone(),
    };
    config.scope_mode = if matches!(selector, ManagerNodeSelectorV1::Project) {
        HarnessManagerScopeModeV1::Project
    } else {
        HarnessManagerScopeModeV1::Selected
    };
    config.row_version = row.4;
    let direct_epics = store.manager_direct_epics(&config, seat_id)?;
    config.epic_ids = direct_epics.clone();
    config.selected_epic_ids = Some(direct_epics);

    Ok(Some(ManagerAuthorityV2 {
        config,
        grant: HarnessManagerPolicyConfigV2 {
            project_id,
            manager_session_id: node_id,
            scope_version: row.4,
            row_version: row.3,
            policy,
            updated_at: Utc::now(),
            revoked: false,
        },
        caller,
        is_manager: true,
    }))
}

impl Store {
    /// Resolve the current area grant for read surfaces. The same effect-time
    /// checks used by mutations reject retired seats and changed ancestry.
    pub(crate) fn manager_area_authority_current(
        &self,
        caller: Uuid,
    ) -> Result<Option<ManagerAuthorityV2>> {
        let Ok(tip) = self.manager_lineage_tip(caller) else {
            return Ok(None);
        };
        let Some((_, _, row)) = node_for_seat(self, tip)? else {
            return Ok(None);
        };
        authorize_area_node(
            self,
            caller,
            &ManagerFenceV2 {
                scope_version: row.4,
                policy_version: row.3,
            },
            None,
        )
    }

    /// The V2 policy grant that governs `config`. The project's root manager
    /// reads the project policy row exactly as before recursive nodes existed:
    /// absent (`None`) or anchored to a revoked scope (`revoked`) is for the
    /// caller to judge. Only an area node's config resolves through its live
    /// node grant, and a changed node authority refuses.
    pub(crate) fn manager_policy_for_config(
        &self,
        config: &HarnessManagerConfigV1,
    ) -> Result<Option<HarnessManagerPolicyConfigV2>> {
        let root = self.get_harness_manager_policy(config.project_id)?;
        let is_root_manager = self
            .get_harness_manager(config.project_id)?
            .is_some_and(|current| current.manager_session_id == config.manager_session_id);
        if is_root_manager
            || root
                .as_ref()
                .is_some_and(|grant| grant.manager_session_id == config.manager_session_id)
        {
            return Ok(root);
        }
        let caller = config
            .current_session_id
            .ok_or_else(|| refused("manager_node_seat_unavailable"))?;
        let authority = self
            .manager_area_authority_current(caller)?
            .ok_or_else(|| refused("manager_node_authority_changed"))?;
        if authority.config.manager_session_id != config.manager_session_id
            || authority.config.row_version != config.row_version
        {
            return Err(refused("manager_node_authority_changed"));
        }
        Ok(Some(authority.grant))
    }
}

fn node_for_seat(store: &Store, tip: Uuid) -> Result<Option<(String, String, NodeAuthorityRow)>> {
    let candidates: Vec<(String, String, NodeAuthorityRow)> = {
        let mut statement = store.conn.prepare(
            "SELECT n.id,s.project_id,n.seat_root_session_id,n.state,n.grant_version,
                    n.policy_version,n.authority_epoch,s.selector_json,g.state,
                    g.grant_json,g.policy_json
             FROM manager_nodes n
             JOIN manager_node_scopes s ON s.node_id=n.id
             JOIN manager_node_grants g ON g.node_id=n.id AND g.grant_version=n.grant_version
             WHERE n.parent_node_id IS NOT NULL AND n.state='active'
               AND s.grant_version=n.grant_version AND g.state='granted'
             ORDER BY n.id",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    (
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                        row.get(10)?,
                    ),
                ))
            })?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut matches = Vec::new();
    for candidate in candidates {
        if store.manager_lineage_tip(parse_uuid(&candidate.2.0)?).ok() == Some(tip) {
            matches.push(candidate);
        }
    }
    if matches.len() > 1 {
        return Err(refused("manager_node_seat_ambiguous"));
    }
    Ok(matches.pop())
}

fn live_selected_epics(
    store: &Store,
    project: Uuid,
    selector: &ManagerNodeSelectorV1,
) -> Result<Vec<Uuid>> {
    let mut statement = store.conn.prepare(
        "SELECT id,parent_id FROM sessions
         WHERE project_id=?1 AND session_kind='Epic' AND status NOT IN ('Archived','Deleted')
         ORDER BY id",
    )?;
    let rows = statement.query_map([project.to_string()], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })?;
    let mut epics = Vec::new();
    for row in rows {
        let (epic, group) = row?;
        let epic = parse_uuid(&epic)?;
        let group = group.map(|id| parse_uuid(&id)).transpose()?;
        if selector.covers_epic(epic, group) {
            epics.push(epic);
        }
    }
    Ok(epics)
}

fn grant_matches_policy(grant: &ManagerNodeGrantV1, policy: &ManagerPolicyV2) -> bool {
    grant.validate().is_ok()
        && policy.validate().is_ok()
        && grant.capabilities.len() == policy.capabilities.len()
        && grant
            .capabilities
            .iter()
            .all(|capability| policy.capabilities.contains(capability))
        && grant.allowed_launches.len() == policy.allowed_launches.len()
        && grant
            .allowed_launches
            .iter()
            .all(|choice| policy.allowed_launches.contains(choice))
        && grant.allowance.max_created_containers == policy.max_created_containers
        && grant.allowance.max_created_sessions == policy.max_created_sessions
        && grant.allowance.max_active_sessions == policy.max_active_sessions
        && grant.allowance.max_spend_usd == policy.max_spend_usd
        && grant.allowance.provider_limits.len() == policy.provider_limits.len()
        && grant
            .allowance
            .provider_limits
            .iter()
            .all(|limit| policy.provider_limits.contains(limit))
}

fn parse_uuid(value: &str) -> Result<Uuid> {
    Uuid::parse_str(value).map_err(|_| refused("manager_node_invalid_stored_identity"))
}
