use super::*;
use crate::model_control::{ExpectedUsage, InvocationCompletion, ModelAdmissionRequest};
use crate::store::StoreAdmissionOutcome;
#[cfg(test)]
use crate::store::manager_actions::ManagerActionOriginV2;
#[cfg(test)]
use crate::store::manager_coordinator::ManagerDecisionDeliveryV2;
pub use crate::store::manager_coordinator::tests::{fixture, raise_question};
#[cfg(test)]
use rsi_common::harness_manager_v2::*;
use rsi_common::model_control::{InvocationOwner, ModelInvocationPurpose, ModelTier};
#[cfg(test)]
use rsi_common::rpc::{ProviderRateLimitSnapshot, ProviderRateLimitWindow};

#[cfg(test)]
fn update_policy(
    store: &Store,
    config: &HarnessManagerConfigV1,
    edit: impl FnOnce(&mut ManagerPolicyV2),
) {
    let mut grant = store
        .get_harness_manager_policy(config.project_id)
        .unwrap()
        .unwrap();
    edit(&mut grant.policy);
    store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: config.project_id,
            expected_scope_version: config.row_version,
            expected_policy_version: grant.row_version,
            idempotency_key: Uuid::new_v4().to_string(),
            policy: grant.policy,
        })
        .unwrap();
}

#[cfg(test)]
fn group(store: &Store, config: &HarnessManagerConfigV1) -> Uuid {
    store
        .get_session(config.epic_ids[0])
        .unwrap()
        .unwrap()
        .parent_id
        .unwrap()
}

#[cfg(test)]
fn clone_session(store: &Store, base: &Session, parent: Uuid, kind: SessionKind) -> Session {
    let mut next = base.clone();
    next.id = Uuid::new_v4();
    next.parent_id = Some(parent);
    next.session_kind = kind;
    next.continued_from = None;
    next.rotation_depth = 0;
    next.cost_usd = Some(0.0);
    next.status = SessionStatus::Completed;
    store.insert_session(&next).unwrap();
    next
}

#[cfg(test)]
fn completed_rotation(store: &Store, origin: &Session, cost: f64) -> Session {
    let mut next = origin.clone();
    next.id = Uuid::new_v4();
    next.continued_from = Some(origin.id);
    next.rotation_depth += 1;
    next.status = SessionStatus::Completed;
    next.cost_usd = Some(cost);
    store.insert_session(&next).unwrap();
    store
        .update_session_status(origin.id, SessionStatus::Archived)
        .unwrap();
    assert!(
        store
            .record_harness_manager_rotation(origin.id, next.id)
            .unwrap()
    );
    next
}

#[cfg(test)]
fn attribute(store: &Store, config: &HarnessManagerConfigV1, session: &Session) {
    let policy = store
        .get_harness_manager_policy(config.project_id)
        .unwrap()
        .unwrap();
    let operation = store
        .manager_v2_save_receipt(
            config,
            None,
            policy.row_version,
            "test_creation",
            &Uuid::new_v4().to_string(),
            &json!({}),
            &json!({}),
        )
        .unwrap();
    store.conn.execute(
        "INSERT INTO harness_manager_v2_entities(session_id,operation_id,project_id,manager_session_id,scope_version,policy_version,kind,created_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![session.id.to_string(), operation.to_string(), config.project_id.to_string(), config.manager_session_id.to_string(),
            config.row_version, policy.row_version, serde_json::to_value(session.session_kind).unwrap().as_str().unwrap(), super::super::harness_manager_v2::now()]).unwrap();
}

fn admission_request(session: &Session) -> ModelAdmissionRequest {
    ModelAdmissionRequest {
        purpose: ModelInvocationPurpose::SessionLaunchFresh,
        provider: Some(format!("{:?}", session.provider)),
        model: Some("claude-sonnet-4-6".into()),
        backend: Some("Claude".into()),
        effort: None,
        trigger: "manager resource regression".into(),
        owner: InvocationOwner {
            session_id: Some(session.id),
            project_id: session.project_id,
            ..Default::default()
        },
        dedup_key: Some(Uuid::new_v4().to_string()),
        request_fingerprint: Some("sha256:manager-resource-test".into()),
        parent_invocation_id: None,
        retry_of_invocation_id: None,
        expected_usage: Some(ExpectedUsage {
            input_tokens: 100,
            output_tokens: 40,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            reasoning_tokens: 0,
            embedding_input_count: 0,
            wall_time_ms: 100,
        }),
        baseline_input_tokens: 0,
        baseline_output_tokens: 0,
        baseline_cache_creation_tokens: 0,
        baseline_cache_read_tokens: 0,
        baseline_reasoning_tokens: 0,
        baseline_embedding_input_count: 0,
        baseline_wall_time_ms: 0,
    }
}

fn admit(store: &Store, session: &Session) -> StoreAdmissionOutcome {
    let request = admission_request(session);
    store
        .admit_model_invocation(
            Uuid::new_v4(),
            *crate::model_control::registry::entry(request.purpose),
            ModelTier::Standard,
            &request,
        )
        .unwrap()
}

pub fn admitted(store: &Store, session: &Session) -> Uuid {
    match admit(store, session) {
        StoreAdmissionOutcome::Admitted(id) => id,
        other => panic!("expected admission, got {other:?}"),
    }
}

pub fn complete(store: &Store, invocation: Uuid, cost: f64) {
    store
        .complete_model_invocation(
            invocation,
            &InvocationCompletion {
                estimated_cost_usd: Some(cost),
                ..Default::default()
            },
        )
        .unwrap();
}

#[cfg(test)]
fn assert_denied(outcome: StoreAdmissionOutcome, code: &str) {
    assert!(
        matches!(outcome, StoreAdmissionOutcome::Denied { ref reason, .. } if reason.contains(code)),
        "{outcome:?}; expected {code}"
    );
}

