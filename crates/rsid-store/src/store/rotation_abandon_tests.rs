use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::rotation_abandon::*;
use crate::store_support::spawn_single_flight::RotationPublicationGuards;
use crate::test_support::test_session;
use rsi_common::types::SessionKind;
use rsi_common::types::SessionStatus;
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

const X: &str = "rot-blocked";
const Y: &str = "rot-blocked:abandon:1";

struct Chain {
    store: Store,
    predecessor: Uuid,
    reserved: Uuid,
    replacement: Uuid,
    epic: Uuid,
}

/// The durable shape of an abandoned blocked rotation just before the
/// replacement's publication: P (seat, leads the Epic) → S (reserved,
/// never started) → R (the replacement, reserved under S's abandon
/// rotation).
fn abandoned_chain() -> Chain {
    let store = Store::open_in_memory().expect("store");
    let dir = std::path::PathBuf::from("/var/empty/rsi-1176");
    let (predecessor, reserved, replacement, epic) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let mut seat = test_session(predecessor, dir.clone());
    seat.status = SessionStatus::Completed;
    seat.retry_attempt = None;
    seat.max_retries = None;
    let mut never_started = test_session(reserved, dir.clone());
    never_started.continued_from = Some(predecessor);
    never_started.rotation_depth = 1;
    never_started.retry_attempt = None;
    never_started.max_retries = None;
    let mut fresh = test_session(replacement, dir.clone());
    fresh.status = SessionStatus::Starting;
    fresh.continued_from = Some(reserved);
    fresh.rotation_depth = 2;
    fresh.retry_attempt = None;
    fresh.max_retries = None;
    let mut container = test_session(epic, dir);
    container.session_kind = SessionKind::Epic;
    container.status = SessionStatus::Completed;
    for session in [&container, &seat, &never_started, &fresh] {
        store.insert_session(session).expect("insert session");
    }
    let config = store
        .configure_harness_manager(
            &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                project_id: seat.project_id.unwrap(),
                session_id: predecessor,
                epic_ids: None,
                group_ids: vec![],
                expected_row_version: 0,
            },
        )
        .expect("appointed manager");
    assert!(!config.is_revoked());
    store
        .set_lead_session(epic, Some(predecessor))
        .expect("seat leads the Epic");
    let event =
        |session: Uuid, rotation: &str, phase: &str, kind: &str, metadata: serde_json::Value| {
            store
                .insert_rotation_event(session, rotation, phase, kind, Some(&metadata.to_string()))
                .expect("rotation event");
        };
    event(
        predecessor,
        X,
        crate::store::COMPLETED_TRIGGER_PHASE,
        "entered",
        serde_json::json!({ "trigger": "manual_triggered" }),
    );
    event(
        predecessor,
        X,
        "reserved",
        "successor_reserved",
        serde_json::json!({ "successor_id": reserved }),
    );
    assert!(
        store
            .record_rotation_recovery_blocked(predecessor, X, reserved)
            .expect("blocked")
    );
    event(
        predecessor,
        X,
        "reserved",
        ABANDON_REQUESTED,
        serde_json::json!({
            "seq": 1,
            "holder_id": reserved,
            "abandon_rotation_id": Y,
            "idempotency_key": "k1",
            "provider": null,
            "model": null,
        }),
    );
    event(
        reserved,
        Y,
        "reserved",
        "successor_reserved",
        serde_json::json!({ "successor_id": replacement }),
    );
    Chain {
        store,
        predecessor,
        reserved,
        replacement,
        epic,
    }
}

fn completed_successor(store: &Store, session: Uuid, rotation: &str) -> Option<String> {
    store
        .conn
        .query_row(
            "SELECT json_extract(metadata,'$.successor_id') FROM rotation_events
             WHERE session_id=?1 AND rotation_id=?2 AND event_type='completed'",
            params![session.to_string(), rotation],
            |row| row.get(0),
        )
        .optional()
        .expect("completed event")
}

fn lead_of(store: &Store, epic: Uuid) -> Option<Uuid> {
    store
        .get_session(epic)
        .expect("epic")
        .expect("epic row")
        .lead_session_id
}

