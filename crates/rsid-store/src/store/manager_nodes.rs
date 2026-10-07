//! Stable recursive manager principals. V<next> is additive: the legacy
//! project appointment remains the root compatibility projection until all
//! authority and mail paths have moved to node identity.

use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::harness_manager_v2::ManagerPolicyV2;
use rsi_common::manager_nodes::{
    MANAGER_NODE_DEFAULT_MAX_DIRECT_REPORTS, ManagerNodeAllowanceV1, ManagerNodeGrantV1,
    ManagerNodeSelectorV1,
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use uuid::Uuid;

use super::Store;
use crate::error::{DaemonError, Result};

// The provisional number is assigned at landing.
// RSI-RELEASED-MIGRATION-BEGIN: manager-nodes-migration
pub(super) const MANAGER_NODE_SCHEMA_VERSION: i32 = 138;

pub(super) fn apply_manager_node_migration(store: &Store) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != MANAGER_NODE_SCHEMA_VERSION - 1 {
        return Err(DaemonError::Store(format!(
            "V{MANAGER_NODE_SCHEMA_VERSION} requires V{}, found V{version}",
            MANAGER_NODE_SCHEMA_VERSION - 1
        )));
    }
    tx.execute_batch(
        "CREATE TABLE manager_nodes (
            id TEXT PRIMARY KEY NOT NULL,
            parent_node_id TEXT REFERENCES manager_nodes(id),
            legacy_project_id TEXT REFERENCES projects(id),
            seat_root_session_id TEXT NOT NULL REFERENCES sessions(id),
            state TEXT NOT NULL CHECK(state IN ('active','revoked')),
            grant_version INTEGER NOT NULL CHECK(grant_version>0),
            policy_version INTEGER NOT NULL CHECK(policy_version>=0),
            authority_epoch INTEGER NOT NULL CHECK(authority_epoch>0),
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            CHECK(parent_node_id IS NULL OR parent_node_id<>id),
            CHECK(legacy_project_id IS NULL OR parent_node_id IS NULL)
        );
        CREATE INDEX manager_nodes_parent ON manager_nodes(parent_node_id,state,id);
        CREATE UNIQUE INDEX manager_nodes_legacy_project
            ON manager_nodes(legacy_project_id) WHERE legacy_project_id IS NOT NULL;
        CREATE UNIQUE INDEX manager_nodes_active_seat
            ON manager_nodes(seat_root_session_id) WHERE state='active';
        CREATE TRIGGER manager_nodes_parent_immutable
            BEFORE UPDATE OF parent_node_id ON manager_nodes
            BEGIN SELECT RAISE(ABORT,'manager node parent is immutable'); END;
        CREATE TRIGGER manager_nodes_no_delete
            BEFORE DELETE ON manager_nodes
            BEGIN SELECT RAISE(ABORT,'manager nodes are retained for audit'); END;

        CREATE TABLE manager_node_scopes (
            node_id TEXT NOT NULL REFERENCES manager_nodes(id),
            project_id TEXT NOT NULL REFERENCES projects(id),
            selector_json TEXT NOT NULL CHECK(json_valid(selector_json) AND length(selector_json)<=8192),
            grant_version INTEGER NOT NULL CHECK(grant_version>0),
            PRIMARY KEY(node_id,project_id)
        );
        CREATE INDEX manager_node_scopes_project
            ON manager_node_scopes(project_id,node_id);

        CREATE TABLE manager_node_grants (
            node_id TEXT NOT NULL REFERENCES manager_nodes(id),
            grant_version INTEGER NOT NULL CHECK(grant_version>0),
            state TEXT NOT NULL CHECK(state IN ('absent','granted','revoked')),
            grant_json TEXT CHECK(grant_json IS NULL OR (json_valid(grant_json) AND length(grant_json)<=32768)),
            policy_json TEXT CHECK(policy_json IS NULL OR (json_valid(policy_json) AND length(policy_json)<=32768)),
            operator_origin TEXT NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY(node_id,grant_version),
            CHECK((state='granted')=(grant_json IS NOT NULL AND policy_json IS NOT NULL))
        );
        CREATE TRIGGER manager_node_grants_no_update
            BEFORE UPDATE ON manager_node_grants
            BEGIN SELECT RAISE(ABORT,'manager node grants are immutable'); END;
        CREATE TRIGGER manager_node_grants_no_delete
            BEFORE DELETE ON manager_node_grants
            BEGIN SELECT RAISE(ABORT,'manager node grants are retained for audit'); END;

        CREATE TABLE manager_node_reservations (
            parent_node_id TEXT NOT NULL REFERENCES manager_nodes(id),
            child_node_id TEXT NOT NULL REFERENCES manager_nodes(id),
            resource_kind TEXT NOT NULL,
            amount INTEGER NOT NULL CHECK(amount>=0),
            grant_version INTEGER NOT NULL CHECK(grant_version>0),
            state TEXT NOT NULL CHECK(state IN ('active','released')),
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY(parent_node_id,child_node_id,resource_kind,grant_version)
        );
        CREATE INDEX manager_node_reservations_active
            ON manager_node_reservations(parent_node_id,resource_kind)
            WHERE state='active';
        CREATE TABLE manager_node_operations (
            project_id TEXT NOT NULL REFERENCES projects(id),
            idempotency_key TEXT NOT NULL,
            request_hash TEXT NOT NULL,
            response_json TEXT NOT NULL CHECK(json_valid(response_json)),
            created_at TEXT NOT NULL,
            PRIMARY KEY(project_id,idempotency_key)
        );
        CREATE TABLE manager_node_escalations (
            id TEXT PRIMARY KEY NOT NULL,
            project_id TEXT NOT NULL REFERENCES projects(id),
            subject_id TEXT NOT NULL,
            source_node_id TEXT NOT NULL REFERENCES manager_nodes(id),
            target_node_id TEXT NOT NULL REFERENCES manager_nodes(id),
            reason TEXT NOT NULL CHECK(length(reason)>0 AND length(reason)<=8192),
            source_authority_epoch INTEGER NOT NULL CHECK(source_authority_epoch>0),
            source_grant_version INTEGER NOT NULL CHECK(source_grant_version>0),
            target_authority_epoch INTEGER NOT NULL CHECK(target_authority_epoch>0),
            target_grant_version INTEGER NOT NULL CHECK(target_grant_version>0),
            target_session_id TEXT NOT NULL REFERENCES sessions(id),
            idempotency_key TEXT NOT NULL,
            version INTEGER NOT NULL CHECK(version>0),
            state TEXT NOT NULL CHECK(state IN ('open','ruled')),
            ruling TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            CHECK((state='ruled')=(ruling IS NOT NULL))
        );
        CREATE UNIQUE INDEX manager_node_escalations_one_open_subject
            ON manager_node_escalations(project_id,subject_id) WHERE state='open';
        CREATE INDEX manager_node_escalations_addressed
            ON manager_node_escalations(target_node_id,state,created_at,id);
        CREATE TRIGGER manager_node_escalations_no_delete
            BEFORE DELETE ON manager_node_escalations
            BEGIN SELECT RAISE(ABORT,'manager node escalations are retained for audit'); END;
        CREATE TABLE manager_node_escalation_events (
            escalation_id TEXT NOT NULL REFERENCES manager_node_escalations(id),
            version INTEGER NOT NULL CHECK(version>0),
            actor_node_id TEXT NOT NULL REFERENCES manager_nodes(id),
            target_node_id TEXT NOT NULL REFERENCES manager_nodes(id),
            action TEXT NOT NULL CHECK(action IN ('created','forwarded','ruled')),
            ruling TEXT,
            created_at TEXT NOT NULL,
            PRIMARY KEY(escalation_id,version)
        );
        CREATE TRIGGER manager_node_escalation_events_no_update
            BEFORE UPDATE ON manager_node_escalation_events
            BEGIN SELECT RAISE(ABORT,'manager node escalation events are immutable'); END;
        CREATE TRIGGER manager_node_escalation_events_no_delete
            BEFORE DELETE ON manager_node_escalation_events
            BEGIN SELECT RAISE(ABORT,'manager node escalation events are retained for audit'); END;
        CREATE INDEX manager_agent_mail_authority_lookup
            ON harness_manager_v2_events(kind,record_key)
            WHERE kind='agent_mail_authority';
        CREATE TRIGGER manager_node_epic_insert_overlap
        BEFORE INSERT ON sessions WHEN NEW.session_kind='Epic' AND NEW.project_id IS NOT NULL
        BEGIN
            SELECT RAISE(ABORT,'manager_scope_overlap') WHERE EXISTS (
                SELECT 1 FROM manager_nodes a
                JOIN manager_node_scopes sa ON sa.node_id=a.id AND sa.project_id=NEW.project_id
                JOIN manager_nodes b ON b.parent_node_id=a.parent_node_id AND b.id>a.id AND b.state='active'
                JOIN manager_node_scopes sb ON sb.node_id=b.id AND sb.project_id=NEW.project_id
                WHERE a.state='active' AND a.parent_node_id IS NOT NULL
                  AND (json_extract(sa.selector_json,'$.mode')='project'
                       OR EXISTS(SELECT 1 FROM json_each(sa.selector_json,'$.group_ids') WHERE value=NEW.parent_id)
                       OR EXISTS(SELECT 1 FROM json_each(sa.selector_json,'$.epic_ids') WHERE value=NEW.id))
                  AND (json_extract(sb.selector_json,'$.mode')='project'
                       OR EXISTS(SELECT 1 FROM json_each(sb.selector_json,'$.group_ids') WHERE value=NEW.parent_id)
                       OR EXISTS(SELECT 1 FROM json_each(sb.selector_json,'$.epic_ids') WHERE value=NEW.id))
            );
        END;
        CREATE TRIGGER manager_node_epic_update_overlap
        BEFORE UPDATE OF parent_id,project_id,session_kind ON sessions
        WHEN NEW.session_kind='Epic' AND NEW.project_id IS NOT NULL
        BEGIN
            SELECT RAISE(ABORT,'manager_scope_overlap') WHERE EXISTS (
                SELECT 1 FROM manager_nodes a
                JOIN manager_node_scopes sa ON sa.node_id=a.id AND sa.project_id=NEW.project_id
                JOIN manager_nodes b ON b.parent_node_id=a.parent_node_id AND b.id>a.id AND b.state='active'
                JOIN manager_node_scopes sb ON sb.node_id=b.id AND sb.project_id=NEW.project_id
                WHERE a.state='active' AND a.parent_node_id IS NOT NULL
                  AND (json_extract(sa.selector_json,'$.mode')='project'
                       OR EXISTS(SELECT 1 FROM json_each(sa.selector_json,'$.group_ids') WHERE value=NEW.parent_id)
                       OR EXISTS(SELECT 1 FROM json_each(sa.selector_json,'$.epic_ids') WHERE value=NEW.id))
                  AND (json_extract(sb.selector_json,'$.mode')='project'
                       OR EXISTS(SELECT 1 FROM json_each(sb.selector_json,'$.group_ids') WHERE value=NEW.parent_id)
                       OR EXISTS(SELECT 1 FROM json_each(sb.selector_json,'$.epic_ids') WHERE value=NEW.id))
            );
        END;",
    )?;
    backfill_legacy_roots(store, &tx)?;
    let source_count: i64 =
        tx.query_row("SELECT count(*) FROM harness_manager_scopes", [], |row| {
            row.get(0)
        })?;
    let node_count: i64 = tx.query_row(
        "SELECT count(*) FROM manager_nodes WHERE legacy_project_id IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    if source_count != node_count {
        return Err(DaemonError::Store(
            "manager node root backfill count mismatch".into(),
        ));
    }
    tx.execute(
        &format!("PRAGMA user_version = {MANAGER_NODE_SCHEMA_VERSION}"),
        [],
    )?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: manager-nodes-migration

// RSI-RELEASED-MIGRATION-BEGIN: manager-nodes-backfill
type LegacyScope = (String, String, String, i64, String, String);