#[cfg(test)]
fn queue_answer(
    store: &Store,
    config: &HarnessManagerConfigV1,
    session: Uuid,
) -> ManagerDecisionDeliveryV2 {
    raise_question(store, session, 1);
    store.manager_v2_refresh_question_decisions(config).unwrap();
    let decision = store
        .manager_v2_record(config, "decision", &format!("question:{session}"))
        .unwrap()
        .unwrap();
    let grant = store
        .get_harness_manager_policy(config.project_id)
        .unwrap()
        .unwrap();
    store
        .manager_v2_prepare_decision_answer(
            &AnswerHarnessManagerDecisionRequestV2 {
                project_id: config.project_id,
                fence: ManagerFenceV2 {
                    scope_version: config.row_version,
                    policy_version: grant.row_version,
                },
                decision_key: decision.key,
                expected_row_version: decision.row_version,
                target_digest: decision.payload["target_digest"].as_str().unwrap().into(),
                answer: "Proceed with this exact gate".into(),
                idempotency_key: Uuid::new_v4().to_string(),
            },
            |_, _, _, _| unreachable!("provider question is not a work acceptance"),
        )
        .unwrap();
    serde_json::from_value(
        store
            .manager_v2_records_of_kind(config, "decision_delivery")
            .unwrap()[0]
            .payload
            .clone(),
    )
    .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_historical_creation_cannot_authorize_reparented_answer_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scope.db");
    let store = Store::open_test_fixture(&path).unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    attribute(&store, &config, &lead);
    let delivery = queue_answer(&store, &config, lead.id);
    let other = clone_session(&store, &lead, group(&store, &config), SessionKind::Epic);
    store
        .update_session_parent(lead.id, Some(other.id))
        .unwrap();
    let cohort = store.manager_v2_cohort(&config).unwrap();
    assert_eq!(
        cohort
            .iter()
            .find(|m| m.session.id == lead.id)
            .unwrap()
            .epic_id,
        None
    );
    assert!(store.manager_v2_check_decision_target(&delivery).is_err());
    drop(store);
    let store = Store::open(&path).unwrap();
    assert!(store.manager_v2_check_decision_target(&delivery).is_err());
    assert!(
        store
            .manager_v2_claim_decision_delivery(&config, Uuid::new_v4())
            .unwrap()
            .is_none()
    );
    let journal = store
        .manager_v2_record(&config, "decision_delivery", &delivery.key)
        .unwrap()
        .unwrap();
    assert_eq!(journal.payload["state"], "revoked");
    assert_eq!(
        store.get_session(lead.id).unwrap().unwrap().parent_id,
        Some(other.id)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_answer_requires_live_epic_and_group() {
    for retire_group in [false, true] {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, ManagerPolicyV2::default());
        let delivery = queue_answer(&store, &config, lead.id);
        let container = if retire_group {
            group(&store, &config)
        } else {
            config.epic_ids[0]
        };
        store
            .update_session_status(container, SessionStatus::Archived)
            .unwrap();
        assert_eq!(
            store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
            1
        );
        assert!(store.manager_v2_check_decision_target(&delivery).is_err());
        assert!(
            store
                .manager_v2_claim_decision_delivery(&config, Uuid::new_v4())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .manager_v2_record(&config, "decision_delivery", &delivery.key)
                .unwrap()
                .unwrap()
                .payload["state"],
            "revoked"
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_answer_cannot_redirect_between_selected_epics() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let other = clone_session(&store, &lead, group(&store, &config), SessionKind::Epic);
    attribute(&store, &config, &other);
    let config = store
        .get_harness_manager(config.project_id)
        .unwrap()
        .unwrap();
    assert!(config.epic_ids.contains(&other.id));
    let delivery = queue_answer(&store, &config, lead.id);
    store
        .update_session_parent(lead.id, Some(other.id))
        .unwrap();
    assert_eq!(
        store
            .manager_v2_live_epic_for_session(&config, lead.id)
            .unwrap(),
        other.id
    );
    assert!(store.manager_v2_check_decision_target(&delivery).is_err());
    // Reading the board republishes the same actual question under its new
    // owning Epic. It must never freshen the old operator delivery's authority.
    store
        .manager_v2_refresh_question_decisions(&config)
        .unwrap();
    let moved = store
        .manager_v2_record(&config, "decision", &delivery.decision_key)
        .unwrap()
        .unwrap();
    assert_eq!(moved.epic_id, Some(other.id));
    assert_eq!(moved.payload["status"], "pending");
    assert!(store.manager_v2_check_decision_target(&delivery).is_err());
    assert!(
        store
            .manager_v2_claim_decision_delivery(&config, Uuid::new_v4())
            .unwrap()
            .is_none()
    );
    let current = store
        .manager_v2_record(&config, "decision", &delivery.decision_key)
        .unwrap()
        .unwrap();
    assert_eq!(current.row_version, moved.row_version);
    assert_eq!(current.payload["status"], "pending");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_group_rotation_needs_receipt_and_retains_capacity_pause_and_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rotation.db");
    let store = Store::open_test_fixture(&path).unwrap();
    let (config, lead) = fixture(
        &store,
        ManagerPolicyV2 {
            max_active_sessions: 1,
            ..Default::default()
        },
    );
    let root_group = group(&store, &config);
    update_policy(&store, &config, |p| p.group_ids = vec![root_group]);
    let mut origin = clone_session(&store, &lead, root_group, SessionKind::Standard);
    origin.cost_usd = Some(3.0);
    store
        .conn
        .execute(
            "UPDATE sessions SET cost_usd=3 WHERE id=?1",
            [origin.id.to_string()],
        )
        .unwrap();
    attribute(&store, &config, &origin);
    let mut next = origin.clone();
    next.id = Uuid::new_v4();
    next.continued_from = Some(origin.id);
    next.rotation_depth += 1;
    next.status = SessionStatus::Running;
    next.cost_usd = Some(2.0);
    store.insert_session(&next).unwrap();
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
        0
    );
    store
        .update_session_status(origin.id, SessionStatus::Archived)
        .unwrap();
    store
        .record_harness_manager_rotation(origin.id, next.id)
        .unwrap();
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
        1
    );
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["known_spend_usd"],
        5.0
    );
    assert_denied(admit(&store, &lead), "concurrency_capacity");
    store
        .manager_v2_resource_gate_for_session(next.id, next.provider)
        .unwrap();
    update_policy(&store, &config, |p| p.paused = true);
    assert_denied(admit(&store, &next), "policy_paused");
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_denied(admit(&store, &next), "policy_paused");
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["known_spend_usd"],
        5.0
    );
    // Restoring a predecessor retires its authority receipt, not its past bill.
    store
        .update_session_status(origin.id, SessionStatus::Completed)
        .unwrap();
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
        1
    );
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["known_spend_usd"],
        5.0
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_rotation_attribution_has_a_fixed_walk_budget() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let mut previous = clone_session(&store, &lead, group(&store, &config), SessionKind::Standard);
    attribute(&store, &config, &previous);
    for _ in 0..=ROTATION_LIMIT {
        let mut next = previous.clone();
        next.id = Uuid::new_v4();
        next.continued_from = Some(previous.id);
        next.rotation_depth += 1;
        store.insert_session(&next).unwrap();
        store
            .update_session_status(previous.id, SessionStatus::Archived)
            .unwrap();
        store
            .record_harness_manager_rotation(previous.id, next.id)
            .unwrap();
        previous = next;
    }
    assert!(
        store
            .manager_v2_resource_snapshot(&config)
            .unwrap_err()
            .to_string()
            .contains("manager_v2_cohort_subdivision_required")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
#[allow(clippy::unwrap_used)] // Fixture setup and spend assertions must fail immediately.
fn manager_v2_resources_unattributed_rotation_admission_skips_spend_walk() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let mut current = clone_session(&store, &lead, group(&store, &config), SessionKind::Standard);
    for _ in 0..=ROTATION_LIMIT {
        current = completed_rotation(&store, &current, 0.0);
    }

    let spend_before =
        store.manager_v2_resource_snapshot(&config).unwrap()["known_spend_usd"].clone();
    let invocation = admitted(&store, &current);
    complete(&store, invocation, 1.0);
    let spend_after =
        store.manager_v2_resource_snapshot(&config).unwrap()["known_spend_usd"].clone();

    assert_eq!(spend_after, spend_before);
    assert!(
        store
            .manager_v2_record(&config, SPEND_KIND, &current.id.to_string())
            .unwrap()
            .is_none()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
#[allow(clippy::unwrap_used)] // Fixture setup and exact attribution assertions must fail immediately.
fn manager_v2_resources_rotation_prefilter_includes_selected_epic_parent_attribution() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let origin = clone_session(&store, &lead, config.epic_ids[0], SessionKind::Standard);
    assert_eq!(origin.parent_id, Some(config.epic_ids[0]));
    let successor = completed_rotation(&store, &origin, 0.0);

    assert!(
        store
            .manager_v2_rotation_lineage_has_spend_attribution(&config, &successor)
            .unwrap()
    );
    assert!(
        store
            .manager_v2_rotation_spend_attributed(&config, &successor)
            .unwrap()
    );
    store
        .manager_v2_capture_resource_spend(successor.id)
        .unwrap();
    assert!(
        store
            .manager_v2_record(&config, SPEND_KIND, &successor.id.to_string())
            .unwrap()
            .is_some()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
#[allow(clippy::unwrap_used)] // Fixture setup and result inspection must fail immediately.
fn manager_v2_resources_rejects_changed_rotation_attribution_and_cycle() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let previous = clone_session(&store, &lead, group(&store, &config), SessionKind::Standard);
    attribute(&store, &config, &previous);
    let mut next = previous.clone();
    next.id = Uuid::new_v4();
    next.continued_from = Some(previous.id);
    next.rotation_depth += 1;
    store.insert_session(&next).unwrap();
    store
        .update_session_status(previous.id, SessionStatus::Archived)
        .unwrap();
    store
        .record_harness_manager_rotation(previous.id, next.id)
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE sessions SET continued_from=NULL WHERE id=?1",
            [next.id.to_string()],
        )
        .unwrap();
    let error = store.manager_v2_resource_snapshot(&config).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_rotation_attribution_changed"),
        "{error}"
    );

    store
        .conn
        .execute(
            "UPDATE sessions SET continued_from=?2 WHERE id=?1",
            params![next.id.to_string(), previous.id.to_string()],
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO harness_manager_rotation_edges(predecessor_session_id,successor_session_id,committed_at)
             VALUES(?1,?2,?3)",
            params![next.id.to_string(), previous.id.to_string(), Utc::now().to_rfc3339()],
        )
        .unwrap();
    assert!(
        store
            .manager_v2_resource_snapshot(&config)
            .unwrap_err()
            .to_string()
            .contains("manager_v2_cohort_cycle")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
#[allow(clippy::unwrap_used)] // Fixture setup and result inspection must fail immediately.
fn manager_v2_resources_resumed_completed_descendant_keeps_legacy_spend_floor() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let worker = clone_session(&store, &lead, config.epic_ids[0], SessionKind::Task);
    store
        .conn
        .execute(
            "UPDATE sessions SET cost_usd=5 WHERE id=?1",
            [worker.id.to_string()],
        )
        .unwrap();
    let resumed = admitted(&store, &worker);
    complete(&store, resumed, 1.0);
    store
        .conn
        .execute(
            "UPDATE sessions SET cost_usd=1 WHERE id=?1",
            [worker.id.to_string()],
        )
        .unwrap();
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["known_spend_usd"],
        5.0
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
#[allow(clippy::unwrap_used)] // Exact fixture setup and spend assertions must fail immediately.
fn manager_v2_resources_group_standard_rotation_successor_keeps_spend_floor() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let origin = clone_session(&store, &lead, group(&store, &config), SessionKind::Standard);
    attribute(&store, &config, &origin);
    let first_successor = completed_rotation(&store, &origin, 0.0);
    let successor = completed_rotation(&store, &first_successor, 5.0);
    let invocation = admitted(&store, &successor);
    complete(&store, invocation, 1.0);
    store
        .conn
        .execute(
            "UPDATE sessions SET cost_usd=1 WHERE id=?1",
            [successor.id.to_string()],
        )
        .unwrap();
    let checkpoint = store
        .manager_v2_record(&config, SPEND_KIND, &successor.id.to_string())
        .unwrap()
        .unwrap();
    assert_eq!(checkpoint.payload["known_floor_usd"], 5.0);
    assert!(
        store.manager_v2_resource_snapshot(&config).unwrap()["known_spend_usd"]
            .as_f64()
            .unwrap()
            >= 5.0
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
#[allow(clippy::unwrap_used)] // Exact fixture setup and spend assertions must fail immediately.
fn manager_v2_resources_root_context_rotation_successor_keeps_spend_floor() {
    let store = Store::open_in_memory().unwrap();
    let (config, _) = fixture(&store, ManagerPolicyV2::default());
    let manager = store
        .get_session(config.manager_session_id)
        .unwrap()
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO manager_root_resource_origins
             (session_id,project_id,manager_session_id,scope_version,provider,zero_origin,known_floor_usd,created_at)
             VALUES(?1,?2,?1,?3,?4,0,0,?5)",
            params![
                manager.id.to_string(),
                config.project_id.to_string(),
                config.row_version,
                serde_json::to_value(manager.provider).unwrap().as_str().unwrap(),
                super::super::harness_manager_v2::now()
            ],
        )
        .unwrap();
    let successor = completed_rotation(&store, &manager, 5.0);
    let current = store
        .get_harness_manager(config.project_id)
        .unwrap()
        .unwrap();
    let invocation = admitted(&store, &successor);
    complete(&store, invocation, 1.0);
    store
        .conn
        .execute(
            "UPDATE sessions SET cost_usd=1 WHERE id=?1",
            [successor.id.to_string()],
        )
        .unwrap();
    let checkpoint = store
        .manager_v2_record(&current, SPEND_KIND, &successor.id.to_string())
        .unwrap()
        .unwrap();
    assert_eq!(checkpoint.payload["known_floor_usd"], 5.0);
    assert!(
        store.manager_v2_resource_snapshot(&current).unwrap()["known_spend_usd"]
            .as_f64()
            .unwrap()
            >= 5.0
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
#[allow(clippy::unwrap_used)] // Exact fixture setup and negative attribution assertions must fail immediately.
fn manager_v2_resources_unreceipted_continuation_gains_no_spend_floor() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let origin = clone_session(&store, &lead, group(&store, &config), SessionKind::Standard);
    attribute(&store, &config, &origin);
    let before = store.manager_v2_resource_snapshot(&config).unwrap()["known_spend_usd"].clone();
    let mut unrelated = origin.clone();
    unrelated.id = Uuid::new_v4();
    unrelated.continued_from = Some(origin.id);
    unrelated.rotation_depth += 1;
    unrelated.cost_usd = Some(5.0);
    store.insert_session(&unrelated).unwrap();
    let invocation = admitted(&store, &unrelated);
    complete(&store, invocation, 1.0);
    assert!(
        store
            .manager_v2_record(&config, SPEND_KIND, &unrelated.id.to_string())
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["known_spend_usd"],
        before
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
#[allow(clippy::unwrap_used)] // Fixture setup and result inspection must fail immediately.
fn manager_v2_resources_refreshes_terminal_question_decision() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let worker = clone_session(&store, &lead, config.epic_ids[0], SessionKind::Task);
    store
        .update_session_status(worker.id, SessionStatus::Running)
        .unwrap();
    raise_question(&store, worker.id, 1);
    store
        .manager_v2_refresh_question_decisions(&config)
        .unwrap();
    let key = format!("question:{}", worker.id);
    assert_eq!(
        store
            .manager_v2_record(&config, "decision", &key)
            .unwrap()
            .unwrap()
            .payload["status"],
        "pending"
    );
    store
        .update_session_status(worker.id, SessionStatus::Completed)
        .unwrap();
    store
        .manager_v2_refresh_question_decisions(&config)
        .unwrap();
    assert_eq!(
        store
            .manager_v2_record(&config, "decision", &key)
            .unwrap()
            .unwrap()
            .payload["status"],
        "target_unavailable"
    );
    store
        .update_session_status(worker.id, SessionStatus::Archived)
        .unwrap();
    store
        .manager_v2_refresh_question_decisions(&config)
        .unwrap();
    assert_eq!(
        store
            .manager_v2_record(&config, "decision", &key)
            .unwrap()
            .unwrap()
            .payload["status"],
        "scope_revoked"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
#[allow(clippy::unwrap_used)] // Fixture setup and result inspection must fail immediately.
fn manager_v2_resources_refreshes_archived_approval_decision() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let worker = clone_session(&store, &lead, config.epic_ids[0], SessionKind::Task);
    let publication = Uuid::new_v4();
    let incarnation = Uuid::new_v4();
    let key = format!("approval:{publication}");
    let target = json!({"kind":"appserver_approval","session_id":worker.id,
        "publication_id":publication,"incarnation_id":incarnation,"request_id":1,
        "method":"item/commandExecution/requestApproval","params":{}});
    let stamp = Utc::now().to_rfc3339();
    store
        .conn
        .execute(
            "INSERT INTO approvals(id,session_id,tool_name,tool_input,status,created_at)
         VALUES(?1,?2,'approval',?3,'Pending',?4)",
            params![
                publication.to_string(),
                worker.id.to_string(),
                target.to_string(),
                stamp
            ],
        )
        .unwrap();
    store.conn.execute(
        "INSERT INTO appserver_approval_publications
         (publication_id,session_id,incarnation_id,request_id_json,approval_id,state,target_json,created_at,updated_at)
         VALUES(?1,?2,?3,'1',?1,'unresolved',?4,?5,?5)",
        params![publication.to_string(), worker.id.to_string(), incarnation.to_string(),
            target.to_string(), stamp],
    ).unwrap();
    store
        .manager_v2_record_changed(
            &config,
            "decision",
            &key,
            Some(config.epic_ids[0]),
            &json!({"key":key,"session_id":worker.id,"status":"pending"}),
        )
        .unwrap();
    store
        .manager_v2_refresh_approval_decisions(&config)
        .unwrap();
    let record = store
        .manager_v2_record(&config, "decision", &key)
        .unwrap()
        .unwrap();
    assert_eq!(record.payload["status"], "blocked");
    assert_eq!(record.epic_id, Some(config.epic_ids[0]));
    store
        .update_session_status(worker.id, SessionStatus::Archived)
        .unwrap();
    store
        .manager_v2_refresh_approval_decisions(&config)
        .unwrap();
    let record = store
        .manager_v2_record(&config, "decision", &key)
        .unwrap()
        .unwrap();
    assert_eq!(record.payload["status"], "blocked");
    assert_eq!(record.epic_id, None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_cancellation_holds_capacity_until_actual_settlement_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("capacity.db");
    let store = Store::open_test_fixture(&path).unwrap();
    let (config, lead) = fixture(
        &store,
        ManagerPolicyV2 {
            max_active_sessions: 1,
            provider_limits: vec![ManagerProviderLimitV2 {
                provider: SessionProvider::Claude,
                max_active: 1,
            }],
            ..Default::default()
        },
    );
    let other = clone_session(&store, &lead, config.epic_ids[0], SessionKind::Task);
    let invocation = admitted(&store, &lead);
    store
        .request_model_invocation_cancellation(invocation, "cancel", "test")
        .unwrap();
    assert_eq!(
        store.get_session(lead.id).unwrap().unwrap().status,
        SessionStatus::Completed
    );
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["active_by_provider"]
            [format!("{:?}", lead.provider)],
        1
    );
    assert_denied(admit(&store, &other), "concurrency_capacity");
    update_policy(&store, &config, |p| {
        p.max_active_sessions = 4;
        p.provider_limits = vec![ManagerProviderLimitV2 {
            provider: lead.provider,
            max_active: 1,
        }];
    });
    assert_denied(admit(&store, &other), "provider_capacity");
    update_policy(&store, &config, |p| p.max_active_sessions = 1);
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
        1
    );
    assert_denied(admit(&store, &other), "concurrency_capacity");
    complete(&store, invocation, 0.0);
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
        0
    );
    admitted(&store, &other);
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
        1
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_first_invocation_preserves_legacy_floor_and_unknown_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spend.db");
    let store = Store::open_test_fixture(&path).unwrap();
    let (config, lead) = fixture(
        &store,
        ManagerPolicyV2 {
            max_spend_usd: Some(11.0),
            ..Default::default()
        },
    );
    store
        .conn
        .execute(
            "UPDATE sessions SET cost_usd=10 WHERE id=?1",
            [lead.id.to_string()],
        )
        .unwrap();
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["known_spend_usd"],
        10.0
    );
    let invocation = admitted(&store, &lead);
    complete(&store, invocation, 1.0);
    // A provider's next estimate need not include pre-ledger history.
    store
        .conn
        .execute(
            "UPDATE sessions SET cost_usd=1 WHERE id=?1",
            [lead.id.to_string()],
        )
        .unwrap();
    let snapshot = store.manager_v2_resource_snapshot(&config).unwrap();
    assert_eq!(snapshot["known_spend_usd"], 10.0);
    assert_eq!(snapshot["unknown_spend_observations"], 1);
    update_policy(&store, &config, |p| p.max_spend_usd = Some(100.0));
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["known_spend_usd"],
        10.0
    );
    assert_denied(admit(&store, &lead), "spend_unknown");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_proven_zero_origin_allows_measured_ledger_cost_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zero-origin.db");
    let store = Store::open_test_fixture(&path).unwrap();
    let (config, lead) = fixture(
        &store,
        ManagerPolicyV2 {
            max_spend_usd: Some(2.0),
            ..Default::default()
        },
    );
    let invocation = admitted(&store, &lead);
    complete(&store, invocation, 1.0);
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["known_spend_usd"],
        1.0
    );
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["unknown_spend_observations"],
        0
    );
    update_policy(&store, &config, |p| p.retry_delay_seconds += 1);
    drop(store);
    let store = Store::open(&path).unwrap();
    let next = admitted(&store, &lead);
    // The row holds the session's running total (#2593): 1.0 + 1.0.
    complete(&store, next, 2.0);
    assert_denied(admit(&store, &lead), "spend_exhausted");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_preexisting_telemetry_does_not_prove_legacy_coverage() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(
        &store,
        ManagerPolicyV2 {
            max_spend_usd: Some(100.0),
            ..Default::default()
        },
    );
    let outside = clone_session(&store, &lead, group(&store, &config), SessionKind::Epic);
    let worker = clone_session(&store, &lead, outside.id, SessionKind::Task);
    store
        .conn
        .execute(
            "UPDATE sessions SET cost_usd=5 WHERE id=?1",
            [worker.id.to_string()],
        )
        .unwrap();
    let invocation = admitted(&store, &worker);
    complete(&store, invocation, 1.0);
    store
        .update_session_parent(worker.id, Some(config.epic_ids[0]))
        .unwrap();
    let snapshot = store.manager_v2_resource_snapshot(&config).unwrap();
    assert_eq!(snapshot["known_spend_usd"], 5.0);
    assert_eq!(snapshot["unknown_spend_observations"], 1);
    assert_denied(admit(&store, &worker), "spend_unknown");
}

