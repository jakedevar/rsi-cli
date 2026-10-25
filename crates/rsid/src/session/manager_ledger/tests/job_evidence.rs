//! Stage updates that cite a caller-owned verification job (`job_evidence`).
//! The caller is the epic lead, who has live custody in this fixture; the
//! daemon reads the job, owner, receipt and report from its own tables and Git.

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
use super::*;
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
use crate::store::agent_jobs::NewAgentJob;
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
use rsi_common::agent_jobs::{
    AgentJobResultV1, JobParams, JobState, JobWake, ScopedToolProvenanceV1, ScopedToolSource,
};

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
const REPORT: &str = "thoughts/shared/qa/job-evidence-report.md";

/// Commit a QA report on the caller's own branch; returns the commit.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
fn commit_report(f: &Fixture) -> String {
    commit_report_at(&f.source_root)
}

/// [`commit_report`] in an explicit caller checkout.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
fn commit_report_at(root: &Path) -> String {
    std::fs::create_dir_all(root.join("thoughts/shared/qa")).unwrap();
    std::fs::write(root.join(REPORT), "QA report\n").unwrap();
    command(root, &["add", REPORT]);
    command(root, &["commit", "-m", "qa report"]);
    command(root, &["rev-parse", "HEAD"])
}

/// A `scoped_test` job row in the real store. `settled` is `None` for a job
/// still running, else its terminal state and typed result.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
async fn job(
    f: &Fixture,
    owner: Uuid,
    scoped: bool,
    settled: Option<(JobState, AgentJobResultV1)>,
) -> Uuid {
    let project = f
        .handle
        .store
        .lock()
        .await
        .get_session(f.manager)
        .unwrap()
        .unwrap()
        .project_id;
    job_in(f, owner, project, scoped, settled).await
}

/// `job` recorded under an explicit project (`None` is a legacy row).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
async fn job_in(
    f: &Fixture,
    owner: Uuid,
    project_id: Option<Uuid>,
    scoped: bool,
    settled: Option<(JobState, AgentJobResultV1)>,
) -> Uuid {
    let id = Uuid::new_v4();
    let params = if scoped {
        json!({"scoped_test": {"base": "origin/rolling"}})
    } else {
        json!({"package": "rsid"})
    };
    let store = f.handle.store.lock().await;
    store
        .insert_agent_job(
            &NewAgentJob {
                id,
                owner_session_id: owner,
                project_id,
                name: None,
                params: JobParams::Test(serde_json::from_value(params).unwrap()),
                cwd: f.source_root.display().to_string(),
                unit_name: format!("job-evidence-{id}"),
                log_path: format!("/tmp/{id}.log"),
                status_path: format!("/tmp/{id}.status"),
                idempotency_key: None,
                wake: JobWake::None,
            },
            Utc::now(),
        )
        .unwrap();
    if let Some((state, result)) = settled {
        // The daemon records the tool provenance before launch; settlement keeps it.
        if let Some(provenance) = &result.scoped_tool {
            store.record_agent_job_scoped_tool(id, provenance).unwrap();
        }
        store
            .settle_agent_job(id, state, &result, false, Utc::now())
            .unwrap();
    }
    id
}

/// The daemon's recorded tool identity for a `scoped_test` run.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
fn tool(source: ScopedToolSource) -> ScopedToolProvenanceV1 {
    ScopedToolProvenanceV1 {
        source,
        path: "/tool/scripts/scoped-test".into(),
        rev: None,
        files: Vec::new(),
    }
}

