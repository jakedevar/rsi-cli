pub use crate::store_support::issue_tracker::{
    BlockerRef, DispatchRecord, IssueState, IssueTrackerConfig, IssueTrackerStatus, TickResult,
    TrackedIssue,
};

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn issue_tracker_config_default_correctness() {
        let config = IssueTrackerConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.kind, "linear");
        assert_eq!(config.poll_interval_ms, 30_000);
        assert_eq!(config.max_concurrent, 5);
        assert_eq!(config.active_states, vec!["started", "unstarted"]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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
