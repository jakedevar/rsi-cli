//! RSI-021 — Integration tests for the version-controlled pre-commit hook.
//!
//! Each test sets up an isolated git repo in a tempdir, copies the hook
//! source from the project tree (via `CARGO_MANIFEST_DIR`), installs it,
//! stages a path that exercises one cell of the rule table, sets
//! `CLAUDE_AGENT_ROLE`, runs `git commit`, and asserts the exit status.
//!
//! Rule table mirrored in `tools/git-hooks/pre-commit`:
//!   pipeline-research  → branch=main, path=thoughts/shared/research/**
//!   pipeline-plan      → branch=main, path=thoughts/shared/plans/**
//!   pipeline-implement → branch != main
//!   <unset>            → no enforcement (human commits)
//!   <unknown>          → fail closed

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// Path to the tracked hook source. Resolves from the test crate's manifest
/// dir up to the workspace root, then into `tools/git-hooks/pre-commit`.
fn hook_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("tools/git-hooks/pre-commit"))
        .expect("workspace root resolves from CARGO_MANIFEST_DIR")
}

fn setup_repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    run(
        &dir.path().to_path_buf(),
        &["git", "init", "-q", "-b", "main"],
    );
    run(
        &dir.path().to_path_buf(),
        &["git", "config", "user.email", "t@t"],
    );
    run(
        &dir.path().to_path_buf(),
        &["git", "config", "user.name", "t"],
    );
    // Initial commit so HEAD exists and branch operations work.
    std::fs::write(dir.path().join("README.md"), "init").unwrap();
    run(&dir.path().to_path_buf(), &["git", "add", "README.md"]);
    run(
        &dir.path().to_path_buf(),
        &["git", "commit", "-q", "-m", "init"],
    );

    // Copy the hook into .git/hooks/pre-commit and chmod +x.
    let hooks_dir = dir.path().join(".git/hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    let dest = hooks_dir.join("pre-commit");
    std::fs::copy(hook_source(), &dest).expect("copy hook");
    set_executable(&dest);
    dir
}

fn set_executable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(p).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(p, perms).unwrap();
}

/// Run a command in the repo root and assert it exits zero.
fn run(repo: &PathBuf, argv: &[&str]) {
    let status = Command::new(argv[0])
        .args(&argv[1..])
        .current_dir(repo)
        .status()
        .expect("spawn cmd");
    assert!(status.success(), "command failed: {:?}", argv);
}

/// Stage a file (creating parent dirs) and commit with the given role.
/// Returns the exit status of `git commit`.
fn commit_with_role(
    repo: &Path,
    role: Option<&str>,
    rel_path: &str,
    contents: &str,
) -> std::process::ExitStatus {
    let path = repo.join(rel_path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, contents).unwrap();
    Command::new("git")
        .args(["add", rel_path])
        .current_dir(repo)
        .status()
        .expect("git add");

    let mut cmd = Command::new("git");
    cmd.args(["commit", "-q", "-m", "test"]).current_dir(repo);
    match role {
        Some(r) => {
            cmd.env("CLAUDE_AGENT_ROLE", r);
        }
        None => {
            cmd.env_remove("CLAUDE_AGENT_ROLE");
        }
    }
    cmd.status().expect("git commit")
}

fn checkout_new_branch(repo: &Path, name: &str) {
    Command::new("git")
        .args(["checkout", "-q", "-b", name])
        .current_dir(repo)
        .status()
        .expect("git checkout -b");
}

// --- Role: pipeline-research ------------------------------------------------

#[test]
fn pipeline_research_on_main_thoughts_research_path_passes() {
    let dir = setup_repo();
    let status = commit_with_role(
        dir.path(),
        Some("pipeline-research"),
        "thoughts/shared/research/2026-04-25-test.md",
        "# research\n",
    );
    assert!(
        status.success(),
        "expected commit to succeed; status: {:?}",
        status.code()
    );
}

#[test]
fn pipeline_research_on_main_thoughts_plans_path_rejects() {
    let dir = setup_repo();
    let status = commit_with_role(
        dir.path(),
        Some("pipeline-research"),
        "thoughts/shared/plans/2026-04-25-test.md",
        "# plan\n",
    );
    assert_eq!(status.code(), Some(1));
}

#[test]
fn pipeline_research_on_feature_branch_rejects() {
    let dir = setup_repo();
    checkout_new_branch(dir.path(), "feature-x");
    let status = commit_with_role(
        dir.path(),
        Some("pipeline-research"),
        "thoughts/shared/research/2026-04-25-test.md",
        "# research\n",
    );
    assert_eq!(status.code(), Some(1));
}

// --- Role: pipeline-plan ----------------------------------------------------

#[test]
fn pipeline_plan_on_main_thoughts_plans_path_passes() {
    let dir = setup_repo();
    let status = commit_with_role(
        dir.path(),
        Some("pipeline-plan"),
        "thoughts/shared/plans/2026-04-25-test.md",
        "# plan\n",
    );
    assert!(
        status.success(),
        "expected commit to succeed; status: {:?}",
        status.code()
    );
}

#[test]
fn pipeline_plan_on_main_thoughts_research_path_rejects() {
    let dir = setup_repo();
    let status = commit_with_role(
        dir.path(),
        Some("pipeline-plan"),
        "thoughts/shared/research/2026-04-25-test.md",
        "# research\n",
    );
    assert_eq!(status.code(), Some(1));
}

#[test]
fn pipeline_plan_on_feature_branch_rejects() {
    let dir = setup_repo();
    checkout_new_branch(dir.path(), "feature-x");
    let status = commit_with_role(
        dir.path(),
        Some("pipeline-plan"),
        "thoughts/shared/plans/2026-04-25-test.md",
        "# plan\n",
    );
    assert_eq!(status.code(), Some(1));
}

// --- Role: pipeline-implement -----------------------------------------------

#[test]
fn pipeline_implement_on_main_rejects() {
    let dir = setup_repo();
    let status = commit_with_role(
        dir.path(),
        Some("pipeline-implement"),
        "src/foo.rs",
        "// code\n",
    );
    assert_eq!(status.code(), Some(1));
}

#[test]
fn pipeline_implement_on_feature_branch_passes() {
    let dir = setup_repo();
    checkout_new_branch(dir.path(), "feature-x");
    let status = commit_with_role(
        dir.path(),
        Some("pipeline-implement"),
        "src/foo.rs",
        "// code\n",
    );
    assert!(
        status.success(),
        "expected commit to succeed; status: {:?}",
        status.code()
    );
}

// --- Role: unset / unknown --------------------------------------------------

#[test]
fn unset_role_passes_any_branch_any_path() {
    let dir = setup_repo();
    // Main branch, code path — would be rejected for any pipeline role.
    let status = commit_with_role(dir.path(), None, "src/foo.rs", "// human commit\n");
    assert!(
        status.success(),
        "expected human commit to succeed; status: {:?}",
        status.code()
    );

    // Feature branch, thoughts path — also fine for humans.
    checkout_new_branch(dir.path(), "feature-x");
    let status = commit_with_role(dir.path(), None, "thoughts/shared/plans/p.md", "# plan\n");
    assert!(status.success());
}

#[test]
fn unknown_role_rejects() {
    let dir = setup_repo();
    let status = commit_with_role(
        dir.path(),
        Some("pipeline-frobnicate"),
        "thoughts/shared/research/r.md",
        "# r\n",
    );
    assert_eq!(status.code(), Some(1));
}
