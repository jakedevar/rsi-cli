//! #1747: the operator to-do list: scope derived from the seat, upsert
//! idempotency, the evidence rule and the table-level backstops.

use super::*;
use crate::store::manager_nodes::tests::{area_fixture, area_request};
use crate::store::portfolio_nodes::PortfolioGrantor;
use crate::test_support::test_session;
use rsi_common::harness_manager_v2::{ManagerLaunchChoiceV2, ManagerPolicyV2};
use rsi_common::manager_nodes::ManagerNodeSelectorV1;
use rsi_common::portfolio_nodes::ConfigurePortfolioNodeRequestV1;
use rsi_common::types::{SessionProvider, SessionStatus};
use std::path::PathBuf;

fn code(error: DaemonError) -> String {
    match error {
        DaemonError::InvalidParam(code) => code,
        other => panic!("expected a typed refusal, got {other:?}"),
    }
}

struct Seats {
    store: Store,
    project: Uuid,
    pm: Uuid,
    area: Uuid,
    portfolio: Uuid,
}

fn seats() -> Seats {
    let (store, project, root, group, _epics) = area_fixture(true);
    let area_node = store
        .appoint_area_node(&area_request(
            &store,
            project,
            &root,
            ManagerNodeSelectorV1::Selected {
                group_ids: vec![group],
                epic_ids: vec![],
            },
        ))
        .unwrap();
    let portfolio = Uuid::new_v4();
    let mut row = test_session(portfolio, PathBuf::from("/tmp/operator-todos"));
    row.status = SessionStatus::Completed;
    store.insert_session(&row).unwrap();
    store
        .configure_portfolio_node_confirmed(
            &ConfigurePortfolioNodeRequestV1 {
                node_id: None,
                parent_node_id: None,
                adopt_node_ids: vec![],
                expected_parent_grant_version: None,
                tier_label: "global".into(),
                seat_session_id: portfolio,
                project_ids: vec![project],
                allowed_launches: vec![ManagerLaunchChoiceV2 {
                    provider: SessionProvider::Claude,
                    model: "claude-opus-5-5".into(),
                    effort: None,
                }],
                policy: ManagerPolicyV2::default(),
                child_policy: None,
                max_direct_reports: 5,
                expected_node_grant_version: 0,
                expected_authority_epoch: 0,
                idempotency_key: "todo-portfolio".into(),
            },
            PortfolioGrantor::Operator,
            "operator:test",
            true,
        )
        .unwrap();
    Seats {
        project,
        pm: root.seat_root_session_id,
        area: area_node.seat_root_session_id,
        portfolio,
        store,
    }
}

fn upsert(key: &str, title: &str, idem: &str) -> AgentOperatorTodoUpsertRequestV1 {
    AgentOperatorTodoUpsertRequestV1 {
        todo_key: key.into(),
        title: title.into(),
        how: "codex login".into(),
        link: Some("issue:#1747".into()),
        priority: None,
        idempotency_key: idem.into(),
    }
}

