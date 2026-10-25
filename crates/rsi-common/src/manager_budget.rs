//! #2502: one obvious place to see and raise a manager chain's budget.
//!
//! Two halves share these types:
//!
//! - The creation refusals (`manager_v2_creation_limit`,
//!   `manager_ancestor_allowance_exceeded`) carry a [`CreationCapRefusalV1`]:
//!   the node that refused, the cap field, used and limit, and the exact TUI
//!   path that raises it. The store renders it into the refusal message (a
//!   stable `code: key=value ...` line, so every existing code-prefix reader
//!   keeps working) and the RPC layer parses it back into typed `error.data`.
//! - The operator's Budget panel in the global manager workspace reads a
//!   [`GlobalManagerBudgetV1`]: every cap of every node along each covered
//!   project's chain (portfolio nodes, then the project manager) as used and
//!   limit, with the binding node per cap marked.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The acting principal's own created-sessions/containers cap is spent.
pub const MANAGER_V2_CREATION_LIMIT: &str = "manager_v2_creation_limit";

/// The one TUI path that raises any manager budget cap (#2502). Kept in one
/// place so a refusal, the panel help and the docs never drift apart.
pub const BUDGET_PANEL_PATH: &str = "TUI: gm (global manager workspace) > b Budget";

/// Where a delegate's (area node or Epic lead) allowance is edited: the
/// Budget panel has no delegate rows (#2520).
pub const DELEGATE_EDIT_PATH: &str = "TUI: :manager tree";

/// Operator-only RPC: read the Budget panel.
pub const GET_GLOBAL_MANAGER_BUDGET: &str = "GetGlobalManagerBudget";

/// Which kind of manager principal a cap belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetNodeKindV1 {
    /// A portfolio node (global, pinnacle, swarm, ...): its grant's project
    /// policy caps this node and everything under it, per covered project.
    Portfolio,
    /// A project manager's own V2 policy: caps this PM's own launches.
    Project,
    /// An area node or Epic lead acting under a project manager.
    Delegate,
}

impl BudgetNodeKindV1 {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Portfolio => "portfolio",
            Self::Project => "project",
            Self::Delegate => "delegate",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "portfolio" => Self::Portfolio,
            "project" => Self::Project,
            "delegate" => Self::Delegate,
            _ => return None,
        })
    }

    /// What this kind of policy governs, in the operator's words.
    pub fn governs(self) -> &'static str {
        match self {
            Self::Portfolio => "this node and everything under it",
            Self::Project => "this PM's own launches",
            Self::Delegate => "this delegate's own launches",
        }
    }
}

/// One budget cap of a manager policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetCapFieldV1 {
    CreatedSessions,
    ActiveSessions,
    CreatedContainers,
    SpendUsd,
}

impl BudgetCapFieldV1 {
    pub const ALL: [Self; 4] = [
        Self::CreatedSessions,
        Self::ActiveSessions,
        Self::CreatedContainers,
        Self::SpendUsd,
    ];

