//! Issue-tracker data types the store and config persist (moved down from
//! `issue_tracker` so they sit below it; `issue_tracker::{types,poller}` re-export them).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use rsi_common::issue_workspace::{
    IssueDispatchRecordV1 as DispatchRecord, IssueTrackerStatusV1 as IssueTrackerStatus,
    IssueTrackerTickResultV1 as TickResult,
};

/// Normalized issue from any tracker backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedIssue {
    /// Tracker-internal UUID (e.g., Linear issue UUID).
    pub id: String,
    /// Human-readable identifier (e.g., "ENG-42").
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    /// Priority: 1=urgent, 2=high, 3=medium, 4=low. None=unset.
    pub priority: Option<u8>,
    pub state: IssueState,
    pub branch_name: Option<String>,
    pub url: String,
    pub labels: Vec<String>,
    /// Issues blocking this one (id + state_type for terminal checking).
    pub blocked_by: Vec<BlockerRef>,
    pub assignee_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueState {
    pub id: String,
    pub name: String,
    /// Linear state type: "started", "unstarted", "completed", "cancelled", "triage".
    pub state_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockerRef {
    pub id: String,
    pub state_type: String,
}

/// Configuration for the issue tracker subsystem.
#[derive(Debug, Clone)]
pub struct IssueTrackerConfig {
    pub enabled: bool,
    pub kind: String, // "linear"
    pub api_key: String,
    pub team_id: String,
    /// Optional assignee filter. "me" resolves to viewer ID.
    pub assignee: Option<String>,
    pub working_dir: std::path::PathBuf,
    pub project_id: Option<uuid::Uuid>,
    pub provider: rsi_common::types::SessionProvider,
    pub model: Option<String>,
    pub poll_interval_ms: u64,
    pub active_states: Vec<String>,
    pub max_concurrent: usize,
    pub max_retries: u8,
    pub stall_timeout_ms: u64,
    pub max_turns: Option<u32>,
    /// Optional post-completion state transition (e.g., "In Review").
    pub completion_state: Option<String>,
}

impl Default for IssueTrackerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            kind: "linear".to_string(),
            api_key: String::new(),
            team_id: String::new(),
            assignee: None,
            working_dir: std::path::PathBuf::from("/tmp"),
            project_id: None,
            provider: rsi_common::types::SessionProvider::Claude,
            model: None,
            poll_interval_ms: 30_000,
            active_states: vec!["started".to_string(), "unstarted".to_string()],
            max_concurrent: 5,
            max_retries: 3,
            stall_timeout_ms: 300_000,
            max_turns: None,
            completion_state: None,
        }
    }
}

/// Typed give-up fact for a terminal-watch delivery the wake tip never
/// consumed (issue #648). `reason()` renders the historical log/warning text
/// byte-for-byte so the non-manager path is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnconsumedDelivery {
    /// Rotation-lineage tip the delivery was addressed to.
    pub tip: Uuid,
    /// Watched child whose terminal row anchors the give-up clock.
    pub watched: Uuid,
    /// Whole minutes between the watched child's terminal row and give-up.
    pub minutes: i64,
    /// Typed code and count of the last refused delivery since the last
    /// accepted one (#653). `None` keeps the historical wording.
    pub last_refusal: Option<(String, u32)>,
}

impl UnconsumedDelivery {
    #[must_use]
    pub fn reason(&self) -> String {
        let Self {
            tip,
            watched,
            minutes,
            last_refusal,
        } = self;
        if let Some((code, count)) = last_refusal {
            return format!(
                "delivery to {tip} was refused: continuation_refused:{code} \
                 ({count} refusals) over {minutes} min of re-delivery attempts for \
                 watched child {watched}. The notification is being dropped; resume \
                 {tip} manually to pick the work back up"
            );
        }
        format!(
            "delivery to {tip} was never consumed: no provider output after \
             {minutes} min of re-delivery attempts for watched child {watched}. The \
             notification is being dropped; resume {tip} manually to pick the \
             work back up"
        )
    }
}