#[cfg(test)]
fn create_action(
    store: &Store,
    config: &HarnessManagerConfigV1,
    launch: ManagerLaunchChoiceV2,
) -> Result<ManagerActionReceiptV2> {
    let policy = store
        .get_harness_manager_policy(config.project_id)?
        .unwrap();
    store.enqueue_manager_action(
        ManagerActionOriginV2::Agent {
            caller: config.manager_session_id,
        },
        AgentManagerControlRequestV2 {
            ..AgentManagerControlRequestV2::new(
                ManagerFenceV2 {
                    scope_version: config.row_version,
                    policy_version: policy.row_version,
                },
                Uuid::new_v4().to_string(),
                ManagerActionV2::CreateSession {
                    parent_id: config.epic_ids[0],
                    kind: SessionKind::Task,
                    query: "Scoped resource launch".into(),
                    launch,
                    sandbox_source: None,
                },
            )
        },
    )
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_actions_share_scoped_capacity_cost_unknowns_and_provider_windows() {
    let store = Store::open_in_memory().unwrap();
    let launch = ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "claude-sonnet-4-6".into(),
        effort: None,
    };
    let (config, lead) = fixture(
        &store,
        ManagerPolicyV2 {
            capabilities: vec![ManagerCapabilityV2::SessionCreate],
            max_created_sessions: 10,
            max_active_sessions: 1,
            max_spend_usd: Some(10.0),
            allowed_launches: vec![launch.clone()],
            ..Default::default()
        },
    );
    let repo = tempfile::tempdir().unwrap();
    for args in [
        vec!["init", "--quiet"],
        vec![
            "-c",
            "user.name=Resource Test",
            "-c",
            "user.email=resource@example.test",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    ] {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    store
        .conn
        .execute(
            "UPDATE sessions SET working_dir=?1 WHERE project_id=?2",
            params![repo.path().to_str().unwrap(), config.project_id.to_string()],
        )
        .unwrap();
    // A manager create resolves its worker source from the target project's
    // registered repository (#617), not from the parent session's directory.
    store
        .conn
        .execute(
            "UPDATE projects SET path=?1 WHERE id=?2",
            params![repo.path().to_str().unwrap(), config.project_id.to_string()],
        )
        .unwrap();
    let outside = clone_session(&store, &lead, group(&store, &config), SessionKind::Epic);
    let unrelated = clone_session(&store, &lead, outside.id, SessionKind::Task);
    store
        .conn
        .execute(
            "UPDATE sessions SET cost_usd=10000,status='Running' WHERE id=?1",
            [unrelated.id.to_string()],
        )
        .unwrap();
    let queued = create_action(&store, &config, launch.clone()).unwrap();
    assert_eq!(queued.state, ManagerActionStateV2::Queued);
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["known_spend_usd"],
        0.0
    );
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["pending_operations"],
        1
    );
    let mut ungranted = launch.clone();
    ungranted.model = "ungranted-model".into();
    assert!(
        create_action(&store, &config, ungranted)
            .unwrap_err()
            .to_string()
            .contains("launch_not_granted")
    );
    let invocation = admitted(&store, &lead);
    store
        .request_model_invocation_cancellation(invocation, "cancel", "test")
        .unwrap();
    assert!(
        create_action(&store, &config, launch.clone())
            .unwrap_err()
            .to_string()
            .contains("concurrency_capacity")
    );
    complete(&store, invocation, 0.0);
    store
        .upsert_provider_rate_limit_snapshot(
            &ProviderRateLimitSnapshot {
                provider: launch.provider,
                status: None,
                rate_limit_type: None,
                overage_status: None,
                is_using_overage: false,
                observed_at: Utc::now(),
                windows: vec![ProviderRateLimitWindow {
                    window_key: "five_hour".into(),
                    utilization: 1.0,
                    resets_at_epoch: Some(Utc::now().timestamp() + 3600),
                }],
                stale_reason: None,
            },
            None,
        )
        .unwrap();
    assert!(
        create_action(&store, &config, launch.clone())
            .unwrap_err()
            .to_string()
            .contains("provider_usage_limit")
    );
    // An unmeasured scoped worker blocks a finite budget even with no invocation ID.
    let unknown = clone_session(&store, &lead, config.epic_ids[0], SessionKind::Task);
    store
        .conn
        .execute(
            "UPDATE sessions SET cost_usd=NULL WHERE id=?1",
            [unknown.id.to_string()],
        )
        .unwrap();
    assert!(
        create_action(&store, &config, launch)
            .unwrap_err()
            .to_string()
            .contains("spend_unknown")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_rotation_reserves_capacity_before_successor_publication() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rotation-admission.db");
    let store = Store::open_test_fixture(&path).unwrap();
    let (config, lead) = fixture(
        &store,
        ManagerPolicyV2 {
            max_active_sessions: 1,
            ..Default::default()
        },
    );
    let source = clone_session(&store, &lead, group(&store, &config), SessionKind::Standard);
    attribute(&store, &config, &source);
    let parent = admitted(&store, &source);
    complete(&store, parent, 0.0);
    let mut candidate = source.clone();
    candidate.id = Uuid::new_v4();
    candidate.continued_from = Some(source.id);
    candidate.rotation_depth += 1;
    let mut request = admission_request(&candidate);
    request.purpose = ModelInvocationPurpose::SessionRotateChild;
    request.parent_invocation_id = Some(parent);
    let registry = *crate::model_control::registry::entry(request.purpose);
    update_policy(&store, &config, |p| p.paused = true);
    assert_denied(
        store
            .admit_model_invocation(Uuid::new_v4(), registry, ModelTier::Standard, &request)
            .unwrap(),
        "policy_paused",
    );
    update_policy(&store, &config, |p| p.paused = false);
    request.dedup_key = Some(Uuid::new_v4().to_string());
    let invocation = Uuid::new_v4();
    assert_eq!(
        store
            .admit_model_invocation(invocation, registry, ModelTier::Standard, &request)
            .unwrap(),
        StoreAdmissionOutcome::Admitted(invocation)
    );
    assert!(store.get_session(candidate.id).unwrap().is_none());
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
        1
    );
    assert_denied(admit(&store, &lead), "concurrency_capacity");
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
        1
    );
    assert_denied(admit(&store, &lead), "concurrency_capacity");
    // While the candidate row is present but custody is not yet committed,
    // nested admissions still consume its existing scoped reservation.
    store.insert_session(&candidate).unwrap();
    store
        .manager_v2_resource_gate_for_session(candidate.id, candidate.provider)
        .unwrap();
    update_policy(&store, &config, |p| p.paused = true);
    assert_denied(admit(&store, &candidate), "policy_paused");
    complete(&store, invocation, 0.0);
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
        0
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_unproved_rotation_is_unknown_but_proven_unrelated_origin_is_unscoped() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(
        &store,
        ManagerPolicyV2 {
            max_active_sessions: 1,
            ..Default::default()
        },
    );
    let outside = clone_session(&store, &lead, group(&store, &config), SessionKind::Epic);
    let source = clone_session(&store, &lead, outside.id, SessionKind::Task);
    let parent = admitted(&store, &source);
    complete(&store, parent, 0.0);
    store
        .update_session_status(lead.id, SessionStatus::Running)
        .unwrap();
    let mut candidate = source.clone();
    candidate.id = Uuid::new_v4();
    let mut request = admission_request(&candidate);
    request.purpose = ModelInvocationPurpose::SessionRotateChild;
    let registry = *crate::model_control::registry::entry(request.purpose);
    assert_denied(
        store
            .admit_model_invocation(Uuid::new_v4(), registry, ModelTier::Standard, &request)
            .unwrap(),
        "rotation_origin_unknown",
    );
    request.dedup_key = Some(Uuid::new_v4().to_string());
    request.parent_invocation_id = Some(parent);
    let invocation = Uuid::new_v4();
    assert_eq!(
        store
            .admit_model_invocation(invocation, registry, ModelTier::Standard, &request)
            .unwrap(),
        StoreAdmissionOutcome::Admitted(invocation)
    );
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
        1
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_concurrent_store_admissions_reserve_one_scoped_slot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("race.db");
    let store = Store::open_test_fixture(&path).unwrap();
    let (config, first) = fixture(
        &store,
        ManagerPolicyV2 {
            max_active_sessions: 1,
            ..Default::default()
        },
    );
    let second = clone_session(&store, &first, config.epic_ids[0], SessionKind::Task);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let joins: Vec<_> = [first, second]
        .into_iter()
        .map(|session| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = Store::open(&path).unwrap();
                barrier.wait();
                admit(&store, &session)
            })
        })
        .collect();
    let results: Vec<_> = joins.into_iter().map(|j| j.join().unwrap()).collect();
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, StoreAdmissionOutcome::Admitted(_)))
            .count(),
        1
    );
    assert_eq!(results.iter().filter(|r| matches!(r, StoreAdmissionOutcome::Denied { reason, .. } if reason.contains("concurrency_capacity"))).count(), 1);
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
        1
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_denied_attempt_cannot_certify_later_untracked_history_as_zero() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(
        &store,
        ManagerPolicyV2 {
            paused: true,
            max_spend_usd: Some(10.0),
            ..Default::default()
        },
    );
    assert_denied(admit(&store, &lead), "policy_paused");
    // A delayed legacy estimate arrives before there is an admitted ledger row.
    store
        .conn
        .execute(
            "UPDATE sessions SET cost_usd=3 WHERE id=?1",
            [lead.id.to_string()],
        )
        .unwrap();
    update_policy(&store, &config, |p| p.paused = false);
    let invocation = admitted(&store, &lead);
    complete(&store, invocation, 1.0);
    store
        .conn
        .execute(
            "UPDATE sessions SET cost_usd=1 WHERE id=?1",
            [lead.id.to_string()],
        )
        .unwrap();
    let snapshot = store.manager_v2_resource_snapshot(&config).unwrap();
    assert_eq!(snapshot["known_spend_usd"], 3.0);
    assert_eq!(snapshot["unknown_spend_observations"], 1);
    assert_denied(admit(&store, &lead), "spend_unknown");
}

