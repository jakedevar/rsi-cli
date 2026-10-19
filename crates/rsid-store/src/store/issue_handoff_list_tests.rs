//! `ListIssueHandoffs` (#2420): the operator read over the append-only
//! `issue_handoffs` records.

use super::*;
use crate::error::DaemonError;
use crate::store::d04_test_project_id;
use rsi_common::issue_workspace::{ISSUE_HANDOFF_LIST_MAX_LIMIT, IssueWorkspaceErrorV1};
use rsi_common::types::NewIssue;

fn new_issue(project_id: Uuid, title: &str) -> NewIssue {
    NewIssue {
        project_id,
        title: title.to_string(),
        body: String::new(),
        priority: None,
        labels: Vec::new(),
        created_by_session_id: None,
        assignee: None,
        idea_id: None,
        source_event_id: None,
        source_finding_ref: None,
    }
}

fn publish(store: &Store, project_id: Uuid, issue_id: Uuid, sequence: i64, body: &str) -> Uuid {
    let author = Uuid::new_v4();
    store
        .conn
        .execute(
            "INSERT INTO issue_handoffs
                 (issue_id, sequence, project_id, author_session_id, idempotency_key,
                  body, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, '2026-10-15T09:00:00.000000000Z')",
            params![
                issue_id.to_string(),
                sequence,
                project_id.to_string(),
                author.to_string(),
                format!("handoff-{issue_id}-{sequence}"),
                body
            ],
        )
        .unwrap();
    author
}

fn request(project_id: Uuid, issue_id: Uuid) -> ListIssueHandoffsRequestV1 {
    ListIssueHandoffsRequestV1 {
        project_id,
        issue_id,
        after_sequence: None,
        limit: 16,
    }
}

fn workspace_error(error: DaemonError) -> IssueWorkspaceErrorV1 {
    let DaemonError::StructuredRpc { data, .. } = error else {
        panic!("expected typed Issue workspace error, got {error}");
    };
    serde_json::from_value(data).expect("valid Issue workspace error envelope")
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn list_issue_handoffs_returns_the_stored_records_oldest_first_and_pages() {
    let store = Store::open_in_memory().unwrap();
    let project_id = d04_test_project_id();
    let issue = store
        .create_issue(&new_issue(project_id, "Has handoffs"))
        .unwrap();
    let other = store.create_issue(&new_issue(project_id, "Other")).unwrap();
    let first_author = publish(&store, project_id, issue.id, 1, "RESULT one");
    publish(&store, project_id, issue.id, 2, "RESULT two");
    publish(&store, project_id, issue.id, 3, "RESULT three");
    publish(&store, project_id, other.id, 1, "RESULT other issue");

    let all = store
        .list_issue_workspace_handoffs(&request(project_id, issue.id))
        .unwrap();
    assert_eq!(all.total, 3);
    assert_eq!(all.next_after_sequence, None);
    assert_eq!(
        all.handoffs
            .iter()
            .map(|handoff| (handoff.sequence, handoff.body.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "RESULT one"), (2, "RESULT two"), (3, "RESULT three")]
    );
    assert_eq!(all.handoffs[0].author_session_id, first_author);

    let page = store
        .list_issue_workspace_handoffs(&ListIssueHandoffsRequestV1 {
            limit: 2,
            ..request(project_id, issue.id)
        })
        .unwrap();
    assert_eq!(page.handoffs.len(), 2);
    assert_eq!(page.next_after_sequence, Some(2));
    assert_eq!(page.total, 3);
    let rest = store
        .list_issue_workspace_handoffs(&ListIssueHandoffsRequestV1 {
            after_sequence: page.next_after_sequence,
            limit: 2,
            ..request(project_id, issue.id)
        })
        .unwrap();
    assert_eq!(
        rest.handoffs
            .iter()
            .map(|handoff| handoff.sequence)
            .collect::<Vec<_>>(),
        vec![3]
    );
    assert_eq!(rest.next_after_sequence, None);

    let none = store
        .list_issue_workspace_handoffs(&request(project_id, Uuid::new_v4()))
        .unwrap_err();
    assert_eq!(
        workspace_error(none).code,
        IssueWorkspaceErrorCodeV1::NotFoundInProject
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn list_issue_handoffs_is_project_scoped_and_bounded() {
    let store = Store::open_in_memory().unwrap();
    let project_id = d04_test_project_id();
    let issue = store
        .create_issue(&new_issue(project_id, "Scoped"))
        .unwrap();
    publish(&store, project_id, issue.id, 1, "RESULT private");
    let empty = store
        .create_issue(&new_issue(project_id, "No handoffs"))
        .unwrap();
    let page = store
        .list_issue_workspace_handoffs(&request(project_id, empty.id))
        .unwrap();
    assert!(page.handoffs.is_empty());
    assert_eq!(page.total, 0);

    // Another project cannot read this Issue's records.
    let foreign = store
        .list_issue_workspace_handoffs(&request(Uuid::new_v4(), issue.id))
        .unwrap_err();
    assert_eq!(
        workspace_error(foreign).code,
        IssueWorkspaceErrorCodeV1::NotFoundInProject
    );
    for limit in [0, ISSUE_HANDOFF_LIST_MAX_LIMIT + 1] {
        let refused = store
            .list_issue_workspace_handoffs(&ListIssueHandoffsRequestV1 {
                limit,
                ..request(project_id, issue.id)
            })
            .unwrap_err();
        assert_eq!(
            workspace_error(refused).code,
            IssueWorkspaceErrorCodeV1::InvalidRequest
        );
    }
}
