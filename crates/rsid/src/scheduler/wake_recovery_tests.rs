//! The original source is an authorization boundary, not a disposable base.
use super::wake_when_tests::{arm, finish, insert_job, jobs_terminal};
use super::*;
use crate::sandbox::{SandboxAllocator, custody::CustodyService};
use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
use rsi_common::agent_jobs::JobState;
use rsi_common::types::{SandboxCleanupState, SandboxCustodyTransitionV1, SandboxKind, Session};
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;

fn git(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn allocate_worker(
    store: &mut Store,
    template: &Session,
    repo: &Path,
    base: &Path,
    source: &str,
) -> Session {
    let mut worker = template.clone();
    worker.id = Uuid::new_v4();
    worker.status = SessionStatus::Completed;
    worker.working_dir = repo.to_path_buf();
    let allocation = SandboxAllocator::new(base.to_path_buf())
        .allocate(worker.id, repo, SandboxKind::GitWorktree, source, None)
        .unwrap();
    let branch = allocation.branch.unwrap();
    worker.sandbox_kind = Some(SandboxKind::GitWorktree);
    worker.sandbox_root = Some(allocation.root.clone());
    worker.sandbox_branch = Some(branch.clone());
    worker.git_branch = Some(branch.clone());
    worker.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
    let identity = std::fs::canonicalize(git(
        &allocation.root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ))
    .unwrap();
    store
        .insert_session_with_custody(
            &worker,
            SessionCustodyBinding::New(NewCustodyRoot {
                custody_id: Uuid::new_v4(),
                canonical_repo_dir: repo.display().to_string(),
                sandbox_root: allocation.root.display().to_string(),
                sandbox_branch: branch,
                repository_identity: identity.display().to_string(),
                source_commit: git(&allocation.root, &["rev-parse", "HEAD"]),
                cause: CustodyCause::FreshLaunch,
            }),
        )
        .unwrap();
    worker
}

struct CustodyLauncher {
    store: Arc<Mutex<Store>>,
    base: PathBuf,
    admitted: StdMutex<Vec<Uuid>>,
}

#[async_trait::async_trait]
impl SessionLauncher for CustodyLauncher {
    async fn launch(&self, _: LaunchConfig) -> crate::error::Result<Uuid> {
        panic!("a recovery wake must not launch implicitly")
    }
    async fn resume_scheduled(&self, target: Uuid, _: String) -> crate::error::Result<Uuid> {
        let mut store = self.store.lock().await;
        let session = store.get_session(target)?.unwrap();
        CustodyService::authorize_live(
            &session,
            &mut store,
            &self.base,
            SandboxCustodyTransitionV1::ResumeWake,
        )?;
        self.admitted.lock().unwrap().push(target);
        Ok(target)
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_rewritten_parked_source_is_recoverable_once_and_its_exact_head_can_be_authorized_anew() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let base = temp.path().join("sandboxes");
    std::fs::create_dir(&repo).unwrap();
    std::fs::create_dir(&base).unwrap();
    let repo = repo.canonicalize().unwrap();
    let base = base.canonicalize().unwrap();
    git(&repo, &["init", "-q"]);
    git(
        &repo,
        &["commit", "--allow-empty", "-qm", "allocated source"],
    );
    let original = git(&repo, &["rev-parse", "HEAD"]);
    let mut store = Store::open_in_memory().unwrap();
    let (config, template) =
        crate::store::manager_coordinator::tests::fixture(&store, Default::default());
    let seat = config.current_session_id.unwrap();
    let worker = allocate_worker(&mut store, &template, &repo, &base, &original);
    let root = worker.sandbox_root.as_ref().unwrap();
    std::fs::write(root.join("result.txt"), "verified worker result\n").unwrap();
    git(root, &["add", "result.txt"]);
    git(root, &["commit", "-qm", "worker result"]);
    // Simulate a rewrite outside the shell guard, without changing the branch
    // or losing the committed tree. The allocated source is no longer a parent.
    let tree = git(root, &["rev-parse", "HEAD^{tree}"]);
    let preserved = git(root, &["commit-tree", &tree, "-m", "rewritten result"]);
    git(root, &["update-ref", "HEAD", &preserved]);
    let producer = insert_job(&store, worker.id, "verified result");
    finish(&store, producer, JobState::Succeeded, 0, None);
    let wake = arm(&store, worker.id, jobs_terminal(&[producer]), None).unwrap();
    let store = Arc::new(Mutex::new(store));
    let launcher = Arc::new(CustodyLauncher {
        store: store.clone(),
        base,
        admitted: StdMutex::new(Vec::new()),
    });
    let dynamic: Arc<dyn SessionLauncher> = launcher.clone();
    let bus = Arc::new(EventBus::new(16));
    // A replay models a scheduler crash after creating the notice. It must
    // neither authorize the rejected root nor create a second owner notice.
    install_fault_for_test(wake.id, "crash_after_recovery_notice");
    process_due_wake_when(&store, &bus, &dynamic).await;
    assert!(
        store
            .lock()
            .await
            .get_scheduled_job(&wake.id)
            .unwrap()
            .unwrap()
            .enabled
    );
    fire_job_for_test(&store, &bus, &dynamic, &wake).await;
    let mut guard = store.lock().await;
    assert_eq!(launcher.admitted.lock().unwrap().len(), 0);
    let settled = guard.get_scheduled_job(&wake.id).unwrap().unwrap();
    assert!(!settled.enabled);
    assert!(settled.last_fired_at.is_some());
    assert!(guard.continuation_retry(wake.id).unwrap().is_none());
    let notices: Vec<_> = guard
        .list_scheduled_jobs()
        .unwrap()
        .into_iter()
        .filter(|row| row.name == format!("gone-wake-escalation:{}", wake.id))
        .collect();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].wake_session_id, Some(seat));
    for fact in [
        "wake_recoverable",
        "source_revision_unavailable",
        "sandbox_source",
        &preserved,
        &producer.to_string(),
    ] {
        assert!(
            notices[0].message.contains(fact),
            "notice must name {fact}: {}",
            notices[0].message
        );
    }
    assert_eq!(git(root, &["rev-parse", "HEAD"]), preserved);
    assert_eq!(
        std::fs::read_to_string(root.join("result.txt")).unwrap(),
        "verified worker result\n"
    );
    assert!(git(root, &["status", "--porcelain"]).is_empty());
    // The manager's exact sandbox_source commit creates independent custody;
    // the old quarantined aggregate is never reused or silently repaired.
    let replacement = allocate_worker(&mut guard, &template, &repo, &launcher.base, &preserved);
    let handle = CustodyService::authorize_live(
        &replacement,
        &mut guard,
        &launcher.base,
        SandboxCustodyTransitionV1::ResumeWake,
    )
    .unwrap();
    drop(handle);
    assert_ne!(replacement.sandbox_root, worker.sandbox_root);
    assert_eq!(
        git(
            replacement.sandbox_root.as_ref().unwrap(),
            &["rev-parse", "HEAD"]
        ),
        preserved
    );
    drop(guard);
    launcher
        .resume_scheduled(replacement.id, "continue verified source".into())
        .await
        .unwrap();
    assert_eq!(*launcher.admitted.lock().unwrap(), vec![replacement.id]);
}