    /// The `ManagerPolicyV2` field this cap edits.
    pub fn policy_field(self) -> &'static str {
        match self {
            Self::CreatedSessions => "max_created_sessions",
            Self::ActiveSessions => "max_active_sessions",
            Self::CreatedContainers => "max_created_containers",
            Self::SpendUsd => "max_spend_usd",
        }
    }

    pub fn from_policy_field(field: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|cap| cap.policy_field() == field)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::CreatedSessions => "created sessions",
            Self::ActiveSessions => "active sessions",
            Self::CreatedContainers => "containers",
            Self::SpendUsd => "spend (USD)",
        }
    }

    /// Whether the count only ever grows (a lifetime allowance) rather than
    /// being a live level that falls as work ends.
    pub fn is_cumulative(self) -> bool {
        matches!(
            self,
            Self::CreatedSessions | Self::CreatedContainers | Self::SpendUsd
        )
    }

    /// One `+` step in the Budget panel.
    pub fn step(self) -> f64 {
        match self {
            Self::CreatedSessions => 32.0,
            Self::ActiveSessions => 1.0,
            Self::CreatedContainers => 4.0,
            Self::SpendUsd => 10.0,
        }
    }

    /// The largest value `ManagerPolicyV2::validate` accepts.
    pub fn max(self) -> f64 {
        match self {
            Self::CreatedSessions => 1024.0,
            Self::ActiveSessions => 100.0,
            Self::CreatedContainers => 64.0,
            Self::SpendUsd => 1_000_000.0,
        }
    }

    /// Read this cap from a policy (`None`: no cap, spend only).
    pub fn limit_of(self, policy: &crate::harness_manager_v2::ManagerPolicyV2) -> Option<f64> {
        match self {
            Self::CreatedSessions => Some(f64::from(policy.max_created_sessions)),
            Self::ActiveSessions => Some(f64::from(policy.max_active_sessions)),
            Self::CreatedContainers => Some(f64::from(policy.max_created_containers)),
            Self::SpendUsd => policy.max_spend_usd,
        }
    }

    /// The value `apply` actually stores for an entered `value` (`None`: no
    /// cap, spend only): clamped to the valid range, counts rounded to whole
    /// numbers, active sessions at least 1, and a spend of 0 meaning no cap.
    /// The review and the read-back compare against this, never the raw entry.
    pub fn normalize(self, value: f64) -> Option<f64> {
        let value = value.clamp(0.0, self.max());
        match self {
            Self::CreatedSessions | Self::CreatedContainers => Some(value.round()),
            Self::ActiveSessions => Some(value.round().max(1.0)),
            Self::SpendUsd => (value > 0.0).then_some(value),
        }
    }

    /// Write `value` into this cap of `policy` (see [`Self::normalize`]).
    // Normalized first, so the casts cannot truncate.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn apply(self, policy: &mut crate::harness_manager_v2::ManagerPolicyV2, value: f64) {
        let stored = self.normalize(value);
        match self {
            Self::CreatedSessions => {
                policy.max_created_sessions = stored.unwrap_or(0.0) as u16;
            }
            Self::ActiveSessions => policy.max_active_sessions = stored.unwrap_or(1.0) as u16,
            Self::CreatedContainers => {
                policy.max_created_containers = stored.unwrap_or(0.0) as u16;
            }
            Self::SpendUsd => policy.max_spend_usd = stored,
        }
    }
}

/// The typed detail of a creation refusal (#2502).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreationCapRefusalV1 {
    /// `manager_v2_creation_limit` or `manager_ancestor_allowance_exceeded`.
    pub code: String,
    pub node_kind: BudgetNodeKindV1,
    /// The portfolio node id, or the project id for a project manager, or
    /// the delegate's manager session id.
    pub node_id: Uuid,
    pub project_id: Uuid,
    /// The `ManagerPolicyV2` field to raise.
    pub cap_field: String,
    pub used: i64,
    pub limit: i64,
    /// Creations this admission asked for.
    pub needed: i64,
    /// Creations of this request already charged (a relaunch).
    pub counted: i64,
    /// Counting window start (the grant or scope start). Counts never reset.
    pub window_start: Option<String>,
    /// The exact TUI path that raises this cap.
    pub raise_in: String,
    /// #3416: the rolling window `used` counts over, in hours (sessions
    /// only; a container cap is a lifetime count and carries `None`).
    #[serde(default)]
    pub window_hours: Option<u16>,
    /// #3416: when the next creation leaves the window and a slot frees
    /// (RFC3339), or `None` when nothing is counted in the window.
    #[serde(default)]
    pub next_free_at: Option<String>,
}

/// #206: the share of a creation cap at which a manager is warned (the
/// `manager_creation_cap_near` health condition), in percent.
pub const CREATION_CAP_WARN_PCT: i64 = 80;

/// #206: one of a manager seat's own creation caps, counted and limited
/// exactly as its `manager_v2_creation_limit` refusal counts it, with the
/// raise path that refusal prints. Read-only: the overview and the health
/// wake show it; admission alone enforces the cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreationCapUsageV1 {
    /// `max_created_sessions` or `max_created_containers`.
    pub cap_field: String,
    pub used: i64,
    pub limit: i64,
    /// The exact path that raises this cap (the refusal's `raise_in`).
    pub raise_in: String,
    /// #3416: the rolling window `used` counts over, in hours (sessions
    /// only).
    #[serde(default)]
    pub window_hours: Option<u16>,
    /// #3416: when the next creation leaves the window (RFC3339).
    #[serde(default)]
    pub next_free_at: Option<String>,
    /// #3416: every charged creation ever, shown beside the window count.
    /// Informational: it is never compared with `limit`.
    #[serde(default)]
    pub lifetime_total: Option<i64>,
}

