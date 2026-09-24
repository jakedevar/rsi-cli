//! Hermetic engine tests. Every repository here is a throwaway under a temp
//! directory; the host repository and live worktrees are never fixtures.

use super::engine::{abort_cleanup_record, ensure_active_lock, file_identity, recovery_proof};
use super::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;
use uuid::Uuid;

const TARGET: &str = "refs/heads/rolling";

fn git_status(dir: &Path, args: &[&str]) -> std::process::Output {
    let mut command = StdCommand::new("git");
    command
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-C",
        ])
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env("HOME", "/nonexistent")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    for key in [
        "XDG_CONFIG_HOME",
        "XDG_CONFIG_DIRS",
        "GIT_CONFIG",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_COUNT",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
        "GIT_CEILING_DIRECTORIES",
        "GIT_REPLACE_REF_BASE",
        "GIT_GRAFT_FILE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_HOOKS_PATH",
        "GIT_EDITOR",
        "GIT_SEQUENCE_EDITOR",
        "GIT_ASKPASS",
        "SSH_ASKPASS",
        "GIT_TERMINAL_PROMPT",
    ] {
        command.env_remove(key);
    }
    command.output().expect("git is runnable")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = git_status(dir, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output is UTF-8")
        .trim()
        .to_string()
}

struct Fixture {
    root: TempDir,
    repo: PathBuf,
    scratch: PathBuf,
    base: String,
    builders: AtomicUsize,
}

