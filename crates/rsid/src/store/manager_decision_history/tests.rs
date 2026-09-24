use super::*;
use crate::store::manager_coordinator::{
    ManagerDecisionDeliveryV2,
    tests::{fixture, raise_question},
};
use crate::store::manager_ledger::LedgerObservation;
use rsi_common::harness_manager_v2::*;
use rusqlite::{Transaction, TransactionBehavior};
use std::collections::BTreeSet;

fn answer(
    store: &Store,
    config: &HarnessManagerConfigV1,
    key: &str,
    idempotency: &str,
) -> AnswerHarnessManagerDecisionRequestV2 {
    let record = store
        .manager_v2_record(config, "decision", key)
        .unwrap()
        .unwrap();
    AnswerHarnessManagerDecisionRequestV2 {
        project_id: config.project_id,
        fence: ManagerFenceV2 {
            scope_version: config.row_version,
            policy_version: 1,
        },
        decision_key: key.into(),
        expected_row_version: record.row_version,
        target_digest: record.payload["target_digest"].as_str().unwrap().into(),
        answer: "Proceed with this exact target".into(),
        idempotency_key: idempotency.into(),
    }
}

#[test]
fn manager_decision_history_preserves_all_pages_and_routes_new_answers_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.db");
    let store = Store::open(&path).unwrap();
    let (config, lead) = fixture(
        &store,
        ManagerPolicyV2 {
            capabilities: vec![ManagerCapabilityV2::WorkPlan],
            ..Default::default()
        },
    );
    let mut expected = BTreeSet::new();
    // Seed retained projection/journal history, not fake live writer authority.
    // A real pending question below exercises current operator admission/claim.
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();
    for n in 0..1030 {
        let publication = Uuid::new_v4();
        let key = format!("approval:{publication}");
        expected.insert(key.clone());
        let target =
            json!({"kind":"appserver_approval","session_id":lead.id,"publication_id":publication});
        store
            .manager_v2_record_changed(&config, "decision_target", &key, lead.parent_id, &target)
            .unwrap();
        store.manager_v2_record_changed(&config,"decision",&key,lead.parent_id,&json!({"key":key,"epic_id":lead.parent_id,"session_id":lead.id,"question":format!("Retained approval {n}"),"status":"resolved","target_digest":format!("history-{n}"),"answer":null,"provider_request":target})).unwrap();
        let delivery = ManagerDecisionDeliveryV2 {
            project_id: config.project_id,
            manager_session_id: config.manager_session_id,
            scope_version: config.row_version,
            policy_version: 1,
            key: Uuid::new_v4().to_string(),
            decision_key: key,
            epic_id: lead.parent_id,
            target_digest: format!("history-{n}"),
            target,
            answer: "approve".into(),
            state: "enqueued".into(),
            effect_started: true,
            boot_id: None,
            outcome: Some("Consumption unconfirmed".into()),
        };
        store
            .manager_v2_put_record(
                &config,
                "decision_delivery",
                &delivery.key,
                lead.parent_id,
                0,
                &serde_json::to_value(&delivery).unwrap(),
            )
            .unwrap();
        store.manager_v2_put_record(&config,"decision_retrieval",&format!("manager:approval:{publication}"),lead.parent_id,0,&json!({"target_digest":format!("history-{n}"),"actor_session_id":config.manager_session_id})).unwrap();
    }
    tx.commit().unwrap();
    // Metadata still uses the real agent contract and remains writable.
    store
        .manager_v2_commit_update(
            config.manager_session_id,
            &AgentManagerUpdateRequestV2 {
                fence: ManagerFenceV2 {
                    scope_version: config.row_version,
                    policy_version: 1,
                },
                idempotency_key: "operator-question".into(),
                change: ManagerUpdateV2::Decision {
                    key: "operator-next".into(),
                    expected_row_version: 0,
                    epic_id: lead.parent_id.unwrap(),
                    question: "Choose the next feature".into(),
                    request_id: None,
                    work_key: None,
                },
            },
            &LedgerObservation::default(),
        )
        .unwrap();
    let request = answer(&store, &config, "operator-next", "operator-choice");
    store
        .manager_v2_prepare_decision_answer(&request, |_, _, _, _| unreachable!())
        .unwrap();
    expected.insert("operator-next".into());
    raise_question(&store, lead.id, 1);
    store
        .manager_v2_refresh_question_decisions(&config)
        .unwrap();
    let question = format!("question:{}", lead.id);
    expected.insert(question.clone());
    drop(store);
    let store = Store::open(&path).unwrap();
    let signature = store
        .manager_v2_notice_signature(&config, &lead, true)
        .unwrap();
    store.manager_v2_reconcile_notices(&config).unwrap();
    store
        .manager_v2_refresh_question_decisions(&config)
        .unwrap();
    assert_eq!(
        store
            .manager_v2_notice_signature(&config, &lead, true)
            .unwrap(),
        signature
    );
    assert!(
        store
            .manager_v2_unread_operator_answer(&config, lead.parent_id.unwrap(), lead.id)
            .unwrap()
    );
    let mut query = AgentManagerInspectRequestV2 {
        section: ManagerInspectSectionV2::Decisions,
        limit: 31,
        ..Default::default()
    };
    let mut seen = BTreeSet::new();
    let mut operator_page = None;
    loop {
        let page = store
            .manager_v2_inspect_operator(config.project_id, &query)
            .unwrap();
        assert!(page.rows.len() <= 31);
        for row in &page.rows {
            let key = row["key"].as_str().unwrap().to_owned();
            assert!(
                seen.insert(key.clone()),
                "each retained identity appears once"
            );
            if key == question {
                assert_eq!(row["question"], "Use the migration allocation?");
            }
            if key == "operator-next" {
                operator_page = Some(page.clone());
            }
        }
        let Some(next) = page.next_cursor else {
            assert!(page.complete);
            break;
        };
        assert!(next.len() <= 512);
        // Routine semantic updates must not restart live decision traversal.
        store
            .manager_v2_record_changed(
                &config,
                "coordinator_error",
                "current",
                lead.parent_id,
                &json!({"page":seen.len()}),
            )
            .unwrap();
        query.cursor = Some(next);
        assert!(seen.len() <= expected.len());
    }
    assert_eq!(seen, expected);
    let mut page = operator_page.unwrap();
    page.rows.retain(|row| row["key"] == "operator-next");
    store
        .manager_v2_retrieve_decision_answers(lead.id, &mut page)
        .unwrap();
    assert!(
        !store
            .manager_v2_unread_operator_answer(&config, lead.parent_id.unwrap(), lead.id)
            .unwrap()
    );
    let request = answer(&store, &config, &question, "current-provider-answer");
    let receipt = store
        .manager_v2_prepare_decision_answer(&request, |_, _, _, _| unreachable!())
        .unwrap();
    let claimed = store
        .manager_v2_claim_decision_delivery(&config, Uuid::new_v4())
        .unwrap()
        .unwrap();
    assert_eq!(claimed.decision_key, question);
    assert_eq!(claimed.state, "running");
    assert!(
        store
            .manager_v2_prepare_decision_answer(&request, |_, _, _, _| unreachable!())
            .unwrap()
            .deduplicated
    );
    store
        .manager_v2_set_decision_delivery(
            &claimed,
            "uncertain",
            true,
            Some("Effect boundary unresolved".into()),
        )
        .unwrap();
    assert!(
        store
            .manager_v2_claim_decision_delivery(&config, Uuid::new_v4())
            .unwrap()
            .is_none()
    );
    assert!(receipt.event_sequence > 0);
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(
        reopened
            .manager_v2_record(&config, "decision_delivery", &claimed.key)
            .unwrap()
            .unwrap()
            .payload["state"],
        "uncertain"
    );
    assert!(
        reopened
            .manager_v2_claim_decision_delivery(&config, Uuid::new_v4())
            .unwrap()
            .is_none()
    );
}

