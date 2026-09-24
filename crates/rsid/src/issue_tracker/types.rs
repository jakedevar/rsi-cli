use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracked_issue_serde_roundtrip() {
        let issue = TrackedIssue {
            id: "abc-123".to_string(),
            identifier: "ENG-42".to_string(),
            title: "Fix auth bug".to_string(),
            description: Some("Auth is broken".to_string()),
            priority: Some(2),
            state: IssueState {
                id: "state-1".to_string(),
                name: "In Progress".to_string(),
                state_type: "started".to_string(),
            },
            branch_name: Some("fix/auth-bug".to_string()),
            url: "https://linear.app/team/ENG-42".to_string(),
            labels: vec!["bug".to_string(), "auth".to_string()],
            blocked_by: vec![BlockerRef {
                id: "other-issue".to_string(),
                state_type: "started".to_string(),
            }],
            assignee_id: Some("user-1".to_string()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let json = serde_json::to_string(&issue).unwrap();
        let deser: TrackedIssue = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.identifier, "ENG-42");
        assert_eq!(deser.labels.len(), 2);
        assert_eq!(deser.blocked_by.len(), 1);
    }

    #[test]
    fn issue_tracker_config_default_correctness() {
        let config = IssueTrackerConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.kind, "linear");
        assert_eq!(config.poll_interval_ms, 30_000);
        assert_eq!(config.max_concurrent, 5);
        assert_eq!(config.active_states, vec!["started", "unstarted"]);
    }

    #[test]
    fn dispatch_record_serde_roundtrip() {
        let record = DispatchRecord {
            issue_id: "issue-1".to_string(),
            issue_identifier: "ENG-42".to_string(),
            tracker: "linear".to_string(),
            session_id: uuid::Uuid::new_v4(),
            dispatched_at: Utc::now(),
            last_reconciled_at: None,
            terminal_state: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        let deser: DispatchRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.issue_identifier, "ENG-42");
        assert_eq!(deser.tracker, "linear");
    }

    #[test]
    fn tick_result_serde_roundtrip() {
        let result = TickResult {
            issues_found: 5,
            dispatched: 2,
            skipped_claimed: 1,
            skipped_blocked: 2,
            errors: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        let deser: TickResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.dispatched, 2);
    }
}