/// #3416: the one phrase every surface shows for a windowed creation cap,
/// `N of M created in the last 24 h; next slot frees at <time>`.
#[must_use]
pub fn creation_window_summary(
    used: i64,
    limit: i64,
    window_hours: u16,
    next_free_at: Option<&str>,
) -> String {
    let base = format!("{used} of {limit} created in the last {window_hours} h");
    match next_free_at {
        Some(at) => {
            let shown = chrono::DateTime::parse_from_rfc3339(at).map_or_else(
                |_| at.to_string(),
                |t| {
                    t.with_timezone(&chrono::Utc)
                        .format("%Y-%m-%d %H:%M UTC")
                        .to_string()
                },
            );
            format!("{base}; next slot frees at {shown}")
        }
        None => base,
    }
}

impl CreationCapUsageV1 {
    /// The windowed phrase, or `None` for a lifetime (container) cap.
    #[must_use]
    pub fn window_summary(&self) -> Option<String> {
        Some(creation_window_summary(
            self.used,
            self.limit,
            self.window_hours?,
            self.next_free_at.as_deref(),
        ))
    }

    /// True at or above [`CREATION_CAP_WARN_PCT`] of a nonzero limit. A zero
    /// limit is the operator's explicit "no creations", not an approaching wall.
    pub fn near_limit(&self) -> bool {
        self.limit > 0 && self.used.saturating_mul(100) >= self.limit * CREATION_CAP_WARN_PCT
    }
}

impl CreationCapRefusalV1 {
    /// The raise path for one node's cap, naming the row to select. A
    /// delegate has no Budget panel row (the panel lists the portfolio chain
    /// and the project manager only); its allowance is edited in the manager
    /// tree.
    pub fn raise_path(kind: BudgetNodeKindV1, node_id: Uuid, cap_field: &str) -> String {
        let label =
            BudgetCapFieldV1::from_policy_field(cap_field).map_or(cap_field, |field| field.label());
        let id = node_id.to_string();
        let short = &id[..8];
        match kind {
            BudgetNodeKindV1::Delegate => format!(
                "{DELEGATE_EDIT_PATH} > the delegate row for manager session {short} (area or Epic) > e > {label}"
            ),
            _ => format!(
                "{BUDGET_PANEL_PATH} > {} {short} > {label} > + or type a number",
                kind.as_str()
            ),
        }
    }

    /// #3028: the raise path for a satellite manager seat whose policy a hub
    /// pushed. The hub's project manager owns that cap (within the hub's own
    /// grant) and raises it with `AgentPushSatelliteManagerPolicy`, so the seat
    /// reports the spent cap to the hub first; the operator path stays as the
    /// fallback. Only the pushed fields (`max_created_sessions`,
    /// `max_active_sessions`) can be raised that way.
    pub fn hub_report_path(cap_field: &str, used: i64, limit: i64, operator_path: &str) -> String {
        // #3416: the session cap is a per-window allowance that refills.
        let per_window = if cap_field == "max_created_sessions" {
            " in the last created_sessions_window_hours (it refills as creations leave the window)"
        } else {
            ""
        };
        format!(
            "report to the hub: AgentReportToHub kind=result naming {cap_field} used {used} of {limit}{per_window}, so the hub PM can raise it with AgentPushSatelliteManagerPolicy; fallback: {operator_path}"
        )
    }

    /// Whether a pushed-policy hub can raise `cap_field` (the push patch
    /// carries the session counts only, not containers or spend).
    pub fn hub_can_raise(cap_field: &str) -> bool {
        matches!(cap_field, "max_created_sessions" | "max_active_sessions")
    }

    /// #3416: the windowed phrase (see [`creation_window_summary`]), or
    /// `None` for a lifetime cap.
    #[must_use]
    pub fn window_summary(&self) -> Option<String> {
        Some(creation_window_summary(
            self.used,
            self.limit,
            self.window_hours?,
            self.next_free_at.as_deref(),
        ))
    }

