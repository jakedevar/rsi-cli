//! One bounded accounting cohort for manager actions and Model Control admission.
//!
//! Creation/rotation receipts and spend checkpoints retain historical charges.
//! They are never live action authority: decisions resolve current legal scope
//! independently. All checkpoints are written inside Model Control's admission
//! transaction, before a new invocation can replace the session's cost estimate.

use std::collections::{BTreeMap, HashSet};

use chrono::Utc;
use rsi_common::harness_manager::HarnessManagerConfigV1;
use rsi_common::types::{Session, SessionKind, SessionProvider, SessionStatus};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::Store;
use super::harness_manager_v2::refused;
use super::manager_coordinator::ManagerCohortMemberV2;
use crate::error::Result;

const COHORT_BUDGET: usize = 1024;
const ROTATION_LIMIT: i64 = 64;
const SPEND_KIND: &str = "resource_spend";
const ORIGIN_KIND: &str = "resource_launch_origin";

mod launch_origin;

/// Daemon-only witness for a new child, never deserialized from an RPC request.
/// The transaction revalidates the parent and current policy before reserving it.
#[derive(Clone)]
pub(crate) struct ManagerResourceLaunchOrigin {
    session_id: Uuid,
    parent_id: Option<Uuid>,
    project_id: Option<Uuid>,
    kind: SessionKind,
    scope: Option<(Uuid, i64)>,
    policy_required: bool,
    reservation: Option<launch_origin::LaunchReservation>,
}

#[derive(Clone, Serialize, Deserialize)]
struct SpendCheckpoint {
    /// A known lower bound, not an assertion that legacy and ledger costs add.
    known_floor_usd: f64,
    /// Only observed before any admitted invocation, with proven zero history.
    zero_origin: bool,
}

fn valid_cost(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite() && *value >= 0.0)
}

