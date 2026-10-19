//! #1641 S1b: a topology review node's requests on the project manager's
//! ledger.
#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use super::issue_541_tests::{
    enable_review_policy, enable_review_policy_launches, fail_review, submit_review,
};
use super::*;
use crate::store::manager_reviews::{
    REVIEW_NO_MANAGER_LEDGER, TOPOLOGY_ONCALL_ACCEPTANCE, TopologyExtraRound,
    TopologyExtraRoundQuery, TopologyOncallAcceptance, TopologyReviewOutcome,
    TopologyReviewRequest, TopologyReviewWorkSettled, topology_review_work_key,
};
use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
use rsi_common::types::{SandboxCleanupState, SandboxKind, SessionProvider};

const NODE: &str = "Review";

fn author_is_openai(f: &Fixture) {
    f.store
        .conn
        .execute(
            "UPDATE sessions SET model='gpt-6-sol' WHERE id=?1",
            [f.lead.to_string()],
        )
        .unwrap();
}

fn reviewer(provider: SessionProvider, model: &str) -> ManagerLaunchChoiceV2 {
    ManagerLaunchChoiceV2 {
        provider,
        model: model.into(),
        effort: None,
    }
}

fn topology_request(
    f: &Fixture,
    execution_id: Uuid,
    commit: &str,
    reviewer: ManagerLaunchChoiceV2,
) -> TopologyReviewRequest {
    TopologyReviewRequest {
        execution_id,
        attempt_id: Uuid::new_v4(),
        node_id: NODE.into(),
        execution_name: "slice".into(),
        project_id: f.project,
        epic_id: f.epic,
        author_session_id: f.lead,
        source_commit: commit.into(),
        reviewer,
        query: "Review the change against its task".into(),
        previous_assignment: None,
        extra_contributors: Vec::new(),
    }
}

fn claude() -> ManagerLaunchChoiceV2 {
    reviewer(SessionProvider::Claude, "claude-sonnet-5")
}

fn assignment_row(f: &Fixture, assignment: Uuid) -> (String, String, String, Value) {
    f.store
        .conn
        .query_row(
            "SELECT state,author_session_id,source_sha,request_json
               FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment.to_string()],
            |row| {
                let json: String = row.get(3)?;
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    serde_json::from_str(&json).unwrap(),
                ))
            },
        )
        .unwrap()
}

