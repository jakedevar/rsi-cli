//! #2502: the operator's Budget panel read. For each project a portfolio
//! node covers, every node along that project's chain (each covering
//! portfolio node, top first, then the project manager) with each cap as
//! used and limit, counted exactly as admission counts it:
//!
//! - created sessions and containers: a portfolio node's charged creations
//!   since its grant started ([`super::manager_actions::head_created_count`],
//!   the same count `manager_ancestor_creation_allowance` refuses on); the
//!   project manager's own scope count (`manager_v2_created_usage`);
//! - active sessions and spend: the charged resource snapshot the resource
//!   gate reads (`manager_v2_resource_snapshot`).

use super::Store;
use super::harness_manager_v2::{PortfolioHead, refused};
use super::manager_actions::{head_created_count, head_created_times, window_usage};
use super::manager_resources::portfolio_coverage_config;
use crate::error::Result;
use rsi_common::harness_manager::HarnessManagerConfigV1;
use rsi_common::harness_manager_v2::ManagerPolicyV2;
use rsi_common::manager_budget::{
    BUDGET_RESETS_TEXT, BudgetCapFieldV1, BudgetCapV1, BudgetNodeKindV1, BudgetNodeV1,
    BudgetProjectV1, GlobalManagerBudgetV1, mark_binding,
};
use serde_json::Value;
use uuid::Uuid;

/// Live usage from a resource snapshot: active sessions, known spend, and
/// whether the spend is incomplete (an unknown observation, or no known
/// total), which makes admission refuse `manager_v2_spend_unknown` under a
/// finite cap whatever the headroom (#2519). Read exactly as
/// `manager_v2_resource_gate` reads it.
#[derive(Debug, Clone, Copy)]
struct LiveUsage {
    active: f64,
    spend: f64,
    spend_unknown: bool,
}

fn live_usage(snapshot: &Value) -> LiveUsage {
    let known = snapshot["known_spend_usd"].as_f64();
    LiveUsage {
        active: snapshot["active_sessions"].as_f64().unwrap_or(0.0),
        spend: known.unwrap_or(0.0),
        spend_unknown: known.is_none()
            || snapshot["unknown_spend_observations"]
                .as_u64()
                .unwrap_or(u64::MAX)
                != 0,
    }
}

fn caps(
    policy: &ManagerPolicyV2,
    created: i64,
    containers: i64,
    live: LiveUsage,
) -> Vec<BudgetCapV1> {
    #[allow(clippy::cast_precision_loss)] // Counts are far below 2^52.
    let used = |field| match field {
        BudgetCapFieldV1::CreatedSessions => created as f64,
        BudgetCapFieldV1::CreatedContainers => containers as f64,
        BudgetCapFieldV1::ActiveSessions => live.active,
        BudgetCapFieldV1::SpendUsd => live.spend,
    };
    BudgetCapFieldV1::ALL
        .into_iter()
        .map(|field| BudgetCapV1 {
            field,
            used: used(field),
            limit: field.limit_of(policy),
            binding: false,
            spend_unknown: field == BudgetCapFieldV1::SpendUsd && live.spend_unknown,
        })
        .collect()
}

impl Store {
    /// The Budget panel of portfolio node `node` (the global node when
    /// `None`). Refuses `global_manager_not_configured` when there is none.
    pub fn global_manager_budget(&self, node: Option<Uuid>) -> Result<GlobalManagerBudgetV1> {
        let node = match node {
            Some(node) => node,
            None => self.global_shim_node()?.ok_or_else(|| {
                refused(rsi_common::global_manager::GLOBAL_MANAGER_NOT_CONFIGURED)
            })?,
        };
        let portfolio = self
            .get_portfolio_node(node)?
            .ok_or_else(|| refused(rsi_common::portfolio_nodes::MANAGER_NODE_STALE))?;
        let mut projects = Vec::new();
        for project in &portfolio.grant.project_ids {
            let project_name = self
                .get_project(*project)?
                .map_or_else(|| project.to_string()[..8].to_string(), |p| p.name);
            let chain = self.budget_chain(*project, &project_name)?;
            projects.push(BudgetProjectV1 {
                project_id: *project,
                project_name,
                chain,
            });
        }
        Ok(GlobalManagerBudgetV1 {
            node_id: node,
            node_label: portfolio.tier_label,
            projects,
            resets: BUDGET_RESETS_TEXT.to_string(),
        })
    }

