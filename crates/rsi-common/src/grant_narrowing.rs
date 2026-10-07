//! Fractal manager hierarchy S3 (#1237, plan §2.2): one narrowing rule for
//! every manager edge (area, project and portfolio).
//!
//! `grant_narrows(child, parent)` replaces both former
//! `strictly_narrower_than` implementations and the portfolio checks. The
//! child must hold:
//! - coverage ⊆ the parent's (`manager_scope_widened`);
//! - capabilities ⊆ the parent's, **equality allowed**, so the PM verb set
//!   survives any depth (`manager_capability_widened`);
//! - launches ⊆ the parent's (`manager_capability_widened`);
//! - each finite allowance strictly lower, so the parent keeps at least one
//!   unit (a zero dimension delegates nothing and stays zero), every provider
//!   ceiling of the parent carved strictly lower, `max_direct_reports` ≤ the
//!   parent's and spend ≤ the parent's; an uncapped parent may have a capped
//!   child, never the reverse (`manager_allowance_exceeded`).

use uuid::Uuid;

use crate::global_manager::GlobalManagerGrantV1;
use crate::harness_manager_v2::{
    ManagerCapabilityV2, ManagerLaunchChoiceV2, ManagerPolicyV2, ManagerProviderLimitV2,
};
use crate::manager_nodes::ManagerNodeGrantV1;

/// The child covers a project (or selector) its parent does not.
pub const MANAGER_SCOPE_WIDENED: &str = "manager_scope_widened";
/// The child holds a capability or launch its parent does not.
pub const MANAGER_CAPABILITY_WIDENED: &str = "manager_capability_widened";
/// A finite allowance, provider ceiling, direct-report limit or spend cap of
/// the child is not within its parent's.
pub const MANAGER_ALLOWANCE_EXCEEDED: &str = "manager_allowance_exceeded";

/// The dimensions one grant is compared on. `coverage: None` means the edge
/// checks coverage elsewhere (an area node's Epic selector).
#[derive(Debug, Clone, PartialEq)]
pub struct GrantBoundsV1 {
    pub coverage: Option<Vec<Uuid>>,
    pub capabilities: Vec<ManagerCapabilityV2>,
    pub launches: Vec<ManagerLaunchChoiceV2>,
    /// Finite allowances, positionally comparable between two grants of the
    /// same kind (an area grant's or a portfolio policy's).
    pub allowances: Vec<u32>,
    pub provider_limits: Vec<ManagerProviderLimitV2>,
    pub max_direct_reports: u16,
    pub max_spend_usd: Option<f64>,
}

/// A finite dimension is carved strictly: a zero parent delegates nothing.
const fn carved(child: u32, parent: u32) -> bool {
    (parent == 0 && child == 0) || child < parent
}

/// See the module docs.
///
/// # Errors
/// `manager_scope_widened`, `manager_capability_widened` or
/// `manager_allowance_exceeded`, checked in that order.
pub fn grant_narrows(child: &GrantBoundsV1, parent: &GrantBoundsV1) -> Result<(), &'static str> {
    if let (Some(child), Some(parent)) = (&child.coverage, &parent.coverage)
        && child.iter().any(|project| !parent.contains(project))
    {
        return Err(MANAGER_SCOPE_WIDENED);
    }
    if child
        .capabilities
        .iter()
        .any(|capability| !parent.capabilities.contains(capability))
        || child
            .launches
            .iter()
            .any(|launch| !parent.launches.contains(launch))
    {
        return Err(MANAGER_CAPABILITY_WIDENED);
    }
    let spend_within = match (child.max_spend_usd, parent.max_spend_usd) {
        (Some(child), Some(parent)) => child <= parent,
        (_, None) => true,
        (None, Some(_)) => false,
    };
    if child.allowances.len() != parent.allowances.len()
        || child
            .allowances
            .iter()
            .zip(&parent.allowances)
            .any(|(child, parent)| !carved(*child, *parent))
        || parent.provider_limits.iter().any(|ceiling| {
            !child.provider_limits.iter().any(|limit| {
                limit.provider == ceiling.provider && limit.max_active < ceiling.max_active
            })
        })
        || child.max_direct_reports > parent.max_direct_reports
        || !spend_within
    {
        return Err(MANAGER_ALLOWANCE_EXCEEDED);
    }
    Ok(())
}