impl Store {
    fn manager_v2_root_origins(
        &self,
        project: Uuid,
    ) -> Result<BTreeMap<Uuid, super::manager_successions::ManagerRootResourceOrigin>> {
        let mut roots = BTreeMap::new();
        let mut stmt = self.conn.prepare(
            "SELECT r.session_id,r.project_id,r.manager_session_id,r.scope_version,
                    r.operation_id,r.predecessor_session_id,r.model_invocation_id,
                    r.provider,r.zero_origin,r.known_floor_usd
             FROM manager_root_resource_origins r LEFT JOIN sessions s ON s.id=r.session_id
             WHERE r.project_id=?1 AND (s.id IS NULL
               OR s.status NOT IN ('Completed','Failed','Interrupted','Archived','Deleted')
               OR EXISTS(SELECT 1 FROM model_invocations i WHERE i.session_id=r.session_id
                  AND i.admission_status='admitted'
                  AND i.status IN ('running','cancellation_requested')))
             ORDER BY r.session_id LIMIT 1025",
        )?;
        let rows = stmt
            .query_map([project.to_string()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, bool>(8)?,
                    r.get::<_, f64>(9)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if rows.len() > COHORT_BUDGET {
            return Err(refused("manager_v2_cohort_subdivision_required"));
        }
        for (id, p, m, scope, op, pred, inv, provider, zero, floor) in rows {
            let parse = |value: String| {
                Uuid::parse_str(&value).map_err(|_| refused("manager_v2_invalid_stored_identity"))
            };
            let origin = super::manager_successions::ManagerRootResourceOrigin {
                session_id: parse(id)?,
                project_id: parse(p)?,
                manager_session_id: parse(m)?,
                scope_version: scope,
                operation_id: op.map(&parse).transpose()?,
                predecessor_session_id: pred.map(&parse).transpose()?,
                model_invocation_id: inv.map(&parse).transpose()?,
                provider: serde_json::from_value(json!(provider))?,
                zero_origin: zero,
                known_floor_usd: floor,
            };
            roots.insert(origin.session_id, origin);
        }
        Ok(roots)
    }

    pub(crate) fn manager_succession_resource_gate(
        &self,
        claim: &super::manager_successions::ManagerSuccessionClaim,
        existing: Option<Uuid>,
    ) -> Result<HarnessManagerConfigV1> {
        let root = self.manager_succession_effect_gate_on(claim)?;
        let config = self
            .get_harness_manager(root.project_id)?
            .ok_or_else(|| refused("manager_succession_appointment_missing"))?;
        if existing.is_some_and(|id| id != root.candidate_session_id) {
            return Err(refused("manager_succession_resource_identity_changed"));
        }
        super::manager_resources::launch_origin::validate_root_choice(&root.frozen.launch)?;
        self.manager_v2_resource_gate(&config, None, root.frozen.launch.provider, existing)?;
        Ok(config)
    }

    pub(crate) fn manager_v2_launch_origin(
        &self,
        session_id: Uuid,
        parent_id: Uuid,
        project_id: Option<Uuid>,
        kind: SessionKind,
    ) -> Result<ManagerResourceLaunchOrigin> {
        let scope = self.manager_v2_launch_parent_scope(parent_id, project_id, kind)?;
        let policy_required = match scope.as_ref() {
            Some((config, _)) => self
                .get_harness_manager_policy(config.project_id)?
                .is_some_and(|policy| !policy.revoked),
            None => false,
        };
        Ok(ManagerResourceLaunchOrigin {
            session_id,
            parent_id: Some(parent_id),
            project_id,
            kind,
            scope: scope.map(|(config, _)| (config.manager_session_id, config.row_version)),
            policy_required,
            reservation: None,
        })
    }

    fn manager_v2_launch_parent_scope(
        &self,
        parent: Uuid,
        project: Option<Uuid>,
        child_kind: SessionKind,
    ) -> Result<Option<(HarnessManagerConfigV1, Option<Uuid>)>> {
        let mut current = self
            .get_session(parent)?
            .ok_or_else(|| refused("manager_v2_launch_parent_missing"))?;
        if current.project_id != project
            || !rsi_common::legal_children(Some(current.session_kind)).contains(&child_kind)
        {
            return Err(refused("manager_v2_launch_parent_changed"));
        }
        let config = project
            .map(|project| self.get_harness_manager(project))
            .transpose()?
            .flatten();
        let mut seen = HashSet::new();
        for _ in 0..64 {
            if !seen.insert(current.id) {
                return Err(refused("manager_v2_cohort_cycle"));
            }
            if matches!(
                current.status,
                SessionStatus::Archived | SessionStatus::Deleted
            ) {
                return Err(refused("manager_v2_launch_parent_retired"));
            }
            if let Some(config) = config.as_ref() {
                if current.session_kind == SessionKind::Epic
                    && config.epic_ids.contains(&current.id)
                {
                    self.manager_epic(config.project_id, current.id)?;
                    return Ok(Some((config.clone(), Some(current.id))));
                }
            }
            let Some(parent) = current.parent_id else {
                return Ok(None);
            };
            let previous_kind = current.session_kind;
            current = self
                .get_session(parent)?
                .ok_or_else(|| refused("manager_v2_illegal_parent"))?;
            if current.project_id != project
                || !rsi_common::legal_children(Some(current.session_kind)).contains(&previous_kind)
            {
                return Err(refused("manager_v2_launch_parent_changed"));
            }
        }
        Err(refused("manager_v2_cohort_subdivision_required"))
    }

    /// Called inside the immediate Model Control transaction; a preflight is
    /// neither a slot reservation nor permission to ignore a newer policy.
    pub(crate) fn manager_v2_admit_launch_origin(
        &self,
        origin: &ManagerResourceLaunchOrigin,
        request: &crate::model_control::ModelAdmissionRequest,
        provider: SessionProvider,
    ) -> Result<Option<(HarnessManagerConfigV1, Option<Uuid>)>> {
        if request.owner.session_id != Some(origin.session_id)
            || request.owner.project_id != origin.project_id
            || self.get_session(origin.session_id)?.is_some()
            || self.manager_v2_ledger_spend(origin.session_id)?.0 != 0
        {
            return Err(refused("manager_v2_launch_identity_changed"));
        }
        self.manager_v2_check_launch_origin(origin, request, provider, None)
    }

    /// An exact unexecuted successor/closure replay can reuse a permit only
    /// after the same current checks. Existing accounting is credited once;
    /// a legacy unrecorded reservation is charged before returning authority.
    pub(crate) fn manager_v2_replay_launch_origin(
        &self,
        origin: &ManagerResourceLaunchOrigin,
        request: &crate::model_control::ModelAdmissionRequest,
        provider: SessionProvider,
        invocation: Uuid,
    ) -> Result<()> {
        if request.owner.session_id != Some(origin.session_id)
            || request.owner.project_id != origin.project_id
            || self.get_session(origin.session_id)?.is_some()
            || self.manager_v2_ledger_spend(origin.session_id)?.0 != 1
        {
            return Err(refused("manager_v2_launch_identity_changed"));
        }
        if let Some((config, epic)) =
            self.manager_v2_check_launch_origin(origin, request, provider, Some(origin.session_id))?
        {
            if let Some(record) = self
                .manager_v2_selected_launch_records(&config)?
                .into_iter()
                .find(|record| record.key == origin.session_id.to_string())
            {
                if record.epic_id != epic
                    || record.payload["parent_id"] != json!(origin.parent_id)
                    || record.payload["provider"] != json!(provider)
                    || record.payload["invocation_id"] != json!(invocation)
                {
                    return Err(refused("manager_v2_launch_reservation_changed"));
                }
            } else {
                self.manager_v2_record_launch_origin(origin, &config, epic, provider, invocation)?;
            }
        }
        Ok(())
    }

    fn manager_v2_check_launch_origin(
        &self,
        origin: &ManagerResourceLaunchOrigin,
        request: &crate::model_control::ModelAdmissionRequest,
        provider: SessionProvider,
        existing_session: Option<Uuid>,
    ) -> Result<Option<(HarnessManagerConfigV1, Option<Uuid>)>> {
        origin.validate_reservation(self, Some((request, provider)))?;
        if let Some(claim) = origin.manager_succession_claim() {
            return self
                .manager_succession_resource_gate(claim, existing_session)
                .map(|config| Some((config, None)));
        }
        let scope = self.manager_v2_launch_parent_scope(
            origin
                .parent_id
                .ok_or_else(|| refused("manager_v2_launch_parent_missing"))?,
            origin.project_id,
            origin.kind,
        )?;
        if origin.scope.is_some()
            && origin.scope
                != scope
                    .as_ref()
                    .map(|(c, _)| (c.manager_session_id, c.row_version))
        {
            return Err(refused("manager_v2_launch_scope_changed"));
        }
        if let Some((config, epic)) = scope.as_ref() {
            if origin.policy_required
                && self
                    .get_harness_manager_policy(config.project_id)?
                    .is_none_or(|g| g.revoked)
            {
                return Err(refused("manager_v2_launch_scope_changed"));
            }
            self.manager_v2_resource_gate(config, *epic, provider, existing_session)?;
        }
        Ok(scope)
    }

    pub(crate) fn manager_v2_record_launch_origin(
        &self,
        origin: &ManagerResourceLaunchOrigin,
        config: &HarnessManagerConfigV1,
        epic: Option<Uuid>,
        provider: SessionProvider,
        invocation: Uuid,
    ) -> Result<()> {
        if let Some(claim) = origin.manager_succession_claim() {
            self.record_manager_succession_admission_on(claim)?;
            return Ok(());
        }
        let key = origin.session_id.to_string();
        self.manager_v2_put_record(
            config,
            ORIGIN_KIND,
            &key,
            epic,
            0,
            &json!({"parent_id":origin.parent_id,"provider":provider,"invocation_id":invocation}),
        )?;
        // No prior session or admitted history existed at this admission. Keep
        // that proof after failed publication, settlement and reopen.
        self.manager_v2_put_record(
            config,
            SPEND_KIND,
            &key,
            epic,
            0,
            &serde_json::to_value(SpendCheckpoint {
                known_floor_usd: 0.0,
                zero_origin: true,
            })?,
        )?;
        Ok(())
    }

    /// Materialize only current members. Historical descendants and receipts
    /// are traversed in SQL so retired rows cannot exhaust the live budget.
    #[allow(clippy::too_many_lines)] // The recursive selection and row validation stay together.
    pub(crate) fn manager_v2_cohort(
        &self,
        config: &HarnessManagerConfigV1,
    ) -> Result<Vec<ManagerCohortMemberV2>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE descendants(id,epic_id,depth) AS (
                SELECT value,value,0 FROM json_each(?1)
                UNION ALL
                SELECT s.id,d.epic_id,d.depth+1 FROM sessions s
                JOIN descendants d ON s.parent_id=d.id WHERE d.depth<?5
             ), seeds(id) AS (
                SELECT id FROM descendants
                UNION SELECT session_id FROM harness_manager_v2_entities
                  WHERE project_id=?2 AND manager_session_id=?3 AND scope_version=?4
                    AND kind NOT IN ('Epic','Group')
                UNION SELECT record_key FROM harness_manager_v2_records
                  WHERE project_id=?2 AND manager_session_id=?3 AND scope_version=?4
                    AND kind='resource_spend'
                UNION SELECT record_key FROM harness_manager_v2_records
                  WHERE project_id=?2 AND kind='resource_launch_origin'
                    AND epic_id IN (SELECT value FROM json_each(?1))
                UNION SELECT session_id FROM manager_root_resource_origins WHERE project_id=?2
             ), lineage(id,depth,path,cycle) AS (
                SELECT id,0,','||id||',',0 FROM seeds
                UNION ALL SELECT e.successor_session_id,l.depth+1,
                  l.path||e.successor_session_id||',',
                  instr(l.path,','||e.successor_session_id||',')>0
                  FROM harness_manager_rotation_edges e JOIN lineage l ON e.predecessor_session_id=l.id
                  WHERE l.depth<?5 AND l.cycle=0
             )
             SELECT l.id,
               (SELECT d.epic_id FROM descendants d WHERE d.id=l.id LIMIT 1),
               MAX(l.depth),
               MAX(l.depth>=?5 AND EXISTS(SELECT 1 FROM harness_manager_rotation_edges e
                      WHERE e.predecessor_session_id=l.id)),
               MAX(l.cycle),
               MAX(EXISTS(SELECT 1 FROM harness_manager_rotation_edges e
                   JOIN sessions p ON p.id=e.predecessor_session_id
                   LEFT JOIN sessions next ON next.id=e.successor_session_id
                   LEFT JOIN session_lineage_detachments det ON det.session_id=next.id
                   WHERE e.predecessor_session_id=l.id AND
                     (next.id IS NULL OR p.project_id IS NOT ?2 OR next.project_id IS NOT ?2
                      OR next.session_kind IS NOT p.session_kind
                      OR next.rotation_depth IS NOT p.rotation_depth+1
                      OR (next.continued_from IS NOT p.id AND NOT
                          (next.continued_from IS NULL AND COALESCE(det.detached_from_session_id=p.id,0))))))
             FROM lineage l LEFT JOIN sessions s ON s.id=l.id
             WHERE s.status NOT IN ('Completed','Failed','Interrupted','Archived','Deleted')
                OR (s.session_kind='Epic' AND l.id IN (SELECT value FROM json_each(?1)))
                OR s.id IN (SELECT lead_session_id FROM sessions
                            WHERE id IN (SELECT value FROM json_each(?1)))
                OR EXISTS(SELECT 1 FROM model_invocations i WHERE i.session_id=l.id
                    AND i.admission_status='admitted'
                    AND i.status IN ('running','cancellation_requested'))
                OR l.cycle
                OR (l.depth>=?5 AND EXISTS(SELECT 1 FROM harness_manager_rotation_edges e
                                           WHERE e.predecessor_session_id=l.id))
                OR EXISTS(SELECT 1 FROM harness_manager_rotation_edges e
                   JOIN sessions p ON p.id=e.predecessor_session_id
                   LEFT JOIN sessions next ON next.id=e.successor_session_id
                   LEFT JOIN session_lineage_detachments det ON det.session_id=next.id
                   WHERE e.predecessor_session_id=l.id AND
                     (next.id IS NULL OR p.project_id IS NOT ?2 OR next.project_id IS NOT ?2
                      OR next.session_kind IS NOT p.session_kind
                      OR next.rotation_depth IS NOT p.rotation_depth+1
                      OR (next.continued_from IS NOT p.id AND NOT
                          (next.continued_from IS NULL AND COALESCE(det.detached_from_session_id=p.id,0)))))
                OR (s.id IS NULL AND EXISTS(SELECT 1 FROM harness_manager_v2_records r
                   WHERE r.record_key=l.id AND r.kind='resource_launch_origin'
                     AND r.project_id=?2))
             GROUP BY l.id LIMIT 1025",
        )?;
        let raw = stmt
            .query_map(
                params![
                    serde_json::to_string(&config.epic_ids)?,
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    ROTATION_LIMIT
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, usize>(2)?,
                        r.get::<_, bool>(3)?,
                        r.get::<_, bool>(4)?,
                        r.get::<_, bool>(5)?,
                    ))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if raw.len() > COHORT_BUDGET || raw.iter().any(|r| r.3) {
            return Err(refused("manager_v2_cohort_subdivision_required"));
        }
        if raw.iter().any(|r| r.4) {
            return Err(refused("manager_v2_cohort_cycle"));
        }
        if raw.iter().any(|r| r.5) {
            return Err(refused("manager_v2_rotation_attribution_changed"));
        }
        let mut members = BTreeMap::new();
        for (id, epic, _, _, _, _) in raw {
            let id =
                Uuid::parse_str(&id).map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
            let epic = epic
                .map(|value| {
                    Uuid::parse_str(&value)
                        .map_err(|_| refused("manager_v2_invalid_stored_identity"))
                })
                .transpose()?;
            let Some(session) = self.get_session(id)? else {
                continue; // An admitted but unpublished launch is a reservation.
            };
            if session.project_id != Some(config.project_id) {
                return Err(refused("manager_v2_cohort_project_mismatch"));
            }
            if epic == Some(id) {
                if session.session_kind != SessionKind::Epic {
                    return Err(refused("manager_v2_legal_epic_required"));
                }
            } else if epic.is_some() {
                let parent = self
                    .get_session(
                        session
                            .parent_id
                            .ok_or_else(|| refused("manager_v2_illegal_parent"))?,
                    )?
                    .ok_or_else(|| refused("manager_v2_illegal_parent"))?;
                if !rsi_common::legal_children(Some(parent.session_kind))
                    .contains(&session.session_kind)
                {
                    return Err(refused("manager_v2_illegal_parent"));
                }
            }
            members.insert(
                id,
                ManagerCohortMemberV2 {
                    session,
                    epic_id: epic,
                },
            );
        }
        Ok(members.into_values().collect())
    }

