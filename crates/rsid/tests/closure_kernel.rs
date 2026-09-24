use std::path::Path;

#[test]
fn closure_review_command_contract_routes_through_executable_sealer_and_preserves_v1_lane() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let overlay = std::fs::read_to_string(
        root.join(".claude/commands/_shared/master_orchestrate_rsi_overlay.md"),
    )
    .expect("master_orchestrate RSI overlay");
    let closure_policy = std::fs::read_to_string(
        root.join(".claude/commands/_shared/master_orchestrate_rsi_closure.md"),
    )
    .expect("master_orchestrate Closure reference");
    let implement = std::fs::read_to_string(root.join(".claude/commands/master_implement.md"))
        .expect("master_implement command");
    let preamble =
        std::fs::read_to_string(root.join(".claude/commands/_shared/worker_preamble.md"))
            .expect("worker preamble");
    let sealer = root.join("scripts/seal-closure-review-evidence.sh");
    let metadata = std::fs::metadata(&sealer).expect("Closure evidence sealer");
    assert!(metadata.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(metadata.permissions().mode() & 0o111, 0);
    }
    let sealer_body = std::fs::read_to_string(sealer).expect("Closure evidence sealer body");
    for required in [
        "RSI_SESSION_ID",
        "RSI_MODEL_INVOCATION_ID",
        "rsi-closure-evidence-validate",
        "git diff --cached --name-only",
        "git -C \"$source_worktree\" rev-parse HEAD",
    ] {
        assert!(sealer_body.contains(required), "missing `{required}`");
    }
    for required in [
        "reviewer_session_id",
        "reviewer_model_invocation_id",
        "review_json_path",
        "manifest_v2_path",
        "sealed_source_sha",
        "evidence_commit_sha",
        "scripts/seal-closure-review-evidence.sh",
        "rsi-closure-evidence-validate",
    ] {
        assert!(closure_policy.contains(required), "missing `{required}`");
        assert!(implement.contains(required), "missing `{required}`");
        assert!(preamble.contains(required), "missing `{required}`");
    }
    assert!(overlay.contains("master_orchestrate_rsi_closure.md"));
    assert!(closure_policy.contains("non-Closure review is unchanged"));
    assert!(implement.contains("Non-Closure execution\ncontinues through the V1"));
    assert!(preamble.contains("ordinary V1 manifest lane above is unchanged"));
}