impl Fixture {
    /// One-commit repository. `held` leaves `rolling` checked out in the main
    /// worktree; otherwise HEAD parks on another branch so nothing holds it.
    fn new(held: bool) -> Self {
        let root = tempfile::tempdir().expect("temp root");
        let repo = root.path().join("repo");
        let scratch = root.path().join("scratch");
        std::fs::create_dir_all(&repo).expect("repo dir");
        std::fs::create_dir_all(&scratch).expect("scratch dir");
        git(&repo, &["init", "-q", "-b", "rolling"]);
        std::fs::write(repo.join("shared.txt"), "base\n").expect("seed file");
        git(&repo, &["add", "shared.txt"]);
        git(&repo, &["commit", "-q", "-m", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        if !held {
            git(&repo, &["checkout", "-q", "-b", "parking"]);
        }
        Self {
            root,
            repo,
            scratch,
            base,
            builders: AtomicUsize::new(0),
        }
    }

    /// New commit on top of `parent`, built in a throwaway detached worktree so
    /// the main worktree and every branch stay exactly as they were.
    fn commit(&self, parent: &str, file: &str, content: &str) -> String {
        let index = self.builders.fetch_add(1, Ordering::Relaxed);
        let builder = self.root.path().join(format!("builder-{index}"));
        let path = builder.to_str().expect("UTF-8 temp path");
        git(
            &self.repo,
            &["worktree", "add", "-q", "--detach", path, parent],
        );
        std::fs::write(builder.join(file), content).expect("fixture file");
        git(&builder, &["add", file]);
        git(&builder, &["commit", "-q", "-m", file]);
        let oid = git(&builder, &["rev-parse", "HEAD"]);
        git(&self.repo, &["worktree", "remove", path]);
        oid
    }

    fn set_ref(&self, name: &str, oid: &str) {
        git(&self.repo, &["update-ref", name, oid]);
    }

    fn resolve(&self, name: &str) -> String {
        git(&self.repo, &["rev-parse", "--verify", name])
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        git_status(
            &self.repo,
            &["merge-base", "--is-ancestor", ancestor, descendant],
        )
        .status
        .success()
    }

    fn registered_worktrees(&self) -> usize {
        git(&self.repo, &["worktree", "list", "--porcelain"])
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count()
    }

    fn scratch_entries(&self) -> usize {
        std::fs::read_dir(&self.scratch).expect("scratch").count()
    }

    async fn prepare(&self, expected_tip: &str, source: &str) -> Result<Prepared> {
        prepare_candidate(
            &config(&[TARGET]),
            &self.repo,
            TARGET,
            expected_tip,
            source,
            &self.scratch,
        )
        .await
    }

    async fn publish(&self, expected_tip: &str, candidate: &str) -> Result<()> {
        publish(
            &config(&[TARGET]),
            &self.repo,
            TARGET,
            expected_tip,
            candidate,
        )
        .await
    }
}

fn config(allowed: &[&str]) -> IntegrationConfig {
    IntegrationConfig {
        allowed_targets: allowed.iter().copied().map(str::to_owned).collect(),
        identity: CommitIdentity {
            name: "rsi integration".to_string(),
            email: "integration@rsi.invalid".to_string(),
        },
        git_timeout: Duration::from_secs(60),
    }
}

fn candidate(prepared: Prepared) -> Candidate {
    match prepared {
        Prepared::Candidate(candidate) => candidate,
        Prepared::AlreadyIntegrated => panic!("expected a candidate, source already integrated"),
    }
}

fn refusal<T: std::fmt::Debug>(result: Result<T>) -> Refusal {
    match result {
        Err(IntegrationError::Refused(refusal)) => refusal,
        other => panic!("expected a typed refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn fast_forward_publishes_source_as_target() {
    let fixture = Fixture::new(false);
    let source = fixture.commit(&fixture.base, "feature.txt", "feature\n");

    let built = candidate(fixture.prepare(&fixture.base, &source).await.unwrap());
    assert_eq!(built.kind, CandidateKind::FastForward);
    assert_eq!(built.oid, source);
    assert_eq!(built.base_tip, fixture.base);
    assert_eq!(
        std::fs::read_to_string(built.handle.worktree.join("feature.txt")).unwrap(),
        "feature\n"
    );
    assert_eq!(
        fixture.resolve(TARGET),
        fixture.base,
        "prepare moves no ref"
    );

    fixture.publish(&built.base_tip, &built.oid).await.unwrap();
    assert_eq!(fixture.resolve(TARGET), source);

    discard_candidate(&config(&[TARGET]), &fixture.repo, &built.handle)
        .await
        .unwrap();
    assert_eq!(fixture.registered_worktrees(), 1);
    assert_eq!(fixture.scratch_entries(), 0);
}

#[tokio::test]
async fn diverged_histories_publish_a_two_parent_merge() {
    let fixture = Fixture::new(false);
    let tip = fixture.commit(&fixture.base, "tip.txt", "tip\n");
    fixture.set_ref(TARGET, &tip);
    let source = fixture.commit(&fixture.base, "feature.txt", "feature\n");

    let built = candidate(fixture.prepare(&tip, &source).await.unwrap());
    assert_eq!(built.kind, CandidateKind::Merge);
    let parents = git(
        &fixture.repo,
        &["rev-list", "--parents", "-n", "1", &built.oid],
    );
    assert_eq!(
        parents.split_whitespace().collect::<Vec<_>>(),
        vec![built.oid.as_str(), tip.as_str(), source.as_str()]
    );
    for (file, content) in [("tip.txt", "tip\n"), ("feature.txt", "feature\n")] {
        assert_eq!(
            std::fs::read_to_string(built.handle.worktree.join(file)).unwrap(),
            content
        );
    }
    assert_eq!(
        git(
            &fixture.repo,
            &["log", "-1", "--format=%an <%ae>", &built.oid]
        ),
        "rsi integration <integration@rsi.invalid>"
    );

    fixture.publish(&built.base_tip, &built.oid).await.unwrap();
    let target = fixture.resolve(TARGET);
    assert_eq!(target, built.oid);
    assert!(fixture.is_ancestor(&tip, &target));
    assert!(fixture.is_ancestor(&source, &target));
}

#[tokio::test]
async fn already_integrated_source_leaves_target_unchanged() {
    let fixture = Fixture::new(false);
    let tip = fixture.commit(&fixture.base, "tip.txt", "tip\n");
    fixture.set_ref(TARGET, &tip);

    let prepared = fixture.prepare(&tip, &fixture.base).await.unwrap();
    assert_eq!(prepared, Prepared::AlreadyIntegrated);
    assert_eq!(fixture.resolve(TARGET), tip);
    assert_eq!(fixture.scratch_entries(), 0);
}

#[tokio::test]
async fn conflicting_merge_reports_paths_and_cleans_up() {
    let fixture = Fixture::new(false);
    let tip = fixture.commit(&fixture.base, "shared.txt", "tip\n");
    fixture.set_ref(TARGET, &tip);
    let source = fixture.commit(&fixture.base, "shared.txt", "source\n");

    let refused = refusal(fixture.prepare(&tip, &source).await);
    assert_eq!(
        refused,
        Refusal::Conflict {
            paths: vec!["shared.txt".to_string()]
        }
    );
    assert_eq!(fixture.resolve(TARGET), tip);
    assert_eq!(fixture.registered_worktrees(), 1);
    assert_eq!(fixture.scratch_entries(), 0);
}

#[tokio::test]
async fn stale_tip_at_prepare_is_refused() {
    let fixture = Fixture::new(false);
    let tip = fixture.commit(&fixture.base, "tip.txt", "tip\n");
    fixture.set_ref(TARGET, &tip);
    let source = fixture.commit(&fixture.base, "feature.txt", "feature\n");

    let refused = refusal(fixture.prepare(&fixture.base, &source).await);
    assert_eq!(
        refused,
        Refusal::StaleTarget {
            observed: Some(tip.clone())
        }
    );
    assert_eq!(fixture.resolve(TARGET), tip);
    assert_eq!(fixture.scratch_entries(), 0);
}

#[tokio::test]
async fn stale_tip_at_publish_keeps_concurrent_writer() {
    let fixture = Fixture::new(false);
    let source = fixture.commit(&fixture.base, "feature.txt", "feature\n");
    let built = candidate(fixture.prepare(&fixture.base, &source).await.unwrap());

    let concurrent = fixture.commit(&fixture.base, "other.txt", "other\n");
    fixture.set_ref(TARGET, &concurrent);

    let refused = refusal(fixture.publish(&built.base_tip, &built.oid).await);
    assert_eq!(
        refused,
        Refusal::StaleTarget {
            observed: Some(concurrent.clone())
        }
    );
    assert_eq!(fixture.resolve(TARGET), concurrent);
}

#[tokio::test]
async fn non_descendant_candidate_is_not_fast_forward() {
    let fixture = Fixture::new(false);
    let tip = fixture.commit(&fixture.base, "tip.txt", "tip\n");
    fixture.set_ref(TARGET, &tip);
    let sibling = fixture.commit(&fixture.base, "side.txt", "side\n");

    let refused = refusal(fixture.publish(&tip, &sibling).await);
    assert_eq!(refused, Refusal::NotFastForward);
    assert_eq!(fixture.resolve(TARGET), tip);
}

#[tokio::test]
async fn held_clean_target_advances_in_place() {
    let fixture = Fixture::new(true);
    let source = fixture.commit(&fixture.base, "feature.txt", "feature\n");
    let built = candidate(fixture.prepare(&fixture.base, &source).await.unwrap());

    fixture.publish(&built.base_tip, &built.oid).await.unwrap();

    assert_eq!(fixture.resolve(TARGET), source);
    assert_eq!(fixture.resolve("HEAD"), source);
    assert_eq!(
        std::fs::read_to_string(fixture.repo.join("feature.txt")).unwrap(),
        "feature\n"
    );
    assert_eq!(git(&fixture.repo, &["status", "--porcelain"]), "");
    assert_eq!(
        git(&fixture.repo, &["symbolic-ref", "HEAD"]),
        TARGET,
        "the holder stays on its branch"
    );
}

#[tokio::test]
async fn held_dirty_target_is_refused_and_preserved() {
    let fixture = Fixture::new(true);
    let source = fixture.commit(&fixture.base, "feature.txt", "feature\n");
    std::fs::write(fixture.repo.join("shared.txt"), "operator edit\n").unwrap();

    let refused = refusal(fixture.publish(&fixture.base, &source).await);
    let Refusal::TargetWorktreeDirty { path } = refused else {
        panic!("expected TargetWorktreeDirty, got {refused:?}");
    };
    assert_eq!(
        std::fs::canonicalize(path).unwrap(),
        std::fs::canonicalize(&fixture.repo).unwrap()
    );
    assert_eq!(fixture.resolve(TARGET), fixture.base);
    assert_eq!(
        std::fs::read_to_string(fixture.repo.join("shared.txt")).unwrap(),
        "operator edit\n"
    );
}

#[tokio::test]
async fn held_target_with_colliding_untracked_file_is_refused_and_preserved() {
    let fixture = Fixture::new(true);
    let source = fixture.commit(&fixture.base, "feature.txt", "feature\n");
    std::fs::write(fixture.repo.join("feature.txt"), "operator scratch\n").unwrap();

    let refused = refusal(fixture.publish(&fixture.base, &source).await);
    assert!(
        matches!(refused, Refusal::TargetWorktreeDirty { .. }),
        "expected TargetWorktreeDirty, got {refused:?}"
    );
    assert_eq!(fixture.resolve(TARGET), fixture.base);
    assert_eq!(
        std::fs::read_to_string(fixture.repo.join("feature.txt")).unwrap(),
        "operator scratch\n"
    );
}

#[tokio::test]
async fn held_target_with_non_colliding_untracked_file_is_refused_and_preserved() {
    let fixture = Fixture::new(true);
    let source = fixture.commit(&fixture.base, "feature.txt", "feature\n");
    // A non-colliding untracked file: the fast-forward would not overwrite it,
    // but strict cleanliness still refuses publication.
    let untracked = fixture.repo.join("unrelated.txt");
    std::fs::write(&untracked, "untracked\n").unwrap();

    let refused = refusal(fixture.publish(&fixture.base, &source).await);
    assert!(
        matches!(refused, Refusal::TargetWorktreeDirty { .. }),
        "expected TargetWorktreeDirty, got {refused:?}"
    );
    assert_eq!(fixture.resolve(TARGET), fixture.base);
    assert_eq!(std::fs::read_to_string(&untracked).unwrap(), "untracked\n");
}

#[tokio::test]
async fn held_target_mid_rebase_is_refused_and_preserved() {
    let fixture = Fixture::new(true);
    // Target diverges from base on shared.txt.
    std::fs::write(fixture.repo.join("shared.txt"), "target\n").unwrap();
    git(&fixture.repo, &["add", "shared.txt"]);
    git(&fixture.repo, &["commit", "-q", "-m", "target-change"]);
    let tip = fixture.resolve(TARGET);
    // Source diverges from base on the same file => rebase conflict.
    let rebase_source = fixture.commit(&fixture.base, "shared.txt", "source\n");
    // Candidate is a fast-forward from the target tip (different file).
    let candidate = fixture.commit(&tip, "new.txt", "new\n");

    // Start a rebase that stops on conflict. The main worktree detaches
    // mid-rebase; refs/heads/rolling still points at the pre-rebase tip.
    let rebase = git_status(&fixture.repo, &["rebase", &rebase_source]);
    assert!(
        !rebase.status.success(),
        "rebase must stop on conflict: {}",
        String::from_utf8_lossy(&rebase.stderr)
    );

    // Publish must fail closed: the rebase owns the target.
    let refused = refusal(fixture.publish(&tip, &candidate).await);
    let Refusal::TargetOperationInProgress { path } = &refused else {
        panic!("expected TargetOperationInProgress, got {refused:?}");
    };
    assert_eq!(
        std::fs::canonicalize(path).unwrap(),
        std::fs::canonicalize(&fixture.repo).unwrap()
    );
    // The ref is unchanged.
    assert_eq!(fixture.resolve(TARGET), tip);
}

#[tokio::test]
async fn protected_targets_are_denied_even_when_allowlisted() {
    let fixture = Fixture::new(false);
    let source = fixture.commit(&fixture.base, "feature.txt", "feature\n");
    let protected = ["refs/heads/main", "refs/heads/master", "refs/heads/MAIN"];
    let allow_everything = config(&protected);
    for target in protected {
        fixture.set_ref(target, &fixture.base);

        let prepared = prepare_candidate(
            &allow_everything,
            &fixture.repo,
            target,
            &fixture.base,
            &source,
            &fixture.scratch,
        )
        .await;
        assert_eq!(refusal(prepared), Refusal::TargetDenied, "{target} prepare");

        let published = publish(
            &allow_everything,
            &fixture.repo,
            target,
            &fixture.base,
            &source,
        )
        .await;
        assert_eq!(
            refusal(published),
            Refusal::TargetDenied,
            "{target} publish"
        );
        assert_eq!(fixture.resolve(target), fixture.base);
    }
    assert_eq!(fixture.scratch_entries(), 0);
}

#[tokio::test]
async fn target_outside_the_allowlist_is_denied() {
    let fixture = Fixture::new(false);
    let source = fixture.commit(&fixture.base, "feature.txt", "feature\n");
    fixture.set_ref("refs/heads/other", &fixture.base);

    let published = publish(
        &config(&[TARGET]),
        &fixture.repo,
        "refs/heads/other",
        &fixture.base,
        &source,
    )
    .await;
    assert_eq!(refusal(published), Refusal::TargetDenied);
    assert_eq!(fixture.resolve("refs/heads/other"), fixture.base);
}

#[tokio::test]
async fn source_that_is_not_a_commit_is_refused() {
    let fixture = Fixture::new(false);
    let blob = git(
        &fixture.repo,
        &["rev-parse", &format!("{}:shared.txt", fixture.base)],
    );
    let missing = "0123456789abcdef0123456789abcdef01234567";

    for source in [blob.as_str(), missing] {
        let refused = refusal(fixture.prepare(&fixture.base, source).await);
        assert_eq!(refused, Refusal::InvalidSource, "{source}");
    }
    let revision = fixture.prepare(&fixture.base, "parking").await;
    assert!(
        matches!(revision, Err(IntegrationError::InvalidInput(_))),
        "revision names are rejected before reaching Git: {revision:?}"
    );
    assert_eq!(fixture.resolve(TARGET), fixture.base);
    assert_eq!(fixture.scratch_entries(), 0);
}

#[tokio::test]
async fn discard_refuses_worktrees_the_engine_did_not_create() {
    let fixture = Fixture::new(false);
    let settings = config(&[TARGET]);

    let plain = fixture.scratch.join("plain-directory");
    std::fs::create_dir_all(&plain).unwrap();
    let forged = CandidateHandle {
        worktree: plain.clone(),
        id: "forged".to_string(),
    };
    let refused = refusal(discard_candidate(&settings, &fixture.repo, &forged).await);
    assert_eq!(refused, Refusal::NotEngineCandidate);
    assert!(plain.is_dir());

    let foreign = fixture.root.path().join("foreign-worktree");
    let foreign_path = foreign.to_str().unwrap();
    git(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            foreign_path,
            &fixture.base,
        ],
    );
    let forged = CandidateHandle {
        worktree: foreign.clone(),
        id: "forged".to_string(),
    };
    let refused = refusal(discard_candidate(&settings, &fixture.repo, &forged).await);
    assert_eq!(refused, Refusal::NotEngineCandidate);
    assert!(foreign.join("shared.txt").is_file());

    let source = fixture.commit(&fixture.base, "feature.txt", "feature\n");
    let built = candidate(fixture.prepare(&fixture.base, &source).await.unwrap());
    let wrong_id = CandidateHandle {
        worktree: built.handle.worktree.clone(),
        id: "forged".to_string(),
    };
    let refused = refusal(discard_candidate(&settings, &fixture.repo, &wrong_id).await);
    assert_eq!(refused, Refusal::NotEngineCandidate);
    assert!(built.handle.worktree.join("feature.txt").is_file());

    discard_candidate(&settings, &fixture.repo, &built.handle)
        .await
        .unwrap();
    assert_eq!(
        fixture.registered_worktrees(),
        2,
        "main plus the foreign worktree"
    );
}

fn shell(script: &str, timeout: Duration) -> GuardCommand {
    GuardCommand {
        program: "sh".to_string(),
        args: vec!["-c".to_string(), script.to_string()],
        timeout,
    }
}

fn spec(commands: Vec<GuardCommand>) -> GuardSpec {
    GuardSpec {
        commands,
        env: BTreeMap::from([("GUARD_VALUE".to_string(), "42".to_string())]),
        output_tail_bytes: 4096,
    }
}

#[tokio::test]
async fn guard_passes_only_when_every_command_passes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("marker"), "present").unwrap();
    let limit = Duration::from_secs(30);

    let report = run_guard(
        dir.path(),
        &spec(vec![
            shell("test -f marker", limit),
            shell("test \"$GUARD_VALUE\" = 42", limit),
        ]),
    )
    .await;
    assert!(report.passed, "{report:?}");
    assert_eq!(report.commands.len(), 2);

    let empty = run_guard(dir.path(), &spec(Vec::new())).await;
    assert!(
        !empty.passed,
        "a guard that verifies nothing is never green"
    );
}

#[tokio::test]
async fn guard_failure_carries_status_and_output_and_stops_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let limit = Duration::from_secs(30);