fn backfill_legacy_roots(_store: &Store, tx: &Transaction<'_>) -> Result<()> {
    let scopes: Vec<LegacyScope> = {
        let mut statement = tx.prepare(
            "SELECT project_id,manager_session_id,scope_mode,row_version,epic_ids_json,group_ids_json
             FROM harness_manager_scopes ORDER BY project_id",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?
    };
    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    for (project, seat, mode, scope_version, epics_json, groups_json) in scopes {
        let project_id = canonical_uuid(&project)?;
        let seat_id = canonical_uuid(&seat)?;
        let existing: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM manager_nodes WHERE legacy_project_id=?1)",
            params![project],
            |row| row.get(0),
        )?;
        if existing {
            continue;
        }
        let epic_ids: Vec<Uuid> = serde_json::from_str(&epics_json)?;
        let group_ids: Vec<Uuid> = serde_json::from_str(&groups_json)?;
        let revoked = mode == "selected" && epic_ids.is_empty() && group_ids.is_empty();
        let selector = match mode.as_str() {
            "project" if !revoked => ManagerNodeSelectorV1::Project,
            "selected" => ManagerNodeSelectorV1::Selected {
                group_ids,
                epic_ids,
            },
            _ => {
                return Err(DaemonError::Store(
                    "invalid legacy manager scope mode".into(),
                ));
            }
        };
        if !revoked {
            selector
                .validate()
                .map_err(|error| DaemonError::Store(error.into()))?;
        }
        // Historical appointments can point at a deleted, forked, or cyclic
        // lineage. Preserve their projection but never restore authority.
        let lineage_valid = if revoked {
            false
        } else {
            match super::harness_manager::manager_lineage_tip_on(tx, seat_id) {
                Ok(_) => true,
                Err(DaemonError::InvalidParam(_)) => false,
                Err(error) => return Err(error),
            }
        };
        let policy: Option<(String, i64, i64, String)> = tx
            .query_row(
                "SELECT manager_session_id,scope_version,row_version,policy_json
                 FROM harness_manager_v2_policies WHERE project_id=?1",
                params![project],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let current_policy = policy.filter(|(policy_seat, policy_scope, _, _)| {
            policy_seat == &seat && *policy_scope == scope_version && lineage_valid
        });
        let (grant_state, policy_version, grant_json, policy_json) =
            if let Some((_, _, version, json)) = current_policy {
                let policy: ManagerPolicyV2 = serde_json::from_str(&json)?;
                policy
                    .validate()
                    .map_err(|error| DaemonError::Store(error.into()))?;
                let grant = ManagerNodeGrantV1 {
                    capabilities: policy.capabilities.clone(),
                    allowed_launches: policy.allowed_launches.clone(),
                    allowance: ManagerNodeAllowanceV1 {
                        max_created_containers: policy.max_created_containers,
                        max_created_sessions: policy.max_created_sessions,
                        max_active_sessions: policy.max_active_sessions,
                        max_build_slots: 0,
                        max_disk_gib: 0,
                        provider_limits: policy.provider_limits.clone(),
                        max_spend_usd: policy.max_spend_usd,
                    },
                    max_direct_reports: MANAGER_NODE_DEFAULT_MAX_DIRECT_REPORTS,
                };
                grant
                    .validate()
                    .map_err(|error| DaemonError::Store(error.into()))?;
                (
                    "granted",
                    version,
                    Some(serde_json::to_string(&grant)?),
                    Some(json),
                )
            } else {
                (if revoked { "revoked" } else { "absent" }, 0, None, None)
            };
        let node_id = Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO manager_nodes(id,parent_node_id,legacy_project_id,seat_root_session_id,state,grant_version,policy_version,authority_epoch,created_at,updated_at)
             VALUES(?1,NULL,?2,?3,?4,1,?5,1,?6,?6)",
            params![node_id, project, seat, if revoked { "revoked" } else { "active" }, policy_version, timestamp],
        )?;
        if !revoked {
            tx.execute(
                "INSERT INTO manager_node_scopes(node_id,project_id,selector_json,grant_version) VALUES(?1,?2,?3,1)",
                params![node_id, project_id.to_string(), serde_json::to_string(&selector)?],
            )?;
        }
        tx.execute(
            "INSERT INTO manager_node_grants(node_id,grant_version,state,grant_json,policy_json,operator_origin,created_at)
             VALUES(?1,1,?2,?3,?4,'legacy_backfill',?5)",
            params![node_id, grant_state, grant_json, policy_json, timestamp],
        )?;
    }
    Ok(())
}

pub(super) fn canonical_uuid(value: &str) -> Result<Uuid> {
    let id = Uuid::parse_str(value)
        .map_err(|_| DaemonError::Store("invalid legacy manager identity".into()))?;
    if id.is_nil() || id.to_string() != value {
        return Err(DaemonError::Store("invalid legacy manager identity".into()));
    }
    Ok(id)
}
// RSI-RELEASED-MIGRATION-END: manager-nodes-backfill

pub(super) fn node_refused(code: &'static str) -> DaemonError {
    DaemonError::InvalidParam(code.into())
}

/// The versions are observations of the parent, not authority supplied by the caller.
#[derive(Debug, Clone, Serialize)]
pub struct AppointAreaNode {
    pub idempotency_key: Option<String>,
    pub node_id: Option<Uuid>,
    pub expected_node_grant_version: i64,
    pub project_id: Uuid,
    pub parent_node_id: Uuid,
    pub expected_parent_grant_version: i64,
    pub expected_parent_policy_version: i64,
    pub expected_parent_epoch: i64,
    pub seat_root_session_id: Uuid,
    pub selector: ManagerNodeSelectorV1,
    pub grant: ManagerNodeGrantV1,
    pub policy: ManagerPolicyV2,
    pub operator_origin: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RevokeAreaNode {
    pub idempotency_key: Option<String>,
    pub project_id: Uuid,
    pub node_id: Uuid,
    pub expected_grant_version: i64,
    pub expected_epoch: i64,
    pub operator_origin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AreaNode {
    pub id: Uuid,
    pub parent_node_id: Option<Uuid>,
    pub project_id: Uuid,
    pub seat_root_session_id: Uuid,
    pub active: bool,
    pub grant_version: i64,
    pub policy_version: i64,
    pub authority_epoch: i64,
    pub selector: Option<ManagerNodeSelectorV1>,
    pub grant: Option<ManagerNodeGrantV1>,
    pub policy: Option<ManagerPolicyV2>,
    pub direct_reports: u16,
    pub updated_at: DateTime<Utc>,
}

pub(super) fn area_node_on(
    tx: &Transaction<'_>,
    project: Uuid,
    id: Uuid,
) -> Result<Option<AreaNode>> {
    let row: Option<(Option<String>, String, String, i64, i64, i64, Option<String>, Option<String>, Option<String>, Option<String>, i64, String)> = tx.query_row(
        "SELECT n.parent_node_id,n.seat_root_session_id,n.state,n.grant_version,n.policy_version,n.authority_epoch,
                s.selector_json,g.state,g.grant_json,g.policy_json,
                (SELECT count(*) FROM manager_nodes child WHERE child.parent_node_id=n.id AND child.state='active'),n.updated_at
         FROM manager_nodes n LEFT JOIN manager_node_scopes s ON s.node_id=n.id AND s.project_id=?2
         JOIN manager_node_grants g ON g.node_id=n.id AND g.grant_version=n.grant_version
         WHERE n.id=?1 AND (s.project_id IS NOT NULL OR n.legacy_project_id=?2)",
        params![id.to_string(),project.to_string()],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,r.get(10)?,r.get(11)?)),
    ).optional()?;
    row.map(
        |(
            parent,
            seat,
            state,
            grant_version,
            policy_version,
            authority_epoch,
            selector,
            grant_state,
            grant,
            policy,
            direct_reports,
            updated_at,
        )| {
            Ok(AreaNode {
                id,
                parent_node_id: parent.map(|s| canonical_uuid(&s)).transpose()?,
                project_id: project,
                seat_root_session_id: canonical_uuid(&seat)?,
                active: state == "active",
                grant_version,
                policy_version,
                authority_epoch,
                selector: selector.map(|s| serde_json::from_str(&s)).transpose()?,
                grant: if grant_state.as_deref() == Some("granted") {
                    grant.map(|s| serde_json::from_str(&s)).transpose()?
                } else {
                    None
                },
                policy: if grant_state.as_deref() == Some("granted") {
                    policy.map(|s| serde_json::from_str(&s)).transpose()?
                } else {
                    None
                },
                direct_reports: u16::try_from(direct_reports).map_err(|_| {
                    DaemonError::Store("manager node direct report count overflow".into())
                })?,
                updated_at: DateTime::parse_from_rfc3339(&updated_at)
                    .map_err(|_| DaemonError::Store("invalid manager node timestamp".into()))?
                    .with_timezone(&Utc),
            })
        },
    )
    .transpose()
}

fn root_id_on(tx: &Transaction<'_>, project: Uuid) -> Result<Option<Uuid>> {
    tx.query_row(
        "SELECT id FROM manager_nodes WHERE legacy_project_id=?1",
        [project.to_string()],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(|s| canonical_uuid(&s))
    .transpose()
}

fn previous_direct_report_cap_on(tx: &Transaction<'_>, root: Uuid) -> Result<u16> {
    let json: Option<String> = tx.query_row(
        "SELECT grant_json FROM manager_node_grants WHERE node_id=?1 AND state='granted' ORDER BY grant_version DESC LIMIT 1",
        [root.to_string()], |r| r.get(0),
    ).optional()?;
    Ok(json
        .map(|value| serde_json::from_str::<ManagerNodeGrantV1>(&value))
        .transpose()?
        .map_or(MANAGER_NODE_DEFAULT_MAX_DIRECT_REPORTS, |grant| {
            grant.max_direct_reports
        }))
}

pub(super) fn live_topology(
    tx: &Transaction<'_>,
    project: Uuid,
) -> Result<(HashSet<Uuid>, Vec<(Uuid, Option<Uuid>)>)> {
    let mut groups = HashSet::new();
    let mut stmt = tx.prepare("SELECT id FROM sessions WHERE project_id=?1 AND session_kind='Group' AND parent_id IS NULL AND status NOT IN ('Archived','Deleted')")?;
    for id in stmt.query_map([project.to_string()], |r| r.get::<_, String>(0))? {
        groups.insert(canonical_uuid(&id?)?);
    }
    let mut epics = Vec::new();
    let mut stmt = tx.prepare("SELECT e.id,e.parent_id FROM sessions e WHERE e.project_id=?1 AND e.session_kind='Epic' AND e.status NOT IN ('Archived','Deleted')")?;
    for row in stmt.query_map([project.to_string()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })? {
        let (id, parent) = row?;
        let group = parent
            .map(|s| canonical_uuid(&s))
            .transpose()?
            .filter(|g| groups.contains(g));
        if group.is_some() {
            epics.push((canonical_uuid(&id)?, group));
        }
    }
    Ok((groups, epics))
}

fn checked_selector(
    selector: &ManagerNodeSelectorV1,
    groups: &HashSet<Uuid>,
    epics: &[(Uuid, Option<Uuid>)],
) -> Result<()> {
    selector.validate().map_err(node_refused)?;
    if let ManagerNodeSelectorV1::Selected {
        group_ids,
        epic_ids,
    } = selector
    {
        if group_ids.iter().any(|id| !groups.contains(id))
            || epic_ids
                .iter()
                .any(|id| !epics.iter().any(|(epic, _)| epic == id))
        {
            return Err(node_refused("manager_node_invalid_selector_target"));
        }
    }
    Ok(())
}

fn strict_child_selector(
    child: &ManagerNodeSelectorV1,
    parent: &ManagerNodeSelectorV1,
    epics: &[(Uuid, Option<Uuid>)],
) -> bool {
    match (child, parent) {
        (ManagerNodeSelectorV1::Selected { .. }, ManagerNodeSelectorV1::Project) => true,
        (
            ManagerNodeSelectorV1::Selected {
                group_ids,
                epic_ids,
            },
            ManagerNodeSelectorV1::Selected {
                group_ids: pg,
                epic_ids: pe,
            },
        ) => {
            group_ids.iter().all(|g| pg.contains(g))
                && epic_ids.iter().all(|e| {
                    epics.iter().any(|(id, group)| {
                        id == e && (pe.contains(e) || group.is_some_and(|g| pg.contains(&g)))
                    })
                })
                && (pg.iter().any(|g| !group_ids.contains(g))
                    || pe.iter().any(|e| !epic_ids.contains(e)))
        }
        _ => false,
    }
}

fn selectors_overlap(
    a: &ManagerNodeSelectorV1,
    b: &ManagerNodeSelectorV1,
    epics: &[(Uuid, Option<Uuid>)],
) -> bool {
    if a.overlaps_on_live_epics(b, epics) {
        return true;
    }
    match (a, b) {
        (
            ManagerNodeSelectorV1::Selected { group_ids: ag, .. },
            ManagerNodeSelectorV1::Selected { group_ids: bg, .. },
        ) => ag.iter().any(|g| bg.contains(g)),
        _ => true,
    }
}

fn policy_matches_grant(policy: &ManagerPolicyV2, grant: &ManagerNodeGrantV1) -> bool {
    policy.validate().is_ok()
        && grant.validate().is_ok()
        && policy.capabilities.len() == grant.capabilities.len()
        && policy
            .capabilities
            .iter()
            .all(|c| grant.capabilities.contains(c))
        && policy.max_created_containers == grant.allowance.max_created_containers
        && policy.max_created_sessions == grant.allowance.max_created_sessions
        && policy.max_active_sessions == grant.allowance.max_active_sessions
        && policy.max_spend_usd == grant.allowance.max_spend_usd
        && policy.provider_limits.len() == grant.allowance.provider_limits.len()
        && policy
            .provider_limits
            .iter()
            .all(|limit| grant.allowance.provider_limits.contains(limit))
        && policy.allowed_launches.len() == grant.allowed_launches.len()
        && policy
            .allowed_launches
            .iter()
            .all(|choice| grant.allowed_launches.contains(choice))
}

fn reserve_child_capacity(
    tx: &Transaction<'_>,
    parent: Uuid,
    child: Uuid,
    parent_version: i64,
    kind: &str,
    amount: i64,
    ceiling: i64,
    now: &str,
) -> Result<()> {
    let used: i64 = tx.query_row(
        "SELECT coalesce(sum(amount),0) FROM manager_node_reservations WHERE parent_node_id=?1 AND resource_kind=?2 AND state='active'",
        params![parent.to_string(),kind],
        |row| row.get(0),
    )?;
    // A delegated finite capacity always leaves one unit with the parent.
    if amount < 0
        || (ceiling > 0 && used.saturating_add(amount) >= ceiling)
        || (ceiling == 0 && amount != 0)
    {
        return Err(node_refused("manager_allowance_exceeded"));
    }
    tx.execute(
        "INSERT INTO manager_node_reservations(parent_node_id,child_node_id,resource_kind,amount,grant_version,state,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,'active',?6,?6)",
        params![parent.to_string(),child.to_string(),kind,amount,parent_version,now],
    )?;
    Ok(())
}

pub(super) fn replay_operation<T: serde::de::DeserializeOwned, R: Serialize>(
    tx: &Transaction<'_>,
    project: Uuid,
    key: Option<&str>,
    request: &R,
) -> Result<Option<T>> {
    let Some(key) = key else { return Ok(None) };
    if !(1..=128).contains(&key.len()) {
        return Err(node_refused("manager_node_invalid_idempotency_key"));
    }
    let row: Option<(String, String)> = tx.query_row(
        "SELECT request_hash,response_json FROM manager_node_operations WHERE project_id=?1 AND idempotency_key=?2",
        params![project.to_string(),key],
        |row| Ok((row.get(0)?,row.get(1)?)),
    ).optional()?;
    let Some((stored_hash, response_json)) = row else {
        return Ok(None);
    };
    let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(request)?));
    if stored_hash != hash {
        return Err(node_refused("manager_node_idempotency_conflict"));
    }
    Ok(Some(serde_json::from_str(&response_json)?))
}

