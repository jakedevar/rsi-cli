//! #2246: one deduplicated Issue per project whose codegraph stays Degraded or
//! Failed. The daemon's health sweep calls [`Store::upsert_codegraph_health_issue`]
//! with the current notice; the Issue id is a deterministic UUIDv5 of the
//! project, so the same graph never files twice and a restart replays safely.
//!
//! Decisions:
//! - The Issue is refreshed (its body rewritten) only when the failure
//!   `signature` (phase and error) changed or it has not been touched for
//!   [`REFRESH_AFTER_HOURS`], so a graph failing every few minutes does not
//!   append an Issue event per run.
//! - A closed or cancelled Issue is left alone: closing it is the operator's
//!   or a manager's acknowledgement, and re-filing would reopen their decision.

use super::Store;
use super::issues::{IssueWriteActor, issue_write_before_commit};
use crate::error::Result;
use chrono::{DateTime, Duration, Utc};
use rsi_common::types::{IssueStatus, IssueUpdate, NewIssue};
use rusqlite::{Transaction, TransactionBehavior};
use uuid::Uuid;

/// An open Issue whose failure is unchanged is rewritten at most this often.
pub const REFRESH_AFTER_HOURS: i64 = 24;
/// Labels of the filed Issue.
pub const CODEGRAPH_HEALTH_ISSUE_LABELS: [&str; 2] = ["kaizen", "codegraph"];

/// What [`Store::upsert_codegraph_health_issue`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodegraphHealthIssueAction {
    Created,
    Refreshed,
    Unchanged,
    /// The Issue exists but is closed or cancelled; it was not touched.
    LeftClosed,
}

/// The Issue id and its display number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodegraphHealthIssue {
    pub issue_id: Uuid,
    pub display_number: i64,
    pub action: CodegraphHealthIssueAction,
}

/// The deterministic Issue id of `project_id`'s codegraph health Issue.
#[must_use]
pub fn codegraph_health_issue_id(project_id: Uuid) -> Uuid {
    Uuid::new_v5(&project_id, b"codegraph-health-issue")
}

fn signature_marker(signature: &str) -> String {
    format!("<!-- codegraph-health-signature: {signature} -->")
}

impl Store {
    /// File or refresh `project_id`'s codegraph health Issue. `signature`
    /// names the failure (phase and error class); `title` and `body` describe
    /// it now.
    ///
    /// # Errors
    /// The project does not exist, or a persistence error.
    pub fn upsert_codegraph_health_issue(
        &self,
        project_id: Uuid,
        signature: &str,
        title: &str,
        body: &str,
        now: DateTime<Utc>,
    ) -> Result<CodegraphHealthIssue> {
        let issue_id = codegraph_health_issue_id(project_id);
        let body = format!("{body}\n{}\n", signature_marker(signature));
        if let Some(existing) = self.get_issue(issue_id)? {
            let display_number = existing.display_number;
            let done = |action| CodegraphHealthIssue {
                issue_id,
                display_number,
                action,
            };
            if !matches!(existing.status, IssueStatus::Open | IssueStatus::InProgress) {
                return Ok(done(CodegraphHealthIssueAction::LeftClosed));
            }
            let same = existing.body.contains(&signature_marker(signature));
            if same && now - existing.updated_at < Duration::hours(REFRESH_AFTER_HOURS) {
                return Ok(done(CodegraphHealthIssueAction::Unchanged));
            }
            self.update_issue(
                issue_id,
                &IssueUpdate {
                    title: Some(title.to_string()),
                    body: Some(body),
                    ..IssueUpdate::default()
                },
            )?;
            return Ok(done(CodegraphHealthIssueAction::Refreshed));
        }
        let new = NewIssue {
            project_id,
            title: title.to_string(),
            body,
            priority: None,
            labels: CODEGRAPH_HEALTH_ISSUE_LABELS
                .iter()
                .map(ToString::to_string)
                .collect(),
            created_by_session_id: None,
            assignee: None,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
        };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let created = Self::idempotent_issue_in_tx(
            &tx,
            issue_id,
            &new,
            IssueWriteActor::System {
                label: "rsi:codegraph-health".to_string(),
            },
        )?;
        issue_write_before_commit()?;
        tx.commit()?;
        Ok(CodegraphHealthIssue {
            issue_id,
            display_number: created.issue.display_number,
            action: CodegraphHealthIssueAction::Created,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::types::Project;

    fn project(store: &Store) -> Uuid {
        let now = Utc::now();
        let project = Project {
            id: Uuid::new_v4(),
            name: format!("cg-health-{}", Uuid::new_v4()),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.to_string(),
            context_files: None,
            created_at: now,
            updated_at: now,
        };
        store.insert_project(&project).expect("insert project");
        project.id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn one_issue_per_project_is_filed_refreshed_and_left_closed_2246() {
        let store = Store::open_in_memory().unwrap();
        let first = project(&store);
        let other = project(&store);
        let now = Utc::now();

        let created = store
            .upsert_codegraph_health_issue(first, "failed:symlink", "graph failed", "body one", now)
            .unwrap();
        assert_eq!(created.action, CodegraphHealthIssueAction::Created);
        let issue = store.get_issue(created.issue_id).unwrap().unwrap();
        assert_eq!(issue.project_id, first);
        assert_eq!(issue.status, IssueStatus::Open);
        assert!(issue.labels.contains(&"kaizen".to_string()));
        assert!(issue.body.contains("body one"));

        // The same failure, soon after, writes nothing.
        let again = store
            .upsert_codegraph_health_issue(
                first,
                "failed:symlink",
                "graph failed",
                "body two",
                now + Duration::hours(1),
            )
            .unwrap();
        assert_eq!(again.action, CodegraphHealthIssueAction::Unchanged);
        assert_eq!(again.display_number, created.display_number);
        let issue = store.get_issue(created.issue_id).unwrap().unwrap();
        assert_eq!(issue.row_version, 1);
        assert!(issue.body.contains("body one"));

        // A different failure refreshes the body in place.
        let changed = store
            .upsert_codegraph_health_issue(
                first,
                "degraded:budget",
                "graph degraded",
                "body three",
                now + Duration::hours(2),
            )
            .unwrap();
        assert_eq!(changed.action, CodegraphHealthIssueAction::Refreshed);
        assert_eq!(changed.display_number, created.display_number);
        let issue = store.get_issue(created.issue_id).unwrap().unwrap();
        assert!(issue.body.contains("body three"));
        assert_eq!(issue.title, "graph degraded");

        // Another project gets its own Issue.
        let second = store
            .upsert_codegraph_health_issue(other, "failed:symlink", "graph failed", "b", now)
            .unwrap();
        assert_eq!(second.action, CodegraphHealthIssueAction::Created);
        assert_ne!(second.issue_id, created.issue_id);

        // A closed Issue is the operator's acknowledgement; it stays closed.
        store
            .update_issue_status(created.issue_id, IssueStatus::Closed)
            .unwrap();
        let closed = store
            .upsert_codegraph_health_issue(
                first,
                "failed:new-error",
                "graph failed",
                "body four",
                now + Duration::hours(80),
            )
            .unwrap();
        assert_eq!(closed.action, CodegraphHealthIssueAction::LeftClosed);
        assert_eq!(
            store.get_issue(created.issue_id).unwrap().unwrap().status,
            IssueStatus::Closed
        );
    }
}