    /// One project's chain, top first, with the binding cap marked.
    fn budget_chain(&self, project: Uuid, name: &str) -> Result<Vec<BudgetNodeV1>> {
        let heads = self.portfolio_chain_heads(project)?;
        let mut chain = Vec::new();
        if let Some(since) = heads.iter().map(|head| head.started.as_str()).min() {
            let since = since.to_string();
            let now = chrono::Utc::now();
            let sessions = self.portfolio_creation_charges(project, false, &since, now)?;
            let containers = self.portfolio_creation_charges(project, true, &since, now)?;
            let epics = self.global_project_epics(project)?;
            for head in &heads {
                chain.push(
                    self.budget_portfolio_node(
                        project,
                        head,
                        &epics,
                        // #3416: sessions count over the head's rolling window.
                        window_usage(
                            &head_created_times(&sessions, head),
                            head.record
                                .grant
                                .project_policy
                                .created_sessions_window_hours,
                            i64::from(head.record.grant.project_policy.max_created_sessions),
                            now,
                        )
                        .used,
                        head_created_count(&containers, head),
                    )?,
                );
            }
        }
        if let Some(pm) = self.budget_project_manager(project, name)? {
            chain.push(pm);
        }
        mark_binding(&mut chain);
        Ok(chain)
    }

    fn budget_portfolio_node(
        &self,
        project: Uuid,
        head: &PortfolioHead,
        epics: &[Uuid],
        created: i64,
        containers: i64,
    ) -> Result<BudgetNodeV1> {
        let config = portfolio_coverage_config(project, head, epics);
        let live = live_usage(&self.manager_v2_resource_snapshot(&config)?);
        let label = self
            .get_portfolio_node(head.node_id)?
            .map_or_else(|| "portfolio".to_string(), |node| node.tier_label);
        Ok(BudgetNodeV1 {
            kind: BudgetNodeKindV1::Portfolio,
            node_id: head.node_id,
            label,
            version: head.record.grant.grant_version,
            epoch: head.epoch,
            counted_since: Some(head.started.clone()),
            caps: caps(&head.record.grant.project_policy, created, containers, live),
        })
    }

    /// The project manager's own policy row, when one is live.
    fn budget_project_manager(&self, project: Uuid, name: &str) -> Result<Option<BudgetNodeV1>> {
        let Some(config) = self.get_harness_manager(project)? else {
            return Ok(None);
        };
        let Some(grant) = self
            .get_harness_manager_policy(project)?
            .filter(|grant| !grant.revoked)
        else {
            return Ok(None);
        };
        Ok(Some(self.budget_manager_node(
            &config,
            &grant.policy,
            grant.row_version,
            name,
        )?))
    }

    fn budget_manager_node(
        &self,
        config: &HarnessManagerConfigV1,
        policy: &ManagerPolicyV2,
        policy_version: i64,
        name: &str,
    ) -> Result<BudgetNodeV1> {
        let created = self.manager_v2_created_usage(config, false)?;
        let containers = self.manager_v2_created_usage(config, true)?;
        let live = live_usage(&self.manager_v2_resource_snapshot(config)?);
        Ok(BudgetNodeV1 {
            kind: BudgetNodeKindV1::Project,
            node_id: config.project_id,
            label: name.to_string(),
            version: policy_version,
            epoch: config.row_version,
            counted_since: None,
            caps: caps(policy, created, containers, live),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spend_cap(snapshot: &Value, limit: Option<f64>) -> BudgetCapV1 {
        let policy = ManagerPolicyV2 {
            max_spend_usd: limit,
            ..ManagerPolicyV2::default()
        };
        caps(&policy, 0, 0, live_usage(snapshot))
            .into_iter()
            .find(|cap| cap.field == BudgetCapFieldV1::SpendUsd)
            .unwrap()
    }

    /// #2519: the panel reads spend completeness exactly as the resource
    /// gate does (`manager_v2_check_usage`): an unknown observation, or no
    /// known total, under a finite cap refuses `manager_v2_spend_unknown`,
    /// so the cap has no headroom whatever its limit.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn a_finite_spend_cap_with_unknown_observations_is_blocked_not_headroom() {
        let unknown =
            json!({"active_sessions":0,"known_spend_usd":0.0,"unknown_spend_observations":1});
        let blocked = spend_cap(&unknown, Some(100.0));
        assert!(blocked.spend_unknown);
        assert!(blocked.blocked_unknown_spend());
        assert_eq!((blocked.used, blocked.headroom()), (0.0, Some(0.0)));

        let missing_total = json!({"active_sessions":0,"unknown_spend_observations":0});
        assert!(spend_cap(&missing_total, Some(100.0)).blocked_unknown_spend());

        let complete =
            json!({"active_sessions":0,"known_spend_usd":0.0,"unknown_spend_observations":0});
        let open = spend_cap(&complete, Some(100.0));
        assert!(!open.spend_unknown);
        assert_eq!(open.headroom(), Some(100.0));

        // No cap: unknown spend refuses nothing.
        let uncapped = spend_cap(&unknown, None);
        assert!(!uncapped.blocked_unknown_spend());
        assert_eq!(uncapped.headroom(), None);
    }
}