    /// The refusal message: `code: key=value ...`, `raise_in` quoted last.
    pub fn message(&self) -> String {
        format!(
            "{}: node_kind={} node_id={} project_id={} cap_field={} used={} limit={} needed={} counted={} window_start={}{} raise_in=\"{}\"",
            self.code,
            self.node_kind.as_str(),
            self.node_id,
            self.project_id,
            self.cap_field,
            self.used,
            self.limit,
            self.needed,
            self.counted,
            self.window_start.as_deref().unwrap_or("scope_start"),
            self.window_hours
                .map(|hours| {
                    format!(
                        " window_hours={hours} next_free_at={}",
                        self.next_free_at.as_deref().unwrap_or("none")
                    )
                })
                .unwrap_or_default(),
            self.raise_in,
        )
    }

    /// Parse a message [`Self::message`] produced (`None` for any other).
    /// The refusal may sit behind a display prefix such as
    /// `Invalid parameter: `, so the code is found where it starts.
    pub fn parse(message: &str) -> Option<Self> {
        let start = [
            MANAGER_V2_CREATION_LIMIT,
            crate::global_manager::MANAGER_ANCESTOR_ALLOWANCE_EXCEEDED,
        ]
        .into_iter()
        .filter_map(|code| message.find(&format!("{code}: node_kind=")))
        .min()?;
        let (code, rest) = message[start..].split_once(": ")?;
        let (fields, raise) = rest.split_once(" raise_in=\"")?;
        let raise_in = raise.strip_suffix('"')?.to_string();
        let mut map = std::collections::HashMap::new();
        for pair in fields.split(' ') {
            let (key, value) = pair.split_once('=')?;
            map.insert(key, value);
        }
        let int = |key: &str| map.get(key)?.parse::<i64>().ok();
        let window_start = map.get("window_start").copied()?;
        Some(Self {
            code: code.to_string(),
            node_kind: BudgetNodeKindV1::parse(map.get("node_kind")?)?,
            node_id: map.get("node_id")?.parse().ok()?,
            project_id: map.get("project_id")?.parse().ok()?,
            cap_field: (*map.get("cap_field")?).to_string(),
            used: int("used")?,
            limit: int("limit")?,
            needed: int("needed")?,
            counted: int("counted")?,
            window_start: (window_start != "scope_start").then(|| window_start.to_string()),
            raise_in,
            window_hours: map.get("window_hours").and_then(|v| v.parse().ok()),
            next_free_at: map
                .get("next_free_at")
                .filter(|v| **v != "none")
                .map(|v| (*v).to_string()),
        })
    }
}

/// Operator-only: read the Budget panel of a portfolio node (the active
/// global node when `node_id` is omitted).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetGlobalManagerBudgetRequestV1 {
    #[serde(default)]
    pub node_id: Option<Uuid>,
}

/// One cap of one node: used and limit (`None`: no cap set).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetCapV1 {
    pub field: BudgetCapFieldV1,
    pub used: f64,
    pub limit: Option<f64>,
    /// The lowest headroom for this cap along the project's chain: raising
    /// any other node's limit admits nothing more.
    #[serde(default)]
    pub binding: bool,
    /// Spend only: the spend of this node is not fully known (an unknown
    /// observation, or no known total), so admission refuses every launch
    /// under a finite cap (`manager_v2_spend_unknown`) whatever the headroom.
    #[serde(default)]
    pub spend_unknown: bool,
}

impl BudgetCapV1 {
    /// Whether a finite spend cap is refusing launches because the spend is
    /// not known, not because it is used up.
    pub fn blocked_unknown_spend(&self) -> bool {
        self.field == BudgetCapFieldV1::SpendUsd && self.spend_unknown && self.limit.is_some()
    }

    /// Room left, as admission sees it (`None`: unlimited). An unknown spend
    /// under a finite cap admits nothing, so it has no room.
    pub fn headroom(&self) -> Option<f64> {
        if self.blocked_unknown_spend() {
            return Some(0.0);
        }
        self.limit.map(|limit| (limit - self.used).max(0.0))
    }
}

/// One node of a project's chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetNodeV1 {
    pub kind: BudgetNodeKindV1,
    /// Portfolio node id, or the project id for the project manager.
    pub node_id: Uuid,
    pub label: String,
    /// The edit fence: a portfolio grant version or the PM policy version.
    pub version: i64,
    /// The second fence: a portfolio authority epoch or the PM scope version.
    pub epoch: i64,
    /// When the counts started (the grant or scope start).
    pub counted_since: Option<String>,
    pub caps: Vec<BudgetCapV1>,
}

impl BudgetNodeV1 {
    pub fn cap(&self, field: BudgetCapFieldV1) -> Option<&BudgetCapV1> {
        self.caps.iter().find(|cap| cap.field == field)
    }
}

