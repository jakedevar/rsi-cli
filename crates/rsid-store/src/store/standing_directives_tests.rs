//! #2649: standing operator directives: scope-keyed delivery that outlives the
//! seat, expiry and clearing, the operator-attributed write path, replay and
//! the table-level backstops.

use super::*;
use crate::store::manager_nodes::tests::{area_fixture, area_request};
use crate::store::portfolio_nodes::PortfolioGrantor;
use crate::test_support::test_session;
use rsi_common::harness_manager_v2::{ManagerLaunchChoiceV2, ManagerPolicyV2};
use rsi_common::manager_nodes::ManagerNodeSelectorV1;
use rsi_common::portfolio_nodes::ConfigurePortfolioNodeRequestV1;
use rsi_common::standing_directives::{
    STANDING_DIRECTIVE_IDEMPOTENCY_CONFLICT, STANDING_DIRECTIVE_INVALID_REQUEST,
    STANDING_DIRECTIVE_LIMIT, STANDING_DIRECTIVE_MAX_ACTIVE_PER_SCOPE,
    STANDING_DIRECTIVE_NOT_AUTHORIZED, STANDING_DIRECTIVE_NOT_FOUND,
};
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
    let mut row = test_session(portfolio, PathBuf::from("/tmp/standing-directives"));
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
                idempotency_key: "directive-portfolio".into(),
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

fn now() -> DateTime<Utc> {
    "2026-10-15T07:35:00Z".parse().unwrap()
}

fn input<'a>(
    scope: ManagerNodeRefV1,
    text: &'a str,
    provenance: Option<&'a str>,
    ttl: Option<u64>,
    key: &'a str,
) -> StandingDirectiveSetInput<'a> {
    StandingDirectiveSetInput {
        scope,
        text,
        provenance,
        expires_in_secs: ttl,
        idempotency_key: key,
    }
}