#[cfg(test)]
fn unpublished_child(store: &Store, base: &Session) -> (Session, ManagerResourceLaunchOrigin) {
    let mut child = base.clone();
    child.id = Uuid::new_v4();
    child.session_kind = SessionKind::Task;
    child.cost_usd = Some(0.0);
    let origin = store
        .manager_v2_launch_origin(
            child.id,
            child.parent_id.unwrap(),
            child.project_id,
            child.session_kind,
        )
        .unwrap();
    (child, origin)
}

#[cfg(test)]
fn admit_unpublished(
    store: &Store,
    child: &Session,
    origin: &ManagerResourceLaunchOrigin,
) -> StoreAdmissionOutcome {
    let request = admission_request(child);
    store
        .admit_model_invocation_with_launch_origin(
            Uuid::new_v4(),
            *crate::model_control::registry::entry(request.purpose),
            ModelTier::Standard,
            &request,
            Some(origin),
        )
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_unpublished_permit_replay_rechecks_current_policy() {
    let store = Store::open_in_memory().unwrap();
    let (scope, lead) = fixture(
        &store,
        ManagerPolicyV2 {
            max_active_sessions: 1,
            provider_limits: vec![ManagerProviderLimitV2 {
                provider: SessionProvider::Claude,
                max_active: 1,
            }],
            ..Default::default()
        },
    );
    let (child, origin) = unpublished_child(&store, &lead);
    let request = admission_request(&child);
    let attempt = || {
        store.admit_model_invocation_with_launch_origin(
            Uuid::new_v4(),
            *crate::model_control::registry::entry(request.purpose),
            ModelTier::Standard,
            &request,
            Some(&origin),
        )
    };
    let StoreAdmissionOutcome::Admitted(id) = attempt().unwrap() else {
        panic!("first admission");
    };
    assert!(
        matches!(attempt().unwrap(), StoreAdmissionOutcome::Duplicate(replayed) if replayed == id)
    );
    assert!(store.get_session(child.id).unwrap().is_none());
    assert_eq!(
        store.manager_v2_resource_snapshot(&scope).unwrap()["active_sessions"],
        1
    );
    update_policy(&store, &scope, |p| p.paused = true);
    assert!(attempt().unwrap_err().to_string().contains("policy_paused"));
    assert_eq!(
        store.manager_v2_resource_snapshot(&scope).unwrap()["active_sessions"],
        1
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_fresh_unpublished_admission_retains_capacity_and_cost_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unpublished.db");
    let store = Store::open_test_fixture(&path).unwrap();
    let (config, lead) = fixture(
        &store,
        ManagerPolicyV2 {
            max_active_sessions: 1,
            max_spend_usd: Some(10.0),
            ..Default::default()
        },
    );
    // Preserve an established zero-cost history for the fixture lead. Reopen
    // otherwise seeds a legacy telemetry row, which correctly remains unknown.
    let lead_invocation = admitted(&store, &lead);
    complete(&store, lead_invocation, 0.0);
    store
        .set_session_model_invocation(lead.id, Some(lead_invocation))
        .unwrap();
    let (first, origin) = unpublished_child(&store, &lead);
    let (second, second_origin) = unpublished_child(&store, &lead);
    let StoreAdmissionOutcome::Admitted(invocation) = admit_unpublished(&store, &first, &origin)
    else {
        panic!("fresh admission");
    };
    assert!(
        store.get_session(first.id).unwrap().is_none(),
        "UUID is still unpublished"
    );
    let snapshot = store.manager_v2_resource_snapshot(&config).unwrap();
    assert_eq!(snapshot["active_sessions"], 1);
    assert_eq!(
        snapshot["active_by_provider"][format!("{:?}", first.provider)],
        1
    );
    assert_denied(
        admit_unpublished(&store, &second, &second_origin),
        "concurrency_capacity",
    );
    store
        .request_model_invocation_cancellation(invocation, "failed publication", "test")
        .unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
        1
    );
    assert_denied(
        admit_unpublished(&store, &second, &second_origin),
        "concurrency_capacity",
    );
    complete(&store, invocation, 3.0);
    let snapshot = store.manager_v2_resource_snapshot(&config).unwrap();
    assert_eq!(snapshot["active_sessions"], 0);
    assert_eq!(snapshot["known_spend_usd"], 3.0);
    assert_eq!(
        snapshot["unknown_spend_observations"],
        0,
        "snapshot={snapshot}; checkpoint={:?}; ledger={:?}; lead={:?}",
        store
            .manager_v2_record(&config, SPEND_KIND, &first.id.to_string())
            .unwrap()
            .map(|r| r.payload),
        store.manager_v2_ledger_spend(first.id),
        store.manager_v2_ledger_spend(lead.id)
    );
    let before_retirement = store.manager_v2_resource_snapshot(&config).unwrap();
    store.manager_v2_retire_bookkeeping(&config).unwrap();
    for kind in [ORIGIN_KIND, SPEND_KIND] {
        assert!(
            store
                .manager_v2_record(&config, kind, &first.id.to_string())
                .unwrap()
                .unwrap()
                .archived
        );
    }
    let after_retirement = store.manager_v2_resource_snapshot(&config).unwrap();
    assert_eq!(
        after_retirement["known_spend_usd"],
        before_retirement["known_spend_usd"]
    );
    assert_eq!(
        after_retirement["unknown_spend_observations"],
        before_retirement["unknown_spend_observations"]
    );
    // Publication after settlement must not count the reservation a second time.
    store.insert_session(&first).unwrap();
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["known_spend_usd"],
        3.0
    );
    update_policy(&store, &config, |p| p.max_spend_usd = Some(2.0));
    assert_denied(
        admit_unpublished(&store, &second, &second_origin),
        "spend_exhausted",
    );
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["known_spend_usd"],
        3.0
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_launch_after_retired_bookkeeping_history() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let lead_invocation = admitted(&store, &lead);
    complete(&store, lead_invocation, 0.0);
    store
        .set_session_model_invocation(lead.id, Some(lead_invocation))
        .unwrap();
    let (first, first_origin) = unpublished_child(&store, &lead);
    let StoreAdmissionOutcome::Admitted(first_invocation) =
        admit_unpublished(&store, &first, &first_origin)
    else {
        panic!("first launch must be admitted");
    };
    complete(&store, first_invocation, 1.0);
    store.manager_v2_retire_bookkeeping(&config).unwrap();
    assert!(
        store
            .manager_v2_record(&config, SPEND_KIND, &first.id.to_string())
            .unwrap()
            .unwrap()
            .archived
    );
    let stamp = super::super::harness_manager_v2::now();
    let tx = store.conn.unchecked_transaction().unwrap();
    for n in 0..=MANAGER_V2_MAX_RECORDS {
        for kind in ["retrieval", "lifecycle_context"] {
            tx.execute(
                "INSERT INTO harness_manager_v2_records(project_id,manager_session_id,scope_version,kind,record_key,row_version,payload_json,archived,created_at,updated_at)
                 VALUES(?1,?2,?3,?4,?5,1,'{}',1,?6,?6)",
                params![config.project_id.to_string(),config.manager_session_id.to_string(),
                    config.row_version,kind,format!("settled:{kind}:{n}"),stamp],
            ).unwrap();
        }
    }
    // Settled launches retain paired origin and spend evidence. Their combined
    // history must not consume the coordination quota during a real admission.
    for n in 0..513_u128 {
        let id = Uuid::from_u128(0x61400000000000000000000000000000 + n).to_string();
        for (kind, payload) in [
            (ORIGIN_KIND, json!({"provider":lead.provider})),
            (
                SPEND_KIND,
                json!({"known_floor_usd":0.0,"zero_origin":true}),
            ),
        ] {
            tx.execute(
                "INSERT INTO harness_manager_v2_records(project_id,manager_session_id,scope_version,kind,record_key,row_version,payload_json,archived,created_at,updated_at)
                 VALUES(?1,?2,?3,?4,?5,1,?6,1,?7,?7)",
                params![config.project_id.to_string(),config.manager_session_id.to_string(),
                    config.row_version,kind,id,payload.to_string(),stamp],
            ).unwrap();
        }
    }
    tx.commit().unwrap();
    let (second, second_origin) = unpublished_child(&store, &lead);
    assert!(matches!(
        admit_unpublished(&store, &second, &second_origin),
        StoreAdmissionOutcome::Admitted(_)
    ));
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["known_spend_usd"],
        1.0
    );
    let overview = store
        .manager_v2_inspect_operator(
            config.project_id,
            &AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Overview,
                limit: 32,
                ..Default::default()
            },
        )
        .unwrap();
    let budget = overview
        .rows
        .iter()
        .find(|row| row["type"] == "record_budget")
        .unwrap();
    assert_eq!(budget["retrieval"]["used"], 0);
    assert_eq!(budget["resource"]["used"], 2);
    assert_eq!(budget["lifecycle"]["used"], 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_fresh_transaction_rechecks_pause_provider_and_spend_after_preflight() {
    for change in ["pause", "provider", "spend", "unknown", "retired"] {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, ManagerPolicyV2::default());
        let (child, origin) = unpublished_child(&store, &lead);
        store
            .manager_v2_resource_gate_for_parent(child.parent_id.unwrap(), child.provider)
            .unwrap();
        let code = match change {
            "pause" => {
                update_policy(&store, &config, |p| p.paused = true);
                "policy_paused"
            }
            "provider" => {
                let _other = admitted(&store, &lead);
                update_policy(&store, &config, |p| {
                    p.provider_limits = vec![ManagerProviderLimitV2 {
                        provider: child.provider,
                        max_active: 1,
                    }]
                });
                "provider_capacity"
            }
            "spend" => {
                let mut changed = lead.clone();
                changed.cost_usd = Some(4.0);
                store.update_session_metadata(&changed).unwrap();
                update_policy(&store, &config, |p| p.max_spend_usd = Some(3.0));
                "spend_exhausted"
            }
            "unknown" => {
                let mut changed = lead.clone();
                changed.cost_usd = None;
                store.update_session_metadata(&changed).unwrap();
                update_policy(&store, &config, |p| p.max_spend_usd = Some(3.0));
                "spend_unknown"
            }
            "retired" => {
                store
                    .update_session_status(config.epic_ids[0], SessionStatus::Archived)
                    .unwrap();
                "launch_parent_retired"
            }
            _ => unreachable!(),
        };
        assert_denied(admit_unpublished(&store, &child, &origin), code);
        assert!(
            store
                .manager_v2_record(&config, ORIGIN_KIND, &child.id.to_string())
                .unwrap()
                .is_none()
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_fresh_origin_is_exact_and_unknown_settlement_stays_unknown() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let (child, origin) = unpublished_child(&store, &lead);
    let (other, _) = unpublished_child(&store, &lead);
    assert_denied(
        admit_unpublished(&store, &other, &origin),
        "launch_identity_changed",
    );
    let StoreAdmissionOutcome::Admitted(invocation) = admit_unpublished(&store, &child, &origin)
    else {
        panic!("fresh admission");
    };
    store
        .complete_model_invocation(
            invocation,
            &InvocationCompletion {
                error_class: Some("publication_failed".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
        0
    );
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["unknown_spend_observations"],
        1
    );
    update_policy(&store, &config, |p| p.max_spend_usd = Some(10.0));
    let (next, next_origin) = unpublished_child(&store, &lead);
    assert_denied(
        admit_unpublished(&store, &next, &next_origin),
        "spend_unknown",
    );
}

#[cfg(target_os = "linux")]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_question_cleanup_reopen_preserves_unknown_usage_and_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cleanup.db");
    let store = Store::open_test_fixture(&path).unwrap();
    let (config, mut lead) = fixture(&store, ManagerPolicyV2::default());
    lead.cost_usd = Some(5.0);
    lead.total_input_tokens = Some(500);
    store.update_session_metadata(&lead).unwrap();
    let invocation = admitted(&store, &lead);
    store
        .set_session_model_invocation(lead.id, Some(invocation))
        .unwrap();
    store
        .mark_manager_question_cleanup_required(invocation)
        .unwrap();
    store
        .request_model_invocation_cancellation(invocation, "stop failed answer", "test")
        .unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
        1
    );
    // Store-only recovery precondition: this fixture never launches a process.
    // The dedicated lifecycle regressions prove checked reaping on real stamped
    // children using an isolated process inventory. Never scan the host cohort.

    assert_eq!(store.reconcile_running_model_invocations().unwrap(), 1);
    let row = store
        .load_model_invocation_record(invocation)
        .unwrap()
        .unwrap();
    assert_eq!(row.usage.estimated_cost_usd, None);
    assert_eq!(row.usage.input_tokens, None);
    assert_eq!(
        row.error_class.as_deref(),
        Some("manager_question_cleanup_restart")
    );
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
        0
    );
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["known_spend_usd"],
        5.0
    );
    assert!(
        store.manager_v2_resource_snapshot(&config).unwrap()["unknown_spend_observations"]
            .as_i64()
            .unwrap()
            > 0
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_selected_pending_uuid_survives_scope_revision_and_reopen() {
    use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pending-scope.db");
    let store = Store::open_test_fixture(&path).unwrap();
    let (scope, lead) = fixture(
        &store,
        ManagerPolicyV2 {
            max_active_sessions: 1,
            ..Default::default()
        },
    );
    let (child, origin) = unpublished_child(&store, &lead);
    let StoreAdmissionOutcome::Admitted(invocation) = admit_unpublished(&store, &child, &origin)
    else {
        panic!("admitted");
    };
    let other_epic = clone_session(&store, &lead, group(&store, &scope), SessionKind::Epic);
    let revised = store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: scope.project_id,
            session_id: scope.manager_session_id,
            epic_ids: Some(vec![scope.epic_ids[0], other_epic.id]),
            expected_row_version: scope.row_version,
        })
        .unwrap();
    update_policy(&store, &revised, |policy| policy.max_active_sessions = 1);
    assert!(revised.row_version > scope.row_version);
    let (second, next_origin) = unpublished_child(&store, &lead);
    assert_eq!(
        store.manager_v2_resource_snapshot(&revised).unwrap()["active_sessions"],
        1
    );
    assert_denied(
        admit_unpublished(&store, &second, &next_origin),
        "concurrency_capacity",
    );
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_denied(
        admit_unpublished(&store, &second, &next_origin),
        "concurrency_capacity",
    );
    complete(&store, invocation, 2.0);
    assert_eq!(
        store.manager_v2_resource_snapshot(&revised).unwrap()["active_sessions"],
        0
    );
    assert_eq!(
        store.manager_v2_resource_snapshot(&revised).unwrap()["known_spend_usd"],
        2.0
    );
    store.insert_session(&child).unwrap();
    assert_eq!(
        store.manager_v2_resource_snapshot(&revised).unwrap()["known_spend_usd"],
        2.0
    );
    // Copying the proof forward for a later admission does not overwrite its
    // historical scope record or reset the previous invocation cost.
    let next = admitted(&store, &child);
    complete(&store, next, 1.0);
    assert_eq!(
        store.manager_v2_resource_snapshot(&revised).unwrap()["known_spend_usd"],
        3.0
    );
    assert_eq!(
        store
            .manager_v2_record(&scope, SPEND_KIND, &child.id.to_string())
            .unwrap()
            .unwrap()
            .payload["zero_origin"],
        true
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
#[allow(clippy::unwrap_used, clippy::too_many_lines)] // Large history fixture needs explicit fail-fast setup and end-to-end assertions.
fn manager_v2_resources_preserves_large_retired_accounting_history_with_small_live_cohort() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let epic = config.epic_ids[0];
    let retired = clone_session(&store, &lead, group(&store, &config), SessionKind::Epic);
    let historical = clone_session(&store, &lead, retired.id, SessionKind::Task);
    attribute(&store, &config, &historical);
    store
        .manager_v2_put_record(
            &config,
            SPEND_KIND,
            &historical.id.to_string(),
            Some(retired.id),
            0,
            &serde_json::to_value(SpendCheckpoint {
                known_floor_usd: 7.0,
                zero_origin: false,
            })
            .unwrap(),
        )
        .unwrap();
    for index in 0..1200 {
        let stale = clone_session(&store, &lead, epic, SessionKind::Task);
        attribute(&store, &config, &stale);
        if index == 1100 {
            store
                .conn
                .execute(
                    "UPDATE sessions SET cost_usd=2.5 WHERE id=?1",
                    [stale.id.to_string()],
                )
                .unwrap();
        }
        if index == 1199 {
            store
                .conn
                .execute(
                    "UPDATE sessions SET cost_usd=NULL WHERE id=?1",
                    [stale.id.to_string()],
                )
                .unwrap();
        }
    }

    let snapshot = store.manager_v2_resource_snapshot(&config).unwrap();
    store
        .manager_v2_inspect_operator(config.project_id, &AgentManagerInspectRequestV2::default())
        .unwrap();
    assert_eq!(
        snapshot["known_spend_usd"], 9.5,
        "retired historical spend must remain in the bounded aggregate"
    );
    assert_eq!(snapshot["unknown_spend_observations"], 1);
    assert_eq!(
        store.manager_v2_cohort(&config).unwrap().len(),
        2,
        "live authority remains the current Epic descendants"
    );
    store
        .update_session_status(lead.id, SessionStatus::Failed)
        .unwrap();
    let source = tempfile::tempdir().unwrap();
    for args in [
        vec!["init"],
        vec![
            "-c",
            "user.name=RSI Test",
            "-c",
            "user.email=rsi@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "source",
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(source.path())
                .status()
                .unwrap()
                .success()
        );
    }
    store
        .conn
        .execute(
            "UPDATE sessions SET working_dir=?2 WHERE id=?1",
            params![lead.id.to_string(), source.path().to_str().unwrap()],
        )
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE sessions SET model='gpt-6-astra' WHERE id=?1",
            [lead.id.to_string()],
        )
        .unwrap();
    let launch = ManagerLaunchChoiceV2 {
        provider: lead.provider,
        model: "gpt-6-astra".into(),
        effort: None,
    };
    update_policy(&store, &config, |policy| {
        policy.mode = ManagerOperatingModeV2::Execute;
        policy.capabilities = vec![ManagerCapabilityV2::LeadControl];
        policy.max_created_sessions = 1;
        policy.max_recovery_attempts = 1;
        policy.allowed_launches = vec![launch.clone()];
    });
    store
        .prepare_manager_action(
            config.manager_session_id,
            AgentManagerPrepareControlRequestV2 {
                project_id: None,
                operation: PreparedManagerActionV2::RetryLead {
                    epic_id: epic,
                    message: "retry after retired cohort".into(),
                    launch: Some(launch),
                },
            },
        )
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
#[allow(clippy::unwrap_used)] // The capacity fixture must fail immediately on setup or admission errors.
fn manager_v2_resources_rejects_more_than_budget_live_members() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    for _ in 0..COHORT_BUDGET {
        let mut child = clone_session(&store, &lead, config.epic_ids[0], SessionKind::Task);
        child.status = SessionStatus::Running;
        store.update_session_status(child.id, child.status).unwrap();
    }
    assert!(
        store
            .manager_v2_resource_snapshot(&config)
            .unwrap_err()
            .to_string()
            .contains("manager_v2_cohort_subdivision_required")
    );
}

