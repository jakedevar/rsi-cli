//! Issue #599 S1: DB-native review admission binds the sealed commit, not the
//! source session's live HEAD, and follows a rotation tip that holds the same
//! sandbox. Stage and Integration keep today's archived-source refusal.
use super::*;

async fn review_at(f: &Fixture, key: &str) -> Result<ManagerMutationReceiptV2> {
    f.handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::RequestReview {
                    key: "product".into(),
                    expected_row_version: 2,
                    source_commit: f.source_head.clone(),
                    query: "Review the exact sealed source.".into(),
                    launch: ManagerLaunchChoiceV2 {
                        provider: SessionProvider::Claude,
                        model: "claude-sonnet-5".into(),
                        effort: None,
                    },
                },
                key,
            ),
        )
        .await
}

fn commit_file(root: &Path, path: &str, body: &str, message: &str) -> String {
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, body).unwrap();
    command(root, &["add", path]);
    command(root, &["commit", "-m", message]);
    command(root, &["rev-parse", "HEAD"])
}

fn refusal(result: Result<ManagerMutationReceiptV2>) -> String {
    result.expect_err("admission must refuse").to_string()
}

/// Seal (record exact source), then rotate: a successor takes the author's
/// custody by a `transferred` event and the author is archived.
async fn seal_then_rotate(f: &Fixture, key: &str) -> Uuid {
    record_db_review_source(f, key).await;
    let mut store = f.handle.store.lock().await;
    let live = store.live_custody_for_session(f.source).unwrap();
    let mut tip = store.get_session(f.source).unwrap().unwrap();
    tip.id = Uuid::new_v4();
    tip.continued_from = Some(f.source);
    tip.status = SessionStatus::Starting;
    store.insert_session(&tip).unwrap();
    store
        .bind_reserved_session_custody(
            tip.id,
            SessionCustodyBinding::Transfer {
                custody_id: live.custody_id,
                from_session_id: f.source,
                generation: live.generation,
                cause: CustodyCause::Rotation,
                origin_session_id: Some(f.source),
                scheduled_job_id: None,
            },
        )
        .unwrap();
    store
        .update_session_status(f.source, SessionStatus::Archived)
        .unwrap();
    tip.id
}