impl GlobalManagerBudgetV1 {
    /// One cap of one node in one covered project's chain.
    pub fn cap_of(
        &self,
        project: Uuid,
        kind: BudgetNodeKindV1,
        node_id: Uuid,
        field: BudgetCapFieldV1,
    ) -> Option<&BudgetCapV1> {
        self.projects
            .iter()
            .find(|entry| entry.project_id == project)?
            .chain
            .iter()
            .find(|node| node.kind == kind && node.node_id == node_id)?
            .cap(field)
    }

    /// #2521: whether the portfolio node `parent` has less room left than
    /// `own` would have at `own_limit`, in any project both govern, using the
    /// accounting headroom (`limit - used` under each node's own counting
    /// window, ancestor-origin charges included) rather than raw limits.
    /// `None` when no covered project lists both nodes (usage unavailable).
    pub fn parent_binds_first(
        &self,
        own: (BudgetNodeKindV1, Uuid),
        parent: Uuid,
        field: BudgetCapFieldV1,
        own_limit: Option<f64>,
    ) -> Option<bool> {
        let mut known = false;
        let mut binds = false;
        for project in &self.projects {
            let find = |kind, id| {
                project
                    .chain
                    .iter()
                    .find(|node| node.kind == kind && node.node_id == id)
                    .and_then(|node| node.cap(field))
            };
            let (Some(own_cap), Some(parent_cap)) = (
                find(own.0, own.1),
                find(BudgetNodeKindV1::Portfolio, parent),
            ) else {
                continue;
            };
            known = true;
            let own_room = if own_cap.blocked_unknown_spend() {
                Some(0.0)
            } else {
                own_limit.map(|limit| (limit - own_cap.used).max(0.0))
            };
            if let Some(parent_room) = parent_cap.headroom() {
                binds |= own_room.is_none_or(|room| parent_room < room);
            }
        }
        known.then_some(binds)
    }
}

/// One covered project: its chain from the topmost portfolio node down to
/// the project manager.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetProjectV1 {
    pub project_id: Uuid,
    pub project_name: String,
    pub chain: Vec<BudgetNodeV1>,
}

/// The Budget panel's snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlobalManagerBudgetV1 {
    /// The portfolio node the panel is opened on.
    pub node_id: Uuid,
    pub node_label: String,
    pub projects: Vec<BudgetProjectV1>,
    /// Plainly whether counts reset.
    pub resets: String,
}

/// The plain statement of the counting window (#2502 scope 4, #2518): the
/// creation counts restart on any operator edit of a portfolio grant; spend
/// is not reset (historical spend of work the node still covers stays
/// counted), and a raise changes only the limit.
pub const BUDGET_RESETS_TEXT: &str = "no timed reset of containers or spend, but created sessions refill: a node counts the sessions created in the last created_sessions_window_hours (default 24), the oldest creation leaves the window and frees a slot, and a seat rotation never resets it; created containers count since the node's current grant began (any operator edit of that grant, a raise here included, starts a new count of creations); spend is NOT reset by an edit: it stays the spend of the work the node still covers, so a raise changes the limit only; a PM counts over its current scope; active sessions are live";

/// Mark, per cap field, the node with the lowest headroom along `chain`
/// (ties go to the upper node: raising a lower one admits nothing).
pub fn mark_binding(chain: &mut [BudgetNodeV1]) {
    for field in BudgetCapFieldV1::ALL {
        let mut best: Option<(usize, f64)> = None;
        for (index, node) in chain.iter().enumerate() {
            let Some(room) = node.cap(field).and_then(BudgetCapV1::headroom) else {
                continue;
            };
            if best.is_none_or(|(_, least)| room < least) {
                best = Some((index, room));
            }
        }
        for (index, node) in chain.iter_mut().enumerate() {
            for cap in node.caps.iter_mut().filter(|cap| cap.field == field) {
                cap.binding = best.is_some_and(|(at, _)| at == index);
            }
        }
    }
}

