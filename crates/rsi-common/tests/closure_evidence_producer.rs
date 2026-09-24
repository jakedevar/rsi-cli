use std::path::Path;
use std::process::Command;

use rsi_common::closure_kernel::{
    CLOSURE_REVIEW_ARTIFACT_SCHEMA_VERSION, ClosureAuditResultV1, ClosureGitShaV1,
    ClosureReviewArtifactV1, ClosureReviewVerdictV1, ClosureReviewerIdentityV1,
    ClosureScopePolicyAuditV1,
};
use rsi_common::types::Sha256Digest;
use uuid::Uuid;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("UTF-8 git output")
        .trim()
        .to_string()
}

#[test]
fn closure_review_lane_binds_environment_runs_real_cli_and_advances_only_evidence_ref() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let sealer = workspace.join("scripts/seal-closure-review-evidence.sh");
    let validator = env!("CARGO_BIN_EXE_rsi-closure-evidence-validate");
    let directory = tempfile::tempdir().expect("temporary repository");
    let repository = directory.path().join("repository");
    let source_worktree = directory.path().join("source");
    let evidence_worktree = directory.path().join("evidence");
    std::fs::create_dir_all(&repository).expect("repository directory");
    git(&repository, &["init", "-q"]);
    git(
        &repository,
        &["config", "user.email", "closure@example.test"],
    );
    git(&repository, &["config", "user.name", "Closure Test"]);
    std::fs::write(repository.join("tracked"), "sealed\n").expect("tracked source");
    git(&repository, &["add", "tracked"]);
    git(&repository, &["commit", "-qm", "sealed source"]);
    let sealed = git(&repository, &["rev-parse", "HEAD"]);
    let source_ref = "refs/heads/closure-source-proof";
    git(
        &repository,
        &[
            "worktree",
            "add",
            "-qb",
            source_ref.trim_start_matches("refs/heads/"),
            source_worktree.to_str().expect("source path"),
            &sealed,
        ],
    );
    git(
        &repository,
        &[
            "worktree",
            "add",
            "-qb",
            "closure-evidence-proof",
            evidence_worktree.to_str().expect("evidence path"),
            &sealed,
        ],
    );

    let program_id = Uuid::new_v4();
    let source_id = Uuid::new_v4();
    let reviewer_session_id = Uuid::new_v4();
    let model_invocation_id = Uuid::new_v4();
    let review_path =
        format!("thoughts/shared/reviews/closure/{program_id}/{source_id}-review-v1.json");
    let manifest_path =
        format!("thoughts/shared/verification/closure/{program_id}/{source_id}-manifest-v2.md");
    let review_policy_digest =
        Sha256Digest::parse(format!("sha256:{}", "c".repeat(64))).expect("policy digest");
    let review = serde_json::to_string_pretty(&ClosureReviewArtifactV1 {
        schema_version: CLOSURE_REVIEW_ARTIFACT_SCHEMA_VERSION,
        reviewed_source_head: ClosureGitShaV1::parse(sealed.clone()).expect("sealed SHA"),
        reviewer: ClosureReviewerIdentityV1 {
            session_id: reviewer_session_id,
            model_invocation_id,
        },
        verdict: ClosureReviewVerdictV1::Accepted,
        findings: Vec::new(),
        unresolved_finding_ids: Vec::new(),
        scope_policy_audit: ClosureScopePolicyAuditV1 {
            scope_result: ClosureAuditResultV1::Pass,
            policy_result: ClosureAuditResultV1::Pass,
            reviewed_scope: vec!["real Closure review producer lane".into()],
            review_policy_digest: review_policy_digest.clone(),
        },
    })
    .expect("strict review JSON");
    let manifest = format!(
        "---\nschema_version: 2\nsource_head: {sealed}\nticket: K1\nplan_doc: thoughts/shared/plans/2026-08-14-p0-1-closure-kernel.md\ngenerated: 2026-08-14T12:00:00Z\nphases_sealed: [1]\nstatus: verified\n---\n\n# Verification Manifest - K1\n\n## Phase 1 - Closure\n\n### Automated\n- [PASS] real validator and producer boundary\n  satisfies: F-003\n\n### Daemon-level\n- (none)\n\n### TUI manual\n- (none)\n"
    );
    for (path, bytes) in [
        (review_path.as_str(), review.as_bytes()),
        (manifest_path.as_str(), manifest.as_bytes()),
    ] {
        let absolute = evidence_worktree.join(path);
        std::fs::create_dir_all(absolute.parent().expect("artifact parent"))
            .expect("artifact directory");
        std::fs::write(absolute, bytes).expect("reviewer-produced artifact");
    }

    let raw_digest = Command::new(validator)
        .current_dir(&evidence_worktree)
        .args([
            "--source-head",
            &sealed,
            "--reviewer-session-id",
            &reviewer_session_id.to_string(),
            "--model-invocation-id",
            &model_invocation_id.to_string(),
            "--review-policy-digest",
            &"c".repeat(64),
            "--review",
            &review_path,
            "--manifest",
            &manifest_path,
        ])
        .output()
        .expect("run validator with synthetic raw digest");
    assert!(!raw_digest.status.success());
    assert!(
        String::from_utf8_lossy(&raw_digest.stderr).contains("must start with sha256:"),
        "the malformed synthetic raw digest stays noncanonical"
    );

    let invoke = |invocation_env: Uuid| {
        Command::new(&sealer)
            .current_dir(&evidence_worktree)
            .env("RSI_SESSION_ID", reviewer_session_id.to_string())
            .env("RSI_MODEL_INVOCATION_ID", invocation_env.to_string())
            .env("RSI_CLOSURE_EVIDENCE_VALIDATOR_BIN", validator)
            .args([
                "--source-head",
                &sealed,
                "--source-ref",
                source_ref,
                "--source-worktree",
                source_worktree.to_str().expect("source worktree"),
                "--reviewer-session-id",
                &reviewer_session_id.to_string(),
                "--model-invocation-id",
                &model_invocation_id.to_string(),
                "--review-policy-digest",
                review_policy_digest.as_str(),
                "--review",
                &review_path,
                "--manifest",
                &manifest_path,
            ])
            .output()
            .expect("run Closure evidence sealer")
    };
    let mismatched = invoke(Uuid::new_v4());
    assert!(!mismatched.status.success());
    assert_eq!(git(&evidence_worktree, &["rev-parse", "HEAD"]), sealed);

    let sealed_output = invoke(model_invocation_id);
    assert!(
        sealed_output.status.success(),
        "sealer: {}",
        String::from_utf8_lossy(&sealed_output.stderr)
    );
    let evidence_commit = String::from_utf8(sealed_output.stdout)
        .expect("evidence commit output")
        .lines()
        .last()
        .expect("evidence commit final line")
        .trim()
        .to_string();
    assert_eq!(
        git(&evidence_worktree, &["rev-parse", "HEAD"]),
        evidence_commit
    );
    assert_eq!(git(&repository, &["rev-parse", source_ref]), sealed);
    assert_eq!(git(&source_worktree, &["rev-parse", "HEAD"]), sealed);
    assert!(git(&source_worktree, &["status", "--porcelain"]).is_empty());
    assert_eq!(
        git(&repository, &["rev-parse", &format!("{evidence_commit}^")]),
        sealed
    );
    assert_eq!(
        git(
            &repository,
            &[
                "diff-tree",
                "--no-commit-id",
                "--name-only",
                "-r",
                &evidence_commit,
            ],
        )
        .lines()
        .collect::<Vec<_>>(),
        [review_path, manifest_path]
    );
}

#[test]
fn non_closure_v1_manifest_parser_path_remains_unchanged() {
    let legacy = "---\nticket: RSI-1\nplan_doc: thoughts/shared/plans/legacy.md\ngenerated: 2026-08-14T12:00:00Z\nphases_sealed: [1]\nstatus: verified\n---\n\n# Verification Manifest\n\n## Phase 1 - Legacy\n\n### Automated\n- [PASS] legacy V1 producer\n\n### Daemon-level\n- (none)\n\n### TUI manual\n- (none)\n";
    let parsed = rsi_common::verification_manifest::parse(legacy)
        .expect("ordinary non-Closure V1 manifest remains accepted");
    assert_eq!(parsed.frontmatter.schema_version, 1);
    assert!(parsed.frontmatter.source_head.is_none());
}
