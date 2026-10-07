//! #1195: a manager-created worker's sandbox base. The default is the freshly
//! fetched `origin/rolling` tip (never the shared checkout's unpublished
//! `HEAD`); `commit` and `path` sources pin an exact commit at admission.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening,
    clippy::large_futures
)]

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
use super::*;

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn create(p: &Pilot, source: Option<ManagerSandboxSourceV1>) -> ManagerActionV2 {
    ManagerActionV2::CreateSession {
        parent_id: p.epic,
        kind: SessionKind::Task,
        query: "build on the chosen base".into(),
        launch: p.policy.allowed_launches[0].clone(),
        sandbox_source: source,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn commit_file(repo: &std::path::Path, name: &str) -> String {
    std::fs::write(repo.join(name), format!("{name}\n")).unwrap();
    git(repo, &["add", name]);
    git(repo, &["commit", "-qm", name]);
    git(repo, &["rev-parse", "HEAD"])
}

/// Launch the admitted child and return its sandbox HEAD and the recorded
/// allocation source commit (`base_commit` in progress).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn launch_child(p: &Pilot, child: Uuid) -> (String, String) {
    let process = super::super::launch::install_controller_candidate_test_process(child);
    p.execute().await.unwrap();
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    let (root, base) = {
        let store = p.manager.store.lock().await;
        (
            store
                .get_session(child)
                .unwrap()
                .unwrap()
                .sandbox_root
                .unwrap(),
            store.live_custody_for_session(child).unwrap().source_commit,
        )
    };
    let head = git(&root, &["rev-parse", "HEAD"]);
    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, child)
        .await
        .unwrap();
    super::super::launch::drop_controller_candidate_test_stream(child);
    (head, base)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn lifecycle_rows(p: &Pilot) -> (i64, i64) {
    let store = p.manager.store.lock().await;
    let one = |sql: &str| -> i64 { store.conn.query_row(sql, [], |row| row.get(0)).unwrap() };
    (
        one("SELECT count(*) FROM harness_manager_v2_operations WHERE kind='lifecycle_action'"),
        one("SELECT count(*) FROM sessions"),
    )
}

/// The defect: the shared checkout is on `rolling` but ahead of `origin` with
/// unpublished commits. The worker still branches from published rolling.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_create_branches_from_origin_rolling_when_the_checkout_is_ahead() {
    let p = pilot().await;
    let origin = rolling_checkout_with_origin(&p);
    let published = advance_origin_rolling(&p, &origin);
    let local = commit_file(&p.repo, "unpublished-local");
    assert_ne!(local, published);

    let receipt = p.admit("default-rolling", create(&p, None)).await;
    assert_eq!(
        receipt.sandbox_source,
        Some(ManagerSandboxSourceReceiptV1 {
            kind: ManagerSandboxSourceKindV1::Rolling,
            commit: None,
            source_dirty: false,
        })
    );
    let (head, base) = launch_child(&p, receipt.target_session_id.unwrap()).await;
    assert_eq!(head, published);
    assert_eq!(base, published);
    // Observed through a private ref only: neither the local branch nor the
    // remote-tracking ref moved, and the private ref is gone.
    assert_eq!(git(&p.repo, &["rev-parse", "refs/heads/rolling"]), local);
    assert!(git(&p.repo, &["for-each-ref", "refs/rsi"]).is_empty());
}