#[cfg(target_os = "linux")]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_answer_reopen_preserves_only_exact_invocation_measurements() {
    for (bind_answer, prefix) in [false, true].into_iter().flat_map(|bind| {
        ["manager.answer:", "remote.answer:"]
            .into_iter()
            .map(move |prefix| (bind, prefix))
    }) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("answer.db");
        let store = Store::open_test_fixture(&path).unwrap();
        let (_, mut lead) = fixture(&store, ManagerPolicyV2::default());
        lead.cost_usd = Some(5.0);
        lead.total_input_tokens = Some(500);
        store.update_session_metadata(&lead).unwrap();
        let mut request = admission_request(&lead);
        request.purpose = ModelInvocationPurpose::SessionContinueResume;
        request.trigger = "continue_session".into();
        request.dedup_key = Some(format!("{prefix}{}", Uuid::new_v4()));
        let StoreAdmissionOutcome::Admitted(invocation) = store
            .admit_model_invocation(
                Uuid::new_v4(),
                *crate::model_control::registry::entry(request.purpose),
                ModelTier::Standard,
                &request,
            )
            .unwrap()
        else {
            panic!("answer admission");
        };
        // Fixture of telemetry persisted on this exact invocation, as opposed
        // to the Session's aggregate from a previous monitored turn.
        store.conn.execute(
            "UPDATE model_invocations SET estimated_cost_usd=0.75,input_tokens=7,usage_confidence='measured' WHERE id=?1",
            [invocation.to_string()],
        ).unwrap();
        if bind_answer {
            store
                .set_session_model_invocation(lead.id, Some(invocation))
                .unwrap();
        }
        drop(store);
        let store = Store::open(&path).unwrap();
        // This Store fixture has no provider process; production performs the
        // checked process-first startup gate before this same reconciliation.
        assert_eq!(store.reconcile_running_model_invocations().unwrap(), 1);
        let row = store
            .load_model_invocation_record(invocation)
            .unwrap()
            .unwrap();
        assert_eq!(row.usage.estimated_cost_usd, Some(0.75));
        assert_eq!(row.usage.input_tokens, Some(7));
        assert_eq!(
            row.usage.confidence,
            rsi_common::model_control::ModelUsageConfidence::Measured
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
#[allow(clippy::unwrap_used)] // Fixture setup and exact usage assertions must fail immediately.
fn manager_v2_resources_reports_created_session_usage_against_the_quota() {
    let store = Store::open_in_memory().unwrap();
    let (config, _lead) = fixture(
        &store,
        ManagerPolicyV2 {
            max_created_sessions: 5,
            ..Default::default()
        },
    );
    let worker = |query: &str| ManagerActionV2::CreateSession {
        parent_id: config.epic_ids[0],
        kind: SessionKind::Task,
        query: query.into(),
        launch: ManagerLaunchChoiceV2 {
            provider: rsi_common::types::SessionProvider::Claude,
            model: "fixture-model".into(),
            effort: None,
        },
        sandbox_source: None,
    };
    for query in ["first worker", "second worker"] {
        store
            .seed_manager_action_for_test(
                &config,
                worker(query),
                ManagerActionStateV2::Succeeded,
                Some(config.manager_session_id),
            )
            .unwrap();
    }
    // Blocked before its session existed: not charged (K15a).
    store
        .seed_manager_action_for_test(
            &config,
            worker("blocked before launch"),
            ManagerActionStateV2::Blocked,
            Some(Uuid::new_v4()),
        )
        .unwrap();
    let row = store.manager_v2_resource_snapshot(&config).unwrap();
    // #3416: the count is the rolling window's, with the lifetime total beside it.
    let created = &row["created_sessions"];
    assert_eq!(
        (
            created["used"].clone(),
            created["limit"].clone(),
            created["remaining"].clone()
        ),
        (
            serde_json::json!(2),
            serde_json::json!(5),
            serde_json::json!(3)
        )
    );
    assert_eq!(created["window_hours"], 24);
    assert_eq!(created["lifetime_total"], 2);
}

#[cfg(test)]
fn helper_admission_request(session: &Session) -> ModelAdmissionRequest {
    // Paid background work is denied by default, so a helper is admitted
    // through the local (free) backend like the real title/memory callers.
    ModelAdmissionRequest {
        purpose: ModelInvocationPurpose::SessionTitle,
        provider: Some("Local".into()),
        model: Some("qwen3:4b".into()),
        backend: Some("ollama".into()),
        ..admission_request(session)
    }
}

#[cfg(test)]
fn admit_helper(store: &Store, session: &Session) -> StoreAdmissionOutcome {
    let request = helper_admission_request(session);
    store
        .admit_model_invocation(
            Uuid::new_v4(),
            *crate::model_control::registry::entry(request.purpose),
            ModelTier::Local,
            &request,
        )
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_helper_calls_skip_the_concurrency_cap_but_obey_pause() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(
        &store,
        ManagerPolicyV2 {
            max_active_sessions: 1,
            ..Default::default()
        },
    );
    let idle = clone_session(&store, &lead, config.epic_ids[0], SessionKind::Task);
    // One running lead call fills the cap of one; a second session is refused.
    admitted(&store, &lead);
    assert_denied(admit(&store, &idle), "concurrency_capacity");
    // A helper call for that same idle session is not refused by the cap and
    // does not occupy a slot.
    match admit_helper(&store, &idle) {
        StoreAdmissionOutcome::Admitted(_) => {}
        other => panic!("helper must bypass the concurrency cap, got {other:?}"),
    }
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
        1
    );
    // The operator pause still stops helper calls, with the lead's refusal.
    update_policy(&store, &config, |p| p.paused = true);
    assert_denied(admit(&store, &idle), "policy_paused");
    assert_denied(admit_helper(&store, &idle), "policy_paused");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_helper_calls_obey_the_spend_cap() {
    let store = Store::open_in_memory().unwrap();
    let (_config, lead) = fixture(
        &store,
        ManagerPolicyV2 {
            max_spend_usd: Some(2.0),
            ..Default::default()
        },
    );
    let first = admitted(&store, &lead);
    complete(&store, first, 1.0);
    let second = admitted(&store, &lead);
    // Launch/resume rows hold the session's running total (#2593): 1.0 + 1.0.
    complete(&store, second, 2.0);
    assert_denied(admit(&store, &lead), "spend_exhausted");
    assert_denied(admit_helper(&store, &lead), "spend_exhausted");
}

/// A paid helper on the memory LLM's Remote HTTP path: provider label
/// `Remote`, a non-loopback OpenAI-compatible endpoint (#1080).
#[cfg(test)]
fn admit_paid_remote_helper(store: &Store, session: &Session) -> StoreAdmissionOutcome {
    let request = ModelAdmissionRequest {
        purpose: ModelInvocationPurpose::MemoryObservationExtract,
        provider: Some("Remote".into()),
        model: Some("gpt-test".into()),
        backend: Some("openai_compatible_api".into()),
        ..admission_request(session)
    };
    // The operator enabled paid background work for this purpose.
    let mut entry = *crate::model_control::registry::entry(request.purpose);
    entry.background_paid_enabled_by_default = true;
    store
        .admit_model_invocation(Uuid::new_v4(), entry, ModelTier::Standard, &request)
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_remote_paid_helper_is_admitted_then_obeys_pause() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    match admit_paid_remote_helper(&store, &lead) {
        StoreAdmissionOutcome::Admitted(_) => {}
        other => panic!("enabled paid remote helper must be admitted, got {other:?}"),
    }
    update_policy(&store, &config, |p| p.paused = true);
    assert_denied(admit(&store, &lead), "policy_paused");
    assert_denied(admit_paid_remote_helper(&store, &lead), "policy_paused");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_remote_paid_helper_obeys_the_spend_cap() {
    let store = Store::open_in_memory().unwrap();
    let (_config, lead) = fixture(
        &store,
        ManagerPolicyV2 {
            max_spend_usd: Some(2.0),
            ..Default::default()
        },
    );
    let first = admitted(&store, &lead);
    complete(&store, first, 1.0);
    let second = admitted(&store, &lead);
    // Launch/resume rows hold the session's running total (#2593): 1.0 + 1.0.
    complete(&store, second, 2.0);
    assert_denied(admit(&store, &lead), "spend_exhausted");
    assert_denied(admit_paid_remote_helper(&store, &lead), "spend_exhausted");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_remote_paid_helper_obeys_provider_usage_limits() {
    let store = Store::open_in_memory().unwrap();
    let (_config, lead) = fixture(&store, ManagerPolicyV2::default());
    // A remote OpenAI-compatible call is a direct-API (Harness) call.
    store
        .upsert_provider_rate_limit_snapshot(
            &ProviderRateLimitSnapshot {
                provider: SessionProvider::Harness,
                status: None,
                rate_limit_type: None,
                overage_status: None,
                is_using_overage: false,
                observed_at: Utc::now(),
                windows: vec![ProviderRateLimitWindow {
                    window_key: "five_hour".into(),
                    utilization: 1.0,
                    resets_at_epoch: Some(Utc::now().timestamp() + 3600),
                }],
                stale_reason: None,
            },
            None,
        )
        .unwrap();
    assert_denied(
        admit_paid_remote_helper(&store, &lead),
        "provider_usage_limit",
    );
}

/// One manager-requested topology execution with `launches` charged session
/// attempts (`boot_id` stamped), requested by `requester` at `scope_version`.
#[cfg(test)]
fn seed_topology_launches(
    store: &Store,
    project: Uuid,
    requester: Uuid,
    scope_version: i64,
    launches: usize,
) {
    seed_topology_launches_at(
        store,
        project,
        requester,
        scope_version,
        launches,
        Utc::now(),
    );
}

/// [`seed_topology_launches`] created at `at` (#3416: the rolling window).
#[cfg(test)]
fn seed_topology_launches_at(
    store: &Store,
    project: Uuid,
    requester: Uuid,
    scope_version: i64,
    launches: usize,
    at: chrono::DateTime<Utc>,
) {
    let execution = Uuid::new_v4();
    let now = at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    store
        .conn
        .execute(
            "INSERT INTO topology_executions (id,topology_name_snapshot,workflow_id,definition_json,definition_digest,project_id,requested_by_kind,requested_by_session_id,scope_version,repo_root,base_commit,custody_plan_json,status,created_at,updated_at)
             VALUES (?1,'usage','wf','{}','d',?2,'manager',?3,?4,'/tmp/repo','0000000000000000000000000000000000000000','{}','running',?5,?5)",
            params![execution.to_string(), project.to_string(), requester.to_string(), scope_version, now],
        )
        .unwrap();
    for n in 0..launches {
        store
            .conn
            .execute(
                "INSERT INTO topology_node_attempts (id,execution_id,node_id,iteration,attempt_no,node_kind,status,dedup_key,session_id,boot_id,input_json,created_at,updated_at)
                 VALUES (?1,?2,?3,0,1,'session','running',?4,NULL,'boot','{}',?5,?5)",
                params![
                    Uuid::new_v4().to_string(),
                    execution.to_string(),
                    format!("N{n}"),
                    format!("dedup-{execution}-{n}"),
                    now
                ],
            )
            .unwrap();
    }
}

/// #1283: a project manager and a global seat holding the same scope version
/// in one project are each charged only for the topology launches they
/// requested, and each snapshot shows its own principal's caps.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_resources_topology_usage_is_charged_to_the_requesting_principal() {
    use rsi_common::global_manager::ConfigureGlobalManagerRequestV1;
    let store = Store::open_in_memory().unwrap();
    let launch = ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "claude-sonnet-4-6".into(),
        effort: None,
    };
    let (pm, _lead) = fixture(
        &store,
        ManagerPolicyV2 {
            capabilities: vec![ManagerCapabilityV2::SessionCreate],
            max_created_sessions: 7,
            allowed_launches: vec![launch.clone()],
            ..Default::default()
        },
    );
    let seat = Uuid::new_v4();
    let mut row = crate::test_support::test_session(seat, std::path::PathBuf::from("/tmp/seat"));
    row.status = SessionStatus::Completed;
    store.insert_session(&row).unwrap();
    store
        .configure_global_manager(
            &ConfigureGlobalManagerRequestV1 {
                session_id: seat,
                project_ids: vec![pm.project_id],
                allowed_launches: vec![launch],
                project_policy: ManagerPolicyV2 {
                    capabilities: vec![ManagerCapabilityV2::SessionCreate],
                    max_created_sessions: 11,
                    ..Default::default()
                },
                expected_grant_version: 0,
                idempotency_key: format!("grant-{seat}"),
            },
            "operator:test",
        )
        .unwrap();
    let principal = store
        .global_project_principal(seat, pm.project_id)
        .unwrap()
        .unwrap();
    let global = store
        .global_manager_authority_for(&principal, seat)
        .unwrap()
        .config;
    // The two principals hold the same scope version in the same project.
    assert_eq!(pm.row_version, global.row_version);
    assert_ne!(pm.manager_session_id, global.manager_session_id);
    // A rotation successor of the global seat requests as the same principal.
    let successor = Uuid::new_v4();
    let mut next = row.clone();
    next.id = successor;
    next.continued_from = Some(seat);
    store.insert_session(&next).unwrap();

    seed_topology_launches(
        &store,
        pm.project_id,
        pm.manager_session_id,
        pm.row_version,
        2,
    );
    seed_topology_launches(&store, pm.project_id, seat, global.row_version, 1);
    seed_topology_launches(&store, pm.project_id, successor, global.row_version, 3);
    // Another scope version of the global seat is not this scope's usage.
    seed_topology_launches(&store, pm.project_id, seat, global.row_version + 5, 4);

    assert_eq!(store.manager_v2_created_usage(&pm, false).unwrap(), 2);
    assert_eq!(store.manager_v2_created_usage(&global, false).unwrap(), 4);

    let pm_snapshot = store.manager_v2_resource_snapshot(&pm).unwrap();
    assert_eq!(pm_snapshot["created_sessions"]["used"], 2);
    assert_eq!(pm_snapshot["created_sessions"]["limit"], 7);
    assert_eq!(pm_snapshot["policy"]["policy"]["max_created_sessions"], 7);
    let global_snapshot = store.manager_v2_resource_snapshot(&global).unwrap();
    assert_eq!(global_snapshot["created_sessions"]["used"], 4);
    assert_eq!(global_snapshot["created_sessions"]["limit"], 11);
    assert_eq!(
        global_snapshot["policy"]["policy"]["max_created_sessions"],
        11
    );
}

/// #289: a successor seat's Claude effort is judged by the CLI's live ladder,
/// as a launch is (#2768): `ultracode` only when the injected ladder lists it,
/// the compiled `low..max` list when the ladder is unknown.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn successor_seat_claude_effort_follows_the_injected_live_ladder() {
    use super::launch_origin::validate_root_choice_with;
    let choice = |effort: &str| ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "claude-opus-9".into(),
        effort: Some(effort.into()),
    };
    let ladder = |levels: &'static [&'static str]| {
        move |model: &str| {
            (model == "claude-opus-9").then(|| {
                levels
                    .iter()
                    .map(|l| l.to_string())
                    .collect::<Vec<String>>()
            })
        }
    };
    let with_ultracode = ladder(&["low", "medium", "high", "xhigh", "max", "ultracode"]);
    let without_ultracode = ladder(&["low", "medium", "high", "xhigh", "max"]);
    let unknown = |_: &str| None;

    for effort in ["ultracode", "max"] {
        assert_eq!(
            validate_root_choice_with(&choice(effort), with_ultracode).ok(),
            Some(()),
            "{effort} is listed"
        );
    }
    assert_eq!(
        validate_root_choice_with(&choice("max"), without_ultracode).ok(),
        Some(())
    );
    assert_eq!(
        validate_root_choice_with(&choice("max"), unknown).ok(),
        Some(())
    );
    for refusal in [
        validate_root_choice_with(&choice("ultracode"), without_ultracode),
        validate_root_choice_with(&choice("ultracode"), unknown),
    ] {
        assert_eq!(
            refusal.unwrap_err().to_string(),
            refused("manager_succession_launch_invalid").to_string()
        );
    }
    // The production entry point with no daemon-installed source keeps the
    // compiled list.
    assert_eq!(
        super::launch_origin::validate_root_choice(&choice("max")).ok(),
        Some(())
    );
    assert_eq!(
        super::launch_origin::validate_root_choice(&choice("ultracode"))
            .unwrap_err()
            .to_string(),
        refused("manager_succession_launch_invalid").to_string()
    );
}

