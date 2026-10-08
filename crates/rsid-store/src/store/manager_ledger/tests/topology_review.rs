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
    TopologyReviewRequest, topology_review_work_key,
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
        verdict: ManagerReviewVerdictV1::Accepted,
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