/// The `ConfigurePortfolioNode` edit that sets one cap of `node`'s own
/// grant (its project policy: "this node and everything under it") to
/// `value`, fenced on the node's current grant version and epoch. Every
/// other field of the grant is carried unchanged.
pub fn raise_portfolio_request(
    node: &crate::portfolio_nodes::PortfolioNodeV1,
    field: BudgetCapFieldV1,
    value: f64,
    idempotency_key: String,
) -> crate::portfolio_nodes::ConfigurePortfolioNodeRequestV1 {
    let mut policy = node.grant.project_policy.clone();
    field.apply(&mut policy, value);
    crate::portfolio_nodes::ConfigurePortfolioNodeRequestV1 {
        node_id: Some(node.node_id),
        parent_node_id: node.parent_node_id,
        adopt_node_ids: Vec::new(),
        expected_parent_grant_version: None,
        tier_label: node.tier_label.clone(),
        seat_session_id: node.grant.seat_session_id,
        project_ids: node.grant.project_ids.clone(),
        allowed_launches: node.grant.allowed_launches.clone(),
        policy,
        child_policy: node.child_policy.clone(),
        max_direct_reports: node.max_direct_reports,
        expected_node_grant_version: node.grant.grant_version,
        expected_authority_epoch: node.authority_epoch,
        idempotency_key,
    }
}

