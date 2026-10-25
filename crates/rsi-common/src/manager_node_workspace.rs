//! Fractal manager hierarchy S6 (#1240): one snapshot of any manager node.
//!
//! The operator and every manager see a node the same way, whatever its
//! tier: its grant and seat, its child nodes (each with seat status, model,
//! context fill and grant), the projects it covers (PM seat, Issue counts,
//! Running and WaitingApproval counts), its pending escalations and a fleet
//! rollup. There is no per-tier screen or query.
//!
//! - `GetManagerNodeWorkspace {node}` is operator-only (AGENTS.md rule 10):
//!   it lists the node's whole coverage. `GetGlobalManagerWorkspace` (#1213)
//!   is its shim for the single root labelled `global`.
//! - `AgentManagerOverview {}` is the caller's own node with the same shape,
//!   bounded for agents: it lists only the projects the node manages
//!   directly. A child portfolio node appears as one bounded digest (counts
//!   over its coverage), never with its projects or its inbox.
//!   `AgentGlobalOverview` stays as its alias with the v0 result shape.
//! - The fleet rollup is #1232's store aggregation filtered to the node's
//!   coverage; there is no second fleet query.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::fleet::{FleetGroup, FleetOverview, FleetUsage};
use crate::global_manager::{
    GlobalIssueCountsV1, GlobalManagerGrantV1, GlobalManagerWorkspaceV1, GlobalSeatSessionV1,
    GlobalWorkspaceProjectV1,
};
use crate::manager_nodes::{ManagerNodeGrantV1, ManagerNodeSelectorV1};
use crate::manager_tier_routing::ManagerNodeRefV1;
use crate::standing_directives::StandingDirectiveV1;

/// The operator-only node workspace RPC (never in an agent catalog).
pub const OPERATOR_METHODS: [&str; 1] = ["GetManagerNodeWorkspace"];

/// Most children one snapshot lists (a node may hold at most 64 direct
/// reports; a portfolio node's direct projects count separately).
pub const MANAGER_NODE_WORKSPACE_MAX_CHILDREN: usize = 128;
/// Most pending escalations one snapshot lists.
pub const MANAGER_NODE_WORKSPACE_MAX_ESCALATIONS: usize = 64;
/// Longest escalation reason one snapshot carries (bytes, on a char boundary).
pub const MANAGER_NODE_WORKSPACE_MAX_REASON_BYTES: usize = 512;

/// Operator-only `GetManagerNodeWorkspace {node}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetManagerNodeWorkspaceRequestV1 {
    pub node: ManagerNodeRefV1,
}

impl GetManagerNodeWorkspaceRequestV1 {
    /// # Errors
    /// `manager_tier_invalid_request` for a nil id.
    pub fn validate(&self) -> Result<(), &'static str> {
        let id = match self.node {
            ManagerNodeRefV1::Portfolio { node_id } | ManagerNodeRefV1::Area { node_id } => node_id,
            ManagerNodeRefV1::Project { project_id } => project_id,
        };
        if id.is_nil() {
            return Err(crate::manager_tier_routing::MANAGER_TIER_INVALID_REQUEST);
        }
        Ok(())
    }
}

/// `AgentManagerOverview {fleet?}`: the caller's own node.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerOverviewRequestV1 {
    /// #2052: how much of the fleet rollup to return. Defaults to `summary`.
    #[serde(default)]
    pub fleet: ManagerOverviewFleetV1,
    /// #3531: `compact` (default) or `full` (the whole serialized workspace
    /// apart from `fleet`).
    #[serde(default)]
    pub detail: ManagerOverviewDetailV1,
    /// #3531: carry the full text of every standing directive (compact
    /// returns id, scope, title and updated_at only). Implied by `detail:
    /// full`.
    #[serde(default)]
    pub directives: bool,
}

/// How much of the repeated per-seat data `AgentManagerOverview` returns
/// (#3531).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerOverviewDetailV1 {
    /// One seat digest per project child (the project's `manager` row holds
    /// the seat; its `children[].seat` keeps only identity and lineage),
    /// standing directives as id/scope/title/updated_at, every seat's
    /// `coordinator_budget` once per distinct budget under
    /// `coordinator_budgets`, predecessors as a count plus the newest, and
    /// one launches form (`grant.allowed_launches`).
    #[default]
    Compact,
    /// The whole serialized workspace.
    Full,
}