/// #3066: the hold deadline of an exhausted plan window is its latest reset;
/// a window with no reset time is re-checked after the short fallback, and a
/// window that has reset (or is below full) holds nothing.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn usage_window_hold_until_is_the_latest_exhausted_reset() {
    let store = Store::open_in_memory().unwrap();
    let now = Utc::now();
    let snapshot = |windows: Vec<ProviderRateLimitWindow>| ProviderRateLimitSnapshot {
        provider: SessionProvider::Claude,
        status: None,
        rate_limit_type: None,
        overage_status: None,
        is_using_overage: false,
        observed_at: now,
        windows,
        stale_reason: None,
    };
    let window = |key: &str, utilization: f64, reset: Option<i64>| ProviderRateLimitWindow {
        window_key: key.into(),
        utilization,
        resets_at_epoch: reset,
    };
    assert_eq!(
        store
            .provider_usage_window_hold_until(SessionProvider::Claude, now)
            .unwrap(),
        None,
        "no windows, no hold"
    );
    let five_hour_reset = now.timestamp() + 3 * 3600;
    let seven_day_reset = now.timestamp() + 2 * 24 * 3600;
    store
        .upsert_provider_rate_limit_snapshot(
            &snapshot(vec![
                window("five_hour", 1.0, Some(five_hour_reset)),
                window("seven_day", 1.0, Some(seven_day_reset)),
            ]),
            None,
        )
        .unwrap();
    let held = store
        .provider_usage_window_hold_until(SessionProvider::Claude, now)
        .unwrap()
        .expect("held");
    assert_eq!(held.timestamp(), seven_day_reset, "every window must clear");
    assert_eq!(
        store
            .provider_usage_window_hold_until(SessionProvider::Codex, now)
            .unwrap(),
        None,
        "another provider's account is not held"
    );
    // The five-hour window reset and the seven-day one is below full: free.
    store
        .upsert_provider_rate_limit_snapshot(
            &snapshot(vec![
                window("five_hour", 1.0, Some(now.timestamp() - 1)),
                window("seven_day", 0.4, Some(seven_day_reset)),
            ]),
            None,
        )
        .unwrap();
    assert_eq!(
        store
            .provider_usage_window_hold_until(SessionProvider::Claude, now)
            .unwrap(),
        None
    );
    // A full window with no reset time is re-checked soon, never held forever.
    store
        .upsert_provider_rate_limit_snapshot(&snapshot(vec![window("five_hour", 1.0, None)]), None)
        .unwrap();
    let unknown = store
        .provider_usage_window_hold_until(SessionProvider::Claude, now)
        .unwrap()
        .expect("held");
    assert_eq!(unknown, now + chrono::Duration::minutes(5));
}

