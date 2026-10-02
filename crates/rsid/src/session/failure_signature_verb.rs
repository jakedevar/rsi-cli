//! `AgentQueryFailureSignatures` (#1016 acceptance 3): every leaf session may
//! ask whether a red is already known. The read is scoped to the caller's own
//! project and returns typed signature records only, never an Issue body.
//! Records of a closed, cancelled or archived owner Issue are not returned, so
//! expiry (acceptance 4) is live.

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};
use rsi_common::agent_failure_signatures::{
    AgentQueryFailureSignaturesRequestV1, AgentQueryFailureSignaturesResultV1,
    FAILURE_SIGNATURE_QUERY_INVALID,
};
use rsi_common::types::SessionStatus;
use uuid::Uuid;

/// Stable refusal for a caller that is not a live leaf session of a project.
pub const FAILURE_SIGNATURE_CALLER_UNAVAILABLE: &str = "failure_signature_caller_unavailable";

impl AgentControlHandle {
    /// # Errors
    /// `failure_signature_query_invalid`, `failure_signature_caller_unavailable`,
    /// or a persistence error.
    pub async fn agent_query_failure_signatures(
        &self,
        caller: Uuid,
        request: AgentQueryFailureSignaturesRequestV1,
    ) -> Result<AgentQueryFailureSignaturesResultV1> {
        request
            .validate()
            .map_err(|_| DaemonError::InvalidParam(FAILURE_SIGNATURE_QUERY_INVALID.into()))?;
        let unavailable =
            || DaemonError::InvalidParam(FAILURE_SIGNATURE_CALLER_UNAVAILABLE.into());
        let sources = {
            let store = self.store.lock().await;
            let session = store
                .get_session(caller)?
                .filter(|session| {
                    rsi_common::is_leaf_kind(session.session_kind)
                        && !matches!(
                            session.status,
                            SessionStatus::Archived | SessionStatus::Deleted
                        )
                })
                .ok_or_else(unavailable)?;
            let project_id = session.project_id.ok_or_else(unavailable)?;
            store.open_failure_signature_issues(project_id)?
        };
        Ok(AgentQueryFailureSignaturesResultV1::from_issues(
            &sources, &request,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::super::agent_verbs::tests::{control_handle_with_store, test_session};
    use super::*;
    use rsi_common::failure_signature::{FENCE_INFO, digest};
    use rsi_common::types::{IssueStatus, NewIssue, Project, SessionKind};
    use std::sync::Arc;

    const TEXT: &str = "thread 'a::b' (1) panicked at crates/x.rs:1:1:\nboom 42\nnote: run with `RUST_BACKTRACE=1`";

    type SharedStore = Arc<tokio::sync::Mutex<crate::store::Store>>;

    fn body(issue: u64) -> String {
        let record = serde_json::json!({
            "test_id": "a::b", "matcher": {"digest": digest(TEXT)},
            "issue": issue, "class": "regression",
        });
        format!("```{FENCE_INFO}\n{record}\n```\n")
    }

    async fn project_with_worker(store: &SharedStore) -> (Uuid, Uuid) {
        let project = Uuid::new_v4();
        let worker = Uuid::new_v4();
        let guard = store.lock().await;
        let now = chrono::Utc::now();
        guard
            .insert_project(&Project {
                id: project,
                name: format!("p-{project}"),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .expect("project");
        let mut row = test_session(worker, std::path::PathBuf::from("/tmp"));
        row.project_id = Some(project);
        row.session_kind = SessionKind::Task;
        row.status = SessionStatus::Running;
        guard.insert_session(&row).expect("session");
        (project, worker)
    }

    /// Create an Issue whose body carries a record naming its own number.
    async fn issue_with_record(store: &SharedStore, project: Uuid) -> (Uuid, u64) {
        let guard = store.lock().await;
        let created = guard
            .create_issue(&NewIssue {
                project_id: project,
                title: "qa-regression".into(),
                body: String::new(),
                priority: None,
                labels: Vec::new(),
                created_by_session_id: None,
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            })
            .expect("issue");
        let number = u64::try_from(created.display_number).expect("number");
        guard
            .update_issue(
                created.id,
                &rsi_common::types::IssueUpdate {
                    body: Some(body(number)),
                    ..Default::default()
                },
            )
            .expect("body");
        (created.id, number)
    }

    fn query() -> AgentQueryFailureSignaturesRequestV1 {
        AgentQueryFailureSignaturesRequestV1 {
            test_id: Some("a::b".into()),
            digest: None,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn worker_reads_open_owner_by_test_name_and_by_digest_and_expiry_is_live() {
        let (control, store) = control_handle_with_store();
        let (project, worker) = project_with_worker(&store).await;
        let (issue_id, number) = issue_with_record(&store, project).await;

        let by_name = control
            .agent_query_failure_signatures(worker, query())
            .await
            .expect("query");
        assert_eq!(by_name.records.len(), 1);
        assert_eq!(by_name.records[0].record.issue, number);
        assert_eq!(by_name.records[0].issue_id, issue_id);
        assert_eq!(by_name.records[0].issue_status, "Open");
        let by_digest = control
            .agent_query_failure_signatures(
                worker,
                AgentQueryFailureSignaturesRequestV1 {
                    test_id: None,
                    digest: Some(digest(TEXT)),
                },
            )
            .await
            .expect("digest query");
        assert_eq!(by_digest.records, by_name.records);

        store
            .lock()
            .await
            .update_issue_status(issue_id, IssueStatus::InProgress)
            .expect("in progress");
        let live = control
            .agent_query_failure_signatures(worker, query())
            .await
            .expect("in progress query");
        assert_eq!(live.records.len(), 1);
        assert_eq!(live.records[0].issue_status, "InProgress");

        store
            .lock()
            .await
            .update_issue_status(issue_id, IssueStatus::Closed)
            .expect("close");
        let expired = control
            .agent_query_failure_signatures(worker, query())
            .await
            .expect("closed query");
        assert!(expired.records.is_empty(), "closed owner must expire");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn unknown_query_is_typed_empty_and_other_projects_are_invisible() {
        let (control, store) = control_handle_with_store();
        let (project, worker) = project_with_worker(&store).await;
        let (other_project, other_worker) = project_with_worker(&store).await;
        issue_with_record(&store, project).await;

        let unknown = control
            .agent_query_failure_signatures(
                worker,
                AgentQueryFailureSignaturesRequestV1 {
                    test_id: Some("no::such".into()),
                    digest: None,
                },
            )
            .await
            .expect("unknown query");
        assert_eq!(unknown, AgentQueryFailureSignaturesResultV1::default());

        let foreign = control
            .agent_query_failure_signatures(other_worker, query())
            .await
            .expect("foreign query");
        assert!(foreign.records.is_empty(), "project {other_project} sees no records");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn invalid_query_and_unavailable_caller_are_refused_with_stable_codes() {
        let (control, store) = control_handle_with_store();
        let (_, worker) = project_with_worker(&store).await;
        let empty = control
            .agent_query_failure_signatures(worker, AgentQueryFailureSignaturesRequestV1::default())
            .await
            .unwrap_err();
        assert!(empty.to_string().contains(FAILURE_SIGNATURE_QUERY_INVALID));
        let stranger = control
            .agent_query_failure_signatures(Uuid::new_v4(), query())
            .await
            .unwrap_err();
        assert!(stranger.to_string().contains(FAILURE_SIGNATURE_CALLER_UNAVAILABLE));
    }
}