async fn publish(chain: &Chain) -> Result<RotationPublication> {
    let guards = RotationPublicationGuards::acquire(chain.reserved, chain.replacement).await;
    chain.store.publish_rotation_successor_with_chain(
        &guards,
        Y,
        &serde_json::json!({ "successor_id": chain.replacement }).to_string(),
    )
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[tokio::test]
async fn abandon_publication_settles_the_blocked_chain_in_one_commit() {
    let chain = abandoned_chain();
    let store = &chain.store;
    assert_eq!(
        rotation_abandon_chain_on(&store.conn, chain.reserved, Y)
            .expect("chain")
            .expect("an abandon rotation")
            .hops,
        vec![RotationAbandonHop {
            from: chain.predecessor,
            rotation_id: X.to_string(),
            to: chain.reserved,
        }]
    );
    let publication = publish(&chain).await.expect("publish");
    assert_eq!(publication.epics, vec![chain.epic]);
    assert_eq!(
        publication.archived,
        vec![chain.predecessor, chain.reserved]
    );
    assert_eq!(lead_of(store, chain.epic), Some(chain.replacement));
    assert_eq!(
        completed_successor(store, chain.predecessor, X),
        Some(chain.reserved.to_string()),
        "the blocked intent is settled by the hop to its reserved successor"
    );
    assert_eq!(
        completed_successor(store, chain.reserved, Y),
        Some(chain.replacement.to_string())
    );
    assert_eq!(
        store
            .get_session(chain.predecessor)
            .expect("seat")
            .expect("row")
            .status,
        SessionStatus::Archived
    );
    assert_eq!(
        store.get_session(chain.reserved).unwrap().unwrap().status,
        SessionStatus::Archived
    );
    assert_eq!(
        store.manager_lineage_tip(chain.predecessor).unwrap(),
        chain.replacement
    );
    let config = store
        .get_harness_manager(
            store
                .get_session(chain.predecessor)
                .unwrap()
                .unwrap()
                .project_id
                .unwrap(),
        )
        .unwrap()
        .unwrap();
    assert!(!config.is_revoked());
    assert_eq!(config.current_session_id, Some(chain.replacement));
    assert_eq!(
        store.published_lineage_tip(chain.predecessor).expect("tip"),
        Some(chain.replacement)
    );
    assert_eq!(
        store
            .latest_open_rotation_intent(chain.predecessor)
            .expect("intent"),
        None
    );
    assert_eq!(
        store
            .blocked_rotation_chain_of(chain.predecessor)
            .expect("blocked"),
        None
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[tokio::test]
async fn a_failed_abandon_publication_leaves_the_whole_chain_unpublished() {
    let chain = abandoned_chain();
    let store = &chain.store;
    store
        .conn
        .execute_batch(
            "CREATE TEMP TRIGGER fail_holder_completion BEFORE INSERT ON rotation_events
             WHEN NEW.event_type='completed' AND NEW.rotation_id='rot-blocked:abandon:1'
             BEGIN SELECT RAISE(ABORT,'injected'); END;",
        )
        .expect("fault trigger");
    assert!(publish(&chain).await.is_err());
    assert_eq!(lead_of(store, chain.epic), Some(chain.predecessor));
    assert_eq!(completed_successor(store, chain.predecessor, X), None);
    assert_eq!(
        store
            .get_session(chain.predecessor)
            .expect("seat")
            .expect("row")
            .status,
        SessionStatus::Completed
    );
    store
        .conn
        .execute_batch("DROP TRIGGER fail_holder_completion;")
        .expect("drop fault trigger");
    publish(&chain)
        .await
        .expect("the retried publication commits");
    assert_eq!(lead_of(store, chain.epic), Some(chain.replacement));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[tokio::test]
async fn an_abandon_publication_refuses_a_blocked_intent_that_was_settled_otherwise() {
    let chain = abandoned_chain();
    let store = &chain.store;
    assert!(
        store
            .close_open_rotation_intent(chain.predecessor, X, "operator")
            .expect("close")
    );
    let error = publish(&chain).await.expect_err("settled chain");
    assert!(
        matches!(&error, DaemonError::PolicyDenied(reason) if reason.starts_with("rotation_already_settled")),
        "{error}"
    );
    assert_eq!(completed_successor(store, chain.reserved, Y), None);
    assert_eq!(lead_of(store, chain.epic), Some(chain.predecessor));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn abandon_requests_resolve_by_holder_rotation_and_by_key() {
    let chain = abandoned_chain();
    let store = &chain.store;
    let request = store
        .rotation_abandon_request_for(chain.reserved, Y)
        .expect("lookup")
        .expect("request");
    assert_eq!(request.predecessor, chain.predecessor);
    assert_eq!(request.rotation_id, X);
    assert_eq!(request.holder_id, chain.reserved);
    assert_eq!(
        store
            .rotation_abandon_request_by_key(chain.replacement, "k1")
            .expect("by key"),
        Some(request.clone())
    );
    assert_eq!(
        store
            .rotation_abandon_hop_of(chain.replacement)
            .expect("hop"),
        Some((chain.reserved, Y.to_string()))
    );
    assert!(
        store
            .holder_has_open_rotation_abandon(chain.reserved)
            .expect("open")
    );
    assert_eq!(
        store
            .rotation_abandon_replacement(&request)
            .expect("replacement"),
        Some((chain.replacement, SessionStatus::Starting))
    );
}