/// What a scoped-test receipt records: the tested `head`, the verdict and the
/// failures it names.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
fn finished(head: &str, exit_code: i32, failing: &[&str]) -> (JobState, AgentJobResultV1) {
    let failures: Vec<_> = failing
        .iter()
        .map(|test| json!({"kind": "lib", "package": "rsid", "target": "rsid", "test": test}))
        .collect();
    let result = AgentJobResultV1 {
        exit_code: Some(exit_code),
        failing_tests: failing.iter().map(|test| format!("rsid {test}")).collect(),
        receipt: Some(json!({
            "ok": exit_code == 0, "exit_code": exit_code, "base": "origin/rolling",
            "head": head, "filters": [], "plan": "precise", "schema": 1, "packages": [],
            "failures": failures, "baseline_reds": [],
        })),
        scoped_tool: Some(tool(ScopedToolSource::Rolling)),
        ..Default::default()
    };
    let state = if exit_code == 0 {
        JobState::Succeeded
    } else {
        JobState::Failed
    };
    (state, result)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
fn cite(job_id: Uuid, source_commit: &str, artifact_commit: &str) -> ManagerJobEvidenceV2 {
    ManagerJobEvidenceV2 {
        job_id,
        source_commit: source_commit.into(),
        artifact_path: REPORT.into(),
        artifact_commit: artifact_commit.into(),
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
fn verification(
    version: i64,
    state: ManagerStageStateV2,
    job: ManagerJobEvidenceV2,
    key: &str,
) -> AgentManagerUpdateRequestV2 {
    req(
        ManagerUpdateV2::Stage {
            key: "product".into(),
            expected_row_version: version,
            stage: ManagerWorkStageV2::Verification,
            state,
            note: "Caller-run verification".into(),
            evidence: None,
            job_evidence: Some(job),
        },
        key,
    )
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
async fn product(f: &Fixture) -> (i64, crate::store::manager_ledger::WorkRecord) {
    let store = f.handle.store.lock().await;
    let project = store
        .get_session(f.manager)
        .unwrap()
        .unwrap()
        .project_id
        .unwrap();
    let config = store.get_harness_manager(project).unwrap().unwrap();
    let (row, work) = store.manager_v2_work(&config, "product").unwrap();
    (row.row_version, work)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
async fn refusal(f: &Fixture, caller: Uuid, request: AgentManagerUpdateRequestV2) -> String {
    f.handle
        .agent_manager_update(caller, request)
        .await
        .unwrap_err()
        .to_string()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn a_caller_owned_green_job_is_reported_stage_evidence_never_an_admission() {
    let f = fixture().await;
    let tested = f.source_head.clone();
    let report = commit_report(&f);
    let green = job(&f, f.source, true, Some(finished(&tested, 0, &[]))).await;

    f.handle
        .agent_manager_update(
            f.source,
            verification(
                1,
                ManagerStageStateV2::Passed,
                cite(green, &tested, &report),
                "green",
            ),
        )
        .await
        .unwrap();

    let (version, work) = product(&f).await;
    assert_eq!(version, 2);
    let stage = work
        .stages
        .iter()
        .find(|stage| stage.stage == ManagerWorkStageV2::Verification)
        .unwrap();
    assert_eq!(stage.state, ManagerStageStateV2::Passed);
    let observed = stage.job_evidence.as_ref().unwrap();
    assert_eq!(observed.job_id, green);
    assert_eq!(observed.owner_session_id, f.source);
    assert_eq!(observed.source_commit, tested);
    assert_eq!(observed.artifact_commit, report);
    assert_eq!(observed.artifact_path, REPORT);
    assert!(observed.artifact_digest.starts_with("sha256:"));
    assert_eq!(
        observed.outcome,
        crate::store::manager_ledger::JobEvidenceOutcome::Green
    );
    assert_eq!(observed.exit_code, Some(0));
    // Reported evidence only: no admission, no worker identity, no acceptance.
    assert!(stage.admission.is_none() && stage.evidence.is_none());
    assert!(work.source_session_id.is_none() && work.source_commit.is_none());
    assert!(work.acceptance.is_none());
}

/// #3793: a green receipt printed by a candidate-tool, sandbox-tool or unstamped
/// (legacy) run is tool-under-test evidence, never a verified pass.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn a_non_rolling_tool_green_job_is_never_a_verified_pass() {
    let f = fixture().await;
    let tested = f.source_head.clone();
    let report = commit_report(&f);
    for (label, source) in [
        ("candidate", Some(ScopedToolSource::Candidate)),
        ("sandbox", Some(ScopedToolSource::Sandbox)),
        ("missing", None),
    ] {
        let (state, mut result) = finished(&tested, 0, &[]);
        result.scoped_tool = source.map(tool);
        let forged = job(&f, f.source, true, Some((state, result))).await;
        let claim = verification(
            1,
            ManagerStageStateV2::Passed,
            cite(forged, &tested, &report),
            &format!("forged-{label}"),
        );
        assert!(
            refusal(&f, f.source, claim)
                .await
                .contains("manager_v2_job_evidence_state_mismatch"),
            "{label}"
        );
        assert_eq!(product(&f).await.0, 1, "{label}");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn a_red_job_is_honest_failed_evidence_and_never_a_pass() {
    let f = fixture().await;
    let tested = f.source_head.clone();
    let report = commit_report(&f);
    let red = job(
        &f,
        f.source,
        true,
        Some(finished(&tested, 1, &["session::tests::broken"])),
    )
    .await;

    let claim = verification(
        1,
        ManagerStageStateV2::Passed,
        cite(red, &tested, &report),
        "red-pass",
    );
    assert!(
        refusal(&f, f.source, claim)
            .await
            .contains("manager_v2_job_evidence_state_mismatch")
    );
    assert_eq!(product(&f).await.0, 1);

    f.handle
        .agent_manager_update(
            f.source,
            verification(
                1,
                ManagerStageStateV2::Failed,
                cite(red, &tested, &report),
                "red-fail",
            ),
        )
        .await
        .unwrap();
    let (_, work) = product(&f).await;
    let stage = work
        .stages
        .iter()
        .find(|stage| stage.stage == ManagerWorkStageV2::Verification)
        .unwrap();
    assert_eq!(stage.state, ManagerStageStateV2::Failed);
    let observed = stage.job_evidence.as_ref().unwrap();
    assert_eq!(
        observed.outcome,
        crate::store::manager_ledger::JobEvidenceOutcome::Red
    );
    assert_eq!(observed.exit_code, Some(1));
    assert_eq!(observed.new_failures, vec!["session::tests::broken"]);
    assert!(observed.known_failures.is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn foreign_running_missing_and_mismatched_jobs_are_refused_and_change_nothing() {
    let f = fixture().await;
    let tested = f.source_head.clone();
    let report = commit_report(&f);
    let green = |head: &str| finished(head, 0, &[]);
    let foreign = job(&f, f.manager, true, Some(green(&tested))).await;
    let running = job(&f, f.source, true, None).await;
    let other_head = job(&f, f.source, true, Some(green(&report))).await;
    let no_receipt = job(
        &f,
        f.source,
        true,
        Some((
            JobState::Failed,
            AgentJobResultV1 {
                exit_code: Some(1),
                refusal: Some("scoped_test_receipt_missing".into()),
                ..Default::default()
            },
        )),
    )
    .await;
    let not_scoped = job(&f, f.source, false, Some(green(&tested))).await;
    let good = job(&f, f.source, true, Some(green(&tested))).await;
    // Owned by the caller and green, but recorded under another project (the
    // caller's own project when it acts for a granted target), or under none.
    let other_project = job_in(
        &f,
        f.source,
        Some(Uuid::new_v4()),
        true,
        Some(green(&tested)),
    )
    .await;
    let legacy = job_in(&f, f.source, None, true, Some(green(&tested))).await;

    let cases = [
        (
            "other-project",
            cite(other_project, &tested, &report),
            "manager_v2_job_evidence_job_unavailable",
        ),
        (
            "legacy-no-project",
            cite(legacy, &tested, &report),
            "manager_v2_job_evidence_job_unavailable",
        ),
        (
            "foreign",
            cite(foreign, &tested, &report),
            "manager_v2_job_evidence_job_unavailable",
        ),
        (
            "missing",
            cite(Uuid::new_v4(), &tested, &report),
            "manager_v2_job_evidence_job_unavailable",
        ),
        (
            "running",
            cite(running, &tested, &report),
            "manager_v2_job_evidence_job_running",
        ),
        (
            "receipt-names-another-commit",
            cite(other_head, &tested, &report),
            "manager_v2_job_evidence_head_mismatch",
        ),
        (
            "supplied-commit-is-not-tested",
            cite(good, &report, &report),
            "manager_v2_job_evidence_head_mismatch",
        ),
        (
            "no-receipt",
            cite(no_receipt, &tested, &report),
            "manager_v2_job_evidence_receipt_missing",
        ),
        (
            "not-a-scoped-test",
            cite(not_scoped, &tested, &report),
            "manager_v2_job_evidence_kind_unsupported",
        ),
        (
            "artifact-not-committed-at-that-commit",
            cite(good, &tested, &tested),
            "manager_v2_regular_artifact_required",
        ),
    ];
    for (name, cited, code) in cases {
        let error = refusal(
            &f,
            f.source,
            verification(1, ManagerStageStateV2::Passed, cited, name),
        )
        .await;
        assert!(error.contains(code), "{name}: {error}");
    }
    // A commit the caller's branch does not contain is not its committed report.
    std::fs::create_dir_all(f.review_root.join("thoughts/shared/qa")).unwrap();
    std::fs::write(f.review_root.join(REPORT), "foreign report\n").unwrap();
    command(&f.review_root, &["add", REPORT]);
    command(&f.review_root, &["commit", "-m", "foreign report"]);
    let foreign_report = command(&f.review_root, &["rev-parse", "HEAD"]);
    let error = refusal(
        &f,
        f.source,
        verification(
            1,
            ManagerStageStateV2::Passed,
            cite(good, &tested, &foreign_report),
            "foreign-report",
        ),
    )
    .await;
    assert!(
        error.contains("manager_v2_job_evidence_artifact_unreachable"),
        "{error}"
    );
    // The same job, cited correctly, is still accepted: only the citation failed.
    assert_eq!(product(&f).await.0, 1);
    f.handle
        .agent_manager_update(
            f.source,
            verification(
                1,
                ManagerStageStateV2::Passed,
                cite(good, &tested, &report),
                "accepted",
            ),
        )
        .await
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn job_evidence_keeps_the_works_recorded_worker_source_and_refuses_another_commit() {
    let f = fixture().await;
    // Worker evidence binds the Work's source session and commit, as before.
    record_db_review_source(&f, "bind-source").await;
    let report = commit_report(&f);
    let tested = f.source_head.clone();
    let on_report = job(&f, f.source, true, Some(finished(&report, 0, &[]))).await;
    let on_source = job(&f, f.source, true, Some(finished(&tested, 0, &[]))).await;

    let error = refusal(
        &f,
        f.source,
        verification(
            2,
            ManagerStageStateV2::Passed,
            cite(on_report, &report, &report),
            "other-commit",
        ),
    )
    .await;
    assert!(error.contains("manager_v2_source_changed"), "{error}");

    f.handle
        .agent_manager_update(
            f.source,
            verification(
                2,
                ManagerStageStateV2::Passed,
                cite(on_source, &tested, &report),
                "same-commit",
            ),
        )
        .await
        .unwrap();
    let (_, work) = product(&f).await;
    assert_eq!(work.source_commit.as_deref(), Some(tested.as_str()));
    assert_eq!(work.source_session_id, Some(f.source));
}

/// A second project B with a live root Group and Epic, as a granted portfolio
/// seat finds one: the seat's `Work` and `Stage` updates target it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
async fn project_with_epic(f: &Fixture) -> (Uuid, Uuid) {
    let store = f.handle.store.lock().await;
    let project = Uuid::new_v4();
    store
        .insert_project(&Project {
            id: project,
            name: "Granted target".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .unwrap();
    let mut group =
        crate::session::agent_verbs::tests::test_session(Uuid::new_v4(), f.source_root.clone());
    group.session_kind = SessionKind::Group;
    group.status = SessionStatus::Completed;
    group.project_id = Some(project);
    store.insert_session(&group).unwrap();
    let mut epic = group.clone();
    epic.id = Uuid::new_v4();
    epic.session_kind = SessionKind::Epic;
    epic.parent_id = Some(group.id);
    store.insert_session(&epic).unwrap();
    (project, epic.id)
}

/// A portfolio seat that lives in `project` (not in the project it is granted),
/// with its own live custody checkout, as a manager launch records one.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
async fn seat_with_custody(f: &Fixture, project: Uuid) -> (Uuid, PathBuf) {
    let seat = Uuid::new_v4();
    let invocation = Uuid::new_v4();
    let repo = f.dir.path().join("repo");
    let root = f.dir.path().join("sandboxes").join(seat.to_string());
    command(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "seat",
            root.to_str().unwrap(),
            &f.source_head,
        ],
    );
    let mut session = crate::session::agent_verbs::tests::test_session(seat, repo.clone());
    session.session_kind = SessionKind::Standard;
    session.status = SessionStatus::Starting;
    session.project_id = Some(project);
    session.sandbox_kind = Some(SandboxKind::GitWorktree);
    session.sandbox_root = Some(root.clone());
    session.sandbox_branch = Some("seat".into());
    session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
    session.model = Some("claude-sonnet-5".into());
    let mut store = f.handle.store.lock().await;
    store.conn.execute("INSERT INTO model_invocations(id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,provider,model,trigger_source,session_id,policy_snapshot_json,usage_confidence,created_at) VALUES(?1,'session_launch_fresh','session_lifecycle','foreground','paid_capable','admitted','running','Claude','test','manager_evidence_test',?2,'{}','unavailable',?3)",params![invocation.to_string(),seat.to_string(),Utc::now().to_rfc3339()]).unwrap();
    store
        .insert_direct_session_with_custody_and_invocation(
            &session,
            SessionCustodyBinding::New(NewCustodyRoot {
                custody_id: Uuid::new_v4(),
                canonical_repo_dir: repo.display().to_string(),
                sandbox_root: root.display().to_string(),
                sandbox_branch: "seat".into(),
                repository_identity: std::fs::canonicalize(repo.join(".git"))
                    .unwrap()
                    .display()
                    .to_string(),
                source_commit: f.source_head.clone(),
                cause: CustodyCause::FreshLaunch,
            }),
            invocation,
        )
        .unwrap();
    store
        .update_session_status(seat, SessionStatus::Running)
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE model_invocations SET status='completed',completed_at=?2 WHERE id=?1",
            params![invocation.to_string(), Utc::now().to_rfc3339()],
        )
        .unwrap();
    (seat, root)
}

/// Grant `seat` WorkPlan authority over `project` only; returns the grant version.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
async fn grant_work_plan(f: &Fixture, seat: Uuid, project: Uuid) -> i64 {
    let launch = ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "claude-sonnet-5".into(),
        effort: None,
    };
    f.handle
        .store
        .lock()
        .await
        .configure_global_manager(
            &rsi_common::global_manager::ConfigureGlobalManagerRequestV1 {
                session_id: seat,
                project_ids: vec![project],
                allowed_launches: vec![launch],
                project_policy: ManagerPolicyV2 {
                    mode: rsi_common::harness_manager_v2::ManagerOperatingModeV2::Execute,
                    capabilities: vec![ManagerCapabilityV2::WorkPlan],
                    max_created_sessions: 8,
                    ..Default::default()
                },
                expected_grant_version: 0,
                idempotency_key: "job-evidence-grant".into(),
            },
            "operator:test",
        )
        .unwrap()
        .grant_version
}

/// The seat's view of project B's `product` Work: its row version and record.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
async fn product_in(
    f: &Fixture,
    seat: Uuid,
    project: Uuid,
    grant_version: i64,
) -> (i64, crate::store::manager_ledger::WorkRecord) {
    let store = f.handle.store.lock().await;
    let authority = store
        .manager_v2_authorize_in(
            seat,
            Some(project),
            &ManagerFenceV2 {
                scope_version: grant_version,
                policy_version: grant_version,
            },
            None,
        )
        .unwrap();
    let (row, work) = store.manager_v2_work(&authority.config, "product").unwrap();
    (row.row_version, work)
}

/// #2713: a portfolio seat living in project A and granted project B cites a
/// job in a Stage update that targets B. The job's project is compared with
/// the ledger's target project (B), never with the caller's session project
/// (A): a green job it owns but recorded under A is refused as unavailable and
/// B's Work is untouched, while an equally green job recorded under B is
/// accepted for the same update.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn a_portfolio_seats_job_is_fenced_to_the_granted_target_project_not_its_own() {
    let f = fixture().await;
    let project_a = f
        .handle
        .store
        .lock()
        .await
        .get_session(f.manager)
        .unwrap()
        .unwrap()
        .project_id
        .unwrap();
    let (project_b, epic_b) = project_with_epic(&f).await;
    let (seat, seat_root) = seat_with_custody(&f, project_a).await;
    let grant_version = grant_work_plan(&f, seat, project_b).await;
    let tested = f.source_head.clone();
    let report = commit_report_at(&seat_root);

    // The seat plans an unbound Work in B through the granted target.
    let mut plan = req(
        ManagerUpdateV2::Work {
            key: "product".into(),
            expected_row_version: 0,
            epic_id: epic_b,
            title: "Granted product".into(),
            kind: ManagerWorkKindV2::Product,
            priority: 1,
            weight: 1,
            required_gates: vec![
                ManagerWorkStageV2::Implementation,
                ManagerWorkStageV2::Review,
                ManagerWorkStageV2::Verification,
            ],
            risk_tier: Default::default(),
        },
        "granted-work",
    );
    plan.project_id = Some(project_b);
    plan.fence = ManagerFenceV2 {
        scope_version: grant_version,
        policy_version: grant_version,
    };
    f.handle.agent_manager_update(seat, plan).await.unwrap();
    let (version, before) = product_in(&f, seat, project_b, grant_version).await;
    assert_eq!(version, 1);
    assert!(before.source_session_id.is_none() && before.source_commit.is_none());

    let stage = |job: Uuid, key: &str| {
        let mut request = verification(
            1,
            ManagerStageStateV2::Passed,
            cite(job, &tested, &report),
            key,
        );
        request.project_id = Some(project_b);
        request.fence = ManagerFenceV2 {
            scope_version: grant_version,
            policy_version: grant_version,
        };
        request
    };

    // Green, terminal, owned by the seat, recorded under the seat's own
    // project A: not evidence for B. A comparison with the caller's session
    // project would accept it.
    let in_a = job_in(
        &f,
        seat,
        Some(project_a),
        true,
        Some(finished(&tested, 0, &[])),
    )
    .await;
    let error = f
        .handle
        .agent_manager_update(seat, stage(in_a, "cites-a-job"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("manager_v2_job_evidence_job_unavailable"),
        "{error}"
    );
    let (version, work) = product_in(&f, seat, project_b, grant_version).await;
    assert_eq!(version, 1, "a refused citation changes nothing");
    assert!(work.stages.iter().all(|stage| stage.job_evidence.is_none()));
    let state_of = |work: &crate::store::manager_ledger::WorkRecord| {
        work.stages
            .iter()
            .map(|stage| (stage.stage, stage.state))
            .collect::<Vec<_>>()
    };
    assert_eq!(state_of(&work), state_of(&before));

    // Positive control: the same update citing a job recorded under B.
    let in_b = job_in(
        &f,
        seat,
        Some(project_b),
        true,
        Some(finished(&tested, 0, &[])),
    )
    .await;
    f.handle
        .agent_manager_update(seat, stage(in_b, "cites-b-job"))
        .await
        .unwrap();
    let (version, work) = product_in(&f, seat, project_b, grant_version).await;
    assert_eq!(version, 2);
    let verification_stage = work
        .stages
        .iter()
        .find(|stage| stage.stage == ManagerWorkStageV2::Verification)
        .unwrap();
    assert_eq!(verification_stage.state, ManagerStageStateV2::Passed);
    let observed = verification_stage.job_evidence.as_ref().unwrap();
    assert_eq!(observed.job_id, in_b);
    assert_eq!(observed.owner_session_id, seat);
    assert_eq!(observed.source_commit, tested);
    assert_eq!(observed.artifact_commit, report);
    // Project A's own `product` Work never moved.
    assert_eq!(product(&f).await.0, 1);
}
