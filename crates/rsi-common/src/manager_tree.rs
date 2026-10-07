//! Recursive-manager operator tree (#890 Slice C v0): wire types of the
//! read-only `GetManagerTree` snapshot.
//!
//! The snapshot is operator-only and bounded: rows are a flat, depth-first
//! page of the visible hierarchy (portfolio nodes of any tier, project seats,
//! area nodes, Epics with leads). A count the daemon could not traverse completely
//! is `None` / `complete == false`, never zero.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::harness_manager_v2::{ManagerCapabilityV2, ManagerLaunchChoiceV2};
use crate::types::SessionStatus;

/// Most rows one page may carry.
pub const MANAGER_TREE_MAX_LIMIT: u16 = 200;
/// Most led Epics listed under one project before the project is `incomplete`.
pub const MANAGER_TREE_MAX_EPICS_PER_PROJECT: usize = 256;
/// The page cursor names no row of the current snapshot.
pub const MANAGER_TREE_STALE_CURSOR: &str = "manager_tree_stale_cursor";
/// The request is malformed.
pub const MANAGER_TREE_INVALID_REQUEST: &str = "manager_tree_invalid_request";

/// `GetManagerTree {after?, limit}`. `after` is the `key` of the last row of
/// the previous page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetManagerTreeRequestV1 {
    #[serde(default)]
    pub after: Option<String>,
    pub limit: u16,
}

impl GetManagerTreeRequestV1 {
    /// # Errors
    /// A stable refusal code.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(1..=MANAGER_TREE_MAX_LIMIT).contains(&self.limit)
            || self
                .after
                .as_deref()
                .is_some_and(|key| key.is_empty() || key.len() > 128)
        {
            return Err(MANAGER_TREE_INVALID_REQUEST);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerTreeKindV1 {
    /// The v0 global grant row. Daemons since #1236 render every manager
    /// above project level as a `Portfolio` row instead; the variant stays so
    /// a newer client reads an older daemon's snapshot.
    Global,
    /// #1236: a portfolio node of any tier (`tier_label` names it).
    Portfolio,
    Project,
    Area,
    Epic,
}

/// Live state of the session that holds a node's seat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagerTreeSeatV1 {
    pub session_id: Uuid,
    pub status: SessionStatus,
    pub model: Option<String>,
    pub context_fill_pct: Option<f64>,
    pub updated_at: DateTime<Utc>,
}

/// Grant and allowance of an area node (None on rows without a node grant).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagerTreeGrantV1 {
    pub grant_version: i64,
    pub capabilities: Vec<ManagerCapabilityV2>,
    pub max_active_sessions: u16,
    pub max_created_sessions: u16,
    pub max_created_containers: u16,
    pub max_direct_reports: u16,
    pub max_spend_usd: Option<f64>,
    /// Active reservations this node holds for its children, by resource kind.
    pub reserved: Vec<ManagerTreeReservedV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerTreeReservedV1 {
    pub resource_kind: String,
    pub amount: i64,
}

/// Per-node load. `None` means the daemon could not count it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerTreeLoadV1 {
    pub running_workers: Option<i64>,
    pub direct_reports: Option<i64>,
    pub pending_escalations: Option<i64>,
    pub pending_decisions: Option<i64>,
}

/// One row of the flat depth-first tree page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagerTreeRowV1 {
    /// Stable per-row key: `portfolio:<id>` (`global` from a pre-#1236
    /// daemon), `project:<id>`, `area:<id>`, `epic:<id>`.
    pub key: String,
    pub parent_key: Option<String>,
    pub depth: u16,
    pub kind: ManagerTreeKindV1,
    pub label: String,
    /// #1236: a portfolio row's display tier ("global", "pinnacle", ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_label: Option<String>,
    /// #1239: who granted this row's seat: `operator` or `node:<uuid>` (a
    /// portfolio node, or the PM a node appointed). `None` for rows without
    /// a seat grant (an unappointed project, area and Epic rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grantor: Option<String>,
    pub project_id: Option<Uuid>,
    pub node_id: Option<Uuid>,
    pub epic_id: Option<Uuid>,
    /// Human summary of the selected scope (`project`, `2 groups, 1 epic`, ...).
    pub scope: Option<String>,
    pub seat: Option<ManagerTreeSeatV1>,
    /// The session Enter jumps to (the seat, or an Epic's lead).
    pub focus_session_id: Option<Uuid>,
    pub grant: Option<ManagerTreeGrantV1>,
    /// #1412: the launches this row's manager may make now: a node's grant
    /// list narrowed by its project policy, a project manager's own list (or
    /// "any" when empty) intersected live with every node above it. Empty for
    /// rows that launch nothing (Epic) or whose set is unrestricted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub launches: Vec<ManagerLaunchChoiceV2>,
    pub load: ManagerTreeLoadV1,
    /// False when this row's children could not be listed completely.
    pub complete: bool,
}

/// `GetManagerTree` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GetManagerTreeResultV1 {
    pub rows: Vec<ManagerTreeRowV1>,
    /// `key` of the last row of this page when more rows follow.
    pub next_after: Option<String>,
    /// Exact row count of the whole snapshot (never the page size).
    pub total_rows: u64,
    /// False when any row's traversal is incomplete: the total is then a
    /// lower bound and the operator view says so.
    pub complete: bool,
    /// The grant version of the single active root labelled `global`, when
    /// exactly one exists (the `*GlobalManager` shims' node).
    pub global_grant_version: Option<i64>,
}