/// Fleet detail of `AgentManagerOverview` (#2052).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerOverviewFleetV1 {
    /// `fleet` is `null`.
    None,
    /// Active agents, 1h/24h totals and one row per model.
    #[default]
    Summary,
    /// The whole rollup (every group, three windows of nine counters).
    Full,
}

impl ManagerNodeFleetV1 {
    /// #2052: the compact agent view of the rollup: `active`, the 1h and 24h
    /// totals and one row per model (1h and 24h invocations, errors and cost).
    #[must_use]
    pub fn summary_value(&self) -> serde_json::Value {
        fn window(usage: &FleetUsage) -> serde_json::Value {
            let mut value = serde_json::json!({"invocations":usage.invocations,"errors":usage.errors,"input":usage.input,"output":usage.output,"cache_read":usage.cache_read,"cache_write":usage.cache_write});
            if usage.cost > 0.0 {
                value["metered_spend_usd"] =
                    serde_json::json!((usage.cost * 10_000.0).round() / 10_000.0);
            }
            value
        }
        let models: Vec<_> = self
            .groups
            .iter()
            .filter(|group| group.dimension == "model")
            .map(|group| {
                serde_json::json!({
                    "model": group.label,
                    "active": group.active,
                    "1h": window(&group.windows[1]),
                    "24h": window(&group.windows[2]),
                })
            })
            .collect();
        serde_json::json!({
            "view": "summary",
            "as_of": self.as_of,
            "active": self.active,
            "totals": {"1h": window(&self.totals[1]), "24h": window(&self.totals[2])},
            "models": models,
            "plan_windows": self.plan_windows,
            "plan_usage_by_role": self.plan_usage_by_role,
            "provider_windows": self.provider_windows,
            "agents_truncated": self.agents_truncated,
            "usage_truncated": self.usage_truncated,
        })
    }
}

impl ManagerNodeWorkspaceV1 {
    /// #2052: the agent-facing JSON of the overview. `Full` is exactly the
    /// serialized workspace; `Summary` and `None` replace only `fleet`.
    ///
    /// # Errors
    /// A serialization error.
    pub fn into_overview_value(
        self,
        request: &AgentManagerOverviewRequestV1,
    ) -> Result<serde_json::Value, serde_json::Error> {
        let compact = match request.fleet {
            ManagerOverviewFleetV1::Full => None,
            ManagerOverviewFleetV1::Summary => Some(self.fleet.summary_value()),
            ManagerOverviewFleetV1::None => Some(serde_json::Value::Null),
        };
        let mut value = serde_json::to_value(&self)?;
        if let (Some(compact), Some(object)) = (compact, value.as_object_mut()) {
            object.insert("fleet".to_string(), compact);
        }
        if request.detail == ManagerOverviewDetailV1::Compact {
            compact_overview(&mut value, request.directives);
        }
        Ok(value)
    }
}

/// Longest standing-directive title in a compact overview (chars).
const DIRECTIVE_TITLE_CHARS: usize = 80;