/// By how much the active `children` together overflow each finite
/// dimension of `parent`.
///
/// Plan §2.2's aggregate rule, the portfolio mirror of the area edge's
/// `reserve_child_capacity`. A positive value is a violation. Each allowance and provider ceiling must leave the parent at
/// least one unit (`sum - ceiling + 1`); spend, in micro-dollars, may reach
/// the parent's cap (`sum - ceiling`), as one child's may. A zero or
/// uncapped parent dimension is omitted: [`grant_narrows`] already keeps
/// each child at zero (or allows any cap) there.
#[must_use]
pub fn allowance_overflow(
    children: &[&GrantBoundsV1],
    parent: &GrantBoundsV1,
) -> Vec<(String, i64)> {
    let mut overflow = Vec::new();
    for (index, ceiling) in parent.allowances.iter().enumerate() {
        if *ceiling == 0 {
            continue;
        }
        let sum: i64 = children
            .iter()
            .map(|child| i64::from(child.allowances.get(index).copied().unwrap_or(0)))
            .sum();
        overflow.push((format!("allowance:{index}"), sum - i64::from(*ceiling) + 1));
    }
    for ceiling in &parent.provider_limits {
        if ceiling.max_active == 0 {
            continue;
        }
        let sum: i64 = children
            .iter()
            .flat_map(|child| &child.provider_limits)
            .filter(|limit| limit.provider == ceiling.provider)
            .map(|limit| i64::from(limit.max_active))
            .sum();
        overflow.push((
            format!("provider:{:?}", ceiling.provider),
            sum - i64::from(ceiling.max_active) + 1,
        ));
    }
    if let Some(cap) = parent.max_spend_usd {
        #[allow(clippy::cast_possible_truncation)]
        let micros = |usd: f64| (usd * 1_000_000.0).ceil() as i64;
        let sum: i64 = children
            .iter()
            .filter_map(|child| child.max_spend_usd)
            .map(micros)
            .sum();
        #[allow(clippy::cast_possible_truncation)]
        let ceiling = (cap * 1_000_000.0).floor() as i64;
        overflow.push(("spend_micro_usd".into(), sum - ceiling));
    }
    overflow
}

/// Whether an edge write makes an aggregate worse.
///
/// Some dimension of `after` overflows, and by more than it did `before`
/// (`None`: nothing stood there before, so any overflow counts). A parent already over its allowance (an
/// operator-granted child re-parented by a revoke, #1305) may still be
/// edited and may shrink a child, but grants nothing that adds to the
/// overflow until it is back under.
#[must_use]
pub fn allowance_worsened(before: Option<&[(String, i64)]>, after: &[(String, i64)]) -> bool {
    after.iter().any(|(dimension, overflow)| {
        *overflow > 0
            && before
                .and_then(|before| before.iter().find(|(key, _)| key == dimension))
                .is_none_or(|(_, previous)| overflow > previous)
    })
}

impl ManagerNodeGrantV1 {
    /// An area grant's bounds (its selector is checked by the store).
    #[must_use]
    pub fn bounds(&self) -> GrantBoundsV1 {
        let allowance = &self.allowance;
        GrantBoundsV1 {
            coverage: None,
            capabilities: self.capabilities.clone(),
            launches: self.allowed_launches.clone(),
            allowances: vec![
                u32::from(allowance.max_created_containers),
                u32::from(allowance.max_created_sessions),
                u32::from(allowance.max_active_sessions),
                u32::from(allowance.max_build_slots),
                allowance.max_disk_gib,
            ],
            provider_limits: allowance.provider_limits.clone(),
            max_direct_reports: self.max_direct_reports,
            max_spend_usd: allowance.max_spend_usd,
        }
    }
}

/// The launches a portfolio grant may make in a project: the operator grant's
/// `allowed_launches`, narrowed by the project policy's list when that list
/// is non-empty (an empty V2 list means "any launch"). A disjoint policy list
/// falls back to the grant's list, which is still what the operator granted.
#[must_use]
pub fn portfolio_effective_launches(
    allowed_launches: &[ManagerLaunchChoiceV2],
    policy: &ManagerPolicyV2,
) -> Vec<ManagerLaunchChoiceV2> {
    let narrowed: Vec<_> = allowed_launches
        .iter()
        .filter(|launch| {
            policy.allowed_launches.is_empty() || policy.allowed_launches.contains(launch)
        })
        .cloned()
        .collect();
    if narrowed.is_empty() {
        allowed_launches.to_vec()
    } else {
        narrowed
    }
}

/// A portfolio grant's bounds: its coverage, its in-project policy, its
/// effective launches and its direct-report limit.
#[must_use]
pub fn portfolio_bounds(
    project_ids: &[Uuid],
    allowed_launches: &[ManagerLaunchChoiceV2],
    policy: &ManagerPolicyV2,
    max_direct_reports: u16,
) -> GrantBoundsV1 {
    policy_bounds(
        Some(project_ids.to_vec()),
        policy,
        portfolio_effective_launches(allowed_launches, policy),
        max_direct_reports,
    )
}

