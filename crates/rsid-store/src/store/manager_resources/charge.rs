//! #1309, plan §2.3(d): live caps are charged by origin. A live session and
//! its spend count only against the node that originated it and that node's
//! ancestors, never against a descendant, as #1301 charges creation budgets.
//!
//! A session's origin is the ledger that created it: a lifecycle creation
//! (`create_session`, `replace_lead`, `retry_lead`, `succeed_manager`) or a
//! launch-origin reservation by a portfolio principal, a manager-requested
//! topology attempt, or a delegated appointment's grantor (#1314). Rotation
//! successors and the children a leaf spawns inherit it. Work no portfolio
//! node originated (a project manager's, an operator's) is charged to every
//! node covering the project, as before.

use super::*;
use crate::store::harness_manager_v2::PortfolioHead;

/// Whose caps one policy check charges.
pub(super) struct ChargeScope {
    /// Portfolio nodes whose origin is not charged here: every node whose
    /// lineage excludes this principal. Empty when no node exists, which
    /// keeps the unattributed accounting byte for byte.
    foreign: Vec<Uuid>,
    /// The ledgers of the chain nodes below this node in the project: what
    /// they originate (an appointed seat, #1314) counts here too.
    below: Vec<HarnessManagerConfigV1>,
}

/// One policy's usage under a [`ChargeScope`].
pub(super) struct ChargedUsage {
    pub active: u64,
    pub provider_active: u64,
    pub known_spend_usd: Option<f64>,
    pub unknown_spend_observations: u64,
}

/// The live members and scoped spend one [`ChargeScope`] pays for.
pub(super) struct ChargedMembers {
    pub active: BTreeMap<Uuid, SessionProvider>,
    /// How many ledgers were counted: `config`'s own plus those below it.
    pub ledgers: usize,
    pub known_spend: f64,
    pub unknown_spend: i64,
}

/// SQL: whether the V2 ledger principal (`manager`, `version`) acts for a
/// node of the JSON array `nodes` ([`Store::portfolio_origin_node`]).
fn foreign_principal(manager: &str, version: &str, nodes: &str) -> String {
    format!(
        // sql-dynamic-ok: column names and placeholders from this module
        "EXISTS(SELECT 1 FROM global_manager_grants g0 WHERE g0.grant_version={version}
           AND g0.node_id IN (SELECT value FROM json_each({nodes}))
           AND EXISTS(SELECT 1 FROM global_manager_grants g WHERE g.seat_session_id={manager}
             AND g.grant_version>=g0.grant_version AND g.node_id IS g0.node_id))"
    )
}

/// SQL CTEs `foreign_origin(id)` and `foreign_ids(id)`: the sessions of
/// `project` a node of `nodes` originated, and everything that inherits that
/// origin. Constant-false when `nodes` is empty.
pub(super) fn foreign_ctes(project: &str, nodes: &str) -> String {
    let op = foreign_principal("o.manager_session_id", "o.scope_version", nodes);
    let record = foreign_principal("r.manager_session_id", "r.scope_version", nodes);
    let topology = foreign_principal("e.requested_by_session_id", "e.scope_version", nodes);
    format!(
        // sql-dynamic-ok: placeholders and predicates from this module
        "foreign_origin(id) AS (
            SELECT o.target_session_id FROM harness_manager_v2_operations o
             WHERE json_array_length({nodes})>0 AND o.project_id={project}
               AND o.kind='lifecycle_action' AND o.target_session_id IS NOT NULL
               AND json_extract(o.payload_json,'$.request.operation.action')
                 IN ('create_session','replace_lead','retry_lead','succeed_manager')
               AND {op}
            UNION SELECT r.record_key FROM harness_manager_v2_records r
             WHERE json_array_length({nodes})>0 AND r.project_id={project}
               AND r.kind='resource_launch_origin' AND {record}
            UNION SELECT a.session_id FROM manager_portfolio_appointments a
             WHERE json_array_length({nodes})>0 AND a.launch_project_id={project}
               AND a.grantor_node_id IN (SELECT value FROM json_each({nodes}))
            UNION SELECT t.session_id FROM topology_node_attempts t
               JOIN topology_executions e ON e.id=t.execution_id
             WHERE json_array_length({nodes})>0 AND e.project_id={project}
               AND e.requested_by_kind='manager' AND t.session_id IS NOT NULL AND {topology}
         ), foreign_ids(id) AS (
            SELECT id FROM foreign_origin
            UNION SELECT s.id FROM foreign_ids f JOIN sessions s ON s.continued_from=f.id
            UNION SELECT e.successor_session_id FROM foreign_ids f
              JOIN harness_manager_rotation_edges e ON e.predecessor_session_id=f.id
            UNION SELECT s.id FROM foreign_ids f JOIN sessions p ON p.id=f.id
              JOIN sessions s ON s.parent_id=p.id WHERE p.session_kind NOT IN ('Group','Epic')
         )"
    )
}

