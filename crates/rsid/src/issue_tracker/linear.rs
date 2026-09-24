//! Linear GraphQL API client — implements the Tracker trait for Linear.

use super::tracker::Tracker;
use super::types::{BlockerRef, IssueState, IssueTrackerConfig, TrackedIssue};
use crate::error::{DaemonError, Result};
use async_trait::async_trait;
use serde::Deserialize;

const LINEAR_API_URL: &str = "https://api.linear.app/graphql";

/// GraphQL query for fetching candidate issues with pagination.
const FETCH_CANDIDATES_QUERY: &str = r#"
query FetchCandidates($teamId: String!, $stateTypes: [String!]!, $cursor: String) {
  issues(
    filter: {
      team: { id: { eq: $teamId } }
      state: { type: { in: $stateTypes } }
    }
    first: 50
    after: $cursor
  ) {
    pageInfo {
      hasNextPage
      endCursor
    }
    nodes {
      id
      identifier
      title
      description
      priority
      state { id name type }
      branchName
      url
      labels { nodes { name } }
      relations {
        nodes {
          type
          relatedIssue { id state { type } }
        }
      }
      assignee { id }
      createdAt
      updatedAt
    }
  }
}
"#;

/// GraphQL query for fetching issues by ID list (reconciliation).
const FETCH_BY_IDS_QUERY: &str = r#"
query FetchByIds($ids: [ID!]!) {
  issues(filter: { id: { in: $ids } }) {
    nodes {
      id
      identifier
      title
      description
      priority
      state { id name type }
      branchName
      url
      labels { nodes { name } }
      relations {
        nodes {
          type
          relatedIssue { id state { type } }
        }
      }
      assignee { id }
      createdAt
      updatedAt
    }
  }
}
"#;

/// GraphQL mutation for updating issue state.
const UPDATE_STATE_MUTATION: &str = r#"
mutation UpdateIssueState($issueId: String!, $stateId: String!) {
  issueUpdate(id: $issueId, input: { stateId: $stateId }) {
    success
  }
}
"#;

/// GraphQL query for resolving the viewer's user ID.
const VIEWER_QUERY: &str = r#"
query Viewer {
  viewer { id }
}
"#;

/// GraphQL query for finding a workflow state by name and team.
const FIND_STATE_QUERY: &str = r#"
query FindState($teamId: String!, $stateName: String!) {
  workflowStates(filter: { team: { id: { eq: $teamId } }, name: { eq: $stateName } }) {
    nodes { id name type }
  }
}
"#;

/// Linear-specific Tracker implementation.
pub struct LinearClient {
    http: reqwest::Client,
}