/// The bounds of a stored portfolio grant.
#[must_use]
pub fn portfolio_grant_bounds(
    grant: &GlobalManagerGrantV1,
    max_direct_reports: u16,
) -> GrantBoundsV1 {
    portfolio_bounds(
        &grant.project_ids,
        &grant.allowed_launches,
        &grant.project_policy,
        max_direct_reports,
    )
}

/// #1239: the launches a policy handed down by a node may make: its own
/// explicit list, or (an empty V2 list means "any launch") the node's
/// effective launches, so a handed policy never means "any".
#[must_use]
pub fn handed_policy_launches(
    handed: &ManagerPolicyV2,
    node_allowed_launches: &[ManagerLaunchChoiceV2],
    node_policy: &ManagerPolicyV2,
) -> Vec<ManagerLaunchChoiceV2> {
    if handed.allowed_launches.is_empty() {
        portfolio_effective_launches(node_allowed_launches, node_policy)
    } else {
        handed.allowed_launches.clone()
    }
}

/// #1239: a policy a node hands down (its `child_policy`, given to the PMs
/// it appoints and inherited by nothing else) narrows the node's own policy
/// in every dimension: capabilities, launches (within the node's effective
/// launches), each finite allowance strictly lower, provider ceilings and
/// spend.
///
/// # Errors
/// `manager_capability_widened` or `manager_allowance_exceeded`.
pub fn handed_policy_narrows(
    handed: &ManagerPolicyV2,
    node_allowed_launches: &[ManagerLaunchChoiceV2],
    node_policy: &ManagerPolicyV2,
) -> Result<(), &'static str> {
    let node_launches = portfolio_effective_launches(node_allowed_launches, node_policy);
    grant_narrows(
        &policy_bounds(
            None,
            handed,
            handed_policy_launches(handed, node_allowed_launches, node_policy),
            0,
        ),
        &policy_bounds(None, node_policy, node_launches, 0),
    )
}