pub(super) fn record_operation<T: Serialize, R: Serialize>(
    tx: &Transaction<'_>,
    project: Uuid,
    key: Option<&str>,
    request: &R,
    response: &T,
    now: &str,
) -> Result<()> {
    let Some(key) = key else { return Ok(()) };
    let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(request)?));
    tx.execute(
        "INSERT INTO manager_node_operations(project_id,idempotency_key,request_hash,response_json,created_at) VALUES(?1,?2,?3,?4,?5)",
        params![project.to_string(),key,hash,serde_json::to_string(response)?,now],
    )?;
    Ok(())
}

/// Every escalation authority check walks the live grant chain. A revoked
/// ancestor therefore fences descendants even if their own row is still live.
fn retire_subtree_on(
    tx: &Transaction<'_>,
    project: Uuid,
    node: Uuid,
    origin: &str,
    now: &str,
) -> Result<Vec<Uuid>> {
    let mut stmt = tx.prepare("WITH RECURSIVE tree(id,depth) AS (SELECT ?1,0 UNION ALL SELECT n.id,tree.depth+1 FROM manager_nodes n JOIN tree ON n.parent_node_id=tree.id WHERE tree.depth<64) SELECT id FROM tree")?;
    let ids: Vec<String> = stmt
        .query_map([node.to_string()], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    let mut revoked = Vec::new();
    for id in ids {
        let id = canonical_uuid(&id)?;
        let child = area_node_on(tx, project, id)?
            .ok_or_else(|| DaemonError::Store("manager node scope missing".into()))?;
        if !child.active {
            continue;
        }
        let next = child
            .grant_version
            .checked_add(1)
            .ok_or_else(|| node_refused("manager_node_version_exhausted"))?;
        tx.execute("UPDATE manager_nodes SET state='revoked',grant_version=?2,policy_version=policy_version+1,authority_epoch=authority_epoch+1,updated_at=?3 WHERE id=?1 AND state='active'",params![id.to_string(),next,now])?;
        tx.execute("INSERT INTO manager_node_grants(node_id,grant_version,state,grant_json,policy_json,operator_origin,created_at) VALUES(?1,?2,'revoked',NULL,NULL,?3,?4)",params![id.to_string(),next,origin,now])?;
        tx.execute("UPDATE manager_node_reservations SET state='released',updated_at=?2 WHERE child_node_id=?1 AND state='active'",params![id.to_string(),now])?;
        // #1238: escalations it raised no longer wait above the root.
        super::manager_tier_routing::retire_hops_from_source_on(
            tx,
            id,
            child.seat_root_session_id,
            now,
        )?;
        revoked.push(id);
    }
    Ok(revoked)
}

fn retire_terminal_direct_reports_on(
    tx: &Transaction<'_>,
    project: Uuid,
    parent: Uuid,
) -> Result<()> {
    let mut stmt = tx.prepare("SELECT id,seat_root_session_id FROM manager_nodes WHERE parent_node_id=?1 AND state='active'")?;
    let rows: Vec<(String, String)> = stmt
        .query_map([parent.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    for (id, seat) in rows {
        let seat = canonical_uuid(&seat)?;
        let tip = match super::harness_manager::manager_lineage_tip_on(tx, seat) {
            Ok(tip) => tip,
            Err(_) => continue, // Ambiguous lineage requires an operator decision.
        };
        let status: String = tx.query_row(
            "SELECT status FROM sessions WHERE id=?1",
            [tip.to_string()],
            |r| r.get(0),
        )?;
        if status == "Archived" || status == "Deleted" {
            retire_subtree_on(
                tx,
                project,
                canonical_uuid(&id)?,
                "terminal_seat_release",
                &now,
            )?;
        }
    }
    Ok(())
}

impl Store {
    /// Keep the legacy operator appointment as the current root projection.
    /// A root change with active delegates is refused in the same transaction;
    /// the operator can revoke those delegates first.
    pub(super) fn sync_legacy_manager_root_on(
        &self,
        tx: &Transaction<'_>,
        project: Uuid,
    ) -> Result<()> {
        let Some(root) = root_id_on(tx, project)? else {
            return backfill_legacy_roots(self, tx);
        };
        let children: i64 = tx.query_row(
            "SELECT count(*) FROM manager_nodes WHERE parent_node_id=?1 AND state='active'",
            [root.to_string()],
            |row| row.get(0),
        )?;
        if children > 0 {
            return Err(node_refused(
                "manager_node_root_has_active_delegates: revoke child nodes first",
            ));
        }
        let (seat, mode, scope_version, epics_json, groups_json): (String,String,i64,String,String) = tx.query_row(
            "SELECT manager_session_id,scope_mode,row_version,epic_ids_json,group_ids_json FROM harness_manager_scopes WHERE project_id=?1",
            [project.to_string()],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
        )?;
        let epic_ids: Vec<Uuid> = serde_json::from_str(&epics_json)?;
        let group_ids: Vec<Uuid> = serde_json::from_str(&groups_json)?;
        let revoked = mode == "selected" && epic_ids.is_empty() && group_ids.is_empty();
        let selector = match mode.as_str() {
            "project" => ManagerNodeSelectorV1::Project,
            "selected" => ManagerNodeSelectorV1::Selected {
                group_ids,
                epic_ids,
            },
            _ => return Err(node_refused("manager_node_invalid_legacy_scope")),
        };
        if !revoked {
            selector.validate().map_err(node_refused)?;
        }
        let policy: Option<(String,i64,i64,String)> = tx.query_row(
            "SELECT manager_session_id,scope_version,row_version,policy_json FROM harness_manager_v2_policies WHERE project_id=?1",
            [project.to_string()],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
        ).optional()?;
        let current_policy = policy.filter(|(policy_seat, policy_scope, _, _)| {
            policy_seat == &seat && *policy_scope == scope_version && !revoked
        });
        let (grant_state, grant_json, policy_json) = if let Some((_, _, _, json)) = current_policy {
            let policy: ManagerPolicyV2 = serde_json::from_str(&json)?;
            policy.validate().map_err(node_refused)?;
            let grant = ManagerNodeGrantV1 {
                capabilities: policy.capabilities.clone(),
                allowed_launches: policy.allowed_launches.clone(),
                allowance: ManagerNodeAllowanceV1 {
                    max_created_containers: policy.max_created_containers,
                    max_created_sessions: policy.max_created_sessions,
                    max_active_sessions: policy.max_active_sessions,
                    max_build_slots: 0,
                    max_disk_gib: 0,
                    provider_limits: policy.provider_limits.clone(),
                    max_spend_usd: policy.max_spend_usd,
                },
                max_direct_reports: previous_direct_report_cap_on(tx, root)?,
            };
            grant.validate().map_err(node_refused)?;
            ("granted", Some(serde_json::to_string(&grant)?), Some(json))
        } else {
            (if revoked { "revoked" } else { "absent" }, None, None)
        };
        let old = area_node_on(tx, project, root)?
            .ok_or_else(|| node_refused("manager_node_root_absent"))?;
        let next = old
            .grant_version
            .checked_add(1)
            .ok_or_else(|| node_refused("manager_node_version_exhausted"))?;
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        tx.execute(
            "UPDATE manager_nodes SET seat_root_session_id=?2,state=?3,grant_version=?4,policy_version=policy_version+1,authority_epoch=authority_epoch+1,updated_at=?5 WHERE id=?1",
            params![root.to_string(),seat,if revoked {"revoked"} else {"active"},next,now],
        )?;
        if revoked {
            tx.execute(
                "DELETE FROM manager_node_scopes WHERE node_id=?1",
                [root.to_string()],
            )?;
        } else {
            tx.execute("INSERT INTO manager_node_scopes(node_id,project_id,selector_json,grant_version) VALUES(?1,?2,?3,?4) ON CONFLICT(node_id,project_id) DO UPDATE SET selector_json=excluded.selector_json,grant_version=excluded.grant_version",
                params![root.to_string(),project.to_string(),serde_json::to_string(&selector)?,next])?;
        }
        tx.execute("INSERT INTO manager_node_grants(node_id,grant_version,state,grant_json,policy_json,operator_origin,created_at) VALUES(?1,?2,?3,?4,?5,'legacy_operator_sync',?6)",
            params![root.to_string(),next,grant_state,grant_json,policy_json,now])?;
        Ok(())
    }

    /// Operator read: a missing project root is distinct from an empty child list.
    pub fn get_area_node(&self, project: Uuid, id: Uuid) -> Result<Option<AreaNode>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let result = area_node_on(&tx, project, id)?;
        tx.commit()?;
        Ok(result)
    }

    pub fn area_node_project(&self, id: Uuid) -> Result<Option<Uuid>> {
        let project: Option<String> = self
            .conn
            .query_row(
                "SELECT project_id FROM manager_node_scopes WHERE node_id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        project.map(|value| canonical_uuid(&value)).transpose()
    }

    pub fn list_area_nodes(&self, project: Uuid) -> Result<Vec<AreaNode>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let Some(root) = root_id_on(&tx, project)? else {
            tx.commit()?;
            return Ok(Vec::new());
        };
        let mut stmt = tx.prepare("WITH RECURSIVE tree(id) AS (SELECT ?1 UNION ALL SELECT n.id FROM manager_nodes n JOIN tree ON n.parent_node_id=tree.id) SELECT id FROM tree ORDER BY id")?;
        let ids: Vec<String> = stmt
            .query_map([root.to_string()], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        let rows = ids
            .into_iter()
            .map(|id| {
                area_node_on(&tx, project, canonical_uuid(&id)?).and_then(|row| {
                    row.ok_or_else(|| DaemonError::Store("manager node scope missing".into()))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        tx.commit()?;
        Ok(rows)
    }

    pub fn appoint_area_node(&self, request: &AppointAreaNode) -> Result<AreaNode> {
        self.appoint_area_node_for(request, None)
    }

    /// A manager may delegate only from the live node executed by its own
    /// current seat. Resolve that identity inside the same write transaction
    /// that reserves the child grant, so succession and revocation fence it.
    pub fn delegate_area_node(
        &self,
        caller_session_id: Uuid,
        request: &AppointAreaNode,
    ) -> Result<AreaNode> {
        self.appoint_area_node_for(request, Some(caller_session_id))
    }

    fn appoint_area_node_for(
        &self,
        request: &AppointAreaNode,
        caller_session_id: Option<Uuid>,
    ) -> Result<AreaNode> {
        if request.project_id.is_nil()
            || (request.parent_node_id.is_nil() && request.node_id.is_none())
            || request.seat_root_session_id.is_nil()
            || request.operator_origin.is_empty()
            || request.operator_origin.len() > 256
            || !policy_matches_grant(&request.policy, &request.grant)
        {
            return Err(node_refused("manager_node_invalid_appointment"));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(caller) = caller_session_id {
            if caller.is_nil() || request.parent_node_id.is_nil() {
                return Err(node_refused("manager_node_delegate_denied"));
            }
            let parent = area_node_on(&tx, request.project_id, request.parent_node_id)?
                .ok_or_else(|| node_refused("manager_node_delegate_denied"))?;
            let tip =
                super::harness_manager::manager_lineage_tip_on(&tx, parent.seat_root_session_id)?;
            let tip_status: String = tx.query_row(
                "SELECT status FROM sessions WHERE id=?1",
                [tip.to_string()],
                |row| row.get(0),
            )?;
            if !parent.active
                || parent.grant.is_none()
                || parent.policy.as_ref().is_none_or(|policy| {
                    policy.paused
                        || policy.mode
                            != rsi_common::harness_manager_v2::ManagerOperatingModeV2::Execute
                })
                || tip != caller
                || !matches!(
                    tip_status.as_str(),
                    "Starting" | "Running" | "WaitingApproval"
                )
            {
                return Err(node_refused("manager_node_delegate_denied"));
            }
        }
        if let Some(previous) = replay_operation(
            &tx,
            request.project_id,
            request.idempotency_key.as_deref(),
            request,
        )? {
            tx.commit()?;
            return Ok(previous);
        }
        let root = root_id_on(&tx, request.project_id)?
            .ok_or_else(|| node_refused("manager_node_root_absent"))?;
        if request.parent_node_id.is_nil() {
            let current = area_node_on(&tx, request.project_id, root)?
                .ok_or_else(|| node_refused("manager_node_root_absent"))?;
            let mut expected = current
                .grant
                .clone()
                .ok_or_else(|| node_refused("manager_node_parent_grant_absent"))?;
            expected.max_direct_reports = request.grant.max_direct_reports;
            if request.node_id != Some(root)
                || !current.active
                || current.grant_version != request.expected_node_grant_version
                || current.seat_root_session_id != request.seat_root_session_id
                || current.selector.as_ref() != Some(&request.selector)
                || current.policy.as_ref() != Some(&request.policy)
                || request.grant != expected
                || request.grant.max_direct_reports > MANAGER_NODE_DEFAULT_MAX_DIRECT_REPORTS
                || request.grant.max_direct_reports < current.direct_reports
                || request.expected_parent_grant_version != 0
                || request.expected_parent_policy_version != 0
                || request.expected_parent_epoch != 0
            {
                return Err(node_refused("manager_node_invalid_root_cap_edit"));
            }
            let mut children = tx.prepare(
                "SELECT g.grant_json FROM manager_nodes n JOIN manager_node_grants g ON g.node_id=n.id AND g.grant_version=n.grant_version WHERE n.parent_node_id=?1 AND n.state='active'",
            )?;
            let child_grants: Vec<String> = children
                .query_map([root.to_string()], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            drop(children);
            for json in child_grants {
                let child: ManagerNodeGrantV1 = serde_json::from_str(&json)?;
                if child.max_direct_reports >= request.grant.max_direct_reports {
                    return Err(node_refused("manager_node_descendant_grant_widened"));
                }
            }
            let next = current
                .grant_version
                .checked_add(1)
                .ok_or_else(|| node_refused("manager_node_version_exhausted"))?;
            let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
            tx.execute("UPDATE manager_nodes SET grant_version=?2,policy_version=policy_version+1,authority_epoch=authority_epoch+1,updated_at=?3 WHERE id=?1",params![root.to_string(),next,now])?;
            tx.execute("UPDATE manager_node_scopes SET grant_version=?2 WHERE node_id=?1 AND project_id=?3",params![root.to_string(),next,request.project_id.to_string()])?;
            tx.execute("INSERT INTO manager_node_grants(node_id,grant_version,state,grant_json,policy_json,operator_origin,created_at) VALUES(?1,?2,'granted',?3,?4,?5,?6)",params![root.to_string(),next,serde_json::to_string(&request.grant)?,serde_json::to_string(&request.policy)?,request.operator_origin,now])?;
            let result = area_node_on(&tx, request.project_id, root)?.expect("updated root");
            record_operation(
                &tx,
                request.project_id,
                request.idempotency_key.as_deref(),
                request,
                &result,
                &now,
            )?;
            tx.commit()?;
            return Ok(result);
        }
        let parent = area_node_on(&tx, request.project_id, request.parent_node_id)?
            .ok_or_else(|| node_refused("manager_node_parent_absent"))?;
        // Traversing to the root also refuses an orphaned or revoked ancestor.
        let mut cursor = parent.clone();
        let mut depth = 0;
        while cursor.id != root {
            depth += 1;
            if depth > 64 || !cursor.active || cursor.grant.is_none() || cursor.policy.is_none() {
                return Err(node_refused("manager_node_parent_revoked"));
            }
            cursor = area_node_on(
                &tx,
                request.project_id,
                cursor
                    .parent_node_id
                    .ok_or_else(|| node_refused("manager_node_parent_absent"))?,
            )?
            .ok_or_else(|| node_refused("manager_node_parent_absent"))?;
        }
        if !cursor.active
            || !parent.active
            || (cursor.id != parent.id && (cursor.grant.is_none() || cursor.policy.is_none()))
        {
            return Err(node_refused("manager_node_parent_revoked"));
        }
        if parent.grant_version != request.expected_parent_grant_version
            || parent.policy_version != request.expected_parent_policy_version
            || parent.authority_epoch != request.expected_parent_epoch
        {
            return Err(node_refused("manager_node_stale_parent"));
        }
        let previous = request
            .node_id
            .map(|id| {
                area_node_on(&tx, request.project_id, id)?
                    .ok_or_else(|| node_refused("manager_node_absent"))
            })
            .transpose()?;
        if let Some(node) = &previous {
            if !node.active
                || node.parent_node_id != Some(parent.id)
                || node.grant_version != request.expected_node_grant_version
                || node.seat_root_session_id != request.seat_root_session_id
            {
                return Err(node_refused("manager_node_stale_update"));
            }
            if node.direct_reports > 0 {
                return Err(node_refused("manager_node_update_has_descendants"));
            }
        } else if request.expected_node_grant_version != 0 {
            return Err(node_refused("manager_node_stale_update"));
        }
        let parent_grant = parent
            .grant
            .as_ref()
            .ok_or_else(|| node_refused("manager_node_parent_grant_absent"))?;
        let parent_policy = parent
            .policy
            .as_ref()
            .ok_or_else(|| node_refused("manager_node_parent_grant_absent"))?;
        // #1237: the unified rule (equal capabilities allowed; typed codes).
        if request.grant.validate().is_err() || parent_grant.validate().is_err() {
            return Err(node_refused("manager_node_grant_not_narrower"));
        }
        rsi_common::grant_narrowing::grant_narrows(&request.grant.bounds(), &parent_grant.bounds())
            .map_err(node_refused)?;
        if request.policy.mode as u8 > parent_policy.mode as u8
            || (parent_policy.paused && !request.policy.paused)
            || (request.policy.allow_create_groups && !parent_policy.allow_create_groups)
            || request.policy.max_recovery_attempts > parent_policy.max_recovery_attempts
            || request
                .policy
                .allowed_launches
                .iter()
                .any(|launch| !parent_policy.allowed_launches.contains(launch))
            || parent_policy.provider_limits.iter().any(|limit| {
                !request.policy.provider_limits.iter().any(|child| {
                    child.provider == limit.provider && child.max_active <= limit.max_active
                })
            })
        {
            return Err(node_refused("manager_node_grant_not_narrower"));
        }
        let (groups, epics) = live_topology(&tx, request.project_id)?;
        checked_selector(&request.selector, &groups, &epics)?;
        let parent_selector = parent
            .selector
            .as_ref()
            .ok_or_else(|| node_refused("manager_node_parent_scope_absent"))?;
        if !strict_child_selector(&request.selector, parent_selector, &epics) {
            return Err(node_refused("manager_node_selector_not_narrower"));
        }
        if request.policy.group_ids.iter().any(|group| {
            !matches!(&request.selector, ManagerNodeSelectorV1::Selected { group_ids, .. } if group_ids.contains(group))
        }) || parent_policy.paused_epic_ids.iter().any(|epic| {
            epics.iter().any(|(id,group)| id == epic && request.selector.covers_epic(*id,*group))
                && !request.policy.paused_epic_ids.contains(epic)
        }) {
            return Err(node_refused("manager_node_policy_out_of_scope"));
        }
        let seat: Option<(String, String, Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT project_id,session_kind,parent_id,continued_from FROM sessions WHERE id=?1",
                [request.seat_root_session_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        if !seat.is_some_and(|(project, kind, parent, predecessor)| {
            project == request.project_id.to_string()
                && kind == "Standard"
                && parent.is_none()
                && predecessor.is_none()
        }) {
            return Err(node_refused("manager_node_invalid_seat"));
        }
        let committed_predecessor: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_rotation_edges WHERE successor_session_id=?1 AND retired_at IS NULL)
                OR EXISTS(SELECT 1 FROM agent_successor_reservations WHERE candidate_session_id=?1 AND state='committed')",
            [request.seat_root_session_id.to_string()],
            |r| r.get(0),
        )?;
        if committed_predecessor {
            return Err(node_refused("manager_node_invalid_seat"));
        }
        let tip =
            super::harness_manager::manager_lineage_tip_on(&tx, request.seat_root_session_id)?;
        let tip_status: String = tx.query_row(
            "SELECT status FROM sessions WHERE id=?1",
            [tip.to_string()],
            |r| r.get(0),
        )?;
        if tip_status == "Archived" || tip_status == "Deleted" {
            return Err(node_refused("manager_node_invalid_seat"));
        }
        let active_seats: Vec<String> = {
            let mut statement = tx.prepare(
                "SELECT seat_root_session_id FROM manager_nodes WHERE state='active' AND id<>?1",
            )?;
            statement
                .query_map([request.node_id.unwrap_or(Uuid::nil()).to_string()], |r| {
                    r.get(0)
                })?
                .collect::<rusqlite::Result<_>>()?
        };
        for active_seat in active_seats {
            let active_root = canonical_uuid(&active_seat)?;
            if active_root == request.seat_root_session_id
                || super::harness_manager::manager_lineage_tip_on(&tx, active_root)? == tip
            {
                return Err(node_refused("manager_node_seat_occupied"));
            }
        }
        retire_terminal_direct_reports_on(&tx, request.project_id, parent.id)?;
        let mut stmt = tx.prepare("SELECT s.selector_json FROM manager_nodes n JOIN manager_node_scopes s ON s.node_id=n.id WHERE n.parent_node_id=?1 AND n.state='active' AND s.project_id=?2 AND n.id<>?3")?;
        let siblings: Vec<String> = stmt
            .query_map(
                params![
                    parent.id.to_string(),
                    request.project_id.to_string(),
                    request.node_id.unwrap_or(Uuid::nil()).to_string()
                ],
                |r| r.get(0),
            )?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        for sibling in siblings {
            if selectors_overlap(&request.selector, &serde_json::from_str(&sibling)?, &epics) {
                return Err(node_refused("manager_node_sibling_overlap"));
            }
        }
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM manager_nodes WHERE parent_node_id=?1 AND state='active'",
            [parent.id.to_string()],
            |r| r.get(0),
        )?;
        if previous.is_none() && count >= i64::from(parent_grant.max_direct_reports) {
            return Err(node_refused("manager_node_direct_report_cap"));
        }
        let id = request.node_id.unwrap_or_else(Uuid::new_v4);
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        let version = if let Some(node) = &previous {
            let next = node
                .grant_version
                .checked_add(1)
                .ok_or_else(|| node_refused("manager_node_version_exhausted"))?;
            tx.execute("UPDATE manager_nodes SET grant_version=?2,policy_version=policy_version+1,authority_epoch=authority_epoch+1,updated_at=?3 WHERE id=?1",params![id.to_string(),next,now])?;
            tx.execute("UPDATE manager_node_scopes SET selector_json=?2,grant_version=?3 WHERE node_id=?1 AND project_id=?4",params![id.to_string(),serde_json::to_string(&request.selector)?,next,request.project_id.to_string()])?;
            tx.execute("UPDATE manager_node_reservations SET state='released',updated_at=?2 WHERE child_node_id=?1 AND state='active'",params![id.to_string(),now])?;
            next
        } else {
            tx.execute("INSERT INTO manager_nodes(id,parent_node_id,legacy_project_id,seat_root_session_id,state,grant_version,policy_version,authority_epoch,created_at,updated_at) VALUES(?1,?2,NULL,?3,'active',1,1,1,?4,?4)",params![id.to_string(),parent.id.to_string(),request.seat_root_session_id.to_string(),now])?;
            tx.execute("INSERT INTO manager_node_scopes(node_id,project_id,selector_json,grant_version) VALUES(?1,?2,?3,1)",params![id.to_string(),request.project_id.to_string(),serde_json::to_string(&request.selector)?])?;
            1
        };
        tx.execute("INSERT INTO manager_node_grants(node_id,grant_version,state,grant_json,policy_json,operator_origin,created_at) VALUES(?1,?2,'granted',?3,?4,?5,?6)",params![id.to_string(),version,serde_json::to_string(&request.grant)?,serde_json::to_string(&request.policy)?,request.operator_origin,now])?;
        tx.execute("INSERT INTO manager_node_reservations(parent_node_id,child_node_id,resource_kind,amount,grant_version,state,created_at,updated_at) VALUES(?1,?2,'direct_report',1,?3,'active',?4,?4)",params![parent.id.to_string(),id.to_string(),version,now])?;
        let parent_allowance = &parent_grant.allowance;
        let child_allowance = &request.grant.allowance;
        for (kind, amount, ceiling) in [
            (
                "created_containers",
                i64::from(child_allowance.max_created_containers),
                i64::from(parent_allowance.max_created_containers),
            ),
            (
                "created_sessions",
                i64::from(child_allowance.max_created_sessions),
                i64::from(parent_allowance.max_created_sessions),
            ),
            (
                "active_sessions",
                i64::from(child_allowance.max_active_sessions),
                i64::from(parent_allowance.max_active_sessions),
            ),
            (
                "build_slots",
                i64::from(child_allowance.max_build_slots),
                i64::from(parent_allowance.max_build_slots),
            ),
            (
                "disk_gib",
                i64::from(child_allowance.max_disk_gib),
                i64::from(parent_allowance.max_disk_gib),
            ),
        ] {
            reserve_child_capacity(&tx, parent.id, id, version, kind, amount, ceiling, &now)?;
        }
        for limit in &child_allowance.provider_limits {
            let parent_limit = parent_allowance
                .provider_limits
                .iter()
                .find(|p| p.provider == limit.provider)
                .ok_or_else(|| node_refused("manager_grant_provider_widened"))?;
            let kind = format!("provider:{}", serde_json::to_string(&limit.provider)?);
            reserve_child_capacity(
                &tx,
                parent.id,
                id,
                version,
                &kind,
                i64::from(limit.max_active),
                i64::from(parent_limit.max_active),
                &now,
            )?;
        }
        if let (Some(child), Some(parent_spend)) = (
            child_allowance.max_spend_usd,
            parent_allowance.max_spend_usd,
        ) {
            let amount = (child * 1_000_000.0).ceil() as i64;
            let ceiling = (parent_spend * 1_000_000.0).floor() as i64;
            reserve_child_capacity(
                &tx,
                parent.id,
                id,
                version,
                "spend_micro_usd",
                amount,
                ceiling,
                &now,
            )?;
        }
        let result = area_node_on(&tx, request.project_id, id)?.expect("inserted node");
        record_operation(
            &tx,
            request.project_id,
            request.idempotency_key.as_deref(),
            request,
            &result,
            &now,
        )?;
        tx.commit()?;
        Ok(result)
    }

    pub fn revoke_area_node(&self, request: &RevokeAreaNode) -> Result<Vec<Uuid>> {
        if request.operator_origin.is_empty() || request.operator_origin.len() > 256 {
            return Err(node_refused("manager_node_invalid_origin"));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(previous) = replay_operation(
            &tx,
            request.project_id,
            request.idempotency_key.as_deref(),
            request,
        )? {
            tx.commit()?;
            return Ok(previous);
        }
        let root = root_id_on(&tx, request.project_id)?
            .ok_or_else(|| node_refused("manager_node_root_absent"))?;
        if request.node_id == root {
            return Err(node_refused("manager_node_root_legacy_owned"));
        }
        let node = area_node_on(&tx, request.project_id, request.node_id)?
            .ok_or_else(|| node_refused("manager_node_absent"))?;
        if !node.active
            || node.grant_version != request.expected_grant_version
            || node.authority_epoch != request.expected_epoch
        {
            return Err(node_refused("manager_node_stale_revoke"));
        }
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        let revoked = retire_subtree_on(
            &tx,
            request.project_id,
            node.id,
            &request.operator_origin,
            &now,
        )?;
        record_operation(
            &tx,
            request.project_id,
            request.idempotency_key.as_deref(),
            request,
            &revoked,
            &now,
        )?;
        tx.commit()?;
        Ok(revoked)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support::test_session;
    use rsi_common::harness_manager::{
        AgentManagerEscalateRequestV1, AgentManagerResolveEscalationRequestV1,
        ConfigureHarnessManagerRequestV1, ManagerNodeEscalationRouteV1,
        ManagerNodeEscalationStateV1,
    };
    use rsi_common::harness_manager_v2::{
        ConfigureHarnessManagerPolicyRequestV2, ManagerCapabilityV2,
    };
    use rsi_common::types::{Project, SessionKind, SessionStatus};
    use std::path::PathBuf;

    fn add_legacy_root(store: &Store, granted: bool) -> Uuid {
        let project = Uuid::new_v4();
        let stamp = Utc::now();
        store
            .insert_project(&Project {
                id: project,
                name: format!("Manager nodes {project}"),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: stamp,
                updated_at: stamp,
            })
            .unwrap();
        let mut manager = test_session(Uuid::new_v4(), PathBuf::from("/tmp/manager-nodes"));
        manager.project_id = Some(project);
        manager.status = SessionStatus::Completed;
        store.insert_session(&manager).unwrap();
        let mut group = manager.clone();
        group.id = Uuid::new_v4();
        group.session_kind = SessionKind::Group;
        store.insert_session(&group).unwrap();
        let mut epic = manager.clone();
        epic.id = Uuid::new_v4();
        epic.session_kind = SessionKind::Epic;
        epic.parent_id = Some(group.id);
        store.insert_session(&epic).unwrap();
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: project,
                session_id: manager.id,
                epic_ids: Some(vec![epic.id]),
                group_ids: vec![],
                expected_row_version: 0,
            })
            .unwrap();
        if granted {
            store
                .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                    project_id: project,
                    expected_scope_version: 1,
                    expected_policy_version: 0,
                    idempotency_key: "legacy-root-grant".into(),
                    policy: ManagerPolicyV2 {
                        capabilities: vec![ManagerCapabilityV2::WorkPlan],
                        ..Default::default()
                    },
                })
                .unwrap();
        }
        project
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn legacy_root_backfill_preserves_grant_and_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        let absent_project = add_legacy_root(&store, false);
        let granted_project = add_legacy_root(&store, true);
        let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();
        backfill_legacy_roots(&store, &tx).unwrap();
        backfill_legacy_roots(&store, &tx).unwrap();
        tx.commit().unwrap();
        let root_count: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM manager_nodes WHERE legacy_project_id IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(root_count, 2);
        let read = |project: Uuid| -> (String, Option<String>, i64, String) {
            store.conn.query_row(
            "SELECT g.state,g.grant_json,n.policy_version,json_extract(s.selector_json,'$.mode')
             FROM manager_nodes n
             JOIN manager_node_grants g ON g.node_id=n.id AND g.grant_version=n.grant_version
             JOIN manager_node_scopes s ON s.node_id=n.id
             WHERE n.legacy_project_id=?1",
            params![project.to_string()],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
            ).unwrap()
        };
        let (state, grant, version, selector) = read(absent_project);
        assert_eq!(state, "absent");
        assert!(grant.is_none());
        assert_eq!(version, 0);
        assert_eq!(selector, "selected");
        let (state, grant, version, selector) = read(granted_project);
        assert_eq!(state, "granted");
        assert_eq!(version, 1);
        assert_eq!(selector, "selected");
        let grant: ManagerNodeGrantV1 = serde_json::from_str(&grant.unwrap()).unwrap();
        assert_eq!(grant.capabilities, vec![ManagerCapabilityV2::WorkPlan]);
        assert_eq!(
            grant.max_direct_reports,
            MANAGER_NODE_DEFAULT_MAX_DIRECT_REPORTS
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn invalid_legacy_manager_lineages_backfill_without_authority() {
        let store = Store::open_in_memory().unwrap();
        let deleted_project = add_legacy_root(&store, true);
        let cycled_project = add_legacy_root(&store, true);
        let seat = |project: Uuid| -> Uuid {
            let id: String = store
                .conn
                .query_row(
                    "SELECT manager_session_id FROM harness_manager_scopes WHERE project_id=?1",
                    [project.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            Uuid::parse_str(&id).unwrap()
        };
        let deleted_seat = seat(deleted_project);
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Deleted' WHERE id=?1",
                [deleted_seat.to_string()],
            )
            .unwrap();
        let cycled_seat = seat(cycled_project);
        let mut successor = store.get_session(cycled_seat).unwrap().unwrap();
        successor.id = Uuid::new_v4();
        successor.continued_from = Some(cycled_seat);
        successor.rotation_depth += 1;
        store.insert_session(&successor).unwrap();
        store
            .update_session_status(cycled_seat, SessionStatus::Archived)
            .unwrap();
        store
            .record_harness_manager_rotation(cycled_seat, successor.id)
            .unwrap();
        store
            .update_session_status(successor.id, SessionStatus::Archived)
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET continued_from=?2 WHERE id=?1",
                params![cycled_seat.to_string(), successor.id.to_string()],
            )
            .unwrap();
        store.conn.execute("INSERT INTO harness_manager_rotation_edges(predecessor_session_id,successor_session_id,committed_at) VALUES(?1,?2,?3)",params![successor.id.to_string(),cycled_seat.to_string(),Utc::now().to_rfc3339_opts(SecondsFormat::Nanos,true)]).unwrap();
        // Legacy rows are written through the current store first; the
        // rewind then removes every later schema (V138 and newer) before the
        // V138 migration runs.
        super::super::tests::rewind_post_v121_tail_to(&store.conn, MANAGER_NODE_SCHEMA_VERSION - 1);
        apply_manager_node_migration(&store).unwrap();
        for project in [deleted_project, cycled_project] {
            let state: String = store.conn.query_row(
                "SELECT g.state FROM manager_nodes n JOIN manager_node_grants g ON g.node_id=n.id WHERE n.legacy_project_id=?1",
                [project.to_string()], |row| row.get(0)).unwrap();
            assert_eq!(state, "absent");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn migration_from_legacy_schema_preserves_scope_policy_and_mail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manager-nodes-upgrade.db");
        let store = Store::open(&path).unwrap();
        let project = add_legacy_root(&store, true);
        let seat: String = store
            .conn
            .query_row(
                "SELECT manager_session_id FROM harness_manager_scopes WHERE project_id=?1",
                [project.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        store.conn.execute(
            "INSERT INTO harness_manager_v2_events(project_id,manager_session_id,scope_version,kind,record_key,row_version,payload_json,created_at) VALUES(?1,?2,1,'agent_mail_authority','legacy-mail',1,'{}',?3)",
            params![project.to_string(), seat, Utc::now().to_rfc3339_opts(SecondsFormat::Nanos,true)]).unwrap();
        super::super::tests::rewind_post_v121_tail_to(&store.conn, MANAGER_NODE_SCHEMA_VERSION - 1);
        let count = |table: &str| -> i64 {
            store
                .conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap()
        };
        assert_eq!(count("harness_manager_scopes"), 1);
        assert_eq!(count("harness_manager_v2_policies"), 1);
        let mail_and_policy_events = count("harness_manager_v2_events");
        assert!(mail_and_policy_events >= 1);
        drop(store);

        let upgraded = Store::open(&path).unwrap();
        assert_eq!(
            upgraded
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                .unwrap(),
            super::super::LATEST_SCHEMA_VERSION
        );
        let count = |table: &str| -> i64 {
            upgraded
                .conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap()
        };
        assert_eq!(count("harness_manager_scopes"), 1);
        assert_eq!(count("harness_manager_v2_policies"), 1);
        assert_eq!(count("harness_manager_v2_events"), mail_and_policy_events);
        assert_eq!(count("manager_nodes"), 1);
        assert_eq!(count("manager_node_grants"), 1);
        assert_eq!(count("manager_node_scopes"), 1);
        let state: String = upgraded
            .conn
            .query_row("SELECT state FROM manager_node_grants", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state, "granted");
        for name in [
            "manager_node_epic_insert_overlap",
            "manager_node_epic_update_overlap",
            "manager_agent_mail_authority_lookup",
        ] {
            let installed: bool = upgraded
                .conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
                    [name],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(installed, "missing migration object {name}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn preupgrade_mail_checks_manager_history_on_predecessor_seats() {
        let (store, project, _root, _, _) = area_fixture(true);
        let mut predecessor = test_session(Uuid::new_v4(), PathBuf::from("/tmp/old-manager-seat"));
        predecessor.project_id = Some(project);
        predecessor.session_kind = SessionKind::Standard;
        predecessor.status = SessionStatus::Archived;
        store.insert_session(&predecessor).unwrap();
        let mut successor = predecessor.clone();
        successor.id = Uuid::new_v4();
        successor.continued_from = Some(predecessor.id);
        successor.rotation_depth += 1;
        successor.status = SessionStatus::Running;
        store.insert_session(&successor).unwrap();
        store
            .record_harness_manager_rotation(predecessor.id, successor.id)
            .unwrap();
        let mut target = test_session(Uuid::new_v4(), PathBuf::from("/tmp/other-manager-target"));
        target.project_id = Some(project);
        store.insert_session(&target).unwrap();
        store.conn.execute(
            "INSERT INTO harness_manager_v2_events(project_id,manager_session_id,scope_version,kind,record_key,row_version,payload_json,created_at) VALUES(?1,?2,1,'retained','prior-manager-action',1,'{}',?3)",
            params![project.to_string(), predecessor.id.to_string(), Utc::now().to_rfc3339_opts(SecondsFormat::Nanos,true)]).unwrap();
        let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();
        assert!(
            !store
                .manager_mail_claim_current(&tx, Uuid::new_v4(), successor.id, target.id)
                .unwrap()
        );
        tx.rollback().unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn root_manager_reads_the_project_policy_row_not_area_node_authority() {
        // Recursive nodes do not change what the project's root manager reads:
        // a missing V2 policy row stays `None` and a present row is returned
        // as stored. Only area-node configs resolve through node grants.
        let store = Store::open_in_memory().unwrap();
        let ungranted = add_legacy_root(&store, false);
        let root = store.get_harness_manager(ungranted).unwrap().unwrap();
        assert!(store.manager_policy_for_config(&root).unwrap().is_none());

        let granted = add_legacy_root(&store, true);
        let root = store.get_harness_manager(granted).unwrap().unwrap();
        let expected = store.get_harness_manager_policy(granted).unwrap().unwrap();
        let actual = store.manager_policy_for_config(&root).unwrap().unwrap();
        assert_eq!(
            (
                actual.manager_session_id,
                actual.row_version,
                actual.revoked
            ),
            (expected.manager_session_id, expected.row_version, false)
        );
    }

    // Shared by manager_node_workspace_tests in every shard (#1397).
    #[allow(dead_code)]
    pub(crate) fn area_fixture(granted: bool) -> (Store, Uuid, AreaNode, Uuid, Vec<Uuid>) {
        let store = Store::open_in_memory().unwrap();
        let project = add_legacy_root(&store, granted);
        if granted {
            store
                .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                    project_id: project,
                    expected_scope_version: 1,
                    expected_policy_version: 1,
                    idempotency_key: "broader-root-grant".into(),
                    policy: ManagerPolicyV2 {
                        mode: rsi_common::harness_manager_v2::ManagerOperatingModeV2::Execute,
                        capabilities: vec![
                            ManagerCapabilityV2::WorkPlan,
                            ManagerCapabilityV2::LeadControl,
                            ManagerCapabilityV2::Topology,
                        ],
                        max_active_sessions: 100,
                        ..Default::default()
                    },
                })
                .unwrap();
        }
        let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();
        backfill_legacy_roots(&store, &tx).unwrap();
        let root = root_id_on(&tx, project).unwrap().unwrap();
        tx.execute(
            "UPDATE manager_node_scopes SET selector_json=?2 WHERE node_id=?1",
            params![
                root.to_string(),
                serde_json::to_string(&ManagerNodeSelectorV1::Project).unwrap()
            ],
        )
        .unwrap();
        tx.commit().unwrap();
        let group: String = store
            .conn
            .query_row(
                "SELECT id FROM sessions WHERE project_id=?1 AND session_kind='Group'",
                [project.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        let group = Uuid::parse_str(&group).unwrap();
        let mut epics = Vec::new();
        for _ in 0..6 {
            let mut epic = test_session(Uuid::new_v4(), PathBuf::from("/tmp/manager-area-epic"));
            epic.project_id = Some(project);
            epic.session_kind = SessionKind::Epic;
            epic.parent_id = Some(group);
            epic.status = SessionStatus::Completed;
            store.insert_session(&epic).unwrap();
            epics.push(epic.id);
        }
        let root = store.get_area_node(project, root).unwrap().unwrap();
        (store, project, root, group, epics)
    }

    // Shared by manager_node_workspace_tests in every shard (#1397).
    #[allow(dead_code)]
    pub(crate) fn area_request(
        store: &Store,
        project: Uuid,
        parent: &AreaNode,
        selector: ManagerNodeSelectorV1,
    ) -> AppointAreaNode {
        let mut seat = test_session(Uuid::new_v4(), PathBuf::from("/tmp/manager-area-seat"));
        seat.project_id = Some(project);
        seat.session_kind = SessionKind::Standard;
        seat.status = SessionStatus::Completed;
        store.insert_session(&seat).unwrap();
        let parent_grant = parent.grant.as_ref().unwrap();
        let grant = ManagerNodeGrantV1 {
            capabilities: parent_grant
                .capabilities
                .iter()
                .take(parent_grant.capabilities.len() - 1)
                .copied()
                .collect(),
            allowed_launches: vec![],
            allowance: ManagerNodeAllowanceV1 {
                max_created_containers: 0,
                max_created_sessions: 0,
                max_active_sessions: if parent_grant.allowance.max_active_sessions == 100 {
                    19
                } else {
                    parent_grant.allowance.max_active_sessions - 1
                },
                max_build_slots: 0,
                max_disk_gib: 0,
                provider_limits: vec![],
                max_spend_usd: None,
            },
            max_direct_reports: parent_grant.max_direct_reports - 1,
        };
        let policy = ManagerPolicyV2 {
            capabilities: grant.capabilities.clone(),
            max_active_sessions: grant.allowance.max_active_sessions,
            ..Default::default()
        };
        AppointAreaNode {
            idempotency_key: None,
            node_id: None,
            expected_node_grant_version: 0,
            project_id: project,
            parent_node_id: parent.id,
            expected_parent_grant_version: parent.grant_version,
            expected_parent_policy_version: parent.policy_version,
            expected_parent_epoch: parent.authority_epoch,
            seat_root_session_id: seat.id,
            selector,
            grant,
            policy,
            operator_origin: "operator-test".into(),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn manager_delegation_requires_live_parent_seat() {
        let (store, project, root, _group, epics) = area_fixture(true);
        let selector = |epic| ManagerNodeSelectorV1::Selected {
            group_ids: vec![],
            epic_ids: vec![epic],
        };
        let denied = area_request(&store, project, &root, selector(epics[0]));
        assert!(store.delegate_area_node(Uuid::new_v4(), &denied).is_err());
        assert!(
            store
                .delegate_area_node(root.seat_root_session_id, &denied)
                .is_err()
        );
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Running' WHERE id=?1",
                [root.seat_root_session_id.to_string()],
            )
            .unwrap();
        let child = store
            .delegate_area_node(root.seat_root_session_id, &denied)
            .unwrap();
        assert_eq!(child.parent_node_id, Some(root.id));
        let second = area_request(&store, project, &root, selector(epics[1]));
        assert!(
            store
                .delegate_area_node(denied.seat_root_session_id, &second)
                .is_err()
        );
        let monitor = store
            .appoint_area_node(&area_request(
                &store,
                project,
                &root,
                ManagerNodeSelectorV1::Selected {
                    group_ids: vec![],
                    epic_ids: vec![epics[2], epics[3]],
                },
            ))
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Running' WHERE id=?1",
                [monitor.seat_root_session_id.to_string()],
            )
            .unwrap();
        let monitor_child = area_request(&store, project, &monitor, selector(epics[2]));
        assert!(
            store
                .delegate_area_node(monitor.seat_root_session_id, &monitor_child)
                .is_err()
        );
        assert_eq!(store.list_area_nodes(project).unwrap().len(), 3);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn rotated_manager_seats_cannot_be_reappointed_as_children() {
        let (store, project, root, _, epics) = area_fixture(true);
        let selected = |epic| ManagerNodeSelectorV1::Selected {
            group_ids: vec![],
            epic_ids: vec![epic],
        };
        let sibling = store
            .appoint_area_node(&area_request(&store, project, &root, selected(epics[0])))
            .unwrap();
        let rotate = |anchor: Uuid| -> Uuid {
            let mut successor = store.get_session(anchor).unwrap().unwrap();
            successor.id = Uuid::new_v4();
            successor.continued_from = Some(anchor);
            successor.rotation_depth += 1;
            successor.status = SessionStatus::Running;
            store.insert_session(&successor).unwrap();
            store
                .update_session_status(anchor, SessionStatus::Archived)
                .unwrap();
            store
                .record_harness_manager_rotation(anchor, successor.id)
                .unwrap();
            successor.id
        };
        let sibling_tip = rotate(sibling.seat_root_session_id);
        let mut request = area_request(&store, project, &root, selected(epics[1]));
        request.seat_root_session_id = sibling_tip;
        assert!(
            store
                .appoint_area_node(&request)
                .unwrap_err()
                .to_string()
                .contains("manager_node_invalid_seat")
        );
        assert_eq!(
            store
                .manager_area_authority_current(sibling_tip)
                .unwrap()
                .unwrap()
                .grant
                .manager_session_id,
            sibling.id
        );

        let root_tip = rotate(root.seat_root_session_id);
        let mut request = area_request(&store, project, &root, selected(epics[2]));
        request.seat_root_session_id = root_tip;
        assert!(
            store
                .appoint_area_node(&request)
                .unwrap_err()
                .to_string()
                .contains("manager_node_invalid_seat")
        );
        assert_eq!(
            store
                .manager_area_authority_current(sibling_tip)
                .unwrap()
                .unwrap()
                .grant
                .manager_session_id,
            sibling.id
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn area_nodes_require_disjoint_live_areas_and_release_direct_report_capacity() {
        let (store, project, root, group, epics) = area_fixture(true);
        let selected = |epic| ManagerNodeSelectorV1::Selected {
            group_ids: vec![],
            epic_ids: vec![epic],
        };
        let first = store
            .appoint_area_node(&area_request(&store, project, &root, selected(epics[0])))
            .unwrap();
        assert_eq!(
            store
                .get_area_node(project, first.id)
                .unwrap()
                .unwrap()
                .seat_root_session_id,
            first.seat_root_session_id
        );
        let overlap = area_request(
            &store,
            project,
            &root,
            ManagerNodeSelectorV1::Selected {
                group_ids: vec![group],
                epic_ids: vec![],
            },
        );
        assert!(
            store
                .appoint_area_node(&overlap)
                .unwrap_err()
                .to_string()
                .contains("manager_node_sibling_overlap")
        );
        for epic in epics.iter().take(5).skip(1) {
            store
                .appoint_area_node(&area_request(&store, project, &root, selected(*epic)))
                .unwrap();
        }
        assert_eq!(
            store
                .list_area_nodes(project)
                .unwrap()
                .iter()
                .filter(|n| n.active)
                .count(),
            6
        );
        let sixth = area_request(&store, project, &root, selected(epics[5]));
        assert!(
            store
                .appoint_area_node(&sixth)
                .unwrap_err()
                .to_string()
                .contains("manager_node_direct_report_cap")
        );
        assert_eq!(
            store
                .revoke_area_node(&RevokeAreaNode {
                    idempotency_key: None,
                    project_id: project,
                    node_id: first.id,
                    expected_grant_version: first.grant_version,
                    expected_epoch: first.authority_epoch,
                    operator_origin: "operator-test".into()
                })
                .unwrap(),
            vec![first.id]
        );
        let old = store.get_area_node(project, first.id).unwrap().unwrap();
        assert!(!old.active);
        assert_eq!(old.authority_epoch, first.authority_epoch + 1);
        store.appoint_area_node(&sixth).unwrap();
        let active_reservations: i64 = store.conn.query_row("SELECT count(*) FROM manager_node_reservations WHERE parent_node_id=?1 AND resource_kind='direct_report' AND state='active'",[root.id.to_string()],|r| r.get(0)).unwrap();
        assert_eq!(active_reservations, 5);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn updating_area_node_fences_old_grant_and_rechecks_sibling_scope() {
        let (store, project, root, _, epics) = area_fixture(true);
        let selected = |epic| ManagerNodeSelectorV1::Selected {
            group_ids: vec![],
            epic_ids: vec![epic],
        };
        let mut first_request = area_request(&store, project, &root, selected(epics[0]));
        let first = store.appoint_area_node(&first_request).unwrap();
        store
            .appoint_area_node(&area_request(&store, project, &root, selected(epics[1])))
            .unwrap();
        first_request.node_id = Some(first.id);
        first_request.expected_node_grant_version = first.grant_version;
        first_request.selector = selected(epics[1]);
        assert!(
            store
                .appoint_area_node(&first_request)
                .unwrap_err()
                .to_string()
                .contains("manager_node_sibling_overlap")
        );
        first_request.selector = selected(epics[2]);
        let updated = store.appoint_area_node(&first_request).unwrap();
        assert_eq!(updated.grant_version, first.grant_version + 1);
        assert_eq!(updated.authority_epoch, first.authority_epoch + 1);
        assert!(
            store
                .appoint_area_node(&first_request)
                .unwrap_err()
                .to_string()
                .contains("manager_node_stale_update")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn sibling_allowance_sum_preserves_parent_capacity() {
        let (store, project, root, _, epics) = area_fixture(true);
        let selected = |epic| ManagerNodeSelectorV1::Selected {
            group_ids: vec![],
            epic_ids: vec![epic],
        };
        let mut first = area_request(&store, project, &root, selected(epics[0]));
        first.grant.allowance.max_active_sessions = 60;
        first.policy.max_active_sessions = 60;
        store.appoint_area_node(&first).unwrap();
        let mut second = area_request(&store, project, &root, selected(epics[1]));
        second.grant.allowance.max_active_sessions = 40;
        second.policy.max_active_sessions = 40;
        assert!(
            store
                .appoint_area_node(&second)
                .unwrap_err()
                .to_string()
                .contains("manager_allowance_exceeded")
        );
        second.grant.allowance.max_active_sessions = 39;
        second.policy.max_active_sessions = 39;
        store.appoint_area_node(&second).unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn live_group_move_cannot_make_sibling_manager_scopes_overlap() {
        let (store, project, root, group, _) = area_fixture(true);
        let mut other_group =
            test_session(Uuid::new_v4(), PathBuf::from("/tmp/manager-other-group"));
        other_group.project_id = Some(project);
        other_group.session_kind = SessionKind::Group;
        other_group.status = SessionStatus::Completed;
        store.insert_session(&other_group).unwrap();
        let mut epic = test_session(Uuid::new_v4(), PathBuf::from("/tmp/manager-other-epic"));
        epic.project_id = Some(project);
        epic.session_kind = SessionKind::Epic;
        epic.parent_id = Some(other_group.id);
        epic.status = SessionStatus::Completed;
        store.insert_session(&epic).unwrap();
        store
            .appoint_area_node(&area_request(
                &store,
                project,
                &root,
                ManagerNodeSelectorV1::Selected {
                    group_ids: vec![group],
                    epic_ids: vec![],
                },
            ))
            .unwrap();
        store
            .appoint_area_node(&area_request(
                &store,
                project,
                &root,
                ManagerNodeSelectorV1::Selected {
                    group_ids: vec![],
                    epic_ids: vec![epic.id],
                },
            ))
            .unwrap();
        assert!(
            store
                .update_session_parent(epic.id, Some(group))
                .unwrap_err()
                .to_string()
                .contains("manager_scope_overlap")
        );
        assert_eq!(
            store.get_session(epic.id).unwrap().unwrap().parent_id,
            Some(other_group.id)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn archived_area_seat_releases_direct_report_capacity_on_next_grant() {
        let (store, project, root, _, epics) = area_fixture(true);
        let selected = |epic| ManagerNodeSelectorV1::Selected {
            group_ids: vec![],
            epic_ids: vec![epic],
        };
        let first = store
            .appoint_area_node(&area_request(&store, project, &root, selected(epics[0])))
            .unwrap();
        for epic in epics.iter().take(5).skip(1) {
            store
                .appoint_area_node(&area_request(&store, project, &root, selected(*epic)))
                .unwrap();
        }
        store
            .update_session_status(first.seat_root_session_id, SessionStatus::Archived)
            .unwrap();
        store
            .appoint_area_node(&area_request(&store, project, &root, selected(epics[5])))
            .unwrap();
        let retired = store.get_area_node(project, first.id).unwrap().unwrap();
        assert!(!retired.active);
        assert_eq!(retired.authority_epoch, first.authority_epoch + 1);
        let active: i64 = store.conn.query_row("SELECT count(*) FROM manager_node_reservations WHERE parent_node_id=?1 AND resource_kind='direct_report' AND state='active'",[root.id.to_string()],|r|r.get(0)).unwrap();
        assert_eq!(active, 5);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn operator_root_cap_below_default_limits_live_delegates() {
        let (store, project, root, _, epics) = area_fixture(true);
        let mut grant = root.grant.clone().unwrap();
        grant.max_direct_reports = 2;
        let edit = AppointAreaNode {
            idempotency_key: Some("root-direct-report-cap".into()),
            node_id: Some(root.id),
            expected_node_grant_version: root.grant_version,
            project_id: project,
            parent_node_id: Uuid::nil(),
            expected_parent_grant_version: 0,
            expected_parent_policy_version: 0,
            expected_parent_epoch: 0,
            seat_root_session_id: root.seat_root_session_id,
            selector: root.selector.clone().unwrap(),
            grant,
            policy: root.policy.clone().unwrap(),
            operator_origin: "operator-test".into(),
        };
        let updated = store.appoint_area_node(&edit).unwrap();
        assert_eq!(
            store.appoint_area_node(&edit).unwrap().grant_version,
            updated.grant_version
        );
        let selected = |epic| ManagerNodeSelectorV1::Selected {
            group_ids: vec![],
            epic_ids: vec![epic],
        };
        for epic in epics.iter().take(2) {
            store
                .appoint_area_node(&area_request(&store, project, &updated, selected(*epic)))
                .unwrap();
        }
        let third = area_request(&store, project, &updated, selected(epics[2]));
        assert!(
            store
                .appoint_area_node(&third)
                .unwrap_err()
                .to_string()
                .contains("manager_node_direct_report_cap")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn operator_node_request_replay_is_exact_and_durable() {
        let (store, project, root, _, epics) = area_fixture(true);
        let mut request = area_request(
            &store,
            project,
            &root,
            ManagerNodeSelectorV1::Selected {
                group_ids: vec![],
                epic_ids: vec![epics[0]],
            },
        );
        request.idempotency_key = Some("node-create-one".into());
        let created = store.appoint_area_node(&request).unwrap();
        let replay = store.appoint_area_node(&request).unwrap();
        assert_eq!(replay.id, created.id);
        assert_eq!(replay.grant_version, created.grant_version);
        request.selector = ManagerNodeSelectorV1::Selected {
            group_ids: vec![],
            epic_ids: vec![epics[1]],
        };
        assert!(
            store
                .appoint_area_node(&request)
                .unwrap_err()
                .to_string()
                .contains("manager_node_idempotency_conflict")
        );
        let revoke = RevokeAreaNode {
            idempotency_key: Some("node-revoke-one".into()),
            project_id: project,
            node_id: created.id,
            expected_grant_version: created.grant_version,
            expected_epoch: created.authority_epoch,
            operator_origin: "operator-test".into(),
        };
        assert_eq!(store.revoke_area_node(&revoke).unwrap(), vec![created.id]);
        assert_eq!(store.revoke_area_node(&revoke).unwrap(), vec![created.id]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn legacy_root_policy_change_rolls_back_while_delegate_is_live() {
        let (store, project, root, _, epics) = area_fixture(true);
        let child = store
            .appoint_area_node(&area_request(
                &store,
                project,
                &root,
                ManagerNodeSelectorV1::Selected {
                    group_ids: vec![],
                    epic_ids: vec![epics[0]],
                },
            ))
            .unwrap();
        let old_policy = store.get_harness_manager_policy(project).unwrap().unwrap();
        let mut changed = old_policy.policy.clone();
        changed.max_active_sessions = 99;
        let request = ConfigureHarnessManagerPolicyRequestV2 {
            project_id: project,
            expected_scope_version: old_policy.scope_version,
            expected_policy_version: old_policy.row_version,
            idempotency_key: "root-policy-after-child".into(),
            policy: changed,
        };
        assert!(
            store
                .configure_harness_manager_policy(&request)
                .unwrap_err()
                .to_string()
                .contains("manager_node_root_has_active_delegates")
        );
        assert_eq!(
            store
                .get_harness_manager_policy(project)
                .unwrap()
                .unwrap()
                .row_version,
            old_policy.row_version
        );
        assert_eq!(
            store
                .get_area_node(project, root.id)
                .unwrap()
                .unwrap()
                .grant_version,
            root.grant_version
        );
        store
            .revoke_area_node(&RevokeAreaNode {
                idempotency_key: None,
                project_id: project,
                node_id: child.id,
                expected_grant_version: child.grant_version,
                expected_epoch: child.authority_epoch,
                operator_origin: "operator-test".into(),
            })
            .unwrap();
        store.configure_harness_manager_policy(&request).unwrap();
        assert_eq!(
            store
                .get_area_node(project, root.id)
                .unwrap()
                .unwrap()
                .grant_version,
            root.grant_version + 1
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn area_nodes_refuse_stale_versions_and_absent_parent_grants() {
        let (store, project, root, _, epics) = area_fixture(true);
        let selector = ManagerNodeSelectorV1::Selected {
            group_ids: vec![],
            epic_ids: vec![epics[0]],
        };
        let mut request = area_request(&store, project, &root, selector);
        request.expected_parent_grant_version -= 1;
        assert!(
            store
                .appoint_area_node(&request)
                .unwrap_err()
                .to_string()
                .contains("manager_node_stale_parent")
        );
        request.expected_parent_grant_version += 1;
        let child = store.appoint_area_node(&request).unwrap();
        let stale = RevokeAreaNode {
            idempotency_key: None,
            project_id: project,
            node_id: child.id,
            expected_grant_version: child.grant_version,
            expected_epoch: child.authority_epoch - 1,
            operator_origin: "operator-test".into(),
        };
        assert!(
            store
                .revoke_area_node(&stale)
                .unwrap_err()
                .to_string()
                .contains("manager_node_stale_revoke")
        );
        let (other, project, root, _, epics) = area_fixture(false);
        let request = area_request(
            &other,
            project,
            &AreaNode {
                grant: Some(ManagerNodeGrantV1 {
                    capabilities: vec![ManagerCapabilityV2::WorkPlan],
                    allowed_launches: vec![],
                    allowance: ManagerNodeAllowanceV1 {
                        max_created_containers: 0,
                        max_created_sessions: 0,
                        max_active_sessions: 4,
                        max_build_slots: 0,
                        max_disk_gib: 0,
                        provider_limits: vec![],
                        max_spend_usd: None,
                    },
                    max_direct_reports: 5,
                }),
                ..root.clone()
            },
            ManagerNodeSelectorV1::Selected {
                group_ids: vec![],
                epic_ids: vec![epics[0]],
            },
        );
        assert!(
            other
                .appoint_area_node(&request)
                .unwrap_err()
                .to_string()
                .contains("manager_node_parent_grant_absent")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn revoking_area_node_revokes_its_subtree_and_epochs() {
        let (store, project, root, group, epics) = area_fixture(true);
        let parent = store
            .appoint_area_node(&area_request(
                &store,
                project,
                &root,
                ManagerNodeSelectorV1::Selected {
                    group_ids: vec![group],
                    epic_ids: vec![],
                },
            ))
            .unwrap();
        let child = store
            .appoint_area_node(&area_request(
                &store,
                project,
                &parent,
                ManagerNodeSelectorV1::Selected {
                    group_ids: vec![],
                    epic_ids: vec![epics[0]],
                },
            ))
            .unwrap();
        let revoked = store
            .revoke_area_node(&RevokeAreaNode {
                idempotency_key: None,
                project_id: project,
                node_id: parent.id,
                expected_grant_version: parent.grant_version,
                expected_epoch: parent.authority_epoch,
                operator_origin: "operator-test".into(),
            })
            .unwrap();
        assert_eq!(revoked.len(), 2);
        for id in [parent.id, child.id] {
            let node = store.get_area_node(project, id).unwrap().unwrap();
            assert!(!node.active);
            assert_eq!(node.authority_epoch, 2);
            assert_eq!(node.grant_version, 2);
            assert!(node.grant.is_none());
        }
        let active_reservations: i64 = store.conn.query_row("SELECT count(*) FROM manager_node_reservations WHERE state='active' AND child_node_id IN (?1,?2)",params![parent.id.to_string(),child.id.to_string()],|r| r.get(0)).unwrap();
        assert_eq!(active_reservations, 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    fn escalation_fixture() -> (
        Store,
        Uuid,
        AreaNode,
        AreaNode,
        AreaNode,
        AreaNode,
        Vec<Uuid>,
    ) {
        let (store, project, root, group, epics) = area_fixture(true);
        let parent = store
            .appoint_area_node(&area_request(
                &store,
                project,
                &root,
                ManagerNodeSelectorV1::Selected {
                    group_ids: vec![group],
                    epic_ids: vec![],
                },
            ))
            .unwrap();
        let child = |epic| {
            let mut request = area_request(
                &store,
                project,
                &parent,
                ManagerNodeSelectorV1::Selected {
                    group_ids: vec![],
                    epic_ids: vec![epic],
                },
            );
            request.grant.allowance.max_active_sessions = 1;
            request.policy.max_active_sessions = 1;
            store.appoint_area_node(&request).unwrap()
        };
        let left = child(epics[0]);
        let right = child(epics[1]);
        (store, project, root, parent, left, right, epics)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    fn escalate_request(
        project: Uuid,
        source: &AreaNode,
        target: &AreaNode,
        route: ManagerNodeEscalationRouteV1,
        key: &str,
    ) -> AgentManagerEscalateRequestV1 {
        AgentManagerEscalateRequestV1 {
            project_id: project,
            subject_id: Uuid::new_v4(),
            reason: "Resolve ownership conflict".into(),
            route,
            expected_source_authority_epoch: source.authority_epoch,
            expected_source_grant_version: source.grant_version,
            expected_target_authority_epoch: target.authority_epoch,
            expected_target_grant_version: target.grant_version,
            expected_target_session_id: target.seat_root_session_id,
            idempotency_key: key.into(),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn escalation_routes_conflict_to_nearest_ancestor_and_forwards_once() {
        let (store, project, root, parent, left, right, epics) = escalation_fixture();
        let request = escalate_request(
            project,
            &left,
            &parent,
            ManagerNodeEscalationRouteV1::EpicConflict {
                left_epic_id: epics[0],
                right_epic_id: epics[1],
            },
            "conflict-create",
        );
        let first = store
            .create_manager_node_escalation(left.seat_root_session_id, &request)
            .unwrap();
        assert_eq!(first.target_node_id, parent.id);
        assert_eq!(
            store
                .create_manager_node_escalation(left.seat_root_session_id, &request)
                .unwrap()
                .id,
            first.id
        );
        assert_eq!(
            store
                .list_manager_node_escalations(parent.seat_root_session_id, project)
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .list_manager_node_escalations(right.seat_root_session_id, project)
                .unwrap()
                .is_empty()
        );
        let mut collision = request.clone();
        collision.idempotency_key = "conflict-collision".into();
        assert!(
            store
                .create_manager_node_escalation(left.seat_root_session_id, &collision)
                .unwrap_err()
                .to_string()
                .contains("manager_node_escalation_subject_open")
        );
        let forward = AgentManagerResolveEscalationRequestV1 {
            escalation_id: first.id,
            expected_version: 1,
            expected_target_authority_epoch: parent.authority_epoch,
            expected_target_grant_version: parent.grant_version,
            expected_target_session_id: parent.seat_root_session_id,
            ruling: None,
            idempotency_key: "conflict-forward".into(),
        };
        let moved = store
            .resolve_manager_node_escalation(parent.seat_root_session_id, &forward)
            .unwrap();
        assert_eq!(moved.target_node_id, root.id);
        assert_eq!(moved.version, 2);
        assert_eq!(
            store
                .resolve_manager_node_escalation(parent.seat_root_session_id, &forward)
                .unwrap()
                .version,
            2
        );
        assert!(
            store
                .list_manager_node_escalations(parent.seat_root_session_id, project)
                .unwrap()
                .is_empty()
        );
        let ruling = AgentManagerResolveEscalationRequestV1 {
            escalation_id: first.id,
            expected_version: 2,
            expected_target_authority_epoch: root.authority_epoch,
            expected_target_grant_version: root.grant_version,
            expected_target_session_id: root.seat_root_session_id,
            ruling: Some("Assign the shared work to the parent scope".into()),
            idempotency_key: "conflict-rule".into(),
        };
        let ruled = store
            .resolve_manager_node_escalation(root.seat_root_session_id, &ruling)
            .unwrap();
        assert_eq!(ruled.state, ManagerNodeEscalationStateV1::Ruled);
        assert_eq!(ruled.version, 3);
        let events: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM manager_node_escalation_events WHERE escalation_id=?1",
                [first.id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 3);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn escalation_fences_stale_and_revoked_nodes_but_survives_seat_succession() {
        let (store, project, root, parent, left, _, _) = escalation_fixture();
        let mut request = escalate_request(
            project,
            &left,
            &parent,
            ManagerNodeEscalationRouteV1::Parent,
            "succession-create",
        );
        request.expected_source_authority_epoch += 1;
        assert!(
            store
                .create_manager_node_escalation(left.seat_root_session_id, &request)
                .unwrap_err()
                .to_string()
                .contains("manager_node_escalation_stale_source")
        );
        request.expected_source_authority_epoch = left.authority_epoch;
        request.expected_target_grant_version += 1;
        assert!(
            store
                .create_manager_node_escalation(left.seat_root_session_id, &request)
                .unwrap_err()
                .to_string()
                .contains("manager_node_escalation_stale_target")
        );
        request.expected_target_grant_version = parent.grant_version;
        let created = store
            .create_manager_node_escalation(left.seat_root_session_id, &request)
            .unwrap();
        let mut successor = store
            .get_session(parent.seat_root_session_id)
            .unwrap()
            .unwrap();
        successor.id = Uuid::new_v4();
        successor.continued_from = Some(parent.seat_root_session_id);
        successor.rotation_depth += 1;
        store.insert_session(&successor).unwrap();
        store
            .update_session_status(parent.seat_root_session_id, SessionStatus::Archived)
            .unwrap();
        store
            .record_harness_manager_rotation(parent.seat_root_session_id, successor.id)
            .unwrap();
        assert_eq!(
            store
                .list_manager_node_escalations(successor.id, project)
                .unwrap()[0]
                .id,
            created.id
        );
        let ruling = AgentManagerResolveEscalationRequestV1 {
            escalation_id: created.id,
            expected_version: 1,
            expected_target_authority_epoch: parent.authority_epoch,
            expected_target_grant_version: parent.grant_version,
            expected_target_session_id: successor.id,
            ruling: Some("Parent owns the work".into()),
            idempotency_key: "succession-rule".into(),
        };
        let mut stale_custody = ruling.clone();
        stale_custody.expected_target_session_id = parent.seat_root_session_id;
        assert!(
            store
                .resolve_manager_node_escalation(successor.id, &stale_custody)
                .unwrap_err()
                .to_string()
                .contains("manager_node_escalation_stale_target")
        );
        assert_eq!(
            store
                .resolve_manager_node_escalation(successor.id, &ruling)
                .unwrap()
                .state,
            ManagerNodeEscalationStateV1::Ruled
        );
        store
            .revoke_area_node(&RevokeAreaNode {
                idempotency_key: None,
                project_id: project,
                node_id: parent.id,
                expected_grant_version: parent.grant_version,
                expected_epoch: parent.authority_epoch,
                operator_origin: "operator-test".into(),
            })
            .unwrap();
        let next = escalate_request(
            project,
            &left,
            &root,
            ManagerNodeEscalationRouteV1::Parent,
            "after-revoke",
        );
        assert!(
            store
                .create_manager_node_escalation(left.seat_root_session_id, &next)
                .unwrap_err()
                .to_string()
                .contains("manager_node_escalation_revoked")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn revoked_escalation_source_cannot_receive_a_later_ruling() {
        let (store, project, _root, parent, left, _right, _epics) = escalation_fixture();
        let request = escalate_request(
            project,
            &left,
            &parent,
            ManagerNodeEscalationRouteV1::Parent,
            "revoked-source",
        );
        let created = store
            .create_manager_node_escalation(left.seat_root_session_id, &request)
            .unwrap();
        store
            .revoke_area_node(&RevokeAreaNode {
                idempotency_key: None,
                project_id: project,
                node_id: left.id,
                expected_grant_version: left.grant_version,
                expected_epoch: left.authority_epoch,
                operator_origin: "operator-test".into(),
            })
            .unwrap();
        let ruling = AgentManagerResolveEscalationRequestV1 {
            escalation_id: created.id,
            expected_version: 1,
            expected_target_authority_epoch: parent.authority_epoch,
            expected_target_grant_version: parent.grant_version,
            expected_target_session_id: parent.seat_root_session_id,
            ruling: Some("Proceed".into()),
            idempotency_key: "revoked-source-rule".into(),
        };
        assert!(
            store
                .resolve_manager_node_escalation(parent.seat_root_session_id, &ruling)
                .unwrap_err()
                .to_string()
                .contains("manager_node_escalation_revoked")
        );
    }
}