const OVERNIGHT: &str = "Overnight: keep the hub and the laptop busy; resolve blocked runs on the wake that reports them; many small landings, reds first; escalate only real gates via the operator to-do list; send ONE morning summary.";

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn operator_directive_is_delivered_verbatim_to_the_scope_seat_and_to_no_other() {
    let s = seats();
    let project = ManagerNodeRefV1::Project {
        project_id: s.project,
    };
    let write = s
        .store
        .standing_directive_set(
            &StandingDirectiveActor::Operator,
            &input(project, OVERNIGHT, None, None, "op-1"),
            now(),
        )
        .unwrap();
    assert!(!write.deduplicated);
    assert_eq!(write.directive.text, OVERNIGHT, "verbatim, every term");
    assert_eq!(write.directive.author, "operator");
    assert_eq!(write.directive.scope, project.as_ref_text());

    // The project's PM seat reads it; no handoff text is involved.
    let for_pm = s.store.standing_directives_for_seat(s.pm, now()).unwrap();
    assert_eq!(for_pm, vec![write.directive.clone()]);
    // The area seat and the portfolio seat hold other scopes.
    assert!(
        s.store
            .standing_directives_for_seat(s.area, now())
            .unwrap()
            .is_empty()
    );
    assert!(
        s.store
            .standing_directives_for_seat(s.portfolio, now())
            .unwrap()
            .is_empty()
    );
    // A session that holds no manager seat reads nothing.
    assert!(
        s.store
            .standing_directives_for_seat(Uuid::new_v4(), now())
            .unwrap()
            .is_empty()
    );
    // A successor seat is the same scope: the read is keyed on the scope,
    // never on the session that wrote or last read it.
    assert_eq!(
        s.store.active_standing_directives(project, now()).unwrap(),
        for_pm
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn directive_expires_and_clearing_ends_delivery_but_keeps_the_row() {
    let s = seats();
    let project = ManagerNodeRefV1::Project {
        project_id: s.project,
    };
    let ops = StandingDirectiveActor::Operator;
    let overnight = s
        .store
        .standing_directive_set(
            &ops,
            &input(project, OVERNIGHT, None, Some(8 * 3600), "a"),
            now(),
        )
        .unwrap()
        .directive;
    let open_ended = s
        .store
        .standing_directive_set(
            &ops,
            &input(project, "Always run the gate.", None, None, "b"),
            now(),
        )
        .unwrap()
        .directive;
    assert_eq!(overnight.expires_at, Some(now() + Duration::hours(8)));
    let at = |hours| now() + Duration::hours(hours);
    assert_eq!(
        s.store
            .active_standing_directives(project, at(7))
            .unwrap()
            .len(),
        2
    );
    let after = s.store.active_standing_directives(project, at(9)).unwrap();
    assert_eq!(
        after,
        vec![open_ended.clone()],
        "the overnight directive expired"
    );

    let cleared = s
        .store
        .standing_directive_clear(&ops, open_ended.id, at(10))
        .unwrap();
    assert!(!cleared.deduplicated);
    assert_eq!(cleared.directive.cleared_by.as_deref(), Some("operator"));
    assert!(
        s.store
            .active_standing_directives(project, at(10))
            .unwrap()
            .is_empty()
    );
    let again = s
        .store
        .standing_directive_clear(&ops, open_ended.id, at(11))
        .unwrap();
    assert!(again.deduplicated);
    assert_eq!(again.directive.cleared_at, cleared.directive.cleared_at);
    let all = s
        .store
        .standing_directive_list(Some(project), true, at(12))
        .unwrap();
    assert_eq!(all.len(), 2, "history keeps cleared and expired rows");
    assert_eq!(
        s.store
            .standing_directive_list(None, false, at(12))
            .unwrap()
            .len(),
        0
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_manager_relays_only_below_its_own_scope_with_provenance() {
    let s = seats();
    let project = ManagerNodeRefV1::Project {
        project_id: s.project,
    };
    let area = s.store.tier_caller_node(s.area).unwrap().unwrap();
    let gm = s.store.standing_directive_agent_actor(s.portfolio).unwrap();
    let pm = s.store.standing_directive_agent_actor(s.pm).unwrap();
    let area_actor = s.store.standing_directive_agent_actor(s.area).unwrap();

    // The portfolio seat relays to the project it covers; provenance is stored.
    let relayed = s
        .store
        .standing_directive_set(
            &gm,
            &input(
                project,
                OVERNIGHT,
                Some("operator, 2026-10-15 07:35Z, TUI chat"),
                None,
                "r1",
            ),
            now(),
        )
        .unwrap()
        .directive;
    assert_eq!(relayed.author, format!("session:{}", s.portfolio));
    assert_eq!(
        relayed.provenance.as_deref(),
        Some("operator, 2026-10-15 07:35Z, TUI chat")
    );
    assert_eq!(
        s.store.standing_directives_for_seat(s.pm, now()).unwrap(),
        vec![relayed.clone()]
    );

    // A relay without provenance is not an operator-attributed path.
    assert_eq!(
        code(
            s.store
                .standing_directive_set(&gm, &input(project, "x", None, None, "r2"), now())
                .unwrap_err()
        ),
        STANDING_DIRECTIVE_INVALID_REQUEST
    );
    // A PM cannot write its own scope; an area seat cannot write upward.
    for (actor, scope) in [
        (&pm, project),
        (&area_actor, project),
        (
            &gm,
            ManagerNodeRefV1::Portfolio {
                node_id: portfolio_id(&s),
            },
        ),
    ] {
        assert_eq!(
            code(
                s.store
                    .standing_directive_set(actor, &input(scope, "x", Some("p"), None, "r3"), now())
                    .unwrap_err()
            ),
            STANDING_DIRECTIVE_NOT_AUTHORIZED,
            "{scope:?}"
        );
    }
    // A PM relays down to its area.
    let below = s
        .store
        .standing_directive_set(
            &pm,
            &input(
                area,
                "Area: land small.",
                Some("operator via GM"),
                None,
                "r4",
            ),
            now(),
        )
        .unwrap();
    assert!(below.directive.scope.starts_with("area:"));
    // The author's key space is its own: another author reusing a key is a new row.
    let other = s
        .store
        .standing_directive_set(
            &gm,
            &input(
                area,
                "Area: land small.",
                Some("operator via GM"),
                None,
                "r4",
            ),
            now(),
        )
        .unwrap();
    assert!(!other.deduplicated);
}

fn portfolio_id(s: &Seats) -> Uuid {
    match s.store.tier_caller_node(s.portfolio).unwrap().unwrap() {
        ManagerNodeRefV1::Portfolio { node_id } => node_id,
        other => panic!("{other:?}"),
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_manager_clears_what_a_manager_relayed_but_never_what_the_operator_wrote() {
    let s = seats();
    let project = ManagerNodeRefV1::Project {
        project_id: s.project,
    };
    let gm = s.store.standing_directive_agent_actor(s.portfolio).unwrap();
    let pm = s.store.standing_directive_agent_actor(s.pm).unwrap();
    let relayed = s
        .store
        .standing_directive_set(&gm, &input(project, "a", Some("op"), None, "k"), now())
        .unwrap()
        .directive;
    let by_operator = s
        .store
        .standing_directive_set(
            &StandingDirectiveActor::Operator,
            &input(project, "b", None, None, "k"),
            now(),
        )
        .unwrap()
        .directive;
    // The PM holds the scope itself, not above it: the row is not visible to it.
    assert_eq!(
        code(
            s.store
                .standing_directive_clear(&pm, relayed.id, now())
                .unwrap_err()
        ),
        STANDING_DIRECTIVE_NOT_FOUND
    );
    assert_eq!(
        code(
            s.store
                .standing_directive_clear(&gm, by_operator.id, now())
                .unwrap_err()
        ),
        STANDING_DIRECTIVE_NOT_AUTHORIZED
    );
    let cleared = s
        .store
        .standing_directive_clear(&gm, relayed.id, now())
        .unwrap();
    assert_eq!(
        cleared.directive.cleared_by,
        Some(format!("session:{}", s.portfolio))
    );
    assert_eq!(
        code(
            s.store
                .standing_directive_clear(&gm, Uuid::new_v4(), now())
                .unwrap_err()
        ),
        STANDING_DIRECTIVE_NOT_FOUND
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn replay_returns_the_first_row_a_changed_request_conflicts_and_the_scope_is_bounded() {
    let s = seats();
    let project = ManagerNodeRefV1::Project {
        project_id: s.project,
    };
    let ops = StandingDirectiveActor::Operator;
    let first = s
        .store
        .standing_directive_set(&ops, &input(project, "same", None, None, "k"), now())
        .unwrap();
    let replay = s
        .store
        .standing_directive_set(&ops, &input(project, "same", None, None, "k"), now())
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.directive.id, first.directive.id);
    assert_eq!(
        code(
            s.store
                .standing_directive_set(&ops, &input(project, "different", None, None, "k"), now())
                .unwrap_err()
        ),
        STANDING_DIRECTIVE_IDEMPOTENCY_CONFLICT
    );
    for index in 1..STANDING_DIRECTIVE_MAX_ACTIVE_PER_SCOPE {
        s.store
            .standing_directive_set(
                &ops,
                &input(project, "more", None, None, &format!("m{index}")),
                now(),
            )
            .unwrap();
    }
    assert_eq!(
        code(
            s.store
                .standing_directive_set(
                    &ops,
                    &input(project, "one too many", None, None, "z"),
                    now()
                )
                .unwrap_err()
        ),
        STANDING_DIRECTIVE_LIMIT
    );
    // An unknown scope is refused for the operator too.
    assert_eq!(
        code(
            s.store
                .standing_directive_set(
                    &ops,
                    &input(
                        ManagerNodeRefV1::Project {
                            project_id: Uuid::new_v4()
                        },
                        "x",
                        None,
                        None,
                        "u"
                    ),
                    now(),
                )
                .unwrap_err()
        ),
        MANAGER_TIER_TARGET_UNKNOWN
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn the_table_refuses_a_session_row_without_provenance_and_a_half_cleared_row() {
    let s = seats();
    let scope = format!("project:{}", s.project);
    let insert = |author: &str,
                  provenance: Option<&str>,
                  cleared_at: Option<&str>,
                  cleared_by: Option<&str>| {
        s.store.conn.execute(
            "INSERT INTO standing_directives
                (id,scope,body,author,provenance,idempotency_key,request_digest,created_at,cleared_at,cleared_by)
             VALUES (?1,?2,'t',?3,?4,'k','d','2026-10-15T07:35:00.000000000Z',?5,?6)",
            params![Uuid::new_v4().to_string(), scope, author, provenance, cleared_at, cleared_by],
        )
    };
    let session = format!("session:{}", Uuid::new_v4());
    assert!(
        insert(&session, None, None, None).is_err(),
        "a relay needs provenance"
    );
    assert!(
        insert(
            "operator",
            None,
            Some("2026-10-15T08:00:00.000000000Z"),
            None
        )
        .is_err()
    );
    assert!(insert("operator", None, None, Some("operator")).is_err());
    assert!(insert("stranger", None, None, None).is_err());
    assert!(insert("operator", None, None, None).is_ok());
}