/// #3531: cut the repeats out of the serialized overview. See
/// [`ManagerOverviewDetailV1::Compact`].
fn compact_overview(value: &mut serde_json::Value, directives_full: bool) {
    use serde_json::Value;
    let Some(root) = value.as_object_mut() else {
        return;
    };
    // The project managers the `projects` rows already carry.
    let manager_seats: std::collections::HashMap<String, String> = root
        .get("projects")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|project| {
            let overview = project.get("overview")?;
            Some((
                overview.get("project_id")?.as_str()?.to_string(),
                overview
                    .pointer("/manager/session_id")?
                    .as_str()?
                    .to_string(),
            ))
        })
        .collect();
    if let Some(children) = root.get_mut("children").and_then(Value::as_array_mut) {
        for child in children {
            let project = child
                .pointer("/node/project_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            let Some(seat) = child.get_mut("seat").and_then(Value::as_object_mut) else {
                continue;
            };
            let duplicated = project.as_deref().is_some_and(|id| {
                manager_seats.get(id).map(String::as_str)
                    == seat.get("session_id").and_then(Value::as_str)
            });
            if duplicated {
                // Same session as `projects[].overview.manager`: keep only
                // what that row lacks.
                seat.retain(|key, _| {
                    matches!(
                        key.as_str(),
                        "session_id"
                            | "project_id"
                            | "launch_turn"
                            | "predecessors"
                            | "predecessors_truncated"
                    )
                });
            }
        }
    }
    // Predecessors: a count plus the newest one.
    for_each_seat(root, &mut |seat| {
        let Some(seat) = seat.as_object_mut() else {
            return;
        };
        let Some(Value::Array(predecessors)) = seat.remove("predecessors") else {
            return;
        };
        seat.insert("predecessor_count".into(), predecessors.len().into());
        if let Some(newest) = predecessors.into_iter().next() {
            seat.insert("newest_predecessor".into(), newest);
        }
    });
    // One launches form: the structured choices stay (they are what a
    // launch request carries); the rendered duplicates go.
    root.remove("launches_display");
    // Standing directives: id, scope, title and updated_at unless asked for.
    if !directives_full {
        if let Some(list) = root.get_mut("standing_directives") {
            digest_directives(list);
        }
        if let Some(projects) = root.get_mut("projects").and_then(Value::as_array_mut) {
            for project in projects {
                if let Some(list) = project.pointer_mut("/overview/standing_directives") {
                    digest_directives(list);
                }
            }
        }
    }
    hoist_coordinator_budgets(root);
}

fn digest_directives(list: &mut serde_json::Value) {
    use serde_json::Value;
    let Some(entries) = list.as_array_mut() else {
        return;
    };
    for entry in entries {
        let text = entry.get("text").and_then(Value::as_str).unwrap_or("");
        let first_line = text.lines().next().unwrap_or("").trim();
        let mut title: String = first_line.chars().take(DIRECTIVE_TITLE_CHARS).collect();
        if first_line.chars().count() > DIRECTIVE_TITLE_CHARS || text.trim() != first_line {
            title.push('…');
        }
        *entry = serde_json::json!({
            "id": entry.get("id"),
            "scope": entry.get("scope"),
            "title": title,
            "updated_at": entry.get("cleared_at").filter(|at| !at.is_null())
                .or_else(|| entry.get("created_at")),
            "text_bytes": text.len(),
        });
    }
}

/// Visit the node's seat, every child's seat and every project's manager row.
fn for_each_seat(
    root: &mut serde_json::Map<String, serde_json::Value>,
    visit: &mut dyn FnMut(&mut serde_json::Value),
) {
    if let Some(seat) = root.get_mut("seat") {
        visit(seat);
    }
    if let Some(children) = root.get_mut("children").and_then(|c| c.as_array_mut()) {
        children
            .iter_mut()
            .filter_map(|child| child.get_mut("seat"))
            .for_each(&mut *visit);
    }
    if let Some(projects) = root.get_mut("projects").and_then(|p| p.as_array_mut()) {
        projects
            .iter_mut()
            .filter_map(|project| project.pointer_mut("/overview/manager"))
            .for_each(&mut *visit);
    }
}

/// Replace every seat's `coordinator_budget` object with a key into one
/// top-level `coordinator_budgets` map: one entry per distinct budget, keyed
/// `<provider>/<model>` (`#2`, `#3`, ... when a model's budgets differ).
fn hoist_coordinator_budgets(root: &mut serde_json::Map<String, serde_json::Value>) {
    use serde_json::Value;
    let mut budgets = serde_json::Map::new();
    for_each_seat(root, &mut |seat| {
        let Some(seat) = seat.as_object_mut() else {
            return;
        };
        let Some(budget) = seat.get("coordinator_budget").filter(|b| b.is_object()) else {
            return;
        };
        let base = format!(
            "{}/{}",
            budget
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or("?"),
            budget.get("model").and_then(Value::as_str).unwrap_or("?"),
        );
        let mut key = base.clone();
        let mut n = 1;
        while budgets.get(&key).is_some_and(|known| known != budget) {
            n += 1;
            key = format!("{base}#{n}");
        }
        budgets.entry(key.clone()).or_insert_with(|| budget.clone());
        seat.insert("coordinator_budget".into(), Value::String(key));
    });
    if !budgets.is_empty() {
        root.insert("coordinator_budgets".into(), Value::Object(budgets));
    }
}