    /// Exact current selected Epic; historical accounting never grants this.
    pub(crate) fn manager_v2_live_epic_for_session(
        &self,
        config: &HarnessManagerConfigV1,
        id: Uuid,
    ) -> Result<Uuid> {
        let session = self
            .get_session(id)?
            .ok_or_else(|| refused("manager_v2_decision_target_changed"))?;
        let epic = session
            .parent_id
            .ok_or_else(|| refused("manager_v2_decision_target_changed"))?;
        if session.project_id != Some(config.project_id)
            || matches!(
                session.status,
                SessionStatus::Archived | SessionStatus::Deleted
            )
            || !rsi_common::legal_children(Some(SessionKind::Epic)).contains(&session.session_kind)
            || !config.epic_ids.contains(&epic)
        {
            return Err(refused("manager_v2_decision_target_changed"));
        }
        self.manager_epic(config.project_id, epic)?;
        Ok(epic)
    }

    fn manager_v2_ledger_spend(&self, id: Uuid) -> Result<(i64, f64, i64)> {
        let (count, known, unknown): (i64, f64, i64) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(CASE WHEN estimated_cost_usd>=0 THEN estimated_cost_usd ELSE 0 END),0),
                COALESCE(SUM(estimated_cost_usd IS NULL OR estimated_cost_usd<0),0)
             FROM model_invocations WHERE session_id=?1 AND admission_status='admitted'",
            [id.to_string()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok((
            count,
            valid_cost(Some(known)).unwrap_or(0.0),
            unknown + i64::from(!known.is_finite()),
        ))
    }