async fn assignment(f: &Fixture, receipt: &ManagerMutationReceiptV2) -> (String, String) {
    let store = f.handle.store.lock().await;
    store
        .conn
        .query_row(
            "SELECT source_sha,author_session_id FROM manager_review_assignments WHERE assignment_id=?1",
            [receipt.key.clone()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

async fn assert_review_at_seal(f: &Fixture, receipt: &ManagerMutationReceiptV2) {
    assert_eq!(
        assignment(f, receipt).await,
        (f.source_head.clone(), f.source.to_string())
    );
    let store = f.handle.store.lock().await;
    let payload: String = store
        .conn
        .query_row(
            "SELECT c.payload_json FROM manager_review_assignments a
               JOIN harness_manager_v2_records c
                 ON c.kind='lifecycle_context' AND c.record_key=a.action_operation_id
              WHERE a.assignment_id=?1",
            [receipt.key.clone()],
            |row| row.get(0),
        )
        .unwrap();
    let context: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(context["source"]["commit"], json!(f.source_head));
}

#[tokio::test]
async fn review_seal_survives_a_thoughts_only_commit_at_the_original_sha() {
    let f = fixture().await;
    record_db_review_source(&f, "seal-notes").await;
    let head = commit_file(
        &f.source_root,
        "thoughts/shared/notes/seal.md",
        "sealed\n",
        "notes: seal",
    );
    assert_ne!(head, f.source_head);

    let receipt = review_at(&f, "seal-notes-review").await.unwrap();
    assert_eq!(
        assignment(&f, &receipt).await,
        (f.source_head.clone(), f.source.to_string())
    );
    let id = Uuid::parse_str(&receipt.key).unwrap();
    let store = f.handle.store.lock().await;
    store.allocate_manager_review_assignment(id).unwrap();
    let state: String = store
        .conn
        .query_row(
            "SELECT state FROM manager_review_assignments WHERE assignment_id=?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "allocating");
}

#[tokio::test]
async fn code_commit_after_the_seal_keeps_the_review_at_the_exact_sha() {
    let f = fixture().await;
    record_db_review_source(&f, "seal-code").await;
    commit_file(&f.source_root, "code.txt", "changed\n", "code after seal");
    commit_file(&f.source_root, "code.txt", "implemented\n", "revert code");
    command(&f.source_root, &["mv", "code.txt", "thoughts-code.txt"]);
    std::fs::create_dir_all(f.source_root.join("thoughts")).unwrap();
    command(
        &f.source_root,
        &["mv", "thoughts-code.txt", "thoughts/code.txt"],
    );
    command(&f.source_root, &["commit", "-m", "move code into thoughts"]);
    let receipt = review_at(&f, "seal-code-review").await.unwrap();
    assert_review_at_seal(&f, &receipt).await;
    assert_eq!(
        command(
            &f.source_root,
            &["show", &format!("{}:code.txt", f.source_head)]
        ),
        "implemented"
    );
}

#[tokio::test]
async fn archived_author_is_reviewed_through_its_rotation_tip_without_substitution() {
    let f = fixture().await;
    let tip = seal_then_rotate(&f, "tip").await;
    commit_file(
        &f.source_root,
        "thoughts/shared/notes/tip.md",
        "handoff\n",
        "tip notes",
    );

    let receipt = review_at(&f, "tip-review").await.unwrap();
    // The work author stays the review identity; only the holder differs.
    assert_eq!(
        assignment(&f, &receipt).await,
        (f.source_head.clone(), f.source.to_string())
    );
    let id = Uuid::parse_str(&receipt.key).unwrap();
    let store = f.handle.store.lock().await;
    store.allocate_manager_review_assignment(id).unwrap();
    let (state, payload): (String, String) = store
        .conn
        .query_row(
            "SELECT a.state,c.payload_json FROM manager_review_assignments a
               JOIN harness_manager_v2_records c
                 ON c.kind='lifecycle_context' AND c.record_key=a.action_operation_id
              WHERE a.assignment_id=?1",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "allocating");
    let context: Value = serde_json::from_str(&payload).unwrap();
    let source = &context["source"];
    assert_eq!(source["session_id"], json!(tip), "fork source is the tip");
    assert_eq!(
        source["commit"],
        json!(f.source_head),
        "fork is at the sealed SHA"
    );
    assert_eq!(source["historical_commit"], json!(true));
}

#[tokio::test]
async fn archived_author_without_a_same_custody_tip_is_unavailable() {
    let f = fixture().await;
    record_db_review_source(&f, "other").await;
    {
        let store = f.handle.store.lock().await;
        // A successor holding a DIFFERENT sandbox is not a rotation tip.
        store
            .conn
            .execute(
                "UPDATE sessions SET continued_from=?2 WHERE id=?1",
                params![f.reviewer.to_string(), f.source.to_string()],
            )
            .unwrap();
        store
            .update_session_status(f.source, SessionStatus::Archived)
            .unwrap();
    }
    assert!(
        refusal(review_at(&f, "other-review").await).contains("manager_review_source_unavailable")
    );
}

#[tokio::test]
async fn two_unarchived_claimants_of_one_sandbox_are_refused() {
    let f = fixture().await;
    seal_then_rotate(&f, "ambiguous").await;
    {
        let store = f.handle.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE sessions SET sandbox_custody_id=(SELECT sandbox_custody_id FROM sessions WHERE id=?2) WHERE id=?1",
                params![f.reviewer.to_string(), f.source.to_string()],
            )
            .unwrap();
    }
    assert!(
        refusal(review_at(&f, "ambiguous-review").await)
            .contains("manager_review_source_unavailable")
    );
}

#[tokio::test]
async fn stage_and_integration_keep_the_archived_source_refusal() {
    let f = fixture().await;
    seal_then_rotate(&f, "paths").await;
    let stage = f
        .handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Stage {
                    key: "product".into(),
                    expected_row_version: 2,
                    stage: ManagerWorkStageV2::Implementation,
                    state: ManagerStageStateV2::Partial,
                    note: "archived source".into(),
                    evidence: Some(ManagerEvidenceV2 {
                        source_session_id: f.source,
                        source_commit: f.source_head.clone(),
                        artifact_path: String::new(),
                        artifact_commit: f.source_head.clone(),
                        closure_evidence_id: None,
                    }),
                },
                "paths-stage",
            ),
        )
        .await;
    assert!(refusal(stage).contains("manager_v2_evidence_source_unavailable"));
    let integration = f
        .handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Integration {
                    key: "product".into(),
                    expected_row_version: 2,
                    source_commit: f.source_head.clone(),
                    target_commit: f.source_head.clone(),
                    verification: None,
                },
                "paths-integration",
            ),
        )
        .await;
    assert!(refusal(integration).contains("manager_v2_evidence_source_unavailable"));
}

