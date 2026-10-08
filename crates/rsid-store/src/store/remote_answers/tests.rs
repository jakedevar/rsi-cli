use super::*;
use rsi_common::remote_pending_decisions::{RemoteAnswerOriginKindV1, RemoteAnswerOriginV1};
use rsi_common::remote_read::Text;
use rsi_common::types::{
    ConversationEvent, EventType, PendingQuestion, QuestionItem, SessionProvider,
};

fn fixture() -> (Store, AnswerPendingDecisionV1) {
    let store = Store::open_in_memory().unwrap();
    let stamp = chrono::Utc::now();
    let project = rsi_common::types::Project {
        id: Uuid::new_v4(),
        name: "Remote answers".into(),
        path: None,
        description: None,
        color: rsi_common::types::Project::DEFAULT_COLOR.into(),
        context_files: None,
        created_at: stamp,
        updated_at: stamp,
    };
    store.insert_project(&project).unwrap();
    let mut session = crate::test_support::test_session(
        Uuid::new_v4(),
        std::path::PathBuf::from("/var/tmp/remote-answer"),
    );
    session.project_id = Some(project.id);
    session.provider = SessionProvider::Claude;
    store.insert_session(&session).unwrap();
    publish(&store, session.id, 1);
    let target = store.pending_question_target(session.id).unwrap().unwrap();
    let request = AnswerPendingDecisionV1 {
        project_id: WireUuid::new(project.id.to_string()).unwrap(),
        session_id: WireUuid::new(session.id.to_string()).unwrap(),
        decision_id: Text::new(format!(
            "pending-question:{}",
            target["publication_id"].as_str().unwrap()
        ))
        .unwrap(),
        expected_target_digest: Text::new(fingerprint(&target).unwrap()).unwrap(),
        answer: Text::new("Use the safe option".into()).unwrap(),
        idempotency_key: WireUuid::new(Uuid::new_v4().to_string()).unwrap(),
        origin: RemoteAnswerOriginV1 {
            kind: RemoteAnswerOriginKindV1::Remote,
            client_node: Text::new("test-device".into()).unwrap(),
            gateway_epoch: WireUuid::new(Uuid::new_v4().to_string()).unwrap(),
        },
    };
    (store, request)
}

fn publish(store: &Store, session: Uuid, sequence: i32) {
    let question = PendingQuestion {
        questions: vec![QuestionItem {
            question: "Which option?".into(),
            header: "Choice".into(),
            options: vec![],
            multi_select: false,
        }],
    };
    store
        .publish_pending_question_event(
            &ConversationEvent {
                id: 0,
                session_id: session,
                sequence,
                event_type: EventType::ToolUse,
                role: None,
                created_at: chrono::Utc::now(),
                content: String::new(),
                tool_name: Some("AskUserQuestion".into()),
                tool_input: Some(Box::new(json!(question))),
                offload_id: None,
                tool_use_id: Some(format!("question-{sequence}")),
                metadata: None,
            },
            None,
            &question,
        )
        .unwrap();
}

fn code<T: std::fmt::Debug>(result: Result<T>, expected: &str) {
    assert!(result.unwrap_err().to_string().contains(expected));
}

fn next_question(
    store: &Store,
    request: &AnswerPendingDecisionV1,
    sequence: i32,
) -> AnswerPendingDecisionV1 {
    publish(store, uuid(&request.session_id), sequence);
    let target = store
        .pending_question_target(uuid(&request.session_id))
        .unwrap()
        .unwrap();
    let mut next = request.clone();
    next.idempotency_key = WireUuid::new(Uuid::new_v4().to_string()).unwrap();
    next.decision_id = Text::new(format!(
        "pending-question:{}",
        target["publication_id"].as_str().unwrap()
    ))
    .unwrap();
    next.expected_target_digest = Text::new(fingerprint(&target).unwrap()).unwrap();
    next
}

