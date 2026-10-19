//! `AgentOperatorTodoUpsert` / `AgentOperatorTodoClose` tests (#1747 slice 1):
//! who may write the operator's to-do list, where an item lands, and that an
//! agent never marks something done on the operator's behalf.

use super::*;
use crate::session::agent_verbs::tests::{control_handle_with_store, test_session};
use crate::store::Store;
use rsi_common::global_manager::ConfigureGlobalManagerRequestV1;
use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
use rsi_common::harness_manager_v2::{ManagerLaunchChoiceV2, ManagerPolicyV2};
use rsi_common::operator_todos::{
    OPERATOR_TODO_EVIDENCE_REQUIRED, OPERATOR_TODO_INVALID_REQUEST, OPERATOR_TODO_NOT_AUTHORIZED,
    OPERATOR_TODO_NOT_FOUND, OPERATOR_TODO_NOT_IN_SCOPE, OperatorTodoStatusV1,
};
use rsi_common::types::{Project, SessionKind, SessionProvider, SessionStatus};
use std::path::PathBuf;
use std::sync::Arc;

type SharedStore = Arc<tokio::sync::Mutex<Store>>;

struct Fixture {
    control: AgentControlHandle,
    store: SharedStore,
    project: Uuid,
    /// Project manager of `project`.
    pm: Uuid,
    /// Project manager of the second project.
    other_pm: Uuid,
    /// Portfolio (global) seat covering both projects.
    seat: Uuid,
    /// A worker in `project` with no manager seat.
    worker: Uuid,
}

fn add_project(store: &Store, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    let now = chrono::Utc::now();
    store
        .insert_project(&Project {
            id,
            name: name.into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();
    id
}

fn add_session(store: &Store, project: Option<Uuid>) -> Uuid {
    let id = Uuid::new_v4();
    let mut row = test_session(id, PathBuf::from("/tmp/operator-todos"));
    row.project_id = project;
    row.session_kind = SessionKind::Standard;
    row.status = SessionStatus::Running;
    store.insert_session(&row).unwrap();
    id
}

fn appoint(store: &Store, project: Uuid, session: Uuid) {
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: project,
            session_id: session,
            epic_ids: None,
            group_ids: vec![],
            expected_row_version: 0,
        })
        .unwrap();
}

async fn fixture() -> Fixture {
    let (control, store) = control_handle_with_store();
    let guard = store.lock().await;
    let project = add_project(&guard, "Alpha");
    let other_project = add_project(&guard, "Beta");
    let pm = add_session(&guard, Some(project));
    let other_pm = add_session(&guard, Some(other_project));
    let worker = add_session(&guard, Some(project));
    let seat = add_session(&guard, None);
    appoint(&guard, project, pm);
    appoint(&guard, other_project, other_pm);
    guard
        .configure_global_manager(
            &ConfigureGlobalManagerRequestV1 {
                session_id: seat,
                project_ids: vec![project, other_project],
                allowed_launches: vec![ManagerLaunchChoiceV2 {
                    provider: SessionProvider::Claude,
                    model: "claude-opus-5-5".into(),
                    effort: Some("high".into()),
                }],
                project_policy: ManagerPolicyV2::default(),
                expected_grant_version: 0,
                idempotency_key: "grant-0".into(),
            },
            "operator:test",
        )
        .unwrap();
    drop(guard);
    Fixture {
        control,
        store,
        project,
        pm,
        other_pm,
        seat,
        worker,
    }
}

fn upsert(key: &str, idem: &str) -> AgentOperatorTodoUpsertRequestV1 {
    AgentOperatorTodoUpsertRequestV1 {
        todo_key: key.into(),
        title: "Log in to Codex on the laptop".into(),
        how: "codex login".into(),
        link: Some("issue:#1747".into()),
        priority: Some(2),
        idempotency_key: idem.into(),
    }
}

fn close_by_key(key: &str, idem: &str) -> AgentOperatorTodoCloseRequestV1 {
    AgentOperatorTodoCloseRequestV1 {
        todo_id: None,
        todo_key: Some(key.into()),
        status: OperatorTodoStatusV1::Done,
        evidence: "[observed] codex login status shows a signed-in account".into(),
        observed: true,
        idempotency_key: idem.into(),
    }
}