// A DB review binds the commit object. Later history and live working-tree
// changes cannot modify the bytes in that object.

const SEALED_NOTE: &str = "thoughts/shared/notes/sealed.md";

/// Seal a source whose sealed tree already carries the notes artifact.
async fn seal_with_note(f: &mut Fixture, key: &str) {
    f.source_head = commit_file(&f.source_root, SEALED_NOTE, "sealed v1\n", "notes: seal");
    record_db_review_source(f, key).await;
}

#[tokio::test]
async fn post_seal_notes_may_be_added_and_then_revised() {
    let mut f = fixture().await;
    seal_with_note(&mut f, "revise-new").await;
    commit_file(
        &f.source_root,
        "thoughts/shared/notes/after.md",
        "draft\n",
        "notes: add",
    );
    commit_file(
        &f.source_root,
        "thoughts/shared/notes/after.md",
        "final\n",
        "notes: revise",
    );
    let receipt = review_at(&f, "revise-new-review").await.unwrap();
    assert_eq!(
        assignment(&f, &receipt).await,
        (f.source_head.clone(), f.source.to_string())
    );
}

#[tokio::test]
async fn sealed_notes_rewrite_after_the_seal_keeps_the_sealed_bytes_under_review() {
    let mut f = fixture().await;
    seal_with_note(&mut f, "rewrite").await;
    commit_file(&f.source_root, SEALED_NOTE, "sealed v2\n", "notes: rewrite");
    let receipt = review_at(&f, "rewrite-review").await.unwrap();
    assert_review_at_seal(&f, &receipt).await;
    assert_eq!(
        command(
            &f.source_root,
            &["show", &format!("{}:{SEALED_NOTE}", f.source_head)]
        ),
        "sealed v1"
    );
    command(&f.source_root, &["rm", "-q", SEALED_NOTE]);
    command(&f.source_root, &["commit", "-m", "notes: delete"]);
    assert_eq!(
        command(
            &f.source_root,
            &["show", &format!("{}:{SEALED_NOTE}", f.source_head)]
        ),
        "sealed v1"
    );
}

#[tokio::test]
async fn dirty_holder_tree_does_not_block_a_review_request() {
    let f = fixture().await;
    record_db_review_source(&f, "dirty").await;
    std::fs::write(f.source_root.join("code.txt"), "uncommitted edit\n").unwrap();
    std::fs::write(f.source_root.join("scratch.txt"), "untracked\n").unwrap();
    assert!(!command(&f.source_root, &["status", "--porcelain"]).is_empty());
    let receipt = review_at(&f, "dirty-review").await.unwrap();
    assert_review_at_seal(&f, &receipt).await;
    assert_eq!(
        command(
            &f.source_root,
            &["show", &format!("{}:code.txt", f.source_head)]
        ),
        "implemented"
    );
}