/// An explicit `{"rolling":{}}` is the default, and a dirty shared checkout no
/// longer blocks the launch: none of its uncommitted state is used.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_rolling_create_ignores_a_dirty_checkout() {
    let p = pilot().await;
    let origin = rolling_checkout_with_origin(&p);
    let published = advance_origin_rolling(&p, &origin);
    std::fs::write(p.repo.join("source"), "operator edit in progress\n").unwrap();

    let receipt = p
        .admit(
            "explicit-rolling",
            create(&p, Some(ManagerSandboxSourceV1::Rolling {})),
        )
        .await;
    let (head, _) = launch_child(&p, receipt.target_session_id.unwrap()).await;
    assert_eq!(head, published);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commit_source_allocates_exactly_that_commit() {
    let p = pilot().await;
    let older = git(&p.repo, &["rev-parse", "HEAD"]);
    let newer = commit_file(&p.repo, "newer");
    assert_ne!(older, newer);

    let receipt = p
        .admit(
            "commit-source",
            create(&p, Some(ManagerSandboxSourceV1::Commit(older.clone()))),
        )
        .await;
    assert_eq!(
        receipt.sandbox_source,
        Some(ManagerSandboxSourceReceiptV1 {
            kind: ManagerSandboxSourceKindV1::Commit,
            commit: Some(older.clone()),
            source_dirty: false,
        })
    );
    let (head, base) = launch_child(&p, receipt.target_session_id.unwrap()).await;
    assert_eq!(head, older);
    assert_eq!(base, older);
    // The settled receipt keeps the admitted source.
    let settled = p.receipt(receipt.operation_id).await;
    assert_eq!(settled.state, ManagerActionStateV2::Succeeded);
    assert_eq!(settled.sandbox_source, receipt.sandbox_source);
}

/// A manager names its own (sibling) worktree: the worker branches from that
/// worktree's committed HEAD as pinned at admission, even if it moves later.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn path_source_pins_a_registered_sibling_worktree_head() {
    let p = pilot().await;
    let sibling = p.repo.parent().unwrap().join("manager-sandbox");
    git(
        &p.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "manager-work",
            sibling.to_str().unwrap(),
        ],
    );
    git(&sibling, &["config", "user.name", "Manager"]);
    git(
        &sibling,
        &["config", "user.email", "manager@example.invalid"],
    );
    let unlanded = commit_file(&sibling, "unlanded-manager-work");
    // Uncommitted state is reported, never used.
    std::fs::write(sibling.join("scratch"), "not committed\n").unwrap();

    let receipt = p
        .admit(
            "path-source",
            create(
                &p,
                Some(ManagerSandboxSourceV1::Path(
                    sibling.to_str().unwrap().into(),
                )),
            ),
        )
        .await;
    assert_eq!(
        receipt.sandbox_source,
        Some(ManagerSandboxSourceReceiptV1 {
            kind: ManagerSandboxSourceKindV1::Path,
            commit: Some(unlanded.clone()),
            source_dirty: true,
        })
    );
    // The sibling moves on after admission; the pin does not.
    let later = commit_file(&sibling, "later-manager-work");
    assert_ne!(later, unlanded);
    let (head, base) = launch_child(&p, receipt.target_session_id.unwrap()).await;
    assert_eq!(head, unlanded);
    assert_eq!(base, unlanded);
}