fn events(f: &Fixture, kind: &str, key: &str) -> Vec<Value> {
    let mut statement = f
        .store
        .conn
        .prepare(
            "SELECT payload_json FROM harness_manager_v2_events
              WHERE kind=?1 AND record_key=?2 ORDER BY sequence",
        )
        .unwrap();
    statement
        .query_map(params![kind, key], |row| {
            let json: String = row.get(0)?;
            Ok(serde_json::from_str(&json).unwrap())
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_review_reserve_creates_work_and_assignment_with_executor_actor() {
    let f = fixture();
    author_is_openai(&f);
    enable_review_policy(&f);
    let execution = Uuid::new_v4();
    let sha = "a".repeat(40);
    let request = topology_request(&f, execution, &sha, claude());
    let assignment = f
        .store
        .manager_review_reserve_for_topology(&request)
        .unwrap();

    // The Work is the review node's, owned by the Epic, bound to the author
    // and the pinned commit.
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let key = topology_review_work_key(execution, NODE);
    assert_eq!(key, format!("topology:{execution}:{NODE}"));
    let (_, work) = f.store.manager_v2_work(&config, &key).unwrap();
    assert_eq!(work.epic_id, f.epic);
    assert_eq!(work.source_session_id, Some(f.lead));
    assert_eq!(work.source_commit.as_deref(), Some(sha.as_str()));
    assert_eq!(work.risk_tier, ManagerWorkRiskTierV2::Tier1);

    // The assignment is a normal reserved DB-native review of that commit,
    // tagged with the topology attempt that asked for it.
    let (state, author, source, stored) = assignment_row(&f, assignment);
    assert_eq!(state, "reserved");
    assert_eq!(author, f.lead.to_string());
    assert_eq!(source, sha);
    assert_eq!(stored["topology_attempt_id"], json!(request.attempt_id));
    assert_eq!(stored["requester_session_id"], json!(f.manager));
    assert_eq!(stored["launch"]["model"], "claude-sonnet-5");

    // The ledger events name the executor as the actor.
    let reserved = events(&f, "review_assignment", &assignment.to_string());
    assert_eq!(reserved.len(), 1);
    assert_eq!(reserved[0]["actor"]["kind"], "topology_executor");
    assert_eq!(reserved[0]["actor"]["execution_id"], json!(execution));
    assert_eq!(
        reserved[0]["actor"]["attempt_id"],
        json!(request.attempt_id)
    );
    let created = events(&f, "topology_review_work", &key);
    assert_eq!(created.len(), 1);
    assert_eq!(created[0]["actor"]["kind"], "topology_executor");

    // Asking again for the same attempt (a crash before the attempt recorded
    // the id) returns the same assignment instead of opening a second one.
    assert_eq!(
        f.store
            .manager_review_reserve_for_topology(&request)
            .unwrap(),
        assignment
    );
    let count: i64 = f
        .store
        .conn
        .query_row(
            "SELECT count(*) FROM manager_review_assignments WHERE work_key=?1",
            [&key],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);

    // The reviewer launches on the manager's own allocation path.
    assert!(
        f.store
            .allocate_manager_review_assignment(assignment)
            .unwrap()
    );
    assert_eq!(assignment_row(&f, assignment).0, "allocating");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_review_refuses_same_family_reviewer() {
    let f = fixture();
    author_is_openai(&f);
    enable_review_policy(&f);
    let execution = Uuid::new_v4();
    let request = topology_request(
        &f,
        execution,
        &"a".repeat(40),
        reviewer(SessionProvider::Codex, "gpt-6-sol"),
    );
    let error = f
        .store
        .manager_review_reserve_for_topology(&request)
        .unwrap_err()
        .to_string();
    assert!(error.contains("manager_review_family_conflict"), "{error}");
    // The refusal leaves nothing behind: no Work and no assignment.
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let key = topology_review_work_key(execution, NODE);
    assert!(
        f.store
            .manager_v2_record(&config, "work", &key)
            .unwrap()
            .is_none()
    );
    let count: i64 = f
        .store
        .conn
        .query_row(
            "SELECT count(*) FROM manager_review_assignments",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_review_without_a_manager_ledger_is_refused_with_its_typed_code() {
    let store = Store::open_in_memory().unwrap();
    let request = TopologyReviewRequest {
        execution_id: Uuid::new_v4(),
        attempt_id: Uuid::new_v4(),
        node_id: NODE.into(),
        execution_name: "slice".into(),
        project_id: Uuid::new_v4(),
        epic_id: Uuid::new_v4(),
        author_session_id: Uuid::new_v4(),
        source_commit: "a".repeat(40),
        reviewer: claude(),
        query: "Review".into(),
        previous_assignment: None,
        extra_contributors: Vec::new(),
    };
    let error = store
        .manager_review_reserve_for_topology(&request)
        .unwrap_err()
        .to_string();
    assert!(error.contains(REVIEW_NO_MANAGER_LEDGER), "{error}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_review_delta_round_carries_revised_source() {
    let f = fixture();
    author_is_openai(&f);
    enable_review_policy(&f);
    let execution = Uuid::new_v4();
    let first_sha = "a".repeat(40);
    let first = f
        .store
        .manager_review_reserve_for_topology(&topology_request(&f, execution, &first_sha, claude()))
        .unwrap();
    // Round one asks for changes with one blocking finding.
    submit_review(&f, first);
    let receipt: String = f
        .store
        .conn
        .query_row(
            "SELECT receipt_id FROM manager_review_receipts WHERE assignment_id=?1",
            [first.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    f.store
        .conn
        .execute(
            "INSERT INTO manager_review_findings
               (receipt_id,finding_key,severity,summary,location,blocking)
             VALUES(?1,'missing-test','error','No test asserts the intent','src/lib.rs:7',1)",
            [receipt],
        )
        .unwrap();
    let outcome = f.store.topology_review_outcome(first).unwrap();
    let TopologyReviewOutcome::ChangesRequested { findings } = outcome else {
        panic!("expected changes requested, got {outcome:?}");
    };
    assert_eq!(findings.len(), 1);
    assert!(findings[0].contains("missing-test"), "{findings:?}");
    assert!(
        findings[0].contains("No test asserts the intent"),
        "{findings:?}"
    );

    // Round two reviews the fix node's commit: a different session authored
    // it, and the previous round's finding is the delta to resolve.
    let mut fixer = f.store.get_session(f.lead).unwrap().unwrap();
    fixer.id = Uuid::new_v4();
    f.store.insert_session(&fixer).unwrap();
    let revised_sha = "b".repeat(40);
    let mut second = topology_request(&f, execution, &revised_sha, claude());
    second.previous_assignment = Some(first);
    second.extra_contributors = vec![fixer.id];
    let second_id = f
        .store
        .manager_review_reserve_for_topology(&second)
        .unwrap();

    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let key = topology_review_work_key(execution, NODE);
    let (_, work) = f.store.manager_v2_work(&config, &key).unwrap();
    assert_eq!(
        work.source_commit.as_deref(),
        Some(revised_sha.as_str()),
        "the Work follows the fix node's commit"
    );
    assert_eq!(work.source_session_id, Some(f.lead));
    let (state, _, source, stored) = assignment_row(&f, second_id);
    assert_eq!(state, "reserved");
    assert_eq!(source, revised_sha);
    assert_eq!(stored["delta_of"], json!(first));
    assert_eq!(stored["finding_keys"], json!(["missing-test"]));
    assert!(
        stored["contributor_session_ids"]
            .as_array()
            .unwrap()
            .contains(&json!(fixer.id)),
        "the fix session's family bars the reviewer too: {stored}"
    );
    let reserved = events(&f, "review_assignment", &second_id.to_string());
    assert_eq!(reserved[0]["previous_source_commit"], json!(first_sha));
    assert_eq!(reserved[0]["actor"]["kind"], "topology_executor");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_review_outcome_is_rederived_from_rows_every_time() {
    let f = fixture();
    author_is_openai(&f);
    enable_review_policy(&f);
    let execution = Uuid::new_v4();
    let sha = "a".repeat(40);
    let assignment = f
        .store
        .manager_review_reserve_for_topology(&topology_request(&f, execution, &sha, claude()))
        .unwrap();
    assert_eq!(
        f.store.topology_review_outcome(assignment).unwrap(),
        TopologyReviewOutcome::Pending
    );
    // A reviewer that ended without a receipt is a typed, unusable verdict.
    fail_review(&f, assignment);
    assert_eq!(
        f.store.topology_review_outcome(assignment).unwrap(),
        TopologyReviewOutcome::Unsettled("manager_review_receipt_missing_final".into())
    );
    // The replacement assignment (a new topology attempt, same commit)
    // supersedes it; the old id now reads the replacement's state.
    let replacement = f
        .store
        .manager_review_reserve_for_topology(&topology_request(&f, execution, &sha, claude()))
        .unwrap();
    assert_ne!(replacement, assignment);
    assert_eq!(
        f.store.topology_review_outcome(assignment).unwrap(),
        TopologyReviewOutcome::Pending
    );
    submit_review(&f, replacement);
    assert!(matches!(
        f.store.topology_review_outcome(assignment).unwrap(),
        TopologyReviewOutcome::ChangesRequested { .. }
    ));
}

/// Drive `assignment` to an accepted, fully admitted receipt the way the
/// ordinary DB review does: allocation, the reviewer session with its own
/// custody, an invocation, the receipt, then completion.
fn accept_review(f: &mut Fixture, assignment: Uuid, sha: &str, complete: bool) {
    submit_accepted_receipt(f, assignment, sha, complete, Vec::new()).unwrap();
}

fn submit_accepted_receipt(
    f: &mut Fixture,
    assignment: Uuid,
    sha: &str,
    complete: bool,
    findings: Vec<ManagerReviewFindingV1>,
) -> crate::error::Result<()> {
    submit_receipt(
        f,
        assignment,
        sha,
        complete,
        ManagerReviewVerdictV1::Accepted,
        findings,
    )
}

fn submit_receipt(
    f: &mut Fixture,
    assignment: Uuid,
    sha: &str,
    complete: bool,
    verdict: ManagerReviewVerdictV1,
    findings: Vec<ManagerReviewFindingV1>,
) -> crate::error::Result<()> {
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
    let reviewer_id = claim.operation.receipt.target_session_id.unwrap();
    let custody_id = Uuid::new_v4();
    let invocation_id = Uuid::new_v4();
    let root = PathBuf::from(format!("/var/tmp/topology-review-{reviewer_id}"));
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
                repository_identity: "topology-review-fixture".into(),
                source_commit: sha.into(),
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
    f.store
        .conn
        .execute(
            "UPDATE sessions SET model='claude-sonnet-5' WHERE id=?1",
            [reviewer_id.to_string()],
        )
        .unwrap();
    let submission = AgentSubmitReviewReceiptRequestV1 {
        assignment_id: assignment,
        verdict,
        findings,
        idempotency_key: format!("topology-receipt-{assignment}"),
    };
    let observed = f
        .store
        .prepare_manager_review_submission(reviewer_id, &submission)?;
    f.store
        .commit_manager_review_submission(reviewer_id, &submission, &observed)?;
    if complete {
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
    }
    Ok(())
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_review_accepts_only_an_admitted_receipt_for_the_exact_commit() {
    let mut f = fixture();
    author_is_openai(&f);
    enable_review_policy(&f);
    let sha = "a".repeat(40);
    let assignment = f
        .store
        .manager_review_reserve_for_topology(&topology_request(&f, Uuid::new_v4(), &sha, claude()))
        .unwrap();
    // The receipt is in, but the reviewer's invocation has not completed:
    // the verdict is not admissible yet, so the node keeps waiting.
    accept_review(&mut f, assignment, &sha, false);
    assert_eq!(
        f.store.topology_review_outcome(assignment).unwrap(),
        TopologyReviewOutcome::Pending
    );
    // Once the reviewer completes, the same rows yield the acceptance.
    let reviewer: String = f
        .store
        .conn
        .query_row(
            "SELECT reviewer_session_id FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    f.store
        .conn
        .execute(
            "UPDATE sessions SET status='Completed',updated_at=?2 WHERE id=?1",
            params![reviewer, now()],
        )
        .unwrap();
    f.store
        .conn
        .execute(
            "UPDATE model_invocations SET status='completed',completed_at=?2
              WHERE session_id=?1",
            params![reviewer, now()],
        )
        .unwrap();
    assert_eq!(
        f.store.topology_review_outcome(assignment).unwrap(),
        TopologyReviewOutcome::Accepted {
            reviewed_commit: sha,
            findings: Vec::new()
        }
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_land_admission_needs_an_admitted_acceptance_of_the_exact_commit() {
    let mut f = fixture();
    author_is_openai(&f);
    enable_review_policy(&f);
    let sha = "a".repeat(40);
    let assignment = f
        .store
        .manager_review_reserve_for_topology(&topology_request(&f, Uuid::new_v4(), &sha, claude()))
        .unwrap();
    // No verdict yet, then a receipt whose invocation has not completed: not
    // admitted, so nothing may be landed.
    assert!(!f.store.topology_land_admitted(assignment, &sha).unwrap());
    accept_review(&mut f, assignment, &sha, false);
    assert!(!f.store.topology_land_admitted(assignment, &sha).unwrap());
    f.store
        .conn
        .execute(
            "UPDATE sessions SET status='Completed',updated_at=?1
              WHERE id=(SELECT reviewer_session_id FROM manager_review_assignments
                         WHERE assignment_id=?2)",
            params![now(), assignment.to_string()],
        )
        .unwrap();
    f.store
        .conn
        .execute(
            "UPDATE model_invocations SET status='completed',completed_at=?1
              WHERE session_id=(SELECT reviewer_session_id FROM manager_review_assignments
                                 WHERE assignment_id=?2)",
            params![now(), assignment.to_string()],
        )
        .unwrap();
    assert!(f.store.topology_land_admitted(assignment, &sha).unwrap());
    // Any other commit is not what was accepted.
    assert!(
        !f.store
            .topology_land_admitted(assignment, &"b".repeat(40))
            .unwrap()
    );
    // The manager's scope moving after the acceptance withdraws admission
    // (the acceptance is tied to the scope version it was given under).
    f.store
        .conn
        .execute(
            "UPDATE harness_manager_scopes SET row_version=row_version+1 WHERE project_id=?1",
            [f.project.to_string()],
        )
        .unwrap();
    assert!(!f.store.topology_land_admitted(assignment, &sha).unwrap());
}

/// #3218: an accepted receipt is bound to its own completed invocation. A later
/// same-session turn (the worker-no-result nudge resumes the reviewer under a
/// new invocation) that submits no receipt must not revoke the acceptance, and
/// the reviewer holding its receipt is not a candidate for that nudge.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn a_later_reviewer_turn_does_not_revoke_an_accepted_receipt() {
    let mut f = fixture();
    author_is_openai(&f);
    enable_review_policy(&f);
    let sha = "a".repeat(40);
    let assignment = f
        .store
        .manager_review_reserve_for_topology(&topology_request(&f, Uuid::new_v4(), &sha, claude()))
        .unwrap();
    accept_review(&mut f, assignment, &sha, true);
    assert!(f.store.topology_land_admitted(assignment, &sha).unwrap());
    let reviewer: Uuid = f
        .store
        .conn
        .query_row(
            "SELECT reviewer_session_id FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment.to_string()],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
        .parse()
        .unwrap();
    assert!(f.store.worker_submitted_review_receipt(reviewer).unwrap());
    assert!(
        !f.store
            .worker_submitted_review_receipt(Uuid::new_v4())
            .unwrap()
    );
    // The nudge resumes the same session under a new invocation.
    let later = Uuid::new_v4();
    f.store
        .conn
        .execute(
            "INSERT INTO model_invocations
            (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
             trigger_source,session_id,project_id,dedup_key,created_at)
         VALUES(?1,'session_continue','session_lifecycle','foreground','paid_capable',
                'admitted','running','worker-no-result',?2,?3,?4,?5)",
            params![
                later.to_string(),
                reviewer.to_string(),
                f.project.to_string(),
                format!("later:{later}"),
                now()
            ],
        )
        .unwrap();
    f.store
        .set_session_model_invocation(reviewer, Some(later))
        .unwrap();
    f.store
        .conn
        .execute(
            "UPDATE sessions SET status='Running' WHERE id=?1",
            [reviewer.to_string()],
        )
        .unwrap();
    assert!(f.store.topology_land_admitted(assignment, &sha).unwrap());
    f.store
        .conn
        .execute(
            "UPDATE sessions SET status='Completed' WHERE id=?1",
            [reviewer.to_string()],
        )
        .unwrap();
    assert!(f.store.topology_land_admitted(assignment, &sha).unwrap());
    // A reviewer that is gone still withdraws the acceptance.
    f.store
        .conn
        .execute(
            "UPDATE sessions SET status='Failed' WHERE id=?1",
            [reviewer.to_string()],
        )
        .unwrap();
    assert!(!f.store.topology_land_admitted(assignment, &sha).unwrap());
}

/// #1740: an on-call `accept` ruling on an exhausted review is recorded as a
/// review-ledger event that admits exactly that assignment's commit, under the
/// scope it was reserved in. Without the record (a reject ruling) or for any
/// other commit the land stays refused.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_oncall_acceptance_admits_only_the_exhausted_assignments_commit() {
    let mut f = fixture();
    author_is_openai(&f);
    enable_review_policy(&f);
    let execution = Uuid::new_v4();
    let sha = "a".repeat(40);
    let request = topology_request(&f, execution, &sha, claude());
    let assignment = f
        .store
        .manager_review_reserve_for_topology(&request)
        .unwrap();
    let ruling = TopologyOncallAcceptance {
        execution_id: execution,
        attempt_id: request.attempt_id,
        node_id: NODE.into(),
        decision_key: "topology:decision".into(),
    };
    // No verdict yet: there is nothing exhausted to accept.
    let error = f
        .store
        .topology_record_oncall_acceptance(assignment, &sha, &ruling)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("topology_oncall_acceptance_not_exhausted"),
        "{error}"
    );

    submit_accepted_receipt(
        &mut f,
        assignment,
        &sha,
        true,
        vec![blocking_finding("late")],
    )
    .unwrap();
    assert!(matches!(
        f.store.topology_review_outcome(assignment).unwrap(),
        TopologyReviewOutcome::ChangesRequested { .. }
    ));
    // A reject ruling records nothing: the review still asks for changes.
    assert!(!f.store.topology_land_admitted(assignment, &sha).unwrap());

    // Another commit, or another node's work, cannot be recorded.
    let error = f
        .store
        .topology_record_oncall_acceptance(assignment, &"b".repeat(40), &ruling)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("topology_oncall_acceptance_commit_mismatch"),
        "{error}"
    );
    let other_node = TopologyOncallAcceptance {
        node_id: "Elsewhere".into(),
        ..ruling.clone()
    };
    let error = f
        .store
        .topology_record_oncall_acceptance(assignment, &sha, &other_node)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("topology_oncall_acceptance_work_mismatch"),
        "{error}"
    );
    assert!(!f.store.topology_land_admitted(assignment, &sha).unwrap());

    f.store
        .topology_record_oncall_acceptance(assignment, &sha, &ruling)
        .unwrap();
    // Idempotent: a retried settle records one fact.
    f.store
        .topology_record_oncall_acceptance(assignment, &sha, &ruling)
        .unwrap();
    let key = assignment.to_string();
    let recorded = events(&f, TOPOLOGY_ONCALL_ACCEPTANCE, &key);
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0]["source_commit"], json!(sha));
    assert_eq!(recorded[0]["assignment_id"], json!(assignment));
    assert_eq!(recorded[0]["spec_revision"], 1);
    assert_eq!(
        recorded[0]["actor"]["attempt_id"],
        json!(request.attempt_id)
    );
    assert_eq!(recorded[0]["actor"]["kind"], "topology_executor");
    assert!(f.store.topology_land_admitted(assignment, &sha).unwrap());
    assert!(
        !f.store
            .topology_land_admitted(assignment, &"b".repeat(40))
            .unwrap(),
        "only the exact commit is admitted"
    );
    // The manager's scope moving withdraws the acceptance like any other.
    f.store
        .conn
        .execute(
            "UPDATE harness_manager_scopes SET row_version=row_version+1 WHERE project_id=?1",
            [f.project.to_string()],
        )
        .unwrap();
    assert!(!f.store.topology_land_admitted(assignment, &sha).unwrap());
}

/// #2280: a reviewer that submitted `blocked` (its required checks never ran)
/// leaves the review unsettled; the owning manager's ruling admits exactly that
/// assignment's commit, and nothing else changes without it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_oncall_acceptance_admits_a_blocked_review_for_its_commit() {
    let mut f = fixture();
    author_is_openai(&f);
    enable_review_policy(&f);
    let execution = Uuid::new_v4();
    let sha = "a".repeat(40);
    let request = topology_request(&f, execution, &sha, claude());
    let assignment = f
        .store
        .manager_review_reserve_for_topology(&request)
        .unwrap();
    submit_receipt(
        &mut f,
        assignment,
        &sha,
        true,
        ManagerReviewVerdictV1::Blocked,
        vec![blocking_finding("tests-not-run")],
    )
    .unwrap();
    assert_eq!(
        f.store.topology_review_outcome(assignment).unwrap(),
        TopologyReviewOutcome::Unsettled("manager_review_blocked".into())
    );
    assert!(!f.store.topology_land_admitted(assignment, &sha).unwrap());

    let ruling = TopologyOncallAcceptance {
        execution_id: execution,
        attempt_id: request.attempt_id,
        node_id: NODE.into(),
        decision_key: "resolve:ruling-1".into(),
    };
    let error = f
        .store
        .topology_record_oncall_acceptance(assignment, &"b".repeat(40), &ruling)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("topology_oncall_acceptance_commit_mismatch"),
        "{error}"
    );
    assert!(!f.store.topology_land_admitted(assignment, &sha).unwrap());

    f.store
        .topology_record_oncall_acceptance(assignment, &sha, &ruling)
        .unwrap();
    f.store
        .topology_record_oncall_acceptance(assignment, &sha, &ruling)
        .unwrap();
    assert_eq!(
        events(&f, TOPOLOGY_ONCALL_ACCEPTANCE, &assignment.to_string()).len(),
        1
    );
    assert!(matches!(
        f.store.topology_review_outcome(assignment).unwrap(),
        TopologyReviewOutcome::Accepted { reviewed_commit, .. } if reviewed_commit == sha
    ));
    assert!(f.store.topology_land_admitted(assignment, &sha).unwrap());
    assert!(
        !f.store
            .topology_land_admitted(assignment, &"b".repeat(40))
            .unwrap(),
        "only the exact commit is admitted"
    );
}

/// #1766: multiple infra replacements retain the request and attempt, so the
/// original assignment's on-call ruling admits the exhausted head.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_oncall_acceptance_follows_same_request_infra_retries() {
    let mut f = fixture();
    author_is_openai(&f);
    enable_review_policy(&f);
    let sha = "a".repeat(40);
    let request = topology_request(&f, Uuid::new_v4(), &sha, claude());
    let original = f
        .store
        .manager_review_reserve_for_topology(&request)
        .unwrap();
    let first = f
        .store
        .manager_review_infra_retry_for_test(original)
        .unwrap();
    let head = f.store.manager_review_infra_retry_for_test(first).unwrap();
    submit_accepted_receipt(&mut f, head, &sha, true, vec![blocking_finding("head")]).unwrap();
    let ruling = TopologyOncallAcceptance {
        execution_id: request.execution_id,
        attempt_id: request.attempt_id,
        node_id: NODE.into(),
        decision_key: "topology:exhausted-infra-retry".into(),
    };
    for id in [original, first, head] {
        f.store
            .topology_record_oncall_acceptance(id, &sha, &ruling)
            .unwrap();
        assert!(f.store.topology_land_admitted(id, &sha).unwrap());
        assert!(!f.store.topology_land_admitted(id, &"b".repeat(40)).unwrap());
    }
    let recorded = events(&f, TOPOLOGY_ONCALL_ACCEPTANCE, &head.to_string());
    assert_eq!(recorded.len(), 1, "the ruling admits the retry head once");
    assert_eq!(recorded[0]["assignment_id"], json!(head));
    assert_eq!(
        recorded[0]["actor"]["attempt_id"],
        json!(request.attempt_id)
    );
}

/// #1752 / #1766: a manager re-review changes the request and cannot inherit
/// an old ruling, including one whose assignment had an infra replacement.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_oncall_acceptance_refuses_a_superseded_exhausted_assignment() {
    let mut f = fixture();
    author_is_openai(&f);
    enable_review_policy(&f);
    let sha = "a".repeat(40);
    let request_a = topology_request(&f, Uuid::new_v4(), &sha, claude());
    let a = f
        .store
        .manager_review_reserve_for_topology(&request_a)
        .unwrap();
    let retry = f.store.manager_review_infra_retry_for_test(a).unwrap();
    submit_accepted_receipt(&mut f, retry, &sha, true, vec![blocking_finding("a")]).unwrap();
    let ruling_a = TopologyOncallAcceptance {
        execution_id: request_a.execution_id,
        attempt_id: request_a.attempt_id,
        node_id: NODE.into(),
        decision_key: "topology:exhausted-a".into(),
    };
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let (row, work) = f
        .store
        .manager_v2_work(
            &config,
            &topology_review_work_key(request_a.execution_id, NODE),
        )
        .unwrap();
    let mut request_b = request(
        ManagerUpdateV2::RequestReview {
            key: work.key,
            expected_row_version: row.row_version,
            source_commit: sha.clone(),
            query: "Review the same commit again".into(),
            launch: claude(),
            delta_of: None,
            finding_keys: vec![],
        },
        "review-b",
    );
    request_b.fence.policy_version = 2;
    let b = f
        .store
        .manager_v2_commit_update(
            f.manager,
            &request_b,
            &LedgerObservation {
                source_commit: Some(sha.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let b = Uuid::parse_str(&b.key).unwrap();
    assert_ne!(a, b);
    submit_accepted_receipt(&mut f, b, &sha, true, vec![blocking_finding("b")]).unwrap();
    let error = f
        .store
        .topology_record_oncall_acceptance(a, &sha, &ruling_a)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("topology_oncall_acceptance_superseded"),
        "{error}"
    );
    for assignment in [a, retry, b] {
        assert!(events(&f, TOPOLOGY_ONCALL_ACCEPTANCE, &assignment.to_string()).is_empty());
        assert!(!f.store.topology_land_admitted(assignment, &sha).unwrap());
    }
    // Even passing B explicitly cannot rebind A's ruling to another attempt.
    let error = f
        .store
        .topology_record_oncall_acceptance(b, &sha, &ruling_a)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("topology_oncall_acceptance_attempt_mismatch"),
        "{error}"
    );
}

/// #1752 P2: use the real Work mutation, which increments the revision but
/// retains the source commit. Its old on-call acceptance remains visible in
/// audit, while admission and a replay of the old ruling both refuse.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_work_revision_withdraws_oncall_acceptance() {
    let mut f = fixture();
    author_is_openai(&f);
    enable_review_policy(&f);
    let sha = "a".repeat(40);
    let review = topology_request(&f, Uuid::new_v4(), &sha, claude());
    let assignment = f
        .store
        .manager_review_reserve_for_topology(&review)
        .unwrap();
    submit_accepted_receipt(
        &mut f,
        assignment,
        &sha,
        true,
        vec![blocking_finding("revise")],
    )
    .unwrap();
    let ruling = TopologyOncallAcceptance {
        execution_id: review.execution_id,
        attempt_id: review.attempt_id,
        node_id: NODE.into(),
        decision_key: "topology:exhausted-revision".into(),
    };
    f.store
        .topology_record_oncall_acceptance(assignment, &sha, &ruling)
        .unwrap();
    assert!(f.store.topology_land_admitted(assignment, &sha).unwrap());
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let (row, work) = f
        .store
        .manager_v2_work(
            &config,
            &topology_review_work_key(review.execution_id, NODE),
        )
        .unwrap();

    // A moved Work source alone also withdraws admission, before any revision.
    let mut moved = work.clone();
    moved.source_commit = Some("b".repeat(40));
    let moved_row = f
        .store
        .manager_v2_put_record(
            &config,
            "work",
            &work.key,
            Some(f.epic),
            row.row_version,
            &json!(moved),
        )
        .unwrap();
    assert!(!f.store.topology_land_admitted(assignment, &sha).unwrap());
    let restored_row = f
        .store
        .manager_v2_put_record(
            &config,
            "work",
            &work.key,
            Some(f.epic),
            moved_row.row_version,
            &json!(work),
        )
        .unwrap();
    assert!(f.store.topology_land_admitted(assignment, &sha).unwrap());

    let mut revision = request(
        ManagerUpdateV2::Work {
            key: work.key.clone(),
            expected_row_version: restored_row.row_version,
            epic_id: f.epic,
            title: work.title,
            kind: work.kind,
            priority: work.priority,
            weight: work.weight,
            required_gates: work.required_gates,
            risk_tier: work.risk_tier,
        },
        "revise-topology-work",
    );
    revision.fence.policy_version = 2;
    f.store
        .manager_v2_commit_update(f.manager, &revision, &LedgerObservation::default())
        .unwrap();
    let (_, revised) = f.store.manager_v2_work(&config, &work.key).unwrap();
    assert_eq!(revised.spec_revision, work.spec_revision + 1);
    assert_eq!(revised.source_commit.as_deref(), Some(sha.as_str()));
    assert_eq!(
        events(&f, TOPOLOGY_ONCALL_ACCEPTANCE, &assignment.to_string()).len(),
        1
    );
    assert!(!f.store.topology_land_admitted(assignment, &sha).unwrap());
    let error = f
        .store
        .topology_record_oncall_acceptance(assignment, &sha, &ruling)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("topology_oncall_acceptance_work_changed"),
        "{error}"
    );
}

fn blocking_finding(key: &str) -> ManagerReviewFindingV1 {
    ManagerReviewFindingV1 {
        key: key.into(),
        severity: ManagerReviewFindingSeverityV1::Error,
        summary: "Blocks acceptance".into(),
        location: None,
        blocking: true,
    }
}

fn session_with_model(f: &Fixture, model: &str) -> Uuid {
    let mut session = f.store.get_session(f.lead).unwrap().unwrap();
    session.id = Uuid::new_v4();
    session.model = Some(model.into());
    f.store.insert_session(&session).unwrap();
    session.id
}

/// #1669: an accepted receipt that carries a blocking finding is a request
/// for changes everywhere: the immutable receipt stays `accepted`, the fix
/// round's delta is eligible, and the re-review completes.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_accepted_with_blocking_finding_reaches_a_delta_re_review() {
    let mut f = fixture();
    author_is_openai(&f);
    enable_review_policy(&f);
    let execution = Uuid::new_v4();
    let first_sha = "a".repeat(40);
    let first = f
        .store
        .manager_review_reserve_for_topology(&topology_request(&f, execution, &first_sha, claude()))
        .unwrap();
    submit_accepted_receipt(
        &mut f,
        first,
        &first_sha,
        true,
        vec![blocking_finding("missing-test")],
    )
    .unwrap();
    let verdict: String = f
        .store
        .conn
        .query_row(
            "SELECT verdict FROM manager_review_receipts WHERE assignment_id=?1",
            [first.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(verdict, "accepted", "the stored receipt is immutable");
    assert!(matches!(
        f.store.topology_review_outcome(first).unwrap(),
        TopologyReviewOutcome::ChangesRequested { .. }
    ));

    let fixer = session_with_model(&f, "gpt-6-sol");
    let revised_sha = "b".repeat(40);
    let mut second = topology_request(&f, execution, &revised_sha, claude());
    second.previous_assignment = Some(first);
    second.extra_contributors = vec![fixer];
    let second_id = f
        .store
        .manager_review_reserve_for_topology(&second)
        .unwrap();
    let (_, _, _, stored) = assignment_row(&f, second_id);
    assert_eq!(stored["delta_of"], json!(first));
    assert_eq!(stored["finding_keys"], json!(["missing-test"]));

    submit_accepted_receipt(&mut f, second_id, &revised_sha, true, Vec::new()).unwrap();
    assert_eq!(
        f.store.topology_review_outcome(second_id).unwrap(),
        TopologyReviewOutcome::Accepted {
            reviewed_commit: revised_sha,
            findings: Vec::new()
        }
    );
}

/// #1669: the delta exemption removes only the previous reviewer's own
/// provenance. A fix author of the reviewer's family still bars the reviewer.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_fix_author_family_bars_the_delta_reviewer_at_reservation() {
    let f = fixture();
    f.store
        .conn
        .execute(
            "UPDATE sessions SET model='claude-sonnet-5' WHERE id=?1",
            [f.lead.to_string()],
        )
        .unwrap();
    f.store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: f.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: "review-policy-both".into(),
            policy: ManagerPolicyV2 {
                mode: ManagerOperatingModeV2::Execute,
                capabilities: vec![
                    ManagerCapabilityV2::WorkPlan,
                    ManagerCapabilityV2::SessionCreate,
                ],
                max_created_sessions: 2,
                allowed_launches: vec![
                    reviewer(SessionProvider::Claude, "claude-sonnet-5"),
                    reviewer(SessionProvider::Codex, "gpt-6-sol"),
                ],
                ..Default::default()
            },
        })
        .unwrap();
    let openai = || reviewer(SessionProvider::Codex, "gpt-6-sol");
    let execution = Uuid::new_v4();
    let first = f
        .store
        .manager_review_reserve_for_topology(&topology_request(
            &f,
            execution,
            &"a".repeat(40),
            openai(),
        ))
        .unwrap();
    submit_review(&f, first);
    let receipt: String = f
        .store
        .conn
        .query_row(
            "SELECT receipt_id FROM manager_review_receipts WHERE assignment_id=?1",
            [first.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    f.store
        .conn
        .execute(
            "INSERT INTO manager_review_findings
               (receipt_id,finding_key,severity,summary,location,blocking)
             VALUES(?1,'missing-test','error','No test asserts the intent',NULL,1)",
            [receipt],
        )
        .unwrap();

    // The fix node is an OpenAI session: its family bars an OpenAI reviewer.
    let fixer = session_with_model(&f, "gpt-6-sol");
    let mut second = topology_request(&f, execution, &"b".repeat(40), openai());
    second.previous_assignment = Some(first);
    second.extra_contributors = vec![fixer];
    let error = f
        .store
        .manager_review_reserve_for_topology(&second)
        .unwrap_err()
        .to_string();
    // Both allowed families are now barred: Anthropic by the author, OpenAI by
    // the fix author.
    assert!(error.contains("manager_review_family_exhausted"), "{error}");
    assert!(error.contains(&format!("openai:{fixer}")), "{error}");

    // Without that fix author the previous reviewer's family is reusable.
    second.extra_contributors = Vec::new();
    f.store
        .manager_review_reserve_for_topology(&second)
        .unwrap();
}

/// #1669: receipt admission recomputes the same bans, so a fix author of the
/// receipt submitter's family refuses the receipt too.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_fix_author_family_bars_the_delta_reviewer_at_receipt_admission() {
    let mut f = fixture();
    author_is_openai(&f);
    enable_review_policy(&f);
    let execution = Uuid::new_v4();
    let first = f
        .store
        .manager_review_reserve_for_topology(&topology_request(
            &f,
            execution,
            &"a".repeat(40),
            claude(),
        ))
        .unwrap();
    submit_review(&f, first);
    let receipt: String = f
        .store
        .conn
        .query_row(
            "SELECT receipt_id FROM manager_review_receipts WHERE assignment_id=?1",
            [first.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    f.store
        .conn
        .execute(
            "INSERT INTO manager_review_findings
               (receipt_id,finding_key,severity,summary,location,blocking)
             VALUES(?1,'missing-test','error','No test asserts the intent',NULL,1)",
            [receipt],
        )
        .unwrap();
    let revised_sha = "b".repeat(40);
    let mut second = topology_request(&f, execution, &revised_sha, claude());
    second.previous_assignment = Some(first);
    let second_id = f
        .store
        .manager_review_reserve_for_topology(&second)
        .unwrap();
    // A fix author of the reviewer's (Anthropic) family joins the stored
    // contributors, as the reservation records them.
    let fixer = session_with_model(&f, "claude-sonnet-5");
    // The identity and forward-only triggers forbid rewriting a reserved row;
    // the fixture drops them to stand in for the contributors a reservation would record.
    f.store
        .conn
        .execute_batch(
            "DROP TRIGGER manager_review_assignments_v121_identity_immutable;
             DROP TRIGGER manager_review_assignments_v121_forward;",
        )
        .unwrap();
    f.store
        .conn
        .execute(
            "UPDATE manager_review_assignments
                SET request_json=json_set(request_json,'$.contributor_session_ids',json(?2))
              WHERE assignment_id=?1",
            params![second_id.to_string(), json!([fixer]).to_string()],
        )
        .unwrap();
    let error = submit_accepted_receipt(&mut f, second_id, &revised_sha, true, Vec::new())
        .unwrap_err()
        .to_string();
    // The only allowed family is now barred by the fix author.
    assert!(error.contains("manager_review_family_exhausted"), "{error}");
    assert!(error.contains(&format!("anthropic:{fixer}")), "{error}");
}

/// `submit_review` plus one blocking finding, so the next round is a delta
/// review that may carry a revised commit.
fn submit_review_with_finding(f: &Fixture, assignment: Uuid) {
    submit_review(f, assignment);
    let receipt: String = f
        .store
        .conn
        .query_row(
            "SELECT receipt_id FROM manager_review_receipts WHERE assignment_id=?1",
            [assignment.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    f.store
        .conn
        .execute(
            "INSERT INTO manager_review_findings
               (receipt_id,finding_key,severity,summary,location,blocking)
             VALUES(?1,'missing-test','error','No test asserts the intent','src/lib.rs:7',1)",
            [receipt],
        )
        .unwrap();
}

/// Two submitted rounds of `execution` at one commit each, the way a normal
/// two-round review node leaves them.
fn two_rounds(f: &Fixture, execution: Uuid) -> (String, Uuid) {
    let first_sha = "a".repeat(40);
    let first = f
        .store
        .manager_review_reserve_for_topology(&topology_request(f, execution, &first_sha, claude()))
        .unwrap();
    submit_review_with_finding(f, first);
    let revised = "b".repeat(40);
    let mut second = topology_request(f, execution, &revised, claude());
    second.previous_assignment = Some(first);
    let second = f
        .store
        .manager_review_reserve_for_topology(&second)
        .unwrap();
    submit_review(f, second);
    (revised, second)
}

fn extra_round(
    f: &Fixture,
    execution: Uuid,
    commit: &str,
    reviewer: &ManagerLaunchChoiceV2,
) -> TopologyExtraRound {
    f.store
        .topology_review_extra_round(&TopologyExtraRoundQuery {
            execution_id: execution,
            node_id: NODE,
            project_id: f.project,
            source_commit: commit,
            reviewer,
            author_session_id: f.lead,
            extra_contributors: &[],
        })
        .unwrap()
}

/// #1715 finding 4: after two submitted reviews the store requires a different
/// reviewer model for the third, and a fourth never opens. The extra round the
/// executor offers must be one the store then accepts.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_extra_round_follows_the_stores_closure_and_budget_rules() {
    let f = fixture();
    author_is_openai(&f);
    let opus = reviewer(SessionProvider::Claude, "claude-opus-5-5");
    enable_review_policy_launches(&f, vec![claude(), opus.clone()]);
    let execution = Uuid::new_v4();

    // One round in: the node's own reviewer can run another.
    let first_sha = "a".repeat(40);
    let first = f
        .store
        .manager_review_reserve_for_topology(&topology_request(&f, execution, &first_sha, claude()))
        .unwrap();
    submit_review_with_finding(&f, first);
    assert_eq!(
        extra_round(&f, execution, &first_sha, &claude()),
        TopologyExtraRound::SameReviewer
    );

    // Two rounds in: the store would refuse the same model, so the offered
    // round names the policy-allowed closure reviewer instead.
    let revised = "b".repeat(40);
    let mut second = topology_request(&f, execution, &revised, claude());
    second.previous_assignment = Some(first);
    let second = f
        .store
        .manager_review_reserve_for_topology(&second)
        .unwrap();
    submit_review(&f, second);
    assert_eq!(
        extra_round(&f, execution, &revised, &claude()),
        TopologyExtraRound::Closure(opus.clone())
    );
    let refused_same = f
        .store
        .manager_review_reserve_for_topology(&topology_request(&f, execution, &revised, claude()))
        .unwrap_err()
        .to_string();
    assert!(
        refused_same.contains("manager_review_closure_specialist_required"),
        "{refused_same}"
    );
    let third = f
        .store
        .manager_review_reserve_for_topology(&topology_request(&f, execution, &revised, opus))
        .unwrap();

    // Three rounds in: no further round exists.
    submit_review(&f, third);
    assert_eq!(
        extra_round(&f, execution, &revised, &claude()),
        TopologyExtraRound::Unavailable("manager_review_round_budget".into())
    );
}

/// #1715 finding 4: when the policy names no other reviewer model, the extra
/// round is reported unavailable instead of being offered and then refused.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_extra_round_is_unavailable_without_a_closure_reviewer() {
    let f = fixture();
    author_is_openai(&f);
    enable_review_policy(&f);
    let execution = Uuid::new_v4();
    let (revised, _) = two_rounds(&f, execution);
    assert_eq!(
        extra_round(&f, execution, &revised, &claude()),
        TopologyExtraRound::Unavailable("manager_review_closure_specialist_required".into())
    );
}

/// Submit a `blocked` receipt with `findings` on a fresh topology review and
/// return what the executor reads back.
fn outcome_of_blocked_receipt(findings: Vec<ManagerReviewFindingV1>) -> TopologyReviewOutcome {
    let mut f = fixture();
    author_is_openai(&f);
    enable_review_policy(&f);
    let sha = "a".repeat(40);
    let assignment = f
        .store
        .manager_review_reserve_for_topology(&topology_request(&f, Uuid::new_v4(), &sha, claude()))
        .unwrap();
    submit_receipt(
        &mut f,
        assignment,
        &sha,
        true,
        ManagerReviewVerdictV1::Blocked,
        findings,
    )
    .unwrap();
    f.store.topology_review_outcome(assignment).unwrap()
}

/// #2570: a receipt whose only blocking finding says a test run was unavailable
/// is an infrastructure outcome, not a verdict on the code, so the executor
/// retries instead of blocking the run. Any real blocking finding keeps it a
/// plain blocked review.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_review_receipt_with_only_unavailable_tests_is_an_infrastructure_outcome() {
    let infrastructure = TopologyReviewOutcome::Unsettled(
        rsi_common::harness_manager_v2::REVIEW_INFRASTRUCTURE_REASON.into(),
    );
    for key in [
        "infra_tests_unavailable",
        "focused_tests_unavailable",
        "required_tests_unrun",
    ] {
        assert_eq!(
            outcome_of_blocked_receipt(vec![blocking_finding(key)]),
            infrastructure,
            "{key}"
        );
    }
    assert_eq!(
        outcome_of_blocked_receipt(vec![
            blocking_finding("infra_tests_unavailable"),
            blocking_finding("missing-test"),
        ]),
        TopologyReviewOutcome::Unsettled("manager_review_blocked".into()),
        "a real blocking finding keeps the review blocked"
    );
    assert_eq!(
        outcome_of_blocked_receipt(vec![blocking_finding("tests-not-run")]),
        TopologyReviewOutcome::Unsettled("manager_review_blocked".into()),
        "an untyped finding is a code finding"
    );
}

/// #2570: the review ledger does not count a receipt that only reported an
/// unavailable check as a spent round, so the retry may use the node's own
/// reviewer; the same round with a code finding does spend it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_infrastructure_receipt_spends_no_review_round() {
    for (key, expected) in [
        ("infra_tests_unavailable", TopologyExtraRound::SameReviewer),
        (
            "missing-test",
            TopologyExtraRound::Unavailable("manager_review_closure_specialist_required".into()),
        ),
    ] {
        let f = fixture();
        author_is_openai(&f);
        enable_review_policy(&f);
        let execution = Uuid::new_v4();
        let first_sha = "a".repeat(40);
        let first = f
            .store
            .manager_review_reserve_for_topology(&topology_request(
                &f,
                execution,
                &first_sha,
                claude(),
            ))
            .unwrap();
        submit_review_with_finding(&f, first);
        let revised = "b".repeat(40);
        let mut second = topology_request(&f, execution, &revised, claude());
        second.previous_assignment = Some(first);
        let second = f
            .store
            .manager_review_reserve_for_topology(&second)
            .unwrap();
        submit_review(&f, second);
        let receipt: String = f
            .store
            .conn
            .query_row(
                "SELECT receipt_id FROM manager_review_receipts WHERE assignment_id=?1",
                [second.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        f.store
            .conn
            .execute(
                "INSERT INTO manager_review_findings
                   (receipt_id,finding_key,severity,summary,location,blocking)
                 VALUES(?1,?2,'error','finding',NULL,1)",
                params![receipt, key],
            )
            .unwrap();
        assert_eq!(
            extra_round(&f, execution, &revised, &claude()),
            expected,
            "{key}"
        );
    }
}

/// Record a deploy that cut `reviewer` off mid-turn (`interrupt_workers`).
fn deploy_interrupts(f: &Fixture, reviewer: Uuid) {
    let deploy = f
        .store
        .insert_agent_deploy(
            &crate::store::agent_deploys::NewDeploy {
                id: Uuid::new_v4(),
                owner_session_id: f.lead,
                idempotency_key: "k2809",
                sha: &"c".repeat(40),
                fingerprint: "f".repeat(64),
                manifest: &[],
                max_wait_secs: 60,
                interrupt_workers: true,
            },
            chrono::Utc::now(),
        )
        .unwrap();
    f.store
        .conn
        .execute(
            "UPDATE agent_deploys SET interrupted_sessions=?2 WHERE id=?1",
            params![
                deploy.id.to_string(),
                serde_json::to_string(&[reviewer.to_string()]).unwrap()
            ],
        )
        .unwrap();
}

fn reviewer_of(f: &Fixture, assignment: Uuid) -> Uuid {
    let reviewer: String = f
        .store
        .conn
        .query_row(
            "SELECT reviewer_session_id FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    Uuid::parse_str(&reviewer).unwrap()
}

/// #2809: a `blocked` receipt from a reviewer a deploy interrupted is no
/// verdict on the code. The outcome names the interruption (the executor
/// retries it as an infrastructure loss) and the review ledger does not count
/// the round, so the retry is not refused `review_round_unavailable`
/// (`manager_review_closure_specialist_required`); the same receipt from a
/// reviewer no deploy cut off keeps the round and the review blocked.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_review_interrupted_by_a_deploy_spends_no_review_round() {
    for interrupted in [false, true] {
        let mut f = fixture();
        author_is_openai(&f);
        enable_review_policy(&f);
        let execution = Uuid::new_v4();
        let first_sha = "a".repeat(40);
        let first = f
            .store
            .manager_review_reserve_for_topology(&topology_request(
                &f,
                execution,
                &first_sha,
                claude(),
            ))
            .unwrap();
        submit_review_with_finding(&f, first);
        let revised = "b".repeat(40);
        let mut second = topology_request(&f, execution, &revised, claude());
        second.previous_assignment = Some(first);
        let second = f
            .store
            .manager_review_reserve_for_topology(&second)
            .unwrap();
        submit_receipt(
            &mut f,
            second,
            &revised,
            true,
            ManagerReviewVerdictV1::Blocked,
            vec![blocking_finding("missing-test")],
        )
        .unwrap();
        if interrupted {
            deploy_interrupts(&f, reviewer_of(&f, second));
        }
        let (outcome, round) = (
            f.store.topology_review_outcome(second).unwrap(),
            extra_round(&f, execution, &revised, &claude()),
        );
        if interrupted {
            assert_eq!(
                outcome,
                TopologyReviewOutcome::Unsettled(
                    rsi_common::harness_manager_v2::REVIEW_INTERRUPTED_REASON.into()
                )
            );
            assert_eq!(round, TopologyExtraRound::SameReviewer);
        } else {
            assert_eq!(
                outcome,
                TopologyReviewOutcome::Unsettled("manager_review_blocked".into())
            );
            assert_eq!(
                round,
                TopologyExtraRound::Unavailable(
                    "manager_review_closure_specialist_required".into()
                )
            );
        }
    }
}

fn work_row(f: &Fixture, key: &str) -> Value {
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    f.store
        .manager_v2_work_rows(&config, None)
        .unwrap()
        .into_iter()
        .find(|row| row["key"] == key)
        .unwrap_or_else(|| panic!("work row {key} is listed"))
}

fn complete_reviewer(f: &Fixture, assignment: Uuid) {
    let reviewer: String = f
        .store
        .conn
        .query_row(
            "SELECT reviewer_session_id FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    f.store
        .conn
        .execute(
            "UPDATE sessions SET status='Completed',updated_at=?2 WHERE id=?1",
            params![reviewer, now()],
        )
        .unwrap();
    f.store
        .conn
        .execute(
            "UPDATE model_invocations SET status='completed',completed_at=?2
              WHERE session_id=?1",
            params![reviewer, now()],
        )
        .unwrap();
}

/// #2607: the land node's publication records integration on the review row
/// whose accepted source it landed, through the ledger's Integration record.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_review_work_integrates_when_its_accepted_source_landed() {
    let mut f = fixture();
    author_is_openai(&f);
    enable_review_policy(&f);
    let execution = Uuid::new_v4();
    let sha = "a".repeat(40);
    let tip = "c".repeat(40);
    let assignment = f
        .store
        .manager_review_reserve_for_topology(&topology_request(&f, execution, &sha, claude()))
        .unwrap();
    accept_review(&mut f, assignment, &sha, true);
    complete_reviewer(&f, assignment);
    let key = topology_review_work_key(execution, NODE);
    let row = work_row(&f, &key);
    assert_eq!(row["source_accepted"], true);
    assert_eq!(row["integrated"], false);

    let open = f.store.topology_open_review_work(execution).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].work_key, key);
    // The execution is still running and nothing landed: the row stays open.
    assert_eq!(
        f.store
            .topology_settle_review_work(execution, &open[0], None, false)
            .unwrap(),
        TopologyReviewWorkSettled::Open
    );
    // The land node published the accepted source.
    assert_eq!(
        f.store
            .topology_settle_review_work(execution, &open[0], Some(&tip), false)
            .unwrap(),
        TopologyReviewWorkSettled::Integrated
    );
    let row = work_row(&f, &key);
    assert_eq!(row["integrated"], true);
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let (_, work) = f.store.manager_v2_work(&config, &key).unwrap();
    let integration = work.integration.unwrap();
    assert_eq!(integration.source_commit, sha);
    assert_eq!(integration.target_commit, tip);
    assert_eq!(events(&f, "topology_review_work_integrated", &key).len(), 1);
    // Idempotent: a second pass finds nothing to settle.
    assert!(
        f.store
            .topology_open_review_work(execution)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        f.store
            .topology_settle_review_work(execution, &open[0], Some(&tip), true)
            .unwrap(),
        TopologyReviewWorkSettled::Open
    );
}

/// #2607: a row whose execution ended without landing an accepted source is
/// retired, so it is no longer unfinished work.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_review_work_of_an_unlanded_settled_execution_is_retired() {
    let f = fixture();
    author_is_openai(&f);
    enable_review_policy(&f);
    let execution = Uuid::new_v4();
    let sha = "a".repeat(40);
    f.store
        .manager_review_reserve_for_topology(&topology_request(&f, execution, &sha, claude()))
        .unwrap();
    let key = topology_review_work_key(execution, NODE);
    let open = f.store.topology_open_review_work(execution).unwrap();
    assert_eq!(open.len(), 1);
    // A different execution's pass never touches this row.
    assert!(
        f.store
            .topology_settle_review_work(Uuid::new_v4(), &open[0], None, true)
            .is_err()
    );
    // Even a landed source is retired when no accepted review admitted it.
    assert_eq!(
        f.store
            .topology_settle_review_work(execution, &open[0], Some(&"c".repeat(40)), true)
            .unwrap(),
        TopologyReviewWorkSettled::Retired
    );
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    assert!(
        f.store
            .manager_v2_record(&config, "work", &key)
            .unwrap()
            .unwrap()
            .archived
    );
    assert_eq!(work_row(&f, &key)["integrated"], false);
    assert!(
        f.store
            .topology_open_review_work(execution)
            .unwrap()
            .is_empty()
    );
    assert_eq!(events(&f, "topology_review_work_retired", &key).len(), 1);
}

/// #2607: an Epic whose only unfinished rows are topology-owned has no
/// unfinished work, so operating-intent recovery never relaunches its lead.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
#[test]
fn topology_owned_review_work_never_triggers_operating_intent_recovery() {
    let f = fixture();
    author_is_openai(&f);
    enable_review_policy(&f);
    let sha = "a".repeat(40);
    for _ in 0..3 {
        f.store
            .manager_review_reserve_for_topology(&topology_request(
                &f,
                Uuid::new_v4(),
                &sha,
                claude(),
            ))
            .unwrap();
    }
    let result = f.store.reconcile_manager_intent(f.project, true).unwrap();
    assert_eq!(result.unfinished, 0, "topology rows are not lead work");
    assert_eq!(result.queued, 0, "no lifecycle action is admitted");
    assert_eq!(result.blocked, 0);
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let intent = f
        .store
        .manager_v2_record(&config, "intent", &f.epic.to_string())
        .unwrap()
        .unwrap();
    assert_eq!(intent.payload["state"], "complete");
    assert_eq!(intent.payload["unfinished_count"], 0);
    assert_eq!(intent.payload["work_count"], 3);
    let actions: i64 = f
        .store
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_v2_records WHERE kind='lifecycle_context'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(actions, 0);
}