/// Session-level counts summed over a node's coverage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerNodeCountsV1 {
    pub issues: GlobalIssueCountsV1,
    pub running_sessions: i64,
    pub waiting_approval_sessions: i64,
    pub pending_questions: i64,
    pub pending_approvals: i64,
}

/// An area node's grant as a workspace shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagerNodeAreaGrantV1 {
    pub project_id: Uuid,
    pub active: bool,
    pub grant_version: i64,
    pub authority_epoch: i64,
    pub selector: Option<ManagerNodeSelectorV1>,
    pub grant: Option<ManagerNodeGrantV1>,
}

/// One child of a node, as a bounded digest: its identity, grant, seat and
/// counts over its coverage. A child's own children, projects and inbox are
/// never included.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagerNodeChildV1 {
    pub node: ManagerNodeRefV1,
    /// A portfolio node's tier label, a project's name, or `area`.
    pub label: String,
    /// `active` or `revoked`; a project without a live PM is `vacant`.
    pub state: String,
    /// `operator` or `node:<uuid>` (portfolio children only).
    pub grantor: Option<String>,
    /// The child's grant version (a project's manager scope version), 0
    /// when it has none.
    pub grant_version: i64,
    /// The child's live seat (status, model, context fill).
    pub seat: Option<GlobalSeatSessionV1>,
    /// The child's coverage (a project child: its project; an area child:
    /// the project it lives in).
    pub project_ids: Vec<Uuid>,
    /// Counts summed over the child's coverage (`None` for an area child,
    /// whose coverage is part of one project).
    pub counts: Option<ManagerNodeCountsV1>,
    /// Escalations waiting on the child.
    pub pending_escalations: i64,
}

/// One escalation waiting on the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerNodePendingEscalationV1 {
    /// The in-project escalation (`manager_node_escalations.id`).
    pub escalation_id: Uuid,
    pub project_id: Uuid,
    pub subject_id: Uuid,
    /// Truncated to `MANAGER_NODE_WORKSPACE_MAX_REASON_BYTES`.
    pub reason: String,
    /// The hop above the project root that holds it, for a portfolio node.
    pub hop: Option<i64>,
    pub created_at: DateTime<Utc>,
}

/// #1232's fleet aggregation filtered to a node's coverage: active agents
/// and usage windows, without the per-agent rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagerNodeFleetV1 {
    pub as_of: DateTime<Utc>,
    /// Active (Starting, Running, WaitingApproval) agents in the coverage.
    pub active: u64,
    /// Usage over the `FLEET_WINDOWS` (5m, 1h, 24h).
    pub totals: [FleetUsage; 3],
    #[serde(default)]
    pub plan_windows: Vec<crate::rpc::ProviderRateLimitSnapshot>,
    #[serde(default)]
    pub plan_usage_by_role: Vec<crate::fleet::PlanRoleUsageV1>,
    /// Project, provider and model groups of the covered rows.
    pub groups: Vec<FleetGroup>,
    /// The aggregation's row bounds were hit; counts may be partial.
    pub agents_truncated: bool,
    pub usage_truncated: bool,
    /// Account-level plan windows shared across projects and models.
    #[serde(default)]
    pub provider_windows: Vec<crate::rpc::ProviderRateLimitSnapshot>,
}

impl ManagerNodeFleetV1 {
    /// No rows (a snapshot taken without the rollup).
    #[must_use]
    pub fn empty(as_of: DateTime<Utc>) -> Self {
        Self {
            as_of,
            active: 0,
            totals: Default::default(),
            plan_windows: Vec::new(),
            plan_usage_by_role: Vec::new(),
            groups: Vec::new(),
            agents_truncated: false,
            usage_truncated: false,
            provider_windows: Vec::new(),
        }
    }

    /// The rollup of a (coverage-filtered) fleet snapshot.
    #[must_use]
    pub fn from_overview(overview: FleetOverview) -> Self {
        Self {
            as_of: overview.as_of,
            active: overview.agents.len() as u64,
            totals: overview.totals,
            plan_windows: overview.plan_windows,
            plan_usage_by_role: overview.plan_usage_by_role,
            groups: overview.groups,
            agents_truncated: overview.agents_truncated,
            usage_truncated: overview.usage_truncated,
            provider_windows: Vec::new(),
        }
    }
}