#[tokio::test]
async fn merge_and_long_history_after_the_seal_keep_the_review() {
    let f = fixture().await;
    record_db_review_source(&f, "merge").await;
    let side = f.dir.path().join("side");
    command(
        &f.source_root,
        &[
            "worktree",
            "add",
            "-b",
            "side",
            side.to_str().unwrap(),
            &f.source_head,
        ],
    );
    commit_file(&side, "code.txt", "side change\n", "side code");
    commit_file(&side, "code.txt", "implemented\n", "side revert");
    commit_file(
        &f.source_root,
        "thoughts/shared/notes/merge.md",
        "notes\n",
        "notes",
    );
    command(&f.source_root, &["merge", "--no-ff", "--no-edit", "side"]);
    let diff = command(
        &f.source_root,
        &["diff", "--name-only", &f.source_head, "HEAD"],
    );
    assert_eq!(diff, "thoughts/shared/notes/merge.md");
    for n in 0..65 {
        command(
            &f.source_root,
            &["commit", "-q", "--allow-empty", "-m", &format!("note {n}")],
        );
    }
    let receipt = review_at(&f, "merge-review").await.unwrap();
    assert_review_at_seal(&f, &receipt).await;
    assert_eq!(
        command(
            &f.source_root,
            &["show", &format!("{}:code.txt", f.source_head)]
        ),
        "implemented"
    );
}

#[tokio::test]
async fn reviewing_the_custody_base_commit_is_not_authored() {
    let f = fixture().await;
    record_db_review_source(&f, "base").await;
    let error = f
        .handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::RequestReview {
                    key: "product".into(),
                    expected_row_version: 2,
                    source_commit: f.allocation_commit.clone(),
                    query: "Review the inherited custody base.".into(),
                    launch: ManagerLaunchChoiceV2 {
                        provider: SessionProvider::Claude,
                        model: "claude-sonnet-5".into(),
                        effort: None,
                    },
                },
                "base-review",
            ),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, crate::error::DaemonError::InvalidParam(code) if code == "manager_review_source_not_authored")
    );
}

/// Hold the Epic behind a pending operator acceptance decision (#541), so only
/// an action the evidence gate binds to a live DB review may cross it.
#[allow(clippy::unwrap_used, clippy::significant_drop_tightening)]
pub(super) fn hold_for_acceptance(store: &Store, f: &Fixture) {
    let project = store
        .get_session(f.epic)
        .unwrap()
        .unwrap()
        .project_id
        .unwrap();
    let config = store.get_harness_manager(project).unwrap().unwrap();
    store
        .manager_v2_put_record(
            &config,
            "decision",
            "accept:product",
            Some(f.epic),
            0,
            &json!(crate::store::manager_ledger::DecisionRecord {
                key: "accept:product".into(),
                epic_id: f.epic,
                question: "Operator acceptance?".into(),
                request_id: None,
                work_key: Some("product".into()),
                target_digest: format!("sha256:{}", "a".repeat(64)),
                target_row_version: Some(2),
                request_row_version: None,
                status: "pending".into(),
                answer: None,
                delivery: None,
            }),
        )
        .unwrap();
    assert!(
        store
            .manager_v2_decision_gate(&config, f.epic)
            .unwrap_err()
            .to_string()
            .contains("manager_v2_pending_operator_decision")
    );
}

/// Claim the queued reviewer launch and cross the effect-boundary gate the
/// executor runs immediately before spawning the reviewer.
#[allow(clippy::unwrap_used, clippy::significant_drop_tightening)]
pub(super) fn claim_and_cross_launch_gate(store: &Store, tip: Uuid) {
    let claim = store.claim_manager_action(Uuid::new_v4()).unwrap().unwrap();
    let source = claim.operation.context.source.as_ref().unwrap();
    assert_eq!(
        source.session_id, tip,
        "the claimed launch forks from the tip"
    );
    assert!(
        store
            .manager_review_evidence_action(&claim.operation)
            .unwrap()
    );
    store.manager_action_runtime_gate(&claim, true).unwrap();
}

#[tokio::test]
#[allow(clippy::unwrap_used, clippy::significant_drop_tightening)]
async fn rotated_review_source_crosses_the_pending_acceptance_gate() {
    let f = fixture().await;
    let tip = seal_then_rotate(&f, "tip-gate").await;
    let receipt = review_at(&f, "tip-gate-review").await.unwrap();
    let id = Uuid::parse_str(&receipt.key).unwrap();
    let store = f.handle.store.lock().await;
    store.allocate_manager_review_assignment(id).unwrap();
    hold_for_acceptance(&store, &f);
    claim_and_cross_launch_gate(&store, tip);
}