/// #360 shares the terminal recovery path; a fence keeps its bounded retries,
/// and exhaustion transfers the preserved source to one durable owner.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn exhausted_continuation_fences_report_the_preserved_head_and_relaunch_path_once() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().canonicalize().unwrap();
    git(&repo, &["init", "-q"]);
    git(
        &repo,
        &["commit", "--allow-empty", "-qm", "verified result"],
    );
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let store = Store::open_in_memory().unwrap();
    let (config, mut worker) =
        crate::store::manager_coordinator::tests::fixture(&store, Default::default());
    worker.id = Uuid::new_v4();
    worker.working_dir = repo;
    store.insert_session(&worker).unwrap();
    let producer = insert_job(&store, worker.id, "green job");
    finish(&store, producer, JobState::Succeeded, 0, None);
    let wake = arm(&store, worker.id, jobs_terminal(&[producer]), None).unwrap();
    let store = Arc::new(Mutex::new(store));
    let bus = Arc::new(EventBus::new(16));
    let error = DaemonError::InvalidParam(
        crate::store::manager_actions::fence::CONTINUATION_TURN_CHANGED.into(),
    );
    for _ in 0..8 {
        settle_retryable_resume_refusal(&store, &bus, &wake, worker.id, &error).await;
    }
    // A stale settlement after a restart is fenced by the retired row.
    settle_retryable_resume_refusal(&store, &bus, &wake, worker.id, &error).await;
    let guard = store.lock().await;
    assert!(!guard.get_scheduled_job(&wake.id).unwrap().unwrap().enabled);
    let notices: Vec<_> = guard
        .list_scheduled_jobs()
        .unwrap()
        .into_iter()
        .filter(|row| row.name == format!("gone-wake-escalation:{}", wake.id))
        .collect();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].wake_session_id, config.current_session_id);
    for fact in [
        "continuation_retry_exhausted",
        "continuation_turn_changed",
        "wake_recoverable",
        "sandbox_source",
        &head,
    ] {
        assert!(notices[0].message.contains(fact), "notice must name {fact}");
    }
}