    let report = run_guard(
        dir.path(),
        &spec(vec![
            shell("echo visible-out; echo visible-err >&2; exit 7", limit),
            shell("touch second-command-ran", limit),
        ]),
    )
    .await;
    assert!(!report.passed);
    assert_eq!(report.commands.len(), 1, "the first failure stops the run");
    let failed = &report.commands[0];
    assert_eq!(failed.status, GuardStatus::Failed { code: Some(7) });
    assert!(failed.stdout_tail.contains("visible-out"), "{failed:?}");
    assert!(failed.stderr_tail.contains("visible-err"), "{failed:?}");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn guard_timeout_fails_and_reaps_the_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let script = "sleep 60 & echo $! > child.pid; echo $$ > parent.pid; wait";

    let report = run_guard(
        dir.path(),
        &spec(vec![shell(script, Duration::from_millis(750))]),
    )
    .await;
    assert!(!report.passed);
    assert_eq!(report.commands[0].status, GuardStatus::TimedOut);

    for file in ["parent.pid", "child.pid"] {
        let pid: i32 = std::fs::read_to_string(dir.path().join(file))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let pid = nix::unistd::Pid::from_raw(pid);
        let mut gone = false;
        for _ in 0..100 {
            if nix::sys::signal::kill(pid, None) == Err(nix::errno::Errno::ESRCH) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(gone, "{file} process {pid} survived the guard timeout");
    }
}

#[tokio::test]
async fn guard_timeout_preserves_output_emitted_before_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let report = run_guard(
        dir.path(),
        &spec(vec![shell(
            "echo early-output; sleep 60",
            Duration::from_millis(500),
        )]),
    )
    .await;
    assert!(!report.passed);
    assert_eq!(report.commands[0].status, GuardStatus::TimedOut);
    assert!(
        report.commands[0].stdout_tail.contains("early-output"),
        "expected early output retained before timeout: {:?}",
        report.commands[0].stdout_tail
    );
}