    /// SQL keeps the historical identity set and per-session maxima inside
    /// `SQLite`. Only the two totals cross into the manager snapshot.
    fn manager_v2_historical_spend(&self, config: &HarnessManagerConfigV1) -> Result<(f64, i64)> {
        let totals = self.conn.query_row(
            "WITH RECURSIVE descendants(id,depth) AS (
                SELECT value,0 FROM json_each(?1)
                UNION ALL SELECT s.id,d.depth+1 FROM sessions s
                  JOIN descendants d ON s.parent_id=d.id WHERE d.depth<64
             ), seeds(id) AS (
                SELECT id FROM descendants
                UNION SELECT session_id FROM harness_manager_v2_entities
                  WHERE project_id=?2 AND manager_session_id=?3 AND scope_version=?4
                    AND kind NOT IN ('Epic','Group')
                UNION SELECT record_key FROM harness_manager_v2_records
                  WHERE project_id=?2 AND manager_session_id=?3 AND scope_version=?4
                    AND kind='resource_spend'
                UNION SELECT record_key FROM harness_manager_v2_records
                  WHERE project_id=?2 AND kind='resource_launch_origin'
                    AND epic_id IN (SELECT value FROM json_each(?1))
                UNION SELECT session_id FROM manager_root_resource_origins WHERE project_id=?2
             ), lineage(id,depth) AS (
                SELECT id,0 FROM seeds
                UNION SELECT e.successor_session_id,l.depth+1
                  FROM harness_manager_rotation_edges e JOIN lineage l ON e.predecessor_session_id=l.id
                  WHERE l.depth<64
             ), ids AS (
                SELECT DISTINCT id FROM lineage
             ), ledger AS (
                SELECT i.session_id,COUNT(*) count,
                  COALESCE(SUM(CASE WHEN i.estimated_cost_usd>=0 THEN i.estimated_cost_usd ELSE 0 END),0) cost,
                  COALESCE(SUM(i.estimated_cost_usd IS NULL OR i.estimated_cost_usd<0),0) missing
                FROM model_invocations i JOIN ids ON ids.id=i.session_id
                WHERE i.admission_status='admitted' GROUP BY i.session_id
             ), checkpoints AS (
                SELECT c.record_key,
                  json_extract(c.payload_json,'$.zero_origin') zero_origin,
                  json_extract(c.payload_json,'$.known_floor_usd') known_floor_usd,
                  ROW_NUMBER() OVER (PARTITION BY c.record_key ORDER BY
                    (c.manager_session_id=?3 AND c.scope_version=?4) DESC,
                    c.scope_version DESC) rank
                FROM harness_manager_v2_records c JOIN ids ON ids.id=c.record_key
                WHERE c.project_id=?2 AND c.kind='resource_spend'
                  AND ((c.manager_session_id=?3 AND c.scope_version=?4)
                    OR (c.epic_id IN (SELECT value FROM json_each(?1))
                      AND EXISTS(SELECT 1 FROM harness_manager_v2_records o
                        WHERE o.project_id=c.project_id AND o.manager_session_id=c.manager_session_id
                          AND o.scope_version=c.scope_version AND o.record_key=c.record_key
                          AND o.kind='resource_launch_origin')))
             ), measured AS (
                SELECT ids.id,s.id session_id,
                  COALESCE(l.count,0) count,COALESCE(l.cost,0) ledger,
                  COALESCE(l.missing,0) missing,
                  CASE WHEN s.cost_usd>=0 AND s.cost_usd<=1e308 THEN s.cost_usd
                    WHEN s.status='Starting' AND s.cost_usd IS NULL
                      AND NOT EXISTS(SELECT 1 FROM conversation_events ce WHERE ce.session_id=s.id)
                    THEN 0.0 ELSE NULL END legacy,
                  r.zero_origin root_zero,r.known_floor_usd root_floor,
                  c.zero_origin checkpoint_zero,c.known_floor_usd checkpoint_floor
                FROM ids LEFT JOIN sessions s ON s.id=ids.id
                  LEFT JOIN ledger l ON l.session_id=ids.id
                  LEFT JOIN manager_root_resource_origins r ON r.session_id=ids.id AND r.project_id=?2
                  LEFT JOIN checkpoints c ON c.record_key=ids.id AND c.rank=1
                WHERE (s.id IS NULL OR (s.project_id=?2 AND s.session_kind NOT IN ('Epic','Group')))
             )
             SELECT COALESCE(SUM(MAX(ledger,COALESCE(legacy,0),
                         COALESCE(root_floor,checkpoint_floor,0))),0),
                    COALESCE(SUM(CASE WHEN session_id IS NULL THEN
                       missing + (COALESCE(root_zero,checkpoint_zero,0)=0)
                      WHEN count=0 THEN (legacy IS NULL OR (root_zero IS NOT NULL AND root_zero=0))
                      ELSE missing + (COALESCE(root_zero,checkpoint_zero,0)=0) END),0)
             FROM measured",
            params![serde_json::to_string(&config.epic_ids)?,config.project_id.to_string(),
                config.manager_session_id.to_string(),config.row_version],
            |row| Ok((row.get::<_, f64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        Ok(totals)
    }

    fn manager_v2_legacy_spend(&self, session: &Session) -> Result<Option<f64>> {
        if let Some(cost) = valid_cost(session.cost_usd) {
            return Ok(Some(cost));
        }
        if session.status == SessionStatus::Starting && session.cost_usd.is_none() {
            let events: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM conversation_events WHERE session_id=?1)",
                [session.id.to_string()],
                |r| r.get(0),
            )?;
            if !events {
                return Ok(Some(0.0));
            }
        }
        Ok(None)
    }

