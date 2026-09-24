//! Pure functions for issue dispatch eligibility evaluation.

use super::types::{DispatchRecord, IssueTrackerConfig, TrackedIssue};
use std::collections::{HashMap, HashSet};

/// Check if an issue is eligible for dispatch.
/// Returns None if eligible, Some(reason) if ineligible.
pub fn check_eligibility(
    issue: &TrackedIssue,
    config: &IssueTrackerConfig,
    claimed: &HashSet<String>,
    running: &HashMap<String, DispatchRecord>,
    running_count: usize,
) -> Option<String> {
    // 1. Required fields present
    if issue.id.is_empty() || issue.identifier.is_empty() || issue.title.is_empty() {
        return Some("missing required fields".to_string());
    }

    // 2. State type in active_states
    if !config.active_states.contains(&issue.state.state_type) {
        return Some(format!(
            "state '{}' not in active states",
            issue.state.state_type
        ));
    }

    // 3. State type NOT in terminal states
    if issue.state.state_type == "completed" || issue.state.state_type == "cancelled" {
        return Some("terminal state".to_string());
    }

    // 4. Not in claimed set
    if claimed.contains(&issue.id) {
        return Some("already claimed".to_string());
    }

    // 5. Not in running map
    if running.contains_key(&issue.id) {
        return Some("already running".to_string());
    }

    // 6. Running count under max concurrent
    if running_count >= config.max_concurrent {
        return Some("concurrency limit".to_string());
    }

    // 7. Blocker gate: if state.type == "unstarted", all blocked_by must be terminal
    if issue.state.state_type == "unstarted" && !issue.blocked_by.is_empty() {
        let all_terminal = issue
            .blocked_by
            .iter()
            .all(|b| b.state_type == "completed" || b.state_type == "cancelled");
        if !all_terminal {
            return Some("blocked".to_string());
        }
    }

    None
}

/// Sort issues by dispatch priority.
/// Priority 1-4 ascending (None sorts last), then oldest first, then identifier tiebreak.
pub fn sort_by_priority(issues: &mut [TrackedIssue]) {
    issues.sort_by(|a, b| {
        // Priority: lower number = higher priority, None sorts last (u8::MAX)
        let pa = a.priority.unwrap_or(u8::MAX);
        let pb = b.priority.unwrap_or(u8::MAX);
        pa.cmp(&pb)
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| a.identifier.cmp(&b.identifier))
    });
}

#[cfg(test)]
mod tests {
    use super::super::types::{BlockerRef, IssueState};
    use super::*;
    use chrono::Utc;

    fn make_config() -> IssueTrackerConfig {
        IssueTrackerConfig {
            max_concurrent: 2,
            active_states: vec!["started".to_string(), "unstarted".to_string()],
            ..Default::default()
        }
    }

    fn make_issue(id: &str, state_type: &str) -> TrackedIssue {
        TrackedIssue {
            id: id.to_string(),
            identifier: format!("ENG-{}", id),
            title: format!("Issue {}", id),
            description: None,
            priority: Some(3),
            state: IssueState {
                id: "state-1".to_string(),
                name: "In Progress".to_string(),
                state_type: state_type.to_string(),
            },
            branch_name: None,
            url: format!("https://linear.app/team/ENG-{}", id),
            labels: vec![],
            blocked_by: vec![],
            assignee_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn eligible_active_state_no_blockers() {
        let config = make_config();
        let issue = make_issue("1", "started");
        let result = check_eligibility(&issue, &config, &HashSet::new(), &HashMap::new(), 0);
        assert!(result.is_none(), "Expected eligible, got: {:?}", result);
    }

    #[test]
    fn ineligible_terminal_state() {
        let config = make_config();
        let mut issue = make_issue("1", "completed");
        // completed is terminal even if we add it to active_states
        issue.state.state_type = "completed".to_string();
        let mut config2 = config.clone();
        config2.active_states.push("completed".to_string());
        let result = check_eligibility(&issue, &config2, &HashSet::new(), &HashMap::new(), 0);
        assert_eq!(result, Some("terminal state".to_string()));
    }

    #[test]
    fn ineligible_already_claimed() {
        let config = make_config();
        let issue = make_issue("1", "started");
        let mut claimed = HashSet::new();
        claimed.insert("1".to_string());
        let result = check_eligibility(&issue, &config, &claimed, &HashMap::new(), 0);
        assert_eq!(result, Some("already claimed".to_string()));
    }

    #[test]
    fn ineligible_concurrency_limit() {
        let config = make_config();
        let issue = make_issue("1", "started");
        let result = check_eligibility(&issue, &config, &HashSet::new(), &HashMap::new(), 2);
        assert_eq!(result, Some("concurrency limit".to_string()));
    }

    #[test]
    fn ineligible_blocked_non_terminal_blocker() {
        let config = make_config();
        let mut issue = make_issue("1", "unstarted");
        issue.blocked_by = vec![BlockerRef {
            id: "blocker-1".to_string(),
            state_type: "started".to_string(),
        }];
        let result = check_eligibility(&issue, &config, &HashSet::new(), &HashMap::new(), 0);
        assert_eq!(result, Some("blocked".to_string()));
    }

    #[test]
    fn eligible_blocked_all_terminal() {
        let config = make_config();
        let mut issue = make_issue("1", "unstarted");
        issue.blocked_by = vec![
            BlockerRef {
                id: "blocker-1".to_string(),
                state_type: "completed".to_string(),
            },
            BlockerRef {
                id: "blocker-2".to_string(),
                state_type: "cancelled".to_string(),
            },
        ];
        let result = check_eligibility(&issue, &config, &HashSet::new(), &HashMap::new(), 0);
        assert!(result.is_none(), "Expected eligible, got: {:?}", result);
    }

    #[test]
    fn sort_priority_lower_first() {
        let mut issues = vec![
            {
                let mut i = make_issue("1", "started");
                i.priority = Some(4);
                i
            },
            {
                let mut i = make_issue("2", "started");
                i.priority = Some(1);
                i
            },
        ];
        sort_by_priority(&mut issues);
        assert_eq!(issues[0].priority, Some(1));
        assert_eq!(issues[1].priority, Some(4));
    }

    #[test]
    fn sort_none_priority_last() {
        let mut issues = vec![
            {
                let mut i = make_issue("1", "started");
                i.priority = None;
                i
            },
            {
                let mut i = make_issue("2", "started");
                i.priority = Some(3);
                i
            },
        ];
        sort_by_priority(&mut issues);
        assert_eq!(issues[0].priority, Some(3));
        assert!(issues[1].priority.is_none());
    }

    #[test]
    fn sort_same_priority_older_first() {
        let now = Utc::now();
        let earlier = now - chrono::Duration::hours(1);
        let mut issues = vec![
            {
                let mut i = make_issue("new", "started");
                i.priority = Some(2);
                i.created_at = now;
                i
            },
            {
                let mut i = make_issue("old", "started");
                i.priority = Some(2);
                i.created_at = earlier;
                i
            },
        ];
        sort_by_priority(&mut issues);
        assert_eq!(issues[0].id, "old");
        assert_eq!(issues[1].id, "new");
    }
}
