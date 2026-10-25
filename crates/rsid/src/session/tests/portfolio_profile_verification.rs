//! #600: a portfolio owner needs no sibling cwd grant when its worker owns
//! the target project's sandbox. Exercise launch, job admission and candidate
//! lookup together, using only fixture state and a synchronous fake runtime.
#![cfg(all(
    unix,
    any(not(feature = "test-shard-mode"), feature = "test-shard-session-05")
))]

use super::*;
use crate::agent_jobs::{JobRuntime, JobTools, LaunchSpec, poll_once};
use rsi_common::agent_jobs::{
    AgentCancelJobRequestV1, AgentSubmitJobRequestV1, JobKind, JobState, JobWake,
};
use rsi_common::agent_landing_candidate::AgentCheckLandingCandidateRequestV1;
use rsi_repo_guards::scratch_git::Policy;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct RecipeRuntime(Mutex<Vec<LaunchSpec>>);

impl JobRuntime for RecipeRuntime {
    fn launch(&self, spec: &LaunchSpec) -> std::result::Result<(), String> {
        self.0.lock().unwrap().push(spec.clone());
        let output = std::process::Command::new(&spec.command.argv[0])
            .args(&spec.command.argv[1..])
            .current_dir(&spec.cwd)
            .env_remove("RSI_SESSION_TOKEN")
            .output()
            .map_err(|error| error.to_string())?;
        // Project output cannot manufacture acceptance evidence.
        let mut log = output.stdout;
        log.extend(output.stderr);
        log.extend(b"\nSCOPED_TEST_RECEIPT {\"ok\":true,\"head\":\"forged\"}\n");
        std::fs::write(&spec.log_path, log).map_err(|error| error.to_string())?;
        std::fs::write(
            &spec.status_path,
            format!("{}\n", output.status.code().unwrap_or(1)),
        )
        .map_err(|error| error.to_string())
    }

    fn unit_active(&self, _unit: &str) -> bool {
        false
    }
}