#[tokio::test]
#[allow(clippy::unwrap_used, clippy::significant_drop_tightening)]
async fn unrotated_author_review_crosses_the_pending_acceptance_gate() {
    let f = fixture().await;
    record_db_review_source(&f, "author-gate").await;
    let receipt = review_at(&f, "author-gate-review").await.unwrap();
    let id = Uuid::parse_str(&receipt.key).unwrap();
    let store = f.handle.store.lock().await;
    store.allocate_manager_review_assignment(id).unwrap();
    hold_for_acceptance(&store, &f);
    claim_and_cross_launch_gate(&store, f.source);
}

/// One persisted invalid launch source for a rotated DB review.
#[derive(Clone, Copy, Debug)]
enum InvalidRotatedSource {
    DifferentSandbox,
    StaleCustodyGeneration,
    ChangedSeal,
    UnrelatedSession,
    ArchivedTipWithoutLiveHolder,
}

/// Persist `path = value` into the claimed action's stored lifecycle context,
/// the record the runtime gate reloads at the launch boundary.
#[allow(clippy::unwrap_used)]
fn persist_action_source(store: &Store, action: Uuid, path: &str, value: &Value) {
    let changed = store
        .conn
        .execute(
            "UPDATE harness_manager_v2_records
                SET payload_json=json_set(payload_json,?2,json(?3))
              WHERE kind='lifecycle_context' AND record_key=?1",
            params![action.to_string(), path, value.to_string()],
        )
        .unwrap();
    assert_eq!(changed, 1);
}

#[tokio::test]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening
)]
async fn launch_gate_refuses_every_persisted_invalid_rotated_source_while_decision_pends() {
    for scenario in [
        InvalidRotatedSource::DifferentSandbox,
        InvalidRotatedSource::StaleCustodyGeneration,
        InvalidRotatedSource::ChangedSeal,
        InvalidRotatedSource::UnrelatedSession,
        InvalidRotatedSource::ArchivedTipWithoutLiveHolder,
    ] {
        let f = fixture().await;
        let tip = seal_then_rotate(&f, "tip-invalid").await;
        let receipt = review_at(&f, "tip-invalid-review").await.unwrap();
        let id = Uuid::parse_str(&receipt.key).unwrap();
        let store = f.handle.store.lock().await;
        store.allocate_manager_review_assignment(id).unwrap();
        hold_for_acceptance(&store, &f);
        let claim = store.claim_manager_action(Uuid::new_v4()).unwrap().unwrap();
        let action = claim.id();
        let bound = claim.operation.context.source.clone().unwrap();
        assert_eq!(
            bound.session_id, tip,
            "{scenario:?}: allocation bound the tip"
        );
        match scenario {
            InvalidRotatedSource::DifferentSandbox => persist_action_source(
                &store,
                action,
                "$.source.sandbox_root",
                &json!(f.review_root),
            ),
            InvalidRotatedSource::StaleCustodyGeneration => persist_action_source(
                &store,
                action,
                "$.source.custody_generation",
                &json!(bound.custody_generation.unwrap() - 1),
            ),
            InvalidRotatedSource::ChangedSeal => {
                persist_action_source(&store, action, "$.source.commit", &json!("b".repeat(40)));
            }
            InvalidRotatedSource::UnrelatedSession => {
                persist_action_source(&store, action, "$.source.session_id", &json!(f.reviewer));
            }
            InvalidRotatedSource::ArchivedTipWithoutLiveHolder => {
                store
                    .update_session_status(tip, SessionStatus::Archived)
                    .unwrap();
            }
        }
        let error = store
            .manager_action_runtime_gate(&claim, true)
            .expect_err("an invalid rotated source must not cross the pending decision");
        assert!(
            matches!(
                &error,
                crate::error::DaemonError::InvalidParam(code)
                    if code == "manager_v2_pending_operator_decision"
            ),
            "{scenario:?}: {error:?}"
        );
        let operation = store.manager_action_operation(action).unwrap().unwrap();
        assert!(
            !operation.effect_started,
            "{scenario:?}: the reviewer launch effect never started"
        );
    }
}