#[tokio::test]
async fn guard_over_bound_output_retains_the_true_tail() {
    let dir = tempfile::tempdir().unwrap();
    // 9 MiB of zeros exceeds the 8 MiB capture bound.
    let mut over_bound = spec(vec![shell(
        "printf 'START'; head -c 9437184 /dev/zero; printf 'END'",
        Duration::from_secs(30),
    )]);
    over_bound.output_tail_bytes = 64;
    let report = run_guard(dir.path(), &over_bound).await;
    assert!(report.passed, "{report:?}");
    let stdout = &report.commands[0].stdout_tail;
    assert!(
        stdout.ends_with("END"),
        "tail must end with END: {stdout:?}"
    );
    assert!(
        !stdout.starts_with("START"),
        "prefix must not survive in the tail: {stdout:?}"
    );
    assert!(report.commands[0].output_truncated);
}

#[tokio::test]
async fn guard_reports_an_unstartable_command_as_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let missing = GuardCommand {
        program: "rsi-integration-guard-program-that-does-not-exist".to_string(),
        args: Vec::new(),
        timeout: Duration::from_secs(5),
    };

    let report = run_guard(dir.path(), &spec(vec![missing])).await;
    assert!(!report.passed);
    assert!(
        matches!(report.commands[0].status, GuardStatus::Unavailable { .. }),
        "{report:?}"
    );
}

#[test]
fn guard_withholds_daemon_authority_and_repository_overrides() {
    for key in ["RSI_SESSION_TOKEN", "RSI_DB", "GIT_DIR", "GIT_WORK_TREE"] {
        assert!(super::guard::scrubbed(key), "{key} must be withheld");
    }
    for key in ["PATH", "HOME", "CARGO_TARGET_DIR", "GUARD_VALUE"] {
        assert!(!super::guard::scrubbed(key), "{key} must be inherited");
    }
}

#[test]
fn target_authorization_is_fail_closed() {
    let settings = config(&[TARGET, "refs/heads/-option", "refs/tags/rolling", "rolling"]);
    assert!(authorize_target(&settings, TARGET).is_ok());
    for target in [
        "refs/heads/-option",
        "refs/tags/rolling",
        "rolling",
        "refs/heads/",
        "refs/heads/unlisted",
    ] {
        assert!(
            matches!(
                authorize_target(&settings, target),
                Err(IntegrationError::Refused(Refusal::TargetDenied))
            ),
            "{target} must be denied"
        );
    }
}