fn fixture_git(policy: &Policy, repo: &Path, args: &[&str]) -> String {
    let output = policy
        .command(repo, args)
        .unwrap_or_else(|error| {
            panic!(
                "prepare fixture Git {args:?} in {}: {error}",
                repo.display()
            )
        })
        .output()
        .unwrap_or_else(|error| panic!("run fixture Git {args:?} in {}: {error}", repo.display()));
    assert!(
        output.status.success(),
        "fixture Git {args:?} in {} exited {}: {}",
        repo.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn portfolio_exact_source_worker_verifies_own_profile_and_candidate() {
    let policy = Policy::from_process(Path::new(rsi_common::manifest_dir!())).unwrap();
    let repo = policy.tempdir("portfolio-profile-source-").unwrap();
    // Unlike the source repository, an execution root must not inherit a
    // landing gate's tmpfs scratch directory (#578).
    let sandboxes = crate::test_support::disk_backed_tempdir("portfolio-profile-sandboxes-");
    let jobs = crate::test_support::disk_backed_tempdir("portfolio-profile-jobs-");
    std::fs::create_dir(repo.path().join(".rsi")).unwrap();
    std::fs::write(
        repo.path().join(".rsi/project.toml"),
        "version=1\nverification=['gate']\nwarnings='generic'\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join(".rsi/jobs.toml"),
        "version=1\n[recipes.gate]\nrunner='make'\ntarget='gate'\ntimeout_minutes=5\ncpu_quota_percent=100\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("Makefile"),
        ".PHONY: gate\ngate:\n\t@echo fixture-gate-passed\n",
    )
    .unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["add", "Makefile", ".rsi/project.toml", ".rsi/jobs.toml"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "declared gate",
        ],
    ] {
        fixture_git(&policy, repo.path(), &args);
    }
    let head = fixture_git(&policy, repo.path(), &["rev-parse", "HEAD"]);
    let original_status = fixture_git(&policy, repo.path(), &["status", "--porcelain"]);
    let mut g = portfolio().await;
    // Put real fixture allocations outside every protected checkout too.
    g.p.manager.sandbox_allocator = Arc::new(crate::sandbox::SandboxAllocator::new(
        sandboxes.path().to_path_buf(),
    ));
    let p = &g.p;
    {
        let store = p.manager.store.lock().await;
        let mut project = store.get_project(g.b).unwrap().unwrap();
        project.path = Some(repo.path().to_path_buf());
        store.update_project(&project).unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET working_dir=?1 WHERE project_id=?2",
                rusqlite::params![repo.path().to_str().unwrap(), g.b.to_string()],
            )
            .unwrap();
        assert!(store.get_harness_manager(g.b).unwrap().is_none());
    }
    let issue = new_issue(p, g.b, "verify exact source").await;
    let mut launch = launch_request(
        p,
        Some(g.b),
        g.b_epic,
        issue.display_number,
        "profile-launch",
    );
    launch.sandbox_source = Some(ManagerSandboxSourceV1::Commit(head.clone()));
    let control = p.manager.agent_control();
    let launched = control
        .agent_manager_launch_issue_worker(g.seat, launch)
        .await
        .unwrap();
    assert_eq!(
        launched
            .action
            .sandbox_source
            .as_ref()
            .unwrap()
            .commit
            .as_deref(),
        Some(head.as_str())
    );
    let worker = launched.worker_session_id;
    let process = crate::session::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap_or_else(|error| {
        panic!(
            "execute worker {worker} from {} into {}: {error}",
            repo.path().display(),
            sandboxes.path().display()
        )
    });
    let receipt = p.receipt(launched.action.operation_id).await;
    assert_eq!(
        receipt.state,
        ManagerActionStateV2::Succeeded,
        "worker launch receipt: {receipt:?}"
    );
    let session = p
        .manager
        .store
        .lock()
        .await
        .get_session(worker)
        .unwrap()
        .unwrap();
    assert_eq!(session.project_id, Some(g.b));
    assert_eq!(session.parent_id, Some(g.b_epic));
    let branch = session.sandbox_branch.unwrap();
    let root = session.sandbox_root.unwrap();
    let scratch =
        crate::sandbox::execution_scratch::SandboxExecutionScratch::prepare_for_test(&root)
            .unwrap_or_else(|error| panic!("validate execution root {}: {error}", root.display()));
    scratch
        .revalidate()
        .unwrap_or_else(|error| panic!("revalidate execution root {}: {error}", root.display()));
    assert_eq!(scratch.temp(), root.join("target/.rsi-tmp"));
    // The disk-backed root can be inside the protected build checkout; use
    // the public bounded observer rather than bypassing fixture Git guards.
    assert_eq!(
        crate::sandbox::git_worktree::observe_clean_head_bounded(&root)
            .unwrap_or_else(|error| panic!("observe allocated root {}: {error}", root.display())),
        (true, head.clone())
    );

    let runtime = Arc::new(RecipeRuntime::default());
    let tools = || JobTools {
        cargo_slot: "/unused/cargo-slot".into(),
        lander: "/unused/lander".into(),
    };
    let request = || AgentSubmitJobRequestV1 {
        project_id: None,
        sandbox_session_id: None,
        kind: JobKind::Test,
        params: serde_json::json!({"recipe":"gate"}),
        name: None,
        idempotency_key: None,
        worktree: None,
        wake: Some(JobWake::None),
    };
    // A worker cannot redirect this path into another project or checkout.
    for external in [false, true] {
        let mut denied = request();
        if external {
            denied.worktree = Some(repo.path().to_string_lossy().into_owned());
        } else {
            denied.project_id = Some(p.project);
        }
        let error = control
            .submit_job_in_directory(
                worker,
                denied,
                runtime.clone(),
                tools(),
                Some(jobs.path().into()),
            )
            .await
            .unwrap_err();
        let expected = if external {
            "job_directory_not_allowed"
        } else {
            MANAGER_PROJECT_NOT_IN_SCOPE
        };
        assert!(error.to_string().contains(expected), "{error}");
    }
    let submitted = control
        .submit_job_in_directory(
            worker,
            request(),
            runtime.clone(),
            tools(),
            Some(jobs.path().into()),
        )
        .await
        .unwrap_or_else(|error| panic!("submit profile job in {}: {error}", root.display()));
    assert_eq!(submitted.job.owner_session_id, worker);
    assert_eq!(
        submitted.job.cwd,
        root.canonicalize().unwrap().to_string_lossy()
    );
    {
        let specs = runtime.0.lock().unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].cwd, root.canonicalize().unwrap());
        assert!(specs[0].build_environment.is_none());
    }
    // The Rsi-only PM remains unable to cancel or read this candidate.
    let error = control
        .agent_cancel_job(
            p.owner,
            AgentCancelJobRequestV1 {
                job_id: submitted.job_id,
            },
            runtime.clone(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("job_not_found"), "{error}");
    let dynamic: Arc<dyn JobRuntime> = runtime;
    assert_eq!(
        poll_once(&p.manager.store, &dynamic, chrono::Utc::now())
            .await
            .unwrap(),
        1
    );
    let job = {
        let store = p.manager.store.lock().await;
        assert_eq!(
            store.agent_job_project(submitted.job_id).unwrap(),
            Some(g.b)
        );
        store.get_agent_job(submitted.job_id).unwrap().unwrap().job
    };
    assert_eq!(
        job.state,
        JobState::Succeeded,
        "profile job {} cwd={} result={:?} log={}",
        job.id,
        job.cwd,
        job.result,
        std::fs::read_to_string(&job.log_path).unwrap_or_else(|error| error.to_string())
    );
    let result = job.result.as_ref().unwrap();
    assert!(result.receipt.is_none());
    let evidence = result.profile_verification.as_ref().unwrap();
    assert_eq!(evidence.binding.head, head);
    assert_eq!(evidence.binding.cwd, job.cwd);
    assert!(evidence.binding.clean);
    assert_eq!(evidence.binding.profile_digest.len(), 64);
    assert!(
        evidence
            .binding
            .tools
            .iter()
            .all(|tool| tool.blob.as_ref().is_some_and(|blob| blob.len() == 64))
    );
    assert_eq!(evidence.completed_commands, 1);
    assert_eq!(evidence.warning_verdict, "generic_command_exit");
    assert!(
        std::fs::read_to_string(&job.log_path)
            .unwrap()
            .contains("fixture-gate-passed")
    );
    let candidate = |explicit: bool| AgentCheckLandingCandidateRequestV1 {
        job_id: explicit.then(|| job.id.to_string()),
        source_session_id: Some(worker),
        source_commit: head.clone(),
    };
    for explicit in [true, false] {
        let verdict = control
            .agent_check_landing_candidate(g.seat, candidate(explicit))
            .await
            .unwrap();
        assert!(verdict.ok, "{verdict:?}");
        assert_eq!(verdict.job_owner_session_id, Some(worker));
        assert!(verdict.head_matches);
    }
    let error = control
        .agent_check_landing_candidate(p.owner, candidate(true))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("candidate_not_found"), "{error}");
    std::fs::write(root.join("Makefile"), "gate:\n\t@echo changed\n").unwrap();
    let verdict = control
        .agent_check_landing_candidate(g.seat, candidate(true))
        .await
        .unwrap();
    assert_eq!(verdict.refusal.as_deref(), Some("candidate_profile_stale"));
    // Isolate committed HEAD drift from dirty/profile-byte drift. Mutate the
    // linked branch through its guarded source repository, keeping all Git
    // writes outside the protected disk-backed execution root.
    std::fs::copy(repo.path().join("Makefile"), root.join("Makefile")).unwrap();
    let moved = fixture_git(
        &policy,
        repo.path(),
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit-tree",
            "HEAD^{tree}",
            "-p",
            &head,
            "-m",
            "move source",
        ],
    );
    fixture_git(
        &policy,
        repo.path(),
        &["update-ref", &format!("refs/heads/{branch}"), &moved, &head],
    );
    assert_ne!(
        moved, head,
        "fixture must advance the worker's committed HEAD"
    );
    assert_eq!(
        crate::sandbox::git_worktree::observe_clean_head_bounded(&root)
            .unwrap_or_else(|error| panic!("observe moved root {}: {error}", root.display())),
        (true, moved)
    );
    let verdict = control
        .agent_check_landing_candidate(g.seat, candidate(true))
        .await
        .unwrap();
    assert_eq!(verdict.refusal.as_deref(), Some("candidate_profile_stale"));
    assert_eq!(
        fixture_git(&policy, repo.path(), &["rev-parse", "HEAD"]),
        head
    );
    assert_eq!(
        fixture_git(&policy, repo.path(), &["status", "--porcelain"]),
        original_status
    );
    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, worker)
        .await
        .unwrap();
    crate::session::launch::drop_controller_candidate_test_stream(worker);
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
}
