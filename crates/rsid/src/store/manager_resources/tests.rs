use super::*;
use crate::model_control::{ExpectedUsage, InvocationCompletion, ModelAdmissionRequest};
use crate::store::StoreAdmissionOutcome;
use crate::store::manager_actions::ManagerActionOriginV2;
use crate::store::manager_coordinator::ManagerDecisionDeliveryV2;
pub(crate) use crate::store::manager_coordinator::tests::{fixture, raise_question};
use rsi_common::harness_manager_v2::*;
use rsi_common::model_control::{InvocationOwner, ModelInvocationPurpose, ModelTier};
use rsi_common::rpc::{ProviderRateLimitSnapshot, ProviderRateLimitWindow};

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

fn group(store: &Store, config: &HarnessManagerConfigV1) -> Uuid {
    store
        .get_session(config.epic_ids[0])
        .unwrap()
        .unwrap()
        .parent_id
        .unwrap()
}

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

pub(crate) fn admitted(store: &Store, session: &Session) -> Uuid {
    match admit(store, session) {
        StoreAdmissionOutcome::Admitted(id) => id,
        other => panic!("expected admission, got {other:?}"),
    }
}

pub(crate) fn complete(store: &Store, invocation: Uuid, cost: f64) {
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

fn assert_denied(outcome: StoreAdmissionOutcome, code: &str) {
    assert!(
        matches!(outcome, StoreAdmissionOutcome::Denied { ref reason, .. } if reason.contains(code)),
        "{outcome:?}; expected {code}"
    );
}

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

#[test]
fn manager_v2_resources_historical_creation_cannot_authorize_reparented_answer_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scope.db");
    let store = Store::open(&path).unwrap();
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

#[test]
fn manager_v2_resources_group_rotation_needs_receipt_and_retains_capacity_pause_and_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rotation.db");
    let store = Store::open(&path).unwrap();
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

#[test]
fn manager_v2_resources_cancellation_holds_capacity_until_actual_settlement_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("capacity.db");
    let store = Store::open(&path).unwrap();
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

#[test]
fn manager_v2_resources_first_invocation_preserves_legacy_floor_and_unknown_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spend.db");
    let store = Store::open(&path).unwrap();
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

#[test]
fn manager_v2_resources_proven_zero_origin_allows_measured_ledger_cost_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zero-origin.db");
    let store = Store::open(&path).unwrap();
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
    complete(&store, next, 1.0);
    assert_denied(admit(&store, &lead), "spend_exhausted");
}

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
            fence: ManagerFenceV2 {
                scope_version: config.row_version,
                policy_version: policy.row_version,
            },
            idempotency_key: Uuid::new_v4().to_string(),
            operation: ManagerActionV2::CreateSession {
                parent_id: config.epic_ids[0],
                kind: SessionKind::Task,
                query: "Scoped resource launch".into(),
                launch,
            },
        },
    )
}

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

#[test]
fn manager_v2_resources_rotation_reserves_capacity_before_successor_publication() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rotation-admission.db");
    let store = Store::open(&path).unwrap();
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

#[test]
fn manager_v2_resources_concurrent_store_admissions_reserve_one_scoped_slot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("race.db");
    let store = Store::open(&path).unwrap();
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

#[test]
fn manager_v2_resources_fresh_unpublished_admission_retains_capacity_and_cost_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unpublished.db");
    let store = Store::open(&path).unwrap();
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
#[test]
fn manager_v2_resources_question_cleanup_reopen_preserves_unknown_usage_and_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cleanup.db");
    let store = Store::open(&path).unwrap();
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

#[test]
fn manager_v2_resources_selected_pending_uuid_survives_scope_revision_and_reopen() {
    use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pending-scope.db");
    let store = Store::open(&path).unwrap();
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
                operation: PreparedManagerActionV2::RetryLead {
                    epic_id: epic,
                    message: "retry after retired cohort".into(),
                    launch: Some(launch),
                },
            },
        )
        .unwrap();
}

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
#[test]
fn manager_v2_answer_reopen_preserves_only_exact_invocation_measurements() {
    for bind_answer in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("answer.db");
        let store = Store::open(&path).unwrap();
        let (_, mut lead) = fixture(&store, ManagerPolicyV2::default());
        lead.cost_usd = Some(5.0);
        lead.total_input_tokens = Some(500);
        store.update_session_metadata(&lead).unwrap();
        let mut request = admission_request(&lead);
        request.purpose = ModelInvocationPurpose::SessionContinueResume;
        request.trigger = "continue_session".into();
        request.dedup_key = Some(format!("manager.answer:{}", Uuid::new_v4()));
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
    assert_eq!(
        row["created_sessions"],
        serde_json::json!({"used":2,"limit":5,"remaining":3})
    );
}