    /// Shared positive attribution predicate for both the bounded verifier and
    /// its SQL prefilter. `session_id_expr` is an internal SQL expression.
    fn manager_v2_rotation_spend_attribution_predicate(session_id_expr: &str) -> String {
        format!(
            "EXISTS(SELECT 1 FROM manager_root_resource_origins
                   WHERE session_id={session_id_expr} AND project_id=?2)
             OR EXISTS(SELECT 1 FROM harness_manager_v2_entities
                       WHERE session_id={session_id_expr} AND project_id=?2
                         AND manager_session_id=?3 AND scope_version=?4
                         AND kind NOT IN ('Epic','Group'))
             OR EXISTS(SELECT 1 FROM harness_manager_v2_records
                       WHERE record_key={session_id_expr} AND project_id=?2
                         AND manager_session_id=?3 AND scope_version=?4
                         AND kind='resource_spend')
             OR EXISTS(SELECT 1 FROM sessions
                       WHERE id={session_id_expr}
                         AND parent_id IN (SELECT value FROM json_each(?5)))"
        )
    }

    /// Follow only exact rotation receipts back to a persisted spend origin.
    /// Each step uses indexed identity/edge lookups, so retired history never
    /// enters the live cohort or an unbounded Rust collection.
    fn manager_v2_rotation_spend_attributed(
        &self,
        config: &HarnessManagerConfigV1,
        session: &Session,
    ) -> Result<bool> {
        let mut current = session.clone();
        let mut seen = HashSet::new();
        for depth in 0..=ROTATION_LIMIT {
            if !seen.insert(current.id) {
                return Err(refused("manager_v2_cohort_cycle"));
            }
            if depth > 0 {
                let predicate = Self::manager_v2_rotation_spend_attribution_predicate("?1");
                let origin: bool = self.conn.query_row(
                    &format!("SELECT {predicate}"),
                    params![
                        current.id.to_string(),
                        config.project_id.to_string(),
                        config.manager_session_id.to_string(),
                        config.row_version,
                        serde_json::to_string(&config.epic_ids)?
                    ],
                    |row| row.get(0),
                )?;
                if origin {
                    return Ok(true);
                }
            }
            let predecessor_id = match current.continued_from {
                Some(id) => Some(id),
                None => self.lineage_detachment_predecessor(current.id)?,
            };
            let Some(predecessor_id) = predecessor_id else {
                return Ok(false);
            };
            let receipt: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM harness_manager_rotation_edges
                 WHERE predecessor_session_id=?1 AND successor_session_id=?2)",
                params![predecessor_id.to_string(), current.id.to_string()],
                |row| row.get(0),
            )?;
            if !receipt {
                return Ok(false);
            }
            if depth == ROTATION_LIMIT {
                return Err(refused("manager_v2_cohort_subdivision_required"));
            }
            let predecessor = self
                .get_session(predecessor_id)?
                .ok_or_else(|| refused("manager_v2_rotation_attribution_changed"))?;
            if predecessor.project_id != Some(config.project_id)
                || current.project_id != Some(config.project_id)
                || predecessor.session_kind != current.session_kind
                || predecessor.rotation_depth.checked_add(1) != Some(current.rotation_depth)
            {
                return Err(refused("manager_v2_rotation_attribution_changed"));
            }
            current = predecessor;
        }
        Err(refused("manager_v2_cohort_subdivision_required"))
    }

    /// Avoid applying the bounded spend walk to sessions whose exact rotation
    /// lineage has no manager attribution. This probe stays in `SQLite` and only
    /// follows receipted edges; the bounded Rust walk below remains the source
    /// of validation and refusal behavior for attributed lineages.
    fn manager_v2_rotation_lineage_has_spend_attribution(
        &self,
        config: &HarnessManagerConfigV1,
        session: &Session,
    ) -> Result<bool> {
        let predicate = Self::manager_v2_rotation_spend_attribution_predicate("lineage.id");
        self.conn
            .query_row(
                &format!(
                    "WITH RECURSIVE lineage(id) AS (
                    SELECT ?1
                    UNION
                    SELECT predecessor.id
                    FROM lineage
                    JOIN sessions successor ON successor.id=lineage.id
                    JOIN sessions predecessor ON predecessor.id=CASE
                        WHEN successor.continued_from IS NOT NULL THEN successor.continued_from
                        ELSE (SELECT detached_from_session_id FROM session_lineage_detachments
                              WHERE session_id=successor.id)
                    END
                    JOIN harness_manager_rotation_edges edge
                      ON edge.predecessor_session_id=predecessor.id
                     AND edge.successor_session_id=successor.id
                )
                SELECT EXISTS(
                    SELECT 1 FROM lineage
                    WHERE {predicate}
                )"
                ),
                params![
                    session.id.to_string(),
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    serde_json::to_string(&config.epic_ids)?
                ],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Called only in the existing immediate Model Control admission transaction.
    /// A positive legacy amount and a later aggregate may overlap: preserve the
    /// known floor and declare coverage unknown instead of dropping or adding it.
    pub(crate) fn manager_v2_capture_resource_spend(&self, session_id: Uuid) -> Result<()> {
        let Some(session) = self.get_session(session_id)? else {
            return Ok(());
        };
        let Some(project) = session.project_id else {
            return Ok(());
        };
        let Some(config) = self.get_harness_manager(project)? else {
            return Ok(());
        };
        if self
            .get_harness_manager_policy(project)?
            .is_none_or(|g| g.revoked)
        {
            return Ok(());
        }
        // A finished session drops out of the live cohort. Its next admission
        // must still checkpoint the legacy floor before Model Control resets
        // the session estimate. Resolve this one identity without loading the
        // historical cohort into Rust.
        let root: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM manager_root_resource_origins
             WHERE session_id=?1 AND project_id=?2)",
            params![session_id.to_string(), project.to_string()],
            |row| row.get(0),
        )?;
        // Follow this session's persisted ancestry, including retired rows.
        // A direct-parent check misses descendants below an Epic child, while
        // loading the live cohort would discard this completed identity.
        let epic = self
            .conn
            .query_row(
                "WITH RECURSIVE ancestry(id,parent_id,depth) AS (
                    SELECT id,parent_id,0 FROM sessions WHERE id=?1 AND project_id=?2
                    UNION ALL SELECT p.id,p.parent_id,a.depth+1 FROM sessions p
                    JOIN ancestry a ON p.id=a.parent_id
                    WHERE a.depth<64 AND p.project_id=?2
                 )
                 SELECT id FROM ancestry WHERE id IN (SELECT value FROM json_each(?3)) LIMIT 1",
                params![
                    session_id.to_string(),
                    project.to_string(),
                    serde_json::to_string(&config.epic_ids)?
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|id| {
                Uuid::parse_str(&id).map_err(|_| refused("manager_v2_invalid_stored_identity"))
            })
            .transpose()?;
        let attributed: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_entities
                 WHERE session_id=?1 AND project_id=?2 AND manager_session_id=?3
                   AND scope_version=?4 AND kind NOT IN ('Epic','Group'))
               OR EXISTS(SELECT 1 FROM harness_manager_v2_records
                 WHERE record_key=?1 AND project_id=?2 AND manager_session_id=?3
                   AND scope_version=?4 AND kind='resource_spend')",
            params![
                session_id.to_string(),
                project.to_string(),
                config.manager_session_id.to_string(),
                config.row_version
            ],
            |row| row.get(0),
        )?;
        if !root
            && epic.is_none()
            && !attributed
            && (!self.manager_v2_rotation_lineage_has_spend_attribution(&config, &session)?
                || !self.manager_v2_rotation_spend_attributed(&config, &session)?)
        {
            return Ok(());
        }
        if root {
            let (_, ledger, _) = self.manager_v2_ledger_spend(session_id)?;
            let legacy = self.manager_v2_legacy_spend(&session)?;
            self.record_manager_root_spend_floor(session_id, ledger.max(legacy.unwrap_or(0.0)))?;
            return Ok(());
        }
        let prior = self.manager_v2_record(&config, SPEND_KIND, &session_id.to_string())?;
        let prior_value = self.manager_v2_spend_checkpoint(&config, session_id)?;
        let (count, ledger, _) = self.manager_v2_ledger_spend(session_id)?;
        let legacy = self.manager_v2_legacy_spend(&session)?;
        let checkpoint = SpendCheckpoint {
            known_floor_usd: ledger
                .max(legacy.unwrap_or(0.0))
                .max(prior_value.as_ref().map_or(0.0, |p| p.known_floor_usd)),
            zero_origin: prior_value
                .as_ref()
                .map_or(count == 0 && legacy == Some(0.0), |p| {
                    // A denied attempt has not established ledger coverage.
                    // Recheck history until the first actual admission exists.
                    p.zero_origin && (count > 0 || legacy == Some(0.0))
                }),
        };
        let payload = serde_json::to_value(checkpoint)?;
        if prior.as_ref().is_none_or(|r| r.payload != payload) {
            self.manager_v2_put_record(
                &config,
                SPEND_KIND,
                &session_id.to_string(),
                prior.as_ref().and_then(|r| r.epic_id).or(epic),
                prior.as_ref().map_or(0, |r| r.row_version),
                &payload,
            )?;
        }
        Ok(())
    }

    /// A pending UUID remains part of a selected Epic across scope revisions
    /// or manager replacement. These are accounting records, not live grants.
    fn manager_v2_selected_launch_records(
        &self,
        config: &HarnessManagerConfigV1,
    ) -> Result<Vec<super::harness_manager_v2::ManagerRecordV2>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.manager_session_id,r.scope_version,r.record_key
             FROM harness_manager_v2_records r LEFT JOIN sessions s ON s.id=r.record_key
             WHERE r.project_id=?1 AND r.kind='resource_launch_origin' AND
               ((r.manager_session_id=?2 AND r.scope_version=?3)
                 OR r.epic_id IN (SELECT value FROM json_each(?4)))
               AND (s.id IS NULL
                 OR s.status NOT IN ('Completed','Failed','Interrupted','Archived','Deleted')
                 OR EXISTS(SELECT 1 FROM model_invocations i WHERE i.session_id=r.record_key
                    AND i.admission_status='admitted'
                    AND i.status IN ('running','cancellation_requested')))
             ORDER BY r.record_key LIMIT 1025",
        )?;
        let rows = stmt
            .query_map(
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    serde_json::to_string(&config.epic_ids)?
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if rows.len() > COHORT_BUDGET {
            return Err(refused("manager_v2_cohort_subdivision_required"));
        }
        rows.into_iter()
            .map(|(manager, scope, key)| {
                let mut historical = config.clone();
                historical.manager_session_id = Uuid::parse_str(&manager)
                    .map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
                historical.row_version = scope;
                self.manager_v2_record(&historical, ORIGIN_KIND, &key)?
                    .ok_or_else(|| refused("manager_v2_launch_origin_missing"))
            })
            .collect()
    }

    fn manager_v2_spend_checkpoint(
        &self,
        config: &HarnessManagerConfigV1,
        id: Uuid,
    ) -> Result<Option<SpendCheckpoint>> {
        if let Some(record) = self.manager_v2_record(config, SPEND_KIND, &id.to_string())? {
            return Ok(Some(serde_json::from_value(record.payload)?));
        }
        // Only an exact daemon-authored fresh origin can carry its zero-history
        // proof into a later selected scope. Bare telemetry cannot do this.
        let json: Option<String> = self.conn.query_row(
            "SELECT s.payload_json FROM harness_manager_v2_records s
             JOIN harness_manager_v2_records o ON o.project_id=s.project_id AND o.manager_session_id=s.manager_session_id
               AND o.scope_version=s.scope_version AND o.record_key=s.record_key AND o.kind='resource_launch_origin'
             WHERE s.project_id=?1 AND s.kind='resource_spend' AND s.record_key=?2
               AND o.epic_id IN (SELECT value FROM json_each(?3))
             ORDER BY s.scope_version DESC LIMIT 1",
            params![config.project_id.to_string(),id.to_string(),serde_json::to_string(&config.epic_ids)?], |row| row.get(0),
        ).optional()?;
        json.map(|json| serde_json::from_str(&json).map_err(Into::into))
            .transpose()
    }

    /// Rotation admission precedes the successor row and committed custody edge.
    /// The durable invocation parent attributes that reservation, including a
    /// failed establishment, without granting the candidate any live authority.
    fn manager_v2_resource_reservations(
        &self,
        config: &HarnessManagerConfigV1,
        cohort: &[ManagerCohortMemberV2],
    ) -> Result<BTreeMap<Uuid, (SessionProvider, Option<Uuid>)>> {
        let ids = serde_json::to_string(&cohort.iter().map(|m| m.session.id).collect::<Vec<_>>())?;
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT child.session_id,child.provider,parent.session_id FROM model_invocations child
             JOIN model_invocations parent ON parent.id=child.parent_invocation_id
             WHERE child.admission_status='admitted' AND child.purpose='session.rotate.child'
               AND child.project_id=?1 AND parent.project_id=?1
               AND child.status IN ('running','cancellation_requested')
               AND (parent.session_id IN (SELECT value FROM json_each(?2))
                 OR parent.session_id IN (SELECT session_id FROM harness_manager_v2_entities
                    WHERE project_id=?1 AND manager_session_id=?3 AND scope_version=?4
                      AND kind NOT IN ('Epic','Group')))
               AND child.session_id NOT IN (SELECT value FROM json_each(?2))
             ORDER BY child.session_id LIMIT 1025",
        )?;
        let rows = stmt
            .query_map(
                params![
                    config.project_id.to_string(),
                    ids,
                    config.manager_session_id.to_string(),
                    config.row_version
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if rows.len() + cohort.len() > COHORT_BUDGET {
            return Err(refused("manager_v2_cohort_subdivision_required"));
        }
        let mut owners = BTreeMap::new();
        for (id, provider, parent) in rows {
            let id =
                Uuid::parse_str(&id).map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
            let provider = super::row_mappers::str_to_session_provider(&provider)?;
            let parent = Uuid::parse_str(&parent)
                .map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
            let epic = cohort
                .iter()
                .find(|m| m.session.id == parent)
                .and_then(|m| m.epic_id);
            if owners
                .insert(id, (provider, epic))
                .is_some_and(|previous| previous != (provider, epic))
            {
                return Err(refused("manager_v2_rotation_attribution_changed"));
            }
        }
        for record in self.manager_v2_selected_launch_records(config)? {
            let id = Uuid::parse_str(&record.key)
                .map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
            if cohort.iter().any(|member| member.session.id == id) {
                continue;
            }
            let provider: SessionProvider =
                serde_json::from_value(record.payload["provider"].clone())?;
            owners.entry(id).or_insert((provider, record.epic_id));
            if owners.len() + cohort.len() > COHORT_BUDGET {
                return Err(refused("manager_v2_cohort_subdivision_required"));
            }
        }
        for origin in self.manager_v2_root_origins(config.project_id)?.values() {
            if !cohort.iter().any(|m| m.session.id == origin.session_id) {
                owners
                    .entry(origin.session_id)
                    .or_insert((origin.provider, None));
            }
        }
        if owners.len() + cohort.len() > COHORT_BUDGET {
            return Err(refused("manager_v2_cohort_subdivision_required"));
        }
        Ok(owners)
    }

    /// This internal request is authored by the lifecycle kernel, never an
    /// agent DTO. Resolve its parent from the ledger rather than trusting a
    /// candidate's supplied continued_from or widening live manager authority.
    pub(crate) fn manager_v2_resource_admission(
        &self,
        request: &crate::model_control::ModelAdmissionRequest,
        provider: SessionProvider,
    ) -> Result<()> {
        use rsi_common::model_control::ModelInvocationPurpose;
        let Some(session) = request.owner.session_id else {
            return Ok(());
        };
        if request.purpose != ModelInvocationPurpose::SessionRotateChild {
            self.manager_v2_capture_resource_spend(session)?;
            return self.manager_v2_resource_gate_for_session(session, provider);
        }
        let Some(project) = request.owner.project_id else {
            return Ok(());
        };
        let Some(config) = self.get_harness_manager(project)? else {
            return Ok(());
        };
        if self
            .get_harness_manager_policy(project)?
            .is_none_or(|g| g.revoked)
        {
            return Ok(());
        }
        let parent: Option<String> = self.conn.query_row(
            "SELECT session_id FROM model_invocations WHERE id=?1 AND project_id=?2 AND admission_status='admitted'",
            params![request.parent_invocation_id.map(|id| id.to_string()), project.to_string()], |r| r.get(0)).optional()?.flatten();
        let parent = parent
            .and_then(|id| Uuid::parse_str(&id).ok())
            .ok_or_else(|| refused("manager_v2_rotation_origin_unknown"))?;
        let cohort = self.manager_v2_cohort(&config)?;
        if let Some(member) = cohort.iter().find(|m| m.session.id == parent) {
            self.manager_v2_capture_resource_spend(parent)?;
            self.manager_v2_resource_gate(&config, member.epic_id, provider, Some(parent))?;
        } else if let Some(session) = self.get_session(parent)? {
            if let Some(parent_id) = session.parent_id
                && let Some((_, epic)) = self.manager_v2_launch_parent_scope(
                    parent_id,
                    session.project_id,
                    session.session_kind,
                )?
            {
                self.manager_v2_resource_gate(&config, epic, provider, Some(parent))?;
            }
            let attributed: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_entities
                 WHERE session_id=?1 AND project_id=?2 AND manager_session_id=?3
                   AND scope_version=?4 AND kind NOT IN ('Epic','Group'))",
                params![
                    parent.to_string(),
                    project.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version
                ],
                |row| row.get(0),
            )?;
            if attributed {
                self.manager_v2_resource_gate(&config, None, provider, Some(parent))?;
            }
        }
        Ok(())
    }

    pub(crate) fn manager_v2_resource_snapshot(
        &self,
        config: &HarnessManagerConfigV1,
    ) -> Result<Value> {
        let cohort = self.manager_v2_cohort(config)?;
        let leaves: Vec<_> = cohort
            .iter()
            .filter(|m| rsi_common::is_leaf_kind(m.session.session_kind))
            .collect();
        let rotation = self.manager_v2_resource_reservations(config, &cohort)?;
        let ids = serde_json::to_string(
            &leaves
                .iter()
                .map(|m| m.session.id)
                .chain(rotation.keys().copied())
                .collect::<Vec<_>>(),
        )?;
        let mut active = HashSet::new();
        let mut by_provider = BTreeMap::<String, usize>::new();
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT session_id FROM model_invocations WHERE admission_status='admitted'
             AND status IN ('running','cancellation_requested') AND session_id IN (SELECT value FROM json_each(?1))",
        )?;
        for id in stmt.query_map([&ids], |r| r.get::<_, String>(0))? {
            active.insert(id?);
        }
        for member in &leaves {
            if matches!(
                member.session.status,
                SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval
            ) {
                active.insert(member.session.id.to_string());
            }
        }
        let (known, unknown) = self.manager_v2_historical_spend(config)?;
        for member in &leaves {
            let session = &member.session;
            if active.contains(&session.id.to_string()) {
                *by_provider
                    .entry(super::row_mappers::session_provider_to_str(session.provider).into())
                    .or_default() += 1;
            }
        }
        for (id, (provider, _)) in rotation {
            if active.contains(&id.to_string()) {
                *by_provider
                    .entry(super::row_mappers::session_provider_to_str(provider).into())
                    .or_default() += 1;
            }
        }
        let pending: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM harness_manager_v2_operations WHERE project_id=?1
             AND manager_session_id=?2 AND scope_version=?3 AND state IN ('queued','running')",
            params![
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version
            ],
            |r| r.get(0),
        )?;
        let policy = self.get_harness_manager_policy(config.project_id)?;
        // #674 K15b: the lifetime creation count the admission gate charges
        // against `max_created_sessions` (reviewer launches and uncreated
        // refusals excluded by K15a).
        let used = self.manager_v2_created_usage(config, false)?;
        let limit = policy
            .as_ref()
            .map(|p| i64::from(p.policy.max_created_sessions));
        Ok(
            json!({"type":"resources", "key":"cohort", "active_sessions":active.len(),
            "active_by_provider":by_provider,"pending_operations":pending,
            "created_sessions":{"used":used,"limit":limit,
                "remaining":limit.map(|limit| (limit - used).max(0))},
            "known_spend_usd":valid_cost(Some(known)), "unknown_spend_observations":unknown,
            "spend_basis":"scoped_invocations_with_preserved_historical_floor",
            "provider_windows":self.load_provider_rate_limit_snapshots()?,
            "policy":policy,"cohort_complete":true}),
        )
    }

    pub(crate) fn manager_v2_resource_gate(
        &self,
        config: &HarnessManagerConfigV1,
        epic_id: Option<Uuid>,
        provider: SessionProvider,
        existing_session: Option<Uuid>,
    ) -> Result<()> {
        let Some(grant) = self.get_harness_manager_policy(config.project_id)? else {
            return Ok(());
        };
        if grant.revoked {
            return Ok(());
        }
        if grant.policy.paused || epic_id.is_some_and(|e| grant.policy.paused_epic_ids.contains(&e))
        {
            return Err(refused("manager_v2_policy_paused"));
        }
        let snapshot = self.manager_v2_resource_snapshot(config)?;
        let mut active = snapshot["active_sessions"].as_u64().unwrap_or(u64::MAX);
        let mut provider_active = snapshot["active_by_provider"]
            [super::row_mappers::session_provider_to_str(provider)]
        .as_u64()
        .unwrap_or(0);
        // A continuation replaces one live slot rather than consuming two.
        // Under the spawn guard this is only an admission estimate, never a
        // license to bypass the ordinary interruption/custody checks.
        let existing_provider = if let Some(id) = existing_session {
            let cohort = self.manager_v2_cohort(config)?;
            if let Some(member) = cohort.iter().find(|row| row.session.id == id) {
                Some(member.session.provider)
            } else {
                self.manager_v2_resource_reservations(config, &cohort)?
                    .get(&id)
                    .map(|(provider, _)| *provider)
            }
        } else {
            None
        };
        if let Some(id) = existing_session.filter(|_| existing_provider.is_some()) {
            let counted: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1 AND status IN ('Starting','Running','WaitingApproval'))
                    OR EXISTS(SELECT 1 FROM model_invocations WHERE session_id=?1 AND admission_status='admitted' AND status IN ('running','cancellation_requested'))",
                [id.to_string()], |r| r.get(0))?;
            if counted {
                active = active.saturating_sub(1);
                if existing_provider == Some(provider) {
                    provider_active = provider_active.saturating_sub(1);
                }
            }
        }
        if active >= u64::from(grant.policy.max_active_sessions) {
            return Err(refused("manager_v2_concurrency_capacity"));
        }
        if grant
            .policy
            .provider_limits
            .iter()
            .any(|p| p.provider == provider && provider_active >= u64::from(p.max_active))
        {
            return Err(refused("manager_v2_provider_capacity"));
        }
        if let Some(cap) = grant.policy.max_spend_usd {
            if snapshot["unknown_spend_observations"].as_u64() != Some(0)
                || snapshot["known_spend_usd"].as_f64().is_none()
            {
                return Err(refused("manager_v2_spend_unknown"));
            }
            if snapshot["known_spend_usd"]
                .as_f64()
                .is_none_or(|spent| spent >= cap)
            {
                return Err(refused("manager_v2_spend_exhausted"));
            }
        }
        let now_epoch = Utc::now().timestamp();
        for snapshot in self.load_provider_rate_limit_snapshots()? {
            if snapshot.provider == provider
                && snapshot.windows.iter().any(|w| {
                    w.utilization >= 1.0 && w.resets_at_epoch.is_none_or(|reset| reset > now_epoch)
                })
            {
                return Err(refused("manager_v2_provider_usage_limit"));
            }
        }
        Ok(())
    }

    pub(crate) fn manager_v2_resource_gate_for_session(
        &self,
        session_id: Uuid,
        provider: SessionProvider,
    ) -> Result<()> {
        let Some(session) = self.get_session(session_id)? else {
            return Ok(());
        };
        let Some(project) = session.project_id else {
            return Ok(());
        };
        let Some(config) = self.get_harness_manager(project)? else {
            return Ok(());
        };
        if self
            .get_harness_manager_policy(project)?
            .is_none_or(|g| g.revoked)
        {
            return Ok(());
        }
        let cohort = self.manager_v2_cohort(&config)?;
        if let Some(member) = cohort.iter().find(|m| m.session.id == session_id) {
            self.manager_v2_resource_gate(&config, member.epic_id, provider, Some(session_id))?;
        } else if let Some((_, epic)) = self
            .manager_v2_resource_reservations(&config, &cohort)?
            .get(&session_id)
        {
            self.manager_v2_resource_gate(&config, *epic, provider, Some(session_id))?;
        } else if let Some(parent) = session.parent_id
            && let Some((_, epic)) = self.manager_v2_launch_parent_scope(
                parent,
                session.project_id,
                session.session_kind,
            )?
        {
            self.manager_v2_resource_gate(&config, epic, provider, Some(session_id))?;
        }
        Ok(())
    }

    /// New ordinary child sessions do not have a persisted row at admission.
    /// Their validated parent gives the cohort scope under the launch guard.
    pub(crate) fn manager_v2_resource_gate_for_parent(
        &self,
        parent: Uuid,
        provider: SessionProvider,
    ) -> Result<()> {
        let Some(mut current) = self.get_session(parent)? else {
            return Ok(());
        };
        let Some(project) = current.project_id else {
            return Ok(());
        };
        let Some(config) = self.get_harness_manager(project)? else {
            return Ok(());
        };
        let mut seen = HashSet::new();
        for _ in 0..64 {
            if !seen.insert(current.id) {
                return Err(refused("manager_v2_cohort_cycle"));
            }
            if current.session_kind == SessionKind::Epic && config.epic_ids.contains(&current.id) {
                return self.manager_v2_resource_gate(&config, Some(current.id), provider, None);
            }
            let Some(parent) = current.parent_id else {
                return Ok(());
            };
            current = self
                .get_session(parent)?
                .ok_or_else(|| refused("manager_v2_illegal_parent"))?;
            if current.project_id != Some(project) {
                return Err(refused("manager_v2_cohort_project_mismatch"));
            }
        }
        Err(refused("manager_v2_cohort_subdivision_required"))
    }
}

#[cfg(test)]
pub(crate) mod tests;
