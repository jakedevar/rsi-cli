#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use super::*;
use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
use rsi_common::types::{SandboxCleanupState, SandboxKind, SessionProvider};

fn source_work(f: &Fixture, key: &str, sha: &str) -> (HarnessManagerConfigV1, i64, WorkRecord) {
    f.store
        .conn
        .execute(
            "UPDATE sessions SET model='gpt-6-sol' WHERE id=?1",
            [f.lead.to_string()],
        )
        .unwrap();
    work(f, key);
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let (row, mut record) = f.store.manager_v2_work(&config, key).unwrap();
    record.source_commit = Some(sha.into());
    record.source_session_id = Some(f.lead);
    let row = f
        .store
        .manager_v2_put_record(
            &config,
            "work",
            key,
            Some(f.epic),
            row.row_version,
            &json!(record),
        )
        .unwrap();
    (config, row.row_version, record)
}

fn review_request(key: &str, version: i64, sha: &str) -> AgentManagerUpdateRequestV2 {
    request(
        ManagerUpdateV2::RequestReview {
            key: key.into(),
            expected_row_version: version,
            source_commit: sha.into(),
            query: "Review exact source".into(),
            launch: ManagerLaunchChoiceV2 {
                provider: SessionProvider::Claude,
                model: "claude-sonnet-5".into(),
                effort: None,
            },
        },
        "review",
    )
}

fn enable_review_policy(f: &Fixture) {
    f.store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: f.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: "review-policy".into(),
            policy: ManagerPolicyV2 {
                mode: ManagerOperatingModeV2::Execute,
                capabilities: vec![
                    ManagerCapabilityV2::WorkPlan,
                    ManagerCapabilityV2::SessionCreate,
                ],
                max_created_sessions: 2,
                allowed_launches: vec![ManagerLaunchChoiceV2 {
                    provider: SessionProvider::Claude,
                    model: "claude-sonnet-5".into(),
                    effort: None,
                }],
                ..Default::default()
            },
        })
        .unwrap();
}

#[test]
fn premature_accept_refuses_without_creating_an_operator_decision() {
    let f = fixture();
    let (config, version, _) = source_work(&f, "premature", &"a".repeat(40));
    let error = f
        .store
        .manager_v2_commit_update(
            f.manager,
            &request(
                ManagerUpdateV2::Accept {
                    key: "premature".into(),
                    expected_row_version: version,
                },
                "premature-accept",
            ),
            &LedgerObservation::default(),
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_independent_evidence_required")
    );
    assert!(
        f.store
            .manager_v2_record(&config, "decision", "accept:premature")
            .unwrap()
            .is_none()
    );
    let (same_row, same_work) = f.store.manager_v2_work(&config, "premature").unwrap();
    assert_eq!(same_row.row_version, version);
    assert!(same_work.acceptance.is_none());
}

#[test]
fn review_request_distinguishes_work_version_from_source_change() {
    let f = fixture();
    let sha = "a".repeat(40);
    let (config, version, record) = source_work(&f, "versioned", &sha);
    let observed = LedgerObservation {
        source_commit: Some(sha.clone()),
        ..Default::default()
    };
    let stale = f
        .store
        .manager_review_reserve_on(
            f.manager,
            &config,
            &review_request("versioned", version - 1, &sha),
            &record,
            version,
            &observed,
        )
        .unwrap_err();
    assert!(
        stale
            .to_string()
            .contains("manager_review_work_version_changed")
    );
    let moved = f
        .store
        .manager_review_reserve_on(
            f.manager,
            &config,
            &review_request("versioned", version, &"b".repeat(40)),
            &record,
            version,
            &observed,
        )
        .unwrap_err();
    assert!(moved.to_string().contains("manager_review_source_changed"));
}