/// #235: `max_active_sessions` caps the workers a manager launches. The
/// manager's own seat lineage (the outgoing seat still `Running` during a
/// succession and the new tip, both resource origins) fills no slot and its
/// own calls are never refused by the cap; the snapshot and the refusal name
/// the counted ids and the excluded seat ids.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_seat_lineage_fills_no_worker_slot_and_the_refusal_names_the_counted_sessions_235() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(
        &store,
        ManagerPolicyV2 {
            max_active_sessions: 3,
            ..Default::default()
        },
    );
    let epic = config.epic_ids[0];
    // The outgoing seat is still Running while its successor already runs.
    let outgoing = config.manager_session_id;
    store
        .update_session_status(outgoing, SessionStatus::Running)
        .unwrap();
    let mut tip = store.get_session(outgoing).unwrap().unwrap();
    tip.id = Uuid::new_v4();
    tip.continued_from = Some(outgoing);
    tip.rotation_depth += 1;
    tip.status = SessionStatus::Running;
    store.insert_session(&tip).unwrap();
    // Both are resource origins of the seat (a root succession records the
    // requesting seat and its candidate), so both are cohort members.
    for (session, predecessor) in [(outgoing, None), (tip.id, Some(outgoing))] {
        store
            .conn
            .execute(
                "INSERT INTO manager_root_resource_origins(session_id,project_id,manager_session_id,
                   scope_version,predecessor_session_id,provider,zero_origin,known_floor_usd,created_at)
                 VALUES(?1,?2,?3,?4,?5,?6,0,0,?7)",
                params![
                    session.to_string(),
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    predecessor.map(|id: Uuid| id.to_string()),
                    serde_json::to_value(tip.provider).unwrap().as_str().unwrap(),
                    super::super::harness_manager_v2::now()
                ],
            )
            .unwrap();
    }
    let worker = |store: &Store| {
        let mut next = clone_session(store, &lead, epic, SessionKind::Feature);
        next.status = SessionStatus::Running;
        store
            .update_session_status(next.id, SessionStatus::Running)
            .unwrap();
        next
    };
    let first = worker(&store);
    let second = worker(&store);
    let ids = |value: &Value| -> std::collections::BTreeSet<String> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap().to_owned())
            .collect()
    };
    let set = |sessions: &[Uuid]| -> std::collections::BTreeSet<String> {
        sessions.iter().map(Uuid::to_string).collect()
    };
    let snapshot = store.manager_v2_resource_snapshot(&config).unwrap();
    assert_eq!(snapshot["active_sessions"], 2);
    assert_eq!(
        snapshot["active_by_provider"]
            .as_object()
            .unwrap()
            .values()
            .map(|n| n.as_u64().unwrap())
            .sum::<u64>(),
        2
    );
    assert_eq!(
        ids(&snapshot["counted_session_ids"]),
        set(&[first.id, second.id])
    );
    assert_eq!(
        ids(&snapshot["excluded_manager_seat_ids"]),
        set(&[outgoing, tip.id])
    );
    // Two workers and two seat sessions under a cap of three: a third
    // worker is admitted.
    store
        .manager_v2_resource_gate(&config, Some(epic), tip.provider, None)
        .unwrap();
    let third = worker(&store);
    assert_eq!(
        store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
        3
    );
    // The fourth is refused, and the refusal names what filled the cap.
    let refusal = store
        .manager_v2_resource_gate(&config, Some(epic), tip.provider, None)
        .unwrap_err()
        .to_string();
    assert!(
        refusal
            .contains("manager_v2_concurrency_capacity: active_sessions=3 max_active_sessions=3"),
        "{refusal}"
    );
    let counted = refusal
        .split("counted=[")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .unwrap();
    assert_eq!(
        counted
            .split(',')
            .map(str::to_owned)
            .collect::<std::collections::BTreeSet<_>>(),
        set(&[first.id, second.id, third.id]),
        "{refusal}"
    );
    let excluded = refusal
        .split("excluded_manager_seat=[")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .unwrap();
    assert_eq!(
        excluded
            .split(',')
            .map(str::to_owned)
            .collect::<std::collections::BTreeSet<_>>(),
        set(&[outgoing, tip.id]),
        "{refusal}"
    );
    // The seat's own calls are not refused by the worker cap, through the
    // gate and through Model Control admission.
    store
        .manager_v2_resource_gate_for_session(tip.id, tip.provider)
        .unwrap();
    store
        .manager_v2_resource_gate_for_session(outgoing, tip.provider)
        .unwrap();
    admitted(&store, &tip);
    // A worker's continuation still replaces its own slot.
    store
        .manager_v2_resource_gate_for_session(first.id, first.provider)
        .unwrap();
}

/// #3416: the created-session quota is a rolling window. A seat with M
/// creations inside the window is refused with the window, the count and the
/// next-free time; once the oldest leaves the window (injected clock) admission
/// resumes with no operator action; a successor inherits the grant and the
/// predecessor's in-window creations still count; the lifetime total is
/// reported beside the window count and never compared with the limit.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn manager_v2_created_sessions_quota_refills_as_creations_leave_the_window() {
    use rsi_common::manager_budget::CreationCapRefusalV1;
    let store = Store::open_in_memory().unwrap();
    let launch = ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "claude-sonnet-4-6".into(),
        effort: None,
    };
    let (pm, _lead) = fixture(
        &store,
        ManagerPolicyV2 {
            capabilities: vec![ManagerCapabilityV2::SessionCreate],
            max_created_sessions: 3,
            created_sessions_window_hours: 24,
            allowed_launches: vec![launch],
            ..Default::default()
        },
    );
    let successor = Uuid::new_v4();
    let mut next = store.get_session(pm.manager_session_id).unwrap().unwrap();
    next.id = successor;
    next.continued_from = Some(pm.manager_session_id);
    store.insert_session(&next).unwrap();

    let now = Utc::now();
    let ago = |hours: i64| now - chrono::Duration::hours(hours);
    // Two creations a month ago (lifetime only), then three inside the
    // window: the oldest 20 h ago by the predecessor, the others by its
    // successor.
    seed_topology_launches_at(
        &store,
        pm.project_id,
        pm.manager_session_id,
        pm.row_version,
        2,
        ago(720),
    );
    seed_topology_launches_at(
        &store,
        pm.project_id,
        pm.manager_session_id,
        pm.row_version,
        1,
        ago(20),
    );
    seed_topology_launches_at(&store, pm.project_id, successor, pm.row_version, 2, ago(2));

    let grant = store
        .get_harness_manager_policy(pm.project_id)
        .unwrap()
        .unwrap();
    assert_eq!(grant.policy.created_sessions_window_hours, 24);
    let authority = crate::store::harness_manager_v2::ManagerAuthorityV2 {
        config: pm.clone(),
        grant,
        caller: successor,
        is_manager: true,
    };
    let usage = store
        .manager_v2_created_window_usage_at(&pm, 24, 3, now)
        .unwrap();
    assert_eq!((usage.used, usage.lifetime), (3, 5), "{usage:?}");
    // The oldest in-window creation leaves 24 h after it was made.
    let frees =
        (ago(20) + chrono::Duration::hours(24)).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    assert_eq!(usage.next_free_at.as_deref(), Some(frees.as_str()));

    // Refused at the window's limit, naming the window, count and next-free.
    let error = store
        .manager_action_creation_budget_at(&authority, false, false, now)
        .unwrap_err()
        .to_string();
    let detail = CreationCapRefusalV1::parse(&error).unwrap_or_else(|| panic!("{error}"));
    assert_eq!(detail.cap_field, "max_created_sessions");
    assert_eq!((detail.used, detail.limit), (3, 3));
    assert_eq!(detail.window_hours, Some(24));
    assert_eq!(detail.next_free_at.as_deref(), Some(frees.as_str()));
    assert!(
        detail
            .window_summary()
            .unwrap()
            .starts_with("3 of 3 created in the last 24 h; next slot frees at "),
        "{detail:?}"
    );

    // Four hours past the oldest in-window creation's expiry, one slot is free.
    let later = ago(20) + chrono::Duration::hours(24) + chrono::Duration::minutes(1);
    let after = store
        .manager_v2_created_window_usage_at(&pm, 24, 3, later)
        .unwrap();
    assert_eq!((after.used, after.lifetime), (2, 5), "{after:?}");
    store
        .manager_action_creation_budget_at(&authority, false, false, later)
        .expect("admission resumes once the oldest creation left the window");

    // A longer window still sees the month-old creations; a shorter one does not.
    let wide = store
        .manager_v2_created_window_usage_at(&pm, 24 * 40, 3, now)
        .unwrap();
    assert_eq!(wide.used, 5);

    // The snapshot reports the window, the next-free time and the lifetime
    // total beside the windowed count.
    let snapshot = store.manager_v2_resource_snapshot(&pm).unwrap();
    let created = &snapshot["created_sessions"];
    assert_eq!(created["used"], 3);
    assert_eq!(created["limit"], 3);
    assert_eq!(created["window_hours"], 24);
    assert_eq!(created["lifetime_total"], 5);
    assert!(
        created["summary"]
            .as_str()
            .unwrap()
            .starts_with("3 of 3 created in the last 24 h; next slot frees at "),
        "{created}"
    );
}