/// #2021: the latest time a portfolio node's seat handed its own seat to a
/// successor it launched (`AgentManagerAppointChild` on its own node).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerNodeSeatHandoffV1 {
    pub appointment_id: Uuid,
    /// The seat that handed the node on.
    pub old_seat: Uuid,
    /// The seat that holds it from then on.
    pub new_seat: Uuid,
    /// Where the old seat wrote its handoff (a repo path or a commit).
    pub handoff: Option<String>,
    pub handed_at: DateTime<Utc>,
    /// #2749: the handoff file was modified after the appointment completed
    /// (the retired seat appended to it, or anything else wrote it). A
    /// successor that read it earlier must read it again. The daemon sets it
    /// when it assembles a workspace, from a handoff that names an absolute
    /// or `~/` path it can stat; otherwise it stays `false`.
    #[serde(default)]
    pub handoff_changed_after_appointment: bool,
    /// #2749: that file's modification time when it is later than
    /// `handed_at`; `None` otherwise.
    #[serde(default)]
    pub handoff_modified_at: Option<DateTime<Utc>>,
}

/// `GetManagerNodeWorkspace` and `AgentManagerOverview` result.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ManagerNodeWorkspaceV1 {
    pub node: ManagerNodeRefV1,
    /// A portfolio node's tier label, a project's name, or `area`.
    pub label: String,
    /// `active` or `revoked`; a project without a live PM is `vacant`.
    pub state: String,
    /// `parent_of(node)`; `None` is the operator.
    pub parent: Option<ManagerNodeRefV1>,
    /// A portfolio node's active grant, else its last (revoked) one. Its
    /// `project_ids` are the node's coverage.
    pub grant: Option<GlobalManagerGrantV1>,
    /// `operator` or `node:<uuid>` (portfolio nodes).
    pub grantor: Option<String>,
    pub max_direct_reports: Option<u16>,
    /// An area node's grant.
    pub area: Option<ManagerNodeAreaGrantV1>,
    /// The node's seat session; `None` when vacant or gone.
    pub seat: Option<GlobalSeatSessionV1>,
    /// Child nodes: child portfolio nodes, then the projects this node
    /// manages directly, then child areas.
    pub children: Vec<ManagerNodeChildV1>,
    pub children_truncated: bool,
    /// Covered projects. The operator workspace lists the whole coverage;
    /// the agent overview lists only the projects the node manages directly
    /// (a child portfolio node's projects stay in its digest).
    pub projects: Vec<GlobalWorkspaceProjectV1>,
    /// Granted project ids whose project no longer exists.
    pub missing_project_ids: Vec<Uuid>,
    pub escalations: Vec<ManagerNodePendingEscalationV1>,
    pub escalations_truncated: bool,
    pub fleet: ManagerNodeFleetV1,
    /// A portfolio node's latest self-handoff (#2021); `None` when its seat
    /// never handed it on.
    #[serde(default)]
    pub last_seat_handoff: Option<ManagerNodeSeatHandoffV1>,
    /// #2649: the standing operator directives active on this node's scope,
    /// verbatim with author, provenance and timestamps. Each covered project's
    /// own directives are on its entry in `projects`.
    #[serde(default)]
    pub standing_directives: Vec<StandingDirectiveV1>,
}

impl ManagerNodeWorkspaceV1 {
    /// The `GetGlobalManagerWorkspace` (#1213) projection of a portfolio
    /// node's operator workspace.
    #[must_use]
    pub fn into_global(self) -> GlobalManagerWorkspaceV1 {
        GlobalManagerWorkspaceV1 {
            grant: self.grant,
            seat: self.seat,
            projects: self.projects,
            missing_project_ids: self.missing_project_ids,
        }
    }
}

