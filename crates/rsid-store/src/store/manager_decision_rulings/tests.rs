//! #1415: a delegated manager settles a non-gate decision record; real gates
//! stay operator-only; the owner withdraws; the operator archives stale ones.

use super::*;
use crate::store::manager_coordinator::tests::fixture;
use crate::store::manager_ledger::LedgerObservation;
use crate::test_support::test_session;
use rsi_common::global_manager::ConfigureGlobalManagerRequestV1;
use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
use rsi_common::types::SessionStatus;
use std::path::PathBuf;

fn policy() -> ManagerPolicyV2 {
    ManagerPolicyV2 {
        capabilities: vec![ManagerCapabilityV2::WorkPlan],
        ..Default::default()
    }
}

fn refusal(error: crate::error::DaemonError) -> String {
    match error {
        crate::error::DaemonError::InvalidParam(code) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The project manager posts one question as the lead's Epic owner.
fn ask(
    store: &Store,
    config: &HarnessManagerConfigV1,
    epic: Uuid,
    key: &str,
    question: &str,
    gate: Option<ManagerDecisionGateV2>,
) -> ManagerMutationReceiptV2 {
    store
        .manager_v2_commit_update(
            config.manager_session_id,
            &AgentManagerUpdateRequestV2 {
                project_id: None,
                fence: ManagerFenceV2 {
                    scope_version: config.row_version,
                    policy_version: 1,
                },
                idempotency_key: format!("ask-{key}"),
                change: ManagerUpdateV2::Decision {
                    key: key.into(),
                    expected_row_version: 0,
                    epic_id: epic,
                    question: question.into(),
                    request_id: None,
                    work_key: None,
                    gate,
                    options: vec![
                        ManagerDecisionOptionV2 {
                            label: "Option A".into(),
                            detail: Some("the cheap one".into()),
                            recommended: true,
                        },
                        ManagerDecisionOptionV2 {
                            label: "Option B".into(),
                            detail: None,
                            recommended: false,
                        },
                    ],
                },
            },
            &LedgerObservation::default(),
        )
        .unwrap()
}

/// A portfolio seat covering the project, in Execute mode.
fn global_seat(store: &Store, project: Uuid, mode: ManagerOperatingModeV2) -> Uuid {
    let seat = Uuid::new_v4();
    let mut row = test_session(seat, PathBuf::from("/tmp/portfolio-seat"));
    row.status = SessionStatus::Completed;
    store.insert_session(&row).unwrap();
    store
        .configure_global_manager(
            &ConfigureGlobalManagerRequestV1 {
                session_id: seat,
                project_ids: vec![project],
                allowed_launches: vec![ManagerLaunchChoiceV2 {
                    provider: rsi_common::types::SessionProvider::Claude,
                    model: "claude-opus-5-5".into(),
                    effort: Some("high".into()),
                }],
                project_policy: ManagerPolicyV2 {
                    mode,
                    capabilities: vec![ManagerCapabilityV2::WorkPlan],
                    ..Default::default()
                },
                expected_grant_version: 0,
                idempotency_key: format!("grant-{seat}"),
            },
            "operator:test",
        )
        .unwrap();
    seat
}

fn ruling_request(
    store: &Store,
    seat: Uuid,
    project: Uuid,
    key: &str,
    change: impl FnOnce(i64, String) -> ManagerUpdateV2,
    idempotency: &str,
    config: &HarnessManagerConfigV1,
) -> AgentManagerUpdateRequestV2 {
    let record = store
        .manager_v2_record(config, "decision", key)
        .unwrap()
        .unwrap();
    let principal = store
        .global_project_principal(seat, project)
        .unwrap()
        .unwrap();
    AgentManagerUpdateRequestV2 {
        project_id: Some(project),
        fence: principal.fence(),
        idempotency_key: idempotency.into(),
        change: change(
            record.row_version,
            record.payload["target_digest"].as_str().unwrap().into(),
        ),
    }
}

fn rule(
    key: &str,
    owner: Option<Uuid>,
    answer: &str,
) -> impl FnOnce(i64, String) -> ManagerUpdateV2 {
    let (key, answer) = (key.to_owned(), answer.to_owned());
    move |expected_row_version, target_digest| ManagerUpdateV2::DecisionRuling {
        key,
        expected_row_version,
        target_digest,
        answer,
        owner_manager_session_id: owner,
    }
}

fn pm_rule(
    store: &Store,
    config: &HarnessManagerConfigV1,
    key: &str,
    answer: &str,
    idempotency: &str,
) -> crate::error::Result<ManagerMutationReceiptV2> {
    let record = store
        .manager_v2_record(config, "decision", key)
        .unwrap()
        .unwrap();
    store.manager_v2_commit_update(
        config.manager_session_id,
        &AgentManagerUpdateRequestV2 {
            project_id: None,
            fence: ManagerFenceV2 {
                scope_version: config.row_version,
                policy_version: 1,
            },
            idempotency_key: idempotency.into(),
            change: ManagerUpdateV2::DecisionRuling {
                key: key.into(),
                expected_row_version: record.row_version,
                target_digest: record.payload["target_digest"].as_str().unwrap().into(),
                answer: answer.into(),
                owner_manager_session_id: None,
            },
        },
        &LedgerObservation::default(),
    )
}

fn operator_row(store: &Store, config: &HarnessManagerConfigV1, key: &str) -> Value {
    let page = store
        .manager_v2_inspect_operator(
            config.project_id,
            &AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Decisions,
                ..Default::default()
            },
        )
        .unwrap();
    page.rows
        .into_iter()
        .find(|row| row["key"] == key)
        .unwrap_or_else(|| panic!("decision row {key}"))
}

fn age(store: &Store, config: &HarnessManagerConfigV1, key: &str, days: i64) {
    let old = (Utc::now() - chrono::Duration::days(days))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    store
        .conn
        .execute(
            "UPDATE harness_manager_v2_records SET created_at=?1 WHERE project_id=?2 AND manager_session_id=?3 AND kind='decision' AND record_key=?4",
            params![old, config.project_id.to_string(), config.manager_session_id.to_string(), key],
        )
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn a_portfolio_ruling_settles_a_non_gate_record_and_unblocks_launches() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    let epic = lead.parent_id.unwrap();
    ask(
        &store,
        &config,
        epic,
        "census-access",
        "Who may read the census extract?",
        None,
    );
    ask(&store, &config, epic, "other", "Pick a name?", None);
    assert_eq!(
        refusal(store.manager_v2_decision_gate(&config, epic).unwrap_err()),
        "manager_v2_pending_operator_decision"
    );
    let pending = operator_row(&store, &config, "census-access");
    assert_eq!(pending["answerable_by"], "manager");
    assert_eq!(pending["blocks"]["launches"], true);
    assert_eq!(pending["blocks"]["epic_ids"], json!([epic]));

    let seat = global_seat(&store, config.project_id, ManagerOperatingModeV2::Execute);
    let request = ruling_request(
        &store,
        seat,
        config.project_id,
        "census-access",
        rule("census-access", Some(config.manager_session_id), "Option A"),
        "rule-census",
        &config,
    );
    let receipt = store
        .manager_v2_commit_update(seat, &request, &LedgerObservation::default())
        .unwrap();
    assert!(!receipt.deduplicated);

    // Ruling settles only this record; another pending decision still blocks.
    assert_eq!(
        refusal(store.manager_v2_decision_gate(&config, epic).unwrap_err()),
        "manager_v2_pending_operator_decision"
    );
    assert_eq!(operator_row(&store, &config, "other")["status"], "pending");
    let replay = store
        .manager_v2_commit_update(seat, &request, &LedgerObservation::default())
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.row_version, receipt.row_version);
    pm_rule(&store, &config, "other", "Option A", "rule-other").unwrap();
    store.manager_v2_decision_gate(&config, epic).unwrap();

    let settled = operator_row(&store, &config, "census-access");
    assert_eq!(settled["status"], "answered");
    assert_eq!(settled["answer"], "Option A");
    assert_eq!(settled["answered_by"]["kind"], "portfolio_manager");
    assert_eq!(settled["answered_by"]["session_id"], json!(seat));
    assert_eq!(settled["answered_by"]["node_label"], "global");
    assert_eq!(settled["answerable_by"], Value::Null);
    assert_eq!(settled["blocks"]["launches"], false);
    assert_eq!(settled["delivery"]["state"], "available_in_scoped_inbox");
    let events: Vec<_> = settled["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["event"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(events, ["asked", "ruled"]);
    assert_eq!(settled["options"][0]["recommended"], true);
    // The lead retrieves the ruling through its scoped inbox.
    assert!(
        store
            .manager_v2_unread_operator_answer(&config, epic, lead.id)
            .unwrap()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn the_owning_project_manager_rules_its_own_non_gate_record() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    let epic = lead.parent_id.unwrap();
    ask(
        &store,
        &config,
        epic,
        "ingest-contact",
        "Which contact does ingest use?",
        None,
    );
    pm_rule(&store, &config, "ingest-contact", "Option B", "pm-rule").unwrap();
    store.manager_v2_decision_gate(&config, epic).unwrap();
    let row = operator_row(&store, &config, "ingest-contact");
    assert_eq!(row["answered_by"]["kind"], "project_manager");
    assert_eq!(
        row["answered_by"]["session_id"],
        json!(config.manager_session_id)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn a_gate_record_refuses_a_manager_answer_and_stays_with_the_operator() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    let epic = lead.parent_id.unwrap();
    let seat = global_seat(&store, config.project_id, ManagerOperatingModeV2::Execute);
    // Declared gate, an undeclared question that names a gate, and a reserved
    // daemon key are all operator-only.
    ask(
        &store,
        &config,
        epic,
        "release",
        "Ship it?",
        Some(ManagerDecisionGateV2::MainOrRelease),
    );
    ask(
        &store,
        &config,
        epic,
        "money",
        "May we raise the monthly budget for the vendor?",
        None,
    );
    for (key, class, source) in [
        ("release", "main_or_release", "declared"),
        ("money", "spend", "daemon_scan"),
    ] {
        let row = operator_row(&store, &config, key);
        assert_eq!(row["gate"], class);
        assert_eq!(row["gate_source"], source);
        assert_eq!(row["answerable_by"], "operator");
        let by_portfolio = ruling_request(
            &store,
            seat,
            config.project_id,
            key,
            rule(key, Some(config.manager_session_id), "yes"),
            &format!("rule-{key}"),
            &config,
        );
        assert_eq!(
            refusal(
                store
                    .manager_v2_commit_update(seat, &by_portfolio, &LedgerObservation::default())
                    .unwrap_err()
            ),
            "manager_v2_decision_operator_gate",
            "{key}: portfolio"
        );
        assert_eq!(
            refusal(pm_rule(&store, &config, key, "yes", &format!("pm-{key}")).unwrap_err()),
            "manager_v2_decision_operator_gate",
            "{key}: owner"
        );
    }
    // Both still gate launches until the operator answers.
    assert!(store.manager_v2_decision_gate(&config, epic).is_err());
    let record = store
        .manager_v2_record(&config, "decision", "release")
        .unwrap()
        .unwrap();
    store
        .manager_v2_prepare_decision_answer(
            &AnswerHarnessManagerDecisionRequestV2 {
                project_id: config.project_id,
                fence: ManagerFenceV2 {
                    scope_version: config.row_version,
                    policy_version: 1,
                },
                decision_key: "release".into(),
                expected_row_version: record.row_version,
                target_digest: record.payload["target_digest"].as_str().unwrap().into(),
                answer: "Hold the release".into(),
                idempotency_key: "operator-release".into(),
            },
            |_, _, _, _| unreachable!(),
        )
        .unwrap();
    let answered = operator_row(&store, &config, "release");
    assert_eq!(answered["answered_by"]["kind"], "operator");
    // A daemon-created human question is a gate whatever its text says.
    assert!(
        classify_decision(
            "question:abc",
            &json!({"question":"Rename the module?"}),
            false,
            false
        )
        .is_some_and(|gate| gate.class == "human_approval")
    );
    assert!(
        classify_decision("approval:abc", &json!({"question":"Run ls?"}), false, false).is_some()
    );
    assert!(
        classify_decision(
            "choose",
            &json!({"question":"Rename the module?"}),
            false,
            false
        )
        .is_none()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn undeclared_gate_paraphrases_refuse_both_manager_rulings() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    let epic = lead.parent_id.unwrap();
    let seat = global_seat(&store, config.project_id, ManagerOperatingModeV2::Execute);
    for (index, (question, class)) in [
        ("May we merge this into `main`?", "main_or_release"),
        ("May we release v1.0 now?", "main_or_release"),
        ("May we buy another cloud instance?", "spend"),
        ("May we rotate the OAuth refresh token?", "credentials"),
        ("May we delete all user data?", "data_deletion"),
        ("May we remove the customer's records?", "data_deletion"),
        (
            "Can we approve this on behalf of the operator?",
            "human_approval",
        ),
        ("Can we grant human sign-off?", "human_approval"),
        ("Can we authorize $200 per month for this service?", "spend"),
        ("May we rotate the SSH key?", "credentials"),
        ("Can we publish version 2.0 now?", "main_or_release"),
        ("Can we tag v2.0 now?", "main_or_release"),
    ]
    .into_iter()
    .enumerate()
    {
        let key = format!("paraphrase-{index}");
        ask(&store, &config, epic, &key, question, None);
        let by_portfolio = ruling_request(
            &store,
            seat,
            config.project_id,
            &key,
            rule(&key, Some(config.manager_session_id), "yes"),
            &format!("portfolio-{key}"),
            &config,
        );
        assert_eq!(
            refusal(
                store
                    .manager_v2_commit_update(seat, &by_portfolio, &LedgerObservation::default())
                    .unwrap_err()
            ),
            "manager_v2_decision_operator_gate",
            "{question}: portfolio"
        );
        assert_eq!(
            refusal(pm_rule(&store, &config, &key, "yes", &format!("pm-{key}")).unwrap_err()),
            "manager_v2_decision_operator_gate",
            "{question}: owner"
        );
        let row = operator_row(&store, &config, &key);
        assert_eq!(row["status"], "pending");
        assert_eq!(row["gate"], class, "{question}");
        assert_eq!(row["gate_source"], "daemon_scan");
        assert_eq!(row["blocks"]["launches"], true);
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn a_portfolio_seat_in_status_mode_or_without_the_project_cannot_rule() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    let epic = lead.parent_id.unwrap();
    ask(&store, &config, epic, "choose", "Pick a name?", None);
    let reader = global_seat(&store, config.project_id, ManagerOperatingModeV2::Status);
    let request = ruling_request(
        &store,
        reader,
        config.project_id,
        "choose",
        rule("choose", Some(config.manager_session_id), "Option A"),
        "status-rule",
        &config,
    );
    assert_eq!(
        refusal(
            store
                .manager_v2_commit_update(reader, &request, &LedgerObservation::default())
                .unwrap_err()
        ),
        "manager_v2_capability_denied"
    );
    // A ruling that names no ledger of this project's chain is refused.
    let bogus = AgentManagerUpdateRequestV2 {
        change: ManagerUpdateV2::DecisionRuling {
            key: "choose".into(),
            expected_row_version: 1,
            target_digest: "x".into(),
            answer: "y".into(),
            owner_manager_session_id: Some(Uuid::new_v4()),
        },
        idempotency_key: "bogus-owner".into(),
        ..request
    };
    assert_eq!(
        refusal(
            store
                .manager_v2_commit_update(reader, &bogus, &LedgerObservation::default())
                .unwrap_err()
        ),
        "manager_v2_capability_denied",
        "a Status-mode seat is refused before the owner is looked at"
    );
    // The project manager cannot rule on another manager's ledger either.
    let pm_bogus = AgentManagerUpdateRequestV2 {
        project_id: None,
        fence: ManagerFenceV2 {
            scope_version: config.row_version,
            policy_version: 1,
        },
        idempotency_key: "pm-foreign".into(),
        change: ManagerUpdateV2::DecisionRuling {
            key: "choose".into(),
            expected_row_version: 1,
            target_digest: "x".into(),
            answer: "y".into(),
            owner_manager_session_id: Some(reader),
        },
    };
    assert_eq!(
        refusal(
            store
                .manager_v2_commit_update(
                    config.manager_session_id,
                    &pm_bogus,
                    &LedgerObservation::default()
                )
                .unwrap_err()
        ),
        "manager_v2_decision_owner_denied",
        "a project manager never rules on a portfolio seat's ledger"
    );
    let unknown = AgentManagerUpdateRequestV2 {
        idempotency_key: "pm-unknown".into(),
        change: ManagerUpdateV2::DecisionRuling {
            key: "choose".into(),
            expected_row_version: 1,
            target_digest: "x".into(),
            answer: "y".into(),
            owner_manager_session_id: Some(Uuid::new_v4()),
        },
        ..pm_bogus
    };
    assert_eq!(
        refusal(
            store
                .manager_v2_commit_update(
                    config.manager_session_id,
                    &unknown,
                    &LedgerObservation::default()
                )
                .unwrap_err()
        ),
        "manager_v2_decision_owner_unknown"
    );
    assert!(store.manager_v2_decision_gate(&config, epic).is_err());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn ruling_rechecks_live_seats_and_refuses_foreign_pms_workers_and_leads() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    let (foreign, _) = fixture(&store, policy());
    let epic = lead.parent_id.unwrap();
    ask(&store, &config, epic, "choose", "Pick a name?", None);
    let record = store
        .manager_v2_record(&config, "decision", "choose")
        .unwrap()
        .unwrap();
    let request = AgentManagerUpdateRequestV2 {
        project_id: Some(config.project_id),
        fence: ManagerFenceV2 {
            scope_version: config.row_version,
            policy_version: 1,
        },
        idempotency_key: "authority-rule".into(),
        change: rule("choose", None, "Option A")(
            record.row_version,
            record.payload["target_digest"].as_str().unwrap().into(),
        ),
    };
    let mut worker = lead.clone();
    worker.id = Uuid::new_v4();
    store.insert_session(&worker).unwrap();
    for (caller, code) in [
        (foreign.manager_session_id, "manager_project_not_in_scope"),
        (worker.id, "manager_scope_denied"),
        (lead.id, "manager_v2_manager_required"),
    ] {
        assert_eq!(
            refusal(
                store
                    .manager_v2_commit_update(caller, &request, &LedgerObservation::default())
                    .unwrap_err()
            ),
            code
        );
    }

    let seat = global_seat(&store, config.project_id, ManagerOperatingModeV2::Execute);
    let portfolio_request = ruling_request(
        &store,
        seat,
        config.project_id,
        "choose",
        rule("choose", Some(config.manager_session_id), "Option A"),
        "live-seat-rule",
        &config,
    );
    let mut successor = test_session(Uuid::new_v4(), PathBuf::from("/tmp/portfolio-successor"));
    successor.continued_from = Some(seat);
    store.insert_session(&successor).unwrap();
    assert!(store.transfer_global_seat(seat, successor.id).unwrap());
    assert_eq!(
        refusal(
            store
                .manager_v2_commit_update(seat, &portfolio_request, &LedgerObservation::default())
                .unwrap_err()
        ),
        "manager_node_custody_changed"
    );
    let successor_request = ruling_request(
        &store,
        successor.id,
        config.project_id,
        "choose",
        rule("choose", Some(config.manager_session_id), "Option A"),
        "revoked-seat-rule",
        &config,
    );
    let grant = store.global_seat_grant(successor.id).unwrap();
    store
        .revoke_global_manager(&rsi_common::global_manager::RevokeGlobalManagerRequestV1 {
            expected_grant_version: grant.grant_version,
            idempotency_key: "revoke-review-seat".into(),
        })
        .unwrap();
    assert_eq!(
        refusal(
            store
                .manager_v2_commit_update(
                    successor.id,
                    &successor_request,
                    &LedgerObservation::default()
                )
                .unwrap_err()
        ),
        "manager_project_not_in_scope"
    );

    let mut replacement = store
        .get_session(config.manager_session_id)
        .unwrap()
        .unwrap();
    replacement.id = Uuid::new_v4();
    store.insert_session(&replacement).unwrap();
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: config.project_id,
            session_id: replacement.id,
            epic_ids: Some(vec![epic]),
            group_ids: vec![],
            expected_row_version: config.row_version,
        })
        .unwrap();
    assert_eq!(
        refusal(
            store
                .manager_v2_commit_update(
                    config.manager_session_id,
                    &request,
                    &LedgerObservation::default()
                )
                .unwrap_err()
        ),
        "manager_scope_denied"
    );
    let unchanged = store
        .manager_v2_record(&config, "decision", "choose")
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.row_version, record.row_version);
    assert_eq!(unchanged.payload["status"], "pending");
    assert_eq!(unchanged.payload["history"].as_array().unwrap().len(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn an_area_manager_cannot_rule_on_its_own_decision_record() {
    use crate::store::manager_nodes::tests::{area_fixture, area_request};
    use rsi_common::manager_nodes::ManagerNodeSelectorV1;

    let (store, project, root, _, epics) = area_fixture(true);
    let node = store
        .appoint_area_node(&area_request(
            &store,
            project,
            &root,
            ManagerNodeSelectorV1::Selected {
                group_ids: vec![],
                epic_ids: vec![epics[0]],
            },
        ))
        .unwrap();
    let authority = store
        .manager_area_authority_current(node.seat_root_session_id)
        .unwrap()
        .unwrap();
    ask(
        &store,
        &authority.config,
        epics[0],
        "area-choice",
        "Pick a name?",
        None,
    );
    assert_eq!(
        refusal(
            pm_rule(
                &store,
                &authority.config,
                "area-choice",
                "Option A",
                "area-rule"
            )
            .unwrap_err()
        ),
        "manager_v2_decision_owner_denied"
    );
    let record = store
        .manager_v2_record(&authority.config, "decision", "area-choice")
        .unwrap()
        .unwrap();
    assert_eq!(record.payload["status"], "pending");
    assert_eq!(record.row_version, 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn the_owning_manager_withdraws_its_record_and_others_cannot() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    let epic = lead.parent_id.unwrap();
    ask(
        &store,
        &config,
        epic,
        "stale-question",
        "Pick the old thing?",
        None,
    );
    ask(&store, &config, epic, "other", "Pick a name?", None);
    let withdraw =
        |caller: Uuid, project: Option<Uuid>, fence: ManagerFenceV2, version: i64, key: &str| {
            store.manager_v2_commit_update(
                caller,
                &AgentManagerUpdateRequestV2 {
                    project_id: project,
                    fence,
                    idempotency_key: format!("withdraw-{caller}-{version}"),
                    change: ManagerUpdateV2::DecisionWithdraw {
                        key: key.into(),
                        expected_row_version: version,
                        reason: "superseded by the plan change".into(),
                    },
                },
                &LedgerObservation::default(),
            )
        };
    let record = store
        .manager_v2_record(&config, "decision", "stale-question")
        .unwrap()
        .unwrap();
    // A lead is not the owning manager.
    assert_eq!(
        refusal(
            withdraw(
                lead.id,
                None,
                ManagerFenceV2 {
                    scope_version: config.row_version,
                    policy_version: 1
                },
                record.row_version,
                "stale-question"
            )
            .unwrap_err()
        ),
        "manager_v2_manager_required"
    );
    // A stale version never withdraws.
    assert_eq!(
        refusal(
            withdraw(
                config.manager_session_id,
                None,
                ManagerFenceV2 {
                    scope_version: config.row_version,
                    policy_version: 1
                },
                record.row_version + 5,
                "stale-question"
            )
            .unwrap_err()
        ),
        "manager_v2_decision_changed"
    );
    assert!(store.manager_v2_decision_gate(&config, epic).is_err());
    withdraw(
        config.manager_session_id,
        None,
        ManagerFenceV2 {
            scope_version: config.row_version,
            policy_version: 1,
        },
        record.row_version,
        "stale-question",
    )
    .unwrap();
    assert_eq!(
        refusal(store.manager_v2_decision_gate(&config, epic).unwrap_err()),
        "manager_v2_pending_operator_decision"
    );
    assert_eq!(operator_row(&store, &config, "other")["status"], "pending");
    pm_rule(&store, &config, "other", "Option A", "withdraw-other-rule").unwrap();
    store.manager_v2_decision_gate(&config, epic).unwrap();
    let row = operator_row(&store, &config, "stale-question");
    assert_eq!(row["status"], "withdrawn");
    assert_eq!(row["answerable_by"], Value::Null);
    assert_eq!(row["blocks"]["launches"], false);
    assert_eq!(row["history"][1]["event"], "withdrawn");
    assert_eq!(row["history"][1]["actor"]["kind"], "project_manager");
    assert_eq!(row["history"][1]["note"], "superseded by the plan change");
    // The withdrawn record can no longer be ruled on.
    assert_eq!(
        refusal(pm_rule(&store, &config, "stale-question", "late", "late-rule").unwrap_err()),
        "manager_v2_decision_changed"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn the_operator_lists_stale_records_and_bulk_archive_deletes_nothing() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    let epic = lead.parent_id.unwrap();
    ask(&store, &config, epic, "old", "An old question?", None);
    ask(&store, &config, epic, "fresh", "A fresh question?", None);
    age(&store, &config, "old", 30);
    let decisions_before: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_v2_records WHERE kind='decision'",
            [],
            |row| row.get(0),
        )
        .unwrap();

    let listed = store
        .manager_v2_list_stale_decisions(&ListStaleManagerDecisionsRequestV2 {
            project_id: Some(config.project_id),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(listed.older_than_days, MANAGER_DECISION_STALE_DAYS_DEFAULT);
    assert!(listed.complete);
    assert_eq!(listed.rows.len(), 1);
    let stale = &listed.rows[0];
    assert_eq!(stale.decision.key, "old");
    assert_eq!(stale.stale_reason, "older_than_days");
    assert_eq!(stale.answerable_by, "manager");
    assert!(stale.age_seconds >= 29 * 86_400);
    assert!(operator_row(&store, &config, "old")["stale"] == true);
    assert!(operator_row(&store, &config, "fresh")["stale"] == false);
    // A wider bound lists nothing; a narrower one lists both.
    assert!(
        store
            .manager_v2_list_stale_decisions(&ListStaleManagerDecisionsRequestV2 {
                older_than_days: Some(60),
                ..Default::default()
            })
            .unwrap()
            .rows
            .is_empty()
    );

    let fresh = store
        .manager_v2_record(&config, "decision", "fresh")
        .unwrap()
        .unwrap();
    let fresh_ref = ManagerDecisionRefV2 {
        project_id: config.project_id,
        owner_manager_session_id: config.manager_session_id,
        scope_version: config.row_version,
        key: "fresh".into(),
        expected_row_version: fresh.row_version,
    };
    let done = store
        .manager_v2_archive_stale_decisions(&ArchiveStaleManagerDecisionsRequestV2 {
            items: vec![stale.decision.clone(), fresh_ref],
            older_than_days: None,
        })
        .unwrap();
    assert_eq!(done.archived.len(), 1);
    assert_eq!(done.archived[0].key, "old");
    assert_eq!(done.skipped.len(), 1);
    assert_eq!(done.skipped[0].decision.key, "fresh");
    assert_eq!(done.skipped[0].reason, "not_stale");

    // Archived, never deleted: the record, its payload and its history stay.
    let decisions_after: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_v2_records WHERE kind='decision'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(decisions_after, decisions_before);
    let old = store
        .manager_v2_record(&config, "decision", "old")
        .unwrap()
        .unwrap();
    assert!(old.archived);
    assert_eq!(old.payload["status"], "archived");
    assert_eq!(old.payload["question"], "An old question?");
    assert_eq!(old.payload["history"][1]["event"], "archived");
    assert_eq!(old.payload["history"][1]["actor"]["kind"], "operator");
    // The fresh question still gates; the archived one does not.
    assert!(store.manager_v2_decision_gate(&config, epic).is_err());
    store
        .conn
        .execute(
            "UPDATE harness_manager_v2_records SET archived=1 WHERE record_key='fresh'",
            [],
        )
        .unwrap();
    store.manager_v2_decision_gate(&config, epic).unwrap();

    // A retried archive reports the record, not a second archive.
    let again = store
        .manager_v2_archive_stale_decisions(&ArchiveStaleManagerDecisionsRequestV2 {
            items: vec![stale.decision.clone()],
            older_than_days: None,
        })
        .unwrap();
    assert!(again.archived.is_empty());
    assert_eq!(again.skipped[0].reason, "already_archived");
    // An archived record is never re-asked or ruled on.
    assert_eq!(
        refusal(pm_rule(&store, &config, "old", "late", "late-old").unwrap_err()),
        "manager_v2_decision_archived"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn a_record_whose_project_manager_is_gone_is_listed_stale() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    let epic = lead.parent_id.unwrap();
    ask(&store, &config, epic, "orphan", "Who was asking?", None);
    let live = store
        .manager_v2_list_stale_decisions(&ListStaleManagerDecisionsRequestV2::default())
        .unwrap();
    assert!(
        live.rows.is_empty(),
        "a fresh record of a live manager is not stale"
    );
    // The project is re-appointed to another manager: the old ledger is gone.
    let successor = Uuid::new_v4();
    let mut row = test_session(successor, PathBuf::from("/tmp/successor"));
    row.project_id = Some(config.project_id);
    store.insert_session(&row).unwrap();
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: config.project_id,
            session_id: successor,
            epic_ids: Some(vec![epic]),
            group_ids: vec![],
            expected_row_version: config.row_version,
        })
        .unwrap();
    let gone = store
        .manager_v2_list_stale_decisions(&ListStaleManagerDecisionsRequestV2::default())
        .unwrap();
    assert_eq!(gone.rows.len(), 1);
    assert_eq!(gone.rows[0].decision.key, "orphan");
    assert_eq!(gone.rows[0].stale_reason, "manager_gone");
    assert!(gone.rows[0].age_seconds < 3600);
    let done = store
        .manager_v2_archive_stale_decisions(&ArchiveStaleManagerDecisionsRequestV2 {
            items: vec![gone.rows[0].decision.clone()],
            older_than_days: None,
        })
        .unwrap();
    assert_eq!(done.archived.len(), 1);
    assert!(
        store
            .manager_v2_list_stale_decisions(&ListStaleManagerDecisionsRequestV2::default())
            .unwrap()
            .rows
            .is_empty()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn the_rulings_section_lists_what_a_seat_may_settle() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    let epic = lead.parent_id.unwrap();
    ask(&store, &config, epic, "plain", "Pick a name?", None);
    ask(
        &store,
        &config,
        epic,
        "gated",
        "Ship it?",
        Some(ManagerDecisionGateV2::MainOrRelease),
    );
    let seat = global_seat(&store, config.project_id, ManagerOperatingModeV2::Execute);
    let query = AgentManagerInspectRequestV2 {
        project_id: Some(config.project_id),
        section: ManagerInspectSectionV2::Rulings,
        ..Default::default()
    };
    let page = store.manager_v2_inspect(seat, &query).unwrap();
    assert_eq!(page.section, ManagerInspectSectionV2::Rulings);
    assert!(page.complete);
    assert_eq!(page.rows.len(), 1, "a gate is not on offer to a manager");
    let row = &page.rows[0];
    assert_eq!(row["key"], "plain");
    assert_eq!(
        row["owner_manager_session_id"],
        json!(config.manager_session_id)
    );
    assert_eq!(row["owner_role"], "project_manager");
    assert_eq!(row["answerable_by"], "manager");
    // The row carries exactly what the ruling needs.
    let ruling = AgentManagerUpdateRequestV2 {
        project_id: Some(config.project_id),
        fence: store
            .global_project_principal(seat, config.project_id)
            .unwrap()
            .unwrap()
            .fence(),
        idempotency_key: "rule-from-row".into(),
        change: ManagerUpdateV2::DecisionRuling {
            key: row["key"].as_str().unwrap().into(),
            expected_row_version: row["row_version"].as_i64().unwrap(),
            target_digest: row["target_digest"].as_str().unwrap().into(),
            answer: "Call it census".into(),
            owner_manager_session_id: Some(
                Uuid::parse_str(row["owner_manager_session_id"].as_str().unwrap()).unwrap(),
            ),
        },
    };
    store
        .manager_v2_commit_update(seat, &ruling, &LedgerObservation::default())
        .unwrap();
    assert!(
        store
            .manager_v2_inspect(seat, &query)
            .unwrap()
            .rows
            .is_empty()
    );
    // A lead never reads the rulings section.
    let denied = store
        .manager_v2_inspect(
            lead.id,
            &AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Rulings,
                ..Default::default()
            },
        )
        .unwrap_err();
    assert_eq!(refusal(denied), "manager_v2_scope_denied");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn a_reposted_question_keeps_its_asker_and_audit_trail() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    let epic = lead.parent_id.unwrap();
    ask(&store, &config, epic, "again", "First wording?", None);
    let first = store
        .manager_v2_record(&config, "decision", "again")
        .unwrap()
        .unwrap();
    store
        .manager_v2_commit_update(
            config.manager_session_id,
            &AgentManagerUpdateRequestV2 {
                project_id: None,
                fence: ManagerFenceV2 {
                    scope_version: config.row_version,
                    policy_version: 1,
                },
                idempotency_key: "ask-again-2".into(),
                change: ManagerUpdateV2::Decision {
                    key: "again".into(),
                    expected_row_version: first.row_version,
                    epic_id: epic,
                    question: "Second wording?".into(),
                    request_id: None,
                    work_key: None,
                    gate: None,
                    options: vec![],
                },
            },
            &LedgerObservation::default(),
        )
        .unwrap();
    let row = operator_row(&store, &config, "again");
    assert_eq!(row["question"], "Second wording?");
    assert_eq!(row["asked_by"]["kind"], "project_manager");
    let events: Vec<_> = row["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["event"].as_str().unwrap())
        .collect();
    assert_eq!(events, ["asked", "reasked"]);
}

// ── #1641 S3b: a topology node's question is the on-call manager's to rule on ──

/// The lead's session as a live topology node attempt of a running execution.
fn make_topology_node(store: &Store, session: Uuid, attempt_status: &str) {
    let execution = Uuid::new_v4();
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    store
        .conn
        .execute(
            "INSERT INTO topology_executions (id,topology_name_snapshot,workflow_id,definition_json,definition_digest,requested_by_kind,repo_root,base_commit,custody_plan_json,status,created_at,updated_at)
             VALUES (?1,'node-question','wf','{}','d','manager','/tmp/repo','0000000000000000000000000000000000000000','{}','running',?2,?2)",
            params![execution.to_string(), now],
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO topology_node_attempts (id,execution_id,node_id,iteration,attempt_no,node_kind,status,dedup_key,session_id,input_json,created_at,updated_at)
             VALUES (?1,?2,'A',0,1,'session',?3,?4,?5,'{}',?6,?6)",
            params![
                Uuid::new_v4().to_string(),
                execution.to_string(),
                attempt_status,
                format!("dedup-{execution}"),
                session.to_string(),
                now
            ],
        )
        .unwrap();
}

/// Like `raise_question` with the asker's own words.
fn raise_node_question(store: &Store, session: Uuid, text: &str) {
    raise_node_question_with_options(store, session, text, &[]);
}

fn raise_node_question_with_options(
    store: &Store,
    session: Uuid,
    text: &str,
    options: &[(&str, &str)],
) {
    use rsi_common::types::{
        ConversationEvent, EventType, PendingQuestion, QuestionItem, QuestionOption,
    };
    store
        .conn
        .execute(
            "UPDATE sessions SET provider='Claude' WHERE id=?1",
            [session.to_string()],
        )
        .unwrap();
    let question = PendingQuestion {
        questions: vec![QuestionItem {
            question: text.into(),
            header: "Node".into(),
            options: options
                .iter()
                .map(|(label, description)| QuestionOption {
                    label: (*label).into(),
                    description: (*description).into(),
                })
                .collect(),
            multi_select: false,
        }],
    };
    store
        .publish_pending_question_event(
            &ConversationEvent {
                id: 0,
                session_id: session,
                sequence: 1,
                event_type: EventType::ToolUse,
                role: None,
                created_at: Utc::now(),
                content: String::new(),
                tool_name: Some("AskUserQuestion".into()),
                tool_input: Some(Box::new(json!(question))),
                offload_id: None,
                tool_use_id: Some("node-question-1".into()),
                metadata: None,
            },
            None,
            &question,
        )
        .unwrap();
    store
        .update_session_status(session, SessionStatus::WaitingApproval)
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn topology_node_question_is_rulable_by_manager() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    make_topology_node(&store, lead.id, "running");
    raise_node_question(&store, lead.id, "Use the reserved migration number?");
    store
        .manager_v2_refresh_question_decisions(&config)
        .unwrap();
    let key = format!("question:{}", lead.id);

    // The board offers it to the manager, not the operator.
    let row = operator_row(&store, &config, &key);
    assert_eq!(row["answerable_by"], "manager");
    assert_eq!(row["gate"], Value::Null);
    let rulings = store.manager_v2_rulings_rows(&config, None, 16).unwrap().0;
    assert!(rulings.iter().any(|row| row["key"] == json!(key)));

    // The ruling queues the exact delivery to the same session.
    let record = store
        .manager_v2_record(&config, "decision", &key)
        .unwrap()
        .unwrap();
    let request = AgentManagerUpdateRequestV2 {
        project_id: None,
        fence: ManagerFenceV2 {
            scope_version: config.row_version,
            policy_version: 1,
        },
        idempotency_key: "rule-node".into(),
        change: ManagerUpdateV2::DecisionRuling {
            key: key.clone(),
            expected_row_version: record.row_version,
            target_digest: record.payload["target_digest"].as_str().unwrap().into(),
            answer: "Use number 162".into(),
            owner_manager_session_id: None,
        },
    };
    let receipt = store
        .manager_v2_commit_update(
            config.manager_session_id,
            &request,
            &LedgerObservation::default(),
        )
        .unwrap();
    assert!(!receipt.deduplicated);
    let settled = operator_row(&store, &config, &key);
    assert_eq!(settled["status"], "answer_queued");
    assert_eq!(settled["answer"], "Use number 162");
    assert_eq!(settled["answered_by"]["kind"], "project_manager");
    let deliveries = store
        .manager_v2_records_of_kind(&config, "decision_delivery")
        .unwrap();
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].payload["state"], "queued");
    assert_eq!(deliveries[0].payload["answer"], "Use number 162");
    assert_eq!(
        deliveries[0].payload["target"]["session_id"],
        json!(lead.id)
    );
    // The coordinator claims it for delivery through the normal path.
    let claimed = store
        .manager_v2_claim_decision_delivery(&config, Uuid::new_v4())
        .unwrap()
        .expect("a queued delivery for the node's session");
    assert_eq!(claimed.decision_key, key);
    assert_eq!(claimed.answer, "Use number 162");
    // A replay of the same ruling does not queue a second delivery.
    let replay = store
        .manager_v2_commit_update(
            config.manager_session_id,
            &request,
            &LedgerObservation::default(),
        )
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(
        store
            .manager_v2_records_of_kind(&config, "decision_delivery")
            .unwrap()
            .len(),
        1
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn topology_node_question_naming_main_stays_operator_gate() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    make_topology_node(&store, lead.id, "running");
    raise_node_question(&store, lead.id, "May we merge this into `main`?");
    store
        .manager_v2_refresh_question_decisions(&config)
        .unwrap();
    let key = format!("question:{}", lead.id);
    let row = operator_row(&store, &config, &key);
    assert_eq!(row["answerable_by"], "operator");
    assert_eq!(row["gate"], "main_or_release");
    assert_eq!(row["gate_source"], "daemon_scan");
    assert_eq!(
        refusal(pm_rule(&store, &config, &key, "yes", "rule-main").unwrap_err()),
        "manager_v2_decision_operator_gate"
    );
    let row = operator_row(&store, &config, &key);
    assert_eq!(row["status"], "pending");
    assert!(
        store
            .manager_v2_records_of_kind(&config, "decision_delivery")
            .unwrap()
            .is_empty()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn topology_node_question_option_naming_main_stays_operator_gate() {
    // #1704: the gate is in an answer choice, not in the question text.
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    make_topology_node(&store, lead.id, "running");
    raise_node_question_with_options(
        &store,
        lead.id,
        "How should I proceed?",
        &[
            ("Merge to main", "Land the work"),
            ("Keep working on rolling", "Continue"),
        ],
    );
    store
        .manager_v2_refresh_question_decisions(&config)
        .unwrap();
    let key = format!("question:{}", lead.id);
    let row = operator_row(&store, &config, &key);
    assert_eq!(row["answerable_by"], "operator");
    assert_eq!(row["gate"], "main_or_release");
    assert_eq!(row["gate_source"], "daemon_scan");
    assert_eq!(
        refusal(pm_rule(&store, &config, &key, "Merge to main", "rule-opt").unwrap_err()),
        "manager_v2_decision_operator_gate"
    );
    assert_eq!(operator_row(&store, &config, &key)["status"], "pending");
    assert!(
        store
            .manager_v2_records_of_kind(&config, "decision_delivery")
            .unwrap()
            .is_empty()
    );
    // A description alone can carry the gate too.
    assert!(
        classify_decision(
            &key,
            &json!({"question":"Proceed?","gate_text":"Proceed?\nYes\nDeploy to production"}),
            true,
            true
        )
        .is_some_and(|gate| gate.class == "main_or_release")
    );
    // A node question projected without the full text fails closed.
    assert!(
        classify_decision(&key, &json!({"question":"Rename the module?"}), true, true)
            .is_some_and(|gate| gate.source == "reserved_key")
    );
}

/// Rule on a running node's question and return its key (#1704).
fn rule_node_question(store: &Store, config: &HarnessManagerConfigV1, lead: Uuid) -> String {
    make_topology_node(store, lead, "running");
    raise_node_question(store, lead, "Use the reserved migration number?");
    store.manager_v2_refresh_question_decisions(config).unwrap();
    let key = format!("question:{lead}");
    pm_rule(store, config, &key, "Use number 162", "rule-cancel").unwrap();
    let deliveries = store
        .manager_v2_records_of_kind(config, "decision_delivery")
        .unwrap();
    assert_eq!(deliveries.len(), 1);
    // The ruling is bound to the exact attempt and execution.
    assert!(deliveries[0].payload["topology"]["attempt_id"].is_string());
    assert!(deliveries[0].payload["topology"]["execution_id"].is_string());
    key
}

fn cancel_topology_execution(store: &Store) {
    store
        .conn
        .execute("UPDATE topology_executions SET status='cancelling'", [])
        .unwrap();
}

fn assert_ruling_revoked_and_unblocked(store: &Store, config: &HarnessManagerConfigV1, key: &str) {
    let row = operator_row(store, config, key);
    assert_eq!(row["status"], "target_unavailable");
    assert_eq!(row["blocks"]["launches"], false);
    assert_eq!(row["blocks"]["epic_ids"], json!([]));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn queued_manager_ruling_is_revoked_by_cancellation_before_claim() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    let key = rule_node_question(&store, &config, lead.id);
    assert_eq!(
        operator_row(&store, &config, &key)["status"],
        "answer_queued"
    );
    cancel_topology_execution(&store);
    assert!(
        store
            .manager_v2_claim_decision_delivery(&config, Uuid::new_v4())
            .unwrap()
            .is_none()
    );
    let deliveries = store
        .manager_v2_records_of_kind(&config, "decision_delivery")
        .unwrap();
    assert_eq!(deliveries[0].payload["state"], "revoked");
    assert_ruling_revoked_and_unblocked(&store, &config, &key);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn claimed_manager_ruling_is_revoked_by_cancellation_before_effect() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    let key = rule_node_question(&store, &config, lead.id);
    let claimed = store
        .manager_v2_claim_decision_delivery(&config, Uuid::new_v4())
        .unwrap()
        .expect("claimable while the execution runs");
    cancel_topology_execution(&store);
    // Provider-effect admission refuses, and nothing marks the effect started.
    let error = store
        .manager_v2_set_decision_delivery(&claimed, "running", true, None)
        .unwrap_err();
    assert_eq!(refusal(error), "manager_v2_decision_topology_revoked");
    store
        .manager_v2_set_decision_delivery(&claimed, "blocked", false, Some("revoked".into()))
        .unwrap();
    assert_ruling_revoked_and_unblocked(&store, &config, &key);
}

/// Rewrite the stored delivery as the e81fef92a producer serialized it: a
/// manager ruling with no topology binding at all (#1704).
fn strip_delivery_binding(store: &Store) {
    let changed = store
        .conn
        .execute(
            "UPDATE harness_manager_v2_records
             SET payload_json=json_remove(payload_json,'$.topology')
             WHERE kind='decision_delivery'",
            [],
        )
        .unwrap();
    assert_eq!(changed, 1);
    let deliveries = store
        .manager_v2_records_of_kind(&store_config(store), "decision_delivery")
        .unwrap();
    assert!(deliveries[0].payload.get("topology").is_none());
}

fn store_config(store: &Store) -> HarnessManagerConfigV1 {
    let project: String = store
        .conn
        .query_row(
            "SELECT project_id FROM harness_manager_v2_records WHERE kind='decision_delivery'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    store
        .get_harness_manager(Uuid::parse_str(&project).unwrap())
        .unwrap()
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn legacy_unbound_manager_ruling_is_revoked_not_treated_as_operator() {
    // Deployed S3b queued a manager ruling with no binding; after this fix
    // lands and before delivery the execution is cancelled while the exact
    // pending question remains. The missing binding must not grant the
    // operator's exemption: the audit identity says a manager ruled.
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    let key = rule_node_question(&store, &config, lead.id);
    strip_delivery_binding(&store);
    cancel_topology_execution(&store);
    assert!(
        store
            .manager_v2_claim_decision_delivery(&config, Uuid::new_v4())
            .unwrap()
            .is_none()
    );
    let deliveries = store
        .manager_v2_records_of_kind(&config, "decision_delivery")
        .unwrap();
    assert_eq!(deliveries[0].payload["state"], "revoked");
    assert_ruling_revoked_and_unblocked(&store, &config, &key);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn legacy_unbound_manager_ruling_is_revoked_even_while_the_execution_runs() {
    // Nothing verifies which attempt an unbound manager ruling answered.
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    let key = rule_node_question(&store, &config, lead.id);
    strip_delivery_binding(&store);
    assert!(
        store
            .manager_v2_claim_decision_delivery(&config, Uuid::new_v4())
            .unwrap()
            .is_none()
    );
    assert_ruling_revoked_and_unblocked(&store, &config, &key);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn recovered_pre_effect_legacy_manager_claim_is_revoked() {
    // A pre-effect claim written by the deployed producer, recovered after a
    // restart, must not reach the effect-start admission as an operator answer.
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    let key = rule_node_question(&store, &config, lead.id);
    let claimed = store
        .manager_v2_claim_decision_delivery(&config, Uuid::new_v4())
        .unwrap()
        .expect("claimable while bound and live");
    assert!(claimed.topology.is_some());
    strip_delivery_binding(&store);
    // The restart returns the pre-effect claim to the queue; the claim check
    // then revokes it.
    assert_eq!(
        store
            .manager_v2_recover_decision_deliveries(Uuid::new_v4())
            .unwrap(),
        1
    );
    assert!(
        store
            .manager_v2_claim_decision_delivery(&config, Uuid::new_v4())
            .unwrap()
            .is_none()
    );
    assert_ruling_revoked_and_unblocked(&store, &config, &key);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn legacy_unbound_manager_claim_is_refused_at_effect_start() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    let key = rule_node_question(&store, &config, lead.id);
    let mut claimed = store
        .manager_v2_claim_decision_delivery(&config, Uuid::new_v4())
        .unwrap()
        .expect("claimable while bound and live");
    strip_delivery_binding(&store);
    claimed.topology = None;
    let error = store
        .manager_v2_set_decision_delivery(&claimed, "running", true, None)
        .unwrap_err();
    assert_eq!(refusal(error), "manager_v2_decision_topology_revoked");
    store
        .manager_v2_set_decision_delivery(&claimed, "blocked", false, Some("revoked".into()))
        .unwrap();
    assert_ruling_revoked_and_unblocked(&store, &config, &key);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn operator_answer_to_node_question_survives_topology_cancellation() {
    // The operator's answer path keeps its authority: it carries no binding.
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    make_topology_node(&store, lead.id, "running");
    raise_node_question(&store, lead.id, "Use the reserved migration number?");
    store
        .manager_v2_refresh_question_decisions(&config)
        .unwrap();
    let key = format!("question:{}", lead.id);
    let record = store
        .manager_v2_record(&config, "decision", &key)
        .unwrap()
        .unwrap();
    store
        .manager_v2_prepare_decision_answer(
            &rsi_common::harness_manager_v2::AnswerHarnessManagerDecisionRequestV2 {
                project_id: config.project_id,
                fence: ManagerFenceV2 {
                    scope_version: config.row_version,
                    policy_version: 1,
                },
                decision_key: key.clone(),
                expected_row_version: record.row_version,
                target_digest: record.payload["target_digest"].as_str().unwrap().into(),
                answer: "operator says go".into(),
                idempotency_key: "operator-node".into(),
            },
            |_, _, _, _| unreachable!(),
        )
        .unwrap();
    cancel_topology_execution(&store);
    let claimed = store
        .manager_v2_claim_decision_delivery(&config, Uuid::new_v4())
        .unwrap()
        .expect("the operator's answer is not fenced by the topology");
    assert!(claimed.topology.is_none());
    assert_eq!(claimed.answer, "operator says go");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn non_topology_question_stays_reserved_gate() {
    // An ordinary worker's question is a human gate whatever its text says:
    // the S3b classifier must not loosen it.
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    raise_node_question(&store, lead.id, "Use the reserved migration number?");
    store
        .manager_v2_refresh_question_decisions(&config)
        .unwrap();
    let key = format!("question:{}", lead.id);
    let row = operator_row(&store, &config, &key);
    assert_eq!(row["answerable_by"], "operator");
    assert_eq!(row["gate"], "human_approval");
    assert_eq!(row["gate_source"], "reserved_key");
    assert_eq!(
        refusal(pm_rule(&store, &config, &key, "Use 162", "rule-plain").unwrap_err()),
        "manager_v2_decision_operator_gate"
    );
    assert!(
        !store
            .manager_v2_rulings_rows(&config, None, 16)
            .unwrap()
            .0
            .iter()
            .any(|row| row["key"] == json!(key))
    );

    // Neither does a topology attempt that has ended, or one of another kind of
    // key, make a question rulable: only a live node attempt's own question.
    make_topology_node(&store, lead.id, "succeeded");
    let row = operator_row(&store, &config, &key);
    assert_eq!(row["answerable_by"], "operator");
    assert_eq!(row["gate_source"], "reserved_key");
    assert!(
        classify_decision(&key, &json!({"question":"Rename the module?"}), true, false).is_some()
    );
    assert!(
        classify_decision("approval:abc", &json!({"question":"Run ls?"}), true, true)
            .is_some_and(|gate| gate.source == "reserved_key")
    );
    assert!(
        classify_decision("accept:abc", &json!({"question":"Accept?"}), true, true)
            .is_some_and(|gate| gate.source == "reserved_key")
    );
    // A node question without its exact answer target cannot be delivered, so
    // it is not rulable either.
    assert!(
        classify_decision(&key, &json!({"question":"Rename the module?"}), false, true)
            .is_some_and(|gate| gate.source == "reserved_key")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn topology_decision_is_filed_once_and_its_gate_decides_who_may_rule() {
    // #1641 S3c: the daemon files a node's question under a `topology:` key.
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, policy());
    let epic = lead.parent_id.unwrap();
    let request = |key: &str, gate: Option<&str>, question: &str| TopologyDecisionRequest {
        key: key.into(),
        epic,
        question: question.into(),
        context: Some("execution e, node A".into()),
        gate: gate.map(str::to_owned),
        source_digest: "d1".into(),
        actor: json!({"kind":"topology_executor","session_id":null,"node_label":null}),
    };

    // The daemon cannot mint a reserved gate key.
    for key in ["question:x", "accept:x", "approval:x", "plain-key"] {
        assert_eq!(
            refusal(
                store
                    .manager_v2_put_topology_decision(&config, &request(key, None, "Rename?"))
                    .unwrap_err()
            ),
            "manager_v2_reserved_decision_key",
            "{key}"
        );
    }

    // Filed once: a second call returns the same record unchanged.
    let first = store
        .manager_v2_put_topology_decision(
            &config,
            &request("topology:e:A:0:1", None, "Keep the old field or rename it?"),
        )
        .unwrap();
    let again = store
        .manager_v2_put_topology_decision(
            &config,
            &request("topology:e:A:0:1", None, "A different text"),
        )
        .unwrap();
    assert_eq!(first.row_version, again.row_version);
    assert_eq!(
        again.payload["question"],
        "Keep the old field or rename it?"
    );
    assert_eq!(first.payload["status"], "pending");
    assert_eq!(first.payload["asked_by"]["kind"], "topology_executor");
    assert_eq!(first.payload["history"][0]["note"], "execution e, node A");
    assert_eq!(
        first.payload["target_digest"],
        json!(topology_decision_digest("topology:e:A:0:1", "d1").unwrap())
    );

    // No declared gate: the board offers it to the manager, who rules.
    let row = operator_row(&store, &config, "topology:e:A:0:1");
    assert_eq!(row["answerable_by"], "manager");
    assert_eq!(row["gate"], Value::Null);
    pm_rule(&store, &config, "topology:e:A:0:1", "Keep it", "rule-1").unwrap();
    let ruled = operator_row(&store, &config, "topology:e:A:0:1");
    assert_eq!(ruled["status"], "answered");
    assert_eq!(ruled["answer"], "Keep it");
    assert_eq!(ruled["answered_by"]["kind"], "project_manager");

    // A declared gate is the operator's alone.
    store
        .manager_v2_put_topology_decision(
            &config,
            &request("topology:e:B:0:1", Some("main_or_release"), "Which tag?"),
        )
        .unwrap();
    assert_eq!(
        operator_row(&store, &config, "topology:e:B:0:1")["answerable_by"],
        "operator"
    );
    assert_eq!(
        refusal(pm_rule(&store, &config, "topology:e:B:0:1", "v1", "rule-2").unwrap_err()),
        "manager_v2_decision_operator_gate"
    );
    // So is a question that names a gate without declaring it.
    store
        .manager_v2_put_topology_decision(
            &config,
            &request("topology:e:C:0:1", None, "May we merge to main?"),
        )
        .unwrap();
    assert_eq!(
        refusal(pm_rule(&store, &config, "topology:e:C:0:1", "yes", "rule-3").unwrap_err()),
        "manager_v2_decision_operator_gate"
    );

    // Withdrawal ends a pending record once, and nothing else.
    assert!(
        store
            .manager_v2_withdraw_topology_decision(&config, "topology:e:B:0:1", "cancelled")
            .unwrap()
    );
    assert!(
        !store
            .manager_v2_withdraw_topology_decision(&config, "topology:e:B:0:1", "cancelled")
            .unwrap()
    );
    assert_eq!(
        operator_row(&store, &config, "topology:e:B:0:1")["status"],
        "withdrawn"
    );
    assert!(
        !store
            .manager_v2_withdraw_topology_decision(&config, "topology:e:A:0:1", "late")
            .unwrap(),
        "an answered record is not withdrawn"
    );

    // An Epic the ledger does not cover is refused.
    let mut foreign = request("topology:e:D:0:1", None, "Rename?");
    foreign.epic = Uuid::new_v4();
    assert_eq!(
        refusal(
            store
                .manager_v2_put_topology_decision(&config, &foreign)
                .unwrap_err()
        ),
        "manager_v2_scope_denied"
    );
}
