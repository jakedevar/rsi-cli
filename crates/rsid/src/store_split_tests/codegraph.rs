// Tests moved out of `rsid-store` (issue #1021 S4); see `mod.rs`.
use rsi_common::types::SessionStatus;
use rsid_store::store::Store;
use rsid_store::test_support::make_test_session;
use rusqlite::params;
use std::path::PathBuf;
use uuid::Uuid;

// moved from rsid-store/src/store/sessions.rs (issue #1021 S4: needs rsid modules)
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[tokio::test]
async fn codegraph_custody_pages_keep_historical_reads_and_find_later_active_owner() {
    let store = Store::open_in_memory().unwrap();
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let mut active_custody = None;
    for ordinal in 0..131_u128 {
        let mut session = make_test_session();
        session.id = Uuid::from_u128(ordinal + 1);
        session.status = if ordinal == 130 {
            SessionStatus::Running
        } else {
            SessionStatus::Completed
        };
        store.insert_session(&session).unwrap();
        let custody_id = Uuid::from_u128(ordinal + 1000);
        let root = format!("/tmp/codegraph-registry-page-{custody_id}");
        store
            .conn
            .execute(
                "INSERT INTO sandbox_custody_roots
             (custody_id,allocation_id,canonical_repo_dir,sandbox_root,sandbox_branch,
              repository_identity,source_commit,state,owner_session_id,generation,
              event_sequence,validation_state,validated_generation,validated_at,
              created_at,updated_at)
             VALUES (?1,?1,'/tmp/codegraph-repo',?2,'rsi/test','repo',
                     '0000000000000000000000000000000000000000',
                     'live',?3,1,1,'verified',1,?4,?4,?4)",
                params![custody_id.to_string(), root, session.id.to_string(), now],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET sandbox_custody_id=?1,sandbox_kind='GitWorktree',
             sandbox_root=?2,sandbox_branch='rsi/test',sandbox_cleanup_state='Live'
             WHERE id=?3",
                params![custody_id.to_string(), root, session.id.to_string()],
            )
            .unwrap();
        if ordinal == 130 {
            active_custody = Some((custody_id, session.id, PathBuf::from(root)));
        }
    }

    let (active_id, active_owner, active_root) = active_custody.unwrap();
    let mut historical = Vec::new();
    let mut after = None;
    loop {
        let page = store.codegraph_custody_page(after, false, 64).unwrap();
        if page.is_empty() {
            break;
        }
        after = page.last().map(|row| row.0);
        historical.extend(page);
    }
    assert_eq!(historical.len(), 131);
    assert_eq!(historical.last().unwrap().0, active_id);
    assert_eq!(
        store.codegraph_custody_page(None, true, 64).unwrap().len(),
        1
    );
    assert_eq!(
        store.codegraph_custody_by_id(active_id).unwrap().unwrap().1,
        active_owner
    );
    assert!(store.is_known_codegraph_custody_root(&active_root).unwrap());

    let resumed = historical[0].1;
    store
        .conn
        .execute(
            "UPDATE sessions SET status='Starting' WHERE id=?1",
            [resumed.to_string()],
        )
        .unwrap();
    let active = store.codegraph_custody_page(None, true, 64).unwrap();
    assert_eq!(active.len(), 2);
    assert_eq!(active[0].0, historical[0].0);
    assert_eq!(active[1].0, active_id);

    let first_root = tempfile::tempdir().unwrap();
    let index_root = tempfile::tempdir().unwrap();
    let first_project = rsi_common::types::Project {
        id: Uuid::new_v4(),
        name: "first".into(),
        path: Some(first_root.path().to_path_buf()),
        description: None,
        color: rsi_common::types::Project::DEFAULT_COLOR.into(),
        context_files: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    store.insert_project(&first_project).unwrap();
    let mut runtime = crate::codegraph::IndexRuntime::start_from_store(
        index_root.path().to_path_buf(),
        &store,
        &[],
        None,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .unwrap();
    assert_eq!(
        runtime
            .handle()
            .registered_project_workspaces(first_project.id)
            .unwrap()
            .len(),
        1
    );
    let store = std::sync::Arc::new(tokio::sync::Mutex::new(store));
    runtime.attach_registry(std::sync::Arc::clone(&store), Vec::new());
    let second_root = tempfile::tempdir().unwrap();
    let second_project = rsi_common::types::Project {
        id: Uuid::new_v4(),
        name: "second".into(),
        path: Some(second_root.path().to_path_buf()),
        ..first_project
    };
    store.lock().await.insert_project(&second_project).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(7), async {
        loop {
            if runtime
                .handle()
                .registered_project_workspaces(second_project.id)
                .unwrap()
                .len()
                == 1
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
}

// moved from rsid-store/src/store/sessions.rs (issue #1021 S4: needs rsid modules)
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[tokio::test]
async fn terminal_custody_keeps_snapshot_reads_and_resume_tools_before_starting() {
    use crate::codegraph::{BoundCodegraphScope, CodegraphReadService, CodegraphServiceError};
    use crate::codegraph::{
        IndexPhase, IndexRuntime, NativeCodegraphBinding, NativeCodegraphToolKind,
        RegisteredWorkspace,
    };
    use rsi_common::codegraph::{
        CodegraphFilterV1, CodegraphOperatorReadV1, CodegraphQueryLimitsV1, CodegraphReadV1,
        CodegraphScopeV1, CodegraphSearchModeV1, CodegraphSnapshotPageRequestV1,
        CodegraphWorkspacePageRequestV1,
    };

    let repo = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let indexes = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join(".git")).unwrap();
    std::fs::write(
        checkout.path().join(".git"),
        format!("gitdir: {}\n", repo.path().join(".git").display()),
    )
    .unwrap();
    std::fs::write(repo.path().join("lib.rs"), "pub fn primary() {}\n").unwrap();
    std::fs::write(checkout.path().join("lib.rs"), "pub fn historic() {}\n").unwrap();
    let project_id = Uuid::new_v4();
    let project = rsi_common::types::Project {
        id: project_id,
        name: "history".into(),
        path: Some(repo.path().to_path_buf()),
        description: None,
        color: rsi_common::types::Project::DEFAULT_COLOR.into(),
        context_files: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let mut store = Store::open_in_memory().unwrap();
    store.insert_project(&project).unwrap();
    let mut session = make_test_session();
    session.project_id = Some(project_id);
    session.status = SessionStatus::Completed;
    session.working_dir = checkout.path().to_path_buf();
    store.insert_session(&session).unwrap();
    let custody_id = Uuid::new_v4();
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    store
        .conn
        .execute(
            "INSERT INTO sandbox_custody_roots
         (custody_id,allocation_id,canonical_repo_dir,sandbox_root,sandbox_branch,
          repository_identity,source_commit,state,owner_session_id,generation,
          event_sequence,validation_state,validated_generation,validated_at,created_at,updated_at)
         VALUES (?1,?1,?2,?3,'rsi/history','repo',
                 '0000000000000000000000000000000000000000',
                 'live',?4,1,1,'verified',1,?5,?5,?5)",
            params![
                custody_id.to_string(),
                repo.path().to_str().unwrap(),
                checkout.path().to_str().unwrap(),
                session.id.to_string(),
                now
            ],
        )
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE sessions SET sandbox_custody_id=?1,sandbox_kind='GitWorktree',
         sandbox_root=?2,sandbox_branch='rsi/history',sandbox_cleanup_state='Live'
         WHERE id=?3",
            params![
                custody_id.to_string(),
                checkout.path().to_str().unwrap(),
                session.id.to_string()
            ],
        )
        .unwrap();
    let workspace = RegisteredWorkspace::registered_checkout(
        project_id,
        repo.path(),
        checkout.path(),
        rsi_codegraph::WorkspaceInstanceKey::RsiSandbox(custody_id),
    )
    .unwrap();
    let original_id = workspace.workspace_id();
    let runtime = IndexRuntime::start_with_registrations(
        indexes.path().to_path_buf(),
        vec![project],
        vec![(
            custody_id,
            repo.path().to_path_buf(),
            checkout.path().to_path_buf(),
        )],
        &[],
    )
    .unwrap();
    runtime.handle().set_enabled(true);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if runtime
                .handle()
                .status(original_id)
                .is_some_and(|item| item.phase == IndexPhase::Ready)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let first_generation = runtime
        .handle()
        .status(original_id)
        .unwrap()
        .ready
        .unwrap()
        .generation;
    std::fs::write(
        checkout.path().join("lib.rs"),
        "pub fn historic() {}\npub fn newer() {}\n",
    )
    .unwrap();
    runtime.handle().force_rescan(original_id).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if runtime
                .handle()
                .status(original_id)
                .and_then(|item| item.ready)
                .is_some_and(|ready| ready.generation > first_generation)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let store = std::sync::Arc::new(tokio::sync::Mutex::new(store));
    let changed = tempfile::tempdir().unwrap();
    std::fs::write(
        changed.path().join(".git"),
        format!("gitdir: {}\n", repo.path().join(".git").display()),
    )
    .unwrap();
    let substituted = RegisteredWorkspace::registered_checkout(
        project_id,
        repo.path(),
        changed.path(),
        rsi_codegraph::WorkspaceInstanceKey::RsiSandbox(custody_id),
    )
    .unwrap();
    assert_eq!(substituted.workspace_id(), original_id);
    let (_substituted_manager, stale_handle) = crate::codegraph::IndexManager::new(
        indexes.path().join("substituted"),
        vec![
            RegisteredWorkspace::primary(project_id, repo.path()).unwrap(),
            substituted,
        ],
    )
    .unwrap();
    let stale_store = store.clone();
    tokio::task::spawn_blocking(move || {
        let service = CodegraphReadService::with_store(&stale_handle, stale_store);
        assert!(matches!(
            service.resolve_operator_scope(
                CodegraphScopeV1 {
                    project_id,
                    workspace_id: Some(original_id),
                },
                true
            ),
            Err(CodegraphServiceError::ScopeDenied)
        ));
    })
    .await
    .unwrap();
    runtime
        .handle()
        .reconcile(vec![
            RegisteredWorkspace::primary(project_id, repo.path()).unwrap(),
        ])
        .unwrap();
    assert!(
        runtime
            .handle()
            .registered_workspace(original_id)
            .unwrap()
            .is_none()
    );
    let handle = runtime.handle().clone();
    let read_store = store.clone();
    let owner = session.id;
    let root = checkout.path().to_path_buf();
    let page_cursor = tokio::task::spawn_blocking(move || {
        let service = CodegraphReadService::with_store(&handle, read_store.clone());
        let scope = CodegraphScopeV1 {
            project_id,
            workspace_id: Some(original_id),
        };
        let bound = service.resolve_operator_scope(scope.clone(), true).unwrap();
        let status = service.status(&bound).unwrap();
        assert_eq!(status.ready.as_ref().unwrap().workspace_id, original_id);
        let page = service
            .list_workspaces(CodegraphWorkspacePageRequestV1 {
                project_id,
                cursor: None,
                limit: 32,
            })
            .unwrap();
        assert!(
            page.workspaces
                .iter()
                .any(|entry| entry.workspace_id == original_id)
        );
        let page_cursor = service
            .list_workspaces(CodegraphWorkspacePageRequestV1 {
                project_id,
                cursor: None,
                limit: 1,
            })
            .unwrap()
            .next_cursor
            .unwrap();
        let history = service
            .list_snapshots(CodegraphSnapshotPageRequestV1 {
                scope: scope.clone(),
                cursor: None,
                limit: 32,
            })
            .unwrap();
        let generation = history.snapshots[0].generation;
        assert_eq!(
            service
                .snapshot_at(scope.clone(), generation)
                .unwrap()
                .snapshot
                .workspace_id,
            original_id
        );
        let search = service
            .read_operator(
                CodegraphOperatorReadV1 {
                    scope: scope.clone(),
                    read: CodegraphReadV1::Search {
                        mode: CodegraphSearchModeV1::ExactName,
                        query: "historic".into(),
                        node_kinds: vec![],
                        path_prefix: None,
                    },
                    filter: CodegraphFilterV1::default(),
                    limits: CodegraphQueryLimitsV1::default(),
                },
                true,
            )
            .unwrap();
        assert_eq!(search.meta.snapshot.workspace_id, original_id);
        let diff = service
            .read_operator(
                CodegraphOperatorReadV1 {
                    scope: scope.clone(),
                    read: CodegraphReadV1::Diff {
                        baseline_generation: None,
                    },
                    filter: CodegraphFilterV1::default(),
                    limits: CodegraphQueryLimitsV1::default(),
                },
                true,
            )
            .unwrap();
        assert_eq!(diff.meta.snapshot.workspace_id, original_id);
        let resumed = NativeCodegraphBinding::for_resumed_launch(
            &handle,
            read_store.clone(),
            project_id,
            owner,
            &root,
        )
        .unwrap()
        .unwrap();
        assert!(resumed.permits(NativeCodegraphToolKind::Status));
        assert!(resumed.permits(NativeCodegraphToolKind::Search));
        assert!(resumed.permits(NativeCodegraphToolKind::Diff));
        let foreign = BoundCodegraphScope::from_daemon_identity(Uuid::new_v4(), original_id, true);
        assert!(matches!(
            service.status(&foreign),
            Err(CodegraphServiceError::ScopeDenied)
        ));
        page_cursor
    })
    .await
    .unwrap();
    store.lock().await.conn.execute("UPDATE sandbox_custody_roots SET validation_state='unverified',validated_generation=NULL,validated_at=NULL WHERE custody_id=?1", [custody_id.to_string()]).unwrap();
    let handle = runtime.handle().clone();
    tokio::task::spawn_blocking(move || {
        let service = CodegraphReadService::with_store(&handle, store.clone());
        let denied = service.resolve_operator_scope(
            CodegraphScopeV1 {
                project_id,
                workspace_id: Some(original_id),
            },
            true,
        );
        assert!(matches!(denied, Err(CodegraphServiceError::ScopeDenied)));
        assert!(matches!(
            service.list_workspaces(CodegraphWorkspacePageRequestV1 {
                project_id,
                cursor: Some(page_cursor),
                limit: 1,
            }),
            Err(CodegraphServiceError::CursorExpired)
        ));
        store
            .blocking_lock()
            .conn
            .execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE sandbox_custody_roots")
            .unwrap();
        assert!(
            NativeCodegraphBinding::for_resumed_launch(
                &handle,
                store,
                project_id,
                owner,
                checkout.path(),
            )
            .is_err()
        );
    })
    .await
    .unwrap();
}

// moved from rsid-store/src/store/sessions.rs (issue #1021 S4: needs rsid modules)
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn codegraph_workspace_cursor_covers_more_than_128_dormant_custodies() {
    use crate::codegraph::{
        CodegraphReadService, CodegraphServiceError, IndexManager, RegisteredWorkspace,
    };
    use rsi_common::codegraph::CodegraphWorkspacePageRequestV1;

    let repo = tempfile::tempdir().unwrap();
    let checkouts = tempfile::tempdir().unwrap();
    let indexes = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join(".git")).unwrap();
    let project_id = Uuid::new_v4();
    let store = std::sync::Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    {
        let guard = store.blocking_lock();
        guard
            .insert_project(&rsi_common::types::Project {
                id: project_id,
                name: "dormant".into(),
                path: Some(repo.path().to_path_buf()),
                description: None,
                color: rsi_common::types::Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .unwrap();
        for ordinal in 0..130_u128 {
            let root = checkouts.path().join(format!("checkout-{ordinal:03}"));
            std::fs::create_dir(&root).unwrap();
            std::fs::write(
                root.join(".git"),
                format!("gitdir: {}\n", repo.path().join(".git").display()),
            )
            .unwrap();
            let mut session = make_test_session();
            session.id = Uuid::from_u128(ordinal + 1);
            session.project_id = Some(project_id);
            session.status = SessionStatus::Completed;
            session.working_dir = root.clone();
            guard.insert_session(&session).unwrap();
            let custody_id = Uuid::from_u128(ordinal + 1000);
            guard.conn.execute(
                "INSERT INTO sandbox_custody_roots
                 (custody_id,allocation_id,canonical_repo_dir,sandbox_root,sandbox_branch,
                  repository_identity,source_commit,state,owner_session_id,generation,
                  event_sequence,validation_state,validated_generation,validated_at,created_at,updated_at)
                 VALUES (?1,?1,?2,?3,'rsi/history','repo',
                         '0000000000000000000000000000000000000000',
                         'live',?4,1,1,'verified',1,?5,?5,?5)",
                params![custody_id.to_string(), repo.path().to_str().unwrap(), root.to_str().unwrap(), session.id.to_string(), now],
            ).unwrap();
            guard.conn.execute(
                "UPDATE sessions SET sandbox_custody_id=?1,sandbox_kind='GitWorktree',
                 sandbox_root=?2,sandbox_branch='rsi/history',sandbox_cleanup_state='Live' WHERE id=?3",
                params![custody_id.to_string(), root.to_str().unwrap(), session.id.to_string()],
            ).unwrap();
        }
    }
    let primary = RegisteredWorkspace::primary(project_id, repo.path()).unwrap();
    let (_manager, handle) =
        IndexManager::new(indexes.path().to_path_buf(), vec![primary]).unwrap();
    let service = CodegraphReadService::with_store(&handle, store.clone());
    let first = service
        .list_workspaces(CodegraphWorkspacePageRequestV1 {
            project_id,
            cursor: None,
            limit: 32,
        })
        .unwrap();
    let old_cursor = first.next_cursor.clone().unwrap();
    let mut seen = first
        .workspaces
        .iter()
        .map(|entry| entry.workspace_id)
        .collect::<std::collections::HashSet<_>>();
    let mut cursor = first.next_cursor;
    while let Some(next) = cursor {
        let page = service
            .list_workspaces(CodegraphWorkspacePageRequestV1 {
                project_id,
                cursor: Some(next),
                limit: 32,
            })
            .unwrap();
        seen.extend(page.workspaces.iter().map(|entry| entry.workspace_id));
        cursor = page.next_cursor;
    }
    assert_eq!(seen.len(), 131);
    let later = (0..130_u128)
        .map(|ordinal| Uuid::from_u128(ordinal + 1000))
        .find(|custody_id| {
            let id = rsi_codegraph::CodegraphStore::workspace_id(
                project_id,
                &format!("rsi-sandbox:{custody_id}"),
            )
            .unwrap();
            !first
                .workspaces
                .iter()
                .any(|entry| entry.workspace_id == id)
        })
        .unwrap();
    store.blocking_lock().conn.execute(
        "UPDATE sandbox_custody_roots SET generation=2,validated_generation=2 WHERE custody_id=?1",
        [later.to_string()],
    ).unwrap();
    assert!(matches!(
        service.list_workspaces(CodegraphWorkspacePageRequestV1 {
            project_id,
            cursor: Some(old_cursor.clone()),
            limit: 32,
        }),
        Err(CodegraphServiceError::CursorExpired)
    ));
    store.blocking_lock().conn.execute(
        "UPDATE sandbox_custody_roots SET validation_state='unverified',validated_generation=NULL,validated_at=NULL WHERE custody_id=?1",
        [later.to_string()],
    ).unwrap();
    assert!(matches!(
        service.list_workspaces(CodegraphWorkspacePageRequestV1 {
            project_id,
            cursor: Some(old_cursor),
            limit: 32,
        }),
        Err(CodegraphServiceError::CursorExpired)
    ));
}