/// Cut `text` to at most `max` bytes on a char boundary.
#[must_use]
pub fn bounded_reason(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_request_names_one_node_and_refuses_extra_fields() {
        let id = Uuid::new_v4();
        let request: GetManagerNodeWorkspaceRequestV1 = serde_json::from_value(
            serde_json::json!({"node": {"kind": "portfolio", "node_id": id}}),
        )
        .unwrap();
        assert_eq!(request.node, ManagerNodeRefV1::Portfolio { node_id: id });
        assert_eq!(request.validate(), Ok(()));
        assert!(
            serde_json::from_value::<GetManagerNodeWorkspaceRequestV1>(serde_json::json!({
                "node": {"kind": "project", "project_id": id}, "extra": 1
            }))
            .is_err()
        );
        let nil = GetManagerNodeWorkspaceRequestV1 {
            node: ManagerNodeRefV1::Area {
                node_id: Uuid::nil(),
            },
        };
        assert!(nil.validate().is_err());
        assert!(
            serde_json::from_value::<AgentManagerOverviewRequestV1>(serde_json::json!({"x": 1}))
                .is_err()
        );
    }

    fn budget(model: &str, cap: u64) -> serde_json::Value {
        serde_json::json!({"provider": "Claude", "model": model, "coordinator_cap": cap})
    }

    fn crafted_overview() -> serde_json::Value {
        let pm = "11111111-1111-1111-1111-111111111111";
        let project = "22222222-2222-2222-2222-222222222222";
        let directive = |n: u8| {
            serde_json::json!({
                "id": format!("0000000{n}-0000-0000-0000-000000000000"),
                "scope": "project:x",
                "text": "Never push main.\nLong rationale follows on a second line.",
                "author": "operator",
                "created_at": "2026-10-01T00:00:00Z",
                "cleared_at": null,
            })
        };
        let pred =
            |n: u8| serde_json::json!({"session_id": format!("p{n}"), "status": "Completed"});
        serde_json::json!({
            "seat": {
                "session_id": "gm", "coordinator_budget": budget("opus", 40),
                "predecessors": [pred(1), pred(2), pred(3)],
                "predecessors_truncated": true,
            },
            "children": [{
                "node": {"kind": "project", "project_id": project},
                "seat": {
                    "session_id": pm, "project_id": project, "status": "Running",
                    "model": "opus", "context_fill_pct": 10.0,
                    "coordinator_budget": budget("opus", 40),
                    "predecessors": [pred(4), pred(5)],
                },
            }, {
                "node": {"kind": "portfolio", "node_id": "n"},
                "seat": {"session_id": "other", "status": "Running",
                         "coordinator_budget": budget("sonnet", 20)},
            }],
            "projects": [{"overview": {
                "project_id": project,
                "manager": {"session_id": pm, "status": "Running",
                            "coordinator_budget": budget("opus", 40), "scope_version": 3},
                "standing_directives": [directive(1)],
            }}],
            "standing_directives": [directive(2)],
            "grant": {"allowed_launches": [{"model": "opus"}]},
            "launches_display": ["Claude/opus/high"],
        })
    }

    #[test]
    fn compact_overview_removes_every_repeat_the_issue_names() {
        let mut value = crafted_overview();
        compact_overview(&mut value, false);
        // One digest per project child: the manager row keeps the seat.
        let seat = &value["children"][0]["seat"];
        assert_eq!(seat["session_id"], "11111111-1111-1111-1111-111111111111");
        for dropped in ["status", "model", "context_fill_pct", "coordinator_budget"] {
            assert!(seat.get(dropped).is_none(), "{dropped} repeated: {seat}");
        }
        assert_eq!(
            value["projects"][0]["overview"]["manager"]["status"],
            "Running"
        );
        // A child that is not a project keeps its only seat copy.
        assert_eq!(value["children"][1]["seat"]["status"], "Running");
        // Predecessors: a count plus the newest one.
        assert_eq!(value["seat"]["predecessor_count"], 3);
        assert_eq!(value["seat"]["newest_predecessor"]["session_id"], "p1");
        assert_eq!(value["seat"]["predecessors_truncated"], true);
        assert!(value["seat"].get("predecessors").is_none());
        assert_eq!(seat["predecessor_count"], 2);
        // Standing directives: id, scope, title, updated_at; no full text.
        for list in [
            &value["standing_directives"],
            &value["projects"][0]["overview"]["standing_directives"],
        ] {
            let entry = &list[0];
            assert_eq!(entry["title"], "Never push main.…");
            assert_eq!(entry["updated_at"], "2026-10-01T00:00:00Z");
            assert!(entry.get("text").is_none(), "{entry}");
            assert!(entry["id"].is_string() && entry["scope"].is_string());
        }
        // coordinator_budget once per distinct budget, referenced by key.
        assert_eq!(value["coordinator_budgets"].as_object().unwrap().len(), 2);
        assert_eq!(
            value["coordinator_budgets"]["Claude/opus"]["coordinator_cap"],
            40
        );
        assert_eq!(value["seat"]["coordinator_budget"], "Claude/opus");
        assert_eq!(
            value["children"][1]["seat"]["coordinator_budget"],
            "Claude/sonnet"
        );
        assert_eq!(
            value["projects"][0]["overview"]["manager"]["coordinator_budget"],
            "Claude/opus"
        );
        // One launches form: the structured choices.
        assert!(value.get("launches_display").is_none());
        assert_eq!(value["grant"]["allowed_launches"][0]["model"], "opus");
    }

    #[test]
    fn compact_overview_keeps_directive_text_on_request_and_splits_differing_budgets() {
        let mut value = crafted_overview();
        value["children"][1]["seat"]["coordinator_budget"] = budget("opus", 25);
        compact_overview(&mut value, true);
        assert!(value["standing_directives"][0]["text"].is_string());
        assert!(value["projects"][0]["overview"]["standing_directives"][0]["text"].is_string());
        // Same model, different budget: a second entry, never a wrong one.
        assert_eq!(
            value["children"][1]["seat"]["coordinator_budget"],
            "Claude/opus#2"
        );
        assert_eq!(
            value["coordinator_budgets"]["Claude/opus#2"]["coordinator_cap"],
            25
        );
    }

    #[test]
    fn overview_request_defaults_to_compact_and_refuses_unknown_fields() {
        let request: AgentManagerOverviewRequestV1 =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(request.detail, ManagerOverviewDetailV1::Compact);
        assert!(!request.directives);
        let request: AgentManagerOverviewRequestV1 =
            serde_json::from_value(serde_json::json!({"detail": "full", "directives": true}))
                .unwrap();
        assert_eq!(request.detail, ManagerOverviewDetailV1::Full);
        assert!(request.directives);
    }

    #[test]
    fn reason_is_cut_on_a_char_boundary() {
        assert_eq!(bounded_reason("abc", 8), "abc");
        assert_eq!(bounded_reason("ééé", 3), "é");
    }
}