impl ChargeScope {
    /// No portfolio node covers the project: nothing is attributed.
    pub(super) const fn plain() -> Self {
        Self {
            foreign: Vec::new(),
            below: Vec::new(),
        }
    }

    /// Nothing to attribute: the unattributed accounting applies unchanged.
    pub(super) const fn is_plain(&self) -> bool {
        self.foreign.is_empty() && self.below.is_empty()
    }
}

impl Store {
    /// The charge of a project-level principal (a project manager, an area
    /// node): it sits below every portfolio node, so no node's origin is
    /// charged to it.
    pub(super) fn manager_v2_manager_charge(&self) -> Result<ChargeScope> {
        Ok(ChargeScope {
            foreign: self.portfolio_node_ids()?,
            below: Vec::new(),
        })
    }

    /// The charge of `chain[index]`, a node covering `project`: the origins
    /// of nodes whose lineage excludes it are foreign; the ledgers of the
    /// chain nodes below it are its own.
    pub(super) fn manager_v2_node_charge(
        &self,
        project: Uuid,
        chain: &[PortfolioHead],
        index: usize,
        epics: &[Uuid],
    ) -> Result<ChargeScope> {
        let node = chain[index].node_id;
        let mut foreign = Vec::new();
        for other in self.portfolio_node_ids()? {
            if !self.portfolio_lineage(other)?.contains(&node) {
                foreign.push(other);
            }
        }
        Ok(ChargeScope {
            foreign,
            below: chain[index + 1..]
                .iter()
                .map(|head| portfolio_coverage_config(project, head, epics))
                .collect(),
        })
    }

    /// Every portfolio node with a grant, active or not.
    fn portfolio_node_ids(&self) -> Result<Vec<Uuid>> {
        let mut statement = self.conn.prepare(
            "SELECT DISTINCT node_id FROM global_manager_grants WHERE node_id IS NOT NULL
             ORDER BY node_id",
        )?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.iter()
            .map(|id| {
                Uuid::parse_str(id).map_err(|_| refused("manager_v2_invalid_stored_identity"))
            })
            .collect()
    }

    /// Whether any of `ids` (a launch's session, its parent) carries an
    /// origin `charge` does not pay for.
    pub(super) fn manager_v2_charge_excludes(
        &self,
        project: Uuid,
        charge: &ChargeScope,
        ids: &[Uuid],
    ) -> Result<bool> {
        if charge.foreign.is_empty() || ids.is_empty() {
            return Ok(false);
        }
        Ok(!self
            .manager_v2_foreign_among(project, &charge.foreign, ids)?
            .is_empty())
    }