#[tokio::test]
async fn held_custody_blocks_raw_checkout_and_abort_releases_it() {
    let fixture = Fixture::new(true);
    let settings = config(&[TARGET]);
    let operation_id = Uuid::new_v4();

    let custody = acquire_target_custody(
        &settings,
        &fixture.repo,
        TARGET,
        &fixture.base,
        &fixture.scratch,
        operation_id,
    )
    .await
    .unwrap();
    let record = custody.record();
    assert_eq!(record.version, 1);
    assert_eq!(record.operation_id, operation_id);
    assert_eq!(record.target_ref, TARGET);
    assert_eq!(record.expected_tip, fixture.base);
    assert_eq!(record.candidate, None);
    assert_eq!(record.phase, CustodyPhase::Acquired);
    assert!(
        !record.engine_owned,
        "held target keeps its existing holder"
    );
    assert_eq!(
        std::fs::canonicalize(&fixture.repo).unwrap(),
        record.holder,
        "the exact existing holder is recorded"
    );
    assert!(
        record.marker.is_dir(),
        "the operation proof directory exists"
    );

    // A second acquisition while the custody is active is refused.
    let second = acquire_target_custody(
        &settings,
        &fixture.repo,
        TARGET,
        &fixture.base,
        &fixture.scratch,
        Uuid::new_v4(),
    )
    .await;
    assert!(
        matches!(refusal(second), Refusal::CustodyHeld { .. }),
        "the active custody holds the target"
    );

    // A raw ordinary checkout in the held worktree is refused: the engine's
    // index.lock is a native Git lock, so no unmanaged process can write the
    // holder index while the custody is active.
    let denied = git_status(&fixture.repo, &["checkout", "-q", "-b", "peek"]);
    assert!(
        !denied.status.success(),
        "checkout must be refused while custody is active: {}",
        String::from_utf8_lossy(&denied.stderr)
    );
    let stderr = String::from_utf8_lossy(&denied.stderr);
    assert!(
        stderr.contains("index.lock"),
        "refusal is the custody lock: {stderr}"
    );
    assert_eq!(
        fixture.resolve(TARGET),
        fixture.base,
        "target ref untouched"
    );
    assert_eq!(
        git(&fixture.repo, &["symbolic-ref", "HEAD"]),
        TARGET,
        "holder stays on the target branch"
    );

    // After abort the identical checkout succeeds.
    abort_target_custody(custody).await.unwrap();
    let allowed = git_status(&fixture.repo, &["checkout", "-q", "-b", "peek"]);
    assert!(
        allowed.status.success(),
        "checkout must succeed after abort: {}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert_eq!(fixture.resolve(TARGET), fixture.base);
}

#[tokio::test]
async fn held_custody_blocks_raw_rebase_and_abort_starts_it() {
    let fixture = Fixture::new(true);
    let settings = config(&[TARGET]);

    // The held worktree advances the target to a tip commit so a rebase has a
    // real commit to replay, then a side commit diverges from the shared base.
    std::fs::write(fixture.repo.join("tip.txt"), "tip\n").unwrap();
    git(&fixture.repo, &["add", "tip.txt"]);
    git(&fixture.repo, &["commit", "-q", "-m", "tip"]);
    let tip = fixture.resolve(TARGET);
    assert_eq!(fixture.resolve("HEAD"), tip, "holder is clean at the tip");
    let side = fixture.commit(&fixture.base, "side.txt", "side\n");

    let custody = acquire_target_custody(
        &settings,
        &fixture.repo,
        TARGET,
        &tip,
        &fixture.scratch,
        Uuid::new_v4(),
    )
    .await
    .unwrap();

    // A real raw rebase that must write the index is refused while the
    // custody is active: git cannot even detach to start.
    let denied = git_status(&fixture.repo, &["rebase", &side]);
    assert!(
        !denied.status.success(),
        "rebase must be refused while custody is active: {}",
        String::from_utf8_lossy(&denied.stderr)
    );
    let stderr = String::from_utf8_lossy(&denied.stderr);
    assert!(
        stderr.contains("index.lock"),
        "refusal is the custody lock: {stderr}"
    );
    assert_eq!(fixture.resolve(TARGET), tip, "target ref untouched");
    assert_eq!(fixture.resolve("HEAD"), tip, "holder HEAD untouched");
    assert_eq!(
        git(&fixture.repo, &["status", "--porcelain"]),
        "",
        "no rebase residue"
    );

    // After abort the same rebase starts and completes.
    abort_target_custody(custody).await.unwrap();
    let allowed = git_status(&fixture.repo, &["rebase", &side]);
    assert!(
        allowed.status.success(),
        "rebase must run after abort: {}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert!(fixture.is_ancestor(&side, &fixture.resolve(TARGET)));
    assert_eq!(git(&fixture.repo, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn unheld_custody_registers_engine_worktree_and_abort_removes_it() {
    let fixture = Fixture::new(false);
    let settings = config(&[TARGET]);
    let operation_id = Uuid::new_v4();
    assert_eq!(fixture.registered_worktrees(), 1, "only the main worktree");

    let custody = acquire_target_custody(
        &settings,
        &fixture.repo,
        TARGET,
        &fixture.base,
        &fixture.scratch,
        operation_id,
    )
    .await
    .unwrap();
    let holder = custody.record().holder.clone();
    let record = custody.record();
    assert_eq!(record.version, 1);
    assert_eq!(record.operation_id, operation_id);
    assert_eq!(record.target_ref, TARGET);
    assert_eq!(record.expected_tip, fixture.base);
    assert_eq!(record.phase, CustodyPhase::Acquired);
    assert!(record.engine_owned, "unheld target gets an engine worktree");
    assert_eq!(
        record.git_dir.join(format!("index.rsi-{operation_id}")),
        record.alt_index
    );
    assert!(record.alt_index.is_file(), "the index snapshot exists");
    assert!(
        record.marker.is_dir(),
        "the operation proof directory exists"
    );

    // The engine worktree is registered as the target holder and checked out.
    assert_eq!(
        fixture.registered_worktrees(),
        2,
        "engine worktree registered"
    );
    let listing = git(&fixture.repo, &["worktree", "list", "--porcelain"]);
    assert!(
        listing.contains(&format!("worktree {}", holder.display())),
        "{listing}"
    );
    assert!(listing.contains("branch refs/heads/rolling"), "{listing}");
    assert!(
        holder.join("shared.txt").is_file(),
        "engine worktree has the target checked out"
    );
    assert_eq!(std::fs::canonicalize(&holder).unwrap(), record.holder);

    // The engine worktree is refused the same raw checkout while it holds the
    // target: its own private git dir carries the custody index.lock.
    let denied = git_status(&holder, &["checkout", "-q", "-b", "peek"]);
    assert!(
        !denied.status.success(),
        "checkout in the engine worktree must be refused"
    );
    assert!(String::from_utf8_lossy(&denied.stderr).contains("index.lock"));

    // Abort removes the path and the registration.
    abort_target_custody(custody).await.unwrap();
    assert_eq!(fixture.registered_worktrees(), 1, "registration removed");
    assert!(!holder.exists(), "engine worktree path removed");
    assert_eq!(fixture.scratch_entries(), 0);
    assert_eq!(
        fixture.resolve(TARGET),
        fixture.base,
        "target ref untouched"
    );
}

/// Every custody artifact must be gone from `git_dir` after a terminal
/// settlement: lock sentinel, marker, alternate index, transition proof
/// files, and any restore temp.
fn assert_no_custody_artifacts(repo: &Path, operation_id: Uuid) {
    let git_dir = repo.join(".git");
    assert!(
        !git_dir.join("index.lock").exists(),
        "lock sentinel must be gone"
    );
    assert!(
        !git_dir
            .join("rsi-integration-custody")
            .join(operation_id.to_string())
            .exists(),
        "operation proof directory must be gone"
    );
    for name in [
        format!("index.rsi-{operation_id}"),
        format!("index.rsi-{operation_id}-restore"),
        format!("rsi-{operation_id}-prepared"),
        format!("rsi-{operation_id}-applying"),
        format!("rsi-{operation_id}-applied"),
        format!("rsi-{operation_id}-tmp"),
    ] {
        assert!(
            !git_dir.join(&name).exists(),
            "custody artifact {name} must be gone"
        );
    }
}

#[tokio::test]
async fn held_custody_prepare_and_publish_lands_the_candidate_and_cleans() {
    let fixture = Fixture::new(true);
    let settings = config(&[TARGET]);
    let source = fixture.commit(&fixture.base, "feature.txt", "feature\n");

    let mut custody = acquire_target_custody(
        &settings,
        &fixture.repo,
        TARGET,
        &fixture.base,
        &fixture.scratch,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let operation_id = custody.record().operation_id;
    let held_record = custody.record().clone();
    assert_eq!(held_record.phase, CustodyPhase::Acquired);

    let prepared = custody
        .prepare_candidate(&source, &fixture.scratch)
        .await
        .unwrap();
    let Prepared::Candidate(candidate) = &prepared else {
        panic!("expected a candidate");
    };
    assert_eq!(custody.record().phase, CustodyPhase::Prepared);
    assert_eq!(
        custody.record().candidate.as_deref(),
        Some(candidate.oid.as_str())
    );

    let outcome = custody.publish(&candidate.oid).await.unwrap();
    assert_eq!(outcome, Publication::Published);

    // The reference-transaction hook ran inside the publish: the holder's
    // working tree and index end at the candidate, exactly as the hook's
    // read-tree applied and finalization promoted.
    assert_eq!(fixture.resolve(TARGET), candidate.oid);
    assert_eq!(fixture.resolve("HEAD"), candidate.oid);
    assert_eq!(
        std::fs::read_to_string(fixture.repo.join("feature.txt")).unwrap(),
        "feature\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo.join("shared.txt")).unwrap(),
        "base\n"
    );
    assert_eq!(
        git(&fixture.repo, &["status", "--porcelain"]),
        "",
        "holder is strictly clean at the candidate"
    );
    assert_eq!(git(&fixture.repo, &["ls-files"]), "feature.txt\nshared.txt");
    assert_eq!(
        git(&fixture.repo, &["symbolic-ref", "HEAD"]),
        TARGET,
        "holder stays on the target branch"
    );

    // Every custody artifact is gone from the held git dir.
    assert_no_custody_artifacts(&fixture.repo, operation_id);

    // Terminal custody cleanup removes the candidate; a forged later discard
    // still cannot claim any worktree.
    let forged = CandidateHandle {
        worktree: candidate.handle.worktree.clone(),
        id: "forged".to_string(),
    };
    assert_eq!(
        refusal(discard_candidate(&settings, &fixture.repo, &forged).await),
        Refusal::NotEngineCandidate
    );
    assert_eq!(fixture.scratch_entries(), 0);
}

#[tokio::test]
async fn unheld_custody_publish_removes_the_engine_holder_after_publication() {
    let fixture = Fixture::new(false);
    let settings = config(&[TARGET]);
    let source = fixture.commit(&fixture.base, "feature.txt", "feature\n");

    let mut custody = acquire_target_custody(
        &settings,
        &fixture.repo,
        TARGET,
        &fixture.base,
        &fixture.scratch,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let operation_id = custody.record().operation_id;
    let holder = custody.record().holder.clone();
    assert!(custody.record().engine_owned);

    let prepared = custody
        .prepare_candidate(&source, &fixture.scratch)
        .await
        .unwrap();
    let built = candidate(prepared);
    assert_eq!(
        fixture.registered_worktrees(),
        3,
        "engine holder plus the candidate worktree are registered"
    );

    let outcome = custody.publish(&built.oid).await.unwrap();
    assert_eq!(outcome, Publication::Published);

    // Publication removed the engine holder path and its registration.
    assert_eq!(
        fixture.registered_worktrees(),
        1,
        "engine holder and candidate worktrees are removed automatically"
    );
    assert!(!holder.exists(), "engine holder path removed");
    assert_eq!(fixture.resolve(TARGET), built.oid);
    assert!(
        !git(&fixture.repo, &["worktree", "list", "--porcelain"])
            .contains(&format!("worktree {}", holder.display())),
        "no worktree entry for the removed engine holder"
    );
    assert!(
        !fixture.repo.join(".git").join("index.lock").exists(),
        "no foreign lock left in the main repo"
    );
    assert!(
        !fixture
            .repo
            .join(".git")
            .join("rsi-integration-custody")
            .exists()
    );
    assert_no_custody_artifacts(&fixture.repo, operation_id);

    assert_eq!(fixture.registered_worktrees(), 1);
    assert_eq!(fixture.scratch_entries(), 0);
}

#[tokio::test]
async fn prepared_custody_still_refuses_raw_checkout_and_rebase() {
    let fixture = Fixture::new(true);
    let settings = config(&[TARGET]);

    // Advance the held worktree to a tip so a raw rebase has a real commit to
    // replay, then diverge a side commit and build a candidate on the tip.
    std::fs::write(fixture.repo.join("tip.txt"), "tip\n").unwrap();
    git(&fixture.repo, &["add", "tip.txt"]);
    git(&fixture.repo, &["commit", "-q", "-m", "tip"]);
    let tip = fixture.resolve(TARGET);
    assert_eq!(fixture.resolve("HEAD"), tip);
    let side = fixture.commit(&fixture.base, "side.txt", "side\n");
    let source = fixture.commit(&tip, "feature.txt", "feature\n");

    let mut custody = acquire_target_custody(
        &settings,
        &fixture.repo,
        TARGET,
        &tip,
        &fixture.scratch,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let prepared = custody
        .prepare_candidate(&source, &fixture.scratch)
        .await
        .unwrap();
    let built = candidate(prepared);
    assert_eq!(custody.record().phase, CustodyPhase::Prepared);

    // A raw ordinary checkout and a real raw rebase are both refused while
    // the Prepared custody holds the holder's index.lock.
    let checkout = git_status(&fixture.repo, &["checkout", "-q", "-b", "peek"]);
    assert!(
        !checkout.status.success(),
        "checkout must be refused while Prepared: {}",
        String::from_utf8_lossy(&checkout.stderr)
    );
    assert!(String::from_utf8_lossy(&checkout.stderr).contains("index.lock"));
    let rebase = git_status(&fixture.repo, &["rebase", &side]);
    assert!(
        !rebase.status.success(),
        "rebase must be refused while Prepared: {}",
        String::from_utf8_lossy(&rebase.stderr)
    );
    assert!(String::from_utf8_lossy(&rebase.stderr).contains("index.lock"));
    assert_eq!(fixture.resolve(TARGET), tip, "target ref untouched");
    assert_eq!(fixture.resolve("HEAD"), tip, "holder HEAD untouched");
    assert_eq!(
        git(&fixture.repo, &["status", "--porcelain"]),
        "",
        "no residue from the refused raw operations"
    );

    // Publish still completes over the refused raw attempts and cleans.
    let outcome = custody.publish(&built.oid).await.unwrap();
    assert_eq!(outcome, Publication::Published);
    assert_eq!(fixture.resolve(TARGET), built.oid);
    assert_eq!(fixture.resolve("HEAD"), built.oid);
    assert_eq!(git(&fixture.repo, &["status", "--porcelain"]), "");
    assert_eq!(fixture.scratch_entries(), 0);
}

#[tokio::test]
async fn injected_transaction_abort_after_application_restores_expected_and_cleans() {
    let fixture = Fixture::new(true);
    let settings = config(&[TARGET]);

    std::fs::write(fixture.repo.join("tip.txt"), "tip\n").unwrap();
    git(&fixture.repo, &["add", "tip.txt"]);
    git(&fixture.repo, &["commit", "-q", "-m", "tip"]);
    let tip = fixture.resolve(TARGET);
    let source = fixture.commit(&tip, "feature.txt", "feature\n");

    let mut custody = acquire_target_custody(
        &settings,
        &fixture.repo,
        TARGET,
        &tip,
        &fixture.scratch,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let operation_id = custody.record().operation_id;
    let prepared = custody
        .prepare_candidate(&source, &fixture.scratch)
        .await
        .unwrap();
    let built = candidate(prepared);

    // The prepared hook applies the candidate to the holder and then aborts
    // the transaction (ref stays at the expected tip, marker left Applying).
    let outcome = custody.publish_for_test(&built.oid, false, true).await;

    match outcome {
        Ok(Publication::Aborted) => {}
        other => panic!("expected Aborted settlement, got {other:?}"),
    }
    assert_eq!(fixture.resolve(TARGET), tip, "target restored to expected");
    assert_eq!(fixture.resolve("HEAD"), tip, "holder HEAD restored");
    assert_eq!(
        git(&fixture.repo, &["status", "--porcelain"]),
        "",
        "holder clean after restore"
    );
    assert!(
        fixture.repo.join("tip.txt").is_file(),
        "expected-tip file restored"
    );
    assert!(
        !fixture.repo.join("feature.txt").exists(),
        "candidate-only file removed by the restore"
    );
    assert_eq!(
        git(&fixture.repo, &["ls-files"]),
        "shared.txt\ntip.txt",
        "real index matches the expected tree"
    );
    assert_no_custody_artifacts(&fixture.repo, operation_id);
    assert_eq!(fixture.scratch_entries(), 0);
}

#[tokio::test]
async fn lost_acknowledgement_reconciles_record_only_to_published_and_cleans() {
    let fixture = Fixture::new(true);
    let settings = config(&[TARGET]);
    let source = fixture.commit(&fixture.base, "feature.txt", "feature\n");

    let mut custody = acquire_target_custody(
        &settings,
        &fixture.repo,
        TARGET,
        &fixture.base,
        &fixture.scratch,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let operation_id = custody.record().operation_id;
    let prepared = custody
        .prepare_candidate(&source, &fixture.scratch)
        .await
        .unwrap();
    let built = candidate(prepared);
    let held_record = custody.record().clone();

    // The ref commits and the Applied marker lands; the live object is
    // dropped without finalization (simulated daemon death).
    let outcome = custody.publish_for_test(&built.oid, true, false).await;
    assert!(
        outcome.is_err(),
        "the simulated crash surfaces as an error so the durable record is recovered"
    );
    assert_eq!(fixture.resolve(TARGET), built.oid, "ref committed");
    assert!(
        fixture
            .repo
            .join(".git")
            .join("rsi-integration-custody")
            .join(operation_id.to_string())
            .join("applied.json")
            .is_file(),
        "Applied proof left as immutable record"
    );

    // Discovery finds the exact marker without any open lock file.
    let discovered = discover_custody_record(&settings, &fixture.repo, operation_id)
        .await
        .unwrap()
        .expect("marker must be discoverable");
    assert_eq!(discovered.operation_id, operation_id);
    assert_eq!(discovered.phase, CustodyPhase::Applied);

    // Record-only reconciliation publishes and cleans the proof.
    let outcome = reconcile_target_custody(&settings, &fixture.repo, &held_record)
        .await
        .unwrap();
    assert_eq!(outcome, Publication::Published);
    assert_eq!(fixture.resolve(TARGET), built.oid);
    assert_eq!(fixture.resolve("HEAD"), built.oid);
    assert_eq!(
        std::fs::read_to_string(fixture.repo.join("feature.txt")).unwrap(),
        "feature\n"
    );
    assert_eq!(git(&fixture.repo, &["status", "--porcelain"]), "");
    assert_eq!(git(&fixture.repo, &["ls-files"]), "feature.txt\nshared.txt");
    assert_no_custody_artifacts(&fixture.repo, operation_id);
    assert_eq!(fixture.scratch_entries(), 0);
}

#[tokio::test]
async fn missing_marker_settles_terminal_held_and_engine_owned_outcomes() {
    for held in [true, false] {
        let fixture = Fixture::new(held);
        let settings = config(&[TARGET]);
        let mut custody = acquire_target_custody(
            &settings,
            &fixture.repo,
            TARGET,
            &fixture.base,
            &fixture.scratch,
            Uuid::new_v4(),
        )
        .await
        .unwrap();
        let prepared = custody
            .prepare_candidate(
                &fixture.commit(&fixture.base, "feature", "feature\n"),
                &fixture.scratch,
            )
            .await
            .unwrap();
        let built = candidate(prepared);
        let published_record = custody.record().clone();
        assert_eq!(
            custody.publish(&built.oid).await.unwrap(),
            Publication::Published
        );
        assert_eq!(
            reconcile_target_custody(&settings, &fixture.repo, &published_record)
                .await
                .unwrap(),
            Publication::Published,
            "candidate settlement held={held}"
        );

        let fixture = Fixture::new(held);
        let custody = acquire_target_custody(
            &settings,
            &fixture.repo,
            TARGET,
            &fixture.base,
            &fixture.scratch,
            Uuid::new_v4(),
        )
        .await
        .unwrap();
        let aborted_record = custody.record().clone();
        abort_target_custody(custody).await.unwrap();
        assert_eq!(
            reconcile_target_custody(&settings, &fixture.repo, &aborted_record)
                .await
                .unwrap(),
            Publication::Aborted,
            "expected settlement held={held}"
        );
    }
}

#[tokio::test]
async fn missing_marker_other_tip_is_uncertain_and_keeps_remaining_artifacts() {
    let fixture = Fixture::new(true);
    let settings = config(&[TARGET]);
    let custody = acquire_target_custody(
        &settings,
        &fixture.repo,
        TARGET,
        &fixture.base,
        &fixture.scratch,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let record = custody.record().clone();
    drop(custody);
    std::fs::remove_dir_all(&record.marker).unwrap();
    let other = fixture.commit(&fixture.base, "other", "other\n");
    fixture.set_ref(TARGET, &other);

    assert!(matches!(
        reconcile_target_custody(&settings, &fixture.repo, &record).await,
        Err(IntegrationError::Refused(Refusal::CustodyUncertain { .. }))
    ));
    assert!(record.git_dir.join("index.lock").exists());
}

#[tokio::test]
async fn missing_lock_recovery_resumes_staged_prepared_and_linked_prefixes() {
    for prefix in ["staged", "installed"] {
        let fixture = Fixture::new(true);
        let settings = config(&[TARGET]);
        let custody = acquire_target_custody(
            &settings,
            &fixture.repo,
            TARGET,
            &fixture.base,
            &fixture.scratch,
            Uuid::new_v4(),
        )
        .await
        .unwrap();
        let record = custody.record().clone();
        let staged = record.marker.join("recovery-lock-staged");
        let staged_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)
            .unwrap();
        staged_file.sync_all().unwrap();
        let identity = file_identity(&staged_file).unwrap();
        assert_ne!(identity, record.index_lock_identity);
        drop(staged_file);
        if prefix == "installed" {
            let prepared = record.marker.join("recovery-lock-prepared.json");
            let installed = record.marker.join("recovery-lock-installed.json");
            let prepared_file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&prepared)
                .unwrap();
            let prepared_identity = file_identity(&prepared_file).unwrap();
            let installed_file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&installed)
                .unwrap();
            let installed_identity = file_identity(&installed_file).unwrap();
            let proof = recovery_proof(
                &record,
                staged.clone(),
                identity.clone(),
                prepared_identity,
                installed_identity,
            );
            let bytes = serde_json::to_vec(&proof).unwrap();
            use std::io::Write;
            let mut prepared_file = prepared_file;
            prepared_file.write_all(&bytes).unwrap();
            prepared_file.sync_all().unwrap();
            let mut installed_file = installed_file;
            installed_file.write_all(&bytes).unwrap();
            installed_file.sync_all().unwrap();
        }

        drop(custody);
        let lock = record.git_dir.join("index.lock");
        std::fs::remove_file(&lock).unwrap();

        let active = ensure_active_lock(&record).unwrap();
        assert_eq!(active.identity, identity, "prefix {prefix}");
        assert!(
            record.marker.join("recovery-lock-installed.json").is_file(),
            "installed recovery proof missing for {prefix}"
        );
        // A restart after the installed proof exists must use that exact
        // chain rather than attempting to create or replace anything.
        drop(active);
        let resumed = ensure_active_lock(&record).unwrap();
        assert_eq!(resumed.identity, identity, "installed restart {prefix}");
        abort_cleanup_record(&settings, &fixture.repo, &record, resumed)
            .await
            .unwrap();
        assert_no_custody_artifacts(&fixture.repo, record.operation_id);
    }
}

#[tokio::test]
async fn unbound_recovery_proof_prefix_is_preserved_without_lock_mutation() {
    let fixture = Fixture::new(true);
    let settings = config(&[TARGET]);
    let custody = acquire_target_custody(
        &settings,
        &fixture.repo,
        TARGET,
        &fixture.base,
        &fixture.scratch,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let record = custody.record().clone();
    drop(custody);
    let staged = record.marker.join("recovery-lock-staged");
    std::fs::File::create(&staged).unwrap();
    let partial = record.marker.join("recovery-lock-prepared.json");
    std::fs::write(&partial, b"{}").unwrap();
    let lock = record.git_dir.join("index.lock");
    std::fs::remove_file(&lock).unwrap();
    let before = std::fs::read(&partial).unwrap();

    assert!(matches!(
        ensure_active_lock(&record),
        Err(IntegrationError::Refused(Refusal::CustodyUncertain { .. }))
    ));
    assert_eq!(std::fs::read(&partial).unwrap(), before);
    assert!(staged.is_file());
    assert!(!lock.exists());
}

#[tokio::test]
async fn replaced_missing_lock_is_uncertain_and_preserves_foreign_lock() {
    let fixture = Fixture::new(true);
    let settings = config(&[TARGET]);
    let custody = acquire_target_custody(
        &settings,
        &fixture.repo,
        TARGET,
        &fixture.base,
        &fixture.scratch,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let record = custody.record().clone();
    let foreign_source = record.git_dir.join("foreign-empty-lock-source");
    let foreign = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&foreign_source)
        .unwrap();
    let foreign_identity = file_identity(&foreign).unwrap();
    assert_ne!(foreign_identity, record.index_lock_identity);
    drop(foreign);
    drop(custody);
    let lock = record.git_dir.join("index.lock");
    std::fs::remove_file(&lock).unwrap();
    std::fs::hard_link(&foreign_source, &lock).unwrap();
    let before = std::fs::read_dir(&record.marker)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), std::fs::read(entry.path()).unwrap())
        })
        .collect::<BTreeMap<_, _>>();

    assert!(matches!(
        ensure_active_lock(&record),
        Err(IntegrationError::Refused(Refusal::CustodyUncertain { .. }))
    ));
    assert_eq!(
        file_identity(&std::fs::File::open(&lock).unwrap()).unwrap(),
        foreign_identity
    );
    assert!(record.marker.join("acquired.json").is_file());
    let after = std::fs::read_dir(&record.marker)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), std::fs::read(entry.path()).unwrap())
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        after, before,
        "foreign lock must not create recovery evidence"
    );
}

#[tokio::test]
async fn mismatched_or_malformed_marker_is_uncertain_and_preserves_proof() {
    let fixture = Fixture::new(true);
    let settings = config(&[TARGET]);

    let custody = acquire_target_custody(
        &settings,
        &fixture.repo,
        TARGET,
        &fixture.base,
        &fixture.scratch,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let held_record = custody.record().clone();
    let git_dir = fixture.repo.join(".git");
    let marker = held_record.marker.clone();
    assert!(marker.is_dir(), "operation proof directory exists");

    // A foreign marker (different operation id) is Uncertain and untouched.
    let mut foreign = held_record.clone();
    foreign.operation_id = Uuid::new_v4();
    let foreign_bytes = serde_json::to_vec(&foreign).unwrap();
    let foreign_phase = marker.join("applying.json");
    std::fs::write(&foreign_phase, &foreign_bytes).unwrap();
    let result = reconcile_target_custody(&settings, &fixture.repo, &held_record).await;
    assert!(
        matches!(
            result,
            Err(IntegrationError::Refused(Refusal::CustodyUncertain { .. }))
        ),
        "foreign marker must be Uncertain, got {result:?}"
    );
    assert_eq!(
        std::fs::read(&foreign_phase).unwrap(),
        foreign_bytes,
        "foreign marker preserved"
    );
    assert!(git_dir.join("index.lock").is_file(), "lock preserved");
    assert!(held_record.alt_index.is_file(), "alternate preserved");

    // A malformed marker is Uncertain and untouched.
    std::fs::remove_file(&foreign_phase).unwrap();
    std::fs::write(&foreign_phase, b"{not-json").unwrap();
    let result = reconcile_target_custody(&settings, &fixture.repo, &held_record).await;
    assert!(
        matches!(
            result,
            Err(IntegrationError::Refused(Refusal::CustodyUncertain { .. }))
        ),
        "malformed marker must be Uncertain, got {result:?}"
    );
    assert_eq!(
        std::fs::read(&foreign_phase).unwrap(),
        b"{not-json",
        "malformed marker preserved"
    );
    assert!(git_dir.join("index.lock").is_file(), "lock still preserved");

    // Restore the exact marker; the acquired custody then aborts cleanly.
    std::fs::remove_file(&foreign_phase).unwrap();
    abort_target_custody(custody).await.unwrap();
    assert_eq!(fixture.resolve(TARGET), fixture.base);
    assert!(git(&fixture.repo, &["status", "--porcelain"]).is_empty());
}

#[tokio::test]
async fn acquired_cleanup_foreign_proof_entry_preserves_all_owned_artifacts() {
    let fixture = Fixture::new(true);
    let settings = config(&[TARGET]);
    let custody = acquire_target_custody(
        &settings,
        &fixture.repo,
        TARGET,
        &fixture.base,
        &fixture.scratch,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let record = custody.record().clone();
    let alt_identity = file_identity(&std::fs::File::open(&record.alt_index).unwrap()).unwrap();
    let lock = record.git_dir.join("index.lock");
    let lock_identity = file_identity(&std::fs::File::open(&lock).unwrap()).unwrap();
    let marker = record.marker.join("acquired.json");
    let marker_bytes = std::fs::read(&marker).unwrap();
    let foreign = record.marker.join("foreign-proof");
    let foreign_bytes = b"untrusted-proof";
    std::fs::write(&foreign, foreign_bytes).unwrap();

    assert!(matches!(
        abort_target_custody(custody).await,
        Err(IntegrationError::Refused(Refusal::CustodyUncertain { .. }))
    ));
    assert_eq!(
        file_identity(&std::fs::File::open(&record.alt_index).unwrap()).unwrap(),
        alt_identity,
        "alternate index must survive an Acquired preflight refusal"
    );
    assert_eq!(
        file_identity(&std::fs::File::open(&lock).unwrap()).unwrap(),
        lock_identity,
        "index lock must survive an Acquired preflight refusal"
    );
    assert_eq!(std::fs::read(&marker).unwrap(), marker_bytes);
    assert_eq!(std::fs::read(&foreign).unwrap(), foreign_bytes);
}

#[tokio::test]
async fn prepared_manifest_binds_create_once_artifacts_and_preserves_tamper_evidence() {
    for case in ["immutable-bytes", "mutable-inode", "symlink"] {
        let fixture = Fixture::new(true);
        let settings = config(&[TARGET]);
        let source = fixture.commit(&fixture.base, "feature.txt", "feature\n");
        let mut custody = acquire_target_custody(
            &settings,
            &fixture.repo,
            TARGET,
            &fixture.base,
            &fixture.scratch,
            Uuid::new_v4(),
        )
        .await
        .unwrap();
        custody
            .prepare_candidate(&source, &fixture.scratch)
            .await
            .unwrap();
        let record = custody.record().clone();
        let binding = record
            .artifact_manifest
            .as_ref()
            .expect("Prepared binds manifest");
        assert!(binding.path.is_absolute());
        let bytes = std::fs::read(&binding.path).unwrap();
        let manifest: ArtifactManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(manifest.artifacts.len(), 9);
        assert_eq!(
            manifest.alt_index_identity,
            file_identity(&std::fs::File::open(&record.alt_index).unwrap()).unwrap()
        );
        for artifact in &manifest.artifacts {
            assert_eq!(
                file_identity(&std::fs::File::open(&artifact.path).unwrap()).unwrap(),
                artifact.identity
            );
        }
        drop(custody);

        let path = match case {
            "immutable-bytes" => record.marker.join("next-prepared"),
            "mutable-inode" | "symlink" => record.marker.join("next-status"),
            _ => unreachable!(),
        };
        if case == "immutable-bytes" {
            std::fs::write(&path, b"{tampered").unwrap();
        } else if case == "mutable-inode" {
            std::fs::remove_file(&path).unwrap();
            std::fs::File::create(&path).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink("/dev/null", &path).unwrap();
        }
        let before = std::fs::read_link(&path).ok();
        let result = reconcile_target_custody(&settings, &fixture.repo, &record).await;
        assert!(
            matches!(refusal(result), Refusal::CustodyUncertain { .. }),
            "{case} must fail closed"
        );
        assert!(path.exists() || std::fs::symlink_metadata(&path).is_ok());
        assert_eq!(
            std::fs::read_link(&path).ok(),
            before,
            "{case} evidence preserved"
        );
        assert!(record.marker.join("artifact-manifest.json").is_file());
    }
}

#[tokio::test]
async fn candidate_transition_preserves_add_delete_mode_and_symlink_tree_shape() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let fixture = Fixture::new(true);
    let settings = config(&[TARGET]);
    let builder = fixture.root.path().join("tree-shape-builder");
    git(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            builder.to_str().unwrap(),
            &fixture.base,
        ],
    );
    std::fs::remove_file(builder.join("shared.txt")).unwrap();
    std::fs::write(builder.join("added.txt"), "added\n").unwrap();
    let executable = builder.join("executable.sh");
    std::fs::write(&executable, "#!/bin/sh\necho shape\n").unwrap();
    let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&executable, permissions).unwrap();
    symlink("added.txt", builder.join("added-link")).unwrap();
    git(&builder, &["add", "-A"]);
    git(&builder, &["commit", "-q", "-m", "tree shape"]);
    let source = git(&builder, &["rev-parse", "HEAD"]);
    git(
        &fixture.repo,
        &["worktree", "remove", builder.to_str().unwrap()],
    );

    let mut custody = acquire_target_custody(
        &settings,
        &fixture.repo,
        TARGET,
        &fixture.base,
        &fixture.scratch,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let built = candidate(
        custody
            .prepare_candidate(&source, &fixture.scratch)
            .await
            .unwrap(),
    );
    assert_eq!(
        custody.publish(&built.oid).await.unwrap(),
        Publication::Published
    );
    assert_eq!(fixture.resolve(TARGET), built.oid);
    assert_eq!(fixture.resolve("HEAD"), built.oid);
    assert!(!fixture.repo.join("shared.txt").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.repo.join("added.txt")).unwrap(),
        "added\n"
    );
    assert_eq!(
        std::fs::read_link(fixture.repo.join("added-link")).unwrap(),
        PathBuf::from("added.txt")
    );
    assert_ne!(
        std::fs::metadata(fixture.repo.join("executable.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert_eq!(
        git(&fixture.repo, &["write-tree"]),
        git(&fixture.repo, &["rev-parse", "HEAD^{tree}"])
    );
    assert!(
        git(
            &fixture.repo,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"]
        )
        .is_empty()
    );
}
