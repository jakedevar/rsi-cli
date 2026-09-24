use super::types::{IssueTrackerConfig, TrackedIssue};
use crate::error::Result;
use async_trait::async_trait;

/// Backend-agnostic issue tracker interface.
#[async_trait]
pub trait Tracker: Send + Sync {
    /// Fetch all candidate issues (paginated internally).
    async fn fetch_candidates(&self, config: &IssueTrackerConfig) -> Result<Vec<TrackedIssue>>;

    /// Fetch specific issues by ID (for reconciliation).
    async fn fetch_by_ids(
        &self,
        config: &IssueTrackerConfig,
        ids: &[String],
    ) -> Result<Vec<TrackedIssue>>;

    /// Update an issue's state (for post-completion webhook).
    async fn update_issue_state(
        &self,
        config: &IssueTrackerConfig,
        issue_id: &str,
        state_name: &str,
    ) -> Result<()>;

    /// Resolve "me" assignee to the API viewer's user ID.
    async fn resolve_viewer_id(&self, config: &IssueTrackerConfig) -> Result<String>;
}