    fn manager_v2_foreign_among(
        &self,
        project: Uuid,
        foreign: &[Uuid],
        ids: &[Uuid],
    ) -> Result<HashSet<Uuid>> {
        let ctes = foreign_ctes("?1", "?2");
        let mut statement = self.conn.prepare(&format!(
            "WITH RECURSIVE {ctes} SELECT value FROM json_each(?3) WHERE value IN (SELECT id FROM foreign_ids)" // sql-dynamic-ok: static CTE text
        ))?;
        let rows = statement
            .query_map(
                params![
                    project.to_string(),
                    serde_json::to_string(foreign)?,
                    serde_json::to_string(ids)?
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.iter()
            .map(|id| {
                Uuid::parse_str(id).map_err(|_| refused("manager_v2_invalid_stored_identity"))
            })
            .collect()
    }

    /// The usage `charge` pays for: the live members and spend of `config`'s
    /// cohort and of the ledgers below it, minus every foreign origin.
    /// `existing_session` replaces its own slot, as in the plain snapshot.
    pub(super) fn manager_v2_charged_usage(
        &self,
        config: &HarnessManagerConfigV1,
        charge: &ChargeScope,
        provider: SessionProvider,
        existing_session: Option<Uuid>,
    ) -> Result<ChargedUsage> {
        let members = self.manager_v2_charged_members(config, charge, true)?;
        let active = members.active;
        let mut count = active.len() as u64;
        let mut provider_active = active.values().filter(|p| **p == provider).count() as u64;
        // A continuation replaces one live slot rather than consuming two.
        if let Some(existing) = existing_session.and_then(|id| active.get(&id)) {
            count = count.saturating_sub(1);
            if *existing == provider {
                provider_active = provider_active.saturating_sub(1);
            }
        }
        Ok(ChargedUsage {
            active: count,
            provider_active,
            known_spend_usd: valid_cost(Some(members.known_spend)),
            unknown_spend_observations: u64::try_from(members.unknown_spend).unwrap_or(u64::MAX),
        })
    }

    /// The live members and spend `charge` pays for in `config`'s cohort and
    /// the ledgers below it, minus every foreign origin: the one computation
    /// behind both the gate ([`Self::manager_v2_charged_usage`]) and the
    /// resource snapshot (#1336), so a reader sees what the gate enforces.
    /// `enforce_budget` refuses a cohort over [`COHORT_BUDGET`] (the gate);
    /// the snapshot reports it instead.
    pub(super) fn manager_v2_charged_members(
        &self,
        config: &HarnessManagerConfigV1,
        charge: &ChargeScope,
        enforce_budget: bool,
    ) -> Result<ChargedMembers> {
        let mut active = BTreeMap::new();
        let mut ledgers = Vec::new();
        for scope in std::iter::once(config).chain(charge.below.iter()) {
            let cohort = self.manager_v2_cohort(scope)?;
            let rotation = self.manager_v2_resource_reservations(scope, &cohort)?;
            for (id, member_provider) in self.manager_v2_active_members(&cohort, &rotation)? {
                active.entry(id).or_insert(member_provider);
            }
            ledgers.push((scope.manager_session_id, scope.row_version));
        }
        if enforce_budget && active.len() > COHORT_BUDGET {
            return Err(refused("manager_v2_cohort_subdivision_required"));
        }
        if !charge.foreign.is_empty() && !active.is_empty() {
            let ids: Vec<Uuid> = active.keys().copied().collect();
            for id in self.manager_v2_foreign_among(config.project_id, &charge.foreign, &ids)? {
                active.remove(&id);
            }
        }
        let (known_spend, unknown_spend) =
            self.manager_v2_scoped_spend(config, &ledgers, &charge.foreign)?;
        Ok(ChargedMembers {
            active,
            ledgers: ledgers.len(),
            known_spend,
            unknown_spend,
        })
    }

    /// The usage the gate charges `config`'s own policy for a new `provider`
    /// launch: `(active, provider_active, known_spend_usd,
    /// unknown_spend_observations)`. Lets a test hold the resource snapshot
    /// to the gate's numbers (#1336).
    #[cfg(test)]
    pub(crate) fn manager_v2_own_charged_usage_for_test(
        &self,
        config: &HarnessManagerConfigV1,
        provider: SessionProvider,
    ) -> Result<(u64, u64, Option<f64>, u64)> {
        let charge = self.manager_v2_own_charge(config)?;
        let usage = self.manager_v2_charged_usage(config, &charge, provider, None)?;
        Ok((
            usage.active,
            usage.provider_active,
            usage.known_spend_usd,
            usage.unknown_spend_observations,
        ))
    }

    /// The charge the resource gate applies to `config`'s own policy
    /// ([`Self::manager_v2_resource_gate_with`]): its node's charge when it
    /// heads a portfolio chain node, otherwise the project-level charge, and
    /// the plain (unattributed) scope when no portfolio node covers the
    /// project.
    pub(super) fn manager_v2_own_charge(
        &self,
        config: &HarnessManagerConfigV1,
    ) -> Result<ChargeScope> {
        let chain = self.portfolio_chain_heads(config.project_id)?;
        if chain.is_empty() {
            return Ok(ChargeScope::plain());
        }
        match chain.iter().position(|head| {
            config.manager_session_id == head.seat_root && config.row_version == head.epoch
        }) {
            Some(index) => {
                let epics = self.global_project_epics(config.project_id)?;
                self.manager_v2_node_charge(config.project_id, &chain, index, &epics)
            }
            None => self.manager_v2_manager_charge(),
        }
    }
}