fn code(error: DaemonError) -> String {
    match error {
        DaemonError::InvalidParam(code) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn project_manager_items_land_in_its_own_project_scope() {
    let f = fixture().await;
    let made = f
        .control
        .agent_operator_todo_upsert(f.pm, upsert("codex-login", "k1"))
        .await
        .unwrap();
    assert!(made.created);
    assert_eq!(made.todo.scope, format!("project:{}", f.project));
    assert_eq!(made.todo.project_id, Some(f.project));
    assert_eq!(made.todo.added_by, format!("session:{}", f.pm));
    let again = f
        .control
        .agent_operator_todo_upsert(f.pm, upsert("codex-login", "k1"))
        .await
        .unwrap();
    assert!(again.deduplicated);
    assert_eq!(again.todo.id, made.todo.id);
    let listed = f.store.lock().await.operator_todo_list(None).unwrap();
    assert_eq!(listed.open.len(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_worker_without_a_manager_seat_cannot_write_the_list() {
    let f = fixture().await;
    for result in [
        f.control
            .agent_operator_todo_upsert(f.worker, upsert("x", "k"))
            .await
            .map(drop),
        f.control
            .agent_operator_todo_close(f.worker, close_by_key("x", "k"))
            .await
            .map(drop),
    ] {
        assert_eq!(code(result.unwrap_err()), OPERATOR_TODO_NOT_AUTHORIZED);
    }
    assert!(
        f.store
            .lock()
            .await
            .operator_todo_list(None)
            .unwrap()
            .open
            .is_empty()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_project_manager_cannot_close_another_projects_item_but_a_portfolio_seat_can() {
    let f = fixture().await;
    let item = f
        .control
        .agent_operator_todo_upsert(f.other_pm, upsert("phone-answers", "k1"))
        .await
        .unwrap()
        .todo;
    let by_id = |idem: &str| AgentOperatorTodoCloseRequestV1 {
        todo_id: Some(item.id),
        todo_key: None,
        ..close_by_key("ignored", idem)
    };
    assert_eq!(
        code(
            f.control
                .agent_operator_todo_close(f.pm, by_id("c1"))
                .await
                .unwrap_err()
        ),
        OPERATOR_TODO_NOT_IN_SCOPE
    );
    // A key never reaches into another scope.
    assert_eq!(
        code(
            f.control
                .agent_operator_todo_close(f.pm, close_by_key("phone-answers", "c2"))
                .await
                .unwrap_err()
        ),
        OPERATOR_TODO_NOT_FOUND
    );
    let closed = f
        .control
        .agent_operator_todo_close(f.seat, by_id("c3"))
        .await
        .unwrap();
    assert_eq!(closed.todo.status, OperatorTodoStatusV1::Done);
    assert_eq!(closed.todo.closed_by, Some(format!("session:{}", f.seat)));
    // The owning seat's later close of the already closed item is a no-op
    // (#2974): the item stays as the portfolio seat closed it.
    let again = f
        .control
        .agent_operator_todo_close(f.other_pm, by_id("c4"))
        .await
        .unwrap();
    assert!(again.deduplicated);
    assert_eq!(again.todo, closed.todo);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn done_needs_observed_evidence_recorded_on_the_item() {
    let f = fixture().await;
    f.control
        .agent_operator_todo_upsert(f.seat, upsert("codex-login", "k1"))
        .await
        .unwrap();
    let mut unobserved = close_by_key("codex-login", "c1");
    unobserved.observed = false;
    assert_eq!(
        code(
            f.control
                .agent_operator_todo_close(f.seat, unobserved)
                .await
                .unwrap_err()
        ),
        OPERATOR_TODO_EVIDENCE_REQUIRED
    );
    let mut no_evidence = close_by_key("codex-login", "c2");
    no_evidence.evidence = String::new();
    assert_eq!(
        code(
            f.control
                .agent_operator_todo_close(f.seat, no_evidence)
                .await
                .unwrap_err()
        ),
        OPERATOR_TODO_EVIDENCE_REQUIRED
    );
    let mut both = close_by_key("codex-login", "c3");
    both.todo_id = Some(Uuid::new_v4());
    assert_eq!(
        code(
            f.control
                .agent_operator_todo_close(f.seat, both)
                .await
                .unwrap_err()
        ),
        OPERATOR_TODO_INVALID_REQUEST
    );
    let closed = f
        .control
        .agent_operator_todo_close(f.seat, close_by_key("codex-login", "c4"))
        .await
        .unwrap();
    assert!(closed.todo.evidence_observed);
    assert_eq!(
        closed.todo.evidence.as_deref(),
        Some("[observed] codex login status shows a signed-in account")
    );
    assert!(closed.todo.closed_at.is_some());
}