fn close(key: &str, status: OperatorTodoStatusV1, idem: &str) -> AgentOperatorTodoCloseRequestV1 {
    AgentOperatorTodoCloseRequestV1 {
        todo_id: None,
        todo_key: Some(key.into()),
        status,
        evidence: "[observed] codex login status shows an account".into(),
        observed: true,
        idempotency_key: idem.into(),
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn scope_comes_from_the_seat_never_from_the_request() {
    let s = seats();
    let pm = s.store.operator_todo_agent_actor(s.pm).unwrap();
    let write = s
        .store
        .operator_todo_upsert(&pm, &upsert("login", "Log in", "k1"))
        .unwrap();
    assert!(write.created && !write.deduplicated);
    assert_eq!(write.todo.scope, format!("project:{}", s.project));
    assert_eq!(write.todo.project_id, Some(s.project));
    assert_eq!(write.todo.added_by, format!("session:{}", s.pm));
    assert_eq!(write.todo.status, OperatorTodoStatusV1::Open);
    assert_eq!(write.todo.priority, 3);

    let area = s.store.operator_todo_agent_actor(s.area).unwrap();
    let area_item = s
        .store
        .operator_todo_upsert(&area, &upsert("login", "Area login", "k1"))
        .unwrap();
    assert!(
        area_item.todo.scope.starts_with("area:"),
        "{}",
        area_item.todo.scope
    );
    assert_ne!(area_item.todo.id, write.todo.id, "same key, other scope");

    let portfolio = s.store.operator_todo_agent_actor(s.portfolio).unwrap();
    let top = s
        .store
        .operator_todo_upsert(&portfolio, &upsert("login", "Top login", "k1"))
        .unwrap();
    assert!(top.todo.scope.starts_with("portfolio:"));
    assert_eq!(top.todo.project_id, None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_session_without_a_manager_seat_is_refused() {
    let s = seats();
    let worker = Uuid::new_v4();
    let mut row = test_session(worker, PathBuf::from("/tmp/operator-todos-worker"));
    row.project_id = Some(s.project);
    row.status = SessionStatus::Running;
    s.store.insert_session(&row).unwrap();
    assert_eq!(
        code(s.store.operator_todo_agent_actor(worker).unwrap_err()),
        OPERATOR_TODO_NOT_AUTHORIZED
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn upsert_is_idempotent_and_updates_the_open_item_by_key() {
    let s = seats();
    let pm = s.store.operator_todo_agent_actor(s.pm).unwrap();
    let first = s
        .store
        .operator_todo_upsert(&pm, &upsert("login", "Log in", "k1"))
        .unwrap();
    let replay = s
        .store
        .operator_todo_upsert(&pm, &upsert("login", "Log in", "k1"))
        .unwrap();
    assert!(replay.deduplicated && !replay.created);
    assert_eq!(replay.todo, first.todo);

    let conflict = s
        .store
        .operator_todo_upsert(&pm, &upsert("login", "Different", "k1"))
        .unwrap_err();
    assert_eq!(code(conflict), OPERATOR_TODO_IDEMPOTENCY_CONFLICT);

    let mut changed = upsert("login", "Log in to Codex", "k2");
    changed.priority = Some(1);
    let updated = s.store.operator_todo_upsert(&pm, &changed).unwrap();
    assert!(!updated.created && !updated.deduplicated);
    assert_eq!(updated.todo.id, first.todo.id);
    assert_eq!(updated.todo.title, "Log in to Codex");
    assert_eq!(updated.todo.priority, 1);
    assert!(updated.todo.updated_at >= first.todo.updated_at);

    // The same content under a new key is a no-op update: updated_at stays.
    let same = s
        .store
        .operator_todo_upsert(&pm, &{
            let mut r = changed.clone();
            r.idempotency_key = "k3".into();
            r
        })
        .unwrap();
    assert_eq!(same.todo.updated_at, updated.todo.updated_at);
    assert_eq!(s.store.operator_todo_list(None).unwrap().open.len(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn an_agent_never_closes_without_evidence_or_marks_done_unobserved() {
    let s = seats();
    let pm = s.store.operator_todo_agent_actor(s.pm).unwrap();
    s.store
        .operator_todo_upsert(&pm, &upsert("login", "Log in", "k1"))
        .unwrap();

    let mut unobserved = close("login", OperatorTodoStatusV1::Done, "c1");
    unobserved.observed = false;
    assert_eq!(
        code(
            s.store
                .operator_todo_agent_close(&pm, &unobserved)
                .unwrap_err()
        ),
        OPERATOR_TODO_EVIDENCE_REQUIRED
    );
    let mut blank = close("login", OperatorTodoStatusV1::Dismissed, "c2");
    blank.evidence = "   ".into();
    assert_eq!(
        code(s.store.operator_todo_agent_close(&pm, &blank).unwrap_err()),
        OPERATOR_TODO_EVIDENCE_REQUIRED
    );
    assert_eq!(s.store.operator_todo_list(None).unwrap().open.len(), 1);

    let done = s
        .store
        .operator_todo_agent_close(&pm, &close("login", OperatorTodoStatusV1::Done, "c3"))
        .unwrap();
    assert_eq!(done.todo.status, OperatorTodoStatusV1::Done);
    assert!(done.todo.evidence_observed);
    assert_eq!(
        done.todo.evidence.as_deref(),
        Some("[observed] codex login status shows an account")
    );
    assert_eq!(done.todo.closed_by, Some(format!("session:{}", s.pm)));
    assert!(done.todo.closed_at.is_some());

    let replay = s
        .store
        .operator_todo_agent_close(&pm, &close("login", OperatorTodoStatusV1::Done, "c3"))
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.todo, done.todo);
    // A fresh key on a closed item is a no-op that reports the recorded close
    // (#2974); an upsert of its key refuses.
    let again = s
        .store
        .operator_todo_agent_close(&pm, &close("login", OperatorTodoStatusV1::Done, "c4"))
        .unwrap();
    assert!(again.deduplicated && !again.created);
    assert_eq!(again.todo, done.todo);
    assert_eq!(
        code(
            s.store
                .operator_todo_upsert(&pm, &upsert("login", "Again", "k9"))
                .unwrap_err()
        ),
        OPERATOR_TODO_CLOSED
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn project_and_area_seats_close_only_their_own_scope_a_portfolio_seat_any() {
    let s = seats();
    let pm = s.store.operator_todo_agent_actor(s.pm).unwrap();
    let area = s.store.operator_todo_agent_actor(s.area).unwrap();
    let portfolio = s.store.operator_todo_agent_actor(s.portfolio).unwrap();
    let pm_item = s
        .store
        .operator_todo_upsert(&pm, &upsert("pm-item", "PM item", "a"))
        .unwrap()
        .todo;

    let mut by_id = close("ignored", OperatorTodoStatusV1::Dismissed, "b");
    by_id.todo_key = None;
    by_id.todo_id = Some(pm_item.id);
    assert_eq!(
        code(
            s.store
                .operator_todo_agent_close(&area, &by_id)
                .unwrap_err()
        ),
        OPERATOR_TODO_NOT_IN_SCOPE
    );
    // A key resolves only inside the caller's own scope.
    assert_eq!(
        code(
            s.store
                .operator_todo_agent_close(
                    &area,
                    &close("pm-item", OperatorTodoStatusV1::Done, "c")
                )
                .unwrap_err()
        ),
        OPERATOR_TODO_NOT_FOUND
    );
    by_id.idempotency_key = "d".into();
    let closed = s
        .store
        .operator_todo_agent_close(&portfolio, &by_id)
        .unwrap();
    assert_eq!(closed.todo.status, OperatorTodoStatusV1::Dismissed);
    assert_eq!(
        closed.todo.closed_by,
        Some(format!("session:{}", s.portfolio))
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn the_operator_adds_and_closes_without_evidence_and_the_list_shows_what_changed() {
    let s = seats();
    let pm = s.store.operator_todo_agent_actor(s.pm).unwrap();
    let before = Utc::now();
    let low = s
        .store
        .operator_todo_upsert(&pm, &{
            let mut r = upsert("low", "Low", "k1");
            r.priority = Some(4);
            r
        })
        .unwrap()
        .todo;
    let urgent = s
        .store
        .operator_todo_upsert(&pm, &{
            let mut r = upsert("urgent", "Urgent", "k2");
            r.priority = Some(1);
            r
        })
        .unwrap()
        .todo;
    let added = s
        .store
        .operator_todo_add(&OperatorTodoAddRequestV1 {
            title: "Turn on phone answers".into(),
            how: "rsi settings".into(),
            link: None,
            priority: None,
            project_id: Some(s.project),
            idempotency_key: "op-1".into(),
        })
        .unwrap();
    assert_eq!(added.todo.scope, "operator");
    assert_eq!(added.todo.added_by, "operator");
    assert!(added.todo.todo_key.starts_with("op-"));
    let replay = s
        .store
        .operator_todo_add(&OperatorTodoAddRequestV1 {
            title: "Turn on phone answers".into(),
            how: "rsi settings".into(),
            link: None,
            priority: None,
            project_id: Some(s.project),
            idempotency_key: "op-1".into(),
        })
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.todo.id, added.todo.id);

    let list = s.store.operator_todo_list(None).unwrap();
    let ids: Vec<_> = list.open.iter().map(|t| t.id).collect();
    assert_eq!(ids, vec![urgent.id, added.todo.id, low.id], "urgent first");
    assert!(list.recently_closed.is_empty());

    let closed = s
        .store
        .operator_todo_operator_close(&OperatorTodoCloseRequestV1 {
            todo_id: low.id,
            status: OperatorTodoStatusV1::Done,
            note: None,
        })
        .unwrap();
    assert_eq!(closed.closed_by.as_deref(), Some("operator"));
    assert_eq!(closed.evidence, None);
    // A repeat of the same operator close is a no-op; another status refuses.
    s.store
        .operator_todo_operator_close(&OperatorTodoCloseRequestV1 {
            todo_id: low.id,
            status: OperatorTodoStatusV1::Done,
            note: None,
        })
        .unwrap();
    assert_eq!(
        code(
            s.store
                .operator_todo_operator_close(&OperatorTodoCloseRequestV1 {
                    todo_id: low.id,
                    status: OperatorTodoStatusV1::Dismissed,
                    note: None,
                })
                .unwrap_err()
        ),
        OPERATOR_TODO_CLOSED
    );
    let list = s.store.operator_todo_list(Some(before)).unwrap();
    assert_eq!(list.open.len(), 2);
    assert_eq!(
        list.recently_closed
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>(),
        vec![low.id]
    );
    let later = s
        .store
        .operator_todo_list(Some(Utc::now() + chrono::Duration::seconds(5)))
        .unwrap();
    assert!(later.recently_closed.is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_scope_holds_at_most_the_open_limit() {
    let s = seats();
    let pm = s.store.operator_todo_agent_actor(s.pm).unwrap();
    for n in 0..OPERATOR_TODO_MAX_OPEN_PER_SCOPE {
        s.store
            .operator_todo_upsert(&pm, &upsert(&format!("k{n}"), "t", &format!("i{n}")))
            .unwrap();
    }
    assert_eq!(
        code(
            s.store
                .operator_todo_upsert(&pm, &upsert("extra", "t", "extra"))
                .unwrap_err()
        ),
        OPERATOR_TODO_LIMIT
    );
    // Updating an existing open item is still allowed at the limit.
    s.store
        .operator_todo_upsert(&pm, &upsert("k0", "changed", "again"))
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn the_table_refuses_an_agent_close_without_evidence_or_an_observed_done() {
    let s = seats();
    let pm = s.store.operator_todo_agent_actor(s.pm).unwrap();
    let item = s
        .store
        .operator_todo_upsert(&pm, &upsert("login", "Log in", "k1"))
        .unwrap()
        .todo;
    let author = format!("session:{}", s.pm);
    let now = stamp();
    let attempt = |status: &str, evidence: Option<&str>, observed: i64| {
        s.store.conn.execute(
            "UPDATE operator_todos SET status=?2,evidence=?3,evidence_observed=?4,closed_by=?5,closed_at=?6 WHERE id=?1",
            params![item.id.to_string(), status, evidence, observed, author, now],
        )
    };
    assert!(attempt("done", None, 1).is_err(), "no evidence");
    assert!(
        attempt("done", Some("saw it"), 0).is_err(),
        "unobserved done"
    );
    assert!(
        attempt("dismissed", None, 0).is_err(),
        "dismiss without reason"
    );
    assert!(attempt("dismissed", Some("not needed"), 0).is_ok());
    // Closed rows are terminal, and identity never changes.
    assert!(
        s.store
            .conn
            .execute(
                "UPDATE operator_todos SET title='x' WHERE id=?1",
                [item.id.to_string()]
            )
            .is_err()
    );
}

struct Tree {
    store: Store,
    b: Uuid,
    global: Uuid,
    left: Uuid,
    right: Uuid,
}

fn node_request(
    label: &str,
    seat: Uuid,
    projects: &[Uuid],
    parent: Option<Uuid>,
    key: &str,
) -> ConfigurePortfolioNodeRequestV1 {
    ConfigurePortfolioNodeRequestV1 {
        node_id: None,
        parent_node_id: parent,
        adopt_node_ids: vec![],
        expected_parent_grant_version: None,
        tier_label: label.into(),
        seat_session_id: seat,
        project_ids: projects.to_vec(),
        allowed_launches: vec![ManagerLaunchChoiceV2 {
            provider: SessionProvider::Claude,
            model: "claude-opus-5-5".into(),
            effort: None,
        }],
        policy: ManagerPolicyV2::default(),
        child_policy: None,
        max_direct_reports: 5,
        expected_node_grant_version: 0,
        expected_authority_epoch: 0,
        idempotency_key: key.into(),
    }
}

/// Finite allowances that halve per level, so two siblings fit under a parent.
fn level_policy(level: u16) -> ManagerPolicyV2 {
    use rsi_common::harness_manager_v2::{ManagerCapabilityV2, ManagerOperatingModeV2};
    ManagerPolicyV2 {
        mode: ManagerOperatingModeV2::Execute,
        capabilities: vec![
            ManagerCapabilityV2::IssueCoordinate,
            ManagerCapabilityV2::SessionCreate,
            ManagerCapabilityV2::SessionControl,
        ],
        max_created_containers: (64 >> level) - 1,
        max_created_sessions: (128 >> level) - 1,
        max_active_sessions: (64 >> level) - 1,
        ..ManagerPolicyV2::default()
    }
}

fn seat_session(store: &Store) -> Uuid {
    let id = Uuid::new_v4();
    let mut row = test_session(id, PathBuf::from("/tmp/operator-todos"));
    row.status = SessionStatus::Completed;
    store.insert_session(&row).unwrap();
    id
}

fn new_project(store: &Store, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now();
    store
        .insert_project(&rsi_common::types::Project {
            id,
            name: format!("{name} {id}"),
            path: None,
            description: None,
            color: rsi_common::types::Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();
    id
}

fn configure(store: &Store, request: &ConfigurePortfolioNodeRequestV1) -> Uuid {
    store
        .configure_portfolio_node_confirmed(
            request,
            PortfolioGrantor::Operator,
            "operator:test",
            true,
        )
        .unwrap_or_else(|e| panic!("{} {e:?}", request.idempotency_key))
        .node_id
}

/// A `global` root over projects A and B with sibling regions: `left` over
/// A, `right` over B.
fn tree() -> Tree {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (new_project(&store, "A"), new_project(&store, "B"));
    let (global, left, right) = (
        seat_session(&store),
        seat_session(&store),
        seat_session(&store),
    );
    let mut request = node_request("global", global, &[a, b], None, "g");
    request.policy = level_policy(0);
    let parent = configure(&store, &request);
    for (seat, project, key) in [(left, a, "left"), (right, b, "right")] {
        let mut request = node_request("region", seat, &[project], Some(parent), key);
        request.policy = level_policy(2);
        configure(&store, &request);
    }
    Tree {
        store,
        b,
        global,
        left,
        right,
    }
}

fn by_id(id: Uuid, idem: &str) -> AgentOperatorTodoCloseRequestV1 {
    let mut request = close("ignored", OperatorTodoStatusV1::Dismissed, idem);
    request.todo_key = None;
    request.todo_id = Some(id);
    request
}

/// #1762: a portfolio seat closes by id only inside its own coverage.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_sibling_portfolio_cannot_close_an_item_outside_its_coverage() {
    let t = tree();
    let left = t.store.operator_todo_agent_actor(t.left).unwrap();
    let right = t.store.operator_todo_agent_actor(t.right).unwrap();
    // An operator item bound to project B, and the right region's own item.
    let bound = t
        .store
        .operator_todo_add(&OperatorTodoAddRequestV1 {
            title: "B item".into(),
            how: "rsi settings".into(),
            link: None,
            priority: None,
            project_id: Some(t.b),
            idempotency_key: "op-b".into(),
        })
        .unwrap()
        .todo;
    let own = t
        .store
        .operator_todo_upsert(&right, &upsert("own", "Right item", "r1"))
        .unwrap()
        .todo;
    for (item, idem) in [(&bound, "x1"), (&own, "x2")] {
        assert_eq!(
            code(
                t.store
                    .operator_todo_agent_close(&left, &by_id(item.id, idem))
                    .unwrap_err()
            ),
            OPERATOR_TODO_NOT_IN_SCOPE
        );
        assert_eq!(
            t.store.load_todo(item.id).unwrap().status,
            OperatorTodoStatusV1::Open
        );
    }
    // The covering region closes the project-bound item.
    let closed = t
        .store
        .operator_todo_agent_close(&right, &by_id(bound.id, "x3"))
        .unwrap();
    assert_eq!(closed.todo.status, OperatorTodoStatusV1::Dismissed);
}

/// #1762: the `global` root and a covering ancestor close items of their
/// descendants; a descendant never closes an ancestor's.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_covering_ancestor_closes_a_descendants_item_but_not_the_reverse() {
    let t = tree();
    let global = t.store.operator_todo_agent_actor(t.global).unwrap();
    let right = t.store.operator_todo_agent_actor(t.right).unwrap();
    let down = t
        .store
        .operator_todo_upsert(&right, &upsert("own", "Right item", "r1"))
        .unwrap()
        .todo;
    let up = t
        .store
        .operator_todo_upsert(&global, &upsert("top", "Global item", "g1"))
        .unwrap()
        .todo;
    assert_eq!(
        code(
            t.store
                .operator_todo_agent_close(&right, &by_id(up.id, "y1"))
                .unwrap_err()
        ),
        OPERATOR_TODO_NOT_IN_SCOPE
    );
    let closed = t
        .store
        .operator_todo_agent_close(&global, &by_id(down.id, "y2"))
        .unwrap();
    assert_eq!(closed.todo.status, OperatorTodoStatusV1::Dismissed);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_wake_may_wait_only_on_an_item_in_the_callers_own_scope() {
    use rsi_common::wake_predicate::WAKE_WHEN_TODO_NOT_FOUND;
    let s = seats();
    let pm = s.store.operator_todo_agent_actor(s.pm).unwrap();
    let item = s
        .store
        .operator_todo_upsert(&pm, &upsert("hold", "Release the hold", "k1"))
        .unwrap()
        .todo;

    // The owner, and the global root that covers it, may wait.
    s.store.operator_todo_wake_authorize(s.pm, item.id).unwrap();
    s.store
        .operator_todo_wake_authorize(s.portfolio, item.id)
        .unwrap();

    // Another scope's seat, a session with no seat and an unknown id are one
    // refusal.
    for (caller, id) in [
        (s.area, item.id),
        (Uuid::new_v4(), item.id),
        (s.pm, Uuid::new_v4()),
    ] {
        assert_eq!(
            code(
                s.store
                    .operator_todo_wake_authorize(caller, id)
                    .unwrap_err()
            ),
            WAKE_WHEN_TODO_NOT_FOUND
        );
    }

    // The snapshot follows the item to its closing evidence.
    assert_eq!(
        s.store
            .operator_todo_by_id(item.id)
            .unwrap()
            .unwrap()
            .status,
        OperatorTodoStatusV1::Open
    );
    s.store
        .operator_todo_agent_close(&pm, &close("hold", OperatorTodoStatusV1::Done, "c1"))
        .unwrap();
    let closed = s.store.operator_todo_by_id(item.id).unwrap().unwrap();
    assert_eq!(closed.status, OperatorTodoStatusV1::Done);
    assert!(closed.evidence.is_some());
    assert!(
        s.store
            .operator_todo_by_id(Uuid::new_v4())
            .unwrap()
            .is_none()
    );
}

fn operator_item(s: &Seats, title: &str, idem: &str) -> OperatorTodoV1 {
    s.store
        .operator_todo_add(&OperatorTodoAddRequestV1 {
            title: title.into(),
            how: "rsi grant restore --caps v8".into(),
            link: Some("issue:#2950".into()),
            priority: Some(2),
            project_id: Some(s.project),
            idempotency_key: idem.into(),
        })
        .unwrap()
        .todo
}

fn close_by_operator(s: &Seats, id: Uuid, status: OperatorTodoStatusV1) {
    s.store
        .operator_todo_operator_close(&OperatorTodoCloseRequestV1 {
            todo_id: id,
            status,
            note: None,
        })
        .unwrap();
}

/// #2950: the operator's undo brings back what they closed, as a fresh open
/// copy; the closed row stays closed and the receipt records the link.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn the_operator_reopens_an_item_they_just_closed_as_an_open_copy() {
    let s = seats();
    let item = operator_item(&s, "Restore grants", "r1");
    close_by_operator(&s, item.id, OperatorTodoStatusV1::Done);
    assert!(s.store.operator_todo_list(None).unwrap().open.is_empty());

    let write = s
        .store
        .operator_todo_operator_reopen(&OperatorTodoReopenRequestV1 { todo_id: item.id })
        .unwrap();
    assert!(write.created && !write.deduplicated);
    let back = write.todo;
    assert_ne!(back.id, item.id);
    assert_eq!(back.status, OperatorTodoStatusV1::Open);
    assert_eq!((back.closed_by.clone(), back.closed_at), (None, None));
    assert_eq!(back.scope, item.scope);
    assert_eq!(back.title, item.title);
    assert_eq!(back.how, item.how);
    assert_eq!(back.link, item.link);
    assert_eq!(back.priority, item.priority);
    assert_eq!(back.project_id, item.project_id);
    assert_eq!(back.added_by, item.added_by);
    assert_eq!(back.created_at, item.created_at);
    assert!(back.todo_key.starts_with(&format!("{}~r", item.todo_key)));
    assert!(back.todo_key.len() <= rsi_common::operator_todos::OPERATOR_TODO_MAX_KEY_BYTES);

    // The closed row is untouched and the open list shows the copy.
    let original = s.store.operator_todo_by_id(item.id).unwrap().unwrap();
    assert_eq!(original.status, OperatorTodoStatusV1::Done);
    assert_eq!(original.closed_by.as_deref(), Some("operator"));
    let list = s.store.operator_todo_list(None).unwrap();
    assert_eq!(
        list.open.iter().map(|t| t.id).collect::<Vec<_>>(),
        vec![back.id]
    );

    // A repeat of the same undo answers with the same copy, writing nothing.
    let again = s
        .store
        .operator_todo_operator_reopen(&OperatorTodoReopenRequestV1 { todo_id: item.id })
        .unwrap();
    assert!(again.deduplicated && !again.created);
    assert_eq!(again.todo.id, back.id);
    assert_eq!(s.store.operator_todo_list(None).unwrap().open.len(), 1);
    // The history record is the receipt.
    let receipt: String = s
        .store
        .conn
        .query_row(
            "SELECT todo_id FROM operator_todo_receipts WHERE principal='operator' AND idempotency_key=?1",
            [format!("reopen:{}", item.id)],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(receipt, back.id.to_string());

    // The copy can be closed and undone again; the key does not grow.
    close_by_operator(&s, back.id, OperatorTodoStatusV1::Dismissed);
    let third = s
        .store
        .operator_todo_operator_reopen(&OperatorTodoReopenRequestV1 { todo_id: back.id })
        .unwrap()
        .todo;
    assert_eq!(third.status, OperatorTodoStatusV1::Open);
    assert_eq!(third.todo_key.len(), back.todo_key.len());
    assert!(third.todo_key.starts_with(&format!("{}~r", item.todo_key)));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn reopen_refuses_an_open_item_a_managers_close_an_unknown_id_and_a_late_undo() {
    let s = seats();
    let pm = s.store.operator_todo_agent_actor(s.pm).unwrap();
    let reopen = |id: Uuid| {
        s.store
            .operator_todo_operator_reopen(&OperatorTodoReopenRequestV1 { todo_id: id })
            .map_err(code)
    };

    let open = operator_item(&s, "Still open", "o1");
    assert_eq!(reopen(open.id).unwrap_err(), OPERATOR_TODO_NOT_CLOSED);
    assert_eq!(reopen(Uuid::new_v4()).unwrap_err(), OPERATOR_TODO_NOT_FOUND);
    assert_eq!(
        reopen(Uuid::nil()).unwrap_err(),
        OPERATOR_TODO_INVALID_REQUEST
    );

    // A manager's close stays final.
    let managed = s
        .store
        .operator_todo_upsert(&pm, &upsert("mgr", "Manager item", "m1"))
        .unwrap()
        .todo;
    s.store
        .operator_todo_agent_close(&pm, &close("mgr", OperatorTodoStatusV1::Done, "m2"))
        .unwrap();
    assert_eq!(
        reopen(managed.id).unwrap_err(),
        OPERATOR_TODO_REOPEN_NOT_OPERATOR_CLOSED
    );

    // Past the window plus grace the undo is refused. Closed rows cannot be
    // rewritten, so seed one the operator closed long ago.
    let long_ago = (Utc::now() - chrono::Duration::minutes(10))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let old = Uuid::new_v4();
    s.store
        .conn
        .execute(
            "INSERT INTO operator_todos(id,scope,todo_key,title,how,priority,added_by,status,closed_by,created_at,updated_at,closed_at)
             VALUES(?1,'operator','op-old','Old','x',3,'operator','done','operator',?2,?2,?2)",
            params![old.to_string(), long_ago],
        )
        .unwrap();
    assert_eq!(
        reopen(old).unwrap_err(),
        OPERATOR_TODO_REOPEN_WINDOW_ELAPSED
    );
    assert_eq!(s.store.operator_todo_list(None).unwrap().open.len(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn reopen_respects_the_open_limit_and_keeps_long_keys_in_bounds() {
    let s = seats();
    let item = operator_item(&s, "Limit", "l1");
    close_by_operator(&s, item.id, OperatorTodoStatusV1::Done);
    for n in 0..OPERATOR_TODO_MAX_OPEN_PER_SCOPE {
        operator_item(&s, "filler", &format!("f{n}"));
    }
    assert_eq!(
        code(
            s.store
                .operator_todo_operator_reopen(&OperatorTodoReopenRequestV1 { todo_id: item.id })
                .unwrap_err()
        ),
        OPERATOR_TODO_LIMIT
    );

    let id = Uuid::new_v4();
    let long = reopened_key(&"é".repeat(120), id);
    assert!(long.len() <= rsi_common::operator_todos::OPERATOR_TODO_MAX_KEY_BYTES);
    assert!(long.contains("~r"));
    assert_eq!(reopened_key("plain~r", id).len(), "plain~r~r".len() + 8);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_seat_closing_an_item_the_operator_already_closed_changes_nothing_2974() {
    let s = seats();
    let pm = s.store.operator_todo_agent_actor(s.pm).unwrap();
    let item = s
        .store
        .operator_todo_upsert(&pm, &upsert("login", "Log in", "k1"))
        .unwrap()
        .todo;
    let operator_closed = s
        .store
        .operator_todo_operator_close(&OperatorTodoCloseRequestV1 {
            todo_id: item.id,
            status: OperatorTodoStatusV1::Done,
            note: Some("did it".into()),
        })
        .unwrap();
    // The seat asks for another status with its own evidence: the end state
    // (closed) holds, so it is no error and the operator's close stands.
    let write = s
        .store
        .operator_todo_agent_close(&pm, &close("login", OperatorTodoStatusV1::Dismissed, "c1"))
        .unwrap();
    assert!(write.deduplicated && !write.created);
    assert_eq!(write.todo, operator_closed);
    assert_eq!(write.todo.status, OperatorTodoStatusV1::Done);
    assert_eq!(write.todo.closed_by.as_deref(), Some("operator"));
    // Evidence is still demanded of the seat.
    let mut blank = close("login", OperatorTodoStatusV1::Dismissed, "c2");
    blank.evidence = String::new();
    assert_eq!(
        code(s.store.operator_todo_agent_close(&pm, &blank).unwrap_err()),
        OPERATOR_TODO_EVIDENCE_REQUIRED
    );
}