#[test]
fn manager_decision_history_keeps_agent_metadata_quota_and_uses_current_queue_indexes() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();
    for n in 0..MANAGER_V2_MAX_RECORDS {
        store
            .manager_v2_put_record(
                &config,
                "quota_fixture",
                &n.to_string(),
                lead.parent_id,
                0,
                &json!({"entry":n}),
            )
            .unwrap();
    }
    assert!(
        store
            .manager_v2_put_record(
                &config,
                "quota_fixture",
                "extra",
                lead.parent_id,
                0,
                &json!({})
            )
            .unwrap_err()
            .to_string()
            .contains("record_limit")
    );
    store
        .manager_v2_record_changed(
            &config,
            "decision",
            "approval:internal",
            lead.parent_id,
            &json!({"question":"Retained provider gate","status":"resolved"}),
        )
        .unwrap();
    assert_eq!(
        store
            .manager_v2_decision_generation(&config, lead.parent_id)
            .unwrap(),
        1
    );
    tx.commit().unwrap();
    for (sql, index) in [
        (
            "EXPLAIN QUERY PLAN SELECT record_key FROM harness_manager_v2_records INDEXED BY harness_manager_v2_queued_answers WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND kind='decision_delivery' AND json_extract(payload_json,'$.state')='queued' ORDER BY record_key LIMIT 32",
            "harness_manager_v2_queued_answers",
        ),
        (
            "EXPLAIN QUERY PLAN SELECT record_key FROM harness_manager_v2_records INDEXED BY harness_manager_v2_operator_inbox WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND kind='decision' AND json_extract(payload_json,'$.delivery.state')='available_in_scoped_inbox' ORDER BY epic_id,record_key LIMIT 1",
            "harness_manager_v2_operator_inbox",
        ),
    ] {
        let mut stmt = store.conn.prepare(sql).unwrap();
        let plan = stmt
            .query_map(
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version
                ],
                |r| r.get::<_, String>(3),
            )
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(plan.len(), 1, "{plan:?}");
        assert!(plan[0].contains(index), "{plan:?}");
    }
}

#[path = "approval_scan_tests.rs"]
mod approval_scan_tests;