/// Each invalid source refuses with its stable code before anything is
/// journalled, so nothing is ever allocated.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn invalid_sandbox_sources_refuse_with_zero_allocation() {
    let p = pilot().await;
    let parent = p.repo.parent().unwrap().to_path_buf();
    let plain = parent.join("plain-dir");
    std::fs::create_dir(&plain).unwrap();
    let inside = p.repo.join("nested");
    std::fs::create_dir(&inside).unwrap();
    let other_repo = parent.join("other-repo");
    git(
        &parent,
        &[
            "clone",
            "-q",
            p.repo.to_str().unwrap(),
            other_repo.to_str().unwrap(),
        ],
    );
    let link = parent.join("link-to-plain");
    std::os::unix::fs::symlink(&plain, &link).unwrap();
    let path = |dir: &std::path::Path| ManagerSandboxSourceV1::Path(dir.to_str().unwrap().into());
    let cases = [
        (path(&plain), MANAGER_SANDBOX_SOURCE_NOT_WORKTREE),
        (path(&inside), MANAGER_SANDBOX_SOURCE_NOT_WORKTREE),
        (path(&other_repo), MANAGER_SANDBOX_SOURCE_NOT_WORKTREE),
        (path(&link), MANAGER_SANDBOX_SOURCE_NOT_WORKTREE),
        (
            path(&parent.join("missing")),
            MANAGER_SANDBOX_SOURCE_NOT_WORKTREE,
        ),
        (
            ManagerSandboxSourceV1::Path("repo".into()),
            MANAGER_SANDBOX_SOURCE_INVALID,
        ),
        (
            ManagerSandboxSourceV1::Commit("0123456789abcdef0123456789abcdef01234567".into()),
            MANAGER_SANDBOX_SOURCE_COMMIT_UNKNOWN,
        ),
        (
            ManagerSandboxSourceV1::Commit(git(&p.repo, &["rev-parse", "HEAD"]).to_uppercase()),
            MANAGER_SANDBOX_SOURCE_INVALID,
        ),
    ];
    let baseline = lifecycle_rows(&p).await;
    for (index, (source, code)) in cases.into_iter().enumerate() {
        let error = p
            .manager
            .agent_control()
            .agent_manager_control(
                p.owner,
                p.request(&format!("invalid-{index}"), create(&p, Some(source))),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains(code), "case {index}: {error}");
        assert_eq!(lifecycle_rows(&p).await, baseline, "case {index}");
    }
    assert_eq!(
        git(&p.repo, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
}

/// A prepared create pins the same source: prepare and commit agree on the
/// digest and the queued receipt carries the pinned commit.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn prepared_create_carries_its_sandbox_source_into_the_queued_action() {
    let p = pilot().await;
    let pinned = git(&p.repo, &["rev-parse", "HEAD"]);
    let control = p.manager.agent_control();
    let prepared = control
        .agent_manager_prepare_control(
            p.owner,
            AgentManagerPrepareControlRequestV2 {
                project_id: None,
                operation: PreparedManagerActionV2::CreateSession {
                    parent_id: p.epic,
                    kind: SessionKind::Task,
                    query: "prepared on an exact commit".into(),
                    launch: p.policy.allowed_launches[0].clone(),
                    sandbox_source: Some(ManagerSandboxSourceV1::Commit(pinned.clone())),
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(prepared.readiness, ManagerPreparedActionReadinessV2::Ready);
    let committed = control
        .agent_manager_commit_prepared_control(
            p.owner,
            AgentManagerCommitPreparedControlRequestV2 {
                project_id: None,
                prepared_id: prepared.prepared_id,
                target_digest: prepared.target_digest,
                idempotency_key: "prepared-commit-source".into(),
            },
        )
        .await
        .unwrap();
    let ManagerPreparedActionCommitResultV2::Queued { receipt } = committed else {
        panic!("prepared create did not queue: {committed:?}");
    };
    assert_eq!(
        receipt.sandbox_source.and_then(|source| source.commit),
        Some(pinned)
    );

    // A default (`rolling`) create resolves its base at launch, so the shared
    // checkout moving between prepare and commit does not change the target.
    let prepared = control
        .agent_manager_prepare_control(
            p.owner,
            AgentManagerPrepareControlRequestV2 {
                project_id: None,
                operation: PreparedManagerActionV2::CreateSession {
                    parent_id: p.epic,
                    kind: SessionKind::Task,
                    query: "prepared on rolling".into(),
                    launch: p.policy.allowed_launches[0].clone(),
                    sandbox_source: None,
                },
            },
        )
        .await
        .unwrap();
    commit_file(&p.repo, "checkout-moved-after-prepare");
    let committed = control
        .agent_manager_commit_prepared_control(
            p.owner,
            AgentManagerCommitPreparedControlRequestV2 {
                project_id: None,
                prepared_id: prepared.prepared_id,
                target_digest: prepared.target_digest,
                idempotency_key: "prepared-commit-rolling".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(
            committed,
            ManagerPreparedActionCommitResultV2::Queued { .. }
        ),
        "{committed:?}"
    );
}