#[test]
fn review_reservation_requires_a_known_different_recorded_family() {
    let f = fixture();
    let sha = "a".repeat(40);
    let (config, version, record) = source_work(&f, "families", &sha);
    let observed = LedgerObservation {
        source_commit: Some(sha.clone()),
        ..Default::default()
    };
    let mut review = review_request("families", version, &sha);
    if let ManagerUpdateV2::RequestReview { launch, .. } = &mut review.change {
        launch.provider = SessionProvider::OpenRouter;
        launch.model = "openai/gpt-6-sol".into();
    }
    let error = f
        .store
        .manager_review_reserve_on(f.manager, &config, &review, &record, version, &observed)
        .unwrap_err();
    assert!(error.to_string().contains("manager_review_family_conflict"));
    let count: i64 = f
        .store
        .conn
        .query_row(
            "SELECT count(*) FROM manager_review_assignments WHERE work_key='families'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);

    if let ManagerUpdateV2::RequestReview { launch, .. } = &mut review.change {
        launch.model = "unrecognized-model".into();
    }
    let error = f
        .store
        .manager_review_reserve_on(f.manager, &config, &review, &record, version, &observed)
        .unwrap_err();
    assert!(error.to_string().contains("manager_review_family_conflict"));

    if let ManagerUpdateV2::RequestReview { launch, .. } = &mut review.change {
        launch.model = "z-ai/glm-5.3-flashx".into();
    }
    f.store
        .conn
        .execute(
            "UPDATE sessions SET model='unrecognized-model' WHERE id=?1",
            [f.lead.to_string()],
        )
        .unwrap();
    let error = f
        .store
        .manager_review_reserve_on(f.manager, &config, &review, &record, version, &observed)
        .unwrap_err();
    assert!(error.to_string().contains("manager_review_family_conflict"));
    f.store
        .conn
        .execute(
            "UPDATE sessions SET model='gpt-6-sol' WHERE id=?1",
            [f.lead.to_string()],
        )
        .unwrap();
    f.store
        .manager_review_reserve_on(f.manager, &config, &review, &record, version, &observed)
        .unwrap();
    let count: i64 = f
        .store
        .conn
        .query_row(
            "SELECT count(*) FROM manager_review_assignments WHERE work_key='families'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn review_author_family_falls_back_to_latest_recorded_invocation() {
    let f = fixture();
    let sha = "a".repeat(40);
    let (config, version, record) = source_work(&f, "fallback-family", &sha);
    f.store
        .conn
        .execute(
            "UPDATE sessions SET model=NULL WHERE id=?1",
            [f.lead.to_string()],
        )
        .unwrap();
    for (index, model) in ["claude-sonnet-5", "gpt-6-sol"].into_iter().enumerate() {
        f.store
            .conn
            .execute(
                "INSERT INTO model_invocations
             (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
              trigger_source,session_id,project_id,dedup_key,model,created_at)
             VALUES(?1,'session_continue','session_lifecycle','foreground','paid_capable',
                    'admitted','completed','review-family-test',?2,?3,?4,?5,?6)",
                params![
                    Uuid::new_v4().to_string(),
                    f.lead.to_string(),
                    f.project.to_string(),
                    format!("review-family-{index}"),
                    model,
                    format!("2026-09-23T00:00:0{index}.000000000Z")
                ],
            )
            .unwrap();
    }
    f.store
        .manager_review_reserve_on(
            f.manager,
            &config,
            &review_request("fallback-family", version, &sha),
            &record,
            version,
            &LedgerObservation {
                source_commit: Some(sha),
                ..Default::default()
            },
        )
        .unwrap();
}

/// Record `sha` as the Work's source (when it moved) and request a review of
/// it as `caller` with `model`.
fn request_at(
    f: &Fixture,
    caller: Uuid,
    key: &str,
    sha: &str,
    model: &str,
) -> crate::error::Result<Uuid> {
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let (mut row, mut record) = f.store.manager_v2_work(&config, key).unwrap();
    if record.source_commit.as_deref() != Some(sha) {
        record.source_commit = Some(sha.into());
        row = f
            .store
            .manager_v2_put_record(
                &config,
                "work",
                key,
                Some(f.epic),
                row.row_version,
                &json!(record),
            )
            .unwrap();
    }
    let mut review = review_request(key, row.row_version, sha);
    if let ManagerUpdateV2::RequestReview { launch, .. } = &mut review.change {
        launch.model = model.into();
    }
    // Production reserves inside the ledger transaction; the deferred
    // supersession key is checked at its commit.
    let tx = f.store.conn.unchecked_transaction().unwrap();
    let receipt = f.store.manager_review_reserve_on(
        caller,
        &config,
        &review,
        &record,
        row.row_version,
        &LedgerObservation {
            source_commit: Some(sha.into()),
            ..Default::default()
        },
    )?;
    tx.commit().unwrap();
    Ok(Uuid::parse_str(&receipt.key).unwrap())
}

fn refusal(result: crate::error::Result<Uuid>) -> String {
    result.unwrap_err().to_string()
}

/// These A1 budget tests intentionally spend repeated Anthropic rounds. Give
/// them the audited manager exception that A3 requires for those rounds.
fn allow_anthropic_rounds(f: &Fixture, work_key: &str) {
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let key = "review-family-override:1:anthropic";
    f.store
        .manager_v2_put_record(
            &config,
            "decision",
            key,
            Some(f.epic),
            0,
            &json!({"key":key,"epic_id":f.epic,"question":"A1 budget fixture",
            "request_id":null,"work_key":work_key,"target_digest":"fixture",
            "target_row_version":null,"request_row_version":null,
            "status":"pending","answer":null,"delivery":null}),
        )
        .unwrap();
    f.store
        .manager_v2_event(&config, Some(f.manager), "decision", key, 1, &json!({}))
        .unwrap();
}

/// Synthetic custody and invocation references: these tests exercise budget
/// accounting, not allocation or custody, so foreign keys are relaxed for the
/// write only. Reviewer sessions are real because contributor provenance reads
/// their recorded provider family.
fn synthetic(f: &Fixture, sql: &str, values: &[&dyn rusqlite::ToSql]) {
    f.store
        .conn
        .execute_batch("PRAGMA foreign_keys=OFF")
        .unwrap();
    let result = f.store.conn.execute(sql, values);
    f.store
        .conn
        .execute_batch("PRAGMA foreign_keys=ON")
        .unwrap();
    result.unwrap();
}

fn stamp() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// reserved -> allocating -> active with a launched reviewer.
fn launch_review(f: &Fixture, assignment: Uuid) {
    let model: String = f.store.conn.query_row(
        "SELECT json_extract(request_json,'$.launch.model') FROM manager_review_assignments WHERE assignment_id=?1",
        [assignment.to_string()], |row| row.get(0),
    ).unwrap();
    let mut reviewer = f.store.get_session(f.lead).unwrap().unwrap();
    reviewer.id = Uuid::new_v4();
    reviewer.session_kind = SessionKind::Research;
    reviewer.model = Some(model);
    f.store.insert_session(&reviewer).unwrap();
    synthetic(
        f,
        "UPDATE manager_review_assignments
            SET state='allocating',reviewer_session_id=?2,action_operation_id=?3,
                row_version=row_version+1,updated_at=?4
          WHERE assignment_id=?1 AND state='reserved'",
        &[
            &assignment.to_string(),
            &reviewer.id.to_string(),
            &Uuid::new_v4().to_string(),
            &stamp(),
        ],
    );
    synthetic(
        f,
        "UPDATE manager_review_assignments
            SET state='active',reviewer_invocation_id=?2,reviewer_custody_id=?3,
                reviewer_custody_generation=1,row_version=row_version+1,updated_at=?4
          WHERE assignment_id=?1 AND state='allocating'",
        &[
            &assignment.to_string(),
            &Uuid::new_v4().to_string(),
            &Uuid::new_v4().to_string(),
            &stamp(),
        ],
    );
}

/// Launch, record an immutable receipt, and settle the row as submitted.
fn submit_review(f: &Fixture, assignment: Uuid) {
    launch_review(f, assignment);
    synthetic(
        f,
        "INSERT INTO manager_review_receipts(
             receipt_id,assignment_id,source_sha,reviewer_session_id,reviewer_invocation_id,
             reviewer_custody_id,reviewer_custody_generation,verdict,idempotency_key,
             request_fingerprint,created_at)
         SELECT ?1,assignment_id,source_sha,reviewer_session_id,reviewer_invocation_id,
                reviewer_custody_id,reviewer_custody_generation,'changes_requested','verdict',
                request_fingerprint,?2
           FROM manager_review_assignments WHERE assignment_id=?3",
        &[
            &Uuid::new_v4().to_string(),
            &stamp(),
            &assignment.to_string(),
        ],
    );
    let at = stamp();
    synthetic(
        f,
        "UPDATE manager_review_assignments
            SET state='submitted',row_version=row_version+1,updated_at=?2,terminal_at=?2
          WHERE assignment_id=?1 AND state='active'",
        &[&assignment.to_string(), &at],
    );
}

fn fail_review(f: &Fixture, assignment: Uuid) {
    let at = stamp();
    f.store
        .conn
        .execute(
            "UPDATE manager_review_assignments
                SET state='failed',failure_code='manager_review_receipt_missing_final',
                    row_version=row_version+1,updated_at=?2,terminal_at=?2
              WHERE assignment_id=?1",
            params![assignment.to_string(), at],
        )
        .unwrap();
}

fn review_rows(f: &Fixture, key: &str) -> (i64, i64) {
    f.store
        .conn
        .query_row(
            "SELECT count(*),count(*) FILTER (WHERE superseded_by_assignment_id IS NULL)
               FROM manager_review_assignments WHERE work_key=?1",
            [key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

const SONNET: &str = "claude-sonnet-5";
const OPUS: &str = "claude-opus-5-5";
const GLM: &str = "z-ai/glm-5.3-flashx";

#[test]
fn review_round_budget_counts_three_non_superseded_assignments_per_revision() {
    let f = fixture();
    source_work(&f, "rounds", &"a".repeat(40));
    for (letter, model) in [('a', SONNET), ('b', SONNET), ('c', OPUS)] {
        request_at(
            &f,
            f.manager,
            "rounds",
            &letter.to_string().repeat(40),
            model,
        )
        .unwrap();
    }
    let error = refusal(request_at(&f, f.manager, "rounds", &"d".repeat(40), GLM));
    assert!(error.contains("manager_review_round_budget"), "{error}");
    assert_eq!(review_rows(&f, "rounds"), (3, 3));
}

/// #599 A1 test 1: S2's same-SHA exemption is withdrawn; a re-request of a
/// launched review at capacity is a fourth attempt.
#[test]
fn same_sha_rerequest_at_capacity_is_refused() {
    let f = fixture();
    source_work(&f, "supersede", &"a".repeat(40));
    let mut current = Uuid::nil();
    for (letter, model) in [('a', SONNET), ('b', SONNET), ('c', OPUS)] {
        current = request_at(
            &f,
            f.manager,
            "supersede",
            &letter.to_string().repeat(40),
            model,
        )
        .unwrap();
    }
    launch_review(&f, current);
    let error = refusal(request_at(&f, f.manager, "supersede", &"c".repeat(40), GLM));
    assert!(error.contains("manager_review_round_budget"), "{error}");
    assert_eq!(review_rows(&f, "supersede"), (3, 3));
    let state: String = f
        .store
        .conn
        .query_row(
            "SELECT state FROM manager_review_assignments WHERE assignment_id=?1",
            [current.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "active");
}

/// #599 A1 test 2.
#[test]
fn supersede_churn_cannot_exceed_three_attempts() {
    let f = fixture();
    let sha = "a".repeat(40);
    source_work(&f, "churn", &sha);
    allow_anthropic_rounds(&f, "churn");
    for model in [SONNET, OPUS, GLM] {
        let assignment = request_at(&f, f.manager, "churn", &sha, model).unwrap();
        submit_review(&f, assignment);
    }
    let error = refusal(request_at(&f, f.manager, "churn", &sha, SONNET));
    assert!(error.contains("manager_review_round_budget"), "{error}");
    assert_eq!(review_rows(&f, "churn"), (3, 1));
}

/// #599 A1 test 3: a chain that failed without a receipt, infra-retry
/// successors, and a replacement superseded before launch spend no budget.
#[test]
fn failed_without_receipt_and_infra_retries_do_not_spend_budget() {
    let f = fixture();
    source_work(&f, "exempt", &"a".repeat(40));
    allow_anthropic_rounds(&f, "exempt");
    let failed = request_at(&f, f.manager, "exempt", &"a".repeat(40), SONNET).unwrap();
    launch_review(&f, failed);
    fail_review(&f, failed);

    let retried = request_at(&f, f.manager, "exempt", &"b".repeat(40), SONNET).unwrap();
    let first_retry = f
        .store
        .manager_review_infra_retry_for_test(retried)
        .unwrap();
    let second_retry = f
        .store
        .manager_review_infra_retry_for_test(first_retry)
        .unwrap();
    let retry_of: String = f
        .store
        .conn
        .query_row(
            "SELECT json_extract(request_json,'$.infra_retry_of')
               FROM manager_review_assignments WHERE assignment_id=?1",
            [second_retry.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retry_of, first_retry.to_string());

    request_at(&f, f.manager, "exempt", &"c".repeat(40), SONNET).unwrap();
    // Replacing an unlaunched row starts the attempt over without spending one.
    request_at(&f, f.manager, "exempt", &"c".repeat(40), SONNET).unwrap();

    // Two counted attempts (the retry chain and the replacement), so this
    // request is the closure round.
    let error = refusal(request_at(&f, f.manager, "exempt", &"d".repeat(40), SONNET));
    assert!(
        error.contains("manager_review_closure_specialist_required"),
        "{error}"
    );
    request_at(&f, f.manager, "exempt", &"d".repeat(40), OPUS).unwrap();
    let error = refusal(request_at(&f, f.manager, "exempt", &"e".repeat(40), GLM));
    assert!(error.contains("manager_review_round_budget"), "{error}");
    assert_eq!(review_rows(&f, "exempt"), (7, 4));
}

/// #599 A1 test 4.
#[test]
fn closure_specialist_rejects_a_model_used_by_a_superseded_round() {
    let f = fixture();
    let sha = "a".repeat(40);
    source_work(&f, "closure", &sha);
    allow_anthropic_rounds(&f, "closure");
    let first = request_at(&f, f.manager, "closure", &sha, SONNET).unwrap();
    launch_review(&f, first);
    // Superseding a launched review keeps it a counted attempt.
    request_at(&f, f.manager, "closure", &sha, OPUS).unwrap();
    for model in [SONNET, OPUS] {
        let error = refusal(request_at(&f, f.manager, "closure", &"b".repeat(40), model));
        assert!(
            error.contains("manager_review_closure_specialist_required"),
            "{model}: {error}"
        );
    }
    request_at(&f, f.manager, "closure", &"b".repeat(40), GLM).unwrap();
    assert_eq!(review_rows(&f, "closure"), (3, 2));
}

/// #599 A1 test 5.
#[test]
fn lead_cannot_supersede_a_manager_requested_assignment() {
    let f = fixture();
    let sha = "a".repeat(40);
    source_work(&f, "owned", &sha);
    let manager_row = request_at(&f, f.manager, "owned", &sha, SONNET).unwrap();
    let error = refusal(request_at(&f, f.lead, "owned", &sha, OPUS));
    assert!(error.contains("manager_review_manager_owned"), "{error}");
    let current: String = f
        .store
        .conn
        .query_row(
            "SELECT assignment_id FROM manager_review_assignments
              WHERE work_key='owned' AND superseded_by_assignment_id IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(current, manager_row.to_string());
    assert_eq!(review_rows(&f, "owned"), (1, 1));

    // A lead may replace rows its own lineage requested; the manager may
    // replace any row.
    let other = "b".repeat(40);
    let lead_row = request_at(&f, f.lead, "owned", &other, SONNET).unwrap();
    let lead_replacement = request_at(&f, f.lead, "owned", &other, SONNET).unwrap();
    let requester: String = f
        .store
        .conn
        .query_row(
            "SELECT json_extract(request_json,'$.requester_session_id')
               FROM manager_review_assignments
              WHERE assignment_id=?1 AND superseded_by_assignment_id IS NULL",
            [lead_replacement.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(requester, f.lead.to_string());
    assert_ne!(lead_row, lead_replacement);
    request_at(&f, f.manager, "owned", &other, OPUS).unwrap();
    assert_eq!(review_rows(&f, "owned"), (4, 2));
}

#[test]
fn pending_acceptance_allows_only_the_bound_reviewer_action_to_cross_decision_gate() {
    let f = fixture();
    let sha = "a".repeat(40);
    let (config, version, record) = source_work(&f, "held", &sha);
    enable_review_policy(&f);
    f.store
        .manager_v2_put_record(
            &config,
            "decision",
            "accept:held",
            Some(f.epic),
            0,
            &json!(DecisionRecord {
                key: "accept:held".into(),
                epic_id: f.epic,
                question: "Operator acceptance?".into(),
                request_id: None,
                work_key: Some("held".into()),
                target_digest: format!("sha256:{}", "a".repeat(64)),
                target_row_version: Some(version),
                request_row_version: None,
                status: "pending".into(),
                answer: None,
                delivery: None,
            }),
        )
        .unwrap();
    assert!(
        f.store
            .manager_v2_decision_gate(&config, f.epic)
            .unwrap_err()
            .to_string()
            .contains("manager_v2_pending_operator_decision")
    );
    let mut review = review_request("held", version, &sha);
    review.fence.policy_version = 2;
    let receipt = f
        .store
        .manager_v2_commit_update(
            f.manager,
            &review,
            &LedgerObservation {
                source_commit: Some(sha.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let assignment = Uuid::parse_str(&receipt.key).unwrap();
    assert!(
        f.store
            .allocate_manager_review_assignment(assignment)
            .unwrap()
    );
    let claim = f
        .store
        .claim_manager_action(Uuid::new_v4())
        .unwrap()
        .unwrap();
    assert!(
        f.store
            .manager_review_evidence_action(&claim.operation)
            .unwrap()
    );
    f.store.manager_action_runtime_gate(&claim, false).unwrap();
    assert!(f.store.manager_v2_decision_gate(&config, f.epic).is_err());
    let mut accept = request(
        ManagerUpdateV2::Accept {
            key: "held".into(),
            expected_row_version: version,
        },
        "held-accept",
    );
    accept.fence.policy_version = 2;
    let accepted = f
        .store
        .manager_v2_commit_update(f.manager, &accept, &LedgerObservation::default())
        .unwrap_err();
    assert!(
        accepted
            .to_string()
            .contains("manager_review_source_not_accepted")
    );
    assert_eq!(record.source_commit.as_deref(), Some(sha.as_str()));
    f.store
        .finish_manager_action(
            &claim,
            ManagerActionStateV2::Blocked,
            "manager_v2_provider_capacity",
        )
        .unwrap();
    assert!(
        f.store
            .refresh_manager_review_assignment(assignment)
            .unwrap()
    );
    let (_, current) = f.store.manager_v2_work(&config, "held").unwrap();
    let projection = f
        .store
        .manager_review_projection(&config, &current)
        .unwrap();
    assert_eq!(
        projection["current"]["failure_code"],
        "manager_v2_provider_capacity"
    );
}

#[test]
fn ordinary_accept_succeeds_only_after_an_eligible_db_review_receipt() {
    let mut f = fixture();
    let sha = "a".repeat(40);
    let (config, version, _) = source_work(&f, "reviewed", &sha);
    enable_review_policy(&f);
    let mut review = review_request("reviewed", version, &sha);
    review.fence.policy_version = 2;
    let receipt = f
        .store
        .manager_v2_commit_update(
            f.manager,
            &review,
            &LedgerObservation {
                source_commit: Some(sha.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let assignment = Uuid::parse_str(&receipt.key).unwrap();
    f.store
        .allocate_manager_review_assignment(assignment)
        .unwrap();
    let claim = f
        .store
        .claim_manager_action(Uuid::new_v4())
        .unwrap()
        .unwrap();
    let reviewer_id = claim.operation.receipt.target_session_id.unwrap();
    let mut accept = request(
        ManagerUpdateV2::Accept {
            key: "reviewed".into(),
            expected_row_version: version,
        },
        "accept-reviewed",
    );
    accept.fence.policy_version = 2;
    assert!(
        f.store
            .manager_v2_commit_update(f.manager, &accept, &LedgerObservation::default())
            .unwrap_err()
            .to_string()
            .contains("manager_review_source_not_accepted")
    );

    let custody_id = Uuid::new_v4();
    let invocation_id = Uuid::new_v4();
    let root = PathBuf::from(format!("/var/tmp/issue-541-reviewer-{reviewer_id}"));
    let mut reviewer = test_session(reviewer_id, PathBuf::from("/var/tmp/ham-v2-fd23a414"));
    reviewer.project_id = Some(f.project);
    reviewer.parent_id = Some(f.epic);
    reviewer.session_kind = SessionKind::Research;
    reviewer.status = SessionStatus::Starting;
    reviewer.sandbox_kind = Some(SandboxKind::GitWorktree);
    reviewer.sandbox_root = Some(root.clone());
    reviewer.sandbox_branch = Some(format!("rsi/{reviewer_id}"));
    reviewer.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
    f.store
        .insert_session_with_custody(
            &reviewer,
            SessionCustodyBinding::New(NewCustodyRoot {
                custody_id,
                canonical_repo_dir: reviewer.working_dir.to_string_lossy().into_owned(),
                sandbox_root: root.to_string_lossy().into_owned(),
                sandbox_branch: reviewer.sandbox_branch.clone().unwrap(),
                repository_identity: "issue-541-fixture".into(),
                source_commit: sha,
                cause: CustodyCause::FreshLaunch,
            }),
        )
        .unwrap();
    f.store
        .conn
        .execute(
            "INSERT INTO model_invocations
            (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
             trigger_source,session_id,project_id,dedup_key,created_at)
         VALUES(?1,'session_continue','session_lifecycle','foreground','paid_capable',
                'admitted','running','manager-review-test',?2,?3,?4,?5)",
            params![
                invocation_id.to_string(),
                reviewer_id.to_string(),
                f.project.to_string(),
                format!("manager.action:{}", claim.id()),
                now()
            ],
        )
        .unwrap();
    f.store
        .set_session_model_invocation(reviewer_id, Some(invocation_id))
        .unwrap();
    f.store.commit_manager_created_session(&claim).unwrap();
    assert!(
        f.store
            .refresh_manager_review_assignment(assignment)
            .unwrap()
    );
    let submission = AgentSubmitReviewReceiptRequestV1 {
        assignment_id: assignment,
        verdict: ManagerReviewVerdictV1::Accepted,
        findings: Vec::new(),
        idempotency_key: "eligible-receipt".into(),
    };
    f.store
        .conn
        .execute(
            "UPDATE sessions SET model='gpt-6-sol' WHERE id=?1",
            [reviewer_id.to_string()],
        )
        .unwrap();
    assert!(
        f.store
            .prepare_manager_review_submission(reviewer_id, &submission)
            .unwrap_err()
            .to_string()
            .contains("manager_review_family_conflict")
    );
    f.store
        .conn
        .execute(
            "UPDATE sessions SET model='claude-sonnet-5' WHERE id=?1",
            [reviewer_id.to_string()],
        )
        .unwrap();
    let observed = f
        .store
        .prepare_manager_review_submission(reviewer_id, &submission)
        .unwrap();
    f.store
        .commit_manager_review_submission(reviewer_id, &submission, &observed)
        .unwrap();
    f.store
        .conn
        .execute(
            "UPDATE sessions SET status='Completed',updated_at=?2 WHERE id=?1",
            params![reviewer_id.to_string(), now()],
        )
        .unwrap();
    f.store
        .conn
        .execute(
            "UPDATE model_invocations SET status='completed',completed_at=?2 WHERE id=?1",
            params![invocation_id.to_string(), now()],
        )
        .unwrap();
    let accepted = f
        .store
        .manager_v2_commit_update(f.manager, &accept, &LedgerObservation::default())
        .unwrap();
    assert_eq!(accepted.key, "reviewed");
    let (_, work) = f.store.manager_v2_work(&config, "reviewed").unwrap();
    assert_eq!(work.acceptance.unwrap().method, "db_review_receipt");
}