/// #3495: a queued or running creation has not made its session yet, so its
/// reservation stays charged however long it was held back; once the session
/// exists the charge runs a full window from the actual creation, exactly
/// once. Uncreated refusals stay free.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
#[allow(clippy::unwrap_used)] // Fixture setup and exact usage assertions must fail immediately.
fn held_creation_reservations_stay_charged_and_count_from_actual_creation() {
    use rsi_common::manager_budget::CreationCapRefusalV1;
    let store = Store::open_in_memory().unwrap();
    let (pm, _lead) = fixture(
        &store,
        ManagerPolicyV2 {
            capabilities: vec![ManagerCapabilityV2::SessionCreate],
            max_created_sessions: 1,
            created_sessions_window_hours: 1,
            ..Default::default()
        },
    );
    let grant = store
        .get_harness_manager_policy(pm.project_id)
        .unwrap()
        .unwrap();
    let authority = crate::store::harness_manager_v2::ManagerAuthorityV2 {
        config: pm.clone(),
        grant,
        caller: pm.manager_session_id,
        is_manager: true,
    };
    let stamp = |t: chrono::DateTime<Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let now = Utc::now();
    let hours_ago = |h: i64| now - chrono::Duration::hours(h);
    let worker = |query: &str| ManagerActionV2::CreateSession {
        parent_id: pm.epic_ids[0],
        kind: SessionKind::Task,
        query: query.into(),
        launch: ManagerLaunchChoiceV2 {
            provider: rsi_common::types::SessionProvider::Claude,
            model: "fixture-model".into(),
            effort: None,
        },
        sandbox_source: None,
    };
    let set_created = |operation: Uuid, at: chrono::DateTime<Utc>| {
        store
            .conn
            .execute(
                "UPDATE harness_manager_v2_operations SET created_at=?2,updated_at=?2 WHERE id=?1",
                params![operation.to_string(), stamp(at)],
            )
            .unwrap();
    };
    let used_at = |at: chrono::DateTime<Utc>| {
        store
            .manager_v2_created_window_usage_at(&pm, 1, 1, at)
            .unwrap()
            .used
    };

    // Uncreated refusals and rejected reservations are free whatever their age.
    let blocked = store
        .seed_manager_action_with_context_for_test(
            &pm,
            worker("blocked"),
            ManagerActionStateV2::Blocked,
            Some(Uuid::new_v4()),
            pm.manager_session_id,
        )
        .unwrap();
    set_created(blocked, hours_ago(5));
    assert_eq!(used_at(now), 0);

    // Held past the window: still charged while queued, own and ancestor view.
    let held = store
        .seed_manager_action_with_context_for_test(
            &pm,
            worker("held"),
            ManagerActionStateV2::Queued,
            Some(Uuid::new_v4()),
            pm.manager_session_id,
        )
        .unwrap();
    set_created(held, hours_ago(3));
    assert_eq!(used_at(now), 1, "a held reservation must not age out");
    assert_eq!(used_at(now + chrono::Duration::hours(10)), 1);
    let error = store
        .manager_action_creation_budget_at(&authority, false, false, now)
        .unwrap_err()
        .to_string();
    let detail = CreationCapRefusalV1::parse(&error).unwrap_or_else(|| panic!("{error}"));
    assert_eq!((detail.used, detail.limit), (1, 1), "{detail:?}");
    let charges = store
        .portfolio_creation_charges(pm.project_id, false, "", now)
        .unwrap();
    assert_eq!(
        charges.iter().map(|(at, _)| at.clone()).collect::<Vec<_>>(),
        vec![stamp(now)],
        "an ancestor sees the reservation as charged now"
    );
    // The same reservation, running, is charged the same way.
    store
        .conn
        .execute(
            "UPDATE harness_manager_v2_operations SET state='running' WHERE id=?1",
            params![held.to_string()],
        )
        .unwrap();
    assert_eq!(used_at(now), 1);

    // Creation happens now: the entity binds at the actual creation.
    let session = {
        let mut session = store.get_session(pm.manager_session_id).unwrap().unwrap();
        session.id = Uuid::new_v4();
        session.continued_from = None;
        store.insert_session(&session).unwrap();
        session.id
    };
    store
        .conn
        .execute(
            "UPDATE harness_manager_v2_operations SET state='succeeded',target_session_id=?2 WHERE id=?1",
            params![held.to_string(), session.to_string()],
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO harness_manager_v2_entities(session_id,operation_id,project_id,manager_session_id,scope_version,policy_version,kind,created_at) VALUES(?1,?2,?3,?4,?5,1,'session',?6)",
            params![
                session.to_string(),
                held.to_string(),
                pm.project_id.to_string(),
                pm.manager_session_id.to_string(),
                pm.row_version,
                stamp(now)
            ],
        )
        .unwrap();
    // Counted from creation, not from the reservation made three hours ago.
    assert_eq!(used_at(now), 1);
    assert_eq!(used_at(now + chrono::Duration::minutes(59)), 1);
    assert_eq!(used_at(now + chrono::Duration::minutes(61)), 0);
    let usage = store
        .manager_v2_created_window_usage_at(&pm, 1, 1, now)
        .unwrap();
    let frees = stamp(now + chrono::Duration::hours(1));
    assert_eq!(usage.next_free_at.as_deref(), Some(frees.as_str()));
    assert_eq!(usage.lifetime, 1, "charged exactly once");
    let charges = store
        .portfolio_creation_charges(pm.project_id, false, "", now + chrono::Duration::hours(2))
        .unwrap();
    assert_eq!(
        charges.iter().map(|(at, _)| at.clone()).collect::<Vec<_>>(),
        vec![stamp(now)],
        "an ancestor counts the creation from the session's actual creation"
    );
}

/// #3497: a topology launch of N session nodes is judged against the same
/// rolling window as a lifecycle creation: the refusal names the window, the
/// count, the N slots it needs and when enough of the oldest creations have
/// left the window (injected clock), and the launch is admitted then with no
/// operator change.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn a_multi_node_topology_launch_is_refused_with_the_refill_time_and_admitted_after_expiry() {
    use rsi_common::manager_budget::CreationCapRefusalV1;
    let store = Store::open_in_memory().unwrap();
    let launch = ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "claude-sonnet-4-6".into(),
        effort: None,
    };
    let (pm, _lead) = fixture(
        &store,
        ManagerPolicyV2 {
            capabilities: vec![ManagerCapabilityV2::SessionCreate],
            max_created_sessions: 3,
            created_sessions_window_hours: 24,
            allowed_launches: vec![launch],
            ..Default::default()
        },
    );
    let now = Utc::now();
    let ago = |hours: i64| now - chrono::Duration::hours(hours);
    let frees = |hours: i64| {
        (ago(hours) + chrono::Duration::hours(24))
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
    };
    seed_topology_launches_at(
        &store,
        pm.project_id,
        pm.manager_session_id,
        pm.row_version,
        1,
        ago(20),
    );
    seed_topology_launches_at(
        &store,
        pm.project_id,
        pm.manager_session_id,
        pm.row_version,
        2,
        ago(2),
    );
    let allowance = |needed: i64, counted: i64, at| {
        store.manager_topology_creation_allowance_at(&pm, 3, 24, needed, counted, at)
    };

    // One node needs the oldest creation to leave; two need the second oldest.
    for (needed, frees_at) in [(1, frees(20)), (2, frees(2)), (3, frees(2))] {
        let error = allowance(needed, 0, now).unwrap_err().to_string();
        let detail = CreationCapRefusalV1::parse(&error).unwrap_or_else(|| panic!("{error}"));
        assert_eq!(detail.code, "manager_v2_creation_limit");
        assert_eq!((detail.used, detail.limit, detail.needed), (3, 3, needed));
        assert_eq!(detail.window_hours, Some(24));
        assert_eq!(
            detail.next_free_at.as_deref(),
            Some(frees_at.as_str()),
            "needed {needed}: {error}"
        );
    }
    // A relaunch of an already charged attempt needs no new slot.
    allowance(1, 1, now).expect("a charged attempt is not charged twice");

    // After the oldest left the window, one node fits and two do not.
    let after_oldest = ago(20) + chrono::Duration::hours(24) + chrono::Duration::minutes(1);
    allowance(1, 0, after_oldest).expect("one slot freed");
    assert!(allowance(2, 0, after_oldest).is_err());
    // After the rest left, the whole allowance is free again.
    let after_all = ago(2) + chrono::Duration::hours(24) + chrono::Duration::minutes(1);
    allowance(3, 0, after_all).expect("the window refilled");
}

/// #3416: a policy saved before the window field existed reads as 24 h.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn a_policy_without_a_window_reads_as_twenty_four_hours() {
    let mut value = serde_json::to_value(ManagerPolicyV2::default()).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .remove("created_sessions_window_hours");
    let policy: ManagerPolicyV2 = serde_json::from_value(value).unwrap();
    assert_eq!(policy.created_sessions_window_hours, 24);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn subscription_history_does_not_consume_metered_budget_or_make_spend_unknown() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    store
        .set_daemon_setting("claude_billing_mode", "subscription")
        .unwrap();
    store
        .set_daemon_setting("codex_billing_mode", "subscription")
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE sessions SET cost_usd=2500 WHERE id=?1",
            [lead.id.to_string()],
        )
        .unwrap();
    update_policy(&store, &config, |policy| policy.max_spend_usd = Some(1.0));
    let snapshot = store.manager_v2_resource_snapshot(&config).unwrap();
    assert_eq!(snapshot["known_spend_usd"], 0.0);
    assert_eq!(snapshot["unknown_spend_observations"], 0);
    assert!(
        store
            .manager_v2_resource_gate(&config, None, lead.provider, Some(lead.id))
            .is_ok()
    );
    store
        .set_daemon_setting("claude_billing_mode", "metered_api")
        .unwrap();
    store
        .set_daemon_setting("codex_billing_mode", "metered_api")
        .unwrap();
    let metered = store.manager_v2_resource_snapshot(&config).unwrap();
    assert!(metered["known_spend_usd"].as_f64().unwrap() >= 2500.0);
    assert!(
        store
            .manager_v2_resource_gate(&config, None, lead.provider, Some(lead.id))
            .is_err()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn metered_calls_inside_subscription_sessions_keep_their_budget_charge() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    store
        .set_daemon_setting("claude_billing_mode", "subscription")
        .unwrap();
    store
        .set_daemon_setting("codex_billing_mode", "subscription")
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE sessions SET cost_usd=2500 WHERE id=?1",
            [lead.id.to_string()],
        )
        .unwrap();
    store.conn.execute("INSERT INTO model_invocations(id,session_id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,provider,model,created_at,estimated_cost_usd)
        VALUES(?1,?2,'text.generate.rpc','direct_text','foreground','paid_capable','admitted','completed','test','OpenRouter','test-model',?3,3.0)",params![Uuid::new_v4().to_string(),lead.id.to_string(),Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos,true)]).unwrap();
    update_policy(&store, &config, |policy| policy.max_spend_usd = Some(1.0));
    let snapshot = store.manager_v2_resource_snapshot(&config).unwrap();
    assert_eq!(snapshot["known_spend_usd"], 3.0);
    assert_eq!(snapshot["unknown_spend_observations"], 0);
    assert!(
        store
            .manager_v2_resource_gate(&config, None, lead.provider, Some(lead.id))
            .is_err()
    );
}

#[cfg(test)]
fn codex_seven_day_snapshot(observed_at: DateTime<Utc>, reset: i64) -> ProviderRateLimitSnapshot {
    ProviderRateLimitSnapshot {
        provider: SessionProvider::Codex,
        status: Some("rejected".into()),
        rate_limit_type: None,
        overage_status: None,
        is_using_overage: false,
        observed_at,
        windows: vec![ProviderRateLimitWindow {
            window_key: "seven_day".into(),
            utilization: 1.0,
            resets_at_epoch: Some(reset),
        }],
        stale_reason: None,
    }
}

/// #4023: a stale exhausted window recorded for the previous account stops
/// refusing launches once a newer account change is observed.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn usage_gate_admits_after_an_account_change_newer_than_the_window() {
    let store = Store::open_in_memory().unwrap();
    let now = Utc::now();
    let reset = now.timestamp() + 5 * 24 * 3600;
    // The previous account's baseline, then its exhausted window.
    store
        .observe_provider_rate_limit_account(SessionProvider::Codex, "old-account", now)
        .unwrap();
    store
        .upsert_provider_rate_limit_snapshot(
            &codex_seven_day_snapshot(now + chrono::Duration::seconds(1), reset),
            None,
        )
        .unwrap();
    let refusal = store
        .manager_v2_provider_usage_gate(SessionProvider::Codex)
        .unwrap_err()
        .to_string();
    assert!(
        refusal.contains("manager_v2_provider_usage_limit"),
        "{refusal}"
    );
    // The login changed to another account after the window was recorded.
    assert!(
        store
            .observe_provider_rate_limit_account(
                SessionProvider::Codex,
                "new-account",
                Utc::now() + chrono::Duration::seconds(5),
            )
            .unwrap()
    );
    store
        .manager_v2_provider_usage_gate(SessionProvider::Codex)
        .expect("a stale window of the previous account must not refuse");
}

/// #4023: a window recorded after the login change is real current evidence and
/// still refuses; the refusal names the window kind, its reset and its source.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn usage_gate_still_refuses_a_window_recorded_after_the_account_change() {
    let store = Store::open_in_memory().unwrap();
    let now = Utc::now();
    let reset = now.timestamp() + 5 * 24 * 3600;
    store
        .observe_provider_rate_limit_account(SessionProvider::Codex, "old-account", now)
        .unwrap();
    store
        .observe_provider_rate_limit_account(
            SessionProvider::Codex,
            "new-account",
            now + chrono::Duration::seconds(1),
        )
        .unwrap();
    store
        .upsert_provider_rate_limit_snapshot(
            &codex_seven_day_snapshot(now + chrono::Duration::seconds(2), reset),
            None,
        )
        .unwrap();
    let refusal = store
        .manager_v2_provider_usage_gate(SessionProvider::Codex)
        .unwrap_err()
        .to_string();
    for part in [
        "manager_v2_provider_usage_limit",
        "seven_day",
        &DateTime::from_timestamp(reset, 0)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "codex plan-window poll",
        "account identity recorded",
        "ClearProviderUsageHold (TUI <Space>hu)",
    ] {
        assert!(refusal.contains(part), "missing {part:?} in {refusal}");
    }
}

/// #4023: with no stored account identity the refusal says so, so the manager
/// can tell the gap from a real limit.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn usage_gate_refusal_names_a_missing_account_identity() {
    let store = Store::open_in_memory().unwrap();
    let now = Utc::now();
    store
        .upsert_provider_rate_limit_snapshot(
            &codex_seven_day_snapshot(now, now.timestamp() + 3600),
            None,
        )
        .unwrap();
    let hold = store
        .provider_usage_window_hold(SessionProvider::Codex, now)
        .unwrap()
        .expect("held");
    assert!(!hold.account_identity_recorded);
    assert!(
        hold.describe(SessionProvider::Codex)
            .contains("account identity not recorded")
    );
}
