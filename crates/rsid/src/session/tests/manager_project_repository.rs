//! #617: a project-bound launch must never borrow its portfolio seat's repo.
#![cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]

use super::*;
use rsi_repo_guards::scratch_git::Policy;
use std::path::{Path, PathBuf};

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
struct TargetRepository {
    policy: Policy,
    _scratch: TempDir,
    _execution: TempDir,
    root: PathBuf,
    origin: PathBuf,
    published: String,
    candidate: String,
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn guarded_git(policy: &Policy, root: &Path, args: &[&str]) -> String {
    let output = policy.command(root, args).unwrap().output().unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn target_repository(g: &mut Portfolio) -> TargetRepository {
    let policy = Policy::from_process(Path::new(rsi_common::manifest_dir!())).unwrap();
    let scratch = policy.tempdir("manager-target-repository-").unwrap();
    let root = scratch.path().join("project-repository");
    let origin = scratch.path().join("origin.git");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&origin).unwrap();
    guarded_git(&policy, &origin, &["init", "-q", "--bare"]);
    for args in [
        vec!["init", "-q", "-b", "rolling"],
        vec!["config", "user.name", "Fixture"],
        vec!["config", "user.email", "fixture@example.invalid"],
        vec!["config", "commit.gpgsign", "false"],
        vec!["remote", "add", "origin", origin.to_str().unwrap()],
    ] {
        guarded_git(&policy, &root, &args);
    }
    std::fs::create_dir(root.join(".rsi")).unwrap();
    std::fs::write(
        root.join(".rsi/project.toml"),
        "version=1\nverification=['test']\nwarnings='generic'\n",
    )
    .unwrap();
    std::fs::write(root.join("target-only"), "registered second project\n").unwrap();
    guarded_git(&policy, &root, &["add", ".rsi/project.toml", "target-only"]);
    guarded_git(
        &policy,
        &root,
        &["commit", "-qm", "target published source"],
    );
    let published = guarded_git(&policy, &root, &["rev-parse", "HEAD"]);
    guarded_git(&policy, &root, &["push", "-q", "origin", "rolling"]);
    // Default launches must freshly fetch; the checkout itself is ahead.
    guarded_git(
        &policy,
        &root,
        &["update-ref", "-d", "refs/remotes/origin/rolling"],
    );
    std::fs::write(root.join("candidate-only"), "exact target source\n").unwrap();
    guarded_git(&policy, &root, &["add", "candidate-only"]);
    guarded_git(&policy, &root, &["commit", "-qm", "target candidate"]);
    let candidate = guarded_git(&policy, &root, &["rev-parse", "HEAD"]);
    assert_ne!(candidate, git(&g.p.repo, &["rev-parse", "HEAD"]));
    let execution = crate::test_support::disk_backed_tempdir("manager-target-execution-");
    g.p.manager.sandbox_allocator = std::sync::Arc::new(crate::sandbox::SandboxAllocator::new(
        execution.path().into(),
    ));
    {
        let store = g.p.manager.store.lock().await;
        let mut project = store.get_project(g.b).unwrap().unwrap();
        project.path = Some(root.clone());
        store.update_project(&project).unwrap();
        // Deliberately preserve B's old containers pointing at project A.
        assert_eq!(
            store.get_session(g.b_epic).unwrap().unwrap().working_dir,
            g.p.repo
        );
    }
    TargetRepository {
        policy,
        _scratch: scratch,
        _execution: execution,
        root,
        origin,
        published,
        candidate,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn assert_target_launch(
    g: &Portfolio,
    repo: &TargetRepository,
    source: Option<ManagerSandboxSourceV1>,
    expected: &str,
    key: &str,
) {
    let p = &g.p;
    let original = (
        git(&p.repo, &["rev-parse", "HEAD"]),
        git(&p.repo, &["status", "--porcelain"]),
        git(&p.repo, &["worktree", "list", "--porcelain"]),
    );
    let issue = new_issue(p, g.b, key).await;
    let mut request = launch_request(p, Some(g.b), g.b_epic, issue.display_number, key);
    request.sandbox_source = source;
    let launched = p
        .manager
        .agent_control()
        .agent_manager_launch_issue_worker(g.seat, request)
        .await
        .unwrap();
    let worker = launched.worker_session_id;
    let process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(launched.action.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    let store = p.manager.store.lock().await;
    let row = store.get_session(worker).unwrap().unwrap();
    assert_eq!(row.project_id, Some(g.b));
    assert_eq!(row.parent_id, Some(g.b_epic));
    assert_eq!(row.working_dir, repo.root);
    let custody = store.live_custody_for_session(worker).unwrap();
    assert_eq!(custody.source_commit, expected);
    let sandbox = row.sandbox_root.unwrap();
    drop(store);
    // Allocations are disk-backed even when the source fixture TMPDIR is
    // tmpfs. They may be inside the protected build checkout: use production
    // bounded readers there rather than bypassing the scratch Git guard.
    let sandbox_observation =
        crate::sandbox::git_worktree::observe_repository_target_locked(&sandbox).unwrap();
    let target_observation =
        crate::sandbox::git_worktree::observe_repository_target_locked(&repo.root).unwrap();
    assert_eq!(sandbox_observation.local_target_oid, expected);
    assert_eq!(
        sandbox_observation.repository_identity,
        target_observation.repository_identity
    );
    // The common directory owns origin's configuration for both worktrees.
    assert_eq!(
        guarded_git(&repo.policy, &repo.root, &["remote", "get-url", "origin"]),
        repo.origin.to_str().unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(sandbox.join("target-only")).unwrap(),
        "registered second project\n"
    );
    assert!(sandbox.join(".rsi/project.toml").is_file());
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, worker)
        .await
        .unwrap();
    crate::session::launch::drop_controller_candidate_test_stream(worker);
    assert_eq!(
        original,
        (
            git(&p.repo, &["rev-parse", "HEAD"]),
            git(&p.repo, &["status", "--porcelain"]),
            git(&p.repo, &["worktree", "list", "--porcelain"])
        )
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn project_bound_worker_uses_registered_target_repository_for_all_sources() {
    let mut g = portfolio().await;
    let repo = target_repository(&mut g).await;
    assert_target_launch(&g, &repo, None, &repo.published, "target-rolling").await;
    assert_target_launch(
        &g,
        &repo,
        Some(ManagerSandboxSourceV1::Commit(repo.candidate.clone())),
        &repo.candidate,
        "target-commit",
    )
    .await;
    assert_target_launch(
        &g,
        &repo,
        Some(ManagerSandboxSourceV1::Path(
            repo.root.to_str().unwrap().into(),
        )),
        &repo.candidate,
        "target-path",
    )
    .await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn project_bound_worker_refuses_foreign_sources_and_unusable_repositories_without_effects() {
    let mut g = portfolio().await;
    let repo = target_repository(&mut g).await;
    let p = &g.p;
    let foreign = git(&p.repo, &["rev-parse", "HEAD"]);
    let missing = repo._scratch.path().join("missing");
    let plain = repo._scratch.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    for (index, (path, source, code)) in [
        (
            Some(repo.root.clone()),
            Some(ManagerSandboxSourceV1::Commit(foreign)),
            MANAGER_SANDBOX_SOURCE_COMMIT_UNKNOWN,
        ),
        (
            Some(repo.root.clone()),
            Some(ManagerSandboxSourceV1::Path(
                p.repo.to_str().unwrap().into(),
            )),
            MANAGER_SANDBOX_SOURCE_NOT_WORKTREE,
        ),
        (Some(missing), None, MANAGER_SANDBOX_SOURCE_INVALID),
        (Some(plain), None, MANAGER_SANDBOX_SOURCE_INVALID),
        (
            Some(repo.root.join(".rsi")),
            None,
            MANAGER_SANDBOX_SOURCE_INVALID,
        ),
        (None, None, MANAGER_SANDBOX_SOURCE_INVALID),
    ]
    .into_iter()
    .enumerate()
    {
        let issue = new_issue(p, g.b, "invalid target source").await;
        let mut request = launch_request(
            p,
            Some(g.b),
            g.b_epic,
            issue.display_number,
            &format!("invalid-target-{index}"),
        );
        request.sandbox_source = source;
        let store = p.manager.store.lock().await;
        let mut project = store.get_project(g.b).unwrap().unwrap();
        project.path = path.clone();
        store.update_project(&project).unwrap();
        let counts = || -> (i64, i64, i64) {
            let count = |sql| store.conn.query_row(sql, [], |row| row.get(0)).unwrap();
            (
                count("SELECT count(*) FROM sessions"),
                count("SELECT count(*) FROM harness_manager_v2_operations"),
                count("SELECT count(*) FROM scheduled_jobs"),
            )
        };
        let before = counts();
        drop(store);
        let error = p
            .manager
            .agent_control()
            .agent_manager_launch_issue_worker(g.seat, request)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(code), "case {index}: {error}");
        let attempted = path.as_deref().map_or_else(
            || "<unconfigured>".into(),
            |path| path.display().to_string(),
        );
        assert!(error.contains(&attempted), "case {index}: {error}");
        let store = p.manager.store.lock().await;
        let count = |sql| -> i64 { store.conn.query_row(sql, [], |row| row.get(0)).unwrap() };
        assert_eq!(
            before,
            (
                count("SELECT count(*) FROM sessions"),
                count("SELECT count(*) FROM harness_manager_v2_operations"),
                count("SELECT count(*) FROM scheduled_jobs")
            )
        );
        assert_eq!(
            store
                .get_issue_by_display_number(g.b, issue.display_number)
                .unwrap()
                .unwrap(),
            issue
        );
    }
    assert_eq!(
        git(&p.repo, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
    assert_eq!(
        guarded_git(
            &repo.policy,
            &repo.root,
            &["worktree", "list", "--porcelain"]
        )
        .matches("worktree ")
        .count(),
        1
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn default_container_plan_uses_registered_repository_instead_of_seat_directory() {
    let mut g = portfolio().await;
    let repo = target_repository(&mut g).await;
    let p = &g.p;
    let store = p.manager.store.lock().await;
    let mut project = store.get_project(p.project).unwrap().unwrap();
    project.path = Some(repo.root.clone());
    store.update_project(&project).unwrap();
    store
        .update_session_status(p.epic, SessionStatus::Archived)
        .unwrap();
    let scope = store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: p.project,
            session_id: p.owner,
            epic_ids: None,
            group_ids: Vec::new(),
            expected_row_version: 1,
        })
        .unwrap();
    // A scope change revokes the former policy; renew it at the new fence,
    // as the existing first-launch fixtures do.
    let grant = store
        .get_harness_manager_policy(p.project)
        .unwrap()
        .unwrap();
    store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: scope.row_version,
            expected_policy_version: grant.row_version,
            idempotency_key: "default-repository-policy".into(),
            policy: grant.policy,
        })
        .unwrap();
    let mut request = launch_request(p, None, p.epic, 1, "default-target-plan");
    request.parent_epic_id = None;
    let plan = store
        .issue_worker_default_container_plan(p.owner, &request)
        .unwrap()
        .unwrap();
    assert_eq!(plan.project_id, p.project);
    assert_eq!(plan.working_dir, repo.root);
    assert_eq!(plan.existing_group, Some(p.group));
    assert_eq!(
        store.get_session(p.owner).unwrap().unwrap().working_dir,
        p.repo
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn project_repository_is_revalidated_before_any_launch_effect() {
    let mut g = portfolio().await;
    let repo = target_repository(&mut g).await;
    let p = &g.p;
    let issue = new_issue(p, g.b, "repository disappears after admission").await;
    let request = launch_request(
        p,
        Some(g.b),
        g.b_epic,
        issue.display_number,
        "repository-moved",
    );
    p.manager
        .agent_control()
        .agent_manager_launch_issue_worker(g.seat, request)
        .await
        .unwrap();
    let claim = p.claim().await;
    let missing = repo._scratch.path().join("missing-at-execution");
    {
        let store = p.manager.store.lock().await;
        let mut project = store.get_project(g.b).unwrap().unwrap();
        project.path = Some(missing.clone());
        store.update_project(&project).unwrap();
    }
    let error = crate::session::manager_actions::manager_action_source_gate(
        &p.manager.custody_execution_runtime(),
        &p.manager.store,
        &claim,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains(MANAGER_SANDBOX_SOURCE_INVALID), "{error}");
    assert!(error.contains(missing.to_str().unwrap()), "{error}");
    assert!(
        p.manager
            .store
            .lock()
            .await
            .get_session(claim.operation.context.target_session_id.unwrap())
            .unwrap()
            .is_none()
    );
    assert_eq!(
        guarded_git(
            &repo.policy,
            &repo.root,
            &["worktree", "list", "--porcelain"]
        )
        .matches("worktree ")
        .count(),
        1
    );
    assert_eq!(
        git(&p.repo, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
}
