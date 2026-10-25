//! #3316 (slice B of #3314): the operator-only, bounded global dashboard
//! snapshot returned by `GetGlobalDashboard {window}`.
//!
//! Contract: `docs/agents/rsid-rpc-contracts.md` ("Global dashboard").
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The window of a dashboard read. Wire form: `"1h"`, `"24h"`, `"7d"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DashboardWindow {
    #[serde(rename = "1h")]
    Hour,
    #[default]
    #[serde(rename = "24h")]
    Day,
    #[serde(rename = "7d")]
    Week,
}

impl DashboardWindow {
    pub fn seconds(self) -> i64 {
        match self {
            Self::Hour => 3600,
            Self::Day => 86400,
            Self::Week => 7 * 86400,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Hour => "1h",
            Self::Day => "24h",
            Self::Week => "7d",
        }
    }
}

/// Params of `GetGlobalDashboard`; `window` defaults to `"24h"`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetGlobalDashboardParams {
    #[serde(default)]
    pub window: DashboardWindow,
}

/// Token and cost sums over the window, from `model_invocations`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DashboardUsage {
    pub invocations: u64,
    pub errors: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub reasoning_tokens: u64,
    pub estimated_cost_usd: f64,
}

impl DashboardUsage {
    pub fn add(&mut self, other: &Self) {
        self.invocations += other.invocations;
        self.errors += other.errors;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_creation_tokens += other.cache_creation_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
        self.estimated_cost_usd += other.estimated_cost_usd;
    }
}

/// Usage of one provider and model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DashboardModelUsage {
    pub provider: String,
    pub model: String,
    pub usage: DashboardUsage,
}

/// Live leaf sessions of one provider, model and effort (empty effort: unset).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardAgentGroup {
    pub provider: String,
    pub model: String,
    pub effort: Option<String>,
    pub count: u64,
}

/// Live (Starting, Running, WaitingApproval) leaf sessions; containers never
/// count. Roles: `managers` (a project manager or portfolio seat), `leads`
/// (the lead of its parent Epic), `reviewers` (`agent_role` contains
/// "review"), `workers` (any other session with a parent) and `other`
/// (parentless sessions).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardAgents {
    pub total: u64,
    pub managers: u64,
    pub leads: u64,
    pub workers: u64,
    pub reviewers: u64,
    pub other: u64,
    pub by_provider: Vec<DashboardProviderCount>,
    pub by_model: Vec<DashboardAgentGroup>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardProviderCount {
    pub provider: String,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardPmSeat {
    pub session_id: Uuid,
    pub status: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardIssues {
    /// Open now, not archived.
    pub open: u64,
    /// In progress now, not archived.
    pub in_progress: u64,
    pub created_in_window: u64,
    /// Status `Closed` with `closed_at` in the window.
    pub closed_in_window: u64,
    /// Open or in progress, not archived, labelled `operator-request`.
    pub open_operator_requests: u64,
}

impl DashboardIssues {
    pub fn add(&mut self, other: &Self) {
        self.open += other.open;
        self.in_progress += other.in_progress;
        self.created_in_window += other.created_in_window;
        self.closed_in_window += other.closed_in_window;
        self.open_operator_requests += other.open_operator_requests;
    }
}

/// Merge-queue entries that finished in the window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardLandings {
    pub published: u64,
    pub refused: u64,
}

impl DashboardLandings {
    pub fn add(&mut self, other: &Self) {
        self.published += other.published;
        self.refused += other.refused;
    }
}

/// Rolling health; present only when the project has a full-library green or
/// an open `qa` Issue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardRollingHealth {
    pub qa_green_sha: Option<String>,
    pub qa_green_age_secs: Option<i64>,
    pub open_qa_issues: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectDashboard {
    pub id: Uuid,
    pub name: String,
    pub path: Option<String>,
    pub is_home: bool,
    pub agents: DashboardAgents,
    pub pm_seat: Option<DashboardPmSeat>,
    pub usage: DashboardUsage,
    pub usage_by_model: Vec<DashboardModelUsage>,
    pub issues: DashboardIssues,
    pub landings: DashboardLandings,
    pub rolling: Option<DashboardRollingHealth>,
}

/// The totals row. Agents and usage cover every session and invocation,
/// including those of no project; Issues and landings sum the listed projects.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DashboardTotals {
    pub agents: DashboardAgents,
    pub usage: DashboardUsage,
    pub usage_by_model: Vec<DashboardModelUsage>,
    pub issues: DashboardIssues,
    pub landings: DashboardLandings,
    /// True when any bounded read hit its cap: the sums are a lower bound.
    pub incomplete: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlobalDashboard {
    pub as_of: DateTime<Utc>,
    /// `"1h"`, `"24h"` or `"7d"`.
    pub window: String,
    pub window_seconds: i64,
    /// The home project first when it exists, then by name.
    pub projects: Vec<ProjectDashboard>,
    pub totals: DashboardTotals,
    /// The projects read hit its cap; `projects` is not every project.
    pub projects_incomplete: bool,
    /// Which bounded reads were capped (`projects`, `sessions`, `invocations`,
    /// `issues:<project id>`, `landings`); empty when the snapshot is complete.
    pub incomplete_reasons: Vec<String>,
}