impl LinearClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    /// Execute a GraphQL request against the Linear API with retry on rate limit.
    async fn graphql(&self, api_key: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let mut attempts = 0u32;
        loop {
            let response = self
                .http
                .post(LINEAR_API_URL)
                .header("Authorization", api_key)
                .header("Content-Type", "application/json")
                .json(body)
                .send()
                .await
                .map_err(|e| DaemonError::Process(format!("Linear API request failed: {}", e)))?;

            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                attempts += 1;
                if attempts > 3 {
                    return Err(DaemonError::Process(
                        "Linear API rate limited after 3 retries".to_string(),
                    ));
                }
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(2u64.pow(attempts));
                tracing::warn!(
                    retry_after_secs = retry_after,
                    attempt = attempts,
                    "Linear API rate limited, retrying"
                );
                tokio::time::sleep(std::time::Duration::from_secs(retry_after)).await;
                continue;
            }

            let status = response.status();
            let body_text = response
                .text()
                .await
                .map_err(|e| DaemonError::Process(format!("Failed to read response: {}", e)))?;

            if !status.is_success() {
                return Err(DaemonError::Process(format!(
                    "Linear API error ({}): {}",
                    status,
                    &body_text[..body_text.len().min(500)]
                )));
            }

            let json: serde_json::Value = serde_json::from_str(&body_text)
                .map_err(|e| DaemonError::Process(format!("Linear API JSON parse error: {}", e)))?;

            if let Some(errors) = json.get("errors") {
                return Err(DaemonError::Process(format!(
                    "Linear GraphQL errors: {}",
                    errors
                )));
            }

            return Ok(json);
        }
    }

    /// Normalize a GraphQL issue node into a TrackedIssue.
    fn normalize_issue(node: &serde_json::Value) -> Option<TrackedIssue> {
        let id = node.get("id")?.as_str()?.to_string();
        let identifier = node.get("identifier")?.as_str()?.to_string();
        let title = node.get("title")?.as_str()?.to_string();
        let description = node
            .get("description")
            .and_then(|v| v.as_str())
            .map(String::from);
        let priority = node
            .get("priority")
            .and_then(|v| v.as_u64())
            .map(|v| v as u8);

        let state_obj = node.get("state")?;
        let state = IssueState {
            id: state_obj.get("id")?.as_str()?.to_string(),
            name: state_obj.get("name")?.as_str()?.to_string(),
            state_type: state_obj.get("type")?.as_str()?.to_string(),
        };

        let branch_name = node
            .get("branchName")
            .and_then(|v| v.as_str())
            .map(String::from);
        let url = node.get("url")?.as_str()?.to_string();

        let labels = node
            .get("labels")
            .and_then(|l| l.get("nodes"))
            .and_then(|n| n.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let blocked_by = node
            .get("relations")
            .and_then(|r| r.get("nodes"))
            .and_then(|n| n.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|rel| {
                        let rel_type = rel.get("type")?.as_str()?;
                        // "blocks" relation type means the related issue blocks this one
                        if rel_type == "blocks" {
                            let related = rel.get("relatedIssue")?;
                            Some(BlockerRef {
                                id: related.get("id")?.as_str()?.to_string(),
                                state_type: related
                                    .get("state")
                                    .and_then(|s| s.get("type"))
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("unknown")
                                    .to_string(),
                            })
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let assignee_id = node
            .get("assignee")
            .and_then(|a| a.get("id"))
            .and_then(|id| id.as_str())
            .map(String::from);

        let created_at = node
            .get("createdAt")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now);

        let updated_at = node
            .get("updatedAt")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now);

        Some(TrackedIssue {
            id,
            identifier,
            title,
            description,
            priority,
            state,
            branch_name,
            url,
            labels,
            blocked_by,
            assignee_id,
            created_at,
            updated_at,
        })
    }
}

#[async_trait]
impl Tracker for LinearClient {
    async fn fetch_candidates(&self, config: &IssueTrackerConfig) -> Result<Vec<TrackedIssue>> {
        let mut all_issues = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let variables = serde_json::json!({
                "teamId": config.team_id,
                "stateTypes": config.active_states,
                "cursor": cursor,
            });

            let body = serde_json::json!({
                "query": FETCH_CANDIDATES_QUERY,
                "variables": variables,
            });

            let response = self.graphql(&config.api_key, &body).await?;

            let issues_data = response.get("data").and_then(|d| d.get("issues"));

            let Some(issues_obj) = issues_data else {
                break;
            };

            if let Some(nodes) = issues_obj.get("nodes").and_then(|n| n.as_array()) {
                for node in nodes {
                    if let Some(issue) = Self::normalize_issue(node) {
                        // Apply assignee filter if configured
                        if let Some(ref assignee_filter) = config.assignee {
                            if issue.assignee_id.as_deref() != Some(assignee_filter) {
                                continue;
                            }
                        }
                        all_issues.push(issue);
                    }
                }
            }

            // Pagination
            let page_info = issues_obj.get("pageInfo");
            let has_next = page_info
                .and_then(|p| p.get("hasNextPage"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if has_next {
                cursor = page_info
                    .and_then(|p| p.get("endCursor"))
                    .and_then(|v| v.as_str())
                    .map(String::from);
            } else {
                break;
            }
        }

        Ok(all_issues)
    }

    async fn fetch_by_ids(
        &self,
        config: &IssueTrackerConfig,
        ids: &[String],
    ) -> Result<Vec<TrackedIssue>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let body = serde_json::json!({
            "query": FETCH_BY_IDS_QUERY,
            "variables": { "ids": ids },
        });

        let response = self.graphql(&config.api_key, &body).await?;

        let issues = response
            .get("data")
            .and_then(|d| d.get("issues"))
            .and_then(|i| i.get("nodes"))
            .and_then(|n| n.as_array())
            .map(|arr| arr.iter().filter_map(Self::normalize_issue).collect())
            .unwrap_or_default();

        Ok(issues)
    }

    async fn update_issue_state(
        &self,
        config: &IssueTrackerConfig,
        issue_id: &str,
        state_name: &str,
    ) -> Result<()> {
        // First, resolve the state name to a state ID
        let find_body = serde_json::json!({
            "query": FIND_STATE_QUERY,
            "variables": {
                "teamId": config.team_id,
                "stateName": state_name,
            },
        });

        let find_response = self.graphql(&config.api_key, &find_body).await?;
        let state_id = find_response
            .get("data")
            .and_then(|d| d.get("workflowStates"))
            .and_then(|w| w.get("nodes"))
            .and_then(|n| n.as_array())
            .and_then(|arr| arr.first())
            .and_then(|s| s.get("id"))
            .and_then(|id| id.as_str())
            .ok_or_else(|| {
                DaemonError::Process(format!(
                    "Could not find Linear workflow state '{}'",
                    state_name
                ))
            })?;

        // Now update the issue
        let update_body = serde_json::json!({
            "query": UPDATE_STATE_MUTATION,
            "variables": {
                "issueId": issue_id,
                "stateId": state_id,
            },
        });

        self.graphql(&config.api_key, &update_body).await?;
        Ok(())
    }

    async fn resolve_viewer_id(&self, config: &IssueTrackerConfig) -> Result<String> {
        let body = serde_json::json!({
            "query": VIEWER_QUERY,
        });

        let response = self.graphql(&config.api_key, &body).await?;
        response
            .get("data")
            .and_then(|d| d.get("viewer"))
            .and_then(|v| v.get("id"))
            .and_then(|id| id.as_str())
            .map(String::from)
            .ok_or_else(|| DaemonError::Process("Could not resolve Linear viewer ID".to_string()))
    }
}

/// Intermediate types for GraphQL response deserialization (used in tests).
#[derive(Debug, Deserialize)]
struct _GraphqlResponse {
    data: Option<serde_json::Value>,
    errors: Option<Vec<serde_json::Value>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_issue_node() -> serde_json::Value {
        serde_json::json!({
            "id": "issue-uuid-1",
            "identifier": "ENG-42",
            "title": "Fix auth bug",
            "description": "Auth tokens expire too quickly",
            "priority": 2,
            "state": { "id": "state-1", "name": "In Progress", "type": "started" },
            "branchName": "fix/auth-bug",
            "url": "https://linear.app/team/ENG-42",
            "labels": { "nodes": [{ "name": "bug" }, { "name": "auth" }] },
            "relations": { "nodes": [
                {
                    "type": "blocks",
                    "relatedIssue": { "id": "blocker-1", "state": { "type": "completed" } }
                }
            ]},
            "assignee": { "id": "user-1" },
            "createdAt": "2026-03-28T10:00:00.000Z",
            "updatedAt": "2026-03-28T12:00:00.000Z"
        })
    }

    #[test]
    fn normalize_issue_full() {
        let node = sample_issue_node();
        let issue = LinearClient::normalize_issue(&node).unwrap();
        assert_eq!(issue.identifier, "ENG-42");
        assert_eq!(issue.title, "Fix auth bug");
        assert_eq!(issue.priority, Some(2));
        assert_eq!(issue.state.state_type, "started");
        assert_eq!(issue.labels, vec!["bug", "auth"]);
        assert_eq!(issue.blocked_by.len(), 1);
        assert_eq!(issue.blocked_by[0].state_type, "completed");
        assert_eq!(issue.assignee_id, Some("user-1".to_string()));
    }

    #[test]
    fn normalize_issue_nil_description() {
        let mut node = sample_issue_node();
        node["description"] = serde_json::Value::Null;
        let issue = LinearClient::normalize_issue(&node).unwrap();
        assert!(issue.description.is_none());
    }

    #[test]
    fn normalize_issue_nil_priority() {
        let mut node = sample_issue_node();
        node["priority"] = serde_json::Value::Null;
        let issue = LinearClient::normalize_issue(&node).unwrap();
        assert!(issue.priority.is_none());
    }

    #[test]
    fn normalize_issue_empty_labels() {
        let mut node = sample_issue_node();
        node["labels"]["nodes"] = serde_json::json!([]);
        let issue = LinearClient::normalize_issue(&node).unwrap();
        assert!(issue.labels.is_empty());
    }

    #[test]
    fn normalize_issue_no_relations() {
        let mut node = sample_issue_node();
        node["relations"]["nodes"] = serde_json::json!([]);
        let issue = LinearClient::normalize_issue(&node).unwrap();
        assert!(issue.blocked_by.is_empty());
    }

    #[test]
    fn normalize_issue_no_assignee() {
        let mut node = sample_issue_node();
        node["assignee"] = serde_json::Value::Null;
        let issue = LinearClient::normalize_issue(&node).unwrap();
        assert!(issue.assignee_id.is_none());
    }

    #[test]
    fn blocked_by_only_blocks_relation_type() {
        let node = serde_json::json!({
            "id": "issue-2",
            "identifier": "ENG-43",
            "title": "Test",
            "description": null,
            "priority": null,
            "state": { "id": "s1", "name": "Todo", "type": "unstarted" },
            "branchName": null,
            "url": "https://linear.app/team/ENG-43",
            "labels": { "nodes": [] },
            "relations": { "nodes": [
                {
                    "type": "related",
                    "relatedIssue": { "id": "other-1", "state": { "type": "started" } }
                },
                {
                    "type": "blocks",
                    "relatedIssue": { "id": "blocker-1", "state": { "type": "started" } }
                }
            ]},
            "assignee": null,
            "createdAt": "2026-03-28T10:00:00.000Z",
            "updatedAt": "2026-03-28T12:00:00.000Z"
        });
        let issue = LinearClient::normalize_issue(&node).unwrap();
        // Only "blocks" relations should be captured, not "related"
        assert_eq!(issue.blocked_by.len(), 1);
        assert_eq!(issue.blocked_by[0].id, "blocker-1");
    }
}