/// The `ConfigureHarnessManagerPolicy` edit that sets one cap of a project
/// manager's own policy ("this PM's own launches") to `value`.
pub fn raise_project_manager_request(
    current: &crate::harness_manager_v2::HarnessManagerPolicyConfigV2,
    field: BudgetCapFieldV1,
    value: f64,
    idempotency_key: String,
) -> crate::harness_manager_v2::ConfigureHarnessManagerPolicyRequestV2 {
    let mut policy = current.policy.clone();
    field.apply(&mut policy, value);
    crate::harness_manager_v2::ConfigureHarnessManagerPolicyRequestV2 {
        project_id: current.project_id,
        expected_scope_version: current.scope_version,
        expected_policy_version: current.row_version,
        idempotency_key,
        policy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal() -> CreationCapRefusalV1 {
        let node = Uuid::from_u128(0x1a2b_3c4d_0000_0000_0000_0000_0000_0001);
        CreationCapRefusalV1 {
            code: crate::global_manager::MANAGER_ANCESTOR_ALLOWANCE_EXCEEDED.into(),
            node_kind: BudgetNodeKindV1::Portfolio,
            node_id: node,
            project_id: Uuid::from_u128(7),
            cap_field: "max_created_sessions".into(),
            used: 388,
            limit: 388,
            needed: 1,
            counted: 0,
            window_start: Some("2026-10-01T00:00:00.000000000Z".into()),
            raise_in: CreationCapRefusalV1::raise_path(
                BudgetNodeKindV1::Portfolio,
                node,
                "max_created_sessions",
            ),
            window_hours: None,
            next_free_at: None,
        }
    }

    /// #3416: a windowed refusal names the window, the count and when a slot
    /// frees, and an old message without them still parses.
    #[test]
    fn a_windowed_refusal_names_the_window_the_count_and_the_next_free_time() {
        let mut detail = refusal();
        detail.window_hours = Some(24);
        detail.next_free_at = Some("2026-10-21T04:12:00.000000000Z".into());
        let message = detail.message();
        assert!(
            message
                .contains("window_hours=24 next_free_at=2026-10-21T04:12:00.000000000Z raise_in="),
            "{message}"
        );
        assert_eq!(CreationCapRefusalV1::parse(&message), Some(detail.clone()));
        assert_eq!(
            detail.window_summary().as_deref(),
            Some("388 of 388 created in the last 24 h; next slot frees at 2026-10-21 04:12 UTC")
        );
        detail.next_free_at = None;
        assert!(detail.message().contains("next_free_at=none"));
        assert_eq!(CreationCapRefusalV1::parse(&detail.message()), Some(detail));
        // A lifetime (container) cap has no window phrase.
        assert_eq!(refusal().window_summary(), None);
        assert!(!refusal().message().contains("window_hours"));
    }

    #[test]
    fn a_refusal_message_round_trips_and_names_the_row_to_raise() {
        let detail = refusal();
        let message = detail.message();
        assert!(message.starts_with("manager_ancestor_allowance_exceeded: node_kind=portfolio"));
        assert!(message.contains("cap_field=max_created_sessions used=388 limit=388"));
        assert!(message.ends_with(
            "raise_in=\"TUI: gm (global manager workspace) > b Budget > portfolio 1a2b3c4d > created sessions > + or type a number\""
        ));
        assert_eq!(CreationCapRefusalV1::parse(&message), Some(detail.clone()));
        // DaemonError's Display puts a prefix before the refusal.
        assert_eq!(
            CreationCapRefusalV1::parse(&format!("Invalid parameter: {message}")),
            Some(detail)
        );
        let mut own = refusal();
        own.code = MANAGER_V2_CREATION_LIMIT.into();
        own.window_start = None;
        assert_eq!(CreationCapRefusalV1::parse(&own.message()), Some(own));
        assert_eq!(
            CreationCapRefusalV1::parse("manager_v2_creation_limit"),
            None
        );
    }

    #[test]
    fn the_binding_cap_is_the_lowest_headroom_and_ties_go_up_the_chain() {
        let node = |kind, used: f64, limit: f64| BudgetNodeV1 {
            kind,
            node_id: Uuid::new_v4(),
            label: String::new(),
            version: 1,
            epoch: 1,
            counted_since: None,
            caps: vec![
                BudgetCapV1 {
                    field: BudgetCapFieldV1::CreatedSessions,
                    used,
                    limit: Some(limit),
                    binding: false,
                    spend_unknown: false,
                },
                BudgetCapV1 {
                    field: BudgetCapFieldV1::SpendUsd,
                    used: 1.0,
                    limit: None,
                    binding: false,
                    spend_unknown: false,
                },
            ],
        };
        let mut chain = vec![
            node(BudgetNodeKindV1::Portfolio, 388.0, 388.0),
            node(BudgetNodeKindV1::Project, 100.0, 100.0),
        ];
        mark_binding(&mut chain);
        assert!(chain[0].caps[0].binding);
        assert!(!chain[1].caps[0].binding);
        chain[1].caps[0].used = 50.0;
        chain[0].caps[0].used = 10.0;
        mark_binding(&mut chain);
        assert!(chain[1].caps[0].binding);
        assert!(chain.iter().all(|node| !node.caps[1].binding));
    }

    #[test]
    fn caps_apply_within_the_policy_range() {
        let mut policy = crate::harness_manager_v2::ManagerPolicyV2::default();
        BudgetCapFieldV1::CreatedSessions.apply(&mut policy, 5000.0);
        assert_eq!(policy.max_created_sessions, 1024);
        BudgetCapFieldV1::ActiveSessions.apply(&mut policy, 0.0);
        assert_eq!(policy.max_active_sessions, 1);
        BudgetCapFieldV1::SpendUsd.apply(&mut policy, 25.5);
        assert_eq!(policy.max_spend_usd, Some(25.5));
        assert_eq!(BudgetCapFieldV1::SpendUsd.limit_of(&policy), Some(25.5));
    }

    /// #2520: a delegate has no Budget panel row, so its refusal names the
    /// manager tree row that edits it, not a panel path that does not exist.
    #[test]
    fn a_hub_pushed_seat_refusal_names_the_hub_report_before_the_operator_path() {
        let operator = CreationCapRefusalV1::raise_path(
            BudgetNodeKindV1::Project,
            Uuid::nil(),
            "max_created_sessions",
        );
        let mut detail = refusal();
        detail.node_kind = BudgetNodeKindV1::Project;
        detail.raise_in =
            CreationCapRefusalV1::hub_report_path("max_created_sessions", 32, 32, &operator);
        assert!(
            detail
                .raise_in
                .starts_with("report to the hub: AgentReportToHub kind=result")
        );
        assert!(detail.raise_in.contains("AgentPushSatelliteManagerPolicy"));
        assert!(detail.raise_in.ends_with(&operator));
        assert!(!detail.raise_in.contains('"'));
        assert_eq!(CreationCapRefusalV1::parse(&detail.message()), Some(detail));
        assert!(CreationCapRefusalV1::hub_can_raise("max_created_sessions"));
        assert!(!CreationCapRefusalV1::hub_can_raise(
            "max_created_containers"
        ));
    }

    #[test]
    fn a_delegate_refusal_names_the_manager_tree_edit_path() {
        let node = Uuid::from_u128(0x7e1e_6a7e_0000_0000_0000_0000_0000_0002);
        let path = CreationCapRefusalV1::raise_path(
            BudgetNodeKindV1::Delegate,
            node,
            "max_created_sessions",
        );
        assert_eq!(
            path,
            "TUI: :manager tree > the delegate row for manager session 7e1e6a7e (area or Epic) > e > created sessions"
        );
        let mut detail = refusal();
        detail.code = MANAGER_V2_CREATION_LIMIT.into();
        detail.node_kind = BudgetNodeKindV1::Delegate;
        detail.node_id = node;
        detail.raise_in = path;
        assert_eq!(CreationCapRefusalV1::parse(&detail.message()), Some(detail));
        // The panel path remains for the nodes the panel does list.
        let panel =
            CreationCapRefusalV1::raise_path(BudgetNodeKindV1::Project, node, "max_spend_usd");
        assert!(panel.starts_with(BUDGET_PANEL_PATH), "{panel}");
    }

    /// #2523: what `apply` stores is what `normalize` says.
    #[test]
    fn normalize_is_what_apply_stores() {
        use crate::harness_manager_v2::ManagerPolicyV2;
        for (field, entered, expected) in [
            (BudgetCapFieldV1::ActiveSessions, 0.0, Some(1.0)),
            (BudgetCapFieldV1::CreatedSessions, 12.5, Some(13.0)),
            (BudgetCapFieldV1::CreatedContainers, 0.0, Some(0.0)),
            (BudgetCapFieldV1::CreatedContainers, 1000.0, Some(64.0)),
            (BudgetCapFieldV1::SpendUsd, 0.0, None),
            (BudgetCapFieldV1::SpendUsd, 25.5, Some(25.5)),
        ] {
            assert_eq!(field.normalize(entered), expected, "{field:?} {entered}");
            let mut policy = ManagerPolicyV2::default();
            field.apply(&mut policy, entered);
            assert_eq!(field.limit_of(&policy), expected, "{field:?} {entered}");
        }
    }

    /// #2519: an unknown spend under a finite cap has no headroom; with no
    /// cap it admits everything.
    #[test]
    fn unknown_spend_under_a_finite_cap_has_no_headroom() {
        let cap = |limit, spend_unknown| BudgetCapV1 {
            field: BudgetCapFieldV1::SpendUsd,
            used: 0.0,
            limit,
            binding: false,
            spend_unknown,
        };
        assert_eq!(cap(Some(100.0), false).headroom(), Some(100.0));
        assert_eq!(cap(Some(100.0), true).headroom(), Some(0.0));
        assert!(cap(Some(100.0), true).blocked_unknown_spend());
        assert_eq!(cap(None, true).headroom(), None);
        assert!(!cap(None, true).blocked_unknown_spend());
    }

    /// #2521: the parent binds by remaining headroom, each node counted in
    /// its own window, not by the larger raw limit.
    #[test]
    fn parent_binds_first_compares_headroom_not_limits() {
        let (parent, child, project) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let node = |kind, id, used: f64, limit: f64| BudgetNodeV1 {
            kind,
            node_id: id,
            label: String::new(),
            version: 1,
            epoch: 1,
            counted_since: None,
            caps: vec![BudgetCapV1 {
                field: BudgetCapFieldV1::CreatedSessions,
                used,
                limit: Some(limit),
                binding: false,
                spend_unknown: false,
            }],
        };
        let budget = |parent_used, child_used| GlobalManagerBudgetV1 {
            node_id: parent,
            node_label: String::new(),
            projects: vec![BudgetProjectV1 {
                project_id: project,
                project_name: String::new(),
                chain: vec![
                    node(BudgetNodeKindV1::Portfolio, parent, parent_used, 388.0),
                    node(BudgetNodeKindV1::Project, child, child_used, 256.0),
                ],
            }],
            resets: String::new(),
        };
        let own = (BudgetNodeKindV1::Project, child);
        let field = BudgetCapFieldV1::CreatedSessions;
        // 388 used of 388: the parent refuses though the PM's 256 is larger
        // than nothing and smaller than the parent's raw limit.
        assert_eq!(
            budget(388.0, 0.0).parent_binds_first(own, parent, field, Some(256.0)),
            Some(true)
        );
        // Parent 388 with 0 used (388 left) against a PM with 1 left: no bind.
        assert_eq!(
            budget(0.0, 255.0).parent_binds_first(own, parent, field, Some(256.0)),
            Some(false)
        );
        // Unknown nodes: usage unavailable.
        assert_eq!(
            budget(0.0, 0.0).parent_binds_first(own, Uuid::from_u128(9), field, Some(256.0)),
            None
        );
    }
}