impl Serialize for ManagerNodeWorkspaceV1 {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut output = serializer.serialize_struct("ManagerNodeWorkspaceV1", 19)?;
        output.serialize_field("node", &self.node)?;
        output.serialize_field("label", &self.label)?;
        output.serialize_field("state", &self.state)?;
        output.serialize_field("parent", &self.parent)?;
        output.serialize_field("grant", &self.grant)?;
        output.serialize_field("grantor", &self.grantor)?;
        output.serialize_field("max_direct_reports", &self.max_direct_reports)?;
        output.serialize_field("area", &self.area)?;
        output.serialize_field("seat", &self.seat)?;
        output.serialize_field("children", &self.children)?;
        output.serialize_field("children_truncated", &self.children_truncated)?;
        output.serialize_field("projects", &self.projects)?;
        output.serialize_field("missing_project_ids", &self.missing_project_ids)?;
        output.serialize_field("escalations", &self.escalations)?;
        output.serialize_field("escalations_truncated", &self.escalations_truncated)?;
        output.serialize_field("fleet", &self.fleet)?;
        output.serialize_field("last_seat_handoff", &self.last_seat_handoff)?;
        output.serialize_field("standing_directives", &self.standing_directives)?;
        let launches_display: Vec<_> = self
            .grant
            .as_ref()
            .map(|grant| grant.allowed_launches.as_slice())
            .or_else(|| {
                self.area
                    .as_ref()?
                    .grant
                    .as_ref()
                    .map(|grant| grant.allowed_launches.as_slice())
            })
            .unwrap_or(&[])
            .iter()
            .map(crate::harness_manager_v2::ManagerLaunchChoiceV2::display)
            .collect();
        output.serialize_field("launches_display", &launches_display)?;
        output.end()
    }
}