/// Bounds of one in-project policy.
#[must_use]
pub fn policy_bounds(
    coverage: Option<Vec<Uuid>>,
    policy: &ManagerPolicyV2,
    launches: Vec<ManagerLaunchChoiceV2>,
    max_direct_reports: u16,
) -> GrantBoundsV1 {
    GrantBoundsV1 {
        coverage,
        capabilities: policy.capabilities.clone(),
        launches,
        allowances: vec![
            u32::from(policy.max_created_containers),
            u32::from(policy.max_created_sessions),
            u32::from(policy.max_active_sessions),
        ],
        provider_limits: policy.provider_limits.clone(),
        max_direct_reports,
        max_spend_usd: policy.max_spend_usd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SessionProvider;

    fn launch(model: &str) -> ManagerLaunchChoiceV2 {
        ManagerLaunchChoiceV2 {
            provider: SessionProvider::Claude,
            model: model.into(),
            effort: Some("high".into()),
        }
    }

    fn parent() -> GrantBoundsV1 {
        GrantBoundsV1 {
            coverage: Some(vec![Uuid::from_u128(1), Uuid::from_u128(2)]),
            capabilities: vec![
                ManagerCapabilityV2::IssueCoordinate,
                ManagerCapabilityV2::SessionCreate,
            ],
            launches: vec![launch("a"), launch("b")],
            allowances: vec![4, 8, 0],
            provider_limits: vec![ManagerProviderLimitV2 {
                provider: SessionProvider::Codex,
                max_active: 4,
            }],
            max_direct_reports: 5,
            max_spend_usd: Some(10.0),
        }
    }

    /// The narrowest legal child: equal capabilities, one unit less of each
    /// finite allowance, equal direct reports and spend.
    fn child() -> GrantBoundsV1 {
        let mut child = parent();
        child.coverage = Some(vec![Uuid::from_u128(1)]);
        child.allowances = vec![3, 7, 0];
        child.provider_limits[0].max_active = 3;
        child
    }

    #[test]
    fn equal_capabilities_with_lower_allowance_narrow() {
        assert_eq!(grant_narrows(&child(), &parent()), Ok(()));
        // The parent never narrows its own child back.
        assert_eq!(
            grant_narrows(&parent(), &child()),
            Err(MANAGER_SCOPE_WIDENED)
        );
    }

    /// I2: widening each dimension once yields its typed refusal.
    #[test]
    fn each_widened_dimension_is_refused_with_its_code() {
        type Widen = fn(&mut GrantBoundsV1);
        let table: [(&str, Widen, &str); 9] = [
            (
                "coverage",
                |c| c.coverage.as_mut().unwrap().push(Uuid::from_u128(3)),
                MANAGER_SCOPE_WIDENED,
            ),
            (
                "capability",
                |c| c.capabilities.push(ManagerCapabilityV2::Topology),
                MANAGER_CAPABILITY_WIDENED,
            ),
            (
                "launch",
                |c| c.launches.push(launch("c")),
                MANAGER_CAPABILITY_WIDENED,
            ),
            (
                "allowance equal",
                |c| c.allowances[0] = 4,
                MANAGER_ALLOWANCE_EXCEEDED,
            ),
            (
                "zero allowance",
                |c| c.allowances[2] = 1,
                MANAGER_ALLOWANCE_EXCEEDED,
            ),
            (
                "provider ceiling",
                |c| c.provider_limits[0].max_active = 4,
                MANAGER_ALLOWANCE_EXCEEDED,
            ),
            (
                "direct reports",
                |c| c.max_direct_reports = 6,
                MANAGER_ALLOWANCE_EXCEEDED,
            ),
            (
                "spend",
                |c| c.max_spend_usd = Some(10.5),
                MANAGER_ALLOWANCE_EXCEEDED,
            ),
            (
                "uncapped spend",
                |c| c.max_spend_usd = None,
                MANAGER_ALLOWANCE_EXCEEDED,
            ),
        ];
        for (dimension, widen, code) in table {
            let mut widened = child();
            widen(&mut widened);
            assert_eq!(grant_narrows(&widened, &parent()), Err(code), "{dimension}");
        }
    }

    #[test]
    fn an_uncapped_parent_may_have_a_capped_child() {
        let mut parent = parent();
        parent.max_spend_usd = None;
        assert_eq!(grant_narrows(&child(), &parent), Ok(()));
    }

    #[test]
    fn area_bounds_carry_every_allowance_dimension() {
        use crate::manager_nodes::{
            MANAGER_NODE_DEFAULT_MAX_DIRECT_REPORTS, ManagerNodeAllowanceV1,
        };
        let grant = ManagerNodeGrantV1 {
            capabilities: vec![ManagerCapabilityV2::WorkPlan],
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
        };
        let mut narrower = grant.clone();
        narrower.allowance.max_created_containers = 7;
        narrower.allowance.max_created_sessions = 15;
        narrower.allowance.max_active_sessions = 7;
        narrower.allowance.max_build_slots = 2;
        narrower.allowance.max_disk_gib = 89;
        assert_eq!(grant_narrows(&narrower.bounds(), &grant.bounds()), Ok(()));
        let mut disk = narrower.clone();
        disk.allowance.max_disk_gib = 90;
        assert_eq!(
            grant_narrows(&disk.bounds(), &grant.bounds()),
            Err(MANAGER_ALLOWANCE_EXCEEDED)
        );
    }

    /// #1302, plan §2.2: siblings that each narrow the parent may still not
    /// sum to it. Two children of 3 of 4 containers overflow by 3; spend may
    /// reach the cap; a zero parent dimension is not an aggregate; an edit
    /// that does not add to an existing overflow is not worse.
    #[test]
    fn sibling_allowances_sum_below_the_parent() {
        let one = child();
        let mut small = child();
        small.allowances = vec![1, 1, 0];
        small.provider_limits[0].max_active = 1;
        small.max_spend_usd = Some(4.0);
        let overflow = allowance_overflow(&[&one, &small], &parent());
        assert_eq!(
            overflow,
            [
                ("allowance:0".to_string(), 1),
                ("allowance:1".to_string(), 1),
                ("provider:Codex".to_string(), 1),
                ("spend_micro_usd".to_string(), 4_000_000),
            ]
        );
        assert!(allowance_worsened(None, &overflow));
        let mut fits = small.clone();
        fits.allowances = vec![0, 0, 0];
        fits.provider_limits[0].max_active = 0;
        fits.max_spend_usd = Some(0.0);
        let fitting = allowance_overflow(&[&one, &fits], &parent());
        assert!(
            fitting.iter().all(|(_, overflow)| *overflow <= 0),
            "{fitting:?}"
        );
        assert!(!allowance_worsened(None, &fitting));
        // Spend may reach the cap exactly.
        let mut half = fits.clone();
        half.max_spend_usd = Some(5.0);
        let mut other = fits;
        other.max_spend_usd = Some(5.0);
        assert!(!allowance_worsened(
            None,
            &allowance_overflow(&[&half, &other], &parent())
        ));
        // Already over: an unchanged or smaller overflow is not worse.
        assert!(!allowance_worsened(Some(&overflow), &overflow));
        let mut bigger = small;
        bigger.allowances[0] = 2;
        assert!(allowance_worsened(
            Some(&overflow),
            &allowance_overflow(&[&one, &bigger], &parent())
        ));
    }
}