fn corrupt_answer(store: &Store, key: &str, corrupt_request: bool) {
    if corrupt_request {
        // Model historical corruption of immutable ingress with valid JSON
        // that cannot decode into the typed request; no production repair.
        store
            .conn
            .execute_batch("DROP TRIGGER remote_answer_deliveries_immutable")
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE remote_answer_deliveries SET request_json='{}' WHERE idempotency_key=?1",
                [key],
            )
            .unwrap();
    } else {
        store
            .conn
            .execute(
                "UPDATE remote_answer_deliveries SET outcome_json='{}' WHERE idempotency_key=?1",
                [key],
            )
            .unwrap();
    }
}

fn assert_corrupt_refused(store: &Store, request: &AnswerPendingDecisionV1, effect_started: bool) {
    let (state, raw, effect, boot): (String, String, bool, Option<String>) = store.conn.query_row(
        "SELECT state,outcome_json,effect_started,claim_boot_id FROM remote_answer_deliveries WHERE idempotency_key=?1",
        [request.idempotency_key.as_str()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
    let receipt: RemoteAnswerReceiptV1 = serde_json::from_str(&raw).unwrap();
    assert_eq!(state, "refused");
    assert_eq!(receipt.receipt_key, request.idempotency_key);
    assert_eq!(receipt.state, RemoteAnswerStateV1::Refused);
    assert_eq!(receipt.outcome.unwrap()["code"], "remote_answer_corrupt");
    assert_eq!(effect, effect_started);
    assert_eq!(boot, None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn corrupt_queued_answer_is_refused_and_later_answer_is_claimed() {
    for corrupt_request in [false, true] {
        let (store, corrupt) = fixture();
        store.prepare_remote_answer(&corrupt).unwrap();
        let healthy = next_question(&store, &corrupt, 2);
        store.prepare_remote_answer(&healthy).unwrap();
        corrupt_answer(&store, corrupt.idempotency_key.as_str(), corrupt_request);
        let boot = Uuid::new_v4();
        let delivery = store.claim_remote_answer(boot).unwrap().unwrap();
        assert_eq!(delivery.request, healthy);
        assert_eq!(delivery.claim_boot_id, Some(boot));
        assert_corrupt_refused(&store, &corrupt, false);
        assert!(store.claim_remote_answer(boot).unwrap().is_none());
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn all_corrupt_claim_batch_is_committed_before_next_batch_claims_healthy_answer() {
    let (store, mut request) = fixture();
    let mut corrupt = Vec::new();
    for sequence in 1..=64 {
        if sequence > 1 {
            request = next_question(&store, &request, sequence);
        }
        store.prepare_remote_answer(&request).unwrap();
        corrupt_answer(&store, request.idempotency_key.as_str(), false);
        corrupt.push(request.clone());
    }
    let healthy = next_question(&store, &request, 65);
    store.prepare_remote_answer(&healthy).unwrap();
    let boot = Uuid::new_v4();
    assert!(store.claim_remote_answer(boot).unwrap().is_none());
    for request in corrupt {
        assert_corrupt_refused(&store, &request, false);
    }
    assert_eq!(
        store.claim_remote_answer(boot).unwrap().unwrap().request,
        healthy
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn recovery_settles_corrupt_answer_and_preserves_retry_and_effect_fences_for_batch() {
    for corrupt_request in [false, true] {
        let (store, corrupt) = fixture();
        let old_boot = Uuid::new_v4();
        store.prepare_remote_answer(&corrupt).unwrap();
        let corrupt_delivery = store.claim_remote_answer(old_boot).unwrap().unwrap();
        store
            .set_remote_answer_delivery(&corrupt_delivery, RemoteAnswerStateV1::Running, true, None)
            .unwrap();
        let healthy_pre = next_question(&store, &corrupt, 2);
        store.prepare_remote_answer(&healthy_pre).unwrap();
        let pre = store.claim_remote_answer(old_boot).unwrap().unwrap();
        store
            .set_remote_answer_delivery(
                &pre,
                RemoteAnswerStateV1::Running,
                false,
                Some(json!({"pre_effect_retries":2})),
            )
            .unwrap();
        let healthy_post = next_question(&store, &corrupt, 3);
        store.prepare_remote_answer(&healthy_post).unwrap();
        let post = store.claim_remote_answer(old_boot).unwrap().unwrap();
        store
            .set_remote_answer_delivery(&post, RemoteAnswerStateV1::Running, true, None)
            .unwrap();
        corrupt_answer(&store, corrupt.idempotency_key.as_str(), corrupt_request);
        let new_boot = Uuid::new_v4();
        assert_eq!(store.recover_remote_answers(new_boot).unwrap(), 3);
        assert_corrupt_refused(&store, &corrupt, true);
        let pre_receipt = store.prepare_remote_answer(&healthy_pre).unwrap();
        assert_eq!(pre_receipt.state, RemoteAnswerStateV1::Queued);
        assert_eq!(pre_receipt.outcome.unwrap()["pre_effect_retries"], 2);
        assert_eq!(
            store.prepare_remote_answer(&healthy_post).unwrap().state,
            RemoteAnswerStateV1::Uncertain
        );
        assert_eq!(store.recover_remote_answers(new_boot).unwrap(), 0);
        assert_eq!(
            store
                .claim_remote_answer(new_boot)
                .unwrap()
                .unwrap()
                .request,
            healthy_pre
        );
        assert!(store.claim_remote_answer(new_boot).unwrap().is_none());
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn journal_replay_survives_clear_and_binds_answer_and_client_identity() {
    let (store, request) = fixture();
    let receipt = store.prepare_remote_answer(&request).unwrap();
    let delivery = store
        .remote_answer_delivery(request.idempotency_key.as_str())
        .unwrap()
        .unwrap();
    assert_eq!(delivery.request.origin.client_node.as_str(), "test-device");
    assert_eq!(
        delivery.request.origin.gateway_epoch,
        request.origin.gateway_epoch
    );
    store
        .clear_pending_question_exact(delivery.session_id(), &delivery.target)
        .unwrap();
    assert_eq!(store.prepare_remote_answer(&request).unwrap(), receipt);
    let mut different = request.clone();
    different.answer = Text::new("another answer".into()).unwrap();
    code(
        store.prepare_remote_answer(&different),
        "idempotency_conflict",
    );
    different = request.clone();
    different.origin.client_node = Text::new("second-device".into()).unwrap();
    code(
        store.prepare_remote_answer(&different),
        "idempotency_conflict",
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn gateway_restart_replays_original_receipt_including_legacy_epoch_fingerprints() {
    for legacy in [false, true] {
        let (store, request) = fixture();
        store.prepare_remote_answer(&request).unwrap();
        if legacy {
            // Seed the fingerprint produced by the original V162 ingress.
            store
                .conn
                .execute_batch("DROP TRIGGER remote_answer_deliveries_immutable")
                .unwrap();
            store
                .conn
                .execute(
                    "UPDATE remote_answer_deliveries SET request_fingerprint=?1",
                    [fingerprint(&serde_json::to_value(&request).unwrap()).unwrap()],
                )
                .unwrap();
        }
        let delivery = store.claim_remote_answer(Uuid::new_v4()).unwrap().unwrap();
        let finished = store
            .set_remote_answer_delivery(
                &delivery,
                RemoteAnswerStateV1::Succeeded,
                false,
                Some(json!({"code":"continuation_established"})),
            )
            .unwrap();
        store
            .clear_pending_question_exact(delivery.session_id(), &delivery.target)
            .unwrap();
        let mut restarted = request.clone();
        restarted.origin.gateway_epoch = WireUuid::new(Uuid::new_v4().to_string()).unwrap();
        assert_eq!(
            store.prepare_remote_answer(&restarted).unwrap(),
            finished.receipt
        );
        assert_eq!(
            store
                .remote_answer_delivery(delivery.key())
                .unwrap()
                .unwrap()
                .request
                .origin,
            request.origin,
            "original ingress provenance is retained"
        );
        restarted.answer = Text::new("different answer".into()).unwrap();
        code(
            store.prepare_remote_answer(&restarted),
            "idempotency_conflict",
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn wrong_digest_scope_publication_and_unpublished_question_dispatch_nothing() {
    let (store, request) = fixture();
    let mut bad = request.clone();
    bad.expected_target_digest = Text::new("sha256:wrong".into()).unwrap();
    code(store.prepare_remote_answer(&bad), "decision_changed");
    bad = request.clone();
    bad.project_id = WireUuid::new(Uuid::new_v4().to_string()).unwrap();
    code(store.prepare_remote_answer(&bad), "decision_changed");
    bad = request.clone();
    bad.decision_id = Text::new(format!("pending-question:{}", Uuid::new_v4())).unwrap();
    code(store.prepare_remote_answer(&bad), "decision_changed");
    publish(&store, uuid(&request.session_id), 2);
    code(store.prepare_remote_answer(&request), "decision_changed");
    store
        .conn
        .execute(
            "UPDATE pending_question_publications SET state='unresolved'",
            [],
        )
        .unwrap();
    code(store.prepare_remote_answer(&request), "decision_changed");
    let count: i64 = store
        .conn
        .query_row("SELECT count(*) FROM remote_answer_deliveries", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn competing_keys_and_terminal_answer_share_the_exact_occurrence() {
    let (store, request) = fixture();
    store.prepare_remote_answer(&request).unwrap();
    let mut other = request.clone();
    other.idempotency_key = WireUuid::new(Uuid::new_v4().to_string()).unwrap();
    code(store.prepare_remote_answer(&other), "decision_changed");
    let delivery = store.claim_remote_answer(Uuid::new_v4()).unwrap().unwrap();
    store
        .clear_pending_question_exact(delivery.session_id(), &delivery.target)
        .unwrap();
    code(
        store.check_remote_answer_target(&delivery),
        "decision_changed",
    );
    // The reverse race, terminal first, refuses remote ingress completely.
    let (store, request) = fixture();
    let target = store
        .pending_question_target(uuid(&request.session_id))
        .unwrap()
        .unwrap();
    store
        .clear_pending_question_exact(uuid(&request.session_id), &target)
        .unwrap();
    code(store.prepare_remote_answer(&request), "decision_changed");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn restart_retries_pre_effect_and_never_replays_post_effect() {
    let (store, request) = fixture();
    store.prepare_remote_answer(&request).unwrap();
    let old = store.claim_remote_answer(Uuid::new_v4()).unwrap().unwrap();
    assert_eq!(old.receipt.state, RemoteAnswerStateV1::Running);
    let new_boot = Uuid::new_v4();
    assert_eq!(store.recover_remote_answers(new_boot).unwrap(), 1);
    let retry = store.claim_remote_answer(new_boot).unwrap().unwrap();
    code(store.check_remote_answer_target(&old), "decision_changed");
    let started = store
        .set_remote_answer_delivery(&retry, RemoteAnswerStateV1::Running, true, None)
        .unwrap();
    assert!(started.effect_started);
    assert_eq!(store.recover_remote_answers(Uuid::new_v4()).unwrap(), 1);
    let receipt = store.prepare_remote_answer(&request).unwrap();
    assert_eq!(receipt.state, RemoteAnswerStateV1::Uncertain);
    assert!(store.claim_remote_answer(Uuid::new_v4()).unwrap().is_none());
    code(
        store.set_remote_answer_delivery(&started, RemoteAnswerStateV1::Succeeded, true, None),
        "decision_changed",
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn stale_claim_cannot_mark_effect_and_identity_is_immutable() {
    let (store, request) = fixture();
    store.prepare_remote_answer(&request).unwrap();
    let delivery = store.claim_remote_answer(Uuid::new_v4()).unwrap().unwrap();
    publish(&store, delivery.session_id(), 2);
    code(
        store.set_remote_answer_delivery(&delivery, RemoteAnswerStateV1::Running, true, None),
        "decision_changed",
    );
    assert!(
        store
            .conn
            .execute(
                "UPDATE remote_answer_deliveries SET answer='replacement'",
                []
            )
            .is_err()
    );
    assert!(
        !store
            .remote_answer_delivery(delivery.key())
            .unwrap()
            .unwrap()
            .effect_started
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn manager_inbox_question_refuses_new_remote_keys_but_retains_existing_receipt_replay() {
    for accepted_before_inbox in [false, true] {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = crate::store::manager_coordinator::tests::fixture(
            &store,
            rsi_common::harness_manager_v2::ManagerPolicyV2::default(),
        );
        crate::store::manager_coordinator::tests::raise_question(&store, lead.id, 1);
        let target = store.manager_v2_question_target(lead.id).unwrap().unwrap();
        let (_, mut request) = fixture();
        request.project_id = WireUuid::new(config.project_id.to_string()).unwrap();
        request.session_id = WireUuid::new(lead.id.to_string()).unwrap();
        request.decision_id = Text::new(format!(
            "pending-question:{}",
            target["publication_id"].as_str().unwrap()
        ))
        .unwrap();
        request.expected_target_digest = Text::new(fingerprint(&target).unwrap()).unwrap();
        let receipt = if accepted_before_inbox {
            Some(store.prepare_remote_answer(&request).unwrap())
        } else {
            None
        };
        store
            .manager_v2_refresh_question_decisions(&config)
            .unwrap();
        if let Some(receipt) = receipt {
            assert_eq!(store.prepare_remote_answer(&request).unwrap(), receipt);
            request.idempotency_key = WireUuid::new(Uuid::new_v4().to_string()).unwrap();
        }
        code(store.prepare_remote_answer(&request), "decision_changed");
        assert_eq!(
            store
                .manager_v2_record(&config, "decision", &format!("question:{}", lead.id))
                .unwrap()
                .unwrap()
                .payload["status"],
            "pending"
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn remote_answer_history_refuses_purge_with_typed_retention_and_preserves_session() {
    let (store, request) = fixture();
    let receipt = store.prepare_remote_answer(&request).unwrap();
    let session = uuid(&request.session_id);
    assert!(
        matches!(store.purge_session(session).unwrap_err(), crate::error::DaemonError::InvalidParam(code)
        if code.starts_with("session_remote_answer_retained:"))
    );
    assert_eq!(store.get_session(session).unwrap().unwrap().id, session);
    assert_eq!(store.prepare_remote_answer(&request).unwrap(), receipt);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn strict_wire_rejects_unbounded_answer_nul_and_unknown_origin() {
    let (store, request) = fixture();
    let mut bad = request.clone();
    bad.answer = Text::new("answer\0suffix".into()).unwrap();
    code(store.prepare_remote_answer(&bad), "remote_answer_invalid");
    let mut wire = serde_json::to_value(&request).unwrap();
    wire["answer"] = json!("x".repeat(2049));
    assert!(serde_json::from_value::<AnswerPendingDecisionV1>(wire).is_err());
    let mut wire = serde_json::to_value(&request).unwrap();
    wire["origin"]["kind"] = json!("agent");
    assert!(serde_json::from_value::<AnswerPendingDecisionV1>(wire).is_err());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn remote_answer_migration_inventory_and_replay_preserve_existing_rows() {
    let store = Store::open_in_memory().unwrap();
    for (kind, name) in [
        ("table", "remote_answer_deliveries"),
        ("index", "remote_answer_deliveries_pending"),
        ("trigger", "remote_answer_deliveries_immutable"),
        ("trigger", "remote_answer_deliveries_forward"),
    ] {
        let present: bool = store
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type=?1 AND name=?2)",
                params![kind, name],
                |r| r.get(0),
            )
            .unwrap();
        assert!(present, "journal catalog object installed: {name}");
    }
    let session = crate::test_support::test_session(
        Uuid::new_v4(),
        std::path::PathBuf::from("/var/tmp/remote-migration"),
    );
    store.insert_session(&session).unwrap();
    let head = super::super::super::LATEST_SCHEMA_VERSION;
    super::super::super::tests::rewind_post_v121_tail_to(&store.conn, head - 1);
    store.init_schema().unwrap();
    assert_eq!(
        store.get_session(session.id).unwrap().unwrap().id,
        session.id
    );
    let count: i64 = store
        .conn
        .query_row("SELECT count(*) FROM remote_answer_deliveries", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
}
